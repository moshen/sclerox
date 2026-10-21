use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::db::{
    embedding_to_bytes, fts,
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

/// A stored chunk's text and line span, as compared on a chunker change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpan {
    pub text: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
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

/// One line of an indexed file that matches a full-text query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMatch {
    pub file_path: String,
    pub line: i64,
    pub text: String,
    /// The line holds the query's words as one phrase, not just each word.
    pub phrase: bool,
}

/// Matching lines reported per file, so one file can't fill every slot.
const TEXT_MATCH_LINES_PER_FILE: usize = 3;

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

    /// The stored ID of the model that produced this database's vectors.
    pub fn embedding_model(&self) -> Result<Option<String>> {
        self.get_meta(crate::embed::models::META_KEY)
    }

    pub fn embedding_model_check(&self) -> Result<crate::embed::models::ModelCheck> {
        Ok(crate::embed::models::check(
            self.embedding_model()?.as_deref(),
        ))
    }

    /// Names this database in warnings.
    fn label(&self) -> String {
        match self.conn.path() {
            Some(p) if !p.is_empty() => format!("the repo index {p}"),
            _ => "a repo index".to_string(),
        }
    }

    /// False when this database's vectors came from a different model than
    /// the configured one: semantic search must then be skipped (symbol search
    /// still works). Warns once per process.
    pub fn semantic_ready(&self) -> bool {
        crate::embed::models::semantic_allowed(&self.label(), self.embedding_model())
    }

    /// Call before writing vectors. False means store the row without them.
    /// Records the configured model if this is the database's first vector.
    /// Run a similarity `read` if this database's vectors match the configured
    /// model, else return nothing; see `crate::db::snapshot_read`.
    fn semantic_read<T: Default>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        crate::db::snapshot_read(&self.conn, || self.semantic_ready(), read)
    }

    fn write_with_vectors<T>(
        &self,
        wants_vectors: bool,
        write: impl FnOnce(bool) -> Result<T>,
    ) -> Result<T> {
        crate::db::claimed_write(&self.conn, wants_vectors, write, || {
            crate::embed::models::claim_writes(&self.label(), self.embedding_model(), |id| {
                self.set_meta(crate::embed::models::META_KEY, id)
            })
        })
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

    /// A file's stored chunks as (text, start line, end line), in order.
    /// Compared on a chunker change, so that a file the new chunker cuts
    /// identically can be left alone.
    pub fn file_chunk_spans(&self, path: &str) -> Result<Vec<ChunkSpan>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.chunk_text, c.start_line, c.end_line FROM chunks c
             JOIN files f ON c.file_id = f.id
             WHERE f.path = ?1 ORDER BY c.chunk_index",
        )?;
        let rows = stmt.query_map(params![path], |r| {
            Ok(ChunkSpan {
                text: r.get(0)?,
                start_line: r.get(1)?,
                end_line: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// A file's stored vectors keyed by chunk text. Read before the file is
    /// re-chunked so that chunks whose text did not change keep their vector
    /// instead of waiting to be embedded again.
    pub fn file_chunk_vectors(
        &self,
        file_id: i64,
    ) -> Result<std::collections::HashMap<String, Vec<f32>>> {
        let mut stmt = self.conn.prepare(
            "SELECT chunk_text, embedding FROM chunks
             WHERE file_id = ?1 AND embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![file_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (text, bytes) = row?;
            let vector = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            out.insert(text, vector);
        }
        Ok(out)
    }

    pub fn delete_file_data(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        self.conn
            .execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
        self.conn
            .execute("DELETE FROM files_fts WHERE rowid = ?1", params![file_id])?;
        Ok(())
    }

    /// Replace the full-text index entry of the stored file at `path` with
    /// `text`, its whole contents. Does nothing for a path not in `files`.
    pub fn set_file_text(&self, path: &str, text: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM files_fts WHERE rowid = (SELECT id FROM files WHERE path = ?1)",
            params![path],
        )?;
        self.conn.execute(
            "INSERT INTO files_fts (rowid, body) SELECT id, ?2 FROM files WHERE path = ?1",
            params![path, text],
        )?;
        Ok(())
    }

    /// Lines of indexed files under `root` that hold every term of `query`
    /// (any case, anywhere in a word), at most `limit` lines in all and
    /// [`TEXT_MATCH_LINES_PER_FILE`] per file. Files holding the terms as one
    /// phrase come first, then the best-ranked, and within a file the phrase
    /// lines lead. The index finds the files; the lines are read
    /// from the files on disk, so a file edited since it was indexed reports
    /// what it says now. Terms under three characters can't be looked up in
    /// the trigram index: they only narrow the lines, and a query of nothing
    /// but such terms matches nothing.
    pub fn search_text(&self, root: &Path, query: &str, limit: usize) -> Result<Vec<TextMatch>> {
        let Some(fts_query) = fts::trigram_query(query) else {
            return Ok(Vec::new());
        };
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let phrase = terms.join(" ");
        let phrase_query = format!("\"{}\"", phrase.replace('"', "\"\""));
        let mut stmt = self.conn.prepare(
            "SELECT f.path FROM files_fts JOIN files f ON f.id = files_fts.rowid
             WHERE files_fts MATCH ?1
             ORDER BY files_fts.rowid NOT IN
                      (SELECT rowid FROM files_fts WHERE files_fts MATCH ?2),
                      files_fts.rank",
        )?;
        let mut rows = stmt.query(params![fts_query, phrase_query])?;
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(row) = rows.next()? else { break };
            let file_path: String = row.get(0)?;
            let Ok(source) = std::fs::read_to_string(root.join(&file_path)) else {
                continue;
            };
            // Lines holding every term; those with the terms as one phrase
            // first (the sort is stable, so each group stays in file order).
            let mut lines: Vec<(bool, usize, &str)> = source
                .lines()
                .enumerate()
                .filter_map(|(i, l)| {
                    let lower = l.to_lowercase();
                    terms
                        .iter()
                        .all(|t| lower.contains(t.as_str()))
                        .then(|| (!lower.contains(&phrase), i, l))
                })
                .collect();
            lines.sort_by_key(|&(not_phrase, _, _)| not_phrase);
            let room = (limit - out.len()).min(TEXT_MATCH_LINES_PER_FILE);
            out.extend(
                lines
                    .into_iter()
                    .take(room)
                    .map(|(not_phrase, i, l)| TextMatch {
                        file_path: file_path.clone(),
                        line: i as i64 + 1,
                        text: l.trim().to_string(),
                        phrase: !not_phrase,
                    }),
            );
        }
        Ok(out)
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
        self.write_with_vectors(embedding.is_some(), |keep| {
            let emb_bytes = embedding.filter(|_| keep).map(embedding_to_bytes);
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
        })
    }

    /// Chunks missing an embedding: (chunk_id, chunk_text). The backfill work
    /// list for `sclerox repo reembed` - hook-indexed repos have chunks but no
    /// embeddings, since the auto-indexer runs without an embedder.
    /// Whether any chunk is still waiting for a vector. Stops at the first one,
    /// so it is cheap enough to ask on every session hook.
    pub fn has_chunks_without_embedding(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE embedding IS NULL)",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn chunks_without_embedding(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, chunk_text FROM chunks WHERE embedding IS NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Set (or replace) a chunk's embedding. Returns false, storing nothing,
    /// if the database holds another model's vectors.
    pub fn set_chunk_embedding(&self, chunk_id: i64, embedding: &[f32]) -> Result<bool> {
        self.write_with_vectors(true, |keep| {
            if !keep {
                return Ok(false);
            }
            self.conn.execute(
                "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
                params![embedding_to_bytes(embedding), chunk_id],
            )?;
            Ok(true)
        })
    }

    /// Re-embed every code chunk and record `model_id` as this database's
    /// model, all-or-nothing; see `crate::db::reembed`.
    pub fn reembed_all(
        &self,
        model_id: &str,
        embed: &mut crate::db::reembed::EmbedFn<'_>,
        progress: &mut crate::db::reembed::ProgressFn<'_>,
    ) -> Result<usize> {
        crate::db::reembed::rebuild(
            &self.conn,
            &crate::db::reembed::REPO,
            model_id,
            embed,
            progress,
        )
    }

    pub fn search_symbols(&self, query: &str) -> Result<Vec<Symbol>> {
        // Use LIKE for substring matching so "Sumo" finds "SumoClient" and
        // "SearchJob" finds "CreateSearchJobRequest". Symbol tables are small
        // (hundreds to low thousands), so a full scan is fast enough.
        let pattern = fts::like_contains(query);
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
        self.semantic_read(|| {
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
        })
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

    /// A 384-dim unit vector with 1.0 at index `i` (matches the embedding
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
    fn search_text_finds_lines_anywhere_in_the_file() {
        let root = tempfile::tempdir().unwrap();
        let src = "//! Frobnicate widgets.\nuse std::io;\n\nfn go() {\n    let x = frob_widget(1);\n    // the frob widget phrase\n}\n";
        std::fs::write(root.path().join("a.rs"), src).unwrap();
        std::fs::write(root.path().join("b.rs"), "fn other() {}\n").unwrap();
        let db = RepoDb::open_in_memory().unwrap();
        for (path, text) in [("a.rs", src), ("b.rs", "fn other() {}\n")] {
            db.upsert_file(path, Some("rust"), "h", None).unwrap();
            db.set_file_text(path, text).unwrap();
        }
        let lines = |q: &str| -> Vec<(String, i64)> {
            db.search_text(root.path(), q, 10)
                .unwrap()
                .into_iter()
                .map(|m| (m.file_path, m.line))
                .collect()
        };
        let a = |line| ("a.rs".to_string(), line);

        // Doc comments and imports, which no chunk holds; any case.
        assert_eq!(lines("FROBNICATE"), [a(1)]);
        assert_eq!(lines("std::io"), [a(2)]);
        // A mid-identifier substring.
        assert_eq!(lines("b_wid"), [a(5)]);
        // Every term, on one line; the phrase first.
        assert_eq!(lines("frob widget"), [a(6), a(1), a(5)]);
        // Short terms only narrow the lines; alone they can't be looked up.
        assert_eq!(lines("x frob"), [a(5)]);
        assert!(lines("fn").is_empty());
        assert!(lines("nowhere").is_empty());
        // The per-file and overall caps.
        assert_eq!(lines("widget").len(), TEXT_MATCH_LINES_PER_FILE);
        assert_eq!(db.search_text(root.path(), "widget", 1).unwrap().len(), 1);

        // A file holding the phrase leads a better-ranked one that holds
        // only the words, even when the limit leaves room for one file.
        std::fs::write(root.path().join("c.rs"), "// gamma delta\n").unwrap();
        std::fs::write(
            root.path().join("d.rs"),
            "// delta gamma\n// delta gamma\n// delta gamma\n",
        )
        .unwrap();
        for (path, text) in [
            ("c.rs", "// gamma delta\n"),
            ("d.rs", "// delta gamma\n// delta gamma\n// delta gamma\n"),
        ] {
            db.upsert_file(path, Some("rust"), "h", None).unwrap();
            db.set_file_text(path, text).unwrap();
        }
        let top = db.search_text(root.path(), "gamma delta", 1).unwrap();
        assert_eq!((top[0].file_path.as_str(), top[0].phrase), ("c.rs", true));

        // Re-indexing a file drops its old text.
        let id = db.upsert_file("a.rs", Some("rust"), "h2", None).unwrap();
        db.delete_file_data(id).unwrap();
        assert!(lines("frobnicate").is_empty());
    }

    #[test]
    fn migration_v8_adds_the_full_text_index() {
        crate::db::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(REPO_SCHEMA).unwrap();
        conn.execute_batch("DROP TABLE files_fts; PRAGMA user_version = 7")
            .unwrap();
        let db = RepoDb { conn };
        db.init().unwrap();
        assert_eq!(db.user_version().unwrap(), REPO_VERSION);
        db.upsert_file("a.rs", None, "h", None).unwrap();
        db.set_file_text("a.rs", "hello there").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "hello there").unwrap();
        assert_eq!(db.search_text(root.path(), "hello", 5).unwrap().len(), 1);
    }

    /// A repo database at v4 (before the model was recorded), with one chunk
    /// whose embedding is `embedded` or NULL.
    fn repo_db_at_v4(embedded: bool) -> RepoDb {
        crate::db::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        // REPO_SCHEMA already holds every table; rewind the version so v5
        // runs as it would on an upgraded database.
        conn.execute_batch(REPO_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        let db = RepoDb { conn };
        let file_id = db.upsert_file("a.rs", Some("rust"), "h", None).unwrap();
        let emb = embedded.then(|| v384(0));
        db.conn
            .execute(
                "INSERT INTO chunks (file_id, chunk_index, chunk_text, embedding)
                 VALUES (?1, 0, 'fn a() {}', ?2)",
                params![file_id, emb.as_deref().map(embedding_to_bytes)],
            )
            .unwrap();
        db
    }

    #[test]
    fn test_migration_v5_stamps_existing_embeddings_as_minilm() {
        let db = repo_db_at_v4(true);
        db.init().unwrap();
        assert_eq!(db.user_version().unwrap(), REPO_VERSION);
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(crate::embed::models::MINILM_L6_V2.id)
        );
    }

    #[test]
    fn test_migration_v5_leaves_unembedded_db_unrecorded() {
        let db = repo_db_at_v4(false);
        db.init().unwrap();
        assert_eq!(db.embedding_model().unwrap(), None);
    }

    #[test]
    fn test_migration_v5_is_harmless_twice() {
        let db = repo_db_at_v4(true);
        db.init().unwrap();
        db.conn
            .execute_batch(crate::db::schema::REPO_MIGRATION_V5)
            .unwrap();
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(crate::embed::models::MINILM_L6_V2.id)
        );
    }

    /// Index names on `table`, for checking a migration created them.
    fn index_names(db: &RepoDb, table: &str) -> Vec<String> {
        db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1")
            .unwrap()
            .query_map(params![table], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn chunks_and_symbols_are_indexed_by_file_fresh_and_migrated() {
        let fresh = RepoDb::open_in_memory().unwrap();
        // An upgraded database: REPO_SCHEMA minus the v6 indexes, at v5.
        let migrated = repo_db_at_v4(false);
        migrated
            .conn
            .execute_batch("DROP INDEX idx_chunks_file; DROP INDEX idx_symbols_file;")
            .unwrap();
        migrated.init().unwrap();
        assert_eq!(migrated.user_version().unwrap(), REPO_VERSION);
        for db in [&fresh, &migrated] {
            assert!(index_names(db, "chunks").contains(&"idx_chunks_file".to_string()));
            assert!(index_names(db, "symbols").contains(&"idx_symbols_file".to_string()));
        }
    }

    #[test]
    fn test_fresh_repo_db_has_no_embedding_model() {
        // Fresh repo databases skip migrations entirely.
        let db = RepoDb::open_in_memory().unwrap();
        assert_eq!(db.embedding_model().unwrap(), None);
    }

    #[test]
    fn test_first_vector_records_the_configured_model() {
        let db = RepoDb::open_in_memory().unwrap();
        let file_id = db.upsert_file("a.rs", Some("rust"), "h", None).unwrap();
        // A chunk without a vector records nothing.
        db.insert_chunk(file_id, 0, "text only", None, None, None)
            .unwrap();
        assert_eq!(db.embedding_model().unwrap(), None);
        db.insert_chunk(file_id, 1, "fn a() {}", None, None, Some(&v384(0)))
            .unwrap();
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(crate::embed::models::configured_id())
        );
    }

    #[test]
    fn test_mismatch_skips_semantic_and_refuses_vectors() {
        let db = repo_db_at_v4(true);
        db.init().unwrap(); // recorded as MiniLM; the configured model is Granite
        assert!(!db.semantic_ready());
        assert!(db.similar_chunks(&v384(0), 5).unwrap().is_empty());

        // New vectors are refused; the chunk text itself is still stored.
        let file_id = db.upsert_file("b.rs", Some("rust"), "h2", None).unwrap();
        db.insert_chunk(file_id, 0, "fn b() {}", None, None, Some(&v384(1)))
            .unwrap();
        assert_eq!(db.chunks_without_embedding().unwrap().len(), 1);
        let (id, _) = db.chunks_without_embedding().unwrap()[0].clone();
        assert!(!db.set_chunk_embedding(id, &v384(1)).unwrap());

        // Symbol (keyword) search is unaffected.
        db.insert_symbol(file_id, "function", "b", None, 1, 1)
            .unwrap();
        assert_eq!(db.search_symbols("b").unwrap().len(), 1);
    }

    #[test]
    fn test_reembed_all_leaves_unembedded_chunks_alone() {
        // Session hooks index without embedding; a rebuild converts what has
        // vectors and doesn't take on embedding the rest.
        let db = repo_db_at_v4(true);
        db.init().unwrap();
        let file_id = db.upsert_file("b.rs", Some("rust"), "h2", None).unwrap();
        db.insert_chunk(file_id, 0, "fn b() {}", None, None, None)
            .unwrap();
        let n = db
            .reembed_all(
                crate::embed::models::GRANITE_SMALL_R2.id,
                &mut |texts: &[&str]| Ok(texts.iter().map(|_| v384(1)).collect()),
                &mut |_, _, _| {},
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.chunks_without_embedding().unwrap().len(), 1);
    }

    #[test]
    fn test_reembed_all_switches_model_and_index() {
        let db = repo_db_at_v4(true);
        db.init().unwrap();
        let granite = crate::embed::models::GRANITE_SMALL_R2.id;
        let n = db
            .reembed_all(
                granite,
                &mut |texts: &[&str]| Ok(texts.iter().map(|_| v384(1)).collect()),
                &mut |_, _, _| {},
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.embedding_model().unwrap().as_deref(), Some(granite));
        let hits = db.similar_chunks(&v384(1), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.99, "chunks_vec holds the new vector");
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
