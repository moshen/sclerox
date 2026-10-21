use anyhow::Result;
use clap::Subcommand;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use crate::cli::format::{code_chunk_lines, code_location, symbol_line};
use crate::db::repos::RepoEntry;
use crate::db::Database;
use crate::index::repo_db::{RepoDb, SimilarChunk, Symbol};
use crate::output::{print_output, OutputFormat};
use crate::search::SearchResult;

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

pub fn run(db: &Database, cmd: CodeCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        CodeCommand::Search { needle, repo } => {
            db.repo_prune_gone()?;
            let repos = db.repo_list()?;
            if repos.is_empty() {
                print_output(format, &Vec::<SearchResult>::new(), || {
                    println!("No repos indexed. Run `sclerox repo index [path]` first.")
                });
                return Ok(());
            }

            // Embed the query once for the semantic tier (best-effort - skipped
            // if embeddings are off or the model is unavailable; each repo DB
            // checks its own model). FTS symbol search always runs.
            let query_emb =
                crate::embed::optional(true).and_then(|mut e| e.embed_query(&needle).ok());
            let sem = &crate::config::settings().search;
            let floor = crate::config::settings().semantic_threshold() as f32;

            // Results as `sclerox search` emits them: symbols, then the
            // semantic code-chunk matches.
            let mut results: Vec<SearchResult> = Vec::new();
            let mut code_chunks: Vec<(&RepoEntry, SimilarChunk)> = Vec::new();
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
                    results.push(SearchResult::Symbol {
                        repo_name: entry.name.clone(),
                        repo_path: entry.path.clone(),
                        kind: s.kind,
                        name: s.name,
                        signature: s.signature,
                        file_path: s.file_path,
                        start_line: s.start_line,
                    });
                }

                if let Some(ref qe) = query_emb {
                    for c in repo_db
                        .similar_chunks(qe, sem.semantic_limit)
                        .unwrap_or_default()
                    {
                        if c.score >= floor {
                            code_chunks.push((entry, c));
                        }
                    }
                }
            }

            // Rank semantic code-chunk matches across all repos; keep top N,
            // dropping any too far below the best of them.
            let margin = crate::config::settings()
                .semantic_margin()
                .map(|m| m as f32);
            let cut = crate::embed::models::semantic_cutoff(
                code_chunks.iter().map(|(_, c)| c.score),
                floor,
                margin,
            );
            code_chunks.retain(|(_, c)| c.score >= cut);
            code_chunks.sort_by(|a, b| {
                b.1.score
                    .partial_cmp(&a.1.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            code_chunks.truncate(sem.semantic_limit);
            results.extend(
                code_chunks
                    .into_iter()
                    .map(|(entry, c)| SearchResult::CodeChunk {
                        repo_name: entry.name.clone(),
                        repo_path: entry.path.clone(),
                        file_path: c.file_path,
                        start_line: c.start_line,
                        score: c.score,
                        snippet: c.chunk_text,
                    }),
            );

            print_output(format, &results, || {
                let mut semantic_header = false;
                for r in &results {
                    match r {
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
                            println!("{}", symbol_line(kind, sig, &loc));
                        }
                        SearchResult::CodeChunk {
                            repo_path,
                            file_path,
                            start_line,
                            score,
                            snippet,
                            ..
                        } => {
                            if !semantic_header {
                                println!("\nSemantic matches:");
                                semantic_header = true;
                            }
                            let loc = code_location(repo_path, file_path, *start_line);
                            println!("  {}", code_chunk_lines(*score, &loc, snippet));
                        }
                        _ => {}
                    }
                }
                if results.is_empty() {
                    let scope = repo
                        .as_deref()
                        .map(|r| format!(" in repos matching '{r}'"))
                        .unwrap_or_default();
                    println!("No matches for '{needle}'{scope}");
                }
            });
        }

        CodeCommand::Calls { symbol, repo } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                no_repos(format);
                return Ok(());
            }

            let mut found: Vec<SymbolEdges> = Vec::new();
            for (root, repo_db) in &repos {
                // Find the symbol
                let syms = repo_db.symbol_by_name(&symbol)?;
                for sym in syms {
                    let callees = repo_db.callees(sym.id)?;
                    if callees.is_empty() {
                        continue;
                    }
                    let edges = callees
                        .into_iter()
                        .map(|(to_name, kind, line, confidence)| {
                            let target = repo_db
                                .symbol_by_name(&to_name)
                                .ok()
                                .and_then(|v| v.into_iter().next());
                            Edge {
                                name: to_name,
                                kind,
                                line,
                                confidence,
                                symbol: target,
                            }
                        })
                        .collect();
                    found.push(SymbolEdges {
                        repo_path: root.clone(),
                        symbol: sym,
                        edges,
                    });
                }
            }

            print_output(format, &found, || {
                for f in &found {
                    let sig = f.symbol.signature.as_deref().unwrap_or(&f.symbol.name);
                    let loc =
                        code_location(&f.repo_path, &f.symbol.file_path, Some(f.symbol.start_line));
                    println!("{sig} ({loc})");
                    for e in &f.edges {
                        let conf_tag = confidence_tag(&e.confidence);
                        if let Some(target) = &e.symbol {
                            let loc = code_location(
                                &f.repo_path,
                                &target.file_path,
                                Some(target.start_line),
                            );
                            println!("  {} {}{} ({loc})", kind_arrow(&e.kind), e.name, conf_tag);
                        } else {
                            println!(
                                "  {} {}{} [unresolved, line {}]",
                                kind_arrow(&e.kind),
                                e.name,
                                conf_tag,
                                e.line
                            );
                        }
                    }
                }
                if found.is_empty() {
                    println!("Symbol '{symbol}' not found or has no recorded outbound edges.");
                    println!(
                        "Tip: re-index with `sclerox repo index` to capture call graph edges."
                    );
                }
            });
        }

        CodeCommand::Refs { symbol, repo } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                no_repos(format);
                return Ok(());
            }

            let mut refs: Vec<Reference> = Vec::new();
            for (root, repo_db) in &repos {
                for (caller, kind, line, confidence) in repo_db.callers(&symbol)? {
                    refs.push(Reference {
                        repo_path: root.clone(),
                        kind,
                        line,
                        confidence,
                        caller,
                    });
                }
            }

            print_output(format, &refs, || {
                for r in &refs {
                    let sig = r.caller.signature.as_deref().unwrap_or(&r.caller.name);
                    let conf_tag = confidence_tag(&r.confidence);
                    let loc =
                        code_location(&r.repo_path, &r.caller.file_path, Some(r.caller.start_line));
                    println!("  {} {}{} ({loc})", kind_arrow(&r.kind), sig, conf_tag);
                }
                if refs.is_empty() {
                    println!("No recorded references to '{symbol}'.");
                    println!(
                        "Tip: re-index with `sclerox repo index` to capture call graph edges."
                    );
                }
            });
        }

        CodeCommand::Graph {
            symbol,
            repo,
            depth: max_depth,
        } => {
            let repos = open_matching_repos(db, repo.as_deref())?;
            if repos.is_empty() {
                no_repos(format);
                return Ok(());
            }

            let nodes = call_graph(&repos, &symbol, max_depth);
            let found_seed = nodes.first().is_some_and(|n| n.symbol.is_some());

            print_output(format, &nodes, || {
                for n in &nodes {
                    let indent = "  ".repeat(n.depth);
                    match (n.depth, &n.repo_path, &n.symbol) {
                        (0, Some(root), Some(sym)) => {
                            let loc = code_location(root, &sym.file_path, Some(sym.start_line));
                            println!("{} [{}] ({loc})", sym.name, sym.kind);
                        }
                        (0, _, _) => println!("{} [not indexed]", n.name),
                        (_, Some(root), Some(sym)) => {
                            let loc = code_location(root, &sym.file_path, Some(sym.start_line));
                            println!(
                                "{indent}  {} {} [{}] ({loc})",
                                kind_arrow(&n.edge),
                                sym.name,
                                sym.kind
                            );
                        }
                        _ => println!("{indent}  {} {} [unresolved]", kind_arrow(&n.edge), n.name),
                    }
                }
                if !found_seed {
                    println!("Symbol '{symbol}' not found in any indexed repo.");
                    println!(
                        "Tip: re-index with `sclerox repo index` to capture call graph edges."
                    );
                }
            });
        }
    }
    Ok(())
}

fn no_repos(format: OutputFormat) {
    print_output(format, &Vec::<()>::new(), || {
        println!("No repos indexed. Run `sclerox repo index [path]` first.")
    });
}

/// A symbol and its outbound edges (`code calls`).
#[derive(serde::Serialize)]
struct SymbolEdges {
    repo_path: String,
    #[serde(flatten)]
    symbol: Symbol,
    edges: Vec<Edge>,
}

/// One outbound edge; `symbol` is its target when it resolves in the repo.
#[derive(serde::Serialize)]
struct Edge {
    name: String,
    kind: String,
    line: i64,
    confidence: String,
    symbol: Option<Symbol>,
}

/// A symbol that calls / uses the one asked about (`code refs`).
#[derive(serde::Serialize)]
struct Reference {
    repo_path: String,
    kind: String,
    line: i64,
    confidence: String,
    caller: Symbol,
}

/// A node of `code graph`, in breadth-first order. `edge` is the kind of edge
/// from its parent (empty for the seed); `symbol` is unset when it doesn't
/// resolve in any repo.
#[derive(serde::Serialize)]
struct GraphNode {
    name: String,
    depth: usize,
    edge: String,
    repo_path: Option<String>,
    symbol: Option<Symbol>,
}

/// Breadth-first call graph from `seed`, up to `max_depth` edges out. An
/// unresolved seed is the only node.
fn call_graph(repos: &[(String, RepoDb)], seed: &str, max_depth: usize) -> Vec<GraphNode> {
    // BFS: (name, depth, edge_kind_from_parent)
    let mut queue: VecDeque<(String, usize, String)> = VecDeque::new();
    queue.push_back((seed.to_string(), 0, String::new()));

    let mut visited: HashSet<String> = HashSet::new();
    let mut nodes = Vec::new();

    while let Some((name, depth, edge_kind)) = queue.pop_front() {
        if visited.contains(&name) {
            continue;
        }
        visited.insert(name.clone());

        // Look up symbol info across all repos
        let sym_info = find_across_repos(repos, &name);
        let first = sym_info.first();
        nodes.push(GraphNode {
            name: name.clone(),
            depth,
            edge: edge_kind,
            repo_path: first.map(|(root, _)| root.to_string()),
            symbol: first.map(|(_, sym)| sym.clone()),
        });
        if first.is_none() {
            // Can't follow edges from unresolved; an unresolved seed ends it.
            if depth == 0 {
                break;
            }
            continue;
        }

        if depth < max_depth {
            // Collect outbound edges from every repo that has this symbol,
            // each looked up in the repo that owns it (ids are per repo).
            for (owner, sym) in &sym_info {
                let Some((_, rdb)) = repos.iter().find(|(root, _)| root == owner) else {
                    continue;
                };
                for (to_name, kind, _, _confidence) in rdb.callees(sym.id).unwrap_or_default() {
                    if !visited.contains(&to_name) {
                        queue.push_back((to_name, depth + 1, kind));
                    }
                }
            }
        }
    }
    nodes
}

/// Open all repo DBs, optionally filtered by name, with each repo's root path.
fn open_matching_repos(db: &Database, repo_filter: Option<&str>) -> Result<Vec<(String, RepoDb)>> {
    db.repo_prune_gone()?;
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
            result.push((entry.path, rdb));
        }
    }
    Ok(result)
}

/// Find a symbol by exact name across a set of open repo DBs.
fn find_across_repos<'a>(repos: &'a [(String, RepoDb)], name: &str) -> Vec<(&'a str, Symbol)> {
    let mut results = Vec::new();
    for (root, rdb) in repos {
        if let Ok(syms) = rdb.symbol_by_name(name) {
            for sym in syms {
                results.push((root.as_str(), sym));
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
