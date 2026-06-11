use anyhow::Result;
use clap::Subcommand;

use crate::db::Database;
use crate::output::{print_output, OutputFormat};

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
    /// List memory entries (active only by default)
    List {
        #[arg(long, value_parser = ["general","user","feedback","project","reference","session"])]
        r#type: Option<String>,
        /// Include stale and superseded entries
        #[arg(long)]
        all: bool,
    },
    /// Full-text search memory (active only by default)
    Search {
        query: String,
        /// Also search stale and superseded entries
        #[arg(long)]
        all: bool,
    },
    /// Delete a memory entry permanently
    Delete { key: String },
    /// Mark a memory as stale (no longer reliable, but kept for history)
    Stale {
        key: String,
        /// Brief reason why this memory is stale
        #[arg(long)]
        reason: Option<String>,
    },
    /// Replace a memory with an updated version, marking the old one as superseded
    Supersede {
        /// Key of the memory being replaced
        old_key: String,
        /// Key for the new memory entry
        new_key: String,
        new_value: String,
        #[arg(long, default_value = "project", value_parser = ["general","user","feedback","project","reference","session"])]
        r#type: String,
    },
    /// Mark a memory as reviewed (you've confirmed it's still accurate)
    Review { key: String },
    /// List memories that haven't been reviewed recently
    NeedsReview {
        /// Flag memories not reviewed in this many days (default: 30)
        #[arg(long, default_value = "30")]
        days: u32,
    },
    /// Import memories from Claude Code's auto-memory markdown files
    ImportClaude {
        /// Override the default ~/.claude/projects path
        #[arg(long)]
        path: Option<String>,
        /// Dry run - show what would be imported without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage people linked to a memory entry
    #[command(subcommand)]
    People(MemoryPeopleCmd),
}

#[derive(clap::Subcommand)]
pub enum MemoryPeopleCmd {
    /// Link a person to a memory entry
    Add { key: String, person_id: i64 },
    /// Remove a person link from a memory entry
    Remove { key: String, person_id: i64 },
    /// List people linked to a memory entry
    List { key: String },
}

pub fn run(db: &Database, cmd: MemoryCommand, format: OutputFormat) -> Result<()> {
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
            Some(entry) => print_output(format, &entry, || {
                println!("Key:     {}", entry.key);
                println!("Value:   {}", entry.value);
                println!("Type:    {}", entry.memory_type);
                if let Some(tags) = &entry.tags {
                    println!("Tags:    {}", tags.join(", "));
                }
                println!("Updated: {}", entry.updated_at);
            }),
            None => println!("Not found: {key}"),
        },

        MemoryCommand::List { r#type, all } => {
            let status = if all { Some("all") } else { None };
            let entries = db.memory_list(r#type.as_deref(), status)?;
            print_output(format, &entries, || {
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
            });
        }

        MemoryCommand::Search { query, all } => {
            let results = if all {
                db.memory_search_filtered(&query, "all")?
            } else {
                db.memory_search(&query)?
            };
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No matches for: {query}");
                } else {
                    for e in &results {
                        println!("[{}] {} - {}", e.memory_type, e.key, truncate(&e.value, 80));
                    }
                    println!("\n{} results", results.len());
                }
            });
        }

        MemoryCommand::Delete { key } => {
            if db.memory_delete(&key)? {
                println!("Deleted: {key}");
            } else {
                println!("Not found: {key}");
            }
        }

        MemoryCommand::Stale { key, reason } => {
            if db.memory_stale(&key, reason.as_deref())? {
                println!("Marked stale: {key}");
            } else {
                println!("Not found or already stale: {key}");
            }
        }

        MemoryCommand::Supersede {
            old_key,
            new_key,
            new_value,
            r#type,
        } => {
            if db.memory_supersede(&old_key, &new_key, &new_value, &r#type)? {
                println!("Superseded '{old_key}' → '{new_key}'");
            } else {
                println!("Not found: {old_key}");
            }
        }

        MemoryCommand::Review { key } => {
            if db.memory_review(&key)? {
                println!("Marked reviewed: {key}");
            } else {
                println!("Not found: {key}");
            }
        }

        MemoryCommand::NeedsReview { days } => {
            let entries = db.memory_review_needed(days)?;
            print_output(format, &entries, || {
                if entries.is_empty() {
                    println!("All memories reviewed within {days} days.");
                } else {
                    for e in &entries {
                        let last = e.reviewed_at.as_deref().unwrap_or("never");
                        println!("[{}] {} (last reviewed: {})", e.memory_type, e.key, last);
                        println!("  {}", truncate(&e.value, 80));
                    }
                    println!("\n{} entries need review", entries.len());
                }
            });
        }

        MemoryCommand::ImportClaude { path, dry_run } => {
            import_claude_memories(db, path.as_deref(), dry_run)?;
        }

        MemoryCommand::People(sub) => match sub {
            MemoryPeopleCmd::Add { key, person_id } => {
                if db.memory_link_person(&key, person_id)? {
                    println!("Linked person #{person_id} to memory '{key}'");
                } else {
                    println!("Memory key '{key}' not found");
                }
            }
            MemoryPeopleCmd::Remove { key, person_id } => {
                if db.memory_unlink_person(&key, person_id)? {
                    println!("Removed person #{person_id} from memory '{key}'");
                } else {
                    println!("Link not found");
                }
            }
            MemoryPeopleCmd::List { key } => {
                let people = db.memory_people(&key)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people linked to memory '{key}'");
                    } else {
                        for p in &people {
                            let email = p.email.as_deref().unwrap_or("-");
                            println!("#{} {} <{}>", p.id, p.name, email);
                        }
                    }
                });
            }
        },
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

/// Parse a Claude Code auto-memory markdown file.
/// Returns (key_slug, value, memory_type) or None if unparseable.
fn parse_claude_memory_file(path: &std::path::Path) -> Option<(String, String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let slug = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Split on frontmatter delimiters
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    let (frontmatter, body) = if parts.len() >= 3 {
        (parts[1], parts[2].trim())
    } else {
        ("", content.trim())
    };

    // Extract type from frontmatter: "type: feedback" or "  type: project"
    let memory_type = frontmatter
        .lines()
        .find_map(|line| {
            let line = line.trim();
            // Handle both top-level and nested under metadata:
            line.strip_prefix("type:")
                .map(|rest| rest.trim().to_string())
        })
        .unwrap_or_else(|| "project".to_string());

    // Use body as value; fall back to description from frontmatter
    let value = if body.is_empty() {
        frontmatter
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("description:")
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| slug.clone())
    } else {
        body.to_string()
    };

    // Skip empty or index files
    if value.is_empty() || slug == "MEMORY" {
        return None;
    }

    Some((slug, value, memory_type))
}

fn import_claude_memories(
    db: &crate::db::Database,
    base_path: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let search_root = if let Some(p) = base_path {
        std::path::PathBuf::from(p)
    } else {
        dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home directory"))?
            .join(".claude")
            .join("projects")
    };

    if !search_root.exists() {
        println!(
            "No Claude projects directory found at {}",
            search_root.display()
        );
        return Ok(());
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;

    // Walk ~/.claude/projects/*/memory/*.md
    for project_entry in std::fs::read_dir(&search_root)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", search_root.display()))?
        .flatten()
    {
        let memory_dir = project_entry.path().join("memory");
        if !memory_dir.is_dir() {
            continue;
        }

        for file_entry in std::fs::read_dir(&memory_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let Some((key, value, memory_type)) = parse_claude_memory_file(&path) else {
                continue;
            };

            if dry_run {
                println!("[would import] {key} ({memory_type})");
                println!("  {}", truncate(&value, 80));
                imported += 1;
            } else {
                // INSERT OR IGNORE - don't overwrite existing manual memories
                let existing = db.memory_get(&key)?;
                if existing.is_some() {
                    skipped += 1;
                    continue;
                }
                db.memory_set_full(&key, &value, &memory_type, None, "claude-auto")?;
                imported += 1;
            }
        }
    }

    if dry_run {
        println!("\n{imported} entries would be imported (dry-run: nothing written)");
    } else {
        println!("Imported {imported} entries, skipped {skipped} (already exist)");
        if imported > 0 {
            println!("Run `ol memory list` to see imported entries.");
        }
    }
    Ok(())
}
