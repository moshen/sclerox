use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Open,
    Done,
    Watch,
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TodoStatus::Open => "open",
            TodoStatus::Done => "done",
            TodoStatus::Watch => "watch",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub notes: Option<String>,
    pub status: String,
    pub source_url: Option<String>,
    pub category: String,
    pub originated_date: String,
    pub deadline_date: Option<String>,
    pub completed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub fn todo_add(
        &self,
        title: &str,
        notes: Option<&str>,
        status: TodoStatus,
        source_url: Option<&str>,
        category: &str,
        originated_date: Option<&str>,
        deadline_date: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO todos (title, notes, status, source_url, category, originated_date, deadline_date)
             VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, date('now')), ?7)",
            params![title, notes, status.as_str(), source_url, category, originated_date, deadline_date],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn todo_get(&self, id: i64) -> Result<Option<Todo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, notes, status, source_url, category, originated_date,
                    deadline_date, completed_at, created_at, updated_at
             FROM todos WHERE id = ?1",
        )?;
        match stmt.query_row(params![id], row_to_todo) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn todo_list(&self, status_filter: Option<&str>) -> Result<Vec<Todo>> {
        let (sql, use_filter) = match status_filter {
            Some("all") | None => (
                "SELECT id, title, notes, status, source_url, category, originated_date,
                        deadline_date, completed_at, created_at, updated_at
                 FROM todos ORDER BY deadline_date ASC NULLS LAST, originated_date ASC",
                false,
            ),
            Some(_) => (
                "SELECT id, title, notes, status, source_url, category, originated_date,
                        deadline_date, completed_at, created_at, updated_at
                 FROM todos WHERE status = ?1
                 ORDER BY deadline_date ASC NULLS LAST, originated_date ASC",
                true,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if use_filter {
            stmt.query_map(params![status_filter.unwrap()], row_to_todo)?
        } else {
            stmt.query_map([], row_to_todo)?
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn todo_search(&self, query: &str) -> Result<Vec<Todo>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, title, notes, status, source_url, category, originated_date,
                    deadline_date, completed_at, created_at, updated_at
             FROM todos
             WHERE id IN (SELECT rowid FROM todos_fts WHERE todos_fts MATCH ?1)
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![query], row_to_todo)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Mark a todo done and record a resolution note in notes.
    pub fn todo_done(&self, id: i64, resolution: Option<&str>) -> Result<bool> {
        let note_append = resolution
            .map(|r| format!("\nResolved: {r}"))
            .unwrap_or_default();
        let n = self.conn.execute(
            "UPDATE todos SET
                status = 'done',
                notes = COALESCE(notes, '') || ?1,
                completed_at = datetime('now'),
                updated_at = datetime('now')
             WHERE id = ?2 AND status != 'done'",
            params![note_append, id],
        )?;
        Ok(n > 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn todo_update(
        &self,
        id: i64,
        title: Option<&str>,
        notes: Option<&str>,
        source_url: Option<&str>,
        deadline_date: Option<&str>,
        category: Option<&str>,
    ) -> Result<bool> {
        let mut parts = vec!["updated_at = datetime('now')".to_string()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        let mut idx = 1usize;

        macro_rules! push {
            ($opt:expr, $col:expr) => {
                if let Some(v) = $opt {
                    parts.push(format!("{} = ?{idx}", $col));
                    vals.push(Box::new(v.to_string()));
                    idx += 1;
                }
            };
        }
        push!(title, "title");
        push!(notes, "notes");
        push!(source_url, "source_url");
        push!(deadline_date, "deadline_date");
        push!(category, "category");

        if parts.len() == 1 {
            return Ok(false);
        }
        let sql = format!("UPDATE todos SET {} WHERE id = ?{idx}", parts.join(", "));
        vals.push(Box::new(id));
        let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|v| v.as_ref()).collect();
        Ok(self.conn.execute(&sql, refs.as_slice())? > 0)
    }

    pub fn todo_set_status(&self, id: i64, status: TodoStatus) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE todos SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![status.as_str(), id],
        )?;
        Ok(n > 0)
    }

    pub fn todo_delete(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM todos WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    pub fn todo_link_person(&self, todo_id: i64, person_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO todo_people (todo_id, person_id) VALUES (?1, ?2)",
            params![todo_id, person_id],
        )?;
        Ok(())
    }

    pub fn todo_unlink_person(&self, todo_id: i64, person_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM todo_people WHERE todo_id = ?1 AND person_id = ?2",
            params![todo_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn todo_people(&self, todo_id: i64) -> Result<Vec<crate::db::people::Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.email, p.slack_id, p.slack_url,
                    p.github_username, p.github_url, p.notes, p.created_at, p.updated_at
             FROM people p
             JOIN todo_people tp ON p.id = tp.person_id
             WHERE tp.todo_id = ?1
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![todo_id], |row| {
            Ok(crate::db::people::Person {
                id: row.get(0)?,
                name: row.get(1)?,
                email: row.get(2)?,
                slack_id: row.get(3)?,
                slack_url: row.get(4)?,
                github_username: row.get(5)?,
                github_url: row.get(6)?,
                notes: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn todo_link_project(&self, todo_id: i64, project_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO todo_projects (todo_id, project_id) VALUES (?1, ?2)",
            params![todo_id, project_id],
        )?;
        Ok(())
    }

    pub fn todo_unlink_project(&self, todo_id: i64, project_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM todo_projects WHERE todo_id = ?1 AND project_id = ?2",
            params![todo_id, project_id],
        )?;
        Ok(n > 0)
    }

    pub fn todo_projects(&self, todo_id: i64) -> Result<Vec<crate::db::projects::Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description, p.links, p.created_at, p.updated_at
             FROM projects p
             JOIN todo_projects tp ON p.id = tp.project_id
             WHERE tp.todo_id = ?1
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![todo_id], |row| {
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

    /// History: search completed items, optionally filtered by date range.
    pub fn todo_history(
        &self,
        query: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<Todo>> {
        // Build extra conditions using qualified column names.
        let mut extra = vec!["t.status = 'done'".to_string()];
        if let Some(f) = from {
            extra.push(format!("t.completed_at >= '{f}'"));
        }
        if let Some(t_val) = to {
            extra.push(format!("t.completed_at <= '{t_val}'"));
        }
        let extra_clause = extra.join(" AND ");

        // Strip the "t." prefix for plain queries (no join).
        let plain_clause = extra_clause.replace("t.", "");

        if let Some(q) = query {
            let q = fts::sanitize(q);
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id, title, notes, status, source_url, category, originated_date,
                        deadline_date, completed_at, created_at, updated_at
                 FROM todos
                 WHERE id IN (SELECT rowid FROM todos_fts WHERE todos_fts MATCH ?1)
                   AND {plain_clause}
                 ORDER BY completed_at DESC"
            ))?;
            let rows = stmt.query_map(params![q], row_to_todo)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        } else {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id, title, notes, status, source_url, category, originated_date,
                        deadline_date, completed_at, created_at, updated_at
                 FROM todos WHERE {plain_clause}
                 ORDER BY completed_at DESC"
            ))?;
            let rows = stmt.query_map([], row_to_todo)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }
    }
}

fn row_to_todo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: row.get(0)?,
        title: row.get(1)?,
        notes: row.get(2)?,
        status: row.get(3)?,
        source_url: row.get(4)?,
        category: row.get(5)?,
        originated_date: row.get(6)?,
        deadline_date: row.get(7)?,
        completed_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn test_todo_add_get() {
        let db = db();
        let id = db
            .todo_add(
                "Fix the login bug",
                None,
                TodoStatus::Open,
                None,
                "github",
                None,
                None,
            )
            .unwrap();
        let todo = db.todo_get(id).unwrap().unwrap();
        assert_eq!(todo.title, "Fix the login bug");
        assert_eq!(todo.status, "open");
        assert_eq!(todo.category, "github");
    }

    #[test]
    fn test_todo_done_records_resolution() {
        let db = db();
        let id = db
            .todo_add(
                "Review PR #123",
                None,
                TodoStatus::Open,
                None,
                "github",
                None,
                None,
            )
            .unwrap();
        assert!(db.todo_done(id, Some("Approved and merged")).unwrap());
        let todo = db.todo_get(id).unwrap().unwrap();
        assert_eq!(todo.status, "done");
        assert!(todo.completed_at.is_some());
        assert!(todo.notes.unwrap().contains("Approved and merged"));
    }

    #[test]
    fn test_todo_done_is_idempotent() {
        let db = db();
        let id = db
            .todo_add("Task", None, TodoStatus::Open, None, "general", None, None)
            .unwrap();
        assert!(db.todo_done(id, None).unwrap());
        // Second done call should return false (already done)
        assert!(!db.todo_done(id, None).unwrap());
    }

    #[test]
    fn test_todo_list_filter() {
        let db = db();
        db.todo_add(
            "Open 1",
            None,
            TodoStatus::Open,
            None,
            "general",
            None,
            None,
        )
        .unwrap();
        db.todo_add("Open 2", None, TodoStatus::Open, None, "slack", None, None)
            .unwrap();
        let id = db
            .todo_add(
                "To watch",
                None,
                TodoStatus::Watch,
                None,
                "general",
                None,
                None,
            )
            .unwrap();
        db.todo_done(id, None).unwrap();

        let open = db.todo_list(Some("open")).unwrap();
        assert_eq!(open.len(), 2);

        let all = db.todo_list(Some("all")).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_todo_search_fts() {
        let db = db();
        db.todo_add(
            "Fix authentication bug in API",
            None,
            TodoStatus::Open,
            None,
            "github",
            None,
            None,
        )
        .unwrap();
        db.todo_add(
            "Update deployment pipeline",
            None,
            TodoStatus::Open,
            None,
            "general",
            None,
            None,
        )
        .unwrap();

        let results = db.todo_search("authentication").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].title.contains("authentication"));
    }

    #[test]
    fn test_todo_history() {
        let db = db();
        let id1 = db
            .todo_add(
                "Done task 1",
                None,
                TodoStatus::Open,
                None,
                "general",
                None,
                None,
            )
            .unwrap();
        let id2 = db
            .todo_add(
                "Done task 2",
                None,
                TodoStatus::Open,
                None,
                "slack",
                None,
                None,
            )
            .unwrap();
        db.todo_add(
            "Still open",
            None,
            TodoStatus::Open,
            None,
            "general",
            None,
            None,
        )
        .unwrap();

        db.todo_done(id1, Some("resolved via PR")).unwrap();
        db.todo_done(id2, None).unwrap();

        let history = db.todo_history(None, None, None).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|t| t.status == "done"));
    }

    #[test]
    fn test_todo_history_search() {
        let db = db();
        let id = db
            .todo_add(
                "Migrate database schema",
                None,
                TodoStatus::Open,
                None,
                "general",
                None,
                None,
            )
            .unwrap();
        db.todo_done(id, Some("applied migration v2")).unwrap();
        db.todo_add(
            "Review PR",
            None,
            TodoStatus::Open,
            None,
            "github",
            None,
            None,
        )
        .unwrap();

        let results = db.todo_history(Some("migration"), None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].title.contains("Migrate"));
    }

    #[test]
    fn test_todo_update() {
        let db = db();
        let id = db
            .todo_add(
                "Original title",
                None,
                TodoStatus::Open,
                None,
                "general",
                None,
                None,
            )
            .unwrap();

        assert!(db
            .todo_update(
                id,
                Some("Updated title"),
                Some("some notes"),
                None,
                None,
                None
            )
            .unwrap());

        let todo = db.todo_get(id).unwrap().unwrap();
        assert_eq!(todo.title, "Updated title");
        assert_eq!(todo.notes.as_deref(), Some("some notes"));
    }

    #[test]
    fn test_todo_update_no_changes_returns_false() {
        let db = db();
        let id = db
            .todo_add("Task", None, TodoStatus::Open, None, "general", None, None)
            .unwrap();
        assert!(!db.todo_update(id, None, None, None, None, None).unwrap());
    }

    #[test]
    fn test_todo_link_people() {
        let db = db();
        let todo_id = db
            .todo_add(
                "Review PR",
                None,
                TodoStatus::Open,
                None,
                "github",
                None,
                None,
            )
            .unwrap();
        let alice = db
            .people_add("Alice", Some("alice@x.com"), None, None, None, None, None)
            .unwrap();
        let bob = db
            .people_add("Bob", Some("bob@x.com"), None, None, None, None, None)
            .unwrap();

        db.todo_link_person(todo_id, alice).unwrap();
        db.todo_link_person(todo_id, bob).unwrap();

        let people = db.todo_people(todo_id).unwrap();
        assert_eq!(people.len(), 2);
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Alice"));
        assert!(names.contains(&"Bob"));
    }

    #[test]
    fn test_todo_unlink_person() {
        let db = db();
        let todo_id = db
            .todo_add("Task", None, TodoStatus::Open, None, "general", None, None)
            .unwrap();
        let person_id = db
            .people_add("Carol", None, None, None, None, None, None)
            .unwrap();

        db.todo_link_person(todo_id, person_id).unwrap();
        assert_eq!(db.todo_people(todo_id).unwrap().len(), 1);

        assert!(db.todo_unlink_person(todo_id, person_id).unwrap());
        assert!(db.todo_people(todo_id).unwrap().is_empty());

        // Second remove returns false
        assert!(!db.todo_unlink_person(todo_id, person_id).unwrap());
    }

    #[test]
    fn test_todo_delete_cascades_people() {
        let db = db();
        let todo_id = db
            .todo_add("Task", None, TodoStatus::Open, None, "general", None, None)
            .unwrap();
        let person_id = db
            .people_add("Dave", None, None, None, None, None, None)
            .unwrap();
        db.todo_link_person(todo_id, person_id).unwrap();

        db.todo_delete(todo_id).unwrap();
        // Junction row should be gone (CASCADE)
        assert!(db.todo_people(todo_id).unwrap().is_empty());
    }
}
