use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::db::{
    embedding_to_bytes,
    migrations::{REPO_MIGRATIONS, REPO_VERSION},
    run_migrations,
    schema::REPO_SCHEMA,
};

pub struct RepoDb {
    pub conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedFile {
    pub id: i64,
    pub path: String,
    pub language: Option<String>,
    pub content_hash: String,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: i64,
    pub file_id: i64,
    pub file_path: String,
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarChunk {
    pub file_path: String,
    pub chunk_text: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub score: f32,
}

impl RepoDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::db::register_vec_extension();
        let conn = Connection::open(path)?;
        // Enforce FOREIGN KEY / ON DELETE CASCADE (per-connection, OFF by
        // default; must be set outside a transaction, so set it before init).
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        crate::db::register_vec_extension();
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        let current: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current == 0 {
            self.conn.execute_batch(REPO_SCHEMA)?;
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {REPO_VERSION}"))?;
        } else {
            run_migrations(&self.conn, current, REPO_MIGRATIONS, REPO_VERSION)
                .context("repo database migration failed")?;
        }
        Ok(())
    }

    pub fn user_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Returns `(current_version, target_version, pending_count)`.
    pub fn migration_status(&self) -> Result<(u32, u32, usize)> {
        let current = self.user_version()?;
        let pending = REPO_MIGRATIONS
            .iter()
            .filter(|m| m.version > current)
            .count();
        Ok((current, REPO_VERSION, pending))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM repo_meta WHERE key = ?1")?;
        match stmt.query_row(params![key], |r| r.get(0)) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash FROM files WHERE path = ?1")?;
        match stmt.query_row(params![path], |r| r.get(0)) {
            Ok(h) => Ok(Some(h)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns true if all chunks for this file have embeddings (none are NULL).
    /// Used to detect files that were indexed without --embed and need re-indexing.
    pub fn file_chunks_all_embedded(&self, path: &str) -> Result<bool> {
        let result: Option<i64> = self
            .conn
            .query_row(
                "SELECT count(*) FROM chunks c
             JOIN files f ON c.file_id = f.id
             WHERE f.path = ?1 AND c.embedding IS NULL",
                params![path],
                |r| r.get(0),
            )
            .ok();
        // 0 missing embeddings = all embedded (also true for files with 0 chunks)
        Ok(result.unwrap_or(0) == 0)
    }

    pub fn upsert_file(
        &self,
        path: &str,
        language: Option<&str>,
        content_hash: &str,
        last_modified: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, language, content_hash, last_modified)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
               language = excluded.language,
               content_hash = excluded.content_hash,
               last_modified = excluded.last_modified",
            params![path, language, content_hash, last_modified],
        )?;
        let id: i64 =
            self.conn
                .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                    r.get(0)
                })?;
        Ok(id)
    }

    pub fn delete_file_data(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        self.conn
            .execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    pub fn insert_symbol(
        &self,
        file_id: i64,
        kind: &str,
        name: &str,
        signature: Option<&str>,
        start_line: i64,
        end_line: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO symbols (file_id, kind, name, signature, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![file_id, kind, name, signature, start_line, end_line],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_edge(
        &self,
        from_symbol_id: i64,
        to_name: &str,
        kind: &str,
        line: u32,
        confidence: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO symbol_edges (from_symbol_id, to_name, kind, line, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![from_symbol_id, to_name, kind, line as i64, confidence],
        )?;
        Ok(())
    }

    /// What does this symbol call/inherit/implement?
    /// Returns (to_name, kind, line, confidence).
    pub fn callees(&self, symbol_id: i64) -> Result<Vec<(String, String, i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT to_name, kind, COALESCE(line, 0), confidence
             FROM symbol_edges
             WHERE from_symbol_id = ?1
             ORDER BY kind, to_name",
        )?;
        let rows = stmt.query_map(params![symbol_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// What symbols call/inherit/implement the given name?
    /// Returns (calling_symbol, edge_kind, line, confidence).
    pub fn callers(&self, symbol_name: &str) -> Result<Vec<(Symbol, String, i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.file_id, f.path, s.kind, s.name, s.signature, s.start_line, s.end_line,
                    e.kind, COALESCE(e.line, 0), e.confidence
             FROM symbol_edges e
             JOIN symbols s ON s.id = e.from_symbol_id
             JOIN files f ON f.id = s.file_id
             WHERE e.to_name = ?1
             ORDER BY s.name",
        )?;
        let rows = stmt.query_map(params![symbol_name], |r| {
            Ok((
                Symbol {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    file_path: r.get(2)?,
                    kind: r.get(3)?,
                    name: r.get(4)?,
                    signature: r.get(5)?,
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                },
                r.get::<_, String>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, String>(10)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Find symbols by exact name match.
    pub fn symbol_by_name(&self, name: &str) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.file_id, f.path, s.kind, s.name, s.signature, s.start_line, s.end_line
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.name = ?1
             ORDER BY s.file_id",
        )?;
        let rows = stmt.query_map(params![name], |r| {
            Ok(Symbol {
                id: r.get(0)?,
                file_id: r.get(1)?,
                file_path: r.get(2)?,
                kind: r.get(3)?,
                name: r.get(4)?,
                signature: r.get(5)?,
                start_line: r.get(6)?,
                end_line: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn insert_chunk(
        &self,
        file_id: i64,
        chunk_index: i64,
        chunk_text: &str,
        start_line: Option<i64>,
        end_line: Option<i64>,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        let emb_bytes = embedding.map(embedding_to_bytes);
        self.conn.execute(
            "INSERT INTO chunks (file_id, chunk_index, chunk_text, start_line, end_line, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                chunk_index,
                chunk_text,
                start_line,
                end_line,
                emb_bytes
            ],
        )?;
        Ok(())
    }

    /// Chunks missing an embedding: (chunk_id, chunk_text). The backfill work
    /// list for `sclerox repo reembed` - hook-indexed repos have chunks but no
    /// embeddings, since the auto-indexer runs without an embedder.
    pub fn chunks_without_embedding(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, chunk_text FROM chunks WHERE embedding IS NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// All chunks: (chunk_id, chunk_text). Used by `reembed --force`.
    pub fn all_chunks(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, chunk_text FROM chunks")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Set (or replace) a chunk's embedding.
    pub fn set_chunk_embedding(&self, chunk_id: i64, embedding: &[f32]) -> Result<()> {
        self.conn.execute(
            "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
            params![embedding_to_bytes(embedding), chunk_id],
        )?;
        Ok(())
    }

    pub fn search_symbols(&self, query: &str) -> Result<Vec<Symbol>> {
        // Use LIKE for substring matching so "Sumo" finds "SumoClient" and
        // "SearchJob" finds "CreateSearchJobRequest". Symbol tables are small
        // (hundreds to low thousands), so a full scan is fast enough.
        let pattern = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.file_id, f.path, s.kind, s.name, s.signature, s.start_line, s.end_line
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.name LIKE ?1 ESCAPE '\\' OR s.signature LIKE ?1 ESCAPE '\\'
             ORDER BY s.name",
        )?;
        let rows = stmt.query_map(params![pattern], |row| {
            Ok(Symbol {
                id: row.get(0)?,
                file_id: row.get(1)?,
                file_path: row.get(2)?,
                kind: row.get(3)?,
                name: row.get(4)?,
                signature: row.get(5)?,
                start_line: row.get(6)?,
                end_line: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Top-`limit` code chunks nearest to `query_embedding`, via the sqlite-vec
    /// KNN index (`chunks_vec`, cosine). The index does the distance work in C,
    /// so this is O(log n)-ish instead of a full Rust scan. `score` is cosine
    /// similarity (1 - cosine distance), matching the previous semantics.
    pub fn similar_chunks(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<SimilarChunk>> {
        let query = embedding_to_bytes(query_embedding);
        let mut stmt = self.conn.prepare(
            "SELECT c.chunk_text, c.start_line, c.end_line, f.path, v.distance
             FROM chunks_vec v
             JOIN chunks c ON c.id = v.rowid
             JOIN files f ON c.file_id = f.id
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |row| {
            let chunk_text: String = row.get(0)?;
            let start_line: Option<i64> = row.get(1)?;
            let end_line: Option<i64> = row.get(2)?;
            let file_path: String = row.get(3)?;
            let distance: f64 = row.get(4)?;
            Ok(SimilarChunk {
                file_path,
                chunk_text,
                start_line,
                end_line,
                score: 1.0 - distance as f32, // cosine distance -> similarity
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_files(&self) -> Result<Vec<IndexedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, language, content_hash, last_modified FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(IndexedFile {
                id: row.get(0)?,
                path: row.get(1)?,
                language: row.get(2)?,
                content_hash: row.get(3)?,
                last_modified: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn stats(&self) -> Result<(i64, i64, i64, i64)> {
        let files: i64 = self
            .conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
        let symbols: i64 = self
            .conn
            .query_row("SELECT count(*) FROM symbols", [], |r| r.get(0))?;
        let chunks: i64 = self
            .conn
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        let edges: i64 = self
            .conn
            .query_row("SELECT count(*) FROM symbol_edges", [], |r| r.get(0))?;
        Ok((files, symbols, chunks, edges))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_db_open_in_memory() {
        let db = RepoDb::open_in_memory().unwrap();
        let (files, symbols, chunks, edges) = db.stats().unwrap();
        assert_eq!(files, 0);
        assert_eq!(symbols, 0);
        assert_eq!(chunks, 0);
        assert_eq!(edges, 0);
    }

    #[test]
    fn test_upsert_file_and_symbols() {
        let db = RepoDb::open_in_memory().unwrap();
        let file_id = db
            .upsert_file("src/main.rs", Some("rust"), "abc123", None)
            .unwrap();
        db.insert_symbol(file_id, "function", "main", Some("fn main()"), 1, 10)
            .unwrap();
        db.insert_symbol(file_id, "struct", "Config", Some("struct Config"), 12, 20)
            .unwrap();

        // Exact match
        let syms = db.search_symbols("main").unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "main");
        assert_eq!(syms[0].file_path, "src/main.rs");

        // Substring match (e.g. "Config" inside "ConfigBuilder")
        db.insert_symbol(
            file_id,
            "struct",
            "ConfigBuilder",
            Some("struct ConfigBuilder"),
            22,
            40,
        )
        .unwrap();
        let by_prefix = db.search_symbols("Config").unwrap();
        assert_eq!(by_prefix.len(), 2, "should match Config and ConfigBuilder");

        // CamelCase substring
        let by_mid = db.search_symbols("Builder").unwrap();
        assert_eq!(by_mid.len(), 1);
        assert_eq!(by_mid[0].name, "ConfigBuilder");
    }

    /// A 384-dim unit vector with 1.0 at index `i` (matches the AllMiniLM
    /// dimension the vec0 index enforces).
    #[cfg(test)]
    fn v384(i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 384];
        v[i] = 1.0;
        v
    }

    #[test]
    fn test_chunks_similarity() {
        let db = RepoDb::open_in_memory().unwrap();
        let file_id = db.upsert_file("a.rs", Some("rust"), "h1", None).unwrap();
        db.insert_chunk(
            file_id,
            0,
            "fn foo() { ... }",
            Some(1),
            Some(5),
            Some(&v384(0)),
        )
        .unwrap();
        db.insert_chunk(
            file_id,
            1,
            "fn bar() { ... }",
            Some(6),
            Some(10),
            Some(&v384(1)),
        )
        .unwrap();

        // Query closest to v384(0) → the "foo" chunk ranks first.
        let mut query = v384(0);
        query[1] = 0.1;
        let results = db.similar_chunks(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].chunk_text.contains("foo"));
    }

    #[test]
    fn test_reembed_backfill() {
        let db = RepoDb::open_in_memory().unwrap();
        let file_id = db.upsert_file("a.rs", Some("rust"), "h1", None).unwrap();
        // One embedded chunk, one without.
        db.insert_chunk(file_id, 0, "embedded", None, None, Some(&v384(0)))
            .unwrap();
        db.insert_chunk(file_id, 1, "needs embedding", None, None, None)
            .unwrap();

        let missing = db.chunks_without_embedding().unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].1, "needs embedding");

        // Backfill it.
        db.set_chunk_embedding(missing[0].0, &v384(1)).unwrap();
        assert!(db.chunks_without_embedding().unwrap().is_empty());

        // --force sees all chunks regardless of embedding state.
        assert_eq!(db.all_chunks().unwrap().len(), 2);
    }

    #[test]
    fn test_file_hash_cache() {
        let db = RepoDb::open_in_memory().unwrap();
        assert!(db.file_hash("src/main.rs").unwrap().is_none());
        db.upsert_file("src/main.rs", Some("rust"), "deadbeef", None)
            .unwrap();
        assert_eq!(
            db.file_hash("src/main.rs").unwrap().as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn test_meta_kv() {
        let db = RepoDb::open_in_memory().unwrap();
        db.set_meta("repo_root", "/home/user/myrepo").unwrap();
        assert_eq!(
            db.get_meta("repo_root").unwrap().as_deref(),
            Some("/home/user/myrepo")
        );
        assert!(db.get_meta("missing").unwrap().is_none());
    }
}
