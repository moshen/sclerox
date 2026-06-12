use anyhow::Result;
use clap::Subcommand;
use std::io::Read;

use crate::db::Database;

#[derive(Subcommand)]
pub enum HookCommand {
    /// Run the Claude Code SessionStart hook: index repo if in a git directory.
    ///
    /// Only indexes if the current directory contains a .git folder.
    /// Fast because indexing is incremental (skips unchanged files).
    Start,

    /// Run the Claude Code Stop hook: index repo + distill session memories.
    ///
    /// Reads the hook JSON from stdin (Claude Code passes session metadata this way).
    /// Safe to call even outside a hook context - missing transcript is a no-op.
    Stop {
        /// AI CLI binary to use for memory distillation (default: claude, or $OL_AI_BIN)
        #[arg(long)]
        via: Option<String>,
        /// Model to pass to the AI binary (optional, uses agent default if omitted)
        #[arg(long)]
        model: Option<String>,
        /// Skip memory distillation - only index the repo
        #[arg(long)]
        no_distill: bool,
    },

    /// Run the OpenCode session.idle hook: index repo + distill session memories.
    ///
    /// Called by the ol-session OpenCode plugin with the session ID and directory.
    /// Reads conversation history from OpenCode's SQLite database.
    Opencode {
        /// OpenCode session ID (passed by the plugin)
        session_id: String,
        /// Project directory (passed by the plugin via ctx.directory)
        directory: Option<String>,
        /// AI CLI binary to use for memory distillation (default: opencode, or $OL_AI_BIN)
        #[arg(long)]
        via: Option<String>,
        /// Model to pass to the AI binary (optional, uses agent default if omitted)
        #[arg(long)]
        model: Option<String>,
        /// Skip memory distillation - only index the repo
        #[arg(long)]
        no_distill: bool,
    },
}

pub fn run(db: &Database, cmd: HookCommand) -> Result<()> {
    match cmd {
        HookCommand::Start => run_start(db),
        HookCommand::Stop {
            via,
            model,
            no_distill,
        } => run_stop(db, via.as_deref(), model.as_deref(), no_distill),
        HookCommand::Opencode {
            session_id,
            directory,
            via,
            model,
            no_distill,
        } => run_opencode(
            db,
            &session_id,
            directory.as_deref(),
            via.as_deref(),
            model.as_deref(),
            no_distill,
        ),
    }
}

fn run_start(db: &Database) -> Result<()> {
    // Consume stdin - SessionStart also sends JSON, ignore broken-pipe if not read.
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);

    let cwd = std::env::current_dir()?;

    // Only index if this is a git repository.
    if !cwd.join(".git").exists() {
        return Ok(());
    }

    let description = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("{n} repo"));

    let mut indexer = crate::index::RepoIndexer::new(None);
    let _ = indexer.index_repo(db, &cwd, description.as_deref());

    Ok(())
}

fn run_stop(db: &Database, via: Option<&str>, model: Option<&str>, no_distill: bool) -> Result<()> {
    // Guard against re-entrant calls: if `claude -p` (used for distillation)
    // also fires the Stop hook we'd recurse infinitely. The env var is inherited
    // by child processes, so the inner `claude -p` session sees it and skips.
    if std::env::var("OL_HOOK_RUNNING").is_ok() {
        return Ok(());
    }
    // Safety: set before any subprocess is spawned; this process exits after
    // the hook returns so there's no need to unset it.
    unsafe { std::env::set_var("OL_HOOK_RUNNING", "1") };

    // Always read stdin - Claude Code sends hook JSON here.
    // Consuming it prevents broken-pipe errors even if we don't use it all.
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);

    let hook_data: serde_json::Value =
        serde_json::from_str(&stdin_buf).unwrap_or(serde_json::json!({}));

    // Index the current repo (existing behaviour, silent on failure)
    let cwd = std::env::current_dir()?;
    {
        let mut indexer = crate::index::RepoIndexer::new(None);
        let description = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n} repo"));
        let _ = indexer.index_repo(db, &cwd, description.as_deref());
    }

    if no_distill {
        return Ok(());
    }

    // Try to distill memories from the session transcript
    let session_id = hook_data["session_id"].as_str().unwrap_or_default();
    if session_id.is_empty() {
        return Ok(()); // not in a hook context, or old Claude Code version
    }

    let transcript = find_transcript(&cwd, session_id);
    let Some(transcript_path) = transcript else {
        return Ok(());
    };

    // Extract conversation turns from the JSONL transcript
    let turns = match extract_conversation_turns(&transcript_path) {
        Ok(t) if t.is_empty() => return Ok(()),
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    if turns.len() < 5 {
        return Ok(());
    }

    // Resolve AI binary
    let env_bin = std::env::var("OL_AI_BIN").unwrap_or_default();
    let bin = via
        .or(if env_bin.is_empty() {
            None
        } else {
            Some(env_bin.as_str())
        })
        .unwrap_or("claude");

    let env_model = std::env::var("OL_AI_MODEL").unwrap_or_default();
    let resolved_model = model.or(if env_model.is_empty() {
        None
    } else {
        Some(env_model.as_str())
    });

    let total = distill_chunked(db, bin, resolved_model, &turns, "session")?;
    if total > 0 {
        eprintln!("[ol] distilled {total} memories from session");
    }

    Ok(())
}

fn run_opencode(
    db: &Database,
    session_id: &str,
    directory: Option<&str>,
    via: Option<&str>,
    model: Option<&str>,
    no_distill: bool,
) -> Result<()> {
    if std::env::var("OL_HOOK_RUNNING").is_ok() {
        return Ok(());
    }
    unsafe { std::env::set_var("OL_HOOK_RUNNING", "1") };

    // Index the repo if directory is a git repo
    let dir = directory
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    if dir.join(".git").exists() {
        let mut indexer = crate::index::RepoIndexer::new(None);
        let description = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n} repo"));
        let _ = indexer.index_repo(db, &dir, description.as_deref());
    }

    if no_distill {
        return Ok(());
    }

    let turns = match extract_opencode_turns(session_id) {
        Ok(t) if t.is_empty() => return Ok(()),
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    if turns.len() < 5 {
        return Ok(());
    }

    let env_bin = std::env::var("OL_AI_BIN").unwrap_or_default();
    let bin = via
        .or(if env_bin.is_empty() {
            None
        } else {
            Some(env_bin.as_str())
        })
        .unwrap_or("opencode");

    let env_model = std::env::var("OL_AI_MODEL").unwrap_or_default();
    let resolved_model = model.or(if env_model.is_empty() {
        None
    } else {
        Some(env_model.as_str())
    });

    let total = distill_chunked(db, bin, resolved_model, &turns, "session")?;
    if total > 0 {
        eprintln!("[ol] distilled {total} memories from opencode session");
    }

    Ok(())
}

/// Extract conversation turns from OpenCode's SQLite database for a given session.
fn extract_opencode_turns(session_id: &str) -> Result<Vec<String>> {
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?
        .join(".local/share/opencode/opencode.db");

    if !db_path.exists() {
        return Err(anyhow::anyhow!("opencode db not found"));
    }

    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let mut stmt = conn.prepare(
        "SELECT json_extract(m.data, '$.role') as role,
                json_extract(p.data, '$.text') as text
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1
           AND json_extract(p.data, '$.type') = 'text'
           AND json_extract(m.data, '$.role') IN ('user', 'assistant')
         ORDER BY m.time_created, p.time_created",
    )?;

    let rows = stmt.query_map([session_id], |row| {
        let role: String = row.get(0)?;
        let content: String = row.get(1).unwrap_or_default();
        Ok((role, content))
    })?;

    let mut turns = Vec::new();
    for row in rows.flatten() {
        let (role, content) = row;
        if content.trim().is_empty() {
            continue;
        }
        let (label, limit) = match role.as_str() {
            "user" => ("User", 500usize),
            "assistant" => ("Assistant", 1000usize),
            _ => continue,
        };
        turns.push(format!("{label}: {}\n", truncate_content(&content, limit)));
    }

    Ok(turns)
}

/// Derive the Claude Code project hash from a working directory path.
/// Claude Code replaces every non-alphanumeric character (slash, dot, etc.)
/// with `-`, so `/Users/colin.kennedy/code/proj` → `-Users-colin-kennedy-code-proj`.
fn path_to_project_hash(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn find_transcript(cwd: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let hash = path_to_project_hash(cwd);
    let transcript = home
        .join(".claude/projects")
        .join(&hash)
        .join(format!("{session_id}.jsonl"));

    if transcript.exists() {
        Some(transcript)
    } else {
        None
    }
}

/// Each chunk sent to the AI for distillation.
/// Keeping chunks small prevents memory spikes from large prompts.
const CHUNK_CHARS: usize = 20_000;

/// Distill a full list of turns in chunks, deduplicating by key.
/// Returns total number of memories stored.
fn distill_chunked(
    db: &Database,
    bin: &str,
    model: Option<&str>,
    turns: &[String],
    source: &str,
) -> Result<usize> {
    let mut stored = 0usize;
    let mut seen_keys = std::collections::HashSet::new();

    for chunk in chunk_turns(turns, CHUNK_CHARS) {
        let Ok(memories) = crate::cli::memory::distill_with_ai_pub(bin, model, &chunk) else {
            continue;
        };
        for m in &memories {
            if seen_keys.insert(m.key.clone()) {
                let _ = db.memory_set_full(&m.key, &m.value, &m.memory_type, None, source);
                stored += 1;
            }
        }
    }

    Ok(stored)
}

/// Split turns into chunks of up to `max_chars` each.
fn chunk_turns(turns: &[String], max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for turn in turns {
        if !current.is_empty() && current.len() + turn.len() > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(turn);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Extract conversation turns from a Claude Code session JSONL.
fn extract_conversation_turns(path: &std::path::Path) -> Result<Vec<String>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut turns = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        match obj["type"].as_str() {
            Some("user") => {
                if let Some(content) = extract_text_content(&obj["message"]["content"]) {
                    if !content.trim().is_empty() {
                        turns.push(format!("User: {}\n", truncate_content(&content, 500)));
                    }
                }
            }
            Some("assistant") => {
                if let Some(content) = extract_text_content(&obj["message"]["content"]) {
                    if !content.trim().is_empty() {
                        turns.push(format!("Assistant: {}\n", truncate_content(&content, 1000)));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(turns)
}

/// Extract plain text from a Claude message content field (string or array of blocks).
fn extract_text_content(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text: String = blocks
                .iter()
                .filter_map(|b| {
                    if b["type"].as_str() == Some("text") {
                        b["text"].as_str().map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

fn truncate_content(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_project_hash() {
        // Plain path
        assert_eq!(
            path_to_project_hash(std::path::Path::new("/Users/colin/code/myproject")),
            "-Users-colin-code-myproject"
        );
        // Dots in username are also replaced
        assert_eq!(
            path_to_project_hash(std::path::Path::new("/Users/colin.kennedy/code/my-project")),
            "-Users-colin-kennedy-code-my-project"
        );
    }

    #[test]
    fn test_extract_text_content_string() {
        let val = serde_json::json!("hello world");
        assert_eq!(extract_text_content(&val), Some("hello world".to_string()));
    }

    #[test]
    fn test_extract_text_content_blocks() {
        let val = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "x"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(extract_text_content(&val), Some("hello world".to_string()));
    }

    #[test]
    fn test_extract_text_content_empty_blocks() {
        let val = serde_json::json!([{"type": "tool_use", "id": "x"}]);
        assert_eq!(extract_text_content(&val), None);
    }

    #[test]
    fn test_chunk_turns_splits_at_limit() {
        let turns: Vec<String> = (0..10)
            .map(|i| "x".repeat(6_000) + &format!(" turn{i}\n"))
            .collect();
        let chunks = chunk_turns(&turns, 20_000);
        // Each chunk should be at most CHUNK_CHARS
        for chunk in &chunks {
            assert!(chunk.len() <= 25_000, "chunk too large: {}", chunk.len());
        }
        // All turns should be covered
        let all: String = chunks.join("");
        for i in 0..10 {
            assert!(all.contains(&format!("turn{i}")));
        }
    }

    #[test]
    fn test_chunk_turns_single_chunk_if_small() {
        let turns = vec![
            "User: hello\n".to_string(),
            "Assistant: world\n".to_string(),
        ];
        let chunks = chunk_turns(&turns, 20_000);
        assert_eq!(chunks.len(), 1);
    }
}
