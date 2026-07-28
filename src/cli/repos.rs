use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::db::Database;
use crate::embed::Embedder;
use crate::index::{find_git_root, repo_db::RepoDb, RepoIndexer};

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
        /// Index even when the folder exceeds the max-files cap
        #[arg(long)]
        force: bool,
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
    /// Backfill embeddings for indexed code chunks that don't have one
    ///
    /// The auto-index hooks index without an embedder, so hook-indexed repos
    /// have code chunks but no embeddings (semantic code search skips them).
    /// This embeds the stored chunk text in place — no re-parsing.
    Reembed {
        /// Restrict to repos whose name contains this string
        #[arg(long)]
        repo: Option<String>,
        /// Re-embed ALL chunks, not just those missing an embedding
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
            force,
        } => {
            let canonical = path.canonicalize().unwrap_or(path);
            // Walk up to the git root so `ol repo index` from a subdirectory indexes the whole repo.
            let canonical = find_git_root(&canonical);
            // Respect a per-folder opt-out before loading the (expensive) model.
            if !crate::index::repo_config(&canonical).index {
                println!(
                    "Skipping {}: indexing disabled in .ol/config.toml",
                    canonical.display()
                );
                return Ok(());
            }
            let mut embedder_opt: Option<Embedder> = if no_embed {
                None
            } else {
                Some(Embedder::new()?)
            };
            let mut indexer = RepoIndexer::new(embedder_opt.as_mut()).with_force(force);
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

        RepoCommand::Reembed { repo, force } => {
            reembed_repos(db, repo.as_deref(), force)?;
        }
    }
    Ok(())
}

/// Backfill (or force-recompute) embeddings for indexed code chunks across
/// registered repos, embedding the stored chunk text in place.
fn reembed_repos(db: &Database, repo_filter: Option<&str>, force: bool) -> Result<()> {
    let repos = db.repo_list()?;
    if repos.is_empty() {
        println!("No repos indexed. Run `ol repo index [path]` first.");
        return Ok(());
    }

    let mut embedder = Embedder::new()?;
    let mut total_embedded = 0usize;
    let mut touched_repos = 0usize;

    for entry in &repos {
        if let Some(f) = repo_filter {
            if !entry.name.to_lowercase().contains(&f.to_lowercase()) {
                continue;
            }
        }
        let db_path = PathBuf::from(&entry.db_path);
        if !db_path.exists() {
            continue;
        }
        let repo_db = match RepoDb::open(&db_path) {
            Ok(r) => r,
            Err(e) => {
                println!("  {}: could not open ({e})", entry.name);
                continue;
            }
        };

        let targets = if force {
            repo_db.all_chunks()?
        } else {
            repo_db.chunks_without_embedding()?
        };
        if targets.is_empty() {
            continue;
        }

        // Embed in batches to bound memory.
        let mut embedded = 0usize;
        for batch in targets.chunks(64) {
            let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
            let vectors = embedder.embed_batch(&texts)?;
            for ((chunk_id, _), vec) in batch.iter().zip(vectors) {
                repo_db.set_chunk_embedding(*chunk_id, &vec)?;
                embedded += 1;
            }
        }
        println!("  {}: embedded {embedded} chunks", entry.name);
        total_embedded += embedded;
        touched_repos += 1;
    }

    if total_embedded == 0 {
        println!("All indexed chunks already have embeddings.");
    } else {
        println!("Embedded {total_embedded} chunks across {touched_repos} repos.");
    }
    Ok(())
}

/// Check all registered repos, apply automatic repairs, and print a summary.
/// If `force` is true, reindex every repo regardless of current state.
fn heal_repos(db: &Database, force: bool) -> anyhow::Result<()> {
    use crate::db::repos::RepoHealthStatus;

    // First consolidate nested registrations: a folder indexed inside another
    // indexed folder is redundant (or a spurious non-git workspace parent).
    for removed in crate::index::prune_nested_repos(db)? {
        println!("Removing nested/redundant entry: {removed}");
    }

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
                // Re-indexing a known repo: don't let the cap drop one that was
                // previously indexed fine.
                let mut indexer = crate::index::RepoIndexer::new(None).with_force(true);
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
