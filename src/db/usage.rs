//! Searches and reads, recorded to measure retrieval (see `MIGRATION_V16`).
//! The CLI decides what to record and swallows failures; see `cli::usage`.

use std::time::Duration;

use anyhow::Result;
use rusqlite::params;

use super::Database;

/// Who ran a command: the agent session and the repo it ran in, when known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageContext {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub repo_path: Option<String>,
}

/// One result of a recorded search, in rank order.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageItem {
    pub item_type: &'static str,
    pub item_ref: String,
    pub score: Option<f32>,
    /// The search tier that found it (`keyword` or `semantic`), when known.
    pub tier: Option<&'static str>,
}

impl UsageItem {
    pub fn new(item_type: &'static str, item_ref: impl ToString) -> Self {
        Self {
            item_type,
            item_ref: item_ref.to_string(),
            score: None,
            tier: None,
        }
    }
}

/// How long a usage write waits on another writer before giving up: long
/// enough for a short write to finish, short enough not to hold up a command.
const USAGE_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
/// rusqlite's default, restored after a usage write.
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl Database {
    /// Record a search and its results, and prune usage rows older than
    /// `retain_days` (0 keeps them). Returns the event's id.
    pub fn usage_log_search(
        &self,
        ctx: &UsageContext,
        command: &str,
        query: &str,
        results: &[UsageItem],
        retain_days: u32,
    ) -> Result<i64> {
        self.usage_write(retain_days, |tx| {
            tx.execute(
                "INSERT INTO retrieval_events
                     (command, query, session_id, agent, repo_path, result_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    command,
                    query,
                    ctx.session_id,
                    ctx.agent,
                    ctx.repo_path,
                    results.len() as i64
                ],
            )?;
            let event = tx.last_insert_rowid();
            let mut stmt = tx.prepare(
                "INSERT INTO retrieval_event_results
                     (event_id, rank, item_type, item_ref, score, tier)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (i, r) in results.iter().enumerate() {
                stmt.execute(params![
                    event,
                    i as i64 + 1,
                    r.item_type,
                    r.item_ref,
                    r.score.map(f64::from),
                    r.tier
                ])?;
            }
            Ok(event)
        })
    }

    /// Record that a command opened one record.
    pub fn usage_log_read(
        &self,
        ctx: &UsageContext,
        command: &str,
        item_type: &str,
        item_ref: &str,
        retain_days: u32,
    ) -> Result<i64> {
        self.usage_write(retain_days, |tx| {
            tx.execute(
                "INSERT INTO read_events
                     (command, item_type, item_ref, session_id, agent, repo_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    command,
                    item_type,
                    item_ref,
                    ctx.session_id,
                    ctx.agent,
                    ctx.repo_path
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// Run `write` and the pruning in one transaction, waiting at most
    /// [`USAGE_BUSY_TIMEOUT`] for the write lock.
    fn usage_write(
        &self,
        retain_days: u32,
        write: impl FnOnce(&rusqlite::Transaction) -> Result<i64>,
    ) -> Result<i64> {
        self.conn.busy_timeout(USAGE_BUSY_TIMEOUT)?;
        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            let id = write(&tx)?;
            if retain_days > 0 {
                let cutoff = format!("-{retain_days} days");
                tx.execute(
                    "DELETE FROM retrieval_events WHERE created_at < datetime('now', ?1)",
                    [&cutoff],
                )?;
                tx.execute(
                    "DELETE FROM read_events WHERE created_at < datetime('now', ?1)",
                    [&cutoff],
                )?;
            }
            tx.commit()?;
            Ok(id)
        })();
        self.conn.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> UsageContext {
        UsageContext {
            session_id: Some("s1".into()),
            agent: Some("claude-code".into()),
            repo_path: None,
        }
    }

    /// rank, item_type, item_ref, score, tier
    type ResultRow = (i64, String, String, Option<f64>, Option<String>);

    #[test]
    fn a_search_is_one_event_with_ordered_results() {
        let db = Database::open_in_memory().unwrap();
        let results = [
            UsageItem {
                score: Some(0.5),
                tier: Some("semantic"),
                ..UsageItem::new("todo", 12)
            },
            UsageItem::new("memory", "gpu-coreml"),
        ];
        let id = db
            .usage_log_search(&ctx(), "search", "why coreml", &results, 90)
            .unwrap();
        let (count, session): (i64, String) = db
            .conn
            .query_row(
                "SELECT result_count, session_id FROM retrieval_events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, session.as_str()), (2, "s1"));
        let rows: Vec<ResultRow> = db
            .conn
            .prepare(
                "SELECT rank, item_type, item_ref, score, tier FROM retrieval_event_results
                 WHERE event_id = ?1 ORDER BY rank",
            )
            .unwrap()
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    1,
                    "todo".into(),
                    "12".into(),
                    Some(0.5),
                    Some("semantic".into())
                ),
                (2, "memory".into(), "gpu-coreml".into(), None, None),
            ]
        );
    }

    #[test]
    fn old_rows_are_pruned_on_write_unless_kept() {
        let db = Database::open_in_memory().unwrap();
        let old = db
            .usage_log_search(&ctx(), "search", "old", &[UsageItem::new("todo", 1)], 0)
            .unwrap();
        db.usage_log_read(&ctx(), "todo get", "todo", "1", 0)
            .unwrap();
        db.conn
            .execute_batch(
                "UPDATE retrieval_events SET created_at = datetime('now', '-40 days');
                 UPDATE read_events SET created_at = datetime('now', '-40 days');",
            )
            .unwrap();
        // 0 keeps everything.
        db.usage_log_read(&ctx(), "todo get", "todo", "2", 0)
            .unwrap();
        let count = |table: &str| -> i64 {
            db.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!((count("retrieval_events"), count("read_events")), (1, 2));

        db.usage_log_read(&ctx(), "todo get", "todo", "3", 30)
            .unwrap();
        assert_eq!((count("retrieval_events"), count("read_events")), (0, 2));
        let results: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retrieval_event_results WHERE event_id = ?1",
                [old],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(results, 0, "results go with their event");
    }
}
