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
    /// active | stale | superseded
    pub status: String,
    /// manual | claude-auto | session
    pub source: String,
    /// key of the memory that replaced this one
    pub superseded_by: Option<String>,
    pub reviewed_at: Option<String>,
}

const MEMORY_COLS: &str =
    "id, key, value, memory_type, tags, created_at, updated_at, status, source, superseded_by, reviewed_at";

impl Database {
    pub fn memory_set(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
    ) -> Result<i64> {
        self.memory_set_full(key, value, memory_type, tags, "manual")
    }

    /// Insert or update a memory entry with explicit source tracking.
    pub fn memory_set_full(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
        source: &str,
    ) -> Result<i64> {
        let tags_json = tags.map(|t| serde_json::to_string(t).unwrap());
        self.conn.execute(
            "INSERT INTO memory (key, value, memory_type, tags, source)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET
               value      = excluded.value,
               memory_type = excluded.memory_type,
               tags       = excluded.tags,
               status     = 'active',
               updated_at = datetime('now')",
            params![key, value, memory_type, tags_json, source],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn memory_get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let sql = format!("SELECT {MEMORY_COLS} FROM memory WHERE key = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        match stmt.query_row(params![key], row_to_memory) {
            Ok(e) => Ok(Some(e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List memories. Defaults to active only; pass status="all" to include stale/superseded.
    /// List memories. Defaults to active only; pass status="all" to include all.
    pub fn memory_list(
        &self,
        memory_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        // All four combinations: (type filter, status filter) each parameterized.
        let filter_status = !matches!(status, Some("all") | None);
        let active_only = status.is_none();

        let sql = format!("SELECT {MEMORY_COLS} FROM memory WHERE {} ORDER BY updated_at DESC",
            match (memory_type.is_some(), filter_status, active_only) {
                (true,  true,  _    ) => "memory_type = ?1 AND status = ?2",
                (true,  false, false) => "memory_type = ?1",
                (true,  false, true ) => "memory_type = ?1 AND status = 'active'",
                (false, true,  _    ) => "status = ?1",
                (false, false, false) => "1=1",
                (false, false, true ) => "status = 'active'",
            }
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match (memory_type, filter_status, active_only) {
            (Some(t), true,  _    ) => stmt.query_map(params![t, status.unwrap()], row_to_memory)?,
            (Some(t), false, _    ) => stmt.query_map(params![t], row_to_memory)?,
            (None,    true,  _    ) => stmt.query_map(params![status.unwrap()], row_to_memory)?,
            (None,    false, _    ) => stmt.query_map([], row_to_memory)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Search active memories by default. Pass status="all" to search everything.
    pub fn memory_search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        self.memory_search_filtered(query, "active")
    }

    pub fn memory_search_filtered(&self, query: &str, status: &str) -> Result<Vec<MemoryEntry>> {
        let fts_query = fts::sanitize(query);
        let like_pat = format!("%{query}%");
        let filter_status = status != "all";

        // Tier 1: FTS prefix matches
        let fts_sql = if filter_status {
            format!("SELECT {MEMORY_COLS} FROM memory
                     WHERE id IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?1)
                       AND status = ?2
                     ORDER BY updated_at DESC")
        } else {
            format!("SELECT {MEMORY_COLS} FROM memory
                     WHERE id IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?1)
                     ORDER BY updated_at DESC")
        };
        let mut stmt = self.conn.prepare(&fts_sql)?;
        let fts_hits: Vec<MemoryEntry> = if filter_status {
            stmt.query_map(params![fts_query, status], row_to_memory)?
        } else {
            stmt.query_map(params![fts_query], row_to_memory)?
        }
        .collect::<Result<Vec<_>, _>>()?;
        let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|m| m.id).collect();

        // Tier 2: LIKE substring fallback
        let like_sql = if filter_status {
            format!("SELECT {MEMORY_COLS} FROM memory
                     WHERE (key LIKE ?1 ESCAPE '\\' OR value LIKE ?1 ESCAPE '\\')
                       AND status = ?2
                     ORDER BY updated_at DESC")
        } else {
            format!("SELECT {MEMORY_COLS} FROM memory
                     WHERE (key LIKE ?1 ESCAPE '\\' OR value LIKE ?1 ESCAPE '\\')
                     ORDER BY updated_at DESC")
        };
        let mut stmt2 = self.conn.prepare(&like_sql)?;
        let like_extras: Vec<MemoryEntry> = if filter_status {
            stmt2.query_map(params![like_pat, status], row_to_memory)?
        } else {
            stmt2.query_map(params![like_pat], row_to_memory)?
        }
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|m| !fts_ids.contains(&m.id))
        .collect();

        let mut results = fts_hits;
        results.extend(like_extras);
        Ok(results)
    }

    pub fn memory_delete(&self, key: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memory WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    /// Mark a memory as stale (no longer reliable). Appends an optional reason to the value.
    /// Stale memories are excluded from default list/search but preserved for history.
    pub fn memory_stale(&self, key: &str, reason: Option<&str>) -> Result<bool> {
        let note = reason
            .map(|r| format!("\n[stale: {r}]"))
            .unwrap_or_default();
        let n = self.conn.execute(
            "UPDATE memory
             SET status = 'stale',
                 value = value || ?1,
                 updated_at = datetime('now')
             WHERE key = ?2 AND status = 'active'",
            params![note, key],
        )?;
        Ok(n > 0)
    }

    /// Replace an old memory with a new one in a single atomic operation.
    /// The old entry is marked superseded and its superseded_by field points to the new key.
    pub fn memory_supersede(
        &self,
        old_key: &str,
        new_key: &str,
        new_value: &str,
        memory_type: &str,
    ) -> Result<bool> {
        // Check old key exists
        let exists: bool = self
            .conn
            .query_row(
                "SELECT count(*) > 0 FROM memory WHERE key = ?1",
                params![old_key],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(false);
        }

        let tx = self.conn.unchecked_transaction()?;
        // Create new entry
        tx.execute(
            "INSERT INTO memory (key, value, memory_type, source)
             VALUES (?1, ?2, ?3, 'manual')
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               memory_type = excluded.memory_type,
               status = 'active',
               updated_at = datetime('now')",
            params![new_key, new_value, memory_type],
        )?;
        // Mark old as superseded
        tx.execute(
            "UPDATE memory
             SET status = 'superseded',
                 superseded_by = ?1,
                 updated_at = datetime('now')
             WHERE key = ?2",
            params![new_key, old_key],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Mark a memory as reviewed (you've confirmed it's still accurate).
    pub fn memory_review(&self, key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memory SET reviewed_at = datetime('now') WHERE key = ?1",
            params![key],
        )?;
        Ok(n > 0)
    }

    /// List active memories not reviewed in the last `days` days (or never reviewed).
    pub fn memory_review_needed(&self, days: u32) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory
             WHERE status = 'active'
               AND (reviewed_at IS NULL
                    OR reviewed_at < datetime('now', '-{days} days'))
             ORDER BY COALESCE(reviewed_at, created_at) ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_memory)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
            "SELECT p.id, p.name, p.notes, p.created_at, p.updated_at
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
                notes: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
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
        status: row.get(7).unwrap_or_else(|_| "active".to_string()),
        source: row.get(8).unwrap_or_else(|_| "manual".to_string()),
        superseded_by: row.get(9).unwrap_or(None),
        reviewed_at: row.get(10).unwrap_or(None),
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

        let user = db.memory_list(Some("user"), None).unwrap();
        assert_eq!(user.len(), 2);

        let all = db.memory_list(None, None).unwrap();
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
            .people_add("Alice", None)
            .unwrap();
        let bob = db
            .people_add("Bob", None)
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
            .people_add("Carol", None)
            .unwrap();
        assert!(!db.memory_link_person("no-such-key", person_id).unwrap());
    }

    #[test]
    fn test_memory_unlink_person() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v", "general", None).unwrap();
        let person_id = db
            .people_add("Dave", None)
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
