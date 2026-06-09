use anyhow::Result;
use clap::Subcommand;

use crate::db::Database;

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Set or update a memory entry
    Set {
        key: String,
        value: String,
        #[arg(long, default_value = "general", value_parser = ["general","user","feedback","project","reference"])]
        r#type: String,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
    },
    /// Get a memory entry by key
    Get { key: String },
    /// List all memory entries
    List {
        #[arg(long, value_parser = ["general","user","feedback","project","reference"])]
        r#type: Option<String>,
    },
    /// Full-text search memory
    Search { query: String },
    /// Delete a memory entry
    Delete { key: String },
}

pub fn run(db: &Database, cmd: MemoryCommand) -> Result<()> {
    match cmd {
        MemoryCommand::Set {
            key,
            value,
            r#type,
            tags,
        } => {
            let tag_list: Option<Vec<String>> = tags
                .as_deref()
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());
            db.memory_set(&key, &value, &r#type, tag_list.as_deref())?;
            println!("Set: {key}");
        }

        MemoryCommand::Get { key } => match db.memory_get(&key)? {
            Some(entry) => {
                println!("Key:     {}", entry.key);
                println!("Value:   {}", entry.value);
                println!("Type:    {}", entry.memory_type);
                if let Some(tags) = &entry.tags {
                    println!("Tags:    {}", tags.join(", "));
                }
                println!("Updated: {}", entry.updated_at);
            }
            None => println!("Not found: {key}"),
        },

        MemoryCommand::List { r#type } => {
            let entries = db.memory_list(r#type.as_deref())?;
            if entries.is_empty() {
                println!("No entries.");
            } else {
                for e in &entries {
                    let tags = e
                        .tags
                        .as_ref()
                        .map(|t| format!(" [{}]", t.join(", ")))
                        .unwrap_or_default();
                    println!(
                        "[{}] {} - {}{}",
                        e.memory_type,
                        e.key,
                        truncate(&e.value, 60),
                        tags
                    );
                }
                println!("\n{} entries", entries.len());
            }
        }

        MemoryCommand::Search { query } => {
            let results = db.memory_search(&query)?;
            if results.is_empty() {
                println!("No matches for: {query}");
            } else {
                for e in &results {
                    println!("[{}] {} - {}", e.memory_type, e.key, truncate(&e.value, 80));
                }
                println!("\n{} results", results.len());
            }
        }

        MemoryCommand::Delete { key } => {
            if db.memory_delete(&key)? {
                println!("Deleted: {key}");
            } else {
                println!("Not found: {key}");
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
