use anyhow::Result;
use clap::Subcommand;
use std::cmp::Ordering;
use std::io::Read;

use crate::db::Database;
use crate::index::find_git_root;

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
        /// Full AI command for distillation (default: built-in claude, or [ai].command / $SCLEROX_AI_COMMAND)
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
        /// Full AI command for distillation (default: built-in claude, or [ai].command / $SCLEROX_AI_COMMAND)
        #[arg(long)]
        via: Option<String>,
        /// Model to pass to the AI binary
        #[arg(long)]
        model: Option<String>,
    },

    /// Run the OpenCode session.idle hook: index repo + distill session memories.
    ///
    /// Called by the sclerox-session OpenCode plugin with the session ID and directory.
    /// Reads conversation history from OpenCode's SQLite database.
    Opencode {
        /// OpenCode session ID (passed by the plugin)
        session_id: String,
        /// Project directory (passed by the plugin via ctx.directory)
        directory: Option<String>,
        /// Full AI command for distillation (default: built-in opencode, or [ai].command / $SCLEROX_AI_COMMAND)
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
    // Walk up from cwd to find the git root so subdirectory sessions index the whole repo.
    let git_root = find_git_root(&cwd);
    let mut repo_name: Option<String> = None;
    if git_root.join(".git").exists() {
        repo_name = git_root
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        // Auto-index policy: "off" disables session-hook indexing entirely.
        if crate::config::settings().index.auto != "off" {
            let description = repo_name.as_ref().map(|n| format!("{n} repo"));
            let mut indexer = crate::index::RepoIndexer::new(None);
            let _ = indexer.index_repo(db, &git_root, description.as_deref());
        }
    }

    // Emit compact session context for Claude Code to inject (layer-1 index only).
    // Hard-capped at ~750 tokens so the agent knows what's available without
    // loading full content. It then fetches detail via `sclerox memory get`, etc.
    if let Ok(ctx) = build_session_context(db, repo_name.as_deref()) {
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

/// Build a compact index of what's in the knowledge base so the agent knows
/// what's available without loading every memory. The agent fetches detail
/// on demand with `sclerox memory get`, `sclerox todo get`, etc. Section sizes and the
/// overall budget come from `[session_context]` in config.
fn build_session_context(db: &Database, repo_name: Option<&str>) -> Result<String> {
    let cfg = &crate::config::settings().session_context;
    let mut out = String::new();
    out.push_str("## sclerox context (run `sclerox memory get <key>` etc. for full content)\n\n");

    // Budget is enforced in real tokens (MiniLM tokenizer); max_chars is only a
    // final byte backstop applied at the end.
    let budget = cfg.max_tokens;

    // 1. Open todos (deadline-sorted). Highest priority — actionable now.
    if let Ok(todos) = db.todo_list(Some("open")) {
        if !todos.is_empty() {
            let mut section = format!("### Open todos ({})\n", todos.len());
            for t in todos.iter().take(cfg.todos_shown) {
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
            for inv in invs.iter().take(cfg.research_shown) {
                section.push_str(&format!("- #{} {} [{}]\n", inv.id, inv.name, inv.slug));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 2b. Unresolved memory conflicts — near-duplicate clusters distillation
    // refused to auto-merge. One line each; they need a content-aware human
    // (or agent) decision, which is exactly what a session can provide.
    if let Ok(conflicts) = db.memory_conflicts() {
        if !conflicts.is_empty() {
            let mut section = format!(
                "### Memory conflicts ({}) — resolve with `sclerox memory conflicts`\n",
                conflicts.len()
            );
            for c in conflicts.iter().take(5) {
                section.push_str(&format!("- '{}' vs '{}'\n", c.memory.key, c.matched.key));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 3. Relevant knowledge — FULL values of the top memories, not just keys.
    // Priority: feedback (user corrections) > project, repo-matched first, then
    // most-recent as fallback. This is the section that actually gets read.
    let relevant = relevant_memories(db, repo_name, cfg.relevant_memories);
    let shown_keys: std::collections::HashSet<String> =
        relevant.iter().map(|m| m.key.clone()).collect();
    if !relevant.is_empty() {
        let mut section = String::from("### Relevant knowledge\n");
        for m in &relevant {
            let val = truncate_for_index(&m.value, 200);
            section.push_str(&format!("- [{}] {}: {}\n", m.memory_type, m.key, val));
        }
        section.push('\n');
        push_if_fits(&mut out, &section, budget);
    }

    // 4. Recent session memories (last 3) — chronological brief.
    if let Ok(sessions) = db.memory_list(Some("session"), Some("active")) {
        if !sessions.is_empty() {
            let mut section = String::from("### Recent sessions\n");
            for m in sessions.iter().take(cfg.sessions_shown) {
                let summary = truncate_for_index(&m.value, 100);
                section.push_str(&format!("- {}: {}\n", m.key, summary));
            }
            section.push('\n');
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 5. Remaining active memory keys (excluding session + already-shown) — a
    // single compact line of keys the agent can `sclerox memory get` on demand.
    if let Ok(all) = db.memory_list(None, Some("active")) {
        let others: Vec<&str> = all
            .iter()
            .filter(|m| m.memory_type != "session" && !shown_keys.contains(&m.key))
            .map(|m| m.key.as_str())
            .take(cfg.memory_keys_shown)
            .collect();
        if !others.is_empty() {
            let section = format!(
                "### More memory keys ({})\n{}\n\n",
                others.len(),
                others.join(", ")
            );
            push_if_fits(&mut out, &section, budget);
        }
    }

    // 6. Code index reminder — every session should know `sclerox code` exists and is
    // the preferred symbol search across indexed repos.
    if let Ok(repos) = db.repo_list() {
        if !repos.is_empty() {
            let section = format!(
                "### Code index\n{} repo(s) indexed. Prefer `sclerox code search <symbol>` \
                 / `sclerox code refs <symbol>` over Grep for symbol lookup.\n\n",
                repos.len()
            );
            push_if_fits(&mut out, &section, budget);
        }
    }

    // Final byte backstop — the token budget should already keep us well under
    // this, but guard against runaway growth. Leave room for the marker and cut
    // on a char boundary so the result stays within max_chars and a multibyte
    // sequence at the limit can't panic the session-start hook.
    const TRUNC_MARKER: &str = "\n…[truncated]";
    if out.len() > cfg.max_chars {
        let limit = cfg.max_chars.saturating_sub(TRUNC_MARKER.len());
        let cut = floor_char_boundary(&out, limit);
        out.truncate(cut);
        out.push_str(TRUNC_MARKER);
    }

    Ok(out)
}

/// Collect up to `limit` distinct active memories to surface with full values.
/// A configurable number of feedback slots (user corrections) are guaranteed
/// when feedback exists. Remaining slots go to repo-name matches, then
/// most-recent feedback, then most-recent project — deduplicated by key.
fn relevant_memories(
    db: &Database,
    repo_name: Option<&str>,
    limit: usize,
) -> Vec<crate::db::memory::MemoryEntry> {
    // Slots reserved for feedback so corrections surface even when repo-matched
    // project facts would otherwise fill every slot.
    let feedback_reserved = crate::config::settings().session_context.feedback_reserved;
    let mut picked: Vec<crate::db::memory::MemoryEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let add = |m: crate::db::memory::MemoryEntry,
               picked: &mut Vec<_>,
               seen: &mut std::collections::HashSet<String>| {
        if picked.len() < limit && seen.insert(m.key.clone()) {
            picked.push(m);
        }
    };

    // Repo-matched feedback, ranked by recency, for both the repo name and its
    // whitespace-split form. Collected up front so the reserved slot prefers a
    // repo-relevant correction over a generic one.
    let repo_feedback: Vec<crate::db::memory::MemoryEntry> = repo_name
        .into_iter()
        .flat_map(|name| [name.to_string(), name.replace(['-', '_'], " ")])
        .filter_map(|token| db.memory_search(&token).ok())
        .flatten()
        .filter(|m| m.memory_type == "feedback")
        .collect();

    // Reserved feedback slot(s): repo-matched feedback first, else most-recent.
    for m in repo_feedback.iter().cloned() {
        if picked.len() >= feedback_reserved {
            break;
        }
        add(m, &mut picked, &mut seen);
    }
    if picked.len() < feedback_reserved {
        if let Ok(list) = db.memory_list(Some("feedback"), Some("active")) {
            for m in list {
                if picked.len() >= feedback_reserved {
                    break;
                }
                add(m, &mut picked, &mut seen);
            }
        }
    }

    // Tier 1: repo-name matches. Split on non-alphanumerics so "sclerox-cli"
    // also matches memories phrased with the individual words.
    if let Some(name) = repo_name {
        for token in [name.to_string(), name.replace(['-', '_'], " ")] {
            if let Ok(hits) = db.memory_search(&token) {
                let mut ordered = hits;
                ordered.sort_by_key(|m| m.memory_type != "feedback"); // feedback first
                for m in ordered {
                    if m.memory_type == "feedback" || m.memory_type == "project" {
                        add(m, &mut picked, &mut seen);
                    }
                }
            }
        }
    }

    // Tier 2/3: most-recent feedback, then most-recent project.
    for ty in ["feedback", "project"] {
        if picked.len() >= limit {
            break;
        }
        if let Ok(list) = db.memory_list(Some(ty), Some("active")) {
            for m in list {
                add(m, &mut picked, &mut seen);
            }
        }
    }

    picked
}

/// Append `section` to `out` only if doing so keeps the payload within
/// `budget_tokens` real tokens. Counts are additive across the join (a slight,
/// safe over-count); with only a handful of sections the re-count is cheap.
fn push_if_fits(out: &mut String, section: &str, budget_tokens: usize) {
    if crate::embed::count_tokens(out) + crate::embed::count_tokens(section) <= budget_tokens {
        out.push_str(section);
    }
}

/// Largest byte index `<= max` that lands on a UTF-8 char boundary (stable-Rust
/// stand-in for the unstable `str::floor_char_boundary`).
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
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
    if std::env::var("SCLEROX_HOOK_RUNNING").is_ok() {
        return Ok(());
    }
    // Safety: set before any subprocess is spawned; this process exits after
    // the hook returns so there's no need to unset it.
    unsafe { std::env::set_var("SCLEROX_HOOK_RUNNING", "1") };

    // Always read stdin - Claude Code sends hook JSON here.
    // Consuming it prevents broken-pipe errors even if we don't use it all.
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);

    let hook_data: serde_json::Value =
        serde_json::from_str(&stdin_buf).unwrap_or(serde_json::json!({}));

    // Index the current repo (existing behaviour, silent on failure).
    // Walk up from cwd to find the git root so subdirectory sessions index the whole repo.
    let cwd = std::env::current_dir()?;
    {
        // Auto-index policy: only the session's git repo root, and only when
        // "off" hasn't disabled it. Matches the SessionStart hook.
        let git_root = find_git_root(&cwd);
        if git_root.join(".git").exists() && crate::config::settings().index.auto != "off" {
            let mut indexer = crate::index::RepoIndexer::new(None);
            let description = git_root
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{n} repo"));
            let _ = indexer.index_repo(db, &git_root, description.as_deref());
        }
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
    let distill_cfg = &crate::config::settings().distill;
    let turn_count = count_turns(&transcript_path).unwrap_or(0);
    if turn_count < distill_cfg.min_turns {
        return Ok(());
    }

    // Skip if we've already distilled this session at this turn count.
    // Prevents repeated distillation when claude -p sub-sessions fire
    // their own Stop hooks, and avoids redundant work on re-runs.
    // Re-distills only after the session grows by distill.min_new_turns turns.
    let marker = distill_marker_path(session_id);
    let last_distilled = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if turn_count <= last_distilled + distill_cfg.min_new_turns {
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

    // Build args for `sclerox hook distill-session --session-id <id> [--via <bin>] [--model <m>]`
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

    // Inherit SCLEROX_DB and SCLEROX_HOOK_RUNNING so the background process uses the
    // same database and does not trigger further distillation recursively.
    let _ = std::process::Command::new(&current_exe)
        .args(&bg_args)
        .env("SCLEROX_HOOK_RUNNING", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    // Write the marker immediately so concurrent Stop hooks spawned by
    // the background claude -p calls don't start a second distillation.
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, turn_count.to_string());

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

    let cfg = crate::config::settings();
    if turns.len() < cfg.distill.min_turns {
        return Ok(());
    }

    // Precedence: --via/--model flag > [ai] (folds SCLEROX_AI_COMMAND/SCLEROX_AI_MODEL).
    // This is a Claude Stop-hook path, so the default command is claude's.
    let command = via.or(cfg.ai.command.as_deref());
    let resolved_model = model.or(cfg.ai.model.as_deref());
    let argv = crate::cli::memory::resolve_distill_command(command, "claude", resolved_model)?;

    // Hold a per-session lock so a manual `distill-session` can't race the
    // hook-spawned worker (or another manual run) and pay for the same
    // distillation twice. Held until this function returns.
    let _lock = match try_lock_session(session_id) {
        LockOutcome::Contended => {
            log::info!("session {session_id} is already being distilled, skipping");
            return Ok(());
        }
        LockOutcome::Acquired(guard) => Some(guard),
        LockOutcome::NoLock => None,
    };

    log::debug!(
        "background: distilling {} turns from {}",
        turns.len(),
        session_id
    );
    let total = distill_chunked(db, &argv, &turns, "session")?;
    // Always log the outcome — a run of zero-memory sessions is the signal
    // that distillation is broken, so it must be visible.
    log::info!("background: distilled {total} memories from session {session_id}");

    // Write marker so this session isn't re-distilled unnecessarily.
    let p = distill_marker_path(session_id);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, turns.len().to_string());

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
    if std::env::var("SCLEROX_HOOK_RUNNING").is_ok() {
        return Ok(());
    }
    unsafe { std::env::set_var("SCLEROX_HOOK_RUNNING", "1") };

    // Index the repo if directory is a git repo.
    // Walk up to the git root so sessions opened in subdirectories index the whole repo.
    let dir = directory
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let git_root = find_git_root(&dir);

    if git_root.join(".git").exists() && crate::config::settings().index.auto != "off" {
        let mut indexer = crate::index::RepoIndexer::new(None);
        let description = git_root
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n} repo"));
        let _ = indexer.index_repo(db, &git_root, description.as_deref());
    }

    if no_distill {
        return Ok(());
    }

    let turns = match extract_opencode_turns(session_id) {
        Ok(t) if t.is_empty() => return Ok(()),
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    let cfg = crate::config::settings();
    if turns.len() < cfg.distill.min_turns {
        return Ok(());
    }

    // Precedence: --via/--model flag > [ai] (folds SCLEROX_AI_COMMAND/SCLEROX_AI_MODEL).
    // This is the OpenCode hook, so the default command is opencode's — a bare
    // [ai].command (if the user set one) still overrides it.
    let command = via.or(cfg.ai.command.as_deref());
    let resolved_model = model.or(cfg.ai.model.as_deref());
    let argv = crate::cli::memory::resolve_distill_command(command, "opencode", resolved_model)?;

    // Per-session lock: don't distill the same session concurrently.
    let _lock = match try_lock_session(session_id) {
        LockOutcome::Contended => {
            log::info!("session {session_id} is already being distilled, skipping");
            return Ok(());
        }
        LockOutcome::Acquired(guard) => Some(guard),
        LockOutcome::NoLock => None,
    };

    let total = distill_chunked(db, &argv, &turns, "session")?;
    if total > 0 {
        eprintln!("[sclerox] distilled {total} memories from opencode session");
    }
    log::info!("opencode: distilled {total} memories from session {session_id}");

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

/// Path to the per-session distillation marker: `<state_home>/sclerox/distilled/<session-id>`
/// (`~/.local/state/sclerox/distilled/<session-id>` by default, on every
/// platform). Contains the turn count at which we last distilled this session.
fn distill_marker_path(session_id: &str) -> std::path::PathBuf {
    crate::xdg::state_home()
        .join("sclerox")
        .join("distilled")
        .join(session_id)
}

/// Locks older than this are treated as abandoned by a crashed process and
/// stolen — a distiller that died must not block the session forever. Set well
/// above any real distillation time (minutes).
const LOCK_STALE_SECS: u64 = 30 * 60;

/// RAII guard for a per-session distillation lock; removes the lockfile on drop.
struct SessionLock {
    path: std::path::PathBuf,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Outcome of trying to acquire the per-session distillation lock.
enum LockOutcome {
    /// Acquired — hold the guard for the duration of distillation.
    Acquired(SessionLock),
    /// Another live process is already distilling this session — skip.
    Contended,
    /// Locking infrastructure is unavailable — proceed WITHOUT a lock rather
    /// than skip (a broken lock dir must never stop distillation entirely).
    NoLock,
}

/// Acquire the per-session lock under `<state_home>/sclerox/distilled/<id>.lock`.
fn try_lock_session(session_id: &str) -> LockOutcome {
    let marker = distill_marker_path(session_id);
    match marker.parent() {
        Some(dir) => try_lock_session_in(dir, session_id, LOCK_STALE_SECS),
        None => LockOutcome::NoLock,
    }
}

/// Testable core: create an exclusive lockfile in `dir`. An existing lock older
/// than `stale_secs` is stolen.
fn try_lock_session_in(dir: &std::path::Path, session_id: &str, stale_secs: u64) -> LockOutcome {
    if std::fs::create_dir_all(dir).is_err() {
        return LockOutcome::NoLock;
    }
    let path = dir.join(format!("{session_id}.lock"));
    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                // PID is informational — the lock is the file's existence.
                let _ = write!(f, "{}", std::process::id());
                return LockOutcome::Acquired(SessionLock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path, stale_secs) {
                    let _ = std::fs::remove_file(&path);
                    continue; // retry the create
                }
                return LockOutcome::Contended;
            }
            // Any other error: don't block distillation on lock trouble.
            Err(_) => return LockOutcome::NoLock,
        }
    }
}

fn lock_is_stale(path: &std::path::Path, stale_secs: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs() >= stale_secs)
        .unwrap_or(false)
}

fn find_transcript(cwd: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let filename = format!("{session_id}.jsonl");

    // Fast path: try the hash derived from cwd first.
    let hash = path_to_project_hash(cwd);
    let candidate = home.join(".claude/projects").join(&hash).join(&filename);
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
///
/// Dedup happens at two levels: exact-key collisions upsert the ACTIVE row
/// (via `memory_set_full`'s partial-index ON CONFLICT), and near-duplicate
/// values supersede the existing entry so re-learning a fact under a new slug
/// doesn't create drift. Superseding is only automatic when it is safe:
/// exactly one match, and not a manually written memory (a human wrote it, so
/// a background job must not silently replace it). Several matches mean a
/// similarity score can't tell restated-same-fact from similar-distinct-facts
/// — the new memory is stored and the cluster is flagged in memory_conflicts
/// for content-aware review (`sclerox memory conflicts`).
fn distill_chunked(
    db: &Database,
    argv: &[String],
    turns: &[String],
    source: &str,
) -> Result<usize> {
    let dedup = &crate::config::settings().dedup;
    let chunk_chars = crate::config::settings().distill.chunk_chars;
    let ctx_limit = crate::config::settings().distill.context_memories;
    let mut stored = 0usize;
    let mut seen_keys = std::collections::HashSet::new();
    // Best-effort embedder, reused across all chunks. None if the model is
    // unavailable — dedup then falls back to lexical and no vectors are stored.
    let mut embedder = crate::embed::Embedder::new().ok();

    for chunk in chunk_turns(turns, chunk_chars) {
        // Show the distiller the memories this chunk is topically near, so it
        // can reuse an existing key instead of inventing a near-identical slug.
        // Without this it is asked to guess what a previous run named a fact it
        // cannot see, which is how one fact ends up under three keys.
        let context_entries = related_memories_for_chunk(db, embedder.as_mut(), &chunk, ctx_limit);
        let existing_keys: std::collections::HashSet<String> =
            context_entries.iter().map(|(k, _)| k.clone()).collect();
        let existing_block = crate::cli::memory::format_existing_block(&context_entries);

        // A failed chunk must be LOUD in the logs: swallowing it silently hid a
        // broken claude flag for 8 days of zero distillations.
        let memories = match crate::cli::memory::distill_with_ai_pub(argv, &chunk, &existing_block)
        {
            Ok(m) => m,
            Err(e) => {
                log::warn!("distill chunk failed (command: {}): {e:#}", argv.join(" "));
                continue;
            }
        };
        for m in &memories {
            if !seen_keys.insert(m.key.clone()) {
                continue;
            }

            // Embed the value once (if possible); reused for dedup and storage.
            let capped: String = m
                .value
                .chars()
                .take(crate::index::MAX_EMBED_CHARS)
                .collect();
            let emb = embedder.as_mut().and_then(|e| e.embed_one(&capped).ok());

            // Existing active memories this value closely matches, under other
            // keys. Prefer semantic (cosine) matching; fall back to lexical.
            const NEAR_DUP_LIMIT: usize = 8;
            let near_dups: Vec<_> = match &emb {
                Some(v) => db
                    .memory_find_near_duplicates_semantic(
                        v,
                        dedup.cosine_threshold as f32,
                        NEAR_DUP_LIMIT,
                    )
                    .unwrap_or_default(),
                None => db
                    .memory_find_near_duplicates(&m.value, dedup.lexical_threshold)
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|sm| sm.entry.key != m.key)
            .collect();

            // The distiller was shown a list of existing keys and may have
            // named one as the same fact. Only honour a key that was actually
            // on that list (never a hallucinated one) and never one the user
            // wrote by hand.
            let declared = m.supersedes.as_deref().filter(|k| {
                *k != m.key
                    && existing_keys.contains(*k)
                    && db
                        .memory_get(k)
                        .ok()
                        .flatten()
                        .is_some_and(|e| e.source != "manual")
            });

            let candidates: Vec<Candidate<'_>> = near_dups
                .iter()
                .map(|sm| Candidate {
                    key: &sm.entry.key,
                    source: &sm.entry.source,
                    score: sm.score,
                })
                .collect();

            match decide_dedup_action(declared, &candidates, dedup.merge_threshold) {
                DedupAction::Insert => {
                    let _ = db.memory_set_full(&m.key, &m.value, &m.memory_type, None, source);
                }
                DedupAction::Merge(old_key) => {
                    let _ = db.memory_supersede(&old_key, &m.key, &m.value, &m.memory_type, source);
                }
                DedupAction::Flag => {
                    // Genuinely ambiguous, or the only matches are hand-written:
                    // store and flag rather than guess which side should win.
                    if let Ok(new_id) =
                        db.memory_set_full(&m.key, &m.value, &m.memory_type, None, source)
                    {
                        for sm in &near_dups {
                            let _ =
                                db.memory_conflict_add(new_id, sm.entry.id, Some(sm.score as f64));
                        }
                        log::info!(
                            "memory '{}' near-duplicates {} existing entries; flagged in \
                             memory_conflicts for review",
                            m.key,
                            near_dups.len()
                        );
                    }
                }
            }
            // Persist the embedding for the (possibly new) key.
            if let Some(v) = &emb {
                let _ = db.memory_set_embedding(&m.key, v);
            }
            stored += 1;
        }
    }

    Ok(stored)
}

/// What to do with a freshly distilled memory, decided before any writes.
#[derive(Debug, PartialEq, Eq)]
enum DedupAction {
    /// No close match: store it as a new memory.
    Insert,
    /// Merge into this existing key, superseding it.
    Merge(String),
    /// Too ambiguous to merge: store it and flag every match for review.
    Flag,
}

/// One near-duplicate candidate, reduced to what the decision needs.
struct Candidate<'a> {
    key: &'a str,
    source: &'a str,
    score: f32,
}

/// Decide how a distilled memory relates to the existing memories it matched.
///
/// `declared` is the key the distiller named via `supersedes`, already validated
/// by the caller as an eligible target. It wins over the scores because the
/// model saw both values and judged them the same fact, whereas cosine only
/// says they are worded alike.
///
/// Hand-written memories are never merged into automatically: the user wrote
/// them deliberately, so a cluster whose only matches are manual is flagged.
///
/// The `many` arm is the important one. Flagging every multi-match ratchets a
/// cluster wider (once a topic has two entries, each later mention matches 2+
/// and adds a third), so a match at or above `merge_threshold` merges instead.
fn decide_dedup_action(
    declared: Option<&str>,
    candidates: &[Candidate<'_>],
    merge_threshold: f64,
) -> DedupAction {
    if let Some(key) = declared {
        return DedupAction::Merge(key.to_string());
    }
    match candidates {
        [] => DedupAction::Insert,
        [single] if single.source != "manual" => DedupAction::Merge(single.key.to_string()),
        many => many
            .iter()
            .filter(|c| c.source != "manual")
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal))
            .filter(|c| c.score as f64 >= merge_threshold)
            .map(|c| DedupAction::Merge(c.key.to_string()))
            .unwrap_or(DedupAction::Flag),
    }
}

/// Existing active memories topically near `chunk`, as (key, value) pairs for
/// the distiller's dedup context.
///
/// Uses a deliberately loose cosine floor: the point is to surface the keys
/// that already cover this topic so the model can reuse one, not to decide
/// anything. The strict thresholds still gate the actual writes.
fn related_memories_for_chunk(
    db: &Database,
    embedder: Option<&mut crate::embed::Embedder>,
    chunk: &str,
    limit: usize,
) -> Vec<(String, String)> {
    /// Below this the retrieved memories are noise rather than context.
    const CONTEXT_COSINE_FLOOR: f32 = 0.30;

    if limit == 0 {
        return Vec::new();
    }
    let Some(embedder) = embedder else {
        return Vec::new();
    };
    let capped: String = chunk.chars().take(crate::index::MAX_EMBED_CHARS).collect();
    let Ok(emb) = embedder.embed_one(&capped) else {
        return Vec::new();
    };
    db.memory_find_near_duplicates_semantic(&emb, CONTEXT_COSINE_FLOOR, limit)
        .unwrap_or_default()
        .into_iter()
        .map(|sm| (sm.entry.key, sm.entry.value))
        .collect()
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

    /// Point SCLEROX_CONFIG at a nonexistent path so `settings()` yields built-in
    /// defaults regardless of the developer's real ~/.config/sclerox/config.toml. All
    /// settings()-using tests call this; since settings() is a process-wide
    /// OnceLock, the first caller locks in defaults for the whole test binary.
    fn isolate_config() {
        // SAFETY: test-only; every caller writes the same value before the
        // OnceLock is first read.
        unsafe { std::env::set_var("SCLEROX_CONFIG", "/nonexistent/sclerox-test-config.toml") };
    }

    #[test]
    fn session_lock_is_exclusive_then_released() {
        let dir = tempfile::TempDir::new().unwrap();
        let held = match try_lock_session_in(dir.path(), "sess", LOCK_STALE_SECS) {
            LockOutcome::Acquired(g) => g,
            _ => panic!("expected to acquire the lock"),
        };
        // A second attempt on the same session while held is contended.
        assert!(matches!(
            try_lock_session_in(dir.path(), "sess", LOCK_STALE_SECS),
            LockOutcome::Contended
        ));
        // A different session is independent.
        assert!(matches!(
            try_lock_session_in(dir.path(), "other", LOCK_STALE_SECS),
            LockOutcome::Acquired(_)
        ));
        // Releasing the guard frees the lock.
        drop(held);
        assert!(matches!(
            try_lock_session_in(dir.path(), "sess", LOCK_STALE_SECS),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn stale_session_lock_is_stolen() {
        let dir = tempfile::TempDir::new().unwrap();
        // Simulate a crashed distiller: leak the guard so the lockfile persists.
        match try_lock_session_in(dir.path(), "sess", LOCK_STALE_SECS) {
            LockOutcome::Acquired(g) => std::mem::forget(g),
            _ => panic!("expected to acquire the lock"),
        };
        // A live process still sees it as contended...
        assert!(matches!(
            try_lock_session_in(dir.path(), "sess", LOCK_STALE_SECS),
            LockOutcome::Contended
        ));
        // ...but once older than the staleness window it is stolen.
        assert!(matches!(
            try_lock_session_in(dir.path(), "sess", 0),
            LockOutcome::Acquired(_)
        ));
    }

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

    fn cand<'a>(key: &'a str, source: &'a str, score: f32) -> Candidate<'a> {
        Candidate { key, source, score }
    }

    #[test]
    fn dedup_inserts_when_nothing_matches() {
        assert_eq!(decide_dedup_action(None, &[], 0.95), DedupAction::Insert);
    }

    #[test]
    fn dedup_merges_single_distilled_match() {
        let c = [cand("existing", "session", 0.86)];
        assert_eq!(
            decide_dedup_action(None, &c, 0.95),
            DedupAction::Merge("existing".to_string())
        );
    }

    #[test]
    fn dedup_never_auto_merges_a_single_manual_match() {
        // The user wrote it deliberately; a distilled near-match must not
        // silently replace it, however high the score.
        let c = [cand("hand-written", "manual", 0.99)];
        assert_eq!(decide_dedup_action(None, &c, 0.95), DedupAction::Flag);
    }

    #[test]
    fn dedup_merges_best_match_above_threshold_instead_of_ratcheting() {
        // This is the regression that grew the conflict list: several matches
        // used to mean "insert a third entry and flag", every time.
        let c = [
            cand("older", "session", 0.91),
            cand("closest", "session", 0.97),
        ];
        assert_eq!(
            decide_dedup_action(None, &c, 0.95),
            DedupAction::Merge("closest".to_string())
        );
    }

    #[test]
    fn dedup_flags_ambiguous_cluster_below_threshold() {
        let c = [cand("one", "session", 0.88), cand("two", "session", 0.90)];
        assert_eq!(decide_dedup_action(None, &c, 0.95), DedupAction::Flag);
    }

    #[test]
    fn dedup_skips_manual_when_picking_the_best_of_many() {
        // The manual entry scores highest but is not an eligible target, so the
        // best distilled match wins instead.
        let c = [
            cand("hand-written", "manual", 0.99),
            cand("distilled", "session", 0.96),
        ];
        assert_eq!(
            decide_dedup_action(None, &c, 0.95),
            DedupAction::Merge("distilled".to_string())
        );
    }

    #[test]
    fn dedup_flags_when_every_high_scorer_is_manual() {
        let c = [
            cand("hand-a", "manual", 0.99),
            cand("hand-b", "manual", 0.97),
        ];
        assert_eq!(decide_dedup_action(None, &c, 0.95), DedupAction::Flag);
    }

    #[test]
    fn dedup_declared_supersedes_wins_over_scores() {
        // The model saw both values and judged them the same fact; cosine only
        // says they are worded alike. Below-threshold matches must not override.
        let c = [cand("some-other", "session", 0.60)];
        assert_eq!(
            decide_dedup_action(Some("declared-key"), &c, 0.95),
            DedupAction::Merge("declared-key".to_string())
        );
    }

    #[test]
    fn dedup_declared_supersedes_applies_with_no_matches_at_all() {
        assert_eq!(
            decide_dedup_action(Some("declared-key"), &[], 0.95),
            DedupAction::Merge("declared-key".to_string())
        );
    }

    #[test]
    fn test_chunk_turns_splits_at_limit() {
        let turns: Vec<String> = (0..10)
            .map(|i| "x".repeat(6_000) + &format!(" turn{i}\n"))
            .collect();
        let chunks = chunk_turns(&turns, 20_000);
        // Each chunk should be at most the configured chunk size
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
        isolate_config();
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
        let ctx = build_session_context(&db, None).unwrap();
        let budget = crate::config::settings().session_context.max_chars;
        assert!(
            ctx.len() <= budget,
            "context exceeded budget: {} > {}",
            ctx.len(),
            budget
        );
    }

    #[test]
    fn test_build_session_context_empty_db_has_header() {
        isolate_config();
        let db = Database::open_in_memory().unwrap();
        let ctx = build_session_context(&db, None).unwrap();
        assert!(ctx.contains("sclerox context"));
    }

    #[test]
    fn test_build_session_context_injects_full_memory_values() {
        isolate_config();
        let db = Database::open_in_memory().unwrap();
        db.memory_set(
            "clippy-as-errors",
            "Fix ALL clippy warnings immediately; zero warnings is the bar",
            "feedback",
            None,
        )
        .unwrap();
        let ctx = build_session_context(&db, None).unwrap();
        // Full value present, not just the key.
        assert!(ctx.contains("Relevant knowledge"));
        assert!(ctx.contains("zero warnings is the bar"));
        assert!(ctx.len() <= crate::config::settings().session_context.max_chars);
    }

    #[test]
    fn test_relevant_memories_guarantees_feedback_slot() {
        isolate_config();
        let db = Database::open_in_memory().unwrap();
        // Fill many repo-matched project memories that would otherwise take all slots.
        for i in 0..10 {
            db.memory_set(
                &format!("myrepo-project-{i}"),
                &format!("myrepo project fact number {i}"),
                "project",
                None,
            )
            .unwrap();
        }
        // A single feedback memory that does NOT mention the repo.
        db.memory_set(
            "prefer-tabs",
            "Always use tabs, never spaces, in this codebase",
            "feedback",
            None,
        )
        .unwrap();

        let picked = relevant_memories(&db, Some("myrepo"), 5);
        assert!(
            picked.iter().any(|m| m.memory_type == "feedback"),
            "feedback slot not guaranteed: {:?}",
            picked.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_session_context_includes_open_todos() {
        isolate_config();
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
        let ctx = build_session_context(&db, None).unwrap();
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
