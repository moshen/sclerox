use anyhow::Result;
use clap::Args;

use crate::db::Database;
use crate::output::{print_output, OutputFormat};
use crate::search::{global_search, SearchResult};

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
    /// Restrict to a specific repo by name (substring match)
    #[arg(long)]
    pub repo: Option<String>,
}

pub fn run(db: &Database, args: SearchArgs, format: OutputFormat) -> Result<()> {
    let mut results = global_search(db, &args.query)?;

    // Filter to a specific repo if requested
    if let Some(repo_filter) = &args.repo {
        let filter = repo_filter.to_lowercase();
        results.retain(|r| match r {
            SearchResult::Symbol { repo_name, .. } => repo_name.to_lowercase().contains(&filter),
            SearchResult::Repo { name, .. } => name.to_lowercase().contains(&filter),
            _ => true, // non-repo results pass through
        });
    }
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
                SearchResult::Todo {
                    id,
                    title,
                    status,
                    category,
                } => {
                    let cb = crate::cli::format::status_checkbox(status);
                    let id_col = format!("{:<5}", format!("#{id}"));
                    let cat_col = format!("{:<9}", format!("[{category}]"));
                    println!("[todo] {cb} {id_col} {cat_col} {title}");
                }
                SearchResult::Investigation {
                    id,
                    name,
                    slug,
                    status,
                    snippet,
                } => {
                    let cb = crate::cli::format::status_checkbox(status);
                    let id_col = format!("{:<5}", format!("#{id}"));
                    println!("[research] {cb} {id_col} {name}  ({slug})");
                    if !snippet.is_empty() {
                        println!("           {snippet}");
                    }
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
                SearchResult::Symbol {
                    repo_name,
                    kind,
                    name,
                    signature,
                    file_path,
                    start_line,
                } => {
                    let sig = signature.as_deref().unwrap_or(name);
                    println!("[symbol] [{kind}] {sig} ({repo_name}/{file_path}:{start_line})");
                }
            }
        }
        println!("\n{} results", results.len());
    });
    Ok(())
}
