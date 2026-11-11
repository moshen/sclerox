use anyhow::Result;
use clap::Subcommand;

use crate::cli::text_chunks;
use crate::db::meetings::{Meeting, MeetingPerson};
use crate::db::{ChunkSource, Database, TextChunkKind};
use crate::output::{floor_char_boundary, print_miss, print_output, OutputFormat};

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
        #[command(flatten)]
        dates: crate::cli::dates::DateArgs,
    },
    /// Search meetings (FTS + semantic similarity combined)
    Search {
        query: String,
        #[command(flatten)]
        dates: crate::cli::dates::DateArgs,
    },
    /// Update a meeting's fields
    Update {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, help = "ISO date, e.g. 2026-06-08")]
        date: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Path to a transcript text file (replaces the stored transcript and
        /// its embeddings)
        #[arg(long)]
        transcript_file: Option<String>,
        /// Skip embedding generation (embeddings are on by default)
        #[arg(long)]
        no_embed: bool,
    },
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

pub fn run(db: &Database, cmd: MeetingCommand, format: OutputFormat) -> Result<()> {
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

            let source =
                text_chunks::meeting_source(&title, notes.as_deref(), transcript.as_deref());
            store_chunks(db, id, &source, no_embed)?;

            let m = db.meeting_get(id)?;
            print_output(format, &m, || println!("Added meeting #{id}: {title}"));
        }

        MeetingCommand::Get { id } => match db.meeting_get(id)? {
            Some(m) => {
                let people = db.meeting_people(id)?;
                print_output(
                    format,
                    &serde_json::json!({"meeting": m, "people": people}),
                    || print_meeting(&m, &people),
                );
                crate::cli::usage::read(db, "meeting get", "meeting", id);
            }
            None => print_miss(format, &format!("Meeting #{id} not found")),
        },

        MeetingCommand::List { dates } => {
            let meetings = db.meeting_list(&dates.dates()?)?;
            print_output(format, &meetings, || {
                if meetings.is_empty() {
                    println!("No meetings found.");
                } else {
                    for m in &meetings {
                        let date = m.meeting_date.as_deref().unwrap_or("no date");
                        println!("#{} [{}] {}", m.id, date, m.title);
                    }
                    println!("\n{} meetings", meetings.len());
                }
            });
        }

        MeetingCommand::Search { query, dates } => {
            let dates = dates.dates()?;
            // FTS results first (exact keyword matches)
            let fts_hits = db.meeting_search(&query, &dates)?;
            let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|m| m.id).collect();

            // Semantic results - appended, deduped against FTS hits
            let query_emb = crate::embed::optional(db.semantic_ready())
                .and_then(|mut e| e.embed_query(&query).ok());
            let semantic: Vec<_> = query_emb
                .map(|qe| db.meeting_similar(&qe, 5, &dates).unwrap_or_default())
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !fts_ids.contains(&r.meeting.id))
                .collect();

            print_output(
                format,
                &serde_json::json!({"fts": fts_hits, "semantic": semantic}),
                || {
                    if fts_hits.is_empty() && semantic.is_empty() {
                        println!("No matches for: {query}");
                        return;
                    }
                    for m in &fts_hits {
                        let date = m.meeting_date.as_deref().unwrap_or("no date");
                        let at = crate::cli::format::stamp("created", &m.created_at);
                        println!("#{} [{}] {}  {at}", m.id, date, m.title);
                    }
                    if !semantic.is_empty() && !fts_hits.is_empty() {
                        println!();
                    }
                    for r in &semantic {
                        let date = r.meeting.meeting_date.as_deref().unwrap_or("no date");
                        println!(
                            "#{} [{}] {}  ({:.0}% match)  {}",
                            r.meeting.id,
                            date,
                            r.meeting.title,
                            r.score * 100.0,
                            crate::cli::format::stamp("created", &r.meeting.created_at)
                        );
                        println!(
                            "  > {}",
                            crate::cli::format::matched(
                                r.matched_section.as_deref(),
                                &r.matched_chunk
                            )
                        );
                    }
                },
            );
            let logged: Vec<_> = fts_hits
                .iter()
                .map(|m| crate::cli::usage::keyword("meeting", m.id))
                .chain(
                    semantic
                        .iter()
                        .map(|r| crate::cli::usage::semantic("meeting", r.meeting.id, r.score)),
                )
                .collect();
            crate::cli::usage::search(db, "meeting search", &query, &logged);
        }

        MeetingCommand::Update {
            id,
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
            let changed = db.meeting_update(
                id,
                title.as_deref(),
                date.as_deref(),
                notes.as_deref(),
                transcript.as_deref(),
            )?;
            if !changed {
                print_miss(format, &format!("Meeting #{id} not found or no changes"));
                return Ok(());
            }
            // The chunks hold the notes and transcript, headed by the title.
            let m = db.meeting_get(id)?;
            if let Some(m) = m
                .as_ref()
                .filter(|_| title.is_some() || notes.is_some() || transcript.is_some())
            {
                let source = text_chunks::meeting_source(
                    &m.title,
                    m.notes.as_deref(),
                    m.transcript.as_deref(),
                );
                store_chunks(db, id, &source, no_embed)?;
            }
            print_output(format, &m, || println!("Updated meeting #{id}"));
        }

        MeetingCommand::People(sub) => match sub {
            MeetingPeopleCmd::Add {
                meeting_id,
                person_id,
                role,
            } => {
                db.meeting_link_person(meeting_id, person_id, role.as_deref())?;
                let people = db.meeting_people(meeting_id)?;
                print_output(format, &people, || {
                    println!("Linked person #{person_id} to meeting #{meeting_id}")
                });
            }
            MeetingPeopleCmd::Remove {
                meeting_id,
                person_id,
            } => {
                if db.meeting_unlink_person(meeting_id, person_id)? {
                    let people = db.meeting_people(meeting_id)?;
                    print_output(format, &people, || {
                        println!("Removed person #{person_id} from meeting #{meeting_id}")
                    });
                } else {
                    print_miss(format, "Link not found");
                }
            }
            MeetingPeopleCmd::List { meeting_id } => {
                let people = db.meeting_people(meeting_id)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people linked to meeting #{meeting_id}");
                    } else {
                        for p in &people {
                            let role = p.role.as_deref().unwrap_or("attendee");
                            println!("#{} {} ({})", p.person_id, p.person_name, role);
                        }
                    }
                });
            }
        },

        MeetingCommand::Delete { id } => {
            if db.meeting_delete(id)? {
                print_output(format, &serde_json::json!({"deleted": id}), || {
                    println!("Deleted meeting #{id}")
                });
            } else {
                print_miss(format, &format!("Meeting #{id} not found"));
            }
        }
    }
    Ok(())
}

/// Store `source` as the meeting's chunks, replacing any existing ones.
/// Shared by `add` and `update`.
fn store_chunks(
    db: &Database,
    meeting_id: i64,
    source: &ChunkSource,
    no_embed: bool,
) -> Result<()> {
    let mut embedder = if no_embed {
        None
    } else {
        crate::embed::optional(db.semantic_ready())
    };
    text_chunks::store(
        db,
        TextChunkKind::Meeting,
        meeting_id,
        source,
        embedder.as_mut(),
    )
}

fn print_meeting(m: &Meeting, people: &[MeetingPerson]) {
    println!("ID:    #{}", m.id);
    println!("Title: {}", m.title);
    println!("Date:  {}", m.meeting_date.as_deref().unwrap_or("-"));
    println!("Created: {}", m.created_at);
    if let Some(notes) = &m.notes {
        println!("Notes:\n{notes}");
    }
    if let Some(transcript) = &m.transcript {
        println!("Transcript ({} chars):", transcript.len());
        println!("{}", &transcript[..floor_char_boundary(transcript, 500)]);
        if transcript.len() > 500 {
            println!("... [truncated]");
        }
    }
    if !people.is_empty() {
        println!("People:");
        for p in people {
            let role = p.role.as_deref().unwrap_or("attendee");
            println!("  - {} ({})", p.person_name, role);
        }
    }
}
