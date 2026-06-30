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

    /// Background distillation worker spawned by `hook stop`.
    ///
    /// Not intended to be called directly; `hook stop` spawns this detached
    /// so the Stop hook itself returns in under a second.
    DistillSession {
        /// Claude Code session ID
        #[arg(long)]
        session_id: String,
        /// AI CLI binary (default: claude or $OL_AI_BIN)
        #[arg(long)]
        via: Option<String>,
        /// Model to pass to the AI binary
        #[arg(long)]
        model: Option<String>,
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
        HookCommand::DistillSession {
            session_id,
            via,
            model,
        } => run_distill_session(db, &session_id, via.as_deref(), model.as_deref()),
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

    // Index the repo if we're in one (silent on failure).
    if cwd.join(".git").exists() {
        let description = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n} repo"));
        let mut indexer = crate::index::RepoIndexer::new(None);
        let _ = indexer.index_repo(db, &cwd, description.as_deref());
    }

    // Emit compact session context for Claude Code to inject (layer-1 index only).
    // Hard-capped at ~750 tokens so the agent knows what's available without
    // loading full content. It then fetches detail via `ol memory get`, etc.
    if let Ok(ctx) = build_session_context(db) {
        if !ctx.is_empty() {
            let payload = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": ctx,
                }
            });
            println!("{payload}");
        }
    }

    Ok(())
}

/// Maximum characters in the injected session context. ~3000 chars ≈ 750 tokens.
const SESSION_CONTEXT_MAX_CHARS: usize = 3000;

/// Build a compact index of what's in the knowledge base so the agent knows
/// what's available without loading every memory. The agent fetches detail
/// on demand with `ol memory get`, `ol todo get`, etc.
fn build_session_context(db: &Database) -> Result<String> {
    let mut out = String::new();
    out.push_str("## ol context (run `ol memory get <key>` etc. for full content)\n\n");

    let budget = SESSION_CONTEXT_MAX_CHARS;

    // 1. Open todos (deadline-sorted, top 5). Highest priority — actionable now.
    if let Ok(todos) = db.todo_list(Some("open")) {
        if !todos.is_empty() {
            let mut section = format!("### Open todos ({})\n", todos.len());
            for t in todos.iter().take(5) {
                let due = t
                    .deadline_date
                    .as_deref()
                    .map(|d| format!(" (due {d})"))
                    .unwrap_or_default();
                section.push_str(&format!("- #{} {}{}\n", t.id, t.title, due));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 2. Open research investigations (top 3).
    if let Ok(invs) = db.investigation_list(Some("open")) {
        if !invs.is_empty() {
            let mut section = format!("### Open research ({})\n", invs.len());
            for inv in invs.iter().take(3) {
                section.push_str(&format!("- #{} {} [{}]\n", inv.id, inv.name, inv.slug));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 3. Recent session memories (last 3) — chronological brief.
    if let Ok(sessions) = db.memory_list(Some("session"), Some("active")) {
        if !sessions.is_empty() {
            let mut section = String::from("### Recent sessions\n");
            for m in sessions.iter().take(3) {
                let summary = truncate_for_index(&m.value, 100);
                section.push_str(&format!("- {}: {}\n", m.key, summary));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 4. Other active memory keys (project/feedback/general) — keys only, no content.
    // The agent fetches full content via `ol memory get <key>` when relevant.
    if let Ok(all) = db.memory_list(None, Some("active")) {
        let others: Vec<_> = all.iter().filter(|m| m.memory_type != "session").collect();
        if !others.is_empty() {
            let mut section = format!("### Memory keys ({})\n", others.len());
            for m in others.iter().take(20) {
                let hint = truncate_for_index(&m.value, 60);
                section.push_str(&format!("- {} — {}\n", m.key, hint));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // Final safety cap — should already fit but guard against runaway growth.
    if out.len() > budget {
        out.truncate(budget);
        out.push_str("\n…[truncated]");
    }

    Ok(out)
}

/// Append `section` to `out` only if doing so keeps `out` under `budget` chars.
fn push_if_fits(out: &mut String, section: &str, budget: usize) {
    if out.len() + section.len() <= budget {
        out.push_str(section);
    }
}

/// Truncate a value string to a single short line for use in the index.
fn truncate_for_index(s: &str, max: usize) -> String {
    let single_line = s.lines().next().unwrap_or("").trim();
    if single_line.chars().count() <= max {
        single_line.to_string()
    } else {
        let cut: String = single_line.chars().take(max).collect();
        format!("{cut}...")
    }
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

    // Distillation calls `claude -p` N times and can take 2-5 minutes for long
    // sessions. Spawn it as a fully detached background process so the Stop hook
    // returns immediately and Claude Code is unblocked.
    let session_id = hook_data["session_id"].as_str().unwrap_or_default();
    if session_id.is_empty() {
        return Ok(());
    }

    // Quick pre-check: is the transcript even worth distilling?
    let transcript = find_transcript(&cwd, session_id);
    let Some(transcript_path) = transcript else {
        return Ok(());
    };
    let turn_count = count_turns(&transcript_path).unwrap_or(0);
    if turn_count < 5 {
        return Ok(());
    }

    // Skip if we've already distilled this session at this turn count.
    // Prevents repeated distillation when claude -p sub-sessions fire
    // their own Stop hooks, and avoids redundant work on re-runs.
    // Re-distills only if the session has grown by at least 50 turns
    // since last time (picking up genuinely new content).
    let marker = distill_marker_path(session_id);
    let last_distilled = marker
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    const MIN_NEW_TURNS: usize = 50;
    if turn_count <= last_distilled + MIN_NEW_TURNS {
        log::debug!(
            "session {} already distilled at {} turns (now {}), skipping",
            session_id,
            last_distilled,
            turn_count
        );
        return Ok(());
    }

    // Resolve the current binary path for the background spawn
    let Ok(current_exe) = std::env::current_exe() else {
        return Ok(());
    };

    // Build args for `ol hook distill-session --session-id <id> [--via <bin>] [--model <m>]`
    let mut bg_args = vec![
        "hook".to_string(),
        "distill-session".to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
    ];
    if let Some(v) = via {
        bg_args.push("--via".to_string());
        bg_args.push(v.to_string());
    }
    if let Some(m) = model {
        bg_args.push("--model".to_string());
        bg_args.push(m.to_string());
    }

    // Inherit OL_DB and OL_HOOK_RUNNING so the background process uses the
    // same database and does not trigger further distillation recursively.
    let _ = std::process::Command::new(&current_exe)
        .args(&bg_args)
        .env("OL_HOOK_RUNNING", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    // Write the marker immediately so concurrent Stop hooks spawned by
    // the background claude -p calls don't start a second distillation.
    if let Some(p) = &marker {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, turn_count.to_string());
    }

    log::debug!(
        "spawned background distillation for session {} ({} turns, last {})",
        session_id,
        turn_count,
        last_distilled
    );

    Ok(())
}

/// Background distillation: reads transcript and calls AI in chunks.
/// Spawned detached by run_stop so the Stop hook itself returns quickly.
fn run_distill_session(
    db: &Database,
    session_id: &str,
    via: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let transcript = find_transcript(&cwd, session_id);
    let Some(transcript_path) = transcript else {
        return Ok(());
    };

    let turns = match extract_conversation_turns(&transcript_path) {
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
        .unwrap_or("claude");

    let env_model = std::env::var("OL_AI_MODEL").unwrap_or_default();
    let resolved_model = model.or(if env_model.is_empty() {
        None
    } else {
        Some(env_model.as_str())
    });

    log::debug!(
        "background: distilling {} turns from {}",
        turns.len(),
        session_id
    );
    let total = distill_chunked(db, bin, resolved_model, &turns, "session")?;
    if total > 0 {
        log::info!("background: distilled {total} memories from session {session_id}");
    }

    // Write marker so this session isn't re-distilled unnecessarily.
    if let Some(p) = distill_marker_path(session_id) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, turns.len().to_string());
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

/// Path to the per-session distillation marker: ~/.ol/distilled/<session-id>
/// Contains the turn count at which we last distilled this session.
fn distill_marker_path(session_id: &str) -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".ol")
            .join("distilled")
            .join(session_id),
    )
}

fn find_transcript(cwd: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let filename = format!("{session_id}.jsonl");

    // Fast path: try the hash derived from cwd first.
    let hash = path_to_project_hash(cwd);
    let candidate = home
        .join(".claude/projects")
        .join(&hash)
        .join(&filename);
    if candidate.exists() {
        return Some(candidate);
    }

    // Fallback: search all project directories. Required when distill-session
    // is invoked from a different cwd than the original session (e.g. manually).
    let projects_dir = home.join(".claude/projects");
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let path = entry.path().join(&filename);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

/// Each chunk sent to the AI for distillation.
/// Keeping chunks small prevents memory spikes from large prompts.
const CHUNK_CHARS: usize = 20_000;

/// Count the number of user/assistant turns in a transcript without reading it fully.
/// Used for a quick gate before spawning the background distillation process.
fn count_turns(path: &std::path::Path) -> Result<usize> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.contains(r#""type":"user""#) || line.contains(r#""type":"assistant""#) {
            count += 1;
        }
    }
    Ok(count)
}

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
/// Strips `<private>...</private>` regions before returning so secrets in the
/// transcript never reach the AI distiller.
fn extract_text_content(content: &serde_json::Value) -> Option<String> {
    let raw = match content {
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
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }?;
    let cleaned = strip_private_sections(&raw);
    (!cleaned.trim().is_empty()).then_some(cleaned)
}

/// Remove any text inside `<private>...</private>` markers (case-insensitive,
/// spans newlines, supports multiple regions per string).
fn strip_private_sections(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let lower = rest.to_lowercase();
        let Some(open) = lower.find("<private>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let after_open = open + "<private>".len();
        match lower[after_open..].find("</private>") {
            Some(close_rel) => {
                let close_abs = after_open + close_rel + "</private>".len();
                rest = &rest[close_abs..];
            }
            None => break, // unclosed tag: drop everything from here on
        }
    }
    out
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
    fn test_strip_private_basic() {
        let input = "before <private>secret</private> after";
        assert_eq!(strip_private_sections(input), "before  after");
    }

    #[test]
    fn test_strip_private_multiline() {
        let input = "line1\n<private>\nAWS_KEY=abc\n</private>\nline2";
        assert_eq!(strip_private_sections(input), "line1\n\nline2");
    }

    #[test]
    fn test_strip_private_multiple_regions() {
        let input = "a <private>x</private> b <PRIVATE>y</PRIVATE> c";
        assert_eq!(strip_private_sections(input), "a  b  c");
    }

    #[test]
    fn test_strip_private_unclosed_drops_to_end() {
        let input = "before <private>oh no";
        assert_eq!(strip_private_sections(input), "before ");
    }

    #[test]
    fn test_strip_private_no_tags_passthrough() {
        let input = "no secrets here";
        assert_eq!(strip_private_sections(input), "no secrets here");
    }

    #[test]
    fn test_extract_text_content_strips_private() {
        let val = serde_json::json!("hello <private>secret</private> world");
        assert_eq!(extract_text_content(&val), Some("hello  world".to_string()));
    }

    #[test]
    fn test_extract_text_content_only_private_returns_none() {
        let val = serde_json::json!("<private>everything</private>");
        assert_eq!(extract_text_content(&val), None);
    }

    #[test]
    fn test_truncate_for_index_short_passthrough() {
        assert_eq!(truncate_for_index("hello", 50), "hello");
    }

    #[test]
    fn test_truncate_for_index_long_truncated() {
        let result = truncate_for_index("a".repeat(200).as_str(), 50);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 53); // 50 + "..."
    }

    #[test]
    fn test_truncate_for_index_only_first_line() {
        assert_eq!(truncate_for_index("first\nsecond", 50), "first");
    }

    #[test]
    fn test_build_session_context_caps_at_budget() {
        let db = Database::open_in_memory().unwrap();
        // Add way more memory than the budget allows
        for i in 0..200 {
            let _ = db.memory_set(
                &format!("project/test/{i}"),
                &"x".repeat(300),
                "project",
                None,
            );
        }
        let ctx = build_session_context(&db).unwrap();
        assert!(
            ctx.len() <= SESSION_CONTEXT_MAX_CHARS,
            "context exceeded budget: {} > {}",
            ctx.len(),
            SESSION_CONTEXT_MAX_CHARS
        );
    }

    #[test]
    fn test_build_session_context_empty_db_has_header() {
        let db = Database::open_in_memory().unwrap();
        let ctx = build_session_context(&db).unwrap();
        assert!(ctx.contains("ol context"));
    }

    #[test]
    fn test_build_session_context_includes_open_todos() {
        use crate::db::todos::TodoStatus;
        let db = Database::open_in_memory().unwrap();
        let _ = db
            .todo_add(
                "fix login bug",
                None,
                TodoStatus::Open,
                None,
                "general",
                None,
                None,
            )
            .unwrap();
        let ctx = build_session_context(&db).unwrap();
        assert!(ctx.contains("Open todos"));
        assert!(ctx.contains("fix login bug"));
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
