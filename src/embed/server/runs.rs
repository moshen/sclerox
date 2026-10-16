//! `server.db`: one row per model server run, kept beside the logs in the
//! state directory rather than in a knowledge-base database. One server
//! serves every database, and its memory figures describe this machine, not
//! the data.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::time::Duration;

use super::protocol::ExitReason;

/// Runs older than this are deleted when a server starts.
const KEEP_DAYS: u32 = 30;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS server_runs (
  id          INTEGER PRIMARY KEY,
  pid         INTEGER NOT NULL,
  version     TEXT NOT NULL,
  model_id    TEXT NOT NULL,
  device      TEXT NOT NULL,
  max_tokens  INTEGER NOT NULL,
  started_at  TEXT NOT NULL,
  loaded_mb   INTEGER,
  peak_mb     INTEGER,
  ended_at    TEXT,
  exit_reason TEXT,
  error       TEXT,
  requests    INTEGER NOT NULL DEFAULT 0,
  bulk_items  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS server_runs_model ON server_runs (model_id, started_at);
";

/// A finished or running server, for `server status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Run {
    pub pid: u32,
    pub version: String,
    pub model: String,
    pub device: String,
    pub started_at: String,
    pub loaded_mb: Option<u64>,
    pub peak_mb: Option<u64>,
    pub ended_at: Option<String>,
    pub exit_reason: Option<String>,
    /// Why a `failed` run couldn't start.
    pub error: Option<String>,
    pub requests: u64,
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
        let conn =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        // Commands read this while a server writes it.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record a server starting; returns its run id.
    pub fn start(
        &self,
        version: &str,
        model: &str,
        device: &str,
        max_tokens: usize,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO server_runs (pid, version, model_id, device, max_tokens, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![
                std::process::id(),
                version,
                model,
                device,
                max_tokens as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn loaded(&self, id: i64, loaded_mb: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE server_runs SET loaded_mb = ?2, peak_mb = ?2 WHERE id = ?1",
            params![id, loaded_mb as i64],
        )?;
        Ok(())
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

    /// Mark `model`'s unfinished runs, other than `current`, as crashed. Only
    /// a server holding the model's lock calls this, so none of them is
    /// still running.
    pub fn mark_crashed(&self, model: &str, current: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE server_runs SET ended_at = datetime('now'), exit_reason = ?3
             WHERE model_id = ?1 AND id != ?2 AND ended_at IS NULL",
            params![model, current, ExitReason::Crashed.as_str()],
        )?)
    }

    /// How many of `model`'s runs failed to start or crashed in the last
    /// `window`. Clients stop starting servers for a model that keeps failing.
    pub fn recent_failures(&self, model: &str, window: Duration) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM server_runs
             WHERE model_id = ?1 AND exit_reason IN (?2, ?3)
               AND ended_at >= datetime('now', ?4)",
            params![
                model,
                ExitReason::Failed.as_str(),
                ExitReason::Crashed.as_str(),
                format!("-{} seconds", window.as_secs()),
            ],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// The most recent runs, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT pid, version, model_id, device, started_at, loaded_mb, peak_mb,
                    ended_at, exit_reason, error, requests
             FROM server_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Run {
                pid: r.get(0)?,
                version: r.get(1)?,
                model: r.get(2)?,
                device: r.get(3)?,
                started_at: r.get(4)?,
                loaded_mb: r.get::<_, Option<i64>>(5)?.map(|n| n as u64),
                peak_mb: r.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                ended_at: r.get(7)?,
                exit_reason: r.get(8)?,
                error: r.get(9)?,
                requests: r.get::<_, i64>(10)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete runs that ended more than [`KEEP_DAYS`] ago.
    pub fn prune(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM server_runs WHERE ended_at < datetime('now', ?1)",
            [format!("-{KEEP_DAYS} days")],
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

    #[test]
    fn a_run_records_its_life() {
        let (_dir, runs) = runs();
        let id = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();
        runs.loaded(id, 300).unwrap();
        runs.progress(id, 450, 7).unwrap();
        // A lower reading never lowers the peak.
        runs.progress(id, 350, 9).unwrap();
        runs.end(id, ExitReason::Idle).unwrap();

        let run = &runs.recent(5).unwrap()[0];
        assert_eq!(run.pid, std::process::id());
        assert_eq!((run.loaded_mb, run.peak_mb), (Some(300), Some(450)));
        assert_eq!(run.requests, 9);
        assert_eq!(run.exit_reason.as_deref(), Some("idle"));
        assert!(run.ended_at.is_some());
    }

    #[test]
    fn unfinished_runs_of_the_model_are_marked_crashed() {
        let (_dir, runs) = runs();
        let dead = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();
        let other_model = runs.start("0.1.0", "minilm", "cpu", 256).unwrap();
        let current = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();
        assert_eq!(runs.mark_crashed("granite", current).unwrap(), 1);

        let reason = |id: i64| {
            runs.recent(10)
                .unwrap()
                .into_iter()
                .rev()
                .nth(id as usize - 1)
                .unwrap()
                .exit_reason
        };
        assert_eq!(reason(dead).as_deref(), Some("crashed"));
        assert_eq!(reason(other_model), None);
        assert_eq!(reason(current), None);
        assert_eq!(
            runs.recent_failures("granite", Duration::from_secs(60))
                .unwrap(),
            1
        );
        assert_eq!(
            runs.recent_failures("minilm", Duration::from_secs(60))
                .unwrap(),
            0
        );
    }

    #[test]
    fn old_failures_stop_counting_and_old_runs_are_pruned() {
        let (_dir, runs) = runs();
        let old = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();
        runs.end(old, ExitReason::Failed).unwrap();
        runs.backdate(old, 40);
        let recent = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();
        runs.failed(recent, "no socket").unwrap();
        assert_eq!(
            runs.recent(1).unwrap()[0].error.as_deref(),
            Some("no socket")
        );
        let running = runs.start("0.1.0", "granite", "cpu", 2048).unwrap();

        let window = Duration::from_secs(30 * 60);
        assert_eq!(runs.recent_failures("granite", window).unwrap(), 1);
        assert_eq!(runs.prune().unwrap(), 1);
        assert_eq!(runs.recent(10).unwrap().len(), 2);
        // A run still going is never pruned, however old.
        runs.backdate(running, 40);
        runs.conn
            .execute("UPDATE server_runs SET ended_at = NULL", [])
            .unwrap();
        assert_eq!(runs.prune().unwrap(), 0);
    }
}
