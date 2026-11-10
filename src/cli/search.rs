use anyhow::Result;
use clap::Args;

use crate::cli::format::{
    code_chunk_lines, code_location, code_text_lines, dated_location, stamp, symbol_line,
};
use crate::db::Database;
use crate::output::{full, print_output, OutputFormat};
use crate::search::{order, search_hits, Hit, SearchResult};

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
    /// Show at most this many results (default: search.result_limit)
    #[arg(long, conflicts_with = "all")]
    pub limit: Option<usize>,
    /// Show every result, past the result limit and token budget
    #[arg(long)]
    pub all: bool,
}

pub fn run(db: &Database, args: SearchArgs, format: OutputFormat) -> Result<()> {
    let s = crate::config::settings();
    let hits = order(
        search_hits(
            db,
            &args.query,
            s.semantic_threshold() as f32,
            s.semantic_margin().map(|m| m as f32),
            s.search.semantic_limit,
            s.search.code_text_limit,
        )?,
        &args.query,
        s.search.ranking,
    );
    let found = hits.len();
    let hits = if args.all {
        hits
    } else {
        within(
            hits,
            args.limit.unwrap_or(s.search.result_limit),
            s.search.token_budget,
        )
    };
    let cut = (hits.len() < found).then(|| {
        format!(
            "Showing {} of {found} results; --limit N or --all shows more.",
            hits.len()
        )
    });
    let logged: Vec<_> = hits.iter().map(crate::cli::usage::hit_item).collect();
    let results: Vec<SearchResult> = hits.into_iter().map(|h| h.result).collect();
    let show = || {
        if results.is_empty() {
            println!("No matches for: {}", args.query);
            return;
        }
        for r in &results {
            println!("{}", render(r));
        }
        match &cut {
            Some(cut) => println!("\n{cut}"),
            None => println!("\n{} results", results.len()),
        }
    };
    let text = matches!(format, OutputFormat::Text);
    // JSON stays a list of hits, so the note goes to stderr.
    if let (Some(cut), false) = (&cut, text) {
        eprintln!("{cut}");
    }
    if full() {
        print_output(format, &whole_records(db, &results)?, show);
        // --full text prints the records in place of `show` and its note.
        if let (Some(cut), true, false) = (&cut, text, results.is_empty()) {
            println!("\n{cut}");
        }
    } else {
        print_output(format, &results, show);
    }
    crate::cli::usage::search(db, "search", &args.query, &logged);
    Ok(())
}

/// The best of `hits`, in order: at most `limit`, and only as many as fit in
/// about `budget` tokens of text output, though always the first.
fn within(hits: Vec<Hit>, limit: usize, budget: usize) -> Vec<Hit> {
    let mut spent = 0;
    hits.into_iter()
        .take(limit)
        .enumerate()
        .take_while(|(i, h)| {
            spent += crate::embed::tokens::estimate_tokens(&render(&h.result));
            *i == 0 || spent <= budget
        })
        .map(|(_, h)| h)
        .collect()
}

/// Each hit with the whole record it found, as that type's `get` command
/// shows it, for `--full`. A code hit has no record beyond the hit itself.
fn whole_records(db: &Database, results: &[SearchResult]) -> Result<Vec<serde_json::Value>> {
    results
        .iter()
        .map(|result| {
            let mut hit = serde_json::to_value(result)?;
            match record_fields(db, result) {
                Ok(serde_json::Value::Object(fields)) => {
                    if let Some(hit) = hit.as_object_mut() {
                        hit.extend(fields);
                    }
                }
                Ok(_) => {}
                Err(e) => log::warn!("could not load the record of a search hit: {e:#}"),
            }
            Ok(hit)
        })
        .collect()
}

/// A hit's stored fields, with the rows linked to it.
fn record_fields(db: &Database, result: &SearchResult) -> Result<serde_json::Value> {
    use serde_json::json;

    fn merged<T: serde::Serialize>(
        record: Option<T>,
        links: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut fields = serde_json::to_value(record)?;
        if let (Some(fields), serde_json::Value::Object(links)) = (fields.as_object_mut(), links) {
            fields.extend(links);
        }
        Ok(fields)
    }

    match result {
        SearchResult::Memory { id, .. } => {
            let detail = db
                .memory_get_by_id(*id)?
                .map(|entry| crate::cli::provenance::MemoryDetail::load(db, entry))
                .transpose()?;
            merged(detail, json!({}))
        }
        SearchResult::Person { id, .. } => merged(
            db.people_get(*id)?,
            json!({ "identifiers": db.people_identifiers_for(*id)? }),
        ),
        SearchResult::Meeting { id, .. } => merged(
            db.meeting_get(*id)?,
            json!({ "people": db.meeting_people(*id)? }),
        ),
        SearchResult::Project { id, .. } => merged(
            db.project_get(*id)?,
            json!({
                "people": db.project_people(*id)?,
                "memories": db.project_memory_keys(*id)?,
            }),
        ),
        SearchResult::Todo { id, .. } => merged(db.todo_get(*id)?, json!({})),
        SearchResult::Investigation { id, .. } => merged(
            db.investigation_get(*id)?,
            json!({ "sources": db.investigation_sources(*id)? }),
        ),
        SearchResult::Repo { path, .. } => merged(db.repo_get_by_path(path)?, json!({})),
        SearchResult::Symbol { .. }
        | SearchResult::CodeText { .. }
        | SearchResult::CodeChunk { .. } => Ok(serde_json::Value::Null),
    }
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
            last_modified,
            ..
        } => {
            let sig = signature.as_deref().unwrap_or(name);
            let loc = code_location(repo_path, file_path, Some(*start_line));
            format!(
                "[symbol] {}",
                symbol_line(kind, sig, &loc, last_modified.as_deref())
            )
        }
        SearchResult::CodeText {
            repo_path,
            file_path,
            line,
            text,
            last_modified,
            ..
        } => {
            let loc = dated_location(repo_path, file_path, Some(*line), last_modified.as_deref());
            format!("[code] {}", code_text_lines(&loc, text))
        }
        SearchResult::CodeChunk {
            repo_path,
            file_path,
            start_line,
            score,
            snippet,
            last_modified,
            ..
        } => {
            let loc = dated_location(repo_path, file_path, *start_line, last_modified.as_deref());
            format!("[code] {}", code_chunk_lines(*score, &loc, snippet))
        }
    }
}

#[cfg(test)]
#[path = "search_eval/mod.rs"]
mod eval;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::HitSource;

    fn hit(key: &str) -> Hit {
        Hit {
            score: None,
            source: HitSource::Keyword,
            result: SearchResult::Memory {
                id: 0,
                key: key.into(),
                snippet: "x".repeat(200),
                created_at: String::new(),
                updated_at: String::new(),
            },
        }
    }

    #[test]
    fn within_keeps_the_limit_and_the_token_budget_but_always_the_first() {
        let hits = || vec![hit("a"), hit("b"), hit("c")];
        let keys =
            |hits: Vec<Hit>| -> Vec<String> { hits.iter().map(|h| h.result.item().1).collect() };
        assert_eq!(keys(within(hits(), 2, 10_000)), ["a", "b"]);
        // Each hit renders to about 60 tokens.
        assert_eq!(keys(within(hits(), 10, 130)), ["a", "b"]);
        assert_eq!(keys(within(hits(), 10, 1)), ["a"]);
    }
}
