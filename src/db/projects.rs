use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectLink {
    pub url: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub links: Vec<ProjectLink>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPerson {
    pub project_id: i64,
    pub person_id: i64,
    pub role: Option<String>,
    pub person_name: String,
}

impl Database {
    pub fn project_add(
        &self,
        name: &str,
        description: Option<&str>,
        links: &[ProjectLink],
    ) -> Result<i64> {
        let links_json = serde_json::to_string(links)?;
        self.conn.execute(
            "INSERT INTO projects (name, description, links) VALUES (?1, ?2, ?3)",
            params![name, description, links_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn project_get(&self, id: i64) -> Result<Option<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, links, created_at, updated_at
             FROM projects WHERE id = ?1",
        )?;
        match stmt.query_row(params![id], row_to_project) {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn project_list(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, links, created_at, updated_at
             FROM projects ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_project)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_search(&self, query: &str) -> Result<Vec<Project>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, links, created_at, updated_at
             FROM projects
             WHERE id IN (SELECT rowid FROM projects_fts WHERE projects_fts MATCH ?1)
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![query], row_to_project)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_update(
        &self,
        id: i64,
        name: Option<&str>,
        description: Option<Option<&str>>,
        links: Option<&[ProjectLink]>,
    ) -> Result<bool> {
        let mut sql_parts: Vec<String> = vec!["updated_at = datetime('now')".to_string()];
        let links_json = links.map(|l| serde_json::to_string(l).unwrap());

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        let mut idx = 1usize;

        if let Some(n) = name {
            sql_parts.push(format!("name = ?{idx}"));
            params_vec.push(Box::new(n.to_string()));
            idx += 1;
        }
        if let Some(d) = description {
            sql_parts.push(format!("description = ?{idx}"));
            params_vec.push(Box::new(d.map(|s| s.to_string())));
            idx += 1;
        }
        if links_json.is_some() {
            sql_parts.push(format!("links = ?{idx}"));
            params_vec.push(Box::new(links_json.clone()));
            idx += 1;
        }

        if sql_parts.len() == 1 {
            return Ok(false);
        }

        let sql = format!(
            "UPDATE projects SET {} WHERE id = ?{idx}",
            sql_parts.join(", ")
        );
        params_vec.push(Box::new(id));
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|v| v.as_ref()).collect();
        let n = self.conn.execute(&sql, refs.as_slice())?;
        Ok(n > 0)
    }

    pub fn project_delete(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    pub fn project_link_person(
        &self,
        project_id: i64,
        person_id: i64,
        role: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO project_people (project_id, person_id, role)
             VALUES (?1, ?2, ?3)",
            params![project_id, person_id, role],
        )?;
        Ok(())
    }

    pub fn project_unlink_person(&self, project_id: i64, person_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM project_people WHERE project_id = ?1 AND person_id = ?2",
            params![project_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn project_link_meeting(&self, project_id: i64, meeting_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO project_meetings (project_id, meeting_id) VALUES (?1, ?2)",
            params![project_id, meeting_id],
        )?;
        Ok(())
    }

    pub fn project_unlink_meeting(&self, project_id: i64, meeting_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM project_meetings WHERE project_id = ?1 AND meeting_id = ?2",
            params![project_id, meeting_id],
        )?;
        Ok(n > 0)
    }

    pub fn project_people(&self, project_id: i64) -> Result<Vec<ProjectPerson>> {
        let mut stmt = self.conn.prepare(
            "SELECT pp.project_id, pp.person_id, pp.role, p.name
             FROM project_people pp
             JOIN people p ON pp.person_id = p.id
             WHERE pp.project_id = ?1",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(ProjectPerson {
                project_id: row.get(0)?,
                person_id: row.get(1)?,
                role: row.get(2)?,
                person_name: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_meetings_list(
        &self,
        project_id: i64,
    ) -> Result<Vec<crate::db::meetings::Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.title, m.meeting_date, m.transcript, m.notes, m.created_at
             FROM meetings m
             JOIN project_meetings pm ON m.id = pm.meeting_id
             WHERE pm.project_id = ?1
             ORDER BY m.meeting_date DESC",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(crate::db::meetings::Meeting {
                id: row.get(0)?,
                title: row.get(1)?,
                meeting_date: row.get(2)?,
                transcript: row.get(3)?,
                notes: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_link_repo(&self, project_id: i64, repo_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO project_repos (project_id, repo_id) VALUES (?1, ?2)",
            params![project_id, repo_id],
        )?;
        Ok(())
    }

    pub fn project_unlink_repo(&self, project_id: i64, repo_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM project_repos WHERE project_id = ?1 AND repo_id = ?2",
            params![project_id, repo_id],
        )?;
        Ok(n > 0)
    }

    pub fn project_repos_list(&self, project_id: i64) -> Result<Vec<crate::db::repos::RepoEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.path, r.name, r.description, r.db_path, r.last_indexed, r.created_at
             FROM repos r
             JOIN project_repos pr ON r.id = pr.repo_id
             WHERE pr.project_id = ?1
             ORDER BY r.name",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(crate::db::repos::RepoEntry {
                id: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                db_path: row.get(4)?,
                last_indexed: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let links_json: Option<String> = row.get(3)?;
    let links = links_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        links,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_add_get() {
        let db = Database::open_in_memory().unwrap();
        let links = vec![ProjectLink {
            url: "https://jira.example.com/PRJ".to_string(),
            label: Some("JIRA".to_string()),
        }];
        let id = db
            .project_add("My Project", Some("A great project"), &links)
            .unwrap();
        let p = db.project_get(id).unwrap().unwrap();
        assert_eq!(p.name, "My Project");
        assert_eq!(p.description.as_deref(), Some("A great project"));
        assert_eq!(p.links.len(), 1);
        assert_eq!(p.links[0].label.as_deref(), Some("JIRA"));
    }

    #[test]
    fn test_project_search() {
        let db = Database::open_in_memory().unwrap();
        db.project_add("Auth Overhaul", Some("Replacing old auth middleware"), &[])
            .unwrap();
        db.project_add("Dashboard v2", Some("Rebuild of main dashboard"), &[])
            .unwrap();

        let results = db.project_search("auth").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Auth Overhaul");
    }

    #[test]
    fn test_project_link_person_and_meeting() {
        let db = Database::open_in_memory().unwrap();
        let project_id = db.project_add("Proj", None, &[]).unwrap();
        let person_id = db.people_add("Bob", None).unwrap();
        let meeting_id = db.meeting_add("Kickoff", None, None, None).unwrap();

        db.project_link_person(project_id, person_id, Some("lead"))
            .unwrap();
        db.project_link_meeting(project_id, meeting_id).unwrap();

        let people = db.project_people(project_id).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].person_name, "Bob");

        let meetings = db.project_meetings_list(project_id).unwrap();
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Kickoff");
    }

    #[test]
    fn test_project_link_repos() {
        let db = Database::open_in_memory().unwrap();
        let project_id = db.project_add("Auth Platform", None, &[]).unwrap();
        let repo_a = db
            .repo_register(
                "/repos/auth-service",
                "auth-service",
                None,
                "/repos/auth-service/.sclerox/repo.db",
                None,
            )
            .unwrap();
        let repo_b = db
            .repo_register(
                "/repos/user-service",
                "user-service",
                None,
                "/repos/user-service/.sclerox/repo.db",
                None,
            )
            .unwrap();

        db.project_link_repo(project_id, repo_a).unwrap();
        db.project_link_repo(project_id, repo_b).unwrap();

        let repos = db.project_repos_list(project_id).unwrap();
        assert_eq!(repos.len(), 2);
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"auth-service"));
        assert!(names.contains(&"user-service"));
    }

    #[test]
    fn test_project_unlink_repo() {
        let db = Database::open_in_memory().unwrap();
        let project_id = db.project_add("Proj", None, &[]).unwrap();
        let repo_id = db
            .repo_register(
                "/repos/svc",
                "svc",
                None,
                "/repos/svc/.sclerox/repo.db",
                None,
            )
            .unwrap();

        db.project_link_repo(project_id, repo_id).unwrap();
        assert_eq!(db.project_repos_list(project_id).unwrap().len(), 1);

        assert!(db.project_unlink_repo(project_id, repo_id).unwrap());
        assert!(db.project_repos_list(project_id).unwrap().is_empty());
        assert!(!db.project_unlink_repo(project_id, repo_id).unwrap());
    }

    #[test]
    fn test_project_delete_cascades() {
        let db = Database::open_in_memory().unwrap();
        let project_id = db.project_add("ToDelete", None, &[]).unwrap();
        let person_id = db.people_add("Alice", None).unwrap();
        db.project_link_person(project_id, person_id, None).unwrap();

        db.project_delete(project_id).unwrap();
        assert!(db.project_get(project_id).unwrap().is_none());

        // Junction row should be gone
        let people = db.project_people(project_id).unwrap();
        assert!(people.is_empty());
    }
}
