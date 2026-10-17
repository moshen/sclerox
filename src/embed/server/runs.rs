//! `server.db`: one row per model server run, and one per model it loaded,
//! kept beside the logs in the state directory rather than in a
//! knowledge-base database. One server serves every database, and its memory
//! figures describe this machine, not the data.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::time::Duration;

use super::protocol::{DbKind, ExitReason};

fn kind_str(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Main => "main",
        DbKind::Repo => "repo",
    }
}

/// Runs older than this are deleted when a server starts.
const KEEP_DAYS: u32 = 30;

/// `PRAGMA user_version` of the schema below. The file only holds history,
/// so one with another version is emptied and recreated rather than
/// migrated.
const VERSION: i32 = 3;

const SCHEMA: &str = "
CREATE TABLE server_runs (
  id          INTEGER PRIMARY KEY,
  pid         INTEGER NOT NULL,
  version     TEXT NOT NULL,
  started_at  TEXT NOT NULL,
  peak_mb     INTEGER,
  ended_at    TEXT,
  exit_reason TEXT,
  error       TEXT,
  requests    INTEGER NOT NULL DEFAULT 0,
  bulk_items  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE model_loads (
  id          INTEGER PRIMARY KEY,
  run_id      INTEGER NOT NULL,
  model       TEXT NOT NULL,
  device      TEXT NOT NULL,
  max_tokens  INTEGER NOT NULL,
  loaded_at   TEXT NOT NULL,
  load_ms     INTEGER,
  loaded_mb   INTEGER,
  error       TEXT,
  requests    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX model_loads_run ON model_loads (run_id);
CREATE TABLE known_dbs (
  path      TEXT PRIMARY KEY,
  kind      TEXT NOT NULL,
  last_seen TEXT NOT NULL,
  has_work  INTEGER NOT NULL DEFAULT 0
);
";

/// A finished or running server, for `server status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Run {
    pub pid: u32,
    pub version: String,
    pub started_at: String,
    pub peak_mb: Option<u64>,
    pub ended_at: Option<String>,
    pub exit_reason: Option<String>,
    /// Why a `failed` run couldn't start.
    pub error: Option<String>,
    pub requests: u64,
    /// The models it loaded, in order; a failed load is marked so.
    pub models: Vec<String>,
}

/// A model a server loaded, or tried to.
pub struct Load<'a> {
    pub model: &'a str,
    pub device: &'a str,
    pub max_tokens: usize,
}

pub struct Runs {
    conn: Connection,
}

impl Runs {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        // Commands read this while a server writes it.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let current =
            |c: &Connection| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0));
        if current(&conn)? != VERSION {
            // Checked again under the write lock: another process may have
            // just recreated it.
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if current(&tx)? != VERSION {
                tx.execute_batch(&format!(
                    "DROP TABLE IF EXISTS server_runs;
                     DROP TABLE IF EXISTS model_loads;
                     DROP TABLE IF EXISTS known_dbs;
                     {SCHEMA}
                     PRAGMA user_version = {VERSION};"
                ))?;
            }
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// Record a server starting; returns its run id.
    pub fn start(&self, version: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO server_runs (pid, version, started_at) VALUES (?1, ?2, datetime('now'))",
            params![std::process::id(), version],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn progress(&self, id: i64, peak_mb: u64, requests: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE server_runs SET peak_mb = max(coalesce(peak_mb, 0), ?2), requests = ?3
             WHERE id = ?1",
            params![id, peak_mb as i64, requests as i64],
        )?;
        Ok(())
    }

    pub fn end(&self, id: i64, reason: ExitReason) -> Result<()> {
        self.conn.execute(
            "UPDATE server_runs SET ended_at = datetime('now'), exit_reason = ?2 WHERE id = ?1",
            params![id, reason.as_str()],
        )?;
        Ok(())
    }

    /// Record a run that couldn't start, and why.
    pub fn failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE server_runs SET ended_at = datetime('now'), exit_reason = ?2, error = ?3
             WHERE id = ?1",
            params![id, ExitReason::Failed.as_str(), error],
        )?;
        Ok(())
    }

    /// Record a model load in run `run_id`: how long it took and how much
    /// memory it added, or why it failed. Returns the load's id.
    pub fn loaded(
        &self,
        run_id: i64,
        load: &Load,
        took: Duration,
        outcome: std::result::Result<u64, &str>,
    ) -> Result<i64> {
        let (loaded_mb, error) = match outcome {
            Ok(mb) => (Some(mb as i64), None),
            Err(e) => (None, Some(e)),
        };
        self.conn.execute(
            "INSERT INTO model_loads
               (run_id, model, device, max_tokens, loaded_at, load_ms, loaded_mb, error)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, ?6, ?7)",
            params![
                run_id,
                load.model,
                load.device,
                load.max_tokens as i64,
                took.as_millis() as i64,
                loaded_mb,
                error
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn model_progress(&self, load_id: i64, requests: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE model_loads SET requests = ?2 WHERE id = ?1",
            params![load_id, requests as i64],
        )?;
        Ok(())
    }

    /// Mark unfinished runs other than `current` as crashed. Only a server
    /// holding the lock calls this, so none of them is still running.
    pub fn mark_crashed(&self, current: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE server_runs SET ended_at = datetime('now'), exit_reason = ?2
             WHERE id != ?1 AND ended_at IS NULL",
            params![current, ExitReason::Crashed.as_str()],
        )?)
    }

    /// How many runs failed to start or crashed in the last `window`.
    /// Clients stop starting a server that keeps failing.
    pub fn recent_failures(&self, window: Duration) -> Result<usize> {
        self.recent_exits(&[ExitReason::Failed, ExitReason::Crashed], window)
    }

    /// Runs that ended for one of `reasons` within `window`.
    pub fn recent_exits(&self, reasons: &[ExitReason], window: Duration) -> Result<usize> {
        let reasons: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM server_runs
             WHERE exit_reason IN (SELECT value FROM json_each(?1))
               AND ended_at >= datetime('now', ?2)",
            params![
                serde_json::to_string(&reasons)?,
                format!("-{} seconds", window.as_secs()),
            ],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Note that the database at `path` has queued work (`has_work`) or
    /// none left.
    pub fn set_work(&self, path: &str, kind: DbKind, has_work: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO known_dbs (path, kind, last_seen, has_work)
             VALUES (?1, ?2, datetime('now'), ?3)
             ON CONFLICT (path) DO UPDATE SET
               kind = excluded.kind, last_seen = excluded.last_seen, has_work = excluded.has_work",
            params![path, kind_str(kind), has_work],
        )?;
        Ok(())
    }

    /// Databases with queued work, for a new server to go on with.
    pub fn with_work(&self) -> Result<Vec<(String, DbKind)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, kind FROM known_dbs WHERE has_work ORDER BY last_seen")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (path, kind) = row?;
            match kind.as_str() {
                "main" => out.push((path, DbKind::Main)),
                "repo" => out.push((path, DbKind::Repo)),
                _ => {}
            }
        }
        Ok(out)
    }

    /// The most recent runs, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.pid, r.version, r.started_at, r.peak_mb, r.ended_at, r.exit_reason,
                    r.error, r.requests,
                    (SELECT group_concat(name, char(31)) FROM (
                       SELECT m.model || CASE WHEN m.error IS NULL THEN '' ELSE ' (failed)' END
                              AS name
                       FROM model_loads m WHERE m.run_id = r.id ORDER BY m.id))
             FROM server_runs r ORDER BY r.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let models: Option<String> = r.get(8)?;
            Ok(Run {
                pid: r.get(0)?,
                version: r.get(1)?,
                started_at: r.get(2)?,
                peak_mb: r.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                ended_at: r.get(4)?,
                exit_reason: r.get(5)?,
                error: r.get(6)?,
                requests: r.get::<_, i64>(7)? as u64,
                models: models
                    .map(|m| m.split('\u{1f}').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete runs, and their model loads, that ended more than
    /// [`KEEP_DAYS`] ago.
    pub fn prune(&self) -> Result<usize> {
        let cutoff = format!("-{KEEP_DAYS} days");
        self.conn.execute(
            "DELETE FROM model_loads WHERE run_id IN
               (SELECT id FROM server_runs WHERE ended_at < datetime('now', ?1))",
            [&cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM known_dbs WHERE NOT has_work AND last_seen < datetime('now', ?1)",
            [&cutoff],
        )?;
        Ok(self.conn.execute(
            "DELETE FROM server_runs WHERE ended_at < datetime('now', ?1)",
            [&cutoff],
        )?)
    }

    #[cfg(test)]
    fn backdate(&self, id: i64, days: u32) {
        self.conn
            .execute(
                "UPDATE server_runs SET ended_at = datetime('now', ?2) WHERE id = ?1",
                params![id, format!("-{days} days")],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs() -> (tempfile::TempDir, Runs) {
        let dir = tempfile::TempDir::new().unwrap();
        let runs = Runs::open(&dir.path().join("state/server.db")).unwrap();
        (dir, runs)
    }

    const GRANITE: Load = Load {
        model: "granite",
        device: "cpu",
        max_tokens: 2048,
    };

    #[test]
    fn a_run_records_its_life_and_its_models() {
        let (_dir, runs) = runs();
        let id = runs.start("0.1.0").unwrap();
        let granite = runs
            .loaded(id, &GRANITE, Duration::from_millis(250), Ok(300))
            .unwrap();
        let minilm = Load {
            model: "minilm",
            max_tokens: 256,
            ..GRANITE
        };
        runs.loaded(id, &minilm, Duration::ZERO, Err("no such file"))
            .unwrap();
        runs.model_progress(granite, 7).unwrap();
        runs.progress(id, 450, 7).unwrap();
        // A lower reading never lowers the peak.
        runs.progress(id, 350, 9).unwrap();
        runs.end(id, ExitReason::Idle).unwrap();

        let run = &runs.recent(5).unwrap()[0];
        assert_eq!(run.pid, std::process::id());
        assert_eq!(run.peak_mb, Some(450));
        assert_eq!(run.requests, 9);
        assert_eq!(run.exit_reason.as_deref(), Some("idle"));
        assert!(run.ended_at.is_some());
        assert_eq!(run.models, ["granite", "minilm (failed)"]);
    }

    #[test]
    fn unfinished_runs_are_marked_crashed() {
        let (_dir, runs) = runs();
        let _dead = runs.start("0.1.0").unwrap();
        let done = runs.start("0.1.0").unwrap();
        runs.end(done, ExitReason::Idle).unwrap();
        let current = runs.start("0.1.0").unwrap();
        assert_eq!(runs.mark_crashed(current).unwrap(), 1);

        let reasons: Vec<_> = runs
            .recent(10)
            .unwrap()
            .into_iter()
            .map(|r| r.exit_reason)
            .collect();
        assert_eq!(
            reasons,
            [None, Some("idle".to_string()), Some("crashed".to_string())]
        );
        assert_eq!(runs.recent_failures(Duration::from_secs(60)).unwrap(), 1);
    }

    #[test]
    fn old_failures_stop_counting_and_old_runs_are_pruned() {
        let (_dir, runs) = runs();
        let old = runs.start("0.1.0").unwrap();
        runs.loaded(old, &GRANITE, Duration::ZERO, Ok(1)).unwrap();
        runs.end(old, ExitReason::Failed).unwrap();
        runs.backdate(old, 40);
        let recent = runs.start("0.1.0").unwrap();
        runs.failed(recent, "no socket").unwrap();
        assert_eq!(
            runs.recent(1).unwrap()[0].error.as_deref(),
            Some("no socket")
        );
        let running = runs.start("0.1.0").unwrap();

        let window = Duration::from_secs(30 * 60);
        assert_eq!(runs.recent_failures(window).unwrap(), 1);
        let memory = runs.start("0.1.0").unwrap();
        runs.end(memory, ExitReason::Memory).unwrap();
        assert_eq!(runs.recent_failures(window).unwrap(), 1, "not a failure");
        assert_eq!(runs.recent_exits(&[ExitReason::Memory], window).unwrap(), 1);
        assert_eq!(runs.prune().unwrap(), 1);
        assert_eq!(runs.recent(10).unwrap().len(), 3);
        let loads: i64 = runs
            .conn
            .query_row("SELECT count(*) FROM model_loads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(loads, 0, "a pruned run's loads go with it");
        // A run still going is never pruned, however old.
        runs.backdate(running, 40);
        runs.conn
            .execute("UPDATE server_runs SET ended_at = NULL", [])
            .unwrap();
        assert_eq!(runs.prune().unwrap(), 0);
    }

    #[test]
    fn a_file_from_another_schema_is_recreated() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("server.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE server_runs (id INTEGER PRIMARY KEY, model_id TEXT NOT NULL);
             INSERT INTO server_runs (model_id) VALUES ('granite');",
        )
        .unwrap();
        drop(old);

        let runs = Runs::open(&path).unwrap();
        assert!(runs.recent(10).unwrap().is_empty());
        runs.start("0.1.0").unwrap();
        // Opening again keeps what is there now.
        assert_eq!(Runs::open(&path).unwrap().recent(10).unwrap().len(), 1);
    }
}
