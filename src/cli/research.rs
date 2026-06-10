use anyhow::Result;
use clap::Subcommand;

use crate::db::Database;
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum ResearchCommand {
    /// Start a new investigation
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
    /// Full-text search across all investigations (name, plan, findings)
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
    /// Link a person to an investigation
    LinkPerson { id: i64, person_id: i64 },
    /// Link a project to an investigation
    LinkProject { id: i64, project_id: i64 },
}

pub fn run(db: &Database, cmd: ResearchCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        ResearchCommand::Start { name, slug, plan } => {
            let id = db.investigation_start(&name, &slug, plan.as_deref())?;
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
                        println!("[{}] #{} {} ({})", inv.status, inv.id, inv.name, inv.slug);
                    }
                    println!("\n{} investigations", invs.len());
                }
            });
        }

        ResearchCommand::Search { query } => {
            let results = db.investigation_search(&query)?;
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No matches for: {query}");
                } else {
                    for inv in &results {
                        println!("[{}] #{} {}", inv.status, inv.id, inv.name);
                        if let Some(f) = &inv.findings {
                            println!("  {}", truncate(f, 100));
                        }
                    }
                }
            });
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
                if format == OutputFormat::Json {
                    if let Some(inv) = db.investigation_get(id)? {
                        println!("{}", serde_json::to_string_pretty(&inv)?);
                    }
                } else {
                    println!("Investigation #{id} concluded.");
                    println!("\nFindings recorded. Consider saving key conclusions as memories:");
                    println!("  ol memory set \"research/{id}/finding\" \"<key finding>\" --type project");
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

        ResearchCommand::LinkPerson { id, person_id } => {
            db.investigation_link_person(id, person_id)?;
            println!("Linked person #{person_id} to investigation #{id}");
        }

        ResearchCommand::LinkProject { id, project_id } => {
            db.investigation_link_project(id, project_id)?;
            println!("Linked project #{project_id} to investigation #{id}");
        }
    }
    Ok(())
}

fn print_investigation_detail(inv: &crate::db::investigations::Investigation) {
    let status_icon = match inv.status.as_str() {
        "concluded" => "✓",
        "active" => "→",
        _ => "○",
    };
    println!("{status_icon} #{} {} [{}]", inv.id, inv.name, inv.slug);
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
