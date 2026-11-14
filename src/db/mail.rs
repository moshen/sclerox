//! The agent mailbox (see `MIGRATION_V21`): a directory of agents that opted
//! in to receive mail, and the messages sent to them.
//!
//! An agent is a session that registered with what it is for. It is
//! addressed by its name, if it took one, else by its session ID or a
//! prefix of it. A message is unread until its recipient is shown it, by a
//! command or a hook; being shown is what reads it. Taking unread mail is
//! one transaction, so a hook and a command never both show the same
//! message.
//!
//! An agent is active while it hasn't left and was seen within a window the
//! caller passes (the CLI takes it from `[mail] active_minutes`). Library
//! code stays settings-free.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::Serialize;

use super::Database;

/// How long a hook waits on another writer before leaving the mail for the
/// next hook: never long enough to hold up the agent.
const HOOK_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
/// rusqlite's default, restored after a hook's write.
const DEFAULT_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// `last_seen_at` is only rewritten once it is this old, so a hook running
/// after every tool call doesn't write each time.
const SEEN_GRANULARITY: &str = "-60 seconds";

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: i64,
    pub session_id: String,
    pub name: Option<String>,
    pub summary: String,
    pub agent: Option<String>,
    pub repo_path: Option<String>,
    pub registered_at: String,
    pub last_seen_at: String,
    pub left_at: Option<String>,
    /// Hasn't left, and was seen within the active window.
    pub active: bool,
    pub unread: i64,
}

impl Agent {
    /// How others address it: its name, else its whole session ID (a
    /// prefix could match another agent's session).
    pub fn address(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.session_id.clone())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    /// Who sent it, as they were addressed then (or `user` from outside a
    /// session).
    pub sender: String,
    /// The session that sent it; none when sent from outside a session.
    pub sender_session: Option<String>,
    /// The sending agent, while it is registered: who a reply goes to.
    pub sender_id: Option<i64>,
    pub recipient: String,
    pub recipient_id: i64,
    pub body: String,
    pub reply_to: Option<i64>,
    pub created_at: String,
    pub read_at: Option<String>,
}

/// Who is sending a message.
#[derive(Debug, Default)]
pub struct Sender<'a> {
    /// How the sender is shown: its agent's address, its short session ID,
    /// or `user`.
    pub label: &'a str,
    pub session_id: Option<&'a str>,
    /// Its agent, if it is registered.
    pub agent_id: Option<i64>,
}

/// A session registering, or updating its registration.
#[derive(Debug, Default)]
pub struct Registration<'a> {
    pub session_id: &'a str,
    pub name: Option<&'a str>,
    /// Required for a new agent; kept when updating if not given.
    pub summary: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub repo_path: Option<&'a str>,
    /// Take over a name that an active agent holds.
    pub replace: bool,
}

/// A session's mailbox at a glance, read without writing: what a hook checks
/// before deciding to do anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peek {
    pub agent_id: i64,
    pub unread: bool,
    /// `last_seen_at` is old enough to be worth rewriting.
    pub seen_stale: bool,
}

const AGENT_COLUMNS: &str = "a.id, a.session_id, a.name, a.summary, a.agent, a.repo_path,
    a.registered_at, a.last_seen_at, a.left_at,
    a.left_at IS NULL AND a.last_seen_at >= datetime('now', ?1),
    (SELECT count(*) FROM messages m WHERE m.recipient_id = a.id AND m.read_at IS NULL)";

const MESSAGE_SELECT: &str = "SELECT m.id, m.sender, m.sender_session, m.sender_id,
    COALESCE(r.name, r.session_id), m.recipient_id, m.body, m.reply_to,
    m.created_at, m.read_at
    FROM messages m JOIN agents r ON r.id = m.recipient_id";

fn window(active_minutes: u32) -> String {
    format!("-{active_minutes} minutes")
}

fn row_to_agent(r: &Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: r.get(0)?,
        session_id: r.get(1)?,
        name: r.get(2)?,
        summary: r.get(3)?,
        agent: r.get(4)?,
        repo_path: r.get(5)?,
        registered_at: r.get(6)?,
        last_seen_at: r.get(7)?,
        left_at: r.get(8)?,
        active: r.get(9)?,
        unread: r.get(10)?,
    })
}

fn row_to_message(r: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        sender: r.get(1)?,
        sender_session: r.get(2)?,
        sender_id: r.get(3)?,
        recipient: r.get(4)?,
        recipient_id: r.get(5)?,
        body: r.get(6)?,
        reply_to: r.get(7)?,
        created_at: r.get(8)?,
        read_at: r.get(9)?,
    })
}

fn agent_where(
    conn: &rusqlite::Connection,
    active_minutes: u32,
    clause: &str,
    arg: &dyn rusqlite::ToSql,
) -> Result<Option<Agent>> {
    let sql = format!("SELECT {AGENT_COLUMNS} FROM agents a WHERE {clause}");
    Ok(conn
        .query_row(&sql, params![window(active_minutes), arg], row_to_agent)
        .optional()?)
}

impl Database {
    /// Register `reg.session_id`, or update its registration, and return the
    /// agent. A name held by another agent is taken over, with its unread
    /// mail, if that agent left or isn't active, or if `reg.replace`.
    pub fn agent_register(&self, reg: &Registration, active_minutes: u32) -> Result<Agent> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mine = agent_where(&tx, active_minutes, "a.session_id = ?2", &reg.session_id)?;
        let holder = match reg.name {
            Some(name) => agent_where(&tx, active_minutes, "a.name = ?2", &name)?,
            None => None,
        };

        let id = match (mine, holder) {
            (Some(mine), Some(holder)) if mine.id == holder.id => mine.id,
            (mine, Some(holder)) => {
                if holder.active && !reg.replace {
                    bail!(
                        "'{}' is held by active session {} (last seen {} UTC); \
                         pass --replace to take it over",
                        holder.address(),
                        holder.session_id,
                        holder.last_seen_at
                    );
                }
                // This session's own mailbox, if it had one, folds into the
                // one it takes over.
                if let Some(mine) = mine {
                    tx.execute(
                        "UPDATE messages SET recipient_id = ?1 WHERE recipient_id = ?2",
                        params![holder.id, mine.id],
                    )?;
                    tx.execute(
                        "UPDATE messages SET sender_id = ?1 WHERE sender_id = ?2",
                        params![holder.id, mine.id],
                    )?;
                    tx.execute("DELETE FROM agents WHERE id = ?1", params![mine.id])?;
                }
                tx.execute(
                    "UPDATE agents SET session_id = ?1 WHERE id = ?2",
                    params![reg.session_id, holder.id],
                )?;
                holder.id
            }
            (Some(mine), None) => {
                if let Some(name) = reg.name {
                    tx.execute(
                        "UPDATE agents SET name = ?1 WHERE id = ?2",
                        params![name, mine.id],
                    )?;
                }
                mine.id
            }
            (None, None) => {
                let Some(summary) = reg.summary.filter(|s| !s.trim().is_empty()) else {
                    bail!("a summary of what this agent is for is required to register");
                };
                tx.execute(
                    "INSERT INTO agents (session_id, name, summary) VALUES (?1, ?2, ?3)",
                    params![reg.session_id, reg.name, summary],
                )?;
                tx.last_insert_rowid()
            }
        };
        tx.execute(
            "UPDATE agents SET
                 summary = COALESCE(?1, summary),
                 agent = COALESCE(?2, agent),
                 repo_path = COALESCE(?3, repo_path),
                 last_seen_at = datetime('now'),
                 left_at = NULL
             WHERE id = ?4",
            params![
                reg.summary.filter(|s| !s.trim().is_empty()),
                reg.agent,
                reg.repo_path,
                id
            ],
        )?;
        let agent = agent_where(&tx, active_minutes, "a.id = ?2", &id)?
            .expect("the agent was just written");
        tx.commit()?;
        Ok(agent)
    }

    /// Mark the session's agent as left, so it is no longer listed or sent
    /// mail and its name is free. False if it isn't registered.
    pub fn agent_leave(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agents SET left_at = datetime('now')
             WHERE session_id = ?1 AND left_at IS NULL",
            params![session_id],
        )? > 0)
    }

    pub fn agent_get(&self, id: i64, active_minutes: u32) -> Result<Option<Agent>> {
        agent_where(&self.conn, active_minutes, "a.id = ?2", &id)
    }

    /// The agent `session_id` holds, left or not.
    pub fn agent_by_session(&self, session_id: &str, active_minutes: u32) -> Result<Option<Agent>> {
        agent_where(&self.conn, active_minutes, "a.session_id = ?2", &session_id)
    }

    /// The agent `address` names: a name, a session ID, or a prefix of one
    /// (at least four characters) that only one agent's session starts with.
    pub fn agent_resolve(&self, address: &str, active_minutes: u32) -> Result<Option<Agent>> {
        let address = address.trim();
        if let Some(a) = agent_where(&self.conn, active_minutes, "a.name = ?2", &address)? {
            return Ok(Some(a));
        }
        if let Some(a) = agent_where(&self.conn, active_minutes, "a.session_id = ?2", &address)? {
            return Ok(Some(a));
        }
        if address.chars().count() < 4 {
            return Ok(None);
        }
        let escaped = address
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let sql = format!(
            "SELECT {AGENT_COLUMNS} FROM agents a
             WHERE a.session_id LIKE ?2 ESCAPE '\\' ORDER BY a.id LIMIT 2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let found: Vec<Agent> = stmt
            .query_map(params![window(active_minutes), pattern], row_to_agent)?
            .collect::<rusqlite::Result<_>>()?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.into_iter().next()),
            _ => bail!("'{address}' matches more than one agent's session; give more of it"),
        }
    }

    /// Agents, most recently seen first: the active ones, or with `all` every
    /// one, left and inactive included.
    pub fn agent_list(&self, all: bool, active_minutes: u32) -> Result<Vec<Agent>> {
        let sql = format!(
            "SELECT {AGENT_COLUMNS} FROM agents a
             WHERE ?2 OR (a.left_at IS NULL AND a.last_seen_at >= datetime('now', ?1))
             ORDER BY a.last_seen_at DESC, a.id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let agents = stmt
            .query_map(params![window(active_minutes), all], row_to_agent)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(agents)
    }

    /// Queue `body` from `sender` for agent `recipient_id` and return the
    /// message. Fails if the recipient has left or is gone: that is checked
    /// by the insert itself, so it can't change in between.
    pub fn mail_send(
        &self,
        sender: &Sender,
        recipient_id: i64,
        body: &str,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        if body.trim().is_empty() {
            bail!("the message is empty");
        }
        // SQLite's length() stops at a NUL, which would let a message slip
        // past a hook's character budget; a message has no use for one.
        if body.contains('\0') {
            bail!("the message holds a NUL character");
        }
        // The insert and the read back are one transaction, so a prune in
        // another process can't delete the message in between.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO messages
                 (sender, sender_session, sender_id, recipient_id, body, reply_to)
             SELECT ?1, ?2, ?3, id, ?5, ?6 FROM agents WHERE id = ?4 AND left_at IS NULL",
            params![
                sender.label,
                sender.session_id,
                sender.agent_id,
                recipient_id,
                body,
                reply_to
            ],
        )?;
        if inserted == 0 {
            bail!("the recipient has left or is no longer registered");
        }
        let message = tx.query_row(
            &format!("{MESSAGE_SELECT} WHERE m.id = ?1"),
            params![tx.last_insert_rowid()],
            row_to_message,
        )?;
        tx.commit()?;
        Ok(message)
    }

    pub fn mail_get(&self, id: i64) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row(
                &format!("{MESSAGE_SELECT} WHERE m.id = ?1"),
                params![id],
                row_to_message,
            )
            .optional()?)
    }

    /// Mark message `id` read, if it isn't yet.
    pub fn mail_mark_read(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET read_at = datetime('now') WHERE id = ?1 AND read_at IS NULL",
            params![id],
        )?;
        Ok(())
    }

    /// Take the unread mail of the mailbox `session_id` holds, oldest first,
    /// marking it read, and note that the agent was seen. With `max_chars`,
    /// stops before the message whose body would take the total past it
    /// (always taking at least one). Returns the messages and how many are
    /// left unread; nothing if the session holds no mailbox, or left.
    ///
    /// The mailbox is looked up in the same transaction that reads the mail,
    /// so mail is never shown to a session that has just lost its mailbox to
    /// another.
    pub fn mail_take_unread(
        &self,
        session_id: &str,
        max_chars: Option<usize>,
    ) -> Result<(Vec<Message>, usize)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(agent_id) = tx
            .query_row(
                "SELECT id FROM agents WHERE session_id = ?1 AND left_at IS NULL",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok((Vec::new(), 0));
        };
        // Choose the batch by body length, so a backlog's bodies aren't
        // loaded only to be left for the next hook.
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id, length(body) FROM messages
                 WHERE recipient_id = ?1 AND read_at IS NULL ORDER BY id",
            )?;
            let mut rows = stmt.query(params![agent_id])?;
            let mut ids = Vec::new();
            let mut chars = 0;
            while let Some(row) = rows.next()? {
                let len = row.get::<_, i64>(1)? as usize;
                if !ids.is_empty() && max_chars.is_some_and(|max| chars + len > max) {
                    break;
                }
                chars += len;
                ids.push(row.get(0)?);
            }
            ids
        };
        let mut taken = Vec::with_capacity(ids.len());
        for id in ids {
            tx.execute(
                "UPDATE messages SET read_at = datetime('now') WHERE id = ?1",
                params![id],
            )?;
            taken.push(tx.query_row(
                &format!("{MESSAGE_SELECT} WHERE m.id = ?1"),
                params![id],
                row_to_message,
            )?);
        }
        let left: i64 = tx.query_row(
            "SELECT count(*) FROM messages WHERE recipient_id = ?1 AND read_at IS NULL",
            params![agent_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE agents SET last_seen_at = datetime('now') WHERE id = ?1",
            params![agent_id],
        )?;
        tx.commit()?;
        Ok((taken, left as usize))
    }

    /// `agent`'s latest mail, newest first: received, and sent by it or by
    /// its session, including before it registered.
    pub fn mail_history(&self, agent: &Agent, limit: usize) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "{MESSAGE_SELECT}
             WHERE m.recipient_id = ?1 OR m.sender_id = ?1 OR m.sender_session = ?2
             ORDER BY m.id DESC LIMIT ?3"
        ))?;
        let messages = stmt
            .query_map(
                params![agent.id, agent.session_id, limit as i64],
                row_to_message,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(messages)
    }

    /// `session_id`'s mailbox, if it is registered and hasn't left: read
    /// only, for a hook that runs after every tool call.
    pub fn mail_peek(&self, session_id: &str) -> Result<Option<Peek>> {
        Ok(self
            .conn
            .query_row(
                "SELECT a.id,
                        EXISTS(SELECT 1 FROM messages m
                               WHERE m.recipient_id = a.id AND m.read_at IS NULL),
                        a.last_seen_at < datetime('now', ?2)
                 FROM agents a WHERE a.session_id = ?1 AND a.left_at IS NULL",
                params![session_id, SEEN_GRANULARITY],
                |r| {
                    Ok(Peek {
                        agent_id: r.get(0)?,
                        unread: r.get(1)?,
                        seen_stale: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Note that the agent `session_id` holds was seen now.
    pub fn agent_touch(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agents SET last_seen_at = datetime('now')
             WHERE session_id = ?1 AND left_at IS NULL",
            params![session_id],
        )?;
        Ok(())
    }

    /// For a hook: take `session_id`'s unread mail as [`Self::mail_take_unread`]
    /// does, or just note it was seen if that is due. Does nothing for a
    /// session that isn't registered, and leaves the mail for the next hook
    /// if another writer holds the database for more than a moment.
    pub fn mail_hook_take(
        &self,
        session_id: &str,
        max_chars: usize,
    ) -> Result<(Vec<Message>, usize)> {
        let Some(peek) = self.mail_peek(session_id)? else {
            return Ok((Vec::new(), 0));
        };
        if !peek.unread && !peek.seen_stale {
            return Ok((Vec::new(), 0));
        }
        self.conn.busy_timeout(HOOK_BUSY_TIMEOUT)?;
        let result = if peek.unread {
            self.mail_take_unread(session_id, Some(max_chars))
        } else {
            self.agent_touch(session_id).map(|()| (Vec::new(), 0))
        };
        self.conn.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
        result
    }

    /// Delete read mail older than `retain_days`, and agents not seen for as
    /// long with all their mail. 0 keeps everything.
    pub fn mail_prune(&self, retain_days: u32) -> Result<()> {
        if retain_days == 0 {
            return Ok(());
        }
        let cutoff = format!("-{retain_days} days");
        self.conn.execute(
            "DELETE FROM messages WHERE read_at IS NOT NULL AND read_at < datetime('now', ?1)",
            params![cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM agents WHERE last_seen_at < datetime('now', ?1)",
            params![cutoff],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: u32 = 30;

    fn user() -> Sender<'static> {
        Sender {
            label: "user",
            ..Default::default()
        }
    }

    fn from(agent: &Agent) -> Sender<'_> {
        Sender {
            label: agent.name.as_deref().unwrap_or("agent"),
            session_id: Some(&agent.session_id),
            agent_id: Some(agent.id),
        }
    }

    fn register(db: &Database, session: &str, name: Option<&str>) -> Result<Agent> {
        db.agent_register(
            &Registration {
                session_id: session,
                name,
                summary: Some("reviews pull requests"),
                ..Default::default()
            },
            WINDOW,
        )
    }

    fn age(db: &Database, id: i64, modifier: &str) {
        db.conn
            .execute(
                "UPDATE agents SET last_seen_at = datetime('now', ?1) WHERE id = ?2",
                params![modifier, id],
            )
            .unwrap();
    }

    #[test]
    fn registering_needs_a_summary_once() {
        let db = Database::open_in_memory().unwrap();
        let err = db
            .agent_register(
                &Registration {
                    session_id: "s1",
                    ..Default::default()
                },
                WINDOW,
            )
            .unwrap_err();
        assert!(err.to_string().contains("summary"), "{err}");

        let first = register(&db, "s1", None).unwrap();
        let again = db
            .agent_register(
                &Registration {
                    session_id: "s1",
                    name: Some("reviewer"),
                    ..Default::default()
                },
                WINDOW,
            )
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(again.summary, "reviews pull requests");
        assert_eq!(again.address(), "reviewer");
        assert!(again.active);
    }

    #[test]
    fn an_active_name_is_only_taken_over_with_replace() {
        let db = Database::open_in_memory().unwrap();
        let old = register(&db, "old-session", Some("reviewer")).unwrap();
        db.mail_send(&user(), old.id, "look at #12", None).unwrap();

        let err = register(&db, "new-session", Some("reviewer")).unwrap_err();
        assert!(err.to_string().contains("--replace"), "{err}");

        age(&db, old.id, "-2 hours");
        let new = register(&db, "new-session", Some("reviewer")).unwrap();
        assert_eq!(new.id, old.id, "the mailbox is kept");
        assert_eq!(new.session_id, "new-session");
        assert_eq!(new.unread, 1, "and its unread mail with it");
        assert!(db
            .agent_by_session("old-session", WINDOW)
            .unwrap()
            .is_none());
    }

    #[test]
    fn taking_over_a_name_folds_in_the_sessions_own_mailbox() {
        let db = Database::open_in_memory().unwrap();
        let held = register(&db, "s1", Some("builder")).unwrap();
        let mine = register(&db, "s2", None).unwrap();
        db.mail_send(&user(), mine.id, "to the unnamed one", None)
            .unwrap();
        db.agent_leave("s1").unwrap();

        let merged = register(&db, "s2", Some("builder")).unwrap();
        assert_eq!(merged.id, held.id);
        assert_eq!(merged.unread, 1);
        assert_eq!(db.agent_list(true, WINDOW).unwrap().len(), 1);
    }

    #[test]
    fn listing_leaves_out_left_and_idle_agents_unless_asked() {
        let db = Database::open_in_memory().unwrap();
        register(&db, "here", None).unwrap();
        let idle = register(&db, "idle", None).unwrap();
        register(&db, "gone", None).unwrap();
        age(&db, idle.id, "-31 minutes");
        db.agent_leave("gone").unwrap();

        let active: Vec<_> = db
            .agent_list(false, WINDOW)
            .unwrap()
            .into_iter()
            .map(|a| a.session_id)
            .collect();
        assert_eq!(active, ["here"]);
        assert_eq!(db.agent_list(true, WINDOW).unwrap().len(), 3);
    }

    #[test]
    fn resolves_names_sessions_and_unique_prefixes() {
        let db = Database::open_in_memory().unwrap();
        let a = register(&db, "abcd1234-0000", Some("planner")).unwrap();
        let b = register(&db, "abcd9999-0000", None).unwrap();
        let resolve = |s| db.agent_resolve(s, WINDOW).unwrap().map(|a| a.id);
        assert_eq!(resolve("planner"), Some(a.id));
        assert_eq!(resolve("abcd9999-0000"), Some(b.id));
        assert_eq!(resolve("abcd99"), Some(b.id));
        assert_eq!(resolve("abc"), None, "too short to be a prefix");
        let c = register(&db, "work_er-0003", None).unwrap();
        register(&db, "workXer-0004", None).unwrap();
        assert_eq!(resolve("work_e"), Some(c.id), "`_` is literal");
        assert_eq!(resolve("work%"), None, "`%` is literal");
        assert_eq!(resolve("zzzz"), None);
        assert!(db.agent_resolve("abcd", WINDOW).is_err(), "ambiguous");
    }

    #[test]
    fn taking_unread_reads_it_once_within_the_budget() {
        let db = Database::open_in_memory().unwrap();
        let me = register(&db, "s1", None).unwrap();
        for body in ["aaaa", "bbbb", "cccc"] {
            db.mail_send(&user(), me.id, body, None).unwrap();
        }
        let (taken, left) = db.mail_take_unread("s1", Some(9)).unwrap();
        let bodies: Vec<_> = taken.iter().map(|m| m.body.as_str()).collect();
        assert_eq!((bodies, left), (vec!["aaaa", "bbbb"], 1));
        assert!(taken.iter().all(|m| m.read_at.is_some()));

        let (taken, left) = db.mail_take_unread("s1", Some(1)).unwrap();
        assert_eq!(taken.len(), 1, "always at least one");
        assert_eq!(left, 0);
        assert!(db.mail_take_unread("s1", None).unwrap().0.is_empty());
        assert!(db.mail_take_unread("nobody", None).unwrap().0.is_empty());
    }

    #[test]
    fn taking_within_a_budget_counts_what_is_left() {
        let db = Database::open_in_memory().unwrap();
        let me = register(&db, "s1", None).unwrap();
        for _ in 0..5 {
            db.mail_send(&user(), me.id, &"x".repeat(10), None).unwrap();
        }
        let (taken, left) = db.mail_take_unread("s1", Some(25)).unwrap();
        assert_eq!((taken.len(), left), (2, 3));
        assert!(taken
            .iter()
            .all(|m| m.read_at.is_some() && m.body.len() == 10));
        assert_eq!(
            me.address(),
            "s1",
            "an unnamed agent goes by its whole session"
        );
    }

    #[test]
    fn a_hook_ignores_unregistered_sessions_and_takes_mail_for_registered_ones() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.mail_hook_take("nobody", 1000).unwrap().0.len(), 0);

        let me = register(&db, "s1", None).unwrap();
        assert_eq!(
            db.mail_peek("s1").unwrap(),
            Some(Peek {
                agent_id: me.id,
                unread: false,
                seen_stale: false
            })
        );
        db.mail_send(&user(), me.id, "ship it", None).unwrap();
        let (taken, _) = db.mail_hook_take("s1", 1000).unwrap();
        assert_eq!(taken[0].body, "ship it");
        assert!(!db.mail_peek("s1").unwrap().unwrap().unread);

        db.agent_leave("s1").unwrap();
        assert_eq!(db.mail_peek("s1").unwrap(), None);
    }

    #[test]
    fn a_session_that_lost_its_mailbox_is_not_shown_its_mail() {
        let db = Database::open_in_memory().unwrap();
        let old = register(&db, "old-session", Some("reviewer")).unwrap();
        db.mail_send(&user(), old.id, "for whoever holds reviewer", None)
            .unwrap();
        // The old session's hook peeked and saw mail, then the name changed
        // hands before the hook took it.
        assert!(db.mail_peek("old-session").unwrap().unwrap().unread);
        age(&db, old.id, "-2 hours");
        register(&db, "new-session", Some("reviewer")).unwrap();

        assert!(db
            .mail_take_unread("old-session", Some(1000))
            .unwrap()
            .0
            .is_empty());
        assert!(db.mail_hook_take("old-session", 1000).unwrap().0.is_empty());
        let (taken, _) = db.mail_take_unread("new-session", None).unwrap();
        assert_eq!(taken.len(), 1);
    }

    #[test]
    fn mail_to_an_agent_that_left_is_refused_by_the_insert() {
        let db = Database::open_in_memory().unwrap();
        let gone = register(&db, "s1", None).unwrap();
        db.agent_leave("s1").unwrap();
        let err = db.mail_send(&user(), gone.id, "hello?", None).unwrap_err();
        assert!(err.to_string().contains("left"), "{err}");
        assert!(db.mail_send(&user(), 999, "nobody", None).is_err());
    }

    #[test]
    fn foreign_keys_from_messages_are_indexed() {
        let db = Database::open_in_memory().unwrap();
        let mut stmt = db
            .conn
            .prepare("SELECT \"from\" FROM pragma_foreign_key_list('messages')")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for column in columns {
            let indexed: bool = db
                .conn
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM pragma_index_list('messages') l,
                                       pragma_index_info(l.name) i
                         WHERE i.name = ?1 AND i.seqno = 0)",
                    params![column],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(indexed, "messages.{column} has no index for deletes to use");
        }
    }

    #[test]
    fn a_body_with_a_nul_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let me = register(&db, "s1", None).unwrap();
        let err = db
            .mail_send(&user(), me.id, &format!("\0{}", "x".repeat(10)), None)
            .unwrap_err();
        assert!(err.to_string().contains("NUL"), "{err}");
    }

    #[test]
    fn pruning_finds_what_to_delete_by_index() {
        let db = Database::open_in_memory().unwrap();
        for (sql, index) in [
            (
                "DELETE FROM messages WHERE read_at IS NOT NULL AND read_at < datetime('now', '-14 days')",
                "messages_read",
            ),
            (
                "DELETE FROM agents WHERE last_seen_at < datetime('now', '-14 days')",
                "agents_last_seen",
            ),
        ] {
            let mut stmt = db
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map([], |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter().any(|p| p.contains(&format!("INDEX {index} "))),
                "{sql}: {plan:?}"
            );
        }
    }

    #[test]
    fn message_ids_are_not_reused_after_pruning() {
        let db = Database::open_in_memory().unwrap();
        let me = register(&db, "s1", None).unwrap();
        let first = db.mail_send(&user(), me.id, "one", None).unwrap();
        db.conn
            .execute("DELETE FROM messages WHERE id = ?1", params![first.id])
            .unwrap();
        let second = db.mail_send(&user(), me.id, "two", None).unwrap();
        assert!(second.id > first.id);
    }

    #[test]
    fn history_holds_sent_and_received_mail() {
        let db = Database::open_in_memory().unwrap();
        let a = register(&db, "s1", Some("a")).unwrap();
        let b = register(&db, "s2", Some("b")).unwrap();
        let first = db.mail_send(&from(&a), b.id, "hi b", None).unwrap();
        db.mail_send(&from(&b), a.id, "hi a", Some(first.id))
            .unwrap();
        let history = db.mail_history(&a, 10).unwrap();
        let bodies: Vec<_> = history.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["hi a", "hi b"]);
        assert_eq!(history[0].reply_to, Some(first.id));
        assert_eq!(history[1].recipient, "b");
        assert_eq!(history[1].sender_session.as_deref(), Some("s1"));
    }

    #[test]
    fn history_holds_mail_sent_before_registering() {
        let db = Database::open_in_memory().unwrap();
        let other = register(&db, "s1", Some("other")).unwrap();
        let early = Sender {
            label: "s2",
            session_id: Some("s2"),
            agent_id: None,
        };
        db.mail_send(&early, other.id, "sent unregistered", None)
            .unwrap();
        let me = register(&db, "s2", None).unwrap();
        let history = db.mail_history(&me, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sender_id, None);
    }

    #[test]
    fn pruning_drops_old_read_mail_and_long_gone_agents() {
        let db = Database::open_in_memory().unwrap();
        let keep = register(&db, "s1", None).unwrap();
        let gone = register(&db, "s2", None).unwrap();
        let old = db.mail_send(&user(), keep.id, "old", None).unwrap();
        db.mail_send(&user(), keep.id, "unread", None).unwrap();
        db.conn
            .execute(
                "UPDATE messages SET read_at = datetime('now', '-30 days') WHERE id = ?1",
                params![old.id],
            )
            .unwrap();
        age(&db, gone.id, "-30 days");

        db.mail_prune(14).unwrap();
        assert!(db.mail_get(old.id).unwrap().is_none());
        assert_eq!(db.agent_list(true, WINDOW).unwrap().len(), 1);
        assert_eq!(
            db.agent_by_session("s1", WINDOW).unwrap().unwrap().unread,
            1
        );
    }
}
