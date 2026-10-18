//! Re-embed stored vectors with a new model (`sclerox db reembed`).
//!
//! A rebuild switches a database all-or-nothing. New vectors are computed
//! first and staged in a database file beside it (so memory stays bounded
//! however large the database), then applied in one short transaction
//! together with the new `embedding_model` meta value. An interrupted or
//! failed rebuild leaves the database exactly as it was, on the old model;
//! the next rebuild to the same model goes on from the vectors already
//! staged. The `*_vec` indexes follow the vector columns through the
//! existing triggers.
//!
//! The apply step also guards against writes made while the vectors were
//! being computed: a row whose text changed gets no vector rather than one for
//! its old text, and any vector the rebuild did not produce is cleared, so
//! nothing from the old model survives. The rows this leaves without a vector
//! are filled in afterwards by [`fill_missing`], which is also what
//! `db reembed` runs on a database that is already on the right model.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

use super::Database;

/// One table's worth of re-embedding work.
pub struct ReembedTable {
    /// Shown in progress output.
    pub label: &'static str,
    pub table: &'static str,
    /// SQL expression for the text that gets embedded.
    pub text: &'static str,
    /// Column holding the vector.
    pub vector: &'static str,
    /// Which rows should carry a vector.
    pub filter: &'static str,
    /// Cut a memory value to `embed::memory_chars` first, as the write path does.
    pub cap: bool,
    /// Whether rows without a vector should get one. False for code chunks:
    /// those are filled by `repo reembed`, `repo index`, and the session
    /// hooks' background `hook embed-repo` worker.
    pub fill: bool,
}

/// Where a database keeps its tables and its recorded model.
pub struct Layout {
    pub tables: &'static [ReembedTable],
    /// Key/value table holding [`crate::embed::models::META_KEY`].
    pub meta_table: &'static str,
}

/// The primary database.
pub const PRIMARY: Layout = Layout {
    tables: &[
        // Only active memories: `memory_vec` indexes nothing else. Inactive
        // ones lose their vector instead, so no old-model vector lingers to
        // be re-indexed if a row ever becomes active again.
        ReembedTable {
            label: "memories",
            table: "memory",
            text: "value",
            vector: "embedding",
            filter: "status = 'active'",
            cap: true,
            fill: true,
        },
        ReembedTable {
            label: "meeting chunks",
            table: "meeting_chunks",
            text: "chunk_text",
            vector: "embedding",
            filter: "1",
            cap: false,
            fill: true,
        },
        ReembedTable {
            label: "todo chunks",
            table: "todo_chunks",
            text: "chunk_text",
            vector: "embedding",
            filter: "1",
            cap: false,
            fill: true,
        },
        ReembedTable {
            label: "research chunks",
            table: "investigation_chunks",
            text: "chunk_text",
            vector: "embedding",
            filter: "1",
            cap: false,
            fill: true,
        },
        // The indexer embeds the description, or the name when there is none.
        ReembedTable {
            label: "repo descriptions",
            table: "repos",
            text: "COALESCE(NULLIF(description, ''), name)",
            vector: "description_embedding",
            filter: "1",
            cap: false,
            fill: true,
        },
    ],
    meta_table: "meta",
};

/// A repo database.
pub const REPO: Layout = Layout {
    tables: &[ReembedTable {
        label: "code chunks",
        table: "chunks",
        text: "chunk_text",
        vector: "embedding",
        filter: "1",
        cap: false,
        fill: false,
    }],
    meta_table: "repo_meta",
};

/// Rows per `embed` call; bounds memory and gives progress a steady beat.
const BATCH: usize = 64;

/// Embeds a batch of texts, one vector per text.
pub type EmbedFn<'a> = dyn FnMut(&[&str]) -> Result<Vec<Vec<f32>>> + 'a;
/// Receives `(table label, rows done, rows total)` after each batch.
pub type ProgressFn<'a> = dyn FnMut(&str, usize, usize) + 'a;

impl Database {
    /// Re-embed every vector in this database and record `model_id` as its
    /// model. Returns the vectors written; see [`rebuild`].
    #[cfg(test)]
    pub fn reembed_all(
        &self,
        model_id: &str,
        embed: &mut EmbedFn<'_>,
        progress: &mut ProgressFn<'_>,
    ) -> Result<usize> {
        self.reembed_all_except(model_id, &[], embed, progress)
    }

    /// [`Self::reembed_all`], leaving the tables named in `skip` as they are.
    /// For tables whose vectors were just written with `model_id`.
    pub fn reembed_all_except(
        &self,
        model_id: &str,
        skip: &[&str],
        embed: &mut EmbedFn<'_>,
        progress: &mut ProgressFn<'_>,
    ) -> Result<usize> {
        rebuild_except(&self.conn, &PRIMARY, model_id, skip, embed, progress)
    }

    /// Embed the rows that should have a vector but don't. Refuses a database
    /// recorded under another model; that needs [`Self::reembed_all`].
    pub fn fill_missing_vectors(
        &self,
        model_id: &str,
        embed: &mut EmbedFn<'_>,
        progress: &mut ProgressFn<'_>,
    ) -> Result<usize> {
        fill_missing(
            &self.conn,
            &PRIMARY,
            model_id,
            Fill::Default,
            embed,
            progress,
        )
    }

    /// How many rows should have a vector but don't.
    pub fn missing_vectors(&self) -> Result<usize> {
        missing(&self.conn, &PRIMARY)
    }
}

/// Rows a [`rebuild_except`] skipping `skip` embeds.
pub fn rebuild_size(conn: &Connection, layout: &Layout, skip: &[&str]) -> Result<usize> {
    let mut n = 0usize;
    for t in layout.tables.iter().filter(|t| !skip.contains(&t.table)) {
        let cond = if t.fill {
            "1".to_string()
        } else {
            format!("{} IS NOT NULL", t.vector)
        };
        let c: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM {} WHERE ({}) AND ({cond})",
                t.table, t.filter
            ),
            [],
            |r| r.get(0),
        )?;
        n += c as usize;
    }
    Ok(n)
}

/// `SELECT id, text` over `table`'s rows matching `cond`, one page after
/// `after` by id. Text is read as bytes so a row that is not valid UTF-8 can
/// never fail the read (and with it the whole rebuild); it is embedded lossily.
fn page_sql(t: &ReembedTable, cond: &str) -> String {
    format!(
        "SELECT id, CAST({text} AS BLOB) FROM {table}
         WHERE ({filter}) AND ({cond}) AND id > ?1 ORDER BY id LIMIT {BATCH}",
        text = t.text,
        table = t.table,
        filter = t.filter,
    )
}

/// Set row `?2`'s vector to `?1` only if its text is still `?3`, else clear it.
fn apply_sql(t: &ReembedTable) -> String {
    format!(
        "UPDATE {table} SET {vector} = CASE WHEN CAST({text} AS BLOB) IS ?3 THEN ?1 END
         WHERE id = ?2",
        table = t.table,
        vector = t.vector,
        text = t.text,
    )
}

type Row = (i64, Option<Vec<u8>>);

/// Walk `table`'s rows matching `cond` in id order, a page at a time, from
/// the first id above `after`, calling `each` with every page's rows and
/// their new vectors. `done` rows were handled before; they count towards
/// progress, not the returned number. `model_id` is the model the vectors
/// are for, which decides how much of a memory is embedded.
#[allow(clippy::too_many_arguments)]
fn embed_rows(
    conn: &Connection,
    t: &ReembedTable,
    model_id: &str,
    cond: &str,
    after: i64,
    done: usize,
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
    mut each: impl FnMut(&[Row], Vec<Vec<f32>>) -> Result<()>,
) -> Result<usize> {
    let total: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM {} WHERE ({}) AND ({cond})",
            t.table, t.filter
        ),
        [],
        |r| r.get(0),
    )?;
    let total = total as usize;
    let sql = page_sql(t, cond);
    let memory_chars = crate::embed::memory_chars_for(model_id);
    if done > 0 {
        progress(t.label, done.min(total), total.max(done));
    }
    let mut done = done;
    let mut embedded = 0usize;
    let mut after = after;
    loop {
        let rows: Vec<Row> = {
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map([after], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let Some(&(last, _)) = rows.last() else {
            break;
        };
        after = last;
        let texts: Vec<String> = rows
            .iter()
            .map(|(_, b)| {
                let s = String::from_utf8_lossy(b.as_deref().unwrap_or_default());
                if t.cap {
                    s.chars().take(memory_chars).collect()
                } else {
                    s.into_owned()
                }
            })
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = embed(&refs).with_context(|| format!("failed to embed {}", t.label))?;
        anyhow::ensure!(
            vectors.len() == rows.len(),
            "embedder returned {} vectors for {} texts",
            vectors.len(),
            rows.len()
        );
        each(&rows, vectors)?;
        done += rows.len();
        embedded += rows.len();
        progress(t.label, done.min(total), total.max(done));
    }
    Ok(embedded)
}

/// Rebuild every vector in `conn`; see the module docs for the guarantees.
/// Returns the vectors written. Follow it with [`fill_missing`]: rows added or
/// changed while vectors were being computed come out of it without one. That
/// is a separate step so its failure can't be mistaken for the rebuild's - by
/// then the database is already on the new model.
pub fn rebuild(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    rebuild_except(conn, layout, model_id, &[], embed, progress)
}

/// [`rebuild`], leaving the tables named in `skip` as they are. Refuses to
/// start while another process re-embeds the database (see [`EmbedLock`]).
pub fn rebuild_except(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    skip: &[&str],
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    let Some(_lock) = EmbedLock::try_new(conn)? else {
        bail!("another process is re-embedding this database; try again once it finishes");
    };
    rebuild_locked(conn, layout, model_id, skip, embed, progress)
}

/// [`rebuild_except`] for a caller holding the [`EmbedLock`].
pub(crate) fn rebuild_locked(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    skip: &[&str],
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    // Stage on disk, not in memory: a large repo has hundreds of MB of vectors.
    let staging = staging_path(conn);
    let file = staging
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    conn.execute("ATTACH DATABASE ?1 AS reembed", [&file])?;
    let result = start_staging(conn, model_id, skip)
        .and_then(|()| stage_and_apply(conn, layout, model_id, skip, embed, progress));
    let _ = conn.execute_batch("DETACH DATABASE reembed");
    if let (Ok(_), Some(path)) = (&result, staging) {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Held by the one process that may rebuild a database or run its queued
/// jobs, so two can't share the staged vectors or the same job: a lock on
/// a file beside the database, released when dropped or when the process
/// ends. An in-memory database needs none.
pub struct EmbedLock {
    _file: Option<std::fs::File>,
}

impl EmbedLock {
    /// `None` while another process holds it.
    pub fn try_new(conn: &Connection) -> Result<Option<Self>> {
        let Some(path) = conn.path().filter(|p| !p.is_empty()) else {
            return Ok(Some(Self { _file: None }));
        };
        let path = format!("{path}-embed.lock");
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {path}"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: Some(file) })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("failed to lock {path}"))
            }
        }
    }
}

/// Which tables [`fill_missing`] fills.
#[derive(Debug, Clone, Copy)]
pub enum Fill<'a> {
    /// The fill tables (see [`ReembedTable::fill`]).
    Default,
    /// Every table, code chunks included.
    All,
    /// Just this one.
    Only(&'a str),
}

impl Fill<'_> {
    fn includes(self, t: &ReembedTable) -> bool {
        match self {
            Fill::Default => t.fill,
            Fill::All => true,
            Fill::Only(table) => t.table == table,
        }
    }
}

/// The file a rebuild of `conn`'s database stages its vectors in, kept until
/// they are applied. `None` for an in-memory database, which stages in a
/// temporary one.
fn staging_path(conn: &Connection) -> Option<PathBuf> {
    conn.path()
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(format!("{p}-reembed")))
}

/// Make the staging tables, keeping what an earlier rebuild to the same
/// model, skipping the same tables, staged.
fn start_staging(conn: &Connection, model_id: &str, skip: &[&str]) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reembed.staging (
             tbl  INTEGER NOT NULL,
             id   INTEGER NOT NULL,
             text BLOB,
             vec  BLOB NOT NULL,
             PRIMARY KEY (tbl, id)
         );
         CREATE TABLE IF NOT EXISTS reembed.info (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );",
    )?;
    let plan = format!("{model_id}; skip {}", skip.join(","));
    let staged_for: Option<String> = conn
        .query_row(
            "SELECT value FROM reembed.info WHERE key = 'plan'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if staged_for.as_deref() != Some(plan.as_str()) {
        conn.execute("DELETE FROM reembed.staging", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO reembed.info (key, value) VALUES ('plan', ?1)",
            [&plan],
        )?;
    }
    Ok(())
}

fn stage_and_apply(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    skip: &[&str],
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    // Embedding writes only to the temp database: no lock on the real one.
    for (i, t) in layout.tables.iter().enumerate() {
        if skip.contains(&t.table) {
            continue;
        }
        let i = i as i64;
        // Fill tables: every row that should have a vector. Code chunks: only
        // those that had one.
        let cond = if t.fill {
            "1".to_string()
        } else {
            format!("{} IS NOT NULL", t.vector)
        };
        // Go on after what an earlier attempt staged.
        let (after, done): (Option<i64>, i64) = conn.query_row(
            "SELECT max(id), count(*) FROM reembed.staging WHERE tbl = ?1",
            [i],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut stage = conn.prepare(
            "INSERT OR REPLACE INTO reembed.staging (tbl, id, text, vec) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let after = after.unwrap_or(i64::MIN);
        embed_rows(
            conn,
            t,
            model_id,
            &cond,
            after,
            done as usize,
            embed,
            progress,
            |rows, vectors| {
                let tx = rusqlite::Transaction::new_unchecked(
                    conn,
                    rusqlite::TransactionBehavior::Deferred,
                )?;
                for ((id, text), v) in rows.iter().zip(vectors) {
                    stage.execute(params![i, id, text, super::embedding_to_bytes(&v)])?;
                }
                tx.commit()?;
                Ok(())
            },
        )?;
    }

    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let mut written = 0usize;
    for (i, t) in layout.tables.iter().enumerate() {
        if skip.contains(&t.table) {
            continue;
        }
        let i = i as i64;
        let mut apply = tx.prepare(&apply_sql(t))?;
        let mut staged = tx.prepare("SELECT id, text, vec FROM reembed.staging WHERE tbl = ?1")?;
        let mut rows = staged.query([i])?;
        while let Some(r) = rows.next()? {
            let (id, text, vec): (i64, Option<Vec<u8>>, Vec<u8>) =
                (r.get(0)?, r.get(1)?, r.get(2)?);
            apply.execute(params![vec, id, text])?;
            written += 1;
        }
        // Whatever the rebuild didn't produce predates it: inactive memories,
        // rows that left the filter while it ran. Clear it.
        tx.execute(
            &format!(
                "UPDATE {table} SET {vector} = NULL WHERE {vector} IS NOT NULL
                 AND id NOT IN (SELECT id FROM reembed.staging WHERE tbl = ?1)",
                table = t.table,
                vector = t.vector,
            ),
            [i],
        )?;
    }
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO {} (key, value) VALUES (?1, ?2)",
            layout.meta_table
        ),
        params![crate::embed::models::META_KEY, model_id],
    )?;
    // Applied: a later rebuild starts afresh, even if the file outlives this.
    tx.execute("DELETE FROM reembed.staging", [])?;
    tx.commit()?;
    Ok(written)
}

fn recorded_model(conn: &Connection, layout: &Layout) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            &format!("SELECT value FROM {} WHERE key = ?1", layout.meta_table),
            [crate::embed::models::META_KEY],
            |r| r.get(0),
        )
        .optional()?)
}

/// Embed the rows of `layout`'s `tables` that have no vector, committing a
/// batch at a time (each row stands alone, so there is nothing to keep
/// all-or-nothing). Records `model_id` if nothing is recorded yet; refuses a
/// database recorded under another model. Returns the vectors written.
pub fn fill_missing(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    tables: Fill<'_>,
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    // Checked up front to fail fast, and again in every batch's transaction:
    // a rebuild to another model may commit while this one embeds.
    let check = |conn: &Connection| -> Result<()> {
        if let Some(recorded) = recorded_model(conn, layout)? {
            if recorded != model_id {
                bail!(
                    "this database holds vectors from '{}'; re-embed it fully instead",
                    crate::embed::models::display_id(&recorded)
                );
            }
        }
        Ok(())
    };
    check(conn)?;
    let record = format!(
        "INSERT OR IGNORE INTO {} (key, value) VALUES (?1, ?2)",
        layout.meta_table
    );
    let mut written = 0usize;
    for t in layout.tables.iter().filter(|t| tables.includes(t)) {
        let cond = format!("{} IS NULL", t.vector);
        let apply = apply_sql(t);
        embed_rows(
            conn,
            t,
            model_id,
            &cond,
            i64::MIN,
            0,
            embed,
            progress,
            |rows, vectors| {
                let tx = rusqlite::Transaction::new_unchecked(
                    conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                check(&tx)?;
                tx.execute(&record, params![crate::embed::models::META_KEY, model_id])?;
                {
                    let mut stmt = tx.prepare_cached(&apply)?;
                    for ((id, text), v) in rows.iter().zip(vectors) {
                        stmt.execute(params![super::embedding_to_bytes(&v), id, text])?;
                        written += 1;
                    }
                }
                tx.commit()?;
                Ok(())
            },
        )?;
    }
    Ok(written)
}

/// Rows of `layout`'s fill tables that should have a vector but don't.
pub fn missing(conn: &Connection, layout: &Layout) -> Result<usize> {
    missing_in(conn, layout, Fill::Default)
}

/// [`missing`] in `tables`, as [`fill_missing`] fills them.
pub fn missing_in(conn: &Connection, layout: &Layout, tables: Fill<'_>) -> Result<usize> {
    let mut n = 0usize;
    for t in layout.tables.iter().filter(|t| tables.includes(t)) {
        let c: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM {} WHERE ({}) AND {} IS NULL",
                t.table, t.filter, t.vector
            ),
            [],
            |r| r.get(0),
        )?;
        n += c as usize;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::models::{ModelCheck, GRANITE_SMALL_R2, MINILM_L6_V2};

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 384];
        v[i] = 1.0;
        v
    }

    /// A fake model: every text maps to the same unit vector, so the index
    /// contents show which "model" wrote them.
    fn fake_embed(axis: usize) -> impl FnMut(&[&str]) -> Result<Vec<Vec<f32>>> {
        move |texts: &[&str]| Ok(texts.iter().map(|_| unit(axis)).collect())
    }

    /// A database whose vectors were all written by "MiniLM" (axis 0).
    fn minilm_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("kept", "an active fact", "feedback", None)
            .unwrap();
        db.memory_set("old", "an outdated fact", "feedback", None)
            .unwrap();
        db.memory_set_embedding("kept", &unit(0)).unwrap();
        db.memory_set_embedding("old", &unit(0)).unwrap();
        db.memory_stale("old", None).unwrap();
        let m = db
            .meeting_add("Standup", None, None, Some("notes"))
            .unwrap();
        db.meeting_store_chunks(m, &[("notes".into(), Some(unit(0)))])
            .unwrap();
        db.set_meta(crate::embed::models::META_KEY, MINILM_L6_V2.id)
            .unwrap();
        db
    }

    #[test]
    fn rebuild_switches_the_model_and_the_index() {
        let db = minilm_db();
        assert_eq!(
            db.embedding_model_check().unwrap(),
            ModelCheck::Mismatch {
                recorded: MINILM_L6_V2.id.into()
            }
        );
        // Semantic search is off while the models disagree...
        assert!(db.memory_similar(&unit(0), 5).unwrap().is_empty());

        let mut seen = Vec::new();
        let n = db
            .reembed_all(
                GRANITE_SMALL_R2.id,
                &mut fake_embed(1),
                &mut |label, done, total| seen.push((label.to_string(), done, total)),
            )
            .unwrap();
        assert_eq!(n, 2, "one active memory + one meeting chunk");
        assert!(seen.contains(&("memories".to_string(), 1, 1)));

        // ...and back on afterwards, against the new vectors only.
        assert_eq!(db.embedding_model_check().unwrap(), ModelCheck::Match);
        let hits = db.memory_similar(&unit(1), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.key, "kept");
        assert!(hits[0].score > 0.99, "index holds the new vector");
        assert!(db.meeting_similar(&unit(1), 5).unwrap()[0].score > 0.99);

        // The inactive memory's old-model vector is gone, not left to resurface.
        let stale_vectors: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM memory WHERE status != 'active' AND embedding IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_vectors, 0);
    }

    #[test]
    fn failed_rebuild_changes_nothing() {
        let db = minilm_db();
        let mut calls = 0;
        let mut failing = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            if calls > 1 {
                anyhow::bail!("interrupted");
            }
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        assert!(db
            .reembed_all(GRANITE_SMALL_R2.id, &mut failing, &mut |_, _, _| {})
            .is_err());
        // Still entirely the old model: recorded ID and vectors alike.
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(MINILM_L6_V2.id)
        );
        let old: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM memory WHERE embedding IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 2);
    }

    fn vector_count(db: &Database, sql: &str) -> i64 {
        db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn rebuild_leaves_skipped_tables_alone() {
        // On the configured model already, as with `db reembed --force`.
        let db = minilm_db();
        db.set_meta(crate::embed::models::META_KEY, GRANITE_SMALL_R2.id)
            .unwrap();
        let mut seen = Vec::new();
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            seen.extend(texts.iter().map(|t| t.to_string()));
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        db.reembed_all_except(
            GRANITE_SMALL_R2.id,
            &crate::db::TEXT_CHUNK_TABLES,
            &mut embed,
            &mut |_, _, _| {},
        )
        .unwrap();
        assert!(
            !seen.iter().any(|t| t == "notes"),
            "meeting chunk re-embedded"
        );
        let vector = |sql: &str| -> Vec<u8> { db.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            vector("SELECT embedding FROM meeting_chunks"),
            crate::db::embedding_to_bytes(&unit(0)),
            "skipped table keeps its vector"
        );
        assert_eq!(
            vector("SELECT embedding FROM memory WHERE key = 'kept' AND status = 'active'"),
            crate::db::embedding_to_bytes(&unit(1))
        );
    }

    #[test]
    fn writes_during_a_rebuild_end_up_embedded_with_the_new_model() {
        let db = minilm_db();
        let mut texts_seen: Vec<String> = Vec::new();
        let mut calls = 0;
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            if calls == 1 {
                // While the rebuild runs: a memory's text changes, and a new
                // memory arrives (without a vector: the database is still
                // mismatched, so writes are refused).
                db.memory_set("kept", "an edited fact", "feedback", None)
                    .unwrap();
                db.memory_set("late", "a fact saved mid-rebuild", "feedback", None)
                    .unwrap();
                assert!(!db.memory_set_embedding("late", &unit(0)).unwrap());
            }
            texts_seen.extend(texts.iter().map(|t| t.to_string()));
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        db.reembed_all(GRANITE_SMALL_R2.id, &mut embed, &mut |_, _, _| {})
            .unwrap();
        assert_eq!(db.embedding_model_check().unwrap(), ModelCheck::Match);
        // The new memory landed on a later page and was embedded by the
        // rebuild itself. The edited one was not: its vector would have been
        // for its old text. The fill pass that follows embeds it.
        assert_eq!(db.missing_vectors().unwrap(), 1);
        assert_eq!(
            vector_count(
                &db,
                // Active row only: the edit also kept the old text as a
                // retired row, which is never embedded.
                "SELECT count(*) FROM memory
                 WHERE key = 'kept' AND status = 'active' AND embedding IS NULL"
            ),
            1
        );
        db.fill_missing_vectors(GRANITE_SMALL_R2.id, &mut embed, &mut |_, _, _| {})
            .unwrap();

        // Both got a vector, and the edited one for its new text.
        assert!(texts_seen.iter().any(|t| t == "an edited fact"));
        assert!(texts_seen.iter().any(|t| t == "a fact saved mid-rebuild"));
        assert_eq!(db.missing_vectors().unwrap(), 0);
        let hits = db.memory_similar(&unit(1), 5).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fill_embeds_rows_left_without_a_vector() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "first", "feedback", None).unwrap();
        db.memory_set("b", "second", "feedback", None).unwrap();
        let m = db.meeting_add("Sync", None, None, Some("notes")).unwrap();
        db.meeting_store_chunks(m, &[("notes".into(), None)])
            .unwrap();
        assert_eq!(db.missing_vectors().unwrap(), 3);

        let n = db
            .fill_missing_vectors(GRANITE_SMALL_R2.id, &mut fake_embed(1), &mut |_, _, _| {})
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(db.missing_vectors().unwrap(), 0);
        // An unrecorded database records the model with its first vectors.
        assert_eq!(db.embedding_model_check().unwrap(), ModelCheck::Match);
    }

    #[test]
    fn fill_stops_when_another_model_takes_over_midway() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..(BATCH + 10) {
            db.memory_set(&format!("m{i}"), "a fact", "feedback", None)
                .unwrap();
        }
        let mut calls = 0;
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            if calls == 2 {
                // A rebuild to another model commits between two batches.
                db.set_meta(crate::embed::models::META_KEY, MINILM_L6_V2.id)
                    .unwrap();
            }
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        assert!(db
            .fill_missing_vectors(GRANITE_SMALL_R2.id, &mut embed, &mut |_, _, _| {})
            .is_err());
        // The first batch went in under Granite; the second was refused.
        assert_eq!(db.missing_vectors().unwrap(), 10);
    }

    #[test]
    fn fill_refuses_a_database_on_another_model() {
        let db = minilm_db();
        db.memory_set("new", "a new fact", "feedback", None)
            .unwrap();
        assert!(db
            .fill_missing_vectors(GRANITE_SMALL_R2.id, &mut fake_embed(1), &mut |_, _, _| {})
            .is_err());
        assert_eq!(
            vector_count(
                &db,
                "SELECT count(*) FROM memory WHERE key = 'new' AND embedding IS NOT NULL"
            ),
            0
        );
    }

    #[test]
    fn text_that_is_not_utf8_cannot_fail_a_rebuild() {
        let db = minilm_db();
        db.conn
            .execute(
                "UPDATE memory SET value = CAST(X'66ff6f' AS TEXT) WHERE key = 'kept'",
                [],
            )
            .unwrap();
        db.reembed_all(GRANITE_SMALL_R2.id, &mut fake_embed(1), &mut |_, _, _| {})
            .unwrap();
        assert_eq!(db.missing_vectors().unwrap(), 0);
        assert_eq!(db.embedding_model_check().unwrap(), ModelCheck::Match);
    }

    /// [`minilm_db`], in a file, with enough memories for several pages.
    fn minilm_file_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
        for i in 0..(BATCH * 2 + 5) {
            db.memory_set(&format!("m{i}"), &format!("fact {i}"), "feedback", None)
                .unwrap();
            db.memory_set_embedding(&format!("m{i}"), &unit(0)).unwrap();
        }
        db.set_meta(crate::embed::models::META_KEY, MINILM_L6_V2.id)
            .unwrap();
        db
    }

    #[test]
    fn an_interrupted_rebuild_goes_on_from_what_it_staged() {
        let dir = tempfile::tempdir().unwrap();
        let db = minilm_file_db(&dir);
        let staging = dir.path().join("db.sqlite-reembed");
        let mut calls = 0;
        let mut failing = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            if calls > 2 {
                anyhow::bail!("interrupted");
            }
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        assert!(db
            .reembed_all(GRANITE_SMALL_R2.id, &mut failing, &mut |_, _, _| {})
            .is_err());
        assert!(staging.exists(), "kept for the next attempt");
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(MINILM_L6_V2.id)
        );

        // Another model's rebuild doesn't use those vectors...
        let mut texts = 0;
        let mut counting = |t: &[&str]| -> Result<Vec<Vec<f32>>> {
            texts += t.len();
            anyhow::bail!("stop")
        };
        let _ = db.reembed_all("other-model", &mut counting, &mut |_, _, _| {});
        // ...and starting it threw them away, so this one starts over.
        let mut texts = 0;
        let mut counting = |t: &[&str]| -> Result<Vec<Vec<f32>>> {
            texts += t.len();
            Ok(t.iter().map(|_| unit(1)).collect())
        };
        db.reembed_all(GRANITE_SMALL_R2.id, &mut counting, &mut |_, _, _| {})
            .unwrap();
        assert_eq!(texts, BATCH * 2 + 5);
        assert!(!staging.exists(), "removed once applied");

        // Interrupted again on the way to MiniLM, then resumed: only the
        // rows after the two staged pages are embedded.
        let mut calls = 0;
        let mut failing = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            if calls > 2 {
                anyhow::bail!("interrupted");
            }
            Ok(texts.iter().map(|_| unit(2)).collect())
        };
        assert!(db
            .reembed_all(MINILM_L6_V2.id, &mut failing, &mut |_, _, _| {})
            .is_err());
        let mut texts = 0;
        let mut seen = Vec::new();
        let mut counting = |t: &[&str]| -> Result<Vec<Vec<f32>>> {
            texts += t.len();
            Ok(t.iter().map(|_| unit(2)).collect())
        };
        let n = db
            .reembed_all(MINILM_L6_V2.id, &mut counting, &mut |_, done, total| {
                seen.push((done, total))
            })
            .unwrap();
        assert_eq!(texts, 5);
        assert_eq!(n, BATCH * 2 + 5, "applies the staged vectors too");
        assert_eq!(seen.last(), Some(&(BATCH * 2 + 5, BATCH * 2 + 5)));
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(MINILM_L6_V2.id)
        );
        assert_eq!(
            vector_count(
                &db,
                "SELECT count(*) FROM memory WHERE embedding IS NOT NULL"
            ),
            (BATCH * 2 + 5) as i64
        );
    }

    #[test]
    fn memories_are_cut_for_the_model_being_rebuilt_to() {
        // On Granite (4000 chars), rebuilding to MiniLM (800).
        let db = Database::open_in_memory().unwrap();
        db.memory_set("long", &"x".repeat(5000), "feedback", None)
            .unwrap();
        let mut lengths = Vec::new();
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            lengths.extend(texts.iter().map(|t| t.chars().count()));
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        db.reembed_all(MINILM_L6_V2.id, &mut embed, &mut |_, _, _| {})
            .unwrap();
        assert_eq!(lengths, [MINILM_L6_V2.memory_chars]);
    }

    #[test]
    fn an_in_memory_rebuild_leaves_nothing_attached() {
        let db = minilm_db();
        db.reembed_all(GRANITE_SMALL_R2.id, &mut fake_embed(1), &mut |_, _, _| {})
            .unwrap();
        let attached: i64 = vector_count(
            &db,
            "SELECT count(*) FROM pragma_database_list WHERE name = 'reembed'",
        );
        assert_eq!(attached, 0);
    }

    #[test]
    fn mismatch_refuses_new_vectors_but_keeps_the_rows() {
        let db = minilm_db();
        db.memory_set("new", "a new fact", "feedback", None)
            .unwrap();
        assert!(!db.memory_set_embedding("new", &unit(1)).unwrap());
        // The memory itself is stored and keyword search still finds it.
        assert_eq!(db.memory_search("new fact").unwrap().len(), 1);
        assert_eq!(db.memory_needing_embedding().unwrap().len(), 1);
    }
}
