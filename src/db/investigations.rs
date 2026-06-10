use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Investigation {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub status: String,
    pub plan: Option<String>,
    pub findings: Option<String>,
    pub created_at: String,
    pub concluded_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvestigationSource {
    pub id: i64,
    pub investigation_id: i64,
    pub url: String,
    pub label: Option<String>,
    pub notes: Option<String>,
    pub added_at: String,
}

impl Database {
    pub fn investigation_start(&self, name: &str, slug: &str, plan: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO investigations (name, slug, plan, status)
             VALUES (?1, ?2, ?3, 'planning')",
            params![name, slug, plan],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn investigation_get(&self, id: i64) -> Result<Option<Investigation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
             FROM investigations WHERE id = ?1",
        )?;
        match stmt.query_row(params![id], row_to_investigation) {
            Ok(i) => Ok(Some(i)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn investigation_get_by_slug(&self, slug: &str) -> Result<Option<Investigation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
             FROM investigations WHERE slug = ?1",
        )?;
        match stmt.query_row(params![slug], row_to_investigation) {
            Ok(i) => Ok(Some(i)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn investigation_list(&self, status_filter: Option<&str>) -> Result<Vec<Investigation>> {
        let (sql, use_filter) = match status_filter {
            Some("all") | None => (
                "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
                 FROM investigations ORDER BY updated_at DESC",
                false,
            ),
            Some(_) => (
                "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
                 FROM investigations WHERE status = ?1 ORDER BY updated_at DESC",
                true,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if use_filter {
            stmt.query_map(params![status_filter.unwrap()], row_to_investigation)?
        } else {
            stmt.query_map([], row_to_investigation)?
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn investigation_search(&self, query: &str) -> Result<Vec<Investigation>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
             FROM investigations
             WHERE id IN (SELECT rowid FROM investigations_fts WHERE investigations_fts MATCH ?1)
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![query], row_to_investigation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn investigation_update(
        &self,
        id: i64,
        name: Option<&str>,
        plan: Option<&str>,
        findings: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool> {
        let mut parts = vec!["updated_at = datetime('now')".to_string()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        let mut idx = 1usize;

        if let Some(v) = name {
            parts.push(format!("name = ?{idx}"));
            vals.push(Box::new(v.to_string()));
            idx += 1;
        }
        if let Some(v) = plan {
            parts.push(format!("plan = ?{idx}"));
            vals.push(Box::new(v.to_string()));
            idx += 1;
        }
        if let Some(v) = findings {
            parts.push(format!("findings = ?{idx}"));
            vals.push(Box::new(v.to_string()));
            idx += 1;
        }
        if let Some(v) = status {
            parts.push(format!("status = ?{idx}"));
            vals.push(Box::new(v.to_string()));
            idx += 1;
        }

        if parts.len() == 1 {
            return Ok(false);
        }
        let sql = format!(
            "UPDATE investigations SET {} WHERE id = ?{idx}",
            parts.join(", ")
        );
        vals.push(Box::new(id));
        let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|v| v.as_ref()).collect();
        Ok(self.conn.execute(&sql, refs.as_slice())? > 0)
    }

    pub fn investigation_conclude(&self, id: i64, findings: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE investigations SET
                status = 'concluded',
                findings = ?1,
                concluded_at = datetime('now'),
                updated_at = datetime('now')
             WHERE id = ?2",
            params![findings, id],
        )?;
        Ok(n > 0)
    }

    pub fn investigation_activate(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE investigations SET status = 'active', updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    pub fn investigation_add_source(
        &self,
        investigation_id: i64,
        url: &str,
        label: Option<&str>,
        notes: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO investigation_sources (investigation_id, url, label, notes)
             VALUES (?1, ?2, ?3, ?4)",
            params![investigation_id, url, label, notes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn investigation_sources(&self, investigation_id: i64) -> Result<Vec<InvestigationSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, investigation_id, url, label, notes, added_at
             FROM investigation_sources WHERE investigation_id = ?1
             ORDER BY added_at",
        )?;
        let rows = stmt.query_map(params![investigation_id], |row| {
            Ok(InvestigationSource {
                id: row.get(0)?,
                investigation_id: row.get(1)?,
                url: row.get(2)?,
                label: row.get(3)?,
                notes: row.get(4)?,
                added_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn investigation_link_person(&self, investigation_id: i64, person_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO investigation_people (investigation_id, person_id) VALUES (?1, ?2)",
            params![investigation_id, person_id],
        )?;
        Ok(())
    }

    pub fn investigation_link_project(&self, investigation_id: i64, project_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO investigation_projects (investigation_id, project_id) VALUES (?1, ?2)",
            params![investigation_id, project_id],
        )?;
        Ok(())
    }
}

fn row_to_investigation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Investigation> {
    Ok(Investigation {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        status: row.get(3)?,
        plan: row.get(4)?,
        findings: row.get(5)?,
        created_at: row.get(6)?,
        concluded_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn test_investigation_lifecycle() {
        let db = db();
        let id = db
            .investigation_start(
                "Bill Attach Traffic Spike",
                "bill-attach-traffic",
                Some("Investigate why bill attachment uploads spiked 3x on 2026-06-01."),
            )
            .unwrap();

        let inv = db.investigation_get(id).unwrap().unwrap();
        assert_eq!(inv.status, "planning");
        assert_eq!(inv.slug, "bill-attach-traffic");

        db.investigation_activate(id).unwrap();
        let inv = db.investigation_get(id).unwrap().unwrap();
        assert_eq!(inv.status, "active");

        db.investigation_conclude(
            id,
            "Root cause: deployment of v2.3.1 changed default file size limit. Fixed in v2.3.2.",
        )
        .unwrap();
        let inv = db.investigation_get(id).unwrap().unwrap();
        assert_eq!(inv.status, "concluded");
        assert!(inv.concluded_at.is_some());
        assert!(inv.findings.is_some());
    }

    #[test]
    fn test_investigation_by_slug() {
        let db = db();
        db.investigation_start("Auth Spike", "auth-spike", None)
            .unwrap();
        let inv = db.investigation_get_by_slug("auth-spike").unwrap().unwrap();
        assert_eq!(inv.name, "Auth Spike");
    }

    #[test]
    fn test_investigation_sources() {
        let db = db();
        let id = db.investigation_start("Perf", "perf", None).unwrap();
        db.investigation_add_source(
            id,
            "https://newrelic.com/query/123",
            Some("New Relic query"),
            None,
        )
        .unwrap();
        db.investigation_add_source(
            id,
            "https://github.com/example/api/pull/456",
            Some("PR that caused it"),
            None,
        )
        .unwrap();

        let sources = db.investigation_sources(id).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].label.as_deref(), Some("New Relic query"));
    }

    #[test]
    fn test_investigation_search_plan_and_findings() {
        let db = db();
        let id = db
            .investigation_start(
                "Timeout errors",
                "timeout-errors",
                Some("Check Temporal workflow timeouts across all services."),
            )
            .unwrap();
        db.investigation_conclude(
            id,
            "Found that accounting-doc-worker has 30s hard limit. Increased to 120s.",
        )
        .unwrap();

        let results = db.investigation_search("Temporal").unwrap();
        assert_eq!(results.len(), 1);

        let results = db.investigation_search("accounting-doc-worker").unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_investigation_list_by_status() {
        let db = db();
        db.investigation_start("Active inv", "active-inv", None)
            .unwrap();
        let id2 = db
            .investigation_start("Concluded inv", "concluded-inv", None)
            .unwrap();
        db.investigation_conclude(id2, "Done.").unwrap();

        let active = db.investigation_list(Some("planning")).unwrap();
        assert_eq!(active.len(), 1);

        let concluded = db.investigation_list(Some("concluded")).unwrap();
        assert_eq!(concluded.len(), 1);

        let all = db.investigation_list(Some("all")).unwrap();
        assert_eq!(all.len(), 2);
    }
}
