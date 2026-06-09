use anyhow::Result;
use clap::Subcommand;

use crate::db::{people::PersonUpdate, Database};

#[derive(Subcommand)]
pub enum PeopleCommand {
    /// Add a person
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        slack_id: Option<String>,
        #[arg(long)]
        slack_url: Option<String>,
        #[arg(long)]
        github_username: Option<String>,
        #[arg(long)]
        github_url: Option<String>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Get a person by ID
    Get { id: i64 },
    /// List all people
    List,
    /// Full-text search people
    Search { query: String },
    /// Update a person's fields
    Update {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        github_username: Option<String>,
        #[arg(long)]
        slack_url: Option<String>,
    },
    /// Delete a person
    Delete { id: i64 },
}

pub fn run(db: &Database, cmd: PeopleCommand) -> Result<()> {
    match cmd {
        PeopleCommand::Add {
            name,
            email,
            slack_id,
            slack_url,
            github_username,
            github_url,
            notes,
        } => {
            let id = db.people_add(
                &name,
                email.as_deref(),
                slack_id.as_deref(),
                slack_url.as_deref(),
                github_username.as_deref(),
                github_url.as_deref(),
                notes.as_deref(),
            )?;
            println!("Added person #{id}: {name}");
        }

        PeopleCommand::Get { id } => match db.people_get(id)? {
            Some(p) => print_person(&p),
            None => println!("Person #{id} not found"),
        },

        PeopleCommand::List => {
            let people = db.people_list()?;
            if people.is_empty() {
                println!("No people recorded.");
            } else {
                for p in &people {
                    let email = p.email.as_deref().unwrap_or("-");
                    let github = p
                        .github_username
                        .as_deref()
                        .map(|g| format!(" @{g}"))
                        .unwrap_or_default();
                    println!("#{} {} <{}>{}", p.id, p.name, email, github);
                }
                println!("\n{} people", people.len());
            }
        }

        PeopleCommand::Search { query } => {
            let results = db.people_search(&query)?;
            if results.is_empty() {
                println!("No matches for: {query}");
            } else {
                for p in &results {
                    println!("#{} {}", p.id, p.name);
                }
            }
        }

        PeopleCommand::Update {
            id,
            name,
            email,
            notes,
            github_username,
            slack_url,
        } => {
            let update = PersonUpdate {
                name,
                email: email.map(Some),
                notes: notes.map(Some),
                github_username: github_username.map(Some),
                slack_url: slack_url.map(Some),
                ..Default::default()
            };
            if db.people_update(id, update)? {
                println!("Updated person #{id}");
            } else {
                println!("Person #{id} not found or no changes");
            }
        }

        PeopleCommand::Delete { id } => {
            if db.people_delete(id)? {
                println!("Deleted person #{id}");
            } else {
                println!("Person #{id} not found");
            }
        }
    }
    Ok(())
}

fn print_person(p: &crate::db::people::Person) {
    println!("ID:      #{}", p.id);
    println!("Name:    {}", p.name);
    println!("Email:   {}", p.email.as_deref().unwrap_or("-"));
    println!(
        "Slack:   {} {}",
        p.slack_id.as_deref().unwrap_or("-"),
        p.slack_url.as_deref().unwrap_or("")
    );
    println!(
        "GitHub:  {} {}",
        p.github_username.as_deref().unwrap_or("-"),
        p.github_url.as_deref().unwrap_or("")
    );
    if let Some(notes) = &p.notes {
        println!("Notes:   {notes}");
    }
    println!("Since:   {}", p.created_at);
}
