use anyhow::Result;
use clap::Subcommand;

use crate::db::{projects::ProjectLink, Database};

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// Add a project
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        /// Links in format "url" or "url|label", repeatable
        #[arg(long = "link")]
        links: Vec<String>,
    },
    /// Get a project by ID
    Get { id: i64 },
    /// List all projects
    List,
    /// Full-text search projects
    Search { query: String },
    /// Update a project
    Update {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Link a person to this project
    LinkPerson {
        project_id: i64,
        person_id: i64,
        #[arg(long)]
        role: Option<String>,
    },
    /// Link a meeting to this project
    LinkMeeting { project_id: i64, meeting_id: i64 },
    /// List people on a project
    People { project_id: i64 },
    /// List meetings on a project
    Meetings { project_id: i64 },
    /// Delete a project
    Delete { id: i64 },
}

pub fn run(db: &Database, cmd: ProjectCommand) -> Result<()> {
    match cmd {
        ProjectCommand::Add {
            name,
            description,
            links,
        } => {
            let parsed_links: Vec<ProjectLink> = links.iter().map(|s| parse_link(s)).collect();
            let id = db.project_add(&name, description.as_deref(), &parsed_links)?;
            println!("Added project #{id}: {name}");
        }

        ProjectCommand::Get { id } => match db.project_get(id)? {
            Some(p) => {
                println!("ID:          #{}", p.id);
                println!("Name:        {}", p.name);
                println!("Description: {}", p.description.as_deref().unwrap_or("-"));
                if !p.links.is_empty() {
                    println!("Links:");
                    for link in &p.links {
                        let label = link.label.as_deref().unwrap_or("link");
                        println!("  - {} ({})", link.url, label);
                    }
                }
                let people = db.project_people(id)?;
                if !people.is_empty() {
                    println!("People:");
                    for pp in &people {
                        let role = pp.role.as_deref().unwrap_or("member");
                        println!("  - #{} {} ({})", pp.person_id, pp.person_name, role);
                    }
                }
            }
            None => println!("Project #{id} not found"),
        },

        ProjectCommand::List => {
            let projects = db.project_list()?;
            if projects.is_empty() {
                println!("No projects.");
            } else {
                for p in &projects {
                    let desc = p
                        .description
                        .as_deref()
                        .map(|d| format!(" - {}", truncate(d, 60)))
                        .unwrap_or_default();
                    println!("#{} {}{}", p.id, p.name, desc);
                }
                println!("\n{} projects", projects.len());
            }
        }

        ProjectCommand::Search { query } => {
            let results = db.project_search(&query)?;
            if results.is_empty() {
                println!("No matches for: {query}");
            } else {
                for p in &results {
                    println!("#{} {}", p.id, p.name);
                }
            }
        }

        ProjectCommand::Update {
            id,
            name,
            description,
        } => {
            let updated = db.project_update(
                id,
                name.as_deref(),
                description.as_ref().map(|d| Some(d.as_str())),
                None,
            )?;
            if updated {
                println!("Updated project #{id}");
            } else {
                println!("Project #{id} not found or no changes");
            }
        }

        ProjectCommand::LinkPerson {
            project_id,
            person_id,
            role,
        } => {
            db.project_link_person(project_id, person_id, role.as_deref())?;
            println!("Linked person #{person_id} to project #{project_id}");
        }

        ProjectCommand::LinkMeeting {
            project_id,
            meeting_id,
        } => {
            db.project_link_meeting(project_id, meeting_id)?;
            println!("Linked meeting #{meeting_id} to project #{project_id}");
        }

        ProjectCommand::People { project_id } => {
            let people = db.project_people(project_id)?;
            if people.is_empty() {
                println!("No people on project #{project_id}");
            } else {
                for p in &people {
                    let role = p.role.as_deref().unwrap_or("member");
                    println!("#{} {} ({})", p.person_id, p.person_name, role);
                }
            }
        }

        ProjectCommand::Meetings { project_id } => {
            let meetings = db.project_meetings_list(project_id)?;
            if meetings.is_empty() {
                println!("No meetings linked to project #{project_id}");
            } else {
                for m in &meetings {
                    let date = m.meeting_date.as_deref().unwrap_or("no date");
                    println!("#{} [{}] {}", m.id, date, m.title);
                }
            }
        }

        ProjectCommand::Delete { id } => {
            if db.project_delete(id)? {
                println!("Deleted project #{id}");
            } else {
                println!("Project #{id} not found");
            }
        }
    }
    Ok(())
}

fn parse_link(s: &str) -> ProjectLink {
    if let Some((url, label)) = s.split_once('|') {
        ProjectLink {
            url: url.to_string(),
            label: Some(label.to_string()),
        }
    } else {
        ProjectLink {
            url: s.to_string(),
            label: None,
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
