//! Chunks of meeting, todo and research text, cut at the `[embed]` sizes.
//!
//! The write paths and `sclerox db reembed` both build a chunk's source text
//! here, so re-cutting stored text gives the same chunks a fresh write would.

use crate::config::settings;
use crate::db::reembed::{EmbedFn, ProgressFn};
use crate::db::{Database, ItemChunks, ReplacedTextChunks, TextChunkKind};
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
pub fn meeting_source(transcript: Option<&str>, notes: Option<&str>) -> String {
    TextChunkKind::Meeting.source_text(transcript, notes)
}

/// A todo's chunks come from its title, then its notes.
pub fn todo_source(title: &str, notes: Option<&str>) -> String {
    TextChunkKind::Todo.source_text(Some(title), notes)
}

/// An investigation's chunks come from its plan, then its findings.
pub fn research_source(plan: Option<&str>, findings: Option<&str>) -> String {
    TextChunkKind::Research.source_text(plan, findings)
}

/// Whether stored chunks were cut by another chunker than the configured one.
/// Chunks with no record count as stale (recording after a write can fail);
/// a database without chunks has nothing to re-cut.
pub fn stale(db: &Database) -> bool {
    match db.text_chunker() {
        Ok(Some(recorded)) => recorded != signature(),
        Ok(None) => db.has_text_chunks().unwrap_or(false),
        Err(_) => false,
    }
}

/// How many items of each kind [`rechunk_all`] re-cut, into how many chunks,
/// and what changed under it.
pub struct Rechunked {
    pub meetings: usize,
    pub todos: usize,
    pub research: usize,
    pub chunks: usize,
    pub replaced: ReplacedTextChunks,
}

/// Re-cut every meeting, todo and investigation from its stored text, then
/// swap the new chunks and the chunker record in with one transaction, so a
/// failure changes nothing. An item edited or deleted meanwhile is handled in
/// the swap; see [`Database::replace_text_chunks`].
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
    let mut counts = [0usize; 3];
    for (n, kind) in counts.iter_mut().zip(TextChunkKind::ALL) {
        for (id, source) in db.text_chunk_sources(kind)? {
            let chunks = if source.trim().is_empty() {
                Vec::new()
            } else {
                chunk(&source).into_iter().map(|c| (c, None)).collect()
            };
            items.push(ItemChunks {
                kind,
                id,
                source,
                chunks,
            });
            *n += 1;
        }
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

    let replaced = db.replace_text_chunks(&items, &signature(), &chunk)?;
    let [meetings, todos, research] = counts;
    Ok(Rechunked {
        meetings,
        todos,
        research,
        chunks: total,
        replaced,
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

    fn item(kind: TextChunkKind, id: i64, source: &str) -> ItemChunks {
        ItemChunks {
            kind,
            id,
            source: source.to_string(),
            chunks: vec![(format!("new {source}"), None)],
        }
    }

    #[test]
    fn unrecorded_chunks_are_stale_and_no_chunks_are_not() {
        let db = Database::open_in_memory().unwrap();
        assert!(!stale(&db));
        let id = db.meeting_add("M", None, Some("text"), None).unwrap();
        // Stored, but the record after it failed.
        db.meeting_store_chunks(id, &[("text".into(), None)])
            .unwrap();
        assert!(stale(&db));
        note(&db);
        assert!(!stale(&db));
    }

    #[test]
    fn a_failed_swap_keeps_every_old_chunk() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .meeting_add("Kept", None, Some("old text"), None)
            .unwrap();
        db.meeting_store_chunks(id, &[("old text".into(), None)])
            .unwrap();
        // Every item is replaced before the chunker record is written last;
        // without the meta table that write fails.
        db.conn.execute_batch("DROP TABLE meta").unwrap();
        let items = [item(TextChunkKind::Meeting, id, "old text")];
        assert!(db
            .replace_text_chunks(&items, &signature(), &chunk)
            .is_err());
        assert_eq!(meeting_chunks(&db, id), [("old text".to_string(), false)]);
    }

    #[test]
    fn the_swap_recuts_edited_items_and_skips_deleted_ones() {
        let db = Database::open_in_memory().unwrap();
        let edited = db
            .meeting_add("Edited", None, Some("before"), None)
            .unwrap();
        let deleted = db.meeting_add("Deleted", None, Some("gone"), None).unwrap();
        let same = db.meeting_add("Same", None, Some("same"), None).unwrap();
        let items = [
            item(TextChunkKind::Meeting, edited, "before"),
            item(TextChunkKind::Meeting, deleted, "gone"),
            item(TextChunkKind::Meeting, same, "same"),
        ];
        // Changed while the new chunks were being embedded.
        db.conn
            .execute(
                "UPDATE meetings SET transcript = 'after' WHERE id = ?1",
                [edited],
            )
            .unwrap();
        db.meeting_delete(deleted).unwrap();

        let replaced = db
            .replace_text_chunks(&items, &signature(), &chunk)
            .unwrap();
        assert_eq!(replaced, ReplacedTextChunks { recut: 1, gone: 1 });
        // Cut from the new text, left for the missing-vector fill.
        assert_eq!(meeting_chunks(&db, edited), [("after".to_string(), false)]);
        assert_eq!(meeting_chunks(&db, same), [("new same".to_string(), false)]);
        assert_eq!(db.text_chunker().unwrap(), Some(signature()));
    }

    #[test]
    fn an_edit_during_embedding_wins_over_the_old_text() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .meeting_add("Plan", None, Some("first draft"), None)
            .unwrap();
        db.set_meta(crate::embed::TEXT_CHUNKER_KEY, "v0").unwrap();
        // The meeting is edited (as by another sclerox) while rechunk_all is
        // embedding the chunks it cut from the first draft.
        let mut embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>> {
            db.conn
                .execute(
                    "UPDATE meetings SET transcript = 'final text' WHERE id = ?1",
                    [id],
                )
                .unwrap();
            Ok(texts.iter().map(|_| vec![0.5f32; 384]).collect())
        };
        let r = rechunk_all(&db, Some(&mut embed), &mut |_, _, _| {}).unwrap();
        assert_eq!(r.replaced.recut, 1);
        assert_eq!(meeting_chunks(&db, id), [("final text".to_string(), false)]);
    }
}
