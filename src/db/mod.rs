pub mod fts;
pub mod investigations;
pub mod meetings;
pub mod memory;
pub mod migrations;
pub mod people;
pub mod projects;
pub mod repos;
pub mod schema;
pub mod todos;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use migrations::{Migration, PRIMARY_MIGRATIONS, PRIMARY_VERSION};

pub struct Database {
    pub conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        log::debug!("opening primary db: {}", path.display());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init()?;
        log::debug!(
            "primary db ready (schema v{})",
            db.user_version().unwrap_or(0)
        );
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        let current = self.user_version()?;
        if current == 0 {
            // Fresh database: apply the v1 baseline schema, then run all migrations in order.
            // This ensures fresh and upgraded databases end up in exactly the same state.
            self.conn.execute_batch(schema::PRIMARY_SCHEMA)?;
            self.set_user_version(1)?;
        }
        // Run any pending migrations (including those just queued for a fresh DB).
        let current = self.user_version()?;
        run_migrations(&self.conn, current, PRIMARY_MIGRATIONS, PRIMARY_VERSION)
            .context("primary database migration failed")?;
        Ok(())
    }

    pub fn user_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn set_user_version(&self, version: u32) -> Result<()> {
        // PRAGMA values cannot be bound as parameters.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {version}"))?;
        Ok(())
    }

    /// Returns `(current_version, target_version, pending_count)`.
    pub fn migration_status(&self) -> Result<(u32, u32, usize)> {
        let current = self.user_version()?;
        let pending = PRIMARY_MIGRATIONS
            .iter()
            .filter(|m| m.version > current)
            .count();
        Ok((current, PRIMARY_VERSION, pending))
    }
}

/// Apply all migrations in `migrations` whose version is between
/// `current + 1` and `target` (inclusive), each in its own transaction.
pub fn run_migrations(
    conn: &Connection,
    current: u32,
    migrations: &[Migration],
    target: u32,
) -> Result<()> {
    for m in migrations {
        if m.version <= current || m.version > target {
            continue;
        }
        log::info!("applying migration v{}: {}", m.version, m.description);
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(m.sql)
            .with_context(|| format!("migration v{} ({}) failed", m.version, m.description))?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", m.version))?;
        tx.commit()?;
        log::debug!("migration v{} applied", m.version);
    }
    Ok(())
}

/// Serialize a float vector to little-endian bytes for BLOB storage.
pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize BLOB bytes back to a float vector.
pub fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let db = Database::open_in_memory().unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn test_fresh_db_is_at_current_version() {
        let db = Database::open_in_memory().unwrap();
        let version = db.user_version().unwrap();
        assert_eq!(
            version, PRIMARY_VERSION,
            "fresh DB should be at current version"
        );
    }

    #[test]
    fn test_migration_status_on_fresh_db() {
        let db = Database::open_in_memory().unwrap();
        let (current, target, pending) = db.migration_status().unwrap();
        assert_eq!(current, target);
        assert_eq!(pending, 0);
    }

    #[test]
    fn test_run_migrations_applies_in_order() {
        use migrations::Migration;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::PRIMARY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();

        // Simulate two future migrations
        let migrations = &[
            Migration {
                version: 2,
                description: "add column a",
                sql: "ALTER TABLE memory ADD COLUMN test_col_a TEXT;",
            },
            Migration {
                version: 3,
                description: "add column b",
                sql: "ALTER TABLE memory ADD COLUMN test_col_b TEXT;",
            },
        ];

        run_migrations(&conn, 1, migrations, 3).unwrap();

        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);

        // Verify both columns were added
        conn.execute(
            "INSERT INTO memory (key, value, test_col_a, test_col_b) VALUES ('k', 'v', 'x', 'y')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn test_run_migrations_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::PRIMARY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();

        let migrations = &[Migration {
            version: 2,
            description: "add test col",
            sql: "ALTER TABLE memory ADD COLUMN idem_test TEXT;",
        }];

        run_migrations(&conn, 1, migrations, 2).unwrap();
        // Running again with current=2 should skip all migrations
        run_migrations(&conn, 2, migrations, 2).unwrap();

        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn test_migration_rollback_on_failure() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::PRIMARY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();

        let bad_migrations = &[Migration {
            version: 2,
            description: "intentionally broken",
            sql: "THIS IS NOT VALID SQL;",
        }];

        assert!(
            run_migrations(&conn, 1, bad_migrations, 2).is_err(),
            "bad migration should return an error"
        );

        // Version should still be 1 - the failed migration was rolled back
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "version must not advance on failed migration");
    }

    #[test]
    fn test_embedding_roundtrip() {
        let original = vec![1.0f32, -0.5, 0.123456, f32::MAX, f32::MIN_POSITIVE];
        let bytes = embedding_to_bytes(&original);
        let recovered = bytes_to_embedding(&bytes);
        for (a, b) in original.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < f32::EPSILON);
        }
    }
}
