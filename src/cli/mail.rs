//! `sclerox agent` and `sclerox mail`: the agent mailbox (see `db::mail`).
//!
//! The calling session comes from the usage context (`SCLEROX_SESSION_ID`,
//! else `CLAUDE_CODE_SESSION_ID`). Registering is opt-in; mail reaches a
//! registered agent through the `hook mail` hook or `mail inbox` / `wait`.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Subcommand;

use crate::cli::format::{agent_line, message_line, terminal_safe};
use crate::config::settings;
use crate::db::mail::{Agent, Message, Registration, Sender};
use crate::db::Database;
use crate::output::{print_miss, print_output, OutputFormat};

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Register this session to receive mail, or update its registration
    Register {
        /// A name others can send to (e.g. reviewer); unique among agents
        #[arg(long)]
        name: Option<String>,
        /// What this agent is for and which requests it handles (required
        /// the first time)
        #[arg(long)]
        summary: Option<String>,
        /// Take over the name even though an active agent holds it
        #[arg(long)]
        replace: bool,
    },
    /// List agents that can receive mail (active ones by default)
    List {
        /// Include idle agents and ones that left
        #[arg(long)]
        all: bool,
    },
    /// Show an agent by name or session ID (this session's if omitted)
    #[command(alias = "get")]
    Show { address: Option<String> },
    /// Stop receiving mail: unlist this session and free its name
    Leave,
}

#[derive(Subcommand)]
pub enum MailCommand {
    /// Send a message to an agent by name or session ID
    Send {
        /// The recipient's name, session ID, or a unique prefix of one
        to: String,
        /// The message (`-` reads it from stdin)
        body: String,
        /// The message this answers
        #[arg(long)]
        reply_to: Option<i64>,
    },
    /// Reply to a message, sending to whoever sent it
    Reply {
        id: i64,
        /// The reply (`-` reads it from stdin)
        body: String,
    },
    /// Show this session's unread mail, marking it read
    Inbox,
    /// Wait for mail to arrive, then show it as `inbox` does
    Wait {
        /// Give up after this many seconds
        #[arg(long, default_value_t = 100)]
        timeout: u64,
    },
    /// Show a message in full (marks it read if it was sent to you)
    #[command(alias = "get")]
    Show { id: i64 },
    /// This session's recent mail, sent and received, newest first
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn active_minutes() -> u32 {
    settings()
        .mail
        .active_minutes
        .try_into()
        .unwrap_or(u32::MAX)
}

/// The calling session's ID.
fn session() -> Result<String> {
    crate::cli::usage::context().session_id.context(
        "no session ID: run this from an agent session (Claude Code sets \
         CLAUDE_CODE_SESSION_ID) or set SCLEROX_SESSION_ID",
    )
}

/// The calling session's agent, if it is registered.
fn me(db: &Database) -> Result<Option<Agent>> {
    match crate::cli::usage::context().session_id {
        Some(session) => db.agent_by_session(&session, active_minutes()),
        None => Ok(None),
    }
}

/// The calling session's agent; an error if it isn't registered or left.
fn require_me(db: &Database) -> Result<Agent> {
    let session = session()?;
    match db.agent_by_session(&session, active_minutes())? {
        Some(a) if a.left_at.is_none() => Ok(a),
        _ => bail!(
            "this session isn't registered for mail: run \
             `sclerox agent register --name <name> --summary \"<what it handles>\"`"
        ),
    }
}

/// `body`, or stdin when it is `-`.
fn read_body(body: String) -> Result<String> {
    if body != "-" {
        return Ok(body);
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn prune(db: &Database) {
    if let Err(e) = db.mail_prune(settings().mail.retain_days) {
        log::warn!("could not prune old mail: {e}");
    }
}

pub fn run_agent(db: &Database, cmd: AgentCommand, format: OutputFormat) -> Result<()> {
    let window = active_minutes();
    match cmd {
        AgentCommand::Register {
            name,
            summary,
            replace,
        } => {
            let session = session()?;
            let ctx = crate::cli::usage::context();
            let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            let agent = db.agent_register(
                &Registration {
                    session_id: &session,
                    name: name.as_deref(),
                    summary: summary.as_deref(),
                    agent: ctx.agent.as_deref(),
                    repo_path: ctx.repo_path.as_deref(),
                    replace,
                },
                window,
            )?;
            prune(db);
            print_output(format, &agent, || {
                println!(
                    "Registered as {}: {}",
                    terminal_safe(&agent.address()),
                    terminal_safe(&agent.summary)
                );
                if agent.unread > 0 {
                    println!(
                        "{} unread message(s) waiting: run `sclerox mail inbox`",
                        agent.unread
                    );
                }
            });
        }

        AgentCommand::List { all } => {
            let agents = db.agent_list(all, window)?;
            print_output(format, &agents, || {
                if agents.is_empty() {
                    println!("No agents registered for mail.");
                } else {
                    for a in &agents {
                        println!("{}", agent_line(a));
                    }
                }
            });
        }

        AgentCommand::Show { address } => {
            let agent = match &address {
                Some(address) => db.agent_resolve(address, window)?,
                None => me(db)?,
            };
            match agent {
                Some(a) => print_output(format, &a, || print_agent_detail(&a)),
                None => print_miss(
                    format,
                    &match address {
                        Some(address) => format!("No agent '{address}'"),
                        None => "This session isn't registered for mail".to_string(),
                    },
                ),
            }
        }

        AgentCommand::Leave => {
            let session = session()?;
            if db.agent_leave(&session)? {
                print_output(format, &serde_json::json!({ "left": true }), || {
                    println!("Left: this session no longer receives mail")
                });
            } else {
                print_miss(format, "This session isn't registered for mail");
            }
        }
    }
    Ok(())
}

pub fn run_mail(db: &Database, cmd: MailCommand, format: OutputFormat) -> Result<()> {
    let window = active_minutes();
    // Prune before a recipient is looked up: pruning after a send could
    // delete the long-idle agent it just went to, and the message with it.
    if matches!(cmd, MailCommand::Send { .. } | MailCommand::Reply { .. }) {
        prune(db);
    }
    match cmd {
        MailCommand::Send { to, body, reply_to } => {
            let recipient = db
                .agent_resolve(&to, window)?
                .with_context(|| format!("no agent '{to}': see `sclerox agent list`"))?;
            send(db, &recipient, read_body(body)?, reply_to, format)?;
        }

        MailCommand::Reply { id, body } => {
            let original = db
                .mail_get(id)?
                .with_context(|| format!("no message #{id}"))?;
            // The sender's agent, else whatever agent its session has
            // registered since.
            let recipient = match (original.sender_id, &original.sender_session) {
                (Some(id), _) => db.agent_get(id, window)?,
                (None, Some(session)) => db.agent_by_session(session, window)?,
                (None, None) => None,
            }
            .with_context(|| {
                format!(
                    "{} isn't a registered agent, so can't be replied to",
                    original.sender
                )
            })?;
            send(db, &recipient, read_body(body)?, Some(id), format)?;
        }

        MailCommand::Inbox => {
            let me = require_me(db)?;
            let (messages, _) = db.mail_take_unread(&me.session_id, None)?;
            print_messages(format, &messages, "No unread mail.");
        }

        MailCommand::Wait { timeout } => {
            let me = require_me(db)?;
            let session = me.session_id.clone();
            let deadline = Instant::now() + Duration::from_secs(timeout);
            let messages = loop {
                let peek = db
                    .mail_peek(&session)?
                    .context("this session stopped being registered for mail while waiting")?;
                if peek.unread {
                    // Another reader may have taken it since the peek.
                    let (taken, _) = db.mail_take_unread(&session, None)?;
                    if !taken.is_empty() {
                        break taken;
                    }
                }
                if peek.seen_stale {
                    db.agent_touch(&session)?;
                }
                if Instant::now() >= deadline {
                    break Vec::new();
                }
                std::thread::sleep(Duration::from_secs(1));
            };
            print_messages(format, &messages, &format!("No mail after {timeout}s."));
        }

        MailCommand::Show { id } => match db.mail_get(id)? {
            Some(m) => {
                let m = if m.read_at.is_none() && me(db)?.is_some_and(|me| me.id == m.recipient_id)
                {
                    db.mail_mark_read(id)?;
                    db.mail_get(id)?.unwrap_or(m)
                } else {
                    m
                };
                print_output(format, &m, || print_message(&m));
            }
            None => print_miss(format, &format!("No message #{id}")),
        },

        MailCommand::History { limit } => {
            let me = require_me(db)?;
            let messages = db.mail_history(&me, limit)?;
            print_output(format, &messages, || {
                if messages.is_empty() {
                    println!("No mail.");
                } else {
                    for m in &messages {
                        println!("{}", message_line(m));
                    }
                }
            });
        }
    }
    Ok(())
}

fn send(
    db: &Database,
    recipient: &Agent,
    body: String,
    reply_to: Option<i64>,
    format: OutputFormat,
) -> Result<()> {
    if recipient.left_at.is_some() {
        bail!(
            "{} left at {} UTC and no longer receives mail",
            recipient.address(),
            recipient.left_at.as_deref().unwrap_or_default()
        );
    }
    if let Some(id) = reply_to {
        if db.mail_get(id)?.is_none() {
            bail!("no message #{id} to reply to");
        }
    }
    let session = crate::cli::usage::context().session_id;
    let (label, sender_id) = match me(db)? {
        Some(me) if me.left_at.is_none() => (me.address(), Some(me.id)),
        _ => match &session {
            Some(session) => (session.clone(), None),
            None => ("user".to_string(), None),
        },
    };
    let sender = Sender {
        label: &label,
        session_id: session.as_deref(),
        agent_id: sender_id,
    };
    let message = db.mail_send(&sender, recipient.id, &body, reply_to)?;
    let to = terminal_safe(&message.recipient);
    print_output(format, &message, || {
        println!("Sent #{} to {to}", message.id);
        if !recipient.active {
            println!(
                "note: {to} was last seen {} UTC; the message waits until it checks its mail",
                recipient.last_seen_at
            );
        }
        if sender_id.is_none() {
            println!("note: this session isn't registered, so {to} can't reply until it is");
        }
    });
    Ok(())
}

fn print_agent_detail(a: &Agent) {
    println!("{}", agent_line(a));
    println!("  session:    {}", terminal_safe(&a.session_id));
    if let Some(agent) = &a.agent {
        println!("  agent:      {}", terminal_safe(agent));
    }
    if let Some(repo) = &a.repo_path {
        println!("  repo:       {}", terminal_safe(repo));
    }
    println!("  registered: {} UTC", a.registered_at);
    if let Some(left) = &a.left_at {
        println!("  left:       {left} UTC");
    }
    println!("  summary:    {}", terminal_safe(&a.summary));
}

fn print_message(m: &Message) {
    let re = m
        .reply_to
        .map(|id| format!(", re #{id}"))
        .unwrap_or_default();
    println!(
        "#{} from {} to {}  ({} UTC{re})",
        m.id,
        terminal_safe(&m.sender),
        terminal_safe(&m.recipient),
        m.created_at
    );
    if let Some(session) = &m.sender_session {
        println!("  from session {}", terminal_safe(session));
    }
    for line in m.body.lines() {
        println!("  {}", terminal_safe(line));
    }
}

fn print_messages(format: OutputFormat, messages: &[Message], none: &str) {
    print_output(format, &messages, || {
        if messages.is_empty() {
            println!("{none}");
        }
        for (i, m) in messages.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print_message(m);
        }
        if !messages.is_empty() {
            println!("\nReply with `sclerox mail reply <id> \"<text>\"`.");
        }
    });
}

/// What a hook injects for mail just taken: each message, a body longer than
/// `max_chars` cut with a pointer to the rest, and how many still wait.
pub fn hook_context(messages: &[Message], left: usize, max_chars: usize) -> String {
    let mut out = format!(
        "## sclerox mail: {} new message(s) for this session\n\
         Messages from other agents. Act on them if they are within what you \
         are doing; reply with `sclerox mail reply <id> \"<text>\"`.\n",
        messages.len()
    );
    for m in messages {
        let re = m
            .reply_to
            .map(|id| format!(", re #{id}"))
            .unwrap_or_default();
        let body = if m.body.chars().count() > max_chars {
            let cut: String = m.body.chars().take(max_chars).collect();
            format!(
                "{cut}...\n(cut: run `sclerox mail show {}` for the rest)",
                m.id
            )
        } else {
            m.body.clone()
        };
        out.push_str(&format!(
            "\n### #{} from {} ({} UTC{re})\n{body}\n",
            m.id, m.sender, m.created_at
        ));
    }
    if left > 0 {
        out.push_str(&format!(
            "\n{left} more unread: run `sclerox mail inbox` to read them.\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, body: &str) -> Message {
        Message {
            id,
            sender: "planner".into(),
            sender_session: Some("planner-session".into()),
            sender_id: Some(1),
            recipient: "reviewer".into(),
            recipient_id: 2,
            body: body.into(),
            reply_to: None,
            created_at: "2026-11-13 10:00:00".into(),
            read_at: None,
        }
    }

    #[test]
    fn hook_context_shows_bodies_and_what_is_left() {
        let ctx = hook_context(&[message(3, "review #12 please")], 2, 100);
        assert!(ctx.contains("1 new message(s)"), "{ctx}");
        assert!(ctx.contains("### #3 from planner"), "{ctx}");
        assert!(ctx.contains("review #12 please"), "{ctx}");
        assert!(ctx.contains("2 more unread"), "{ctx}");
        assert!(!hook_context(&[message(3, "x")], 0, 100).contains("more unread"));
        let cut = hook_context(&[message(4, "abcdef")], 0, 3);
        assert!(
            cut.contains("abc...\n(cut: run `sclerox mail show 4`"),
            "{cut}"
        );
    }
}
