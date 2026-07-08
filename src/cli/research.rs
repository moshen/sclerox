use anyhow::Result;
use clap::Subcommand;

use crate::config::settings;
use crate::db::Database;
use crate::embed::{chunk_text, Embedder};
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum ResearchCommand {
    /// Start a new investigation
    #[command(alias = "create", alias = "add", alias = "new")]
    Start {
        #[arg(long)]
        name: String,
        /// Short slug for the investigation (e.g. "bill-attach-traffic")
        #[arg(long)]
        slug: String,
        /// Initial plan / scope
        #[arg(long)]
        plan: Option<String>,
    },
    /// Get an investigation by ID or slug
    #[command(alias = "show")]
    Get {
        /// ID or slug
        id_or_slug: String,
    },
    /// List investigations
    List {
        #[arg(long, default_value = "open",
              value_parser = ["open","concluded","all"])]
        status: String,
    },
    /// Search investigations (FTS + semantic similarity combined)
    Search { query: String },
    /// Update plan, findings, or status
    Update {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        plan: Option<String>,
        #[arg(long)]
        findings: Option<String>,
    },
    /// Reopen a concluded investigation
    Reopen { id: i64 },
    /// Conclude an investigation with final findings
    #[command(alias = "close", alias = "finish")]
    Conclude {
        id: i64,
        /// Final findings (required - every conclusion must be recorded)
        #[arg(long)]
        findings: String,
    },
    /// Add a source URL with evidence
    AddSource {
        id: i64,
        #[arg(long)]
        url: String,
        /// Human-readable label for the source
        #[arg(long)]
        label: Option<String>,
        /// Notes about what this source shows
        #[arg(long)]
        notes: Option<String>,
    },
    /// List sources for an investigation
    Sources { id: i64 },
    /// Delete an investigation permanently
    Delete { id: i64 },
    /// Manage people linked to an investigation
    #[command(subcommand)]
    People(ResearchPeopleCmd),
    /// Manage projects linked to an investigation
    #[command(subcommand)]
    Projects(ResearchProjectsCmd),
}

#[derive(clap::Subcommand)]
pub enum ResearchProjectsCmd {
    /// Link a project to this investigation
    Add { id: i64, project_id: i64 },
    /// Remove a project from this investigation
    Remove { id: i64, project_id: i64 },
    /// List projects on this investigation
    List { id: i64 },
}

#[derive(clap::Subcommand)]
pub enum ResearchPeopleCmd {
    /// Link a person to this investigation
    Add { id: i64, person_id: i64 },
    /// Remove a person from this investigation
    Remove { id: i64, person_id: i64 },
    /// List people on this investigation
    List { id: i64 },
}

pub fn run(db: &Database, cmd: ResearchCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        ResearchCommand::Start { name, slug, plan } => {
            let id = db.investigation_start(&name, &slug, plan.as_deref())?;
            // Embed the plan immediately if one was provided
            if let Some(ref plan_text) = plan {
                embed_investigation(db, id, Some(plan_text), None)?;
            }
            let inv = db.investigation_get(id)?.unwrap();
            print_output(format, &inv, || {
                println!("Started investigation #{id}: {name}");
                println!("  slug: {slug}");
                println!("\nNext: add sources with `ol research add-source {id} --url <url>`");
                println!("      conclude with `ol research conclude {id} --findings <...>`");
            });
        }

        ResearchCommand::Get { id_or_slug } => {
            let inv = if let Ok(id) = id_or_slug.parse::<i64>() {
                db.investigation_get(id)?
            } else {
                db.investigation_get_by_slug(&id_or_slug)?
            };
            match inv {
                Some(inv) => {
                    let sources = db.investigation_sources(inv.id)?;
                    print_output(
                        format,
                        &serde_json::json!({"investigation": inv, "sources": sources}),
                        || {
                            print_investigation_detail(&inv);
                            if !sources.is_empty() {
                                println!("\nSources ({}):", sources.len());
                                for s in &sources {
                                    let label = s.label.as_deref().unwrap_or("source");
                                    println!("  - [{label}] {}", s.url);
                                    if let Some(n) = &s.notes {
                                        println!("    {n}");
                                    }
                                }
                            }
                        },
                    );
                }
                None => println!("Investigation '{}' not found", id_or_slug),
            }
        }

        ResearchCommand::List { status } => {
            let invs = db.investigation_list(Some(&status))?;
            print_output(format, &invs, || {
                if invs.is_empty() {
                    println!("No investigations.");
                } else {
                    for inv in &invs {
                        println!("{}", research_line(inv));
                    }
                    println!("\n{} investigations", invs.len());
                }
            });
        }

        ResearchCommand::Search { query } => {
            // FTS results first
            let fts_hits = db.investigation_search(&query)?;
            let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|i| i.id).collect();

            for inv in &fts_hits {
                println!("{}", research_line(inv));
                if let Some(f) = &inv.findings {
                    println!("  {}", truncate(f, 100));
                }
            }

            // Semantic results - appended, deduped against FTS hits
            let mut embedder = Embedder::new()?;
            if let Ok(query_emb) = embedder.embed_one(&query) {
                let similar = db.investigation_similar(&query_emb, 5).unwrap_or_default();
                let semantic: Vec<_> = similar
                    .iter()
                    .filter(|r| !fts_ids.contains(&r.investigation.id))
                    .collect();
                if !semantic.is_empty() {
                    if !fts_hits.is_empty() {
                        println!();
                    }
                    for r in &semantic {
                        println!(
                            "{} ({:.0}% match)",
                            research_line(&r.investigation),
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

        ResearchCommand::Update {
            id,
            name,
            plan,
            findings,
        } => {
            if db.investigation_update(
                id,
                name.as_deref(),
                plan.as_deref(),
                findings.as_deref(),
                None,
            )? {
                // Re-embed if plan or findings changed
                if plan.is_some() || findings.is_some() {
                    if let Some(inv) = db.investigation_get(id)? {
                        embed_investigation(db, id, inv.plan.as_deref(), inv.findings.as_deref())?;
                    }
                }
                println!("Updated investigation #{id}");
            } else {
                println!("Investigation #{id} not found or no changes");
            }
        }

        ResearchCommand::Reopen { id } => {
            if db.investigation_reopen(id)? {
                println!("Investigation #{id} reopened");
            } else {
                println!("Investigation #{id} not found");
            }
        }

        ResearchCommand::Conclude { id, findings } => {
            if db.investigation_conclude(id, &findings)? {
                if let Some(inv) = db.investigation_get(id)? {
                    embed_investigation(db, id, inv.plan.as_deref(), Some(&findings))?;
                    if format == OutputFormat::Json {
                        println!("{}", serde_json::to_string_pretty(&inv)?);
                    } else {
                        println!("Investigation #{id} concluded.");
                        println!(
                            "\nFindings recorded. Consider saving key conclusions as memories:"
                        );
                        println!("  ol memory set \"research/{id}/finding\" \"<key finding>\" --type project");
                    }
                }
            } else {
                println!("Investigation #{id} not found");
            }
        }

        ResearchCommand::AddSource {
            id,
            url,
            label,
            notes,
        } => {
            let source_id =
                db.investigation_add_source(id, &url, label.as_deref(), notes.as_deref())?;
            println!("Added source #{source_id} to investigation #{id}");
            if let Some(l) = &label {
                println!("  [{l}] {url}");
            } else {
                println!("  {url}");
            }
        }

        ResearchCommand::Sources { id } => {
            let sources = db.investigation_sources(id)?;
            print_output(format, &sources, || {
                if sources.is_empty() {
                    println!("No sources for investigation #{id}");
                } else {
                    for s in &sources {
                        let label = s.label.as_deref().unwrap_or("source");
                        println!("#{} [{label}] {}", s.id, s.url);
                        if let Some(n) = &s.notes {
                            println!("  {n}");
                        }
                    }
                }
            });
        }

        ResearchCommand::Delete { id } => {
            if db.investigation_delete(id)? {
                println!("Deleted investigation #{id}");
            } else {
                println!("Investigation #{id} not found");
            }
        }

        ResearchCommand::People(sub) => match sub {
            ResearchPeopleCmd::Add { id, person_id } => {
                db.investigation_link_person(id, person_id)?;
                println!("Linked person #{person_id} to investigation #{id}");
            }
            ResearchPeopleCmd::Remove { id, person_id } => {
                if db.investigation_unlink_person(id, person_id)? {
                    println!("Removed person #{person_id} from investigation #{id}");
                } else {
                    println!("Link not found");
                }
            }
            ResearchPeopleCmd::List { id } => {
                let people = db.investigation_people(id)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people on investigation #{id}");
                    } else {
                        for p in &people {
                            println!("#{} {}", p.id, p.name);
                        }
                    }
                });
            }
        },

        ResearchCommand::Projects(sub) => match sub {
            ResearchProjectsCmd::Add { id, project_id } => {
                db.investigation_link_project(id, project_id)?;
                println!("Linked project #{project_id} to investigation #{id}");
            }
            ResearchProjectsCmd::Remove { id, project_id } => {
                if db.investigation_unlink_project(id, project_id)? {
                    println!("Removed project #{project_id} from investigation #{id}");
                } else {
                    println!("Link not found");
                }
            }
            ResearchProjectsCmd::List { id } => {
                let projects = db.investigation_projects(id)?;
                print_output(format, &projects, || {
                    if projects.is_empty() {
                        println!("No projects on investigation #{id}");
                    } else {
                        for p in &projects {
                            println!("#{} {}", p.id, p.name);
                        }
                    }
                });
            }
        },
    }
    Ok(())
}

fn print_investigation_detail(inv: &crate::db::investigations::Investigation) {
    let checkbox = if inv.status == "concluded" {
        "[x]"
    } else {
        "[ ]"
    };
    println!("{checkbox} #{} {} [{}]", inv.id, inv.name, inv.slug);
    println!("  Status:  {}", inv.status);
    println!("  Started: {}", inv.created_at);
    if let Some(at) = &inv.concluded_at {
        println!("  Concluded: {at}");
    }
    if let Some(plan) = &inv.plan {
        println!("\nPlan:\n  {}", plan.trim().replace('\n', "\n  "));
    }
    if let Some(findings) = &inv.findings {
        println!("\nFindings:\n  {}", findings.trim().replace('\n', "\n  "));
    }
}

fn research_line(inv: &crate::db::investigations::Investigation) -> String {
    crate::cli::format::research_line(inv)
}

fn embed_investigation(
    db: &Database,
    id: i64,
    plan: Option<&str>,
    findings: Option<&str>,
) -> Result<()> {
    let text = [plan, findings]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.trim().is_empty() {
        return Ok(());
    }
    let emb_cfg = &settings().embed;
    let raw = chunk_text(&text, emb_cfg.chunk_size, emb_cfg.chunk_overlap);
    let mut embedder = Embedder::new()?;
    let texts: Vec<&str> = raw.iter().map(|s| s.as_str()).collect();
    let embeddings = embedder.embed_batch(&texts)?;
    let chunks: Vec<(String, Option<Vec<f32>>)> = raw
        .into_iter()
        .zip(embeddings.into_iter().map(Some))
        .collect();
    db.investigation_store_chunks(id, &chunks)?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
