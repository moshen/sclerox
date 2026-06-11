use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub memory_type: String,
    pub tags: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
}

impl Database {
    pub fn memory_set(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
    ) -> Result<i64> {
        let tags_json = tags.map(|t| serde_json::to_string(t).unwrap());
        self.conn.execute(
            "INSERT INTO memory (key, value, memory_type, tags)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               memory_type = excluded.memory_type,
               tags = excluded.tags,
               updated_at = datetime('now')",
            params![key, value, memory_type, tags_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn memory_get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, key, value, memory_type, tags, created_at, updated_at
             FROM memory WHERE key = ?1",
        )?;
        let result = stmt.query_row(params![key], row_to_memory);
        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn memory_list(&self, memory_type: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let sql = match memory_type {
            Some(_) => {
                "SELECT id, key, value, memory_type, tags, created_at, updated_at
                        FROM memory WHERE memory_type = ?1 ORDER BY updated_at DESC"
            }
            None => {
                "SELECT id, key, value, memory_type, tags, created_at, updated_at
                     FROM memory ORDER BY updated_at DESC"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if let Some(t) = memory_type {
            stmt.query_map(params![t], row_to_memory)?
        } else {
            stmt.query_map([], row_to_memory)?
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn memory_search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, key, value, memory_type, tags, created_at, updated_at
             FROM memory
             WHERE id IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?1)
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![query], row_to_memory)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn memory_delete(&self, key: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memory WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    pub fn memory_link_person(&self, memory_key: &str, person_id: i64) -> Result<bool> {
        let memory_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1",
                params![memory_key],
                |r| r.get(0),
            )
            .ok();
        let Some(memory_id) = memory_id else {
            return Ok(false);
        };
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_people (memory_id, person_id) VALUES (?1, ?2)",
            params![memory_id, person_id],
        )?;
        Ok(true)
    }

    pub fn memory_unlink_person(&self, memory_key: &str, person_id: i64) -> Result<bool> {
        let memory_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1",
                params![memory_key],
                |r| r.get(0),
            )
            .ok();
        let Some(memory_id) = memory_id else {
            return Ok(false);
        };
        let n = self.conn.execute(
            "DELETE FROM memory_people WHERE memory_id = ?1 AND person_id = ?2",
            params![memory_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn memory_people(&self, memory_key: &str) -> Result<Vec<crate::db::people::Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.email, p.slack_id, p.slack_url,
                    p.github_username, p.github_url, p.notes, p.created_at, p.updated_at
             FROM people p
             JOIN memory_people mp ON p.id = mp.person_id
             JOIN memory m ON mp.memory_id = m.id
             WHERE m.key = ?1
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![memory_key], |row| {
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
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let tags_json: Option<String> = row.get(4)?;
    let tags = tags_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    Ok(MemoryEntry {
        id: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        memory_type: row.get(3)?,
        tags,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_set_get() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("test_key", "test value", "general", None)
            .unwrap();
        let entry = db.memory_get("test_key").unwrap().unwrap();
        assert_eq!(entry.key, "test_key");
        assert_eq!(entry.value, "test value");
        assert_eq!(entry.memory_type, "general");
    }

    #[test]
    fn test_memory_upsert() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v1", "user", None).unwrap();
        db.memory_set("k", "v2", "feedback", None).unwrap();
        let entry = db.memory_get("k").unwrap().unwrap();
        assert_eq!(entry.value, "v2");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn test_memory_list_by_type() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "val", "user", None).unwrap();
        db.memory_set("b", "val", "feedback", None).unwrap();
        db.memory_set("c", "val", "user", None).unwrap();

        let user = db.memory_list(Some("user")).unwrap();
        assert_eq!(user.len(), 2);

        let all = db.memory_list(None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_memory_search() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set(
            "rust-pref",
            "prefer Rust for systems code",
            "feedback",
            None,
        )
        .unwrap();
        db.memory_set("py-pref", "use Python for scripts", "feedback", None)
            .unwrap();

        let results = db.memory_search("Rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "rust-pref");
    }

    #[test]
    fn test_memory_delete() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("del_me", "x", "general", None).unwrap();
        assert!(db.memory_delete("del_me").unwrap());
        assert!(!db.memory_delete("del_me").unwrap());
        assert!(db.memory_get("del_me").unwrap().is_none());
    }

    #[test]
    fn test_memory_link_people() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("auth-pref", "prefer JWT", "project", None)
            .unwrap();
        let alice = db
            .people_add("Alice", Some("alice@x.com"), None, None, None, None, None)
            .unwrap();
        let bob = db
            .people_add("Bob", None, None, None, None, None, None)
            .unwrap();

        assert!(db.memory_link_person("auth-pref", alice).unwrap());
        assert!(db.memory_link_person("auth-pref", bob).unwrap());

        let people = db.memory_people("auth-pref").unwrap();
        assert_eq!(people.len(), 2);
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Alice"));
        assert!(names.contains(&"Bob"));
    }

    #[test]
    fn test_memory_link_missing_key_returns_false() {
        let db = Database::open_in_memory().unwrap();
        let person_id = db
            .people_add("Carol", None, None, None, None, None, None)
            .unwrap();
        assert!(!db.memory_link_person("no-such-key", person_id).unwrap());
    }

    #[test]
    fn test_memory_unlink_person() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v", "general", None).unwrap();
        let person_id = db
            .people_add("Dave", None, None, None, None, None, None)
            .unwrap();

        db.memory_link_person("k", person_id).unwrap();
        assert_eq!(db.memory_people("k").unwrap().len(), 1);

        assert!(db.memory_unlink_person("k", person_id).unwrap());
        assert!(db.memory_people("k").unwrap().is_empty());
        assert!(!db.memory_unlink_person("k", person_id).unwrap());
    }

    #[test]
    fn test_memory_tags() {
        let db = Database::open_in_memory().unwrap();
        let tags = vec!["rust".to_string(), "systems".to_string()];
        db.memory_set("tagged", "some value", "reference", Some(&tags))
            .unwrap();
        let entry = db.memory_get("tagged").unwrap().unwrap();
        assert_eq!(entry.tags.unwrap(), tags);
    }
}
