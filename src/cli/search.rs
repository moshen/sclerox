use anyhow::Result;
use clap::Args;

use crate::cli::format::{code_chunk_lines, code_location, code_text_lines, stamp, symbol_line};
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
        s.search.code_text_limit,
    )?;
    print_output(format, &results, || {
        if results.is_empty() {
            println!("No matches for: {}", args.query);
            return;
        }
        for r in &results {
            println!("{}", render(r));
        }
        println!("\n{} results", results.len());
    });
    Ok(())
}

/// One search hit as `sclerox search` prints it, one or more lines.
pub(crate) fn render(r: &SearchResult) -> String {
    match r {
        SearchResult::Memory {
            key,
            snippet,
            updated_at,
            ..
        } => {
            let at = stamp("updated", updated_at);
            format!("[memory] {key} - {snippet}  {at}")
        }
        SearchResult::Person {
            id,
            name,
            updated_at,
            ..
        } => {
            let at = stamp("updated", updated_at);
            format!("[person] #{id} {name}  {at}")
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
            let line = format!("[meeting] #{id} [{date}] {title}  {at}");
            if snippet.is_empty() {
                line
            } else {
                format!("{line}\n          {snippet}")
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
            format!("[project] #{id} {name} - {desc}  {at}")
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
            format!("[todo] {cb} {id_col} {cat_col} {title}  {at}")
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
            let line = format!("[research] {cb} {id_col} {name}  ({slug})  {at}");
            if snippet.is_empty() {
                line
            } else {
                format!("{line}\n           {snippet}")
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
            format!("[repo] #{id} {name} ({path}) - {desc}  {at}")
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
            format!("[symbol] {}", symbol_line(kind, sig, &loc))
        }
        SearchResult::CodeText {
            repo_path,
            file_path,
            line,
            text,
            ..
        } => {
            let loc = code_location(repo_path, file_path, Some(*line));
            format!("[code] {}", code_text_lines(&loc, text))
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
            format!("[code] {}", code_chunk_lines(*score, &loc, snippet))
        }
    }
}

#[cfg(test)]
#[path = "search_eval/mod.rs"]
mod eval;
