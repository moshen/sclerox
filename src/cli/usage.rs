//! Record what commands searched for, what they returned and which records
//! were opened, in the main database's usage tables (see `MIGRATION_V16`),
//! to measure retrieval. `[usage] enabled = false` turns it off.
//!
//! Recording is best-effort: a failure is logged and never changes a
//! command's output or exit code, and the write waits on a busy database
//! for at most a moment. Commands record after printing.
//!
//! The session comes from `SCLEROX_SESSION_ID`, else Claude Code's
//! `CLAUDE_CODE_SESSION_ID`, which it sets for every command an agent runs;
//! other agents leave it empty. The agent comes from `SCLEROX_AGENT`, else
//! `claude-code` under Claude Code, else `AI_AGENT`.

use crate::db::usage::{UsageContext, UsageItem};
use crate::db::Database;
use crate::search::{Hit, HitSource, SearchResult};

/// The calling session, agent and repo, from the environment and the
/// working directory.
pub fn context() -> UsageContext {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let agent = var("SCLEROX_AGENT").or_else(|| {
        if var("CLAUDECODE").as_deref() == Some("1") {
            Some("claude-code".to_string())
        } else {
            var("AI_AGENT")
        }
    });
    let repo_path = std::env::current_dir().ok().and_then(|cwd| {
        let root = crate::index::find_git_root(&cwd);
        root.join(".git")
            .exists()
            .then(|| root.to_string_lossy().into_owned())
    });
    UsageContext {
        session_id: var("SCLEROX_SESSION_ID").or_else(|| var("CLAUDE_CODE_SESSION_ID")),
        agent,
        repo_path,
    }
}

/// A search result as recorded: its [`SearchResult::item`], and for code,
/// whose variants tell, its tier and any score.
pub fn result_item(r: &SearchResult) -> UsageItem {
    let (item_type, item_ref) = r.item();
    let (score, tier) = match r {
        SearchResult::CodeChunk { score, .. } => (Some(*score), Some("semantic")),
        SearchResult::Symbol { .. } | SearchResult::CodeText { .. } => (None, Some("keyword")),
        _ => (None, None),
    };
    UsageItem {
        score,
        tier,
        ..UsageItem::new(item_type, item_ref)
    }
}

/// A ranked hit as recorded, with its cosine score and tier.
pub fn hit_item(h: &Hit) -> UsageItem {
    UsageItem {
        score: h.score,
        tier: Some(match h.source {
            HitSource::Keyword => "keyword",
            HitSource::Semantic => "semantic",
        }),
        ..result_item(&h.result)
    }
}

/// Record a search `command` ran for `query`, with its results in the order
/// shown.
pub fn search(db: &Database, command: &str, query: &str, results: &[UsageItem]) {
    search_as(db, &context(), command, query, results);
}

/// The command a search is logged under: `base`, plus `--status` when it
/// isn't the default, so searches over different records stay apart.
pub fn with_status(base: &str, status: &str, default: &str) -> String {
    if status == default {
        base.to_string()
    } else {
        format!("{base} --status {status}")
    }
}

/// [`search`] for a caller that knows its own context, such as a hook given
/// the session on stdin.
pub fn search_as(
    db: &Database,
    ctx: &UsageContext,
    command: &str,
    query: &str,
    results: &[UsageItem],
) {
    let s = &crate::config::settings().usage;
    if !s.enabled {
        return;
    }
    if let Err(e) = db.usage_log_search(ctx, command, query, results, s.retain_days) {
        log::warn!("could not record {command} usage: {e:#}");
    }
}

/// Record that `command` opened one record.
pub fn read(db: &Database, command: &str, item_type: &str, item_ref: impl ToString) {
    let s = &crate::config::settings().usage;
    if !s.enabled {
        return;
    }
    let item_ref = item_ref.to_string();
    if let Err(e) = db.usage_log_read(&context(), command, item_type, &item_ref, s.retain_days) {
        log::warn!("could not record {command} usage: {e:#}");
    }
}

/// A result the keyword tier found.
pub fn keyword(item_type: &'static str, item_ref: impl ToString) -> UsageItem {
    UsageItem {
        tier: Some("keyword"),
        ..UsageItem::new(item_type, item_ref)
    }
}

/// A result the semantic tier found, with its cosine score.
pub fn semantic(item_type: &'static str, item_ref: impl ToString, score: f32) -> UsageItem {
    UsageItem {
        score: Some(score),
        tier: Some("semantic"),
        ..UsageItem::new(item_type, item_ref)
    }
}
