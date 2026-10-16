//! Chunks of meeting, todo and research text, cut at the `[embed]` sizes.
//!
//! The write paths and `sclerox db reembed` both build a chunk's source text
//! here, so re-cutting stored text gives the same chunks a fresh write would.

use crate::config::settings;
use crate::db::reembed::{EmbedFn, ProgressFn};
use crate::db::{Database, ItemChunks, TextChunkKind};
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

/// How many items of each kind [`rechunk_all`] re-cut, and into how many chunks.
pub struct Rechunked {
    pub meetings: usize,
    pub todos: usize,
    pub research: usize,
    pub chunks: usize,
}

/// Re-cut every meeting, todo and investigation from its stored text, then
/// swap the new chunks and the chunker record in with one transaction, so a
/// failure changes nothing.
///
/// With `embed`, the new chunks are embedded before the swap and search never
/// sees them without a vector. Without it (the database is on another model
/// and about to be rebuilt) they are stored without one.
pub fn rechunk_all(
    db: &Database,
    embed: Option<&mut EmbedFn<'_>>,
    progress: &mut ProgressFn<'_>,
) -> Result<Rechunked> {
    let mut items = Vec::new();
    let mut push = |kind, id, text: &str| {
        let chunks = if text.trim().is_empty() {
            Vec::new()
        } else {
            chunk(text).into_iter().map(|c| (c, None)).collect()
        };
        items.push(ItemChunks { kind, id, chunks });
    };
    let meetings = db.meeting_chunk_inputs()?;
    for (id, transcript, notes) in &meetings {
        let text = meeting_source(transcript.as_deref(), notes.as_deref());
        push(TextChunkKind::Meeting, *id, text);
    }
    let todos = db.todo_chunk_inputs()?;
    for (id, title, notes) in &todos {
        push(
            TextChunkKind::Todo,
            *id,
            &todo_source(title, notes.as_deref()),
        );
    }
    let research = db.investigation_chunk_inputs()?;
    for (id, plan, findings) in &research {
        let text = research_source(plan.as_deref(), findings.as_deref());
        push(TextChunkKind::Research, *id, &text);
    }

    let total: usize = items.iter().map(|i| i.chunks.len()).sum();
    if let Some(embed) = embed {
        let mut done = 0;
        for item in &mut items {
            if item.chunks.is_empty() {
                continue;
            }
            let vectors = {
                let texts: Vec<&str> = item.chunks.iter().map(|(t, _)| t.as_str()).collect();
                embed(&texts)?
            };
            let n = item.chunks.len();
            anyhow::ensure!(
                vectors.len() == n,
                "embedder returned {} vectors for {n} texts",
                vectors.len()
            );
            for ((_, slot), v) in item.chunks.iter_mut().zip(vectors) {
                *slot = Some(v);
            }
            done += n;
            progress("new text chunks", done, total);
        }
    }

    db.replace_text_chunks(&items, &signature())?;
    Ok(Rechunked {
        meetings: meetings.len(),
        todos: todos.len(),
        research: research.len(),
        chunks: total,
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

        let done = rechunk_all(&db, None, &mut |_, _, _| {}).unwrap();
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

    fn meeting_chunks(db: &Database, id: i64) -> Vec<(String, bool)> {
        db.conn
            .prepare("SELECT chunk_text, embedding IS NOT NULL FROM meeting_chunks WHERE meeting_id = ?1 ORDER BY chunk_index")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn rechunk_embeds_before_the_swap() {
        let db = Database::open_in_memory().unwrap();
        let transcript = "Paragraph on the billing cutover plan. ".repeat(60);
        let id = db
            .meeting_add("Billing", None, Some(&transcript), None)
            .unwrap();
        let mut calls = 0;
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            calls += 1;
            Ok(texts.iter().map(|_| vec![0.5f32; 384]).collect())
        };
        let done = rechunk_all(&db, Some(&mut embed), &mut |_, _, _| {}).unwrap();
        let chunks = meeting_chunks(&db, id);
        assert_eq!(chunks.len(), done.chunks);
        assert!(chunks.iter().all(|(_, has_vector)| *has_vector));
        assert_eq!(calls, 1, "one call per item");
    }

    #[test]
    fn a_failed_swap_keeps_every_old_chunk() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .meeting_add("Kept", None, Some("old text"), None)
            .unwrap();
        db.meeting_store_chunks(id, &[("old text".into(), None)])
            .unwrap();
        db.set_meta(crate::embed::TEXT_CHUNKER_KEY, "v0").unwrap();
        // The second item names a meeting that doesn't exist, so its insert
        // fails after the first item was already replaced.
        let items = [
            ItemChunks {
                kind: TextChunkKind::Meeting,
                id,
                chunks: vec![("new text".into(), None)],
            },
            ItemChunks {
                kind: TextChunkKind::Meeting,
                id: 999,
                chunks: vec![("orphan".into(), None)],
            },
        ];
        assert!(db.replace_text_chunks(&items, &signature()).is_err());
        assert_eq!(meeting_chunks(&db, id), [("old text".to_string(), false)]);
        assert_eq!(db.text_chunker().unwrap().as_deref(), Some("v0"));
    }
}
