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
        /// Generate embeddings for similarity search (downloads model on first run)
        #[arg(long)]
        embed: bool,
    },
    /// List all indexed repositories
    List,
    /// Full-text search repos by name/description
    Search { query: String },
    /// Find repos semantically similar to a query
    Similar {
        query: String,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
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
}

pub fn run(db: &Database, cmd: RepoCommand) -> Result<()> {
    match cmd {
        RepoCommand::Index {
            path,
            description,
            embed,
        } => {
            let canonical = path.canonicalize().unwrap_or(path);
            let mut embedder_opt: Option<Embedder> =
                if embed { Some(Embedder::new()?) } else { None };
            let mut indexer = RepoIndexer::new(embedder_opt.as_mut());
            println!("Indexing {}...", canonical.display());
            let result = indexer.index_repo(db, &canonical, description.as_deref())?;
            println!(
                "Done: {} files indexed, {} skipped, {} symbols, {} chunks",
                result.files_indexed, result.skipped, result.symbols, result.chunks
            );
            if !embed {
                println!("Tip: run with --embed to enable semantic similarity search");
            }
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
            let results = db.repo_search(&query)?;
            if results.is_empty() {
                println!("No repos match: {query}");
            } else {
                for r in &results {
                    println!("#{} {} - {}", r.id, r.name, r.path);
                    if let Some(desc) = &r.description {
                        println!("   {}", truncate(desc, 100));
                    }
                }
            }
        }

        RepoCommand::Similar { query, limit } => {
            let mut embedder = Embedder::new()?;
            let query_emb = embedder.embed_one(&query)?;
            let results = db.repo_similar(&query_emb, limit)?;
            if results.is_empty() {
                println!("No similar repos. Index repos with --embed to enable similarity search.");
            } else {
                for r in &results {
                    println!(
                        "{:.3} #{} {} - {}",
                        r.score, r.repo.id, r.repo.name, r.repo.path
                    );
                    if let Some(desc) = &r.repo.description {
                        println!("      {}", truncate(desc, 100));
                    }
                }
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
            let (files, syms, chunks) = repo_db.stats()?;
            println!("Repo: {}", canonical.display());
            println!("Files: {files}  Symbols: {syms}  Chunks: {chunks}");

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
