use anyhow::{bail, Result};
use clap::Subcommand;

use crate::db::{people::PersonUpdate, Database};
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum PeopleCommand {
    /// Add a person
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        notes: Option<String>,
        /// Add a known identifier shorthand: email, github, slack, atlassian
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        github: Option<String>,
        #[arg(long)]
        slack: Option<String>,
        #[arg(long)]
        atlassian: Option<String>,
        /// Add any identifier as type:value (e.g. --id linkedin:alice)
        #[arg(long = "id")]
        identifiers: Vec<String>,
    },
    /// Get a person by ID
    Get { id: i64 },
    /// List all people
    List,
    /// Search people by name, notes, or any identifier value
    Search { query: String },
    /// Update a person's name or notes
    Update {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Delete a person
    Delete { id: i64 },
    /// Manage identifiers for a person (email, slack, github, etc.)
    #[command(subcommand)]
    Identifier(IdentifierCmd),
    /// Manage identifier types (the catalog of valid types)
    #[command(subcommand)]
    Types(IdentifierTypeCmd),
}

#[derive(Subcommand)]
pub enum IdentifierCmd {
    /// Set (add or update) an identifier for a person
    Add {
        person_id: i64,
        /// Identifier type (must exist in `ol people types list`)
        #[arg(value_name = "TYPE")]
        identifier_type: String,
        /// The identifier value
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Remove an identifier from a person
    Remove {
        person_id: i64,
        #[arg(value_name = "TYPE")]
        identifier_type: String,
    },
    /// List all identifiers for a person
    List { person_id: i64 },
}

#[derive(Subcommand)]
pub enum IdentifierTypeCmd {
    /// List all valid identifier types
    List,
    /// Add a new identifier type
    Add {
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
}

pub fn run(db: &Database, cmd: PeopleCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        PeopleCommand::Add {
            name,
            notes,
            email,
            github,
            slack,
            atlassian,
            identifiers,
        } => {
            let id = db.people_add(&name, notes.as_deref())?;

            // Shorthand flags
            if let Some(v) = email {
                db.people_identifier_set(id, "email", &v)?;
            }
            if let Some(v) = github {
                db.people_identifier_set(id, "github", &v)?;
            }
            if let Some(v) = slack {
                db.people_identifier_set(id, "slack", &v)?;
            }
            if let Some(v) = atlassian {
                db.people_identifier_set(id, "atlassian", &v)?;
            }
            // Generic --id type:value pairs
            for raw in &identifiers {
                let (t, v) = parse_identifier_arg(raw)?;
                db.people_identifier_set(id, t, v)?;
            }

            println!("Added person #{id}: {name}");
        }

        PeopleCommand::Get { id } => match db.people_get(id)? {
            Some(p) => {
                let idents = db.people_identifiers_for(id)?;
                print_output(
                    format,
                    &serde_json::json!({"person": p, "identifiers": idents}),
                    || print_person(&p, &idents),
                );
            }
            None => println!("Person #{id} not found"),
        },

        PeopleCommand::List => {
            let people = db.people_list()?;
            print_output(format, &people, || {
                if people.is_empty() {
                    println!("No people recorded.");
                } else {
                    for p in &people {
                        let idents = db.people_identifiers_for(p.id).unwrap_or_default();
                        let hint = idents
                            .iter()
                            .find(|i| i.identifier_type == "email")
                            .or_else(|| idents.first())
                            .map(|i| format!(" <{}>", i.identifier))
                            .unwrap_or_default();
                        println!("#{} {}{}", p.id, p.name, hint);
                    }
                    println!("\n{} people", people.len());
                }
            });
        }

        PeopleCommand::Search { query } => {
            let results = db.people_search(&query)?;
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No matches for: {query}");
                } else {
                    for p in &results {
                        println!("#{} {}", p.id, p.name);
                    }
                }
            });
        }

        PeopleCommand::Update { id, name, notes } => {
            let update = PersonUpdate {
                name,
                notes: notes.map(Some),
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

        PeopleCommand::Identifier(sub) => match sub {
            IdentifierCmd::Add {
                person_id,
                identifier_type,
                value,
            } => {
                if !db.identifier_type_exists(&identifier_type)? {
                    bail!(
                        "Unknown identifier type '{identifier_type}'. \
                         Run `ol people types list` to see valid types, \
                         or `ol people types add {identifier_type}` to register it."
                    );
                }
                db.people_identifier_set(person_id, &identifier_type, &value)?;
                println!("Set {identifier_type}={value} on person #{person_id}");
            }
            IdentifierCmd::Remove {
                person_id,
                identifier_type,
            } => {
                if db.people_identifier_remove(person_id, &identifier_type)? {
                    println!("Removed {identifier_type} from person #{person_id}");
                } else {
                    println!("No {identifier_type} identifier found on person #{person_id}");
                }
            }
            IdentifierCmd::List { person_id } => {
                let idents = db.people_identifiers_for(person_id)?;
                if idents.is_empty() {
                    println!("No identifiers for person #{person_id}");
                } else {
                    for i in &idents {
                        println!("  {}: {}", i.identifier_type, i.identifier);
                    }
                }
            }
        },

        PeopleCommand::Types(sub) => match sub {
            IdentifierTypeCmd::List => {
                let types = db.identifier_types_list()?;
                for t in &types {
                    let desc = t.description.as_deref().unwrap_or("");
                    println!("  {}  {}", t.name, desc);
                }
            }
            IdentifierTypeCmd::Add { name, description } => {
                if db.identifier_type_exists(&name)? {
                    println!("Type '{name}' already exists");
                } else {
                    db.identifier_type_add(&name, description.as_deref())?;
                    println!("Added identifier type '{name}'");
                }
            }
        },
    }
    Ok(())
}

fn print_person(p: &crate::db::people::Person, idents: &[crate::db::people::PersonIdentifier]) {
    println!("ID:    #{}", p.id);
    println!("Name:  {}", p.name);
    if !idents.is_empty() {
        println!("Identifiers:");
        for i in idents {
            println!("  {}: {}", i.identifier_type, i.identifier);
        }
    }
    if let Some(notes) = &p.notes {
        println!("Notes: {notes}");
    }
    println!("Since: {}", p.created_at);
}

/// Parse "type:value" from a --id argument.
fn parse_identifier_arg(raw: &str) -> Result<(&str, &str)> {
    match raw.split_once(':') {
        Some((t, v)) if !t.is_empty() && !v.is_empty() => Ok((t, v)),
        _ => bail!("--id must be type:value (e.g. --id linkedin:alice), got: '{raw}'"),
    }
}
