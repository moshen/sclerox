use anyhow::Result;
use clap::Subcommand;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use crate::db::Database;
use crate::index::repo_db::RepoDb;

#[derive(Subcommand)]
pub enum CodeCommand {
    /// Search symbols and code across all indexed repos
    Search {
        /// Symbol name, class, or keyword to look for
        needle: String,
        /// Restrict to repos whose name contains this string
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show what a symbol calls / inherits / implements
    Calls {
        symbol: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show what calls / uses a symbol (reverse lookup)
    Refs {
        symbol: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// BFS call graph from a seed symbol
    Graph {
        symbol: String,
        #[arg(long)]
        repo: Option<String>,
        /// Maximum traversal depth (default: 3)
        #[arg(long, default_value = "3")]
        depth: usize,
    },
}

pub fn run(db: &Database, cmd: CodeCommand) -> Result<()> {
    match cmd {
        CodeCommand::Search { needle, repo } => {
            let repos = db.repo_list()?;
            if repos.is_empty() {
                println!("No repos indexed. Run `sclerox repo index [path]` first.");
                return Ok(());
            }

            // Embed the query once for the semantic tier (best-effort - skipped
            // if the model is unavailable). FTS symbol search always runs.
            let query_emb = crate::embed::Embedder::new()
                .ok()
                .and_then(|mut e| e.embed_one(&needle).ok());
            let sem = &crate::config::settings().search;
            let floor = sem.semantic_threshold as f32;

            let mut any = false;
            let mut code_chunks: Vec<(f32, String, crate::index::repo_db::SimilarChunk)> =
                Vec::new();
            for entry in &repos {
                if let Some(filter) = &repo {
                    if !entry.name.to_lowercase().contains(&filter.to_lowercase()) {
                        continue;
                    }
                }

                let db_path = PathBuf::from(&entry.db_path);
                if !db_path.exists() {
                    continue;
                }
                let repo_db = match RepoDb::open(&db_path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                for s in repo_db.search_symbols(&needle)? {
                    let sig = s.signature.as_deref().unwrap_or(&s.name);
                    println!(
                        "[{}] {} ({}/{}:{})",
                        s.kind, sig, entry.name, s.file_path, s.start_line
                    );
                    any = true;
                }

                if let Some(ref qe) = query_emb {
                    for c in repo_db
                        .similar_chunks(qe, sem.semantic_limit)
                        .unwrap_or_default()
                    {
                        if c.score >= floor {
                            code_chunks.push((c.score, entry.name.clone(), c));
                        }
                    }
                }
            }

            // Rank semantic code-chunk matches across all repos; keep top N.
            code_chunks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            code_chunks.truncate(sem.semantic_limit);
            if !code_chunks.is_empty() {
                println!("\nSemantic matches:");
                for (score, rname, c) in code_chunks {
                    let line = c.start_line.map(|l| format!(":{l}")).unwrap_or_default();
                    let snippet: String = c.chunk_text.trim().chars().take(100).collect();
                    println!("  ({score:.2}) {}/{}{}", rname, c.file_path, line);
                    println!("        {snippet}");
                    any = true;
                }
            }

            if !any {
                let scope = repo
                    .as_deref()
                    .map(|r| format!(" in repos matching '{r}'"))
                    .unwrap_or_default();
                println!("No matches for '{needle}'{scope}");
            }
        }

        CodeCommand::Calls { symbol, repo } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                println!("No repos indexed. Run `sclerox repo index [path]` first.");
                return Ok(());
            }

            let mut any = false;
            for (entry_name, repo_db) in &repos {
                // Find the symbol
                let syms = repo_db.symbol_by_name(&symbol)?;
                for sym in &syms {
                    let callees = repo_db.callees(sym.id)?;
                    if callees.is_empty() {
                        continue;
                    }
                    let sig = sym.signature.as_deref().unwrap_or(&sym.name);
                    println!(
                        "{} ({}/{}:{})",
                        sig, entry_name, sym.file_path, sym.start_line
                    );
                    for (to_name, kind, line, confidence) in &callees {
                        let resolved = repo_db
                            .symbol_by_name(to_name)
                            .ok()
                            .and_then(|v| v.into_iter().next());
                        let conf_tag = confidence_tag(confidence);
                        if let Some(target) = resolved {
                            println!(
                                "  {} {}{} ({}:{})",
                                kind_arrow(kind),
                                to_name,
                                conf_tag,
                                target.file_path,
                                target.start_line
                            );
                        } else {
                            println!(
                                "  {} {}{} [unresolved, line {}]",
                                kind_arrow(kind),
                                to_name,
                                conf_tag,
                                line
                            );
                        }
                    }
                    any = true;
                }
            }

            if !any {
                println!("Symbol '{symbol}' not found or has no recorded outbound edges.");
                println!("Tip: re-index with `sclerox repo index` to capture call graph edges.");
            }
        }

        CodeCommand::Refs { symbol, repo } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                println!("No repos indexed. Run `sclerox repo index [path]` first.");
                return Ok(());
            }

            let mut any = false;
            for (entry_name, repo_db) in &repos {
                let callers = repo_db.callers(&symbol)?;
                for (caller_sym, kind, _line, confidence) in &callers {
                    let sig = caller_sym.signature.as_deref().unwrap_or(&caller_sym.name);
                    let conf_tag = confidence_tag(confidence);
                    println!(
                        "  {} {}{} ({}/{}:{})",
                        kind_arrow(kind),
                        sig,
                        conf_tag,
                        entry_name,
                        caller_sym.file_path,
                        caller_sym.start_line
                    );
                    any = true;
                }
            }

            if !any {
                println!("No recorded references to '{symbol}'.");
                println!("Tip: re-index with `sclerox repo index` to capture call graph edges.");
            }
        }

        CodeCommand::Graph {
            symbol,
            repo,
            depth: max_depth,
        } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                println!("No repos indexed. Run `sclerox repo index [path]` first.");
                return Ok(());
            }

            // BFS: (name, depth, edge_kind_from_parent)
            let mut queue: VecDeque<(String, usize, String)> = VecDeque::new();
            queue.push_back((symbol.clone(), 0, String::new()));

            let mut visited: HashSet<String> = HashSet::new();
            let mut found_seed = false;

            while let Some((name, depth, edge_kind)) = queue.pop_front() {
                if visited.contains(&name) {
                    continue;
                }
                visited.insert(name.clone());

                // Look up symbol info across all repos
                let sym_info = find_across_repos(&repos, &name);

                let indent = "  ".repeat(depth);
                if depth == 0 {
                    match sym_info.first() {
                        Some((repo_name, sym)) => {
                            println!(
                                "{} [{}] ({}/{}:{})",
                                sym.name, sym.kind, repo_name, sym.file_path, sym.start_line
                            );
                            found_seed = true;
                        }
                        None => {
                            println!("{} [not indexed]", name);
                            break;
                        }
                    }
                } else {
                    match sym_info.first() {
                        Some((repo_name, sym)) => {
                            println!(
                                "{}  {} {} [{}] ({}/{}:{})",
                                indent,
                                kind_arrow(&edge_kind),
                                sym.name,
                                sym.kind,
                                repo_name,
                                sym.file_path,
                                sym.start_line
                            );
                        }
                        None => {
                            println!(
                                "{}  {} {} [unresolved]",
                                indent,
                                kind_arrow(&edge_kind),
                                name
                            );
                            continue; // can't follow edges from unresolved
                        }
                    }
                }

                if depth < max_depth {
                    // Collect outbound edges from all repos that have this symbol
                    for (_repo_name, sym) in &sym_info {
                        // Find the repo_db that owns this symbol
                        for (_rname, rdb) in &repos {
                            if let Ok(callees) = rdb.callees(sym.id) {
                                for (to_name, kind, _, _confidence) in callees {
                                    if !visited.contains(&to_name) {
                                        queue.push_back((to_name, depth + 1, kind));
                                    }
                                }
                                break; // symbol found in this repo, don't double-count
                            }
                        }
                    }
                }
            }

            if !found_seed && queue.is_empty() {
                println!("Symbol '{symbol}' not found in any indexed repo.");
                println!("Tip: re-index with `sclerox repo index` to capture call graph edges.");
            }
        }
    }
    Ok(())
}

/// Open all repo DBs, optionally filtered by name.
fn open_matching_repos(db: &Database, repo_filter: Option<&str>) -> Result<Vec<(String, RepoDb)>> {
    let entries = db.repo_list()?;
    let mut result = Vec::new();
    for entry in entries {
        if let Some(filter) = repo_filter {
            if !entry.name.to_lowercase().contains(&filter.to_lowercase()) {
                continue;
            }
        }
        let db_path = PathBuf::from(&entry.db_path);
        if !db_path.exists() {
            continue;
        }
        if let Ok(rdb) = RepoDb::open(&db_path) {
            result.push((entry.name, rdb));
        }
    }
    Ok(result)
}

/// Find a symbol by exact name across a set of open repo DBs.
fn find_across_repos<'a>(
    repos: &'a [(String, RepoDb)],
    name: &str,
) -> Vec<(&'a str, crate::index::repo_db::Symbol)> {
    let mut results = Vec::new();
    for (repo_name, rdb) in repos {
        if let Ok(syms) = rdb.symbol_by_name(name) {
            for sym in syms {
                results.push((repo_name.as_str(), sym));
            }
        }
    }
    results
}

fn kind_arrow(kind: &str) -> &str {
    match kind {
        "calls" => "→",
        "inherits" => "⊂",
        "implements" => "⊃",
        _ => "→",
    }
}

/// Returns a display tag for non-default confidence values.
/// "extracted" is the default and adds no noise; "inferred" (future) is flagged.
fn confidence_tag(confidence: &str) -> &str {
    match confidence {
        "extracted" | "" => "",
        "inferred" => " [inferred]",
        _ => " [?]",
    }
}
