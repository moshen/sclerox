//! Embedding work queued in a database (`embed_jobs`), for the model server
//! to run.
//!
//! A command queues a job and tells the server, which runs it with [`run`]
//! and writes the vectors itself. The rows still to embed are found the
//! usual way (a fill takes the rows without a vector, a rebuild goes on from
//! what it staged), so a job only says what is wanted and for which model,
//! and how far it has got. Work cut short by a server restart is picked up
//! by the next server.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::reembed::{self, Fill, Layout};

/// Which kind of database a queue is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbKind {
    Main,
    Repo,
}

impl DbKind {
    pub fn layout(self) -> &'static Layout {
        match self {
            DbKind::Main => &reembed::PRIMARY,
            DbKind::Repo => &reembed::REPO,
        }
    }

    /// Open the database at `path` and hand back its connection.
    pub fn open(self, path: &std::path::Path) -> Result<Connection> {
        Ok(match self {
            DbKind::Main => super::Database::open(path)?.conn,
            DbKind::Repo => crate::index::repo_db::RepoDb::open(path)?.conn,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Re-embed every vector and switch the database to the job's model.
    Rebuild,
    /// Embed the rows without a vector. In a repo database that includes the
    /// code chunks.
    Fill,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Rebuild => "rebuild",
            Kind::Fill => "fill",
        }
    }
}

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
/// Replaced by a job for another model before it finished.
pub const CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub target_model: String,
    /// Limited to this table.
    pub table: Option<String>,
    pub state: String,
    pub progress: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

impl Job {
    pub fn finished(&self) -> bool {
        self.state != QUEUED && self.state != RUNNING
    }
}

/// Whether `path` is a database with an `embed_jobs` table, looked at
/// without creating or migrating it.
pub fn has_queue(path: &std::path::Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'embed_jobs')",
        [],
        |r| r.get(0),
    )?)
}

/// Finished jobs older than this are deleted when another is queued.
const KEEP_FINISHED: &str = "-7 days";

/// Queue `kind` for `model_id` (a model ID, as the database records it),
/// limited to `table` if given, and return the job's ID. A job like it that hasn't started yet is reused.
/// Unfinished jobs for another model are cancelled: their vectors would be
/// refused or replaced.
pub fn queue(conn: &Connection, kind: Kind, model_id: &str, table: Option<&str>) -> Result<i64> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE embed_jobs SET state = ?1, error = ?2, updated_at = datetime('now')
         WHERE state IN (?3, ?4) AND target_model != ?5",
        params![
            CANCELLED,
            format!(
                "replaced by a job for {}",
                crate::embed::models::display_id(model_id)
            ),
            QUEUED,
            RUNNING,
            model_id
        ],
    )?;
    let waiting: Option<i64> = tx
        .query_row(
            "SELECT id FROM embed_jobs
             WHERE state = ?1 AND kind = ?2 AND target_model = ?3 AND table_name IS ?4",
            params![QUEUED, kind.as_str(), model_id, table],
            |r| r.get(0),
        )
        .optional()?;
    let id = match waiting {
        Some(id) => id,
        None => {
            tx.execute(
                "INSERT INTO embed_jobs (kind, target_model, table_name, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now'))",
                params![kind.as_str(), model_id, table, QUEUED],
            )?;
            tx.last_insert_rowid()
        }
    };
    tx.execute(
        "DELETE FROM embed_jobs WHERE state NOT IN (?1, ?2) AND updated_at < datetime('now', ?3)",
        params![QUEUED, RUNNING, KEEP_FINISHED],
    )?;
    tx.commit()?;
    Ok(id)
}

const COLUMNS: &str = "id, kind, target_model, table_name, state, progress, total, error";

fn job(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        kind: r.get(1)?,
        target_model: r.get(2)?,
        table: r.get(3)?,
        state: r.get(4)?,
        progress: r.get::<_, i64>(5)? as u64,
        total: r.get::<_, Option<i64>>(6)?.map(|n| n as u64),
        error: r.get(7)?,
    })
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Job>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM embed_jobs WHERE id = ?1"),
            [id],
            job,
        )
        .optional()?)
}

/// Jobs not yet finished, oldest first.
pub fn unfinished(conn: &Connection) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM embed_jobs WHERE state IN (?1, ?2) ORDER BY id"
    ))?;
    let rows = stmt.query_map([QUEUED, RUNNING], job)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A job cancelled meanwhile keeps that state and its reason.
fn set_state(conn: &Connection, id: i64, state: &str, error: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE embed_jobs SET state = ?2, error = ?3, updated_at = datetime('now')
         WHERE id = ?1 AND state != ?4",
        params![id, state, error, CANCELLED],
    )?;
    Ok(())
}

/// The job was cancelled while it ran.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn set_progress(conn: &Connection, id: i64, progress: usize, total: usize) -> Result<()> {
    conn.execute(
        "UPDATE embed_jobs SET progress = ?2, total = ?3, updated_at = datetime('now')
         WHERE id = ?1",
        params![id, progress as i64, total as i64],
    )?;
    Ok(())
}

/// Work stopped before it finished, for a reason that says nothing about the
/// job: the server is exiting, or the database stayed locked. The job is
/// left to run again.
#[derive(Debug)]
pub struct Interrupted(pub String);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted: {}", self.0)
    }
}

impl std::error::Error for Interrupted {}

/// Whether `e` means the work can go on later, unchanged.
pub fn is_interrupted(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.is::<Interrupted>()
            || matches!(
                cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(f, _))
                    if matches!(
                        f.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    )
            )
    })
}

/// Embeds texts with the model of the given ID.
pub type EmbedWith<'a> = dyn FnMut(&str, &[&str]) -> Result<Vec<Vec<f32>>> + 'a;

/// Run `conn`'s queued jobs, oldest first, until none is left. A job that
/// fails is marked so and the next one runs; one cancelled meanwhile stops
/// at its next batch. Fails with [`Interrupted`] (or a locked database),
/// leaving the job queued, when the work can't go on now, including while
/// another process runs this database's jobs. `before_rebuild` runs before
/// each rebuild and can hold it off.
pub fn run(
    conn: &Connection,
    db: DbKind,
    embed: &mut EmbedWith<'_>,
    before_rebuild: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let Some(_lock) = reembed::EmbedLock::try_new(conn)? else {
        return Err(Interrupted("another process is embedding this database".into()).into());
    };
    loop {
        let Some(job) = unfinished(conn)?.into_iter().next() else {
            return Ok(());
        };
        set_state(conn, job.id, RUNNING, None)?;
        match run_one(conn, db, &job, embed, before_rebuild) {
            Ok(()) => set_state(conn, job.id, DONE, None)?,
            Err(e) if e.chain().any(|c| c.is::<Cancelled>()) => {
                log::info!("embedding job {} ({}) was cancelled", job.id, job.kind);
            }
            Err(e) if is_interrupted(&e) => {
                let _ = set_state(conn, job.id, QUEUED, None);
                return Err(e);
            }
            Err(e) => {
                log::warn!("embedding job {} ({}) failed: {e:#}", job.id, job.kind);
                set_state(conn, job.id, FAILED, Some(&format!("{e:#}")))?;
            }
        }
    }
}

fn run_one(
    conn: &Connection,
    db: DbKind,
    job: &Job,
    embed: &mut EmbedWith<'_>,
    before_rebuild: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let layout = db.layout();
    let model = job.target_model.as_str();
    let mut embed = |texts: &[&str]| {
        if get(conn, job.id)?.is_some_and(|j| j.state == CANCELLED) {
            return Err(Cancelled.into());
        }
        embed(model, texts)
    };
    // Progress is per table; the job's is their sum.
    let mut tables: Vec<(String, usize)> = Vec::new();
    let progress = |total: usize| {
        move |label: &str, done: usize, _: usize| {
            match tables.iter_mut().find(|(l, _)| l == label) {
                Some((_, d)) => *d = done,
                None => tables.push((label.to_string(), done)),
            }
            let done: usize = tables.iter().map(|(_, d)| d).sum();
            if let Err(e) = set_progress(conn, job.id, done, total.max(done)) {
                log::debug!("couldn't record job progress: {e:#}");
            }
        }
    };
    match job.kind.as_str() {
        "rebuild" => {
            let skip: Vec<&str> = match &job.table {
                Some(only) => layout
                    .tables
                    .iter()
                    .map(|t| t.table)
                    .filter(|t| t != only)
                    .collect(),
                None => Vec::new(),
            };
            before_rebuild()?;
            let total = reembed::rebuild_size(conn, layout, &skip)?;
            set_progress(conn, job.id, 0, total)?;
            reembed::rebuild_locked(conn, layout, model, &skip, &mut embed, &mut progress(total))
                .context("rebuild failed, left on its previous model")?;
        }
        "fill" => {
            let tables = match (&job.table, db) {
                (Some(only), _) => Fill::Only(only),
                (None, DbKind::Repo) => Fill::All,
                (None, DbKind::Main) => Fill::Default,
            };
            let total = reembed::missing_in(conn, layout, tables)?;
            set_progress(conn, job.id, 0, total)?;
            reembed::fill_missing(
                conn,
                layout,
                model,
                tables,
                &mut embed,
                &mut progress(total),
            )?;
        }
        other => anyhow::bail!("unknown embedding job kind '{other}'"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::embed::models::{GRANITE_SMALL_R2, MINILM_L6_V2};

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 384];
        v[i] = 1.0;
        v
    }

    fn fake(axis: usize) -> impl FnMut(&str, &[&str]) -> Result<Vec<Vec<f32>>> {
        move |_, texts| Ok(texts.iter().map(|_| unit(axis)).collect())
    }

    fn nothing() -> Result<()> {
        Ok(())
    }

    #[test]
    fn queueing_reuses_a_waiting_job_and_cancels_other_models() {
        let db = Database::open_in_memory().unwrap();
        let a = queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        assert_eq!(
            queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap(),
            a
        );
        let r = queue(&db.conn, Kind::Rebuild, GRANITE_SMALL_R2.id, None).unwrap();
        assert_ne!(r, a);
        assert_eq!(unfinished(&db.conn).unwrap().len(), 2);

        let m = queue(&db.conn, Kind::Rebuild, MINILM_L6_V2.id, None).unwrap();
        let left: Vec<i64> = unfinished(&db.conn).unwrap().iter().map(|j| j.id).collect();
        assert_eq!(left, [m]);
        assert_eq!(get(&db.conn, a).unwrap().unwrap().state, CANCELLED);
    }

    #[test]
    fn jobs_run_in_order_and_record_progress() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "first", "feedback", None).unwrap();
        db.memory_set("b", "second", "feedback", None).unwrap();
        let fill = queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        run(&db.conn, DbKind::Main, &mut fake(1), &mut nothing).unwrap();
        let job = get(&db.conn, fill).unwrap().unwrap();
        assert_eq!(job.state, DONE);
        assert_eq!((job.progress, job.total), (2, Some(2)));
        assert_eq!(db.missing_vectors().unwrap(), 0);

        let rebuild = queue(&db.conn, Kind::Rebuild, MINILM_L6_V2.id, None).unwrap();
        run(&db.conn, DbKind::Main, &mut fake(2), &mut nothing).unwrap();
        assert_eq!(get(&db.conn, rebuild).unwrap().unwrap().state, DONE);
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(MINILM_L6_V2.id)
        );
    }

    #[test]
    fn an_interrupted_job_stays_queued_and_a_failed_one_doesnt_block_the_rest() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "first", "feedback", None).unwrap();
        let fill = queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        let mut stopping = |_: &str, _: &[&str]| -> Result<Vec<Vec<f32>>> {
            Err(Interrupted("exiting".into()).into())
        };
        assert!(run(&db.conn, DbKind::Main, &mut stopping, &mut nothing).is_err());
        assert_eq!(get(&db.conn, fill).unwrap().unwrap().state, QUEUED);

        let mut broken =
            |_: &str, _: &[&str]| -> Result<Vec<Vec<f32>>> { anyhow::bail!("model is broken") };
        run(&db.conn, DbKind::Main, &mut broken, &mut nothing).unwrap();
        let job = get(&db.conn, fill).unwrap().unwrap();
        assert_eq!(job.state, FAILED);
        assert!(job.error.unwrap().contains("model is broken"));
    }

    #[test]
    fn a_one_table_rebuild_leaves_the_others_alone() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "fact", "feedback", None).unwrap();
        let m = db.meeting_add("Sync", None, None, Some("notes")).unwrap();
        db.meeting_store_chunks(m, &[("notes".into(), None)])
            .unwrap();
        queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        run(&db.conn, DbKind::Main, &mut fake(1), &mut nothing).unwrap();

        queue(&db.conn, Kind::Rebuild, GRANITE_SMALL_R2.id, Some("memory")).unwrap();
        let mut seen = Vec::new();
        let mut embed = |_: &str, texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            seen.extend(texts.iter().map(|t| t.to_string()));
            Ok(texts.iter().map(|_| unit(2)).collect())
        };
        run(&db.conn, DbKind::Main, &mut embed, &mut nothing).unwrap();
        assert_eq!(seen, ["fact"]);
        assert!(db.meeting_similar(&unit(1), 1).unwrap()[0].score > 0.99);
    }

    #[test]
    fn a_job_cancelled_while_it_runs_stops_and_stays_cancelled() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..70 {
            db.memory_set(&format!("m{i}"), "a fact", "feedback", None)
                .unwrap();
        }
        let fill = queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        let mut batches = 0;
        let mut embedded = 0;
        let mut embed = |_: &str, texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            batches += 1;
            embedded += texts.len();
            // A job for another model arrives during the first batch.
            if batches == 1 {
                queue(&db.conn, Kind::Rebuild, MINILM_L6_V2.id, None).unwrap();
            }
            Ok(texts.iter().map(|_| unit(1)).collect())
        };
        run(&db.conn, DbKind::Main, &mut embed, &mut nothing).unwrap();
        let job = get(&db.conn, fill).unwrap().unwrap();
        assert_eq!(job.state, CANCELLED);
        assert!(job.error.unwrap().contains("replaced"));
        // The fill's second batch never ran; the rebuild ran next.
        assert_eq!(embedded, 64 + 70);
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some(MINILM_L6_V2.id)
        );
    }

    #[test]
    fn a_fill_can_be_limited_to_one_table() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "fact", "feedback", None).unwrap();
        let m = db.meeting_add("Sync", None, None, Some("notes")).unwrap();
        db.meeting_store_chunks(m, &[("notes".into(), None)])
            .unwrap();
        queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, Some("memory")).unwrap();
        run(&db.conn, DbKind::Main, &mut fake(1), &mut nothing).unwrap();
        assert_eq!(
            db.missing_vectors().unwrap(),
            1,
            "the meeting chunk is left"
        );
    }

    #[test]
    fn only_one_process_runs_a_databases_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
        db.memory_set("a", "fact", "feedback", None).unwrap();
        let fill = queue(&db.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();

        let held = reembed::EmbedLock::try_new(&db.conn).unwrap().unwrap();
        let err = run(&db.conn, DbKind::Main, &mut fake(1), &mut nothing).unwrap_err();
        assert!(is_interrupted(&err), "{err:#}");
        assert_eq!(get(&db.conn, fill).unwrap().unwrap().state, QUEUED);
        assert!(db
            .reembed_all(
                GRANITE_SMALL_R2.id,
                &mut |t| Ok(t.iter().map(|_| unit(1)).collect()),
                &mut |_, _, _| {}
            )
            .is_err());

        drop(held);
        run(&db.conn, DbKind::Main, &mut fake(1), &mut nothing).unwrap();
        assert_eq!(get(&db.conn, fill).unwrap().unwrap().state, DONE);
    }

    #[test]
    fn only_a_database_with_a_queue_counts_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.sqlite");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        assert!(!has_queue(&other).unwrap());
        assert!(!has_queue(&dir.path().join("missing.sqlite")).unwrap());
        let ours = dir.path().join("db.sqlite");
        Database::open(&ours).unwrap();
        assert!(has_queue(&ours).unwrap());
    }

    #[test]
    fn a_repo_fill_embeds_code_chunks() {
        let repo = crate::index::repo_db::RepoDb::open_in_memory().unwrap();
        repo.conn
            .execute_batch(
                "INSERT INTO files (id, path, content_hash) VALUES (1, 'a.rs', 'h');
                 INSERT INTO chunks (file_id, chunk_index, chunk_text) VALUES (1, 0, 'fn a() {}');",
            )
            .unwrap();
        queue(&repo.conn, Kind::Fill, GRANITE_SMALL_R2.id, None).unwrap();
        run(&repo.conn, DbKind::Repo, &mut fake(1), &mut nothing).unwrap();
        assert!(!repo.has_chunks_without_embedding().unwrap());
    }
}
