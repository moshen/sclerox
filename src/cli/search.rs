use anyhow::Result;
use clap::Args;

use crate::cli::format::{code_chunk_lines, code_location, stamp, symbol_line};
use crate::db::Database;
use crate::output::{print_output, OutputFormat};
use crate::search::{global_search, SearchResult};

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
}

pub fn run(db: &Database, args: SearchArgs, format: OutputFormat) -> Result<()> {
    let s = crate::config::settings();
    let results = global_search(
        db,
        &args.query,
        s.semantic_threshold() as f32,
        s.semantic_margin().map(|m| m as f32),
        s.search.semantic_limit,
    )?;
    print_output(format, &results, || {
        if results.is_empty() {
            println!("No matches for: {}", args.query);
            return;
        }
        for r in &results {
            match r {
                SearchResult::Memory {
                    key,
                    snippet,
                    updated_at,
                    ..
                } => {
                    let at = stamp("updated", updated_at);
                    println!("[memory] {key} - {snippet}  {at}");
                }
                SearchResult::Person {
                    id,
                    name,
                    updated_at,
                    ..
                } => {
                    let at = stamp("updated", updated_at);
                    println!("[person] #{id} {name}  {at}");
                }
                SearchResult::Meeting {
                    id,
                    title,
                    date,
                    snippet,
                    created_at,
                } => {
                    let date = date.as_deref().unwrap_or("no date");
                    let at = stamp("created", created_at);
                    println!("[meeting] #{id} [{date}] {title}  {at}");
                    if !snippet.is_empty() {
                        println!("          {snippet}");
                    }
                }
                SearchResult::Project {
                    id,
                    name,
                    description,
                    updated_at,
                    ..
                } => {
                    let desc = description.as_deref().unwrap_or("-");
                    let at = stamp("updated", updated_at);
                    println!("[project] #{id} {name} - {desc}  {at}");
                }
                SearchResult::Todo {
                    id,
                    title,
                    status,
                    category,
                    updated_at,
                    ..
                } => {
                    let cb = crate::cli::format::status_checkbox(status);
                    let id_col = format!("{:<5}", format!("#{id}"));
                    let cat_col = format!("{:<9}", format!("[{category}]"));
                    let at = stamp("updated", updated_at);
                    println!("[todo] {cb} {id_col} {cat_col} {title}  {at}");
                }
                SearchResult::Investigation {
                    id,
                    name,
                    slug,
                    status,
                    snippet,
                    updated_at,
                    ..
                } => {
                    let cb = crate::cli::format::status_checkbox(status);
                    let id_col = format!("{:<5}", format!("#{id}"));
                    let at = stamp("updated", updated_at);
                    println!("[research] {cb} {id_col} {name}  ({slug})  {at}");
                    if !snippet.is_empty() {
                        println!("           {snippet}");
                    }
                }
                SearchResult::Repo {
                    id,
                    name,
                    path,
                    description,
                    created_at,
                } => {
                    let desc = description.as_deref().unwrap_or("-");
                    let at = stamp("created", created_at);
                    println!("[repo] #{id} {name} ({path}) - {desc}  {at}");
                }
                SearchResult::Symbol {
                    repo_path,
                    kind,
                    name,
                    signature,
                    file_path,
                    start_line,
                    ..
                } => {
                    let sig = signature.as_deref().unwrap_or(name);
                    let loc = code_location(repo_path, file_path, Some(*start_line));
                    println!("[symbol] {}", symbol_line(kind, sig, &loc));
                }
                SearchResult::CodeChunk {
                    repo_path,
                    file_path,
                    start_line,
                    score,
                    snippet,
                    ..
                } => {
                    let loc = code_location(repo_path, file_path, *start_line);
                    println!("[code] {}", code_chunk_lines(*score, &loc, snippet));
                }
            }
        }
        println!("\n{} results", results.len());
    });
    Ok(())
}

#[cfg(test)]
#[path = "search_live_eval.rs"]
mod live_eval;
