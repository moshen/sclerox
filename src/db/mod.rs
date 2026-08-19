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
use std::sync::Once;

use migrations::{Migration, PRIMARY_MIGRATIONS, PRIMARY_VERSION};

static VEC_INIT: Once = Once::new();

/// Register the sqlite-vec extension so `vec0` virtual tables are available on
/// every connection opened AFTER this call. Idempotent — call before each
/// `Connection::open` (primary and per-repo). Must run before any migration
/// that creates a vec0 table.
pub fn register_vec_extension() {
    VEC_INIT.call_once(|| {
        // SAFETY: sqlite3_vec_init matches the C entry-point signature that
        // sqlite3_auto_extension expects; registration is process-global and
        // one-time.
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

pub struct Database {
    pub conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        log::debug!("opening primary db: {}", path.display());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        register_vec_extension();
        let conn = Connection::open(path)?;
        // Enforce the schema's FOREIGN KEY / ON DELETE CASCADE constraints.
        // This pragma is per-connection and OFF by default; it must be set
        // outside a transaction (open is), so set it before init/migrations.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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
        register_vec_extension();
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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

/// Serialize a float vector to little-endian bytes for BLOB storage. This is
/// exactly the layout sqlite-vec's `float[N]` columns consume, so the stored
/// BLOB doubles as the vec0 index input with no conversion.
pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
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
    fn test_migration_v12_converts_supersession_and_relaxes_key_uniqueness() {
        register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(schema::PRIMARY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        run_migrations(&conn, 1, PRIMARY_MIGRATIONS, 11).unwrap();

        // Legacy shape: key-based pointers, plus one resurrected row (active
        // but still carrying a stale pointer — the corruption v12 fixes).
        conn.execute_batch(
            "INSERT INTO memory (key, value, status, superseded_by) VALUES
                 ('old', 'v1', 'superseded', 'new'),
                 ('new', 'v2', 'active', NULL),
                 ('zombie', 'v3', 'active', 'new'),
                 ('dangling', 'v4', 'superseded', 'deleted-key');
             INSERT INTO people (name) VALUES ('Alice');
             INSERT INTO memory_people (memory_id, person_id)
                 SELECT m.id, 1 FROM memory m WHERE m.key = 'new';",
        )
        .unwrap();

        run_migrations(&conn, 11, PRIMARY_MIGRATIONS, 12).unwrap();
        let db = Database { conn };

        // Key pointer became an id pointer.
        let new = db.memory_get("new").unwrap().unwrap();
        let old = db.memory_get("old").unwrap().unwrap();
        assert_eq!(old.superseded_by, Some(new.id));
        // Active rows never carry a pointer.
        let zombie = db.memory_get("zombie").unwrap().unwrap();
        assert_eq!(zombie.superseded_by, None);
        // A pointer at a since-deleted key becomes NULL rather than garbage.
        let dangling = db.memory_get("dangling").unwrap().unwrap();
        assert_eq!(dangling.superseded_by, None);
        // memory_people rows survive the rebuild.
        assert_eq!(db.memory_people("new").unwrap().len(), 1);
        // Key uniqueness is active-only: a second ACTIVE row under an active
        // key is rejected, but reusing a retired key inserts a fresh row.
        assert!(db
            .conn
            .execute("INSERT INTO memory (key, value) VALUES ('new', 'x')", [])
            .is_err());
        db.conn
            .execute("INSERT INTO memory (key, value) VALUES ('old', 'x')", [])
            .unwrap();
        // FTS stayed consistent through the rebuild.
        assert_eq!(db.memory_search("v2").unwrap().len(), 1);
    }

    #[test]
    fn test_embedding_to_bytes_le_layout() {
        // sqlite-vec float[N] consumes a contiguous little-endian f32 array;
        // verify our serialization matches that exact layout.
        let v = vec![1.0f32, -0.5, 0.25];
        let bytes = embedding_to_bytes(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-0.5f32).to_le_bytes());
        assert_eq!(&bytes[8..12], &0.25f32.to_le_bytes());
    }
}
