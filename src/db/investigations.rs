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
             VALUES (?1, ?2, ?3, 'open')",
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
        let fts_query = fts::sanitize(query);
        let like_pat = format!("%{query}%");

        let mut stmt = self.conn.prepare(
            "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
             FROM investigations
             WHERE id IN (SELECT rowid FROM investigations_fts WHERE investigations_fts MATCH ?1)
             ORDER BY updated_at DESC",
        )?;
        let fts_hits: Vec<Investigation> = stmt
            .query_map(params![fts_query], row_to_investigation)?
            .collect::<Result<Vec<_>, _>>()?;
        let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|i| i.id).collect();

        let mut stmt2 = self.conn.prepare(
            "SELECT id, name, slug, status, plan, findings, created_at, concluded_at, updated_at
             FROM investigations
             WHERE (name LIKE ?1 ESCAPE '\\' OR slug LIKE ?1 ESCAPE '\\'
                 OR plan LIKE ?1 ESCAPE '\\' OR findings LIKE ?1 ESCAPE '\\')
             ORDER BY updated_at DESC",
        )?;
        let like_extras: Vec<Investigation> = stmt2
            .query_map(params![like_pat], row_to_investigation)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|i| !fts_ids.contains(&i.id))
            .collect();

        let mut results = fts_hits;
        results.extend(like_extras);
        Ok(results)
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

    pub fn investigation_reopen(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE investigations
             SET status = 'open', concluded_at = NULL, updated_at = datetime('now')
             WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
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

    pub fn investigation_unlink_person(
        &self,
        investigation_id: i64,
        person_id: i64,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM investigation_people WHERE investigation_id = ?1 AND person_id = ?2",
            params![investigation_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn investigation_people(
        &self,
        investigation_id: i64,
    ) -> Result<Vec<crate::db::people::Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.notes, p.created_at, p.updated_at
             FROM people p
             JOIN investigation_people ip ON p.id = ip.person_id
             WHERE ip.investigation_id = ?1
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![investigation_id], |row| {
            Ok(crate::db::people::Person {
                id: row.get(0)?,
                name: row.get(1)?,
                notes: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn investigation_link_project(&self, investigation_id: i64, project_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO investigation_projects (investigation_id, project_id) VALUES (?1, ?2)",
            params![investigation_id, project_id],
        )?;
        Ok(())
    }

    pub fn investigation_unlink_project(
        &self,
        investigation_id: i64,
        project_id: i64,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM investigation_projects WHERE investigation_id = ?1 AND project_id = ?2",
            params![investigation_id, project_id],
        )?;
        Ok(n > 0)
    }

    pub fn investigation_projects(
        &self,
        investigation_id: i64,
    ) -> Result<Vec<crate::db::projects::Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description, p.links, p.created_at, p.updated_at
             FROM projects p
             JOIN investigation_projects ip ON p.id = ip.project_id
             WHERE ip.investigation_id = ?1
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![investigation_id], |row| {
            let links_json: Option<String> = row.get(3)?;
            let links = links_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            Ok(crate::db::projects::Project {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                links,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Store pre-chunked text with optional embeddings for an investigation.
    pub fn investigation_store_chunks(
        &self,
        investigation_id: i64,
        chunks: &[(String, Option<Vec<f32>>)],
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM investigation_chunks WHERE investigation_id = ?1",
            params![investigation_id],
        )?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO investigation_chunks (investigation_id, chunk_index, chunk_text, embedding)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (i, (text, emb)) in chunks.iter().enumerate() {
            let emb_bytes = emb.as_ref().map(|e| crate::db::embedding_to_bytes(e));
            stmt.execute(params![investigation_id, i as i64, text, emb_bytes])?;
        }
        Ok(())
    }

    /// Find investigations semantically similar to a query embedding.
    pub fn investigation_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<SimilarInvestigation>> {
        let mut stmt = self.conn.prepare(
            "SELECT ic.investigation_id, ic.chunk_text, ic.embedding,
                    i.id, i.name, i.slug, i.status, i.plan, i.findings,
                    i.created_at, i.concluded_at, i.updated_at
             FROM investigation_chunks ic
             JOIN investigations i ON ic.investigation_id = i.id
             WHERE ic.embedding IS NOT NULL",
        )?;

        let mut scored: Vec<(f32, String, Investigation)> = stmt
            .query_map([], |row| {
                let emb_bytes: Vec<u8> = row.get(2)?;
                let chunk_text: String = row.get(1)?;
                let inv = Investigation {
                    id: row.get(3)?,
                    name: row.get(4)?,
                    slug: row.get(5)?,
                    status: row.get(6)?,
                    plan: row.get(7)?,
                    findings: row.get(8)?,
                    created_at: row.get(9)?,
                    concluded_at: row.get(10)?,
                    updated_at: row.get(11)?,
                };
                Ok((emb_bytes, chunk_text, inv))
            })?
            .filter_map(|r| r.ok())
            .map(|(emb_bytes, chunk, inv)| {
                let emb = crate::db::bytes_to_embedding(&emb_bytes);
                let score = crate::search::similarity::cosine_similarity(query_embedding, &emb);
                (score, chunk, inv)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored
            .into_iter()
            .map(
                |(score, matched_chunk, investigation)| SimilarInvestigation {
                    investigation,
                    score,
                    matched_chunk,
                },
            )
            .collect())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimilarInvestigation {
    pub investigation: Investigation,
    pub score: f32,
    pub matched_chunk: String,
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
        assert_eq!(inv.status, "open");
        assert_eq!(inv.slug, "bill-attach-traffic");

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
        db.investigation_start("Open inv", "open-inv", None)
            .unwrap();
        let id2 = db
            .investigation_start("Concluded inv", "concluded-inv", None)
            .unwrap();
        db.investigation_conclude(id2, "Done.").unwrap();

        let open = db.investigation_list(Some("open")).unwrap();
        assert_eq!(open.len(), 1);

        let concluded = db.investigation_list(Some("concluded")).unwrap();
        assert_eq!(concluded.len(), 1);

        let all = db.investigation_list(Some("all")).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_investigation_reopen() {
        let db = db();
        let id = db
            .investigation_start("Perf issue", "perf-issue", None)
            .unwrap();
        db.investigation_conclude(id, "Fixed in v2.").unwrap();

        let inv = db.investigation_get(id).unwrap().unwrap();
        assert_eq!(inv.status, "concluded");
        assert!(inv.concluded_at.is_some());

        assert!(db.investigation_reopen(id).unwrap());

        let inv = db.investigation_get(id).unwrap().unwrap();
        assert_eq!(inv.status, "open");
        assert!(
            inv.concluded_at.is_none(),
            "concluded_at should be cleared on reopen"
        );
    }

    #[test]
    fn test_investigation_link_people() {
        let db = db();
        let inv_id = db
            .investigation_start("Auth spike", "auth-spike", None)
            .unwrap();
        let alice = db
            .people_add("Alice", None)
            .unwrap();
        let bob = db
            .people_add("Bob", None)
            .unwrap();

        db.investigation_link_person(inv_id, alice).unwrap();
        db.investigation_link_person(inv_id, bob).unwrap();

        let people = db.investigation_people(inv_id).unwrap();
        assert_eq!(people.len(), 2);
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Alice"));
        assert!(names.contains(&"Bob"));
    }

    #[test]
    fn test_investigation_unlink_person() {
        let db = db();
        let inv_id = db.investigation_start("Perf", "perf", None).unwrap();
        let person_id = db
            .people_add("Carol", None)
            .unwrap();

        db.investigation_link_person(inv_id, person_id).unwrap();
        assert_eq!(db.investigation_people(inv_id).unwrap().len(), 1);

        assert!(db.investigation_unlink_person(inv_id, person_id).unwrap());
        assert!(db.investigation_people(inv_id).unwrap().is_empty());
        assert!(!db.investigation_unlink_person(inv_id, person_id).unwrap());
    }
}
