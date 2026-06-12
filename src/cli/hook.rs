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
}

pub fn run(db: &Database, cmd: HookCommand) -> Result<()> {
    match cmd {
        HookCommand::Start => run_start(db),
        HookCommand::Stop {
            via,
            model,
            no_distill,
        } => run_stop(db, via.as_deref(), model.as_deref(), no_distill),
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

    // Extract conversation text from the JSONL transcript
    let text = match extract_conversation_text(&transcript_path) {
        Ok(t) if t.trim().is_empty() => return Ok(()),
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    // Skip very short sessions (fewer than ~5 turns worth of content)
    if text.lines().count() < 10 {
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

    // Distill memories - failures are silent (hook must not block session exit)
    let memories = match crate::cli::memory::distill_with_ai_pub(bin, resolved_model, &text) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    for m in &memories {
        let _ = db.memory_set_full(&m.key, &m.value, &m.memory_type, None, "session");
    }

    if !memories.is_empty() {
        // Print to stderr so it shows in the Claude Code output but doesn't interfere
        eprintln!("[ol] distilled {} memories from session", memories.len());
    }

    Ok(())
}

/// Derive the Claude Code project hash from a working directory path.
/// Claude Code replaces every `/` with `-`, giving `-Users-foo-code-project`.
fn path_to_project_hash(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('/', "-")
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

/// Extract human-readable conversation text from a Claude Code session JSONL.
/// Returns a condensed text suitable for memory distillation.
fn extract_conversation_text(path: &std::path::Path) -> Result<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut text = String::new();

    for line in reader.lines() {
        let line = line?;
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        match obj["type"].as_str() {
            Some("user") => {
                if let Some(content) = extract_text_content(&obj["message"]["content"]) {
                    if !content.trim().is_empty() {
                        text.push_str("User: ");
                        text.push_str(&truncate_content(&content, 1000));
                        text.push('\n');
                    }
                }
            }
            Some("assistant") => {
                if let Some(content) = extract_text_content(&obj["message"]["content"]) {
                    if !content.trim().is_empty() {
                        text.push_str("Assistant: ");
                        text.push_str(&truncate_content(&content, 2000));
                        text.push('\n');
                    }
                }
            }
            _ => {}
        }
    }

    Ok(text)
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
        assert_eq!(
            path_to_project_hash(std::path::Path::new("/Users/colin/code/myproject")),
            "-Users-colin-code-myproject"
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
}
