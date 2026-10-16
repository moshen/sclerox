//! Chunks of meeting, todo and research text, cut at the `[embed]` sizes.
//!
//! The write paths and `sclerox db reembed` both build a chunk's source text
//! here, so re-cutting stored text gives the same chunks a fresh write would.

use crate::config::settings;
use crate::db::Database;
use crate::embed::chunk_text;
use anyhow::Result;

/// Cut `text` at the configured `embed.chunk_size` and `embed.chunk_overlap`.
pub fn chunk(text: &str) -> Vec<String> {
    let cfg = &settings().embed;
    chunk_text(text, cfg.chunk_size, cfg.chunk_overlap)
}

/// The signature of [`chunk`] at the configured sizes.
pub fn signature() -> String {
    let cfg = &settings().embed;
    crate::embed::text_chunker_signature(cfg.chunk_size, cfg.chunk_overlap)
}

/// Record the configured chunker for the database, unless one is recorded.
/// Call after storing chunks.
pub fn note(db: &Database) {
    if let Err(e) = db.note_text_chunker(&signature()) {
        log::warn!("could not record the text chunker: {e:#}");
    }
}

/// A meeting's chunks come from its transcript, or its notes without one.
pub fn meeting_source<'a>(transcript: Option<&'a str>, notes: Option<&'a str>) -> &'a str {
    transcript.filter(|t| !t.is_empty()).or(notes).unwrap_or("")
}

/// A todo's chunks come from its title, then its notes.
pub fn todo_source(title: &str, notes: Option<&str>) -> String {
    match notes {
        Some(n) if !n.trim().is_empty() => format!("{title}\n\n{n}"),
        _ => title.to_string(),
    }
}

/// An investigation's chunks come from its plan, then its findings.
pub fn research_source(plan: Option<&str>, findings: Option<&str>) -> String {
    [plan, findings]
        .into_iter()
        .flatten()
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Whether stored chunks were cut by another chunker than the configured one.
/// A database without chunks has nothing recorded and nothing to re-cut.
pub fn stale(db: &Database) -> bool {
    matches!(db.text_chunker(), Ok(Some(recorded)) if recorded != signature())
}

/// How many items of each kind [`rechunk_all`] re-cut.
pub struct Rechunked {
    pub meetings: usize,
    pub todos: usize,
    pub research: usize,
}

/// Re-cut every meeting, todo and investigation from its stored text, then
/// record the configured chunker. The new chunks carry no vector; the caller
/// embeds them (`db reembed` fills missing vectors next).
pub fn rechunk_all(db: &Database) -> Result<Rechunked> {
    let pieces = |text: &str| -> Vec<(String, Option<Vec<f32>>)> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        chunk(text).into_iter().map(|c| (c, None)).collect()
    };

    let meetings = db.meeting_chunk_inputs()?;
    for (id, transcript, notes) in &meetings {
        let text = meeting_source(transcript.as_deref(), notes.as_deref());
        db.meeting_store_chunks(*id, &pieces(text))?;
    }
    let todos = db.todo_chunk_inputs()?;
    for (id, title, notes) in &todos {
        db.todo_store_chunks(*id, &pieces(&todo_source(title, notes.as_deref())))?;
    }
    let research = db.investigation_chunk_inputs()?;
    for (id, plan, findings) in &research {
        let text = research_source(plan.as_deref(), findings.as_deref());
        db.investigation_store_chunks(*id, &pieces(&text))?;
    }

    db.set_meta(crate::embed::TEXT_CHUNKER_KEY, &signature())?;
    Ok(Rechunked {
        meetings: meetings.len(),
        todos: todos.len(),
        research: research.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_match_the_write_paths() {
        assert_eq!(meeting_source(Some("t"), Some("n")), "t");
        assert_eq!(meeting_source(Some(""), Some("n")), "n");
        assert_eq!(meeting_source(None, None), "");
        assert_eq!(todo_source("Title", Some("notes")), "Title\n\nnotes");
        assert_eq!(todo_source("Title", Some("  ")), "Title");
        assert_eq!(
            research_source(Some("plan"), Some("found")),
            "plan\n\nfound"
        );
        assert_eq!(research_source(None, Some("found")), "found");
        assert_eq!(research_source(Some(""), None), "");
    }

    #[test]
    fn rechunk_recuts_stored_text_and_records_the_chunker() {
        let db = Database::open_in_memory().unwrap();
        let transcript = "Long transcript paragraph about the rollout. ".repeat(60);
        let id = db
            .meeting_add("Sync", None, Some(&transcript), None)
            .unwrap();
        // One oversized chunk, as an older chunker could have left it.
        db.meeting_store_chunks(id, &[(transcript.clone(), None)])
            .unwrap();
        db.set_meta(crate::embed::TEXT_CHUNKER_KEY, "v0").unwrap();
        assert!(stale(&db));

        let done = rechunk_all(&db).unwrap();
        assert_eq!(done.meetings, 1);
        assert!(!stale(&db));
        assert_eq!(db.text_chunker().unwrap(), Some(signature()));
        let chunks: Vec<String> = db
            .conn
            .prepare("SELECT chunk_text FROM meeting_chunks WHERE meeting_id = ?1")
            .unwrap()
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(chunks.len() > 1, "{} chunk(s)", chunks.len());
        let max = settings().embed.chunk_size;
        assert!(chunks.iter().all(|c| c.chars().count() <= max));
    }
}
