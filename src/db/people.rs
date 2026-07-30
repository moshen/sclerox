use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonIdentifier {
    pub id: i64,
    pub person_id: i64,
    /// Must exist in identifier_types.name.
    pub identifier_type: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifierType {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Default)]
pub struct PersonUpdate {
    pub name: Option<String>,
    pub notes: Option<Option<String>>,
}

impl Database {
    pub fn people_add(&self, name: &str, notes: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO people (name, notes) VALUES (?1, ?2)",
            params![name, notes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn people_get(&self, id: i64) -> Result<Option<Person>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, notes, created_at, updated_at FROM people WHERE id = ?1")?;
        match stmt.query_row(params![id], row_to_person) {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn people_list(&self) -> Result<Vec<Person>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, notes, created_at, updated_at FROM people ORDER BY name")?;
        let rows = stmt.query_map([], row_to_person)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Search people by name/notes (FTS) or by identifier value (LIKE).
    pub fn people_search(&self, query: &str) -> Result<Vec<Person>> {
        let fts_query = fts::sanitize(query);
        let like_pat = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, name, notes, created_at, updated_at FROM people
             WHERE id IN (SELECT rowid FROM people_fts WHERE people_fts MATCH ?1)
                OR id IN (SELECT person_id FROM people_identifiers WHERE identifier LIKE ?2)
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![fts_query, like_pat], row_to_person)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn people_update(&self, id: i64, update: PersonUpdate) -> Result<bool> {
        let mut sets = vec!["updated_at = datetime('now')"];
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![];

        if let Some(name) = update.name {
            sets.push("name = ?");
            values.push(Box::new(name));
        }
        if let Some(notes) = update.notes {
            sets.push("notes = ?");
            values.push(Box::new(notes));
        }

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

    // ---------- identifiers ----------

    /// Upsert an identifier for a person. Returns an error if `type_` is not in identifier_types.
    pub fn people_identifier_set(
        &self,
        person_id: i64,
        type_: &str,
        identifier: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO people_identifiers (person_id, type, identifier)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(person_id, type) DO UPDATE SET identifier = excluded.identifier",
            params![person_id, type_, identifier],
        )?;
        Ok(())
    }

    pub fn people_identifier_remove(&self, person_id: i64, type_: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM people_identifiers WHERE person_id = ?1 AND type = ?2",
            params![person_id, type_],
        )?;
        Ok(n > 0)
    }

    pub fn people_identifiers_for(&self, person_id: i64) -> Result<Vec<PersonIdentifier>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, person_id, type, identifier FROM people_identifiers
             WHERE person_id = ?1 ORDER BY type",
        )?;
        let rows = stmt.query_map(params![person_id], |r| {
            Ok(PersonIdentifier {
                id: r.get(0)?,
                person_id: r.get(1)?,
                identifier_type: r.get(2)?,
                identifier: r.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn people_identifier_get(&self, person_id: i64, type_: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT identifier FROM people_identifiers WHERE person_id = ?1 AND type = ?2",
            params![person_id, type_],
            |r| r.get(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ---------- identifier types ----------

    pub fn identifier_types_list(&self) -> Result<Vec<IdentifierType>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, description FROM identifier_types ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(IdentifierType {
                name: r.get(0)?,
                description: r.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn identifier_type_add(&self, name: &str, description: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO identifier_types (name, description) VALUES (?1, ?2)",
            params![name, description],
        )?;
        Ok(())
    }

    pub fn identifier_type_exists(&self, name: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM identifier_types WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }
}

fn row_to_person(row: &rusqlite::Row<'_>) -> rusqlite::Result<Person> {
    Ok(Person {
        id: row.get(0)?,
        name: row.get(1)?,
        notes: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_person(db: &Database, name: &str) -> i64 {
        db.people_add(name, None).unwrap()
    }

    #[test]
    fn test_people_add_get() {
        let db = Database::open_in_memory().unwrap();
        let id = db.people_add("Alice", Some("Great engineer")).unwrap();
        let p = db.people_get(id).unwrap().unwrap();
        assert_eq!(p.name, "Alice");
        assert_eq!(p.notes.as_deref(), Some("Great engineer"));
    }

    #[test]
    fn test_people_identifiers_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let id = make_person(&db, "Alice");
        db.people_identifier_set(id, "email", "alice@example.com")
            .unwrap();
        db.people_identifier_set(id, "github", "alicegit").unwrap();

        let idents = db.people_identifiers_for(id).unwrap();
        assert_eq!(idents.len(), 2);
        assert_eq!(
            db.people_identifier_get(id, "email").unwrap().as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(
            db.people_identifier_get(id, "github").unwrap().as_deref(),
            Some("alicegit")
        );
    }

    #[test]
    fn test_people_identifier_upsert() {
        let db = Database::open_in_memory().unwrap();
        let id = make_person(&db, "Bob");
        db.people_identifier_set(id, "email", "old@b.com").unwrap();
        db.people_identifier_set(id, "email", "new@b.com").unwrap();
        assert_eq!(
            db.people_identifier_get(id, "email").unwrap().as_deref(),
            Some("new@b.com")
        );
    }

    #[test]
    fn test_people_identifier_remove() {
        let db = Database::open_in_memory().unwrap();
        let id = make_person(&db, "Carol");
        db.people_identifier_set(id, "slack", "U123").unwrap();
        assert!(db.people_identifier_remove(id, "slack").unwrap());
        assert!(!db.people_identifier_remove(id, "slack").unwrap());
        assert!(db.people_identifiers_for(id).unwrap().is_empty());
    }

    #[test]
    fn test_people_search_by_identifier() {
        let db = Database::open_in_memory().unwrap();
        let id = make_person(&db, "Dave");
        db.people_identifier_set(id, "email", "abc@example.com")
            .unwrap();

        // Search by email value finds the person
        let results = db.people_search("abc@example.com").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Dave");
    }

    #[test]
    fn test_identifier_type_unknown_rejected() {
        let db = Database::open_in_memory().unwrap();
        let id = db.people_add("Eve", None).unwrap();
        // Unknown type should fail due to FK constraint
        assert!(db.people_identifier_set(id, "telegram", "eve123").is_err());
    }

    #[test]
    fn test_identifier_type_add_then_use() {
        let db = Database::open_in_memory().unwrap();
        let id = db.people_add("Frank", None).unwrap();
        db.identifier_type_add("telegram", Some("Telegram handle"))
            .unwrap();
        db.people_identifier_set(id, "telegram", "@frankbot")
            .unwrap();
        assert_eq!(
            db.people_identifier_get(id, "telegram").unwrap().as_deref(),
            Some("@frankbot")
        );
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

    #[test]
    fn test_identifier_types_seeded() {
        let db = Database::open_in_memory().unwrap();
        let types = db.identifier_types_list().unwrap();
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"email"));
        assert!(names.contains(&"github"));
        assert!(names.contains(&"slack"));
        assert!(names.contains(&"atlassian"));
    }
}
