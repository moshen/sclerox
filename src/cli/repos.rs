use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::db::Database;
use crate::embed::Embedder;
use crate::index::{repo_db::RepoDb, RepoIndexer};

#[derive(Subcommand)]
pub enum RepoCommand {
    /// Index a repository (defaults to current directory)
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        description: Option<String>,
        /// Skip embedding generation (embeddings are on by default)
        #[arg(long)]
        no_embed: bool,
    },
    /// List all indexed repositories
    List,
    /// Search repos by name/description (FTS + semantic combined)
    Search { query: String },
    /// Show indexed files and symbols for a repo
    Show {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Search symbols within the repo
        #[arg(long)]
        symbols: Option<String>,
    },
    /// Remove a repo from the registry
    Unindex {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Search symbols across all indexed repos
    ///
    /// Automatically heals the registry before searching:
    /// removes repos whose directories are gone, reindexes repos whose
    /// DB is missing, and migrates DBs that are behind the current schema.
    SearchSymbols { query: String },
    /// Check and heal all registered repos
    ///
    /// Removes stale entries, reindexes missing DBs, migrates old schemas.
    Sync {
        /// Also re-index all repos even if their DB is current
        #[arg(long)]
        force: bool,
    },
}

pub fn run(db: &Database, cmd: RepoCommand) -> Result<()> {
    match cmd {
        RepoCommand::Index {
            path,
            description,
            no_embed,
        } => {
            let canonical = path.canonicalize().unwrap_or(path);
            let mut embedder_opt: Option<Embedder> = if no_embed {
                None
            } else {
                Some(Embedder::new()?)
            };
            let mut indexer = RepoIndexer::new(embedder_opt.as_mut());
            println!("Indexing {}...", canonical.display());
            let result = indexer.index_repo(db, &canonical, description.as_deref())?;
            println!(
                "Done: {} files indexed, {} skipped, {} symbols, {} chunks, {} edges",
                result.files_indexed, result.skipped, result.symbols, result.chunks, result.edges,
            );
        }

        RepoCommand::List => {
            let repos = db.repo_list()?;
            if repos.is_empty() {
                println!("No repos indexed. Run `ol repo index [path]` to add one.");
            } else {
                for r in &repos {
                    let indexed = r.last_indexed.as_deref().unwrap_or("never");
                    let desc = r
                        .description
                        .as_deref()
                        .map(|d| format!(" - {}", truncate(d, 50)))
                        .unwrap_or_default();
                    println!("#{} {} [{}]{}", r.id, r.name, indexed, desc);
                    println!("   {}", r.path);
                }
                println!("\n{} repos", repos.len());
            }
        }

        RepoCommand::Search { query } => {
            // FTS on name/description first
            let fts_hits = db.repo_search(&query)?;
            let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|r| r.id).collect();

            for r in &fts_hits {
                println!("#{} {} - {}", r.id, r.name, r.path);
                if let Some(desc) = &r.description {
                    println!("   {}", truncate(desc, 100));
                }
            }

            // Semantic results deduped against FTS
            let mut embedder = Embedder::new()?;
            if let Ok(query_emb) = embedder.embed_one(&query) {
                let similar = db.repo_similar(&query_emb, 5).unwrap_or_default();
                let semantic: Vec<_> = similar
                    .iter()
                    .filter(|r| !fts_ids.contains(&r.repo.id))
                    .collect();
                if !semantic.is_empty() {
                    if !fts_hits.is_empty() {
                        println!();
                    }
                    for r in &semantic {
                        println!(
                            "#{} {}  ({:.0}% match)",
                            r.repo.id,
                            r.repo.name,
                            r.score * 100.0
                        );
                        println!("   {}", r.repo.path);
                        if let Some(desc) = &r.repo.description {
                            println!("   {}", truncate(desc, 100));
                        }
                    }
                }
            }

            if fts_hits.is_empty() {
                println!("No repos match: {query}");
            }
        }

        RepoCommand::Show { path, symbols } => {
            let canonical = path.canonicalize().unwrap_or(path);
            let repo_entry = db.repo_get_by_path(&canonical.to_string_lossy())?;
            let db_path = match &repo_entry {
                Some(r) => PathBuf::from(&r.db_path),
                None => canonical.join(".ol").join("repo.db"),
            };

            if !db_path.exists() {
                println!(
                    "Repo not indexed. Run `ol repo index {}`",
                    canonical.display()
                );
                return Ok(());
            }

            let repo_db = RepoDb::open(&db_path)?;
            let (files, syms, chunks, edges) = repo_db.stats()?;
            println!("Repo: {}", canonical.display());
            println!("Files: {files}  Symbols: {syms}  Chunks: {chunks}  Edges: {edges}");

            if let Some(query) = symbols {
                let results = repo_db.search_symbols(&query)?;
                if results.is_empty() {
                    println!("No symbols match: {query}");
                } else {
                    for s in &results {
                        let sig = s.signature.as_deref().unwrap_or(&s.name);
                        println!("  [{}] {} ({}:{})", s.kind, sig, s.file_path, s.start_line);
                    }
                }
            } else {
                let files = repo_db.list_files()?;
                println!("\nFiles:");
                for f in files.iter().take(30) {
                    let lang = f.language.as_deref().unwrap_or("?");
                    println!("  [{}] {}", lang, f.path);
                }
                if files.len() > 30 {
                    println!("  ... and {} more", files.len() - 30);
                }
            }
        }

        RepoCommand::Unindex { path } => {
            let canonical = path.canonicalize().unwrap_or(path);
            if db.repo_remove(&canonical.to_string_lossy())? {
                println!("Removed {} from registry", canonical.display());
            } else {
                println!("Repo not found in registry: {}", canonical.display());
            }
        }

        RepoCommand::SearchSymbols { query } => {
            heal_repos(db, false)?;

            let repos = db.repo_list()?;
            if repos.is_empty() {
                println!("No repos indexed. Run `ol repo index [path]` first.");
                return Ok(());
            }

            let mut any = false;
            for repo in &repos {
                let db_path = PathBuf::from(&repo.db_path);
                if !db_path.exists() {
                    continue;
                }
                let repo_db = match RepoDb::open(&db_path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let results = repo_db.search_symbols(&query)?;
                for s in &results {
                    let sig = s.signature.as_deref().unwrap_or(&s.name);
                    println!(
                        "[{}] {} ({}/{}:{})",
                        s.kind, sig, repo.name, s.file_path, s.start_line
                    );
                    any = true;
                }
            }
            if !any {
                println!("No symbols match: {query}");
                println!("Tip: run `ol repo index [path]` to index a repository first.");
            }
        }

        RepoCommand::Sync { force } => {
            heal_repos(db, force)?;
        }
    }
    Ok(())
}

/// Check all registered repos, apply automatic repairs, and print a summary.
/// If `force` is true, reindex every repo regardless of current state.
fn heal_repos(db: &Database, force: bool) -> anyhow::Result<()> {
    use crate::db::repos::RepoHealthStatus;

    let health = db.repo_health_check()?;
    if health.is_empty() {
        return Ok(());
    }

    for h in &health {
        match &h.status {
            RepoHealthStatus::Ok if !force => {} // nothing to do

            RepoHealthStatus::DirectoryGone => {
                println!("Removing stale entry: {} (directory gone)", h.repo.name);
                db.repo_remove(&h.repo.path)?;
            }

            RepoHealthStatus::DbMissing | RepoHealthStatus::Ok => {
                // DbMissing: reindex from scratch.  Ok + force: reindex anyway.
                let action = if force {
                    "Re-indexing"
                } else {
                    "Reindexing missing DB for"
                };
                println!("{action}: {}", h.repo.name);
                let path = std::path::Path::new(&h.repo.path);
                let mut indexer = crate::index::RepoIndexer::new(None);
                match indexer.index_repo(db, path, h.repo.description.as_deref()) {
                    Ok(r) => println!("  {} files indexed, {} symbols", r.files_indexed, r.symbols),
                    Err(e) => println!("  failed: {e}"),
                }
            }

            RepoHealthStatus::SchemaBehind { current, target } => {
                println!("Migrating {}: schema v{current} → v{target}", h.repo.name);
                // Opening the repo DB runs migrations automatically.
                let db_path = std::path::Path::new(&h.repo.db_path);
                match RepoDb::open(db_path) {
                    Ok(_) => println!("  migrated"),
                    Err(e) => println!("  failed: {e}"),
                }
            }

            RepoHealthStatus::Error(e) => {
                println!("Error with {}: {e}", h.repo.name);
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
