//! Chunks of meeting, todo and research text, cut at the `[embed]` sizes.
//!
//! The write paths and `sclerox db reembed` both build a chunk's source text
//! here, so re-cutting stored text gives the same chunks a fresh write would.

use crate::config::settings;
use crate::db::reembed::{EmbedFn, ProgressFn};
use crate::db::{ChunkSource, Database, ItemChunks, ReplacedTextChunks, TextChunk, TextChunkKind};
use crate::embed::{chunk_text, Embedder};
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

/// A meeting's chunks come from its notes, then its transcript, each headed
/// by the title and section.
pub fn meeting_source(title: &str, notes: Option<&str>, transcript: Option<&str>) -> ChunkSource {
    TextChunkKind::Meeting.source(Some(title), notes, transcript)
}

/// A todo's chunks come from its title, then its notes.
pub fn todo_source(title: &str, notes: Option<&str>) -> ChunkSource {
    TextChunkKind::Todo.source(None, Some(title), notes)
}

/// An investigation's chunks come from its plan, then its findings, each
/// headed by its name and section.
pub fn research_source(name: &str, plan: Option<&str>, findings: Option<&str>) -> ChunkSource {
    TextChunkKind::Research.source(Some(name), plan, findings)
}

/// Cut `source` at the configured sizes.
pub fn cut(source: &ChunkSource) -> Vec<TextChunk> {
    source.cut(&chunk)
}

/// Cut `source` and store it as item `id`'s chunks, replacing its old ones,
/// embedded with `embedder` when there is one. Without one (`--no-embed`,
/// embeddings off, model unavailable) they are stored without vectors, for
/// `db reembed` to fill.
pub fn store(
    db: &Database,
    kind: TextChunkKind,
    id: i64,
    source: &ChunkSource,
    embedder: Option<&mut Embedder>,
) -> Result<()> {
    let mut chunks = cut(source);
    if let Some(embedder) = embedder {
        embed(&mut chunks, &mut |texts| embedder.embed_each(texts))?;
    }
    db.store_text_chunks(kind, id, &chunks)?;
    note(db);
    Ok(())
}

/// Give each of `chunks` its vector, embedded from its passage.
fn embed(chunks: &mut [TextChunk], embed: &mut EmbedFn<'_>) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let passages: Vec<String> = chunks.iter().map(TextChunk::passage).collect();
    let vectors = embed(&passages.iter().map(String::as_str).collect::<Vec<_>>())?;
    anyhow::ensure!(
        vectors.len() == chunks.len(),
        "embedder returned {} vectors for {} texts",
        vectors.len(),
        chunks.len()
    );
    for (chunk, v) in chunks.iter_mut().zip(vectors) {
        chunk.embedding = Some(v);
    }
    Ok(())
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
            let chunks = cut(&source);
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
            let n = item.chunks.len();
            self::embed(&mut item.chunks, embed)?;
            done += n;
            progress("new text chunks", done, total);
        }
    }

    let replaced = db.replace_text_chunks(&items, &signature(), &cut)?;
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
        let notes = |t: &str| (Some("notes"), t.to_string());
        let transcript = |t: &str| (Some("transcript"), t.to_string());
        assert_eq!(
            meeting_source("Sync", Some("n"), Some("t")).sections,
            [notes("n"), transcript("t")]
        );
        assert_eq!(
            meeting_source("Sync", Some(" "), Some("t")).sections,
            [transcript("t")]
        );
        assert!(meeting_source("Sync", None, None).sections.is_empty());
        assert_eq!(
            todo_source("Title", Some("notes")),
            ChunkSource {
                title: None,
                sections: vec![(None, "Title\n\nnotes".into())],
            }
        );
        assert_eq!(
            todo_source("Title", Some("  ")).sections,
            [(None, "Title".to_string())]
        );
        assert_eq!(
            research_source("Cache", Some("plan"), Some("found")).sections,
            [
                (Some("plan"), "plan".to_string()),
                (Some("findings"), "found".to_string())
            ]
        );
        assert!(research_source("Cache", Some(""), None).sections.is_empty());
    }

    #[test]
    fn chunks_are_headed_by_their_title_and_section() {
        let chunks = cut(&meeting_source(
            "Launch sync",
            Some("Moved the date."),
            Some("A long talk."),
        ));
        let parts: Vec<_> = chunks
            .iter()
            .map(|c| (c.section, c.context.as_deref(), c.text.as_str()))
            .collect();
        assert_eq!(
            parts,
            [
                (
                    Some("notes"),
                    Some("Launch sync (notes)"),
                    "Moved the date."
                ),
                (
                    Some("transcript"),
                    Some("Launch sync (transcript)"),
                    "A long talk."
                )
            ]
        );
        assert_eq!(
            chunks[0].passage(),
            "Launch sync (notes)\n\nMoved the date."
        );
        let todo = cut(&todo_source("Renew the cert", None));
        assert_eq!(todo[0].context, None);
        assert_eq!(todo[0].passage(), "Renew the cert");
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

    /// Item `id` as rechunk_all would plan it now, its one chunk marked new.
    fn item(db: &Database, kind: TextChunkKind, id: i64) -> ItemChunks {
        let source = db.text_chunk_source(kind, id).unwrap().unwrap();
        let mut chunks = cut(&source);
        for c in &mut chunks {
            c.text = format!("new {}", c.text);
        }
        ItemChunks {
            kind,
            id,
            source,
            chunks,
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
        let items = [item(&db, TextChunkKind::Meeting, id)];
        assert!(db.replace_text_chunks(&items, &signature(), &cut).is_err());
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
            item(&db, TextChunkKind::Meeting, edited),
            item(&db, TextChunkKind::Meeting, deleted),
            item(&db, TextChunkKind::Meeting, same),
        ];
        // Changed while the new chunks were being embedded.
        db.conn
            .execute(
                "UPDATE meetings SET transcript = 'after' WHERE id = ?1",
                [edited],
            )
            .unwrap();
        db.meeting_delete(deleted).unwrap();

        let replaced = db.replace_text_chunks(&items, &signature(), &cut).unwrap();
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
