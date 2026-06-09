use anyhow::Result;
use clap::Subcommand;

use crate::db::Database;
use crate::embed::{chunk_text, Embedder};

const CHUNK_SIZE: usize = 1500;
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
        /// Generate embeddings for similarity search (requires model download on first run)
        #[arg(long)]
        embed: bool,
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
    /// Full-text search meetings
    Search { query: String },
    /// Find similar meetings using embedding similarity
    Similar {
        /// Query text to find similar meetings
        query: String,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
    /// Link a person to this meeting
    LinkPerson {
        meeting_id: i64,
        person_id: i64,
        #[arg(long)]
        role: Option<String>,
    },
    /// Remove a person link from a meeting
    UnlinkPerson { meeting_id: i64, person_id: i64 },
    /// List people in a meeting
    People { meeting_id: i64 },
    /// Delete a meeting
    Delete { id: i64 },
}

pub fn run(db: &Database, cmd: MeetingCommand) -> Result<()> {
    match cmd {
        MeetingCommand::Add {
            title,
            date,
            notes,
            transcript_file,
            embed,
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
                let chunks: Vec<(String, Option<Vec<f32>>)> = if embed {
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
            let results = db.meeting_search(&query)?;
            if results.is_empty() {
                println!("No matches for: {query}");
            } else {
                for m in &results {
                    let date = m.meeting_date.as_deref().unwrap_or("no date");
                    println!("#{} [{}] {}", m.id, date, m.title);
                }
            }
        }

        MeetingCommand::Similar { query, limit } => {
            let mut embedder = Embedder::new()?;
            let query_emb = embedder.embed_one(&query)?;
            let results = db.meeting_similar(&query_emb, limit)?;
            if results.is_empty() {
                println!("No similar meetings found. Run 'ol meeting add --embed' to enable similarity search.");
            } else {
                for r in &results {
                    println!(
                        "#{} {:.3} - {} [{}]",
                        r.meeting.id,
                        r.score,
                        r.meeting.title,
                        r.meeting.meeting_date.as_deref().unwrap_or("no date")
                    );
                    println!("  > {}", truncate(&r.matched_chunk, 100));
                }
            }
        }

        MeetingCommand::LinkPerson {
            meeting_id,
            person_id,
            role,
        } => {
            db.meeting_link_person(meeting_id, person_id, role.as_deref())?;
            println!("Linked person #{person_id} to meeting #{meeting_id}");
        }

        MeetingCommand::UnlinkPerson {
            meeting_id,
            person_id,
        } => {
            if db.meeting_unlink_person(meeting_id, person_id)? {
                println!("Unlinked person #{person_id} from meeting #{meeting_id}");
            } else {
                println!("Link not found");
            }
        }

        MeetingCommand::People { meeting_id } => {
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
