//! Re-embed stored vectors with a new model (`sclerox db reembed`).
//!
//! A rebuild switches a database all-or-nothing. New vectors are computed
//! first and staged in a temporary table on disk (so memory stays bounded
//! however large the database), then applied in one short transaction
//! together with the new `embedding_model` meta value. An interrupted or
//! failed rebuild leaves the database exactly as it was, on the old model, and
//! simply runs again from the start. The `*_vec` indexes follow the vector
//! columns through the existing triggers.
//!
//! The apply step also guards against writes made while the vectors were
//! being computed: a row whose text changed gets no vector rather than one for
//! its old text, and any vector the rebuild did not produce is cleared, so
//! nothing from the old model survives. The rows this leaves without a vector
//! are filled in afterwards by [`fill_missing`], which is also what
//! `db reembed` runs on a database that is already on the right model.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

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
    /// Truncate text to `MAX_EMBED_CHARS` first, as the write path does.
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
    pub fn reembed_all(
        &self,
        model_id: &str,
        embed: &mut EmbedFn<'_>,
        progress: &mut ProgressFn<'_>,
    ) -> Result<usize> {
        rebuild(&self.conn, &PRIMARY, model_id, embed, progress)
    }

    /// Embed the rows that should have a vector but don't. Refuses a database
    /// recorded under another model; that needs [`Self::reembed_all`].
    pub fn fill_missing_vectors(
        &self,
        model_id: &str,
        embed: &mut EmbedFn<'_>,
        progress: &mut ProgressFn<'_>,
    ) -> Result<usize> {
        fill_missing(&self.conn, &PRIMARY, model_id, embed, progress)
    }

    /// How many rows should have a vector but don't.
    pub fn missing_vectors(&self) -> Result<usize> {
        missing(&self.conn, &PRIMARY)
    }
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

/// Walk `table`'s rows matching `cond` in id order, a page at a time, calling
/// `each` with every page's rows and their new vectors.
fn embed_rows(
    conn: &Connection,
    t: &ReembedTable,
    cond: &str,
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
    let mut done = 0usize;
    let mut after = i64::MIN;
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
                    s.chars().take(crate::index::MAX_EMBED_CHARS).collect()
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
        progress(t.label, done.min(total), total.max(done));
    }
    Ok(done)
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
    // Stage on disk, not in memory: a large repo has hundreds of MB of vectors.
    conn.execute_batch(
        "PRAGMA temp_store = FILE;
         DROP TABLE IF EXISTS temp.reembed_staging;
         CREATE TEMP TABLE reembed_staging (
             tbl  INTEGER NOT NULL,
             id   INTEGER NOT NULL,
             text BLOB,
             vec  BLOB NOT NULL,
             PRIMARY KEY (tbl, id)
         );",
    )?;
    let result = stage_and_apply(conn, layout, model_id, embed, progress);
    let _ = conn.execute_batch("DROP TABLE IF EXISTS temp.reembed_staging;");
    result
}

fn stage_and_apply(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
    embed: &mut EmbedFn<'_>,
    progress: &mut ProgressFn<'_>,
) -> Result<usize> {
    // Embedding writes only to the temp database: no lock on the real one.
    for (i, t) in layout.tables.iter().enumerate() {
        let i = i as i64;
        // Fill tables: every row that should have a vector. Code chunks: only
        // those that had one.
        let cond = if t.fill {
            "1".to_string()
        } else {
            format!("{} IS NOT NULL", t.vector)
        };
        let mut stage = conn.prepare(
            "INSERT INTO temp.reembed_staging (tbl, id, text, vec) VALUES (?1, ?2, ?3, ?4)",
        )?;
        embed_rows(conn, t, &cond, embed, progress, |rows, vectors| {
            for ((id, text), v) in rows.iter().zip(vectors) {
                stage.execute(params![i, id, text, super::embedding_to_bytes(&v)])?;
            }
            Ok(())
        })?;
    }

    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let mut written = 0usize;
    for (i, t) in layout.tables.iter().enumerate() {
        let i = i as i64;
        let mut apply = tx.prepare(&apply_sql(t))?;
        let mut staged =
            tx.prepare("SELECT id, text, vec FROM temp.reembed_staging WHERE tbl = ?1")?;
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
                 AND id NOT IN (SELECT id FROM temp.reembed_staging WHERE tbl = ?1)",
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

/// Embed the rows of `layout`'s fill tables that have no vector, committing a
/// batch at a time (each row stands alone, so there is nothing to keep
/// all-or-nothing). Records `model_id` if nothing is recorded yet; refuses a
/// database recorded under another model. Returns the vectors written.
pub fn fill_missing(
    conn: &Connection,
    layout: &Layout,
    model_id: &str,
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
    for t in layout.tables.iter().filter(|t| t.fill) {
        let cond = format!("{} IS NULL", t.vector);
        let apply = apply_sql(t);
        embed_rows(conn, t, &cond, embed, progress, |rows, vectors| {
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
        })?;
    }
    Ok(written)
}

/// Rows of `layout`'s fill tables that should have a vector but don't.
pub fn missing(conn: &Connection, layout: &Layout) -> Result<usize> {
    let mut n = 0usize;
    for t in layout.tables.iter().filter(|t| t.fill) {
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
                "SELECT count(*) FROM memory WHERE key = 'kept' AND embedding IS NULL"
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

    #[test]
    fn staging_is_dropped_afterwards() {
        let db = minilm_db();
        db.reembed_all(GRANITE_SMALL_R2.id, &mut fake_embed(1), &mut |_, _, _| {})
            .unwrap();
        assert_eq!(
            vector_count(
                &db,
                "SELECT count(*) FROM temp.sqlite_master WHERE name = 'reembed_staging'"
            ),
            0
        );
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
