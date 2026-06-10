use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
    pub slack_id: Option<String>,
    pub slack_url: Option<String>,
    pub github_username: Option<String>,
    pub github_url: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default)]
pub struct PersonUpdate {
    pub name: Option<String>,
    pub email: Option<Option<String>>,
    pub slack_id: Option<Option<String>>,
    pub slack_url: Option<Option<String>>,
    pub github_username: Option<Option<String>>,
    pub github_url: Option<Option<String>>,
    pub notes: Option<Option<String>>,
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub fn people_add(
        &self,
        name: &str,
        email: Option<&str>,
        slack_id: Option<&str>,
        slack_url: Option<&str>,
        github_username: Option<&str>,
        github_url: Option<&str>,
        notes: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO people (name, email, slack_id, slack_url, github_username, github_url, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, email, slack_id, slack_url, github_username, github_url, notes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn people_get(&self, id: i64) -> Result<Option<Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, email, slack_id, slack_url, github_username, github_url, notes, created_at, updated_at
             FROM people WHERE id = ?1",
        )?;
        match stmt.query_row(params![id], row_to_person) {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn people_list(&self) -> Result<Vec<Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, email, slack_id, slack_url, github_username, github_url, notes, created_at, updated_at
             FROM people ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_person)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn people_search(&self, query: &str) -> Result<Vec<Person>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, email, slack_id, slack_url, github_username, github_url, notes, created_at, updated_at
             FROM people
             WHERE id IN (SELECT rowid FROM people_fts WHERE people_fts MATCH ?1)
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![query], row_to_person)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn people_update(&self, id: i64, update: PersonUpdate) -> Result<bool> {
        let mut sets = vec!["updated_at = datetime('now')"];
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![];

        macro_rules! push_field {
            ($field:expr, $col:expr) => {
                if let Some(v) = $field {
                    sets.push($col);
                    values.push(Box::new(v));
                }
            };
        }

        if let Some(name) = update.name {
            sets.push("name = ?");
            values.push(Box::new(name));
        }
        push_field!(update.email, "email = ?");
        push_field!(update.slack_id, "slack_id = ?");
        push_field!(update.slack_url, "slack_url = ?");
        push_field!(update.github_username, "github_username = ?");
        push_field!(update.github_url, "github_url = ?");
        push_field!(update.notes, "notes = ?");

        if sets.len() == 1 {
            return Ok(false);
        }

        let sql = format!("UPDATE people SET {} WHERE id = ?", sets.join(", "));
        let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let mut all_params: Vec<&dyn rusqlite::ToSql> = refs;
        let id_val: i64 = id;
        all_params.push(&id_val);

        let n = self.conn.execute(&sql, all_params.as_slice())?;
        Ok(n > 0)
    }

    pub fn people_delete(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM people WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }
}

fn row_to_person(row: &rusqlite::Row<'_>) -> rusqlite::Result<Person> {
    Ok(Person {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_person(db: &Database, name: &str) -> i64 {
        db.people_add(
            name,
            Some(&format!("{name}@example.com")),
            None,
            None,
            Some(&name.to_lowercase()),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_people_add_get() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .people_add(
                "Alice",
                Some("alice@example.com"),
                Some("U123"),
                Some("https://example.slack.com/team/U123"),
                Some("alicegit"),
                Some("https://github.com/alicegit"),
                Some("Great engineer"),
            )
            .unwrap();
        let p = db.people_get(id).unwrap().unwrap();
        assert_eq!(p.name, "Alice");
        assert_eq!(p.email.as_deref(), Some("alice@example.com"));
        assert_eq!(p.github_username.as_deref(), Some("alicegit"));
    }

    #[test]
    fn test_people_list_ordered() {
        let db = Database::open_in_memory().unwrap();
        make_person(&db, "Zara");
        make_person(&db, "Alice");
        make_person(&db, "Bob");

        let people = db.people_list().unwrap();
        assert_eq!(people.len(), 3);
        assert_eq!(people[0].name, "Alice");
        assert_eq!(people[1].name, "Bob");
        assert_eq!(people[2].name, "Zara");
    }

    #[test]
    fn test_people_search() {
        let db = Database::open_in_memory().unwrap();
        make_person(&db, "Alice");
        make_person(&db, "Bob");

        let results = db.people_search("alice").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Alice");
    }

    #[test]
    fn test_people_delete() {
        let db = Database::open_in_memory().unwrap();
        let id = make_person(&db, "Temp");
        assert!(db.people_delete(id).unwrap());
        assert!(db.people_get(id).unwrap().is_none());
    }
}
