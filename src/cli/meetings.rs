use anyhow::Result;
use clap::Subcommand;

use crate::db::Database;
use crate::embed::{chunk_text, Embedder};

// AllMiniLML6V2 has a 256-token context window (~4 chars/token = ~1024 chars).
// 800 chars gives ~200 tokens with a safety margin for subword tokenization.
// Overlap of 200 chars (~50 words) keeps context across chunk boundaries.
const CHUNK_SIZE: usize = 800;
const CHUNK_OVERLAP: usize = 200;

#[derive(Subcommand)]
pub enum MeetingCommand {
    /// Add a meeting
    Add {
        #[arg(long)]
        title: String,
        #[arg(long, help = "ISO date, e.g. 2026-06-08")]
        date: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Path to a transcript text file
        #[arg(long)]
        transcript_file: Option<String>,
        /// Skip embedding generation (embeddings are on by default)
        #[arg(long)]
        no_embed: bool,
    },
    /// Get a meeting by ID
    Get { id: i64 },
    /// List meetings
    List {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Search meetings (FTS + semantic similarity combined)
    Search { query: String },
    /// Manage people linked to a meeting
    #[command(subcommand)]
    People(MeetingPeopleCmd),
    /// Delete a meeting
    Delete { id: i64 },
}

#[derive(clap::Subcommand)]
pub enum MeetingPeopleCmd {
    /// Link a person to this meeting
    Add {
        meeting_id: i64,
        person_id: i64,
        #[arg(long)]
        role: Option<String>,
    },
    /// Remove a person link from this meeting
    Remove { meeting_id: i64, person_id: i64 },
    /// List people in this meeting
    List { meeting_id: i64 },
}

pub fn run(db: &Database, cmd: MeetingCommand) -> Result<()> {
    match cmd {
        MeetingCommand::Add {
            title,
            date,
            notes,
            transcript_file,
            no_embed,
        } => {
            let transcript = transcript_file
                .as_deref()
                .map(std::fs::read_to_string)
                .transpose()?;

            let id = db.meeting_add(
                &title,
                date.as_deref(),
                transcript.as_deref(),
                notes.as_deref(),
            )?;

            // Chunk and optionally embed the combined text
            let text_to_chunk = transcript.as_deref().or(notes.as_deref()).unwrap_or("");

            if !text_to_chunk.is_empty() {
                let raw_chunks = chunk_text(text_to_chunk, CHUNK_SIZE, CHUNK_OVERLAP);
                let chunks: Vec<(String, Option<Vec<f32>>)> = if !no_embed {
                    let mut embedder = Embedder::new()?;
                    let texts: Vec<&str> = raw_chunks.iter().map(|s| s.as_str()).collect();
                    let embeddings = embedder.embed_batch(&texts)?;
                    raw_chunks
                        .into_iter()
                        .zip(embeddings.into_iter().map(Some))
                        .collect()
                } else {
                    raw_chunks.into_iter().map(|c| (c, None)).collect()
                };
                db.meeting_store_chunks(id, &chunks)?;
            }

            println!("Added meeting #{id}: {title}");
        }

        MeetingCommand::Get { id } => match db.meeting_get(id)? {
            Some(m) => {
                println!("ID:    #{}", m.id);
                println!("Title: {}", m.title);
                println!("Date:  {}", m.meeting_date.as_deref().unwrap_or("-"));
                if let Some(notes) = &m.notes {
                    println!("Notes:\n{notes}");
                }
                if let Some(transcript) = &m.transcript {
                    println!("Transcript ({} chars):", transcript.len());
                    println!("{}", &transcript[..transcript.len().min(500)]);
                    if transcript.len() > 500 {
                        println!("... [truncated]");
                    }
                }
                let people = db.meeting_people(id)?;
                if !people.is_empty() {
                    println!("People:");
                    for p in &people {
                        let role = p.role.as_deref().unwrap_or("attendee");
                        println!("  - {} ({})", p.person_name, role);
                    }
                }
            }
            None => println!("Meeting #{id} not found"),
        },

        MeetingCommand::List { from, to } => {
            let meetings = db.meeting_list(from.as_deref(), to.as_deref())?;
            if meetings.is_empty() {
                println!("No meetings found.");
            } else {
                for m in &meetings {
                    let date = m.meeting_date.as_deref().unwrap_or("no date");
                    println!("#{} [{}] {}", m.id, date, m.title);
                }
                println!("\n{} meetings", meetings.len());
            }
        }

        MeetingCommand::Search { query } => {
            // FTS results first (exact keyword matches)
            let fts_hits = db.meeting_search(&query)?;
            let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|m| m.id).collect();

            for m in &fts_hits {
                let date = m.meeting_date.as_deref().unwrap_or("no date");
                println!("#{} [{}] {}", m.id, date, m.title);
            }

            // Semantic results - appended, deduped against FTS hits
            let mut embedder = Embedder::new()?;
            if let Ok(query_emb) = embedder.embed_one(&query) {
                let similar = db.meeting_similar(&query_emb, 5).unwrap_or_default();
                let semantic: Vec<_> = similar
                    .iter()
                    .filter(|r| !fts_ids.contains(&r.meeting.id))
                    .collect();
                if !semantic.is_empty() {
                    if !fts_hits.is_empty() {
                        println!();
                    }
                    for r in &semantic {
                        let date = r.meeting.meeting_date.as_deref().unwrap_or("no date");
                        println!(
                            "#{} [{}] {}  ({:.0}% match)",
                            r.meeting.id,
                            date,
                            r.meeting.title,
                            r.score * 100.0
                        );
                        println!("  > {}", truncate(&r.matched_chunk, 100));
                    }
                }
            }

            if fts_hits.is_empty() {
                println!("No matches for: {query}");
            }
        }

        MeetingCommand::People(sub) => match sub {
            MeetingPeopleCmd::Add {
                meeting_id,
                person_id,
                role,
            } => {
                db.meeting_link_person(meeting_id, person_id, role.as_deref())?;
                println!("Linked person #{person_id} to meeting #{meeting_id}");
            }
            MeetingPeopleCmd::Remove {
                meeting_id,
                person_id,
            } => {
                if db.meeting_unlink_person(meeting_id, person_id)? {
                    println!("Removed person #{person_id} from meeting #{meeting_id}");
                } else {
                    println!("Link not found");
                }
            }
            MeetingPeopleCmd::List { meeting_id } => {
                let people = db.meeting_people(meeting_id)?;
                if people.is_empty() {
                    println!("No people linked to meeting #{meeting_id}");
                } else {
                    for p in &people {
                        let role = p.role.as_deref().unwrap_or("attendee");
                        println!("#{} {} ({})", p.person_id, p.person_name, role);
                    }
                }
            }
        },

        MeetingCommand::Delete { id } => {
            if db.meeting_delete(id)? {
                println!("Deleted meeting #{id}");
            } else {
                println!("Meeting #{id} not found");
            }
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
