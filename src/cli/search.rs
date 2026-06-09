use anyhow::Result;
use clap::Args;

use crate::db::Database;
use crate::output::{print_output, OutputFormat};
use crate::search::{global_search, SearchResult};

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
}

pub fn run(db: &Database, args: SearchArgs, format: OutputFormat) -> Result<()> {
    let results = global_search(db, &args.query)?;
    print_output(format, &results, || {
        if results.is_empty() {
            println!("No matches for: {}", args.query);
            return;
        }
        for r in &results {
            match r {
                SearchResult::Memory { key, snippet, .. } => {
                    println!("[memory] {} - {}", key, snippet);
                }
                SearchResult::Person { id, name, email } => {
                    let email = email.as_deref().unwrap_or("-");
                    println!("[person] #{id} {name} <{email}>");
                }
                SearchResult::Meeting {
                    id,
                    title,
                    date,
                    snippet,
                } => {
                    let date = date.as_deref().unwrap_or("no date");
                    println!("[meeting] #{id} [{date}] {title}");
                    if !snippet.is_empty() {
                        println!("          {snippet}");
                    }
                }
                SearchResult::Project {
                    id,
                    name,
                    description,
                } => {
                    let desc = description.as_deref().unwrap_or("-");
                    println!("[project] #{id} {name} - {desc}");
                }
                SearchResult::Repo {
                    id,
                    name,
                    path,
                    description,
                } => {
                    let desc = description.as_deref().unwrap_or("-");
                    println!("[repo] #{id} {name} ({path}) - {desc}");
                }
            }
        }
        println!("\n{} results", results.len());
    });
    Ok(())
}
