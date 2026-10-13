pub mod fts;
pub mod investigations;
pub mod meetings;
pub mod memory;
pub mod migrations;
pub mod people;
pub mod projects;
pub mod reembed;
pub mod repos;
pub mod schema;
pub mod todos;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Once;

use migrations::{Migration, PRIMARY_MIGRATIONS, PRIMARY_VERSION};

static VEC_INIT: Once = Once::new();

/// Register the sqlite-vec extension so `vec0` virtual tables are available on
/// every connection opened AFTER this call. Idempotent - call before each
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

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        match stmt.query_row(params![key], |r| r.get(0)) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The stored ID of the model that produced this database's vectors.
    pub fn embedding_model(&self) -> Result<Option<String>> {
        self.get_meta(crate::embed::models::META_KEY)
    }

    pub fn embedding_model_check(&self) -> Result<crate::embed::models::ModelCheck> {
        Ok(crate::embed::models::check(
            self.embedding_model()?.as_deref(),
        ))
    }

    /// False when this database's vectors came from a different model than
    /// the configured one: semantic search and dedup must then be skipped
    /// (keyword search still works). Warns once per process.
    pub fn semantic_ready(&self) -> bool {
        crate::embed::models::semantic_allowed("the main database", self.embedding_model())
    }

    /// Run a similarity `read` if this database's vectors match the configured
    /// model, else return nothing; see [`snapshot_read`].
    pub(crate) fn semantic_read<T: Default>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        snapshot_read(&self.conn, || self.semantic_ready(), read)
    }

    /// Run `write`, telling it whether it may store vectors. False means store
    /// the row without them: mixing vectors from two models makes every score
    /// meaningless. The first vector records the configured model. Pass
    /// `wants_vectors = false` when there are none to store.
    ///
    /// The check and the write share one transaction, so a rebuild to another
    /// model can't commit between them.
    pub(crate) fn write_with_vectors<T>(
        &self,
        wants_vectors: bool,
        write: impl FnOnce(bool) -> Result<T>,
    ) -> Result<T> {
        claimed_write(&self.conn, wants_vectors, write, || {
            crate::embed::models::claim_writes("the main database", self.embedding_model(), |id| {
                self.set_meta(crate::embed::models::META_KEY, id)
            })
        })
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
/// Shared by both database types: run `read` only if `ready` says this
/// database's vectors may be compared with ours (else return nothing), with
/// the check and the read in one read transaction. In WAL mode that pins both
/// to a single snapshot, so a rebuild to another model committing in between
/// can't hand the read vectors the check never saw.
pub(crate) fn snapshot_read<T: Default>(
    conn: &Connection,
    ready: impl FnOnce() -> bool,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let run = || if ready() { read() } else { Ok(T::default()) };
    if !conn.is_autocommit() {
        return run();
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    let out = run()?;
    tx.commit()?;
    Ok(out)
}

/// Shared by both database types: decide with `claim` whether vectors may be
/// written, then run `write`, inside one transaction taken for writing up
/// front. SQLite admits one writer at a time, so nothing (a `db reembed` to
/// another model, in particular) can commit between the decision and the
/// write. Inside a caller's transaction, that transaction already holds.
pub(crate) fn claimed_write<T>(
    conn: &Connection,
    wants_vectors: bool,
    write: impl FnOnce(bool) -> Result<T>,
    claim: impl FnOnce() -> bool,
) -> Result<T> {
    if !wants_vectors {
        return write(false);
    }
    if !conn.is_autocommit() {
        return write(claim());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let out = write(claim())?;
    tx.commit()?;
    Ok(out)
}

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
        // but still carrying a stale pointer - the corruption v12 fixes).
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

    /// A primary database at v13 with the given SQL applied, ready for v14.
    fn db_at_v13(setup: &str) -> Connection {
        register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(schema::PRIMARY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        run_migrations(&conn, 1, PRIMARY_MIGRATIONS, 13).unwrap();
        conn.execute_batch(setup).unwrap();
        conn
    }

    fn v384_bytes() -> Vec<u8> {
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        embedding_to_bytes(&v)
    }

    #[test]
    fn test_migration_v14_stamps_existing_embeddings_as_minilm() {
        let conn = db_at_v13("");
        conn.execute(
            "INSERT INTO memory (key, value, embedding) VALUES ('k', 'v', ?1)",
            params![v384_bytes()],
        )
        .unwrap();
        run_migrations(&conn, 13, PRIMARY_MIGRATIONS, 14).unwrap();
        let db = Database { conn };
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(crate::embed::models::MINILM_L6_V2.id)
        );
    }

    #[test]
    fn test_migration_v14_stamps_any_embedded_table() {
        // A repo description alone is enough to mark the database.
        let conn = db_at_v13("");
        conn.execute(
            "INSERT INTO repos (path, name, db_path, description_embedding)
             VALUES ('/r', 'r', '/r/.sclerox/repo.db', ?1)",
            params![v384_bytes()],
        )
        .unwrap();
        run_migrations(&conn, 13, PRIMARY_MIGRATIONS, 14).unwrap();
        assert!(Database { conn }.embedding_model().unwrap().is_some());
    }

    #[test]
    fn test_migration_v14_leaves_unembedded_db_unrecorded() {
        // Rows without vectors are not "embedded with MiniLM".
        let conn = db_at_v13("INSERT INTO memory (key, value) VALUES ('k', 'v');");
        run_migrations(&conn, 13, PRIMARY_MIGRATIONS, 14).unwrap();
        assert_eq!(Database { conn }.embedding_model().unwrap(), None);
    }

    #[test]
    fn test_fresh_db_has_no_embedding_model() {
        // A fresh database runs v14 too, and must not be marked as MiniLM.
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.embedding_model().unwrap(), None);
        assert_eq!(
            db.embedding_model_check().unwrap(),
            crate::embed::models::ModelCheck::NotRecorded
        );
    }

    #[test]
    fn test_migration_v14_is_harmless_twice() {
        let conn = db_at_v13("");
        conn.execute(
            "INSERT INTO memory (key, value, embedding) VALUES ('k', 'v', ?1)",
            params![v384_bytes()],
        )
        .unwrap();
        conn.execute_batch(schema::MIGRATION_V14).unwrap();
        // A second run neither fails nor overwrites a recorded model.
        conn.execute(
            "UPDATE meta SET value = 'other' WHERE key = 'embedding_model'",
            [],
        )
        .unwrap();
        conn.execute_batch(schema::MIGRATION_V14).unwrap();
        let db = Database { conn };
        assert_eq!(db.embedding_model().unwrap().as_deref(), Some("other"));
    }

    #[test]
    fn a_rebuild_cannot_commit_between_the_model_check_and_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let writer = Database::open(&path).unwrap();
        let rebuild = Database::open(&path).unwrap();
        rebuild
            .conn
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        writer.memory_set("k", "v", "feedback", None).unwrap();

        let wrote = writer
            .write_with_vectors(true, |keep| {
                assert!(keep, "nothing recorded yet: the write claims the database");
                // A rebuild to another model tries to commit right here...
                let switched = rebuild.set_meta(crate::embed::models::META_KEY, "other-model");
                assert!(switched.is_err(), "the writer holds the write lock");
                let mut v = vec![0.0f32; 384];
                v[0] = 1.0;
                writer.conn.execute(
                    "UPDATE memory SET embedding = ?1 WHERE key = 'k'",
                    params![embedding_to_bytes(&v)],
                )?;
                Ok(true)
            })
            .unwrap();
        assert!(wrote);
        // ...and can only do so after the write, where a rebuild replaces it.
        rebuild
            .set_meta(crate::embed::models::META_KEY, "other-model")
            .unwrap();
        assert_eq!(
            writer.embedding_model().unwrap().as_deref(),
            Some("other-model")
        );
    }

    #[test]
    fn a_rebuild_committing_mid_search_is_not_seen_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let search = Database::open(&path).unwrap();
        let rebuild = Database::open(&path).unwrap();
        search
            .set_meta(
                crate::embed::models::META_KEY,
                crate::embed::models::configured_id(),
            )
            .unwrap();

        let seen = search
            .semantic_read(|| {
                // The check passed. A rebuild to another model commits now
                // (readers don't block writers in WAL mode)...
                rebuild
                    .set_meta(crate::embed::models::META_KEY, "other-model")
                    .unwrap();
                // ...but this read still sees the snapshot the check saw.
                search.embedding_model()
            })
            .unwrap();
        assert_eq!(seen.as_deref(), Some(crate::embed::models::configured_id()));
        // The next search sees the switch, and is refused.
        assert_eq!(
            search.embedding_model().unwrap().as_deref(),
            Some("other-model")
        );
        let refused: Option<String> = search.semantic_read(|| panic!("must not run")).unwrap();
        assert_eq!(refused, None);
    }

    #[test]
    fn test_meta_kv() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.get_meta("missing").unwrap().is_none());
        db.set_meta("k", "v1").unwrap();
        db.set_meta("k", "v2").unwrap();
        assert_eq!(db.get_meta("k").unwrap().as_deref(), Some("v2"));
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
