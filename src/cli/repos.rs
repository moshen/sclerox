use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::db::Database;
use crate::embed::Embedder;
use crate::index::{find_git_root, repo_db::RepoDb, RepoIndexer};
use crate::output::{print_miss, print_output, progress, truncate, OutputFormat};
use crate::search::SearchResult;

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
        /// Wait for the model server to embed the chunks, showing progress
        #[arg(long)]
        wait: bool,
    },
    /// List all indexed repositories
    List,
    /// Search repos by name/description/path (FTS + semantic combined)
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
    /// This embeds the stored chunk text in place - no re-parsing. A repo
    /// embedded with a different model than `embed.model` is re-embedded in
    /// full (see also `sclerox db reembed`, which covers every database).
    Reembed {
        /// Restrict to repos whose name contains this string
        #[arg(long)]
        repo: Option<String>,
        /// Re-embed ALL chunks, not just those missing an embedding
        #[arg(long)]
        force: bool,
        /// Wait for the model server to finish, showing progress
        #[arg(long)]
        wait: bool,
    },
}

pub fn run(db: &Database, cmd: RepoCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        RepoCommand::Index {
            path,
            description,
            no_embed,
            force,
            wait,
        } => {
            let canonical = path.canonicalize().unwrap_or(path);
            // Walk up to the git root so `sclerox repo index` from a subdirectory indexes the whole repo.
            let canonical = find_git_root(&canonical);
            // Self-heal a pre-rename `.ol/` before the opt-out check below reads
            // `.sclerox/config.toml` - see `crate::migrate`.
            crate::migrate::migrate_legacy_repo_dir(&canonical);
            // Respect a per-folder opt-out before loading the (expensive) model.
            if !crate::index::repo_config(&canonical).index {
                let msg = format!(
                    "Skipping {}: indexing disabled in .sclerox/config.toml",
                    canonical.display()
                );
                print_output(
                    format,
                    &serde_json::json!({"path": canonical, "indexed": false, "reason": msg}),
                    || println!("{msg}"),
                );
                return Ok(());
            }
            // With the model server, index without vectors and queue them
            // for it afterwards.
            let queue_to = if no_embed || !crate::embed::enabled() {
                None
            } else {
                crate::embed::models::configured()
                    .ok()
                    .filter(|spec| super::queue::server_takes_work(spec))
            };
            // The repo DB checks its own recorded model while indexing.
            let mut embedder_opt: Option<Embedder> = if no_embed || queue_to.is_some() {
                None
            } else {
                crate::embed::optional(true).map(Embedder::bulk)
            };
            let mut indexer = RepoIndexer::new(embedder_opt.as_mut()).with_force(force);
            progress(format_args!("Indexing {}...", canonical.display()));
            let result = indexer.index_repo(db, &canonical, description.as_deref())?;
            let summary = format!(
                "Done: {} files indexed, {} skipped, {} symbols, {} chunks, {} edges",
                result.files_indexed, result.skipped, result.symbols, result.chunks, result.edges,
            );
            // In text the summary comes before any queueing output, as before.
            if format == OutputFormat::Text {
                println!("{summary}");
            }
            let embedding = match queue_to {
                Some(spec) => Some(queue_index_embedding(db, &canonical, spec, wait)?),
                None => None,
            };
            if format == OutputFormat::Json {
                print_output(
                    format,
                    &serde_json::json!({
                        "path": canonical,
                        "indexed": true,
                        "files_indexed": result.files_indexed,
                        "skipped": result.skipped,
                        "symbols": result.symbols,
                        "chunks": result.chunks,
                        "edges": result.edges,
                        "embedding": embedding,
                    }),
                    || {},
                );
            }
        }

        RepoCommand::List => {
            for removed in db.repo_prune_gone()? {
                progress(format_args!(
                    "Removing stale entry: {} (directory gone)",
                    removed.name
                ));
            }
            let repos = db.repo_list()?;
            print_output(format, &repos, || {
                if repos.is_empty() {
                    println!("No repos indexed. Run `sclerox repo index [path]` to add one.");
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
            });
        }

        RepoCommand::Search { query } => {
            db.repo_prune_gone()?;
            // Keyword (FTS + substring) on name/description/path first
            let fts_hits = db.repo_search(&query)?;
            let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|r| r.id).collect();

            // Semantic results deduped against FTS
            let query_emb = crate::embed::optional(db.semantic_ready())
                .and_then(|mut e| e.embed_query(&query).ok());
            let semantic: Vec<_> = query_emb
                .map(|qe| db.repo_similar(&qe, 5).unwrap_or_default())
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !fts_ids.contains(&r.repo.id))
                .collect();

            print_output(
                format,
                &serde_json::json!({"fts": fts_hits, "semantic": semantic}),
                || {
                    if fts_hits.is_empty() && semantic.is_empty() {
                        println!("No repos match: {query}");
                        return;
                    }
                    for r in &fts_hits {
                        let at = crate::cli::format::stamp("created", &r.created_at);
                        println!("#{} {} - {}  {at}", r.id, r.name, r.path);
                        if let Some(desc) = &r.description {
                            println!("   {}", truncate(desc, 100));
                        }
                    }
                    if !semantic.is_empty() && !fts_hits.is_empty() {
                        println!();
                    }
                    for r in &semantic {
                        println!(
                            "#{} {}  ({:.0}% match)  {}",
                            r.repo.id,
                            r.repo.name,
                            r.score * 100.0,
                            crate::cli::format::stamp("created", &r.repo.created_at)
                        );
                        println!("   {}", r.repo.path);
                        if let Some(desc) = &r.repo.description {
                            println!("   {}", truncate(desc, 100));
                        }
                    }
                },
            );
        }

        RepoCommand::Show { path, symbols } => {
            let canonical = path.canonicalize().unwrap_or(path);
            let repo_entry = db.repo_get_by_path(&canonical.to_string_lossy())?;
            let db_path = match &repo_entry {
                Some(r) => PathBuf::from(&r.db_path),
                None => canonical.join(".sclerox").join("repo.db"),
            };

            if !db_path.exists() {
                print_miss(
                    format,
                    &format!(
                        "Repo not indexed. Run `sclerox repo index {}`",
                        canonical.display()
                    ),
                );
                return Ok(());
            }

            let repo_db = RepoDb::open(&db_path)?;
            let (files, syms, chunks, edges) = repo_db.stats()?;
            let header = || {
                println!("Repo: {}", canonical.display());
                println!("Files: {files}  Symbols: {syms}  Chunks: {chunks}  Edges: {edges}");
            };
            let stats = serde_json::json!({
                "path": canonical,
                "files": files,
                "symbols": syms,
                "chunks": chunks,
                "edges": edges,
            });

            if let Some(query) = symbols {
                let results = repo_db.search_symbols(&query)?;
                print_output(
                    format,
                    &serde_json::json!({"repo": stats, "symbols": results}),
                    || {
                        header();
                        if results.is_empty() {
                            println!("No symbols match: {query}");
                        } else {
                            for s in &results {
                                let sig = s.signature.as_deref().unwrap_or(&s.name);
                                println!(
                                    "  [{}] {} ({}:{})",
                                    s.kind, sig, s.file_path, s.start_line
                                );
                            }
                        }
                    },
                );
            } else {
                let file_list = repo_db.list_files()?;
                print_output(
                    format,
                    &serde_json::json!({"repo": stats, "files": file_list}),
                    || {
                        header();
                        println!("\nFiles:");
                        for f in file_list.iter().take(30) {
                            let lang = f.language.as_deref().unwrap_or("?");
                            println!("  [{}] {}", lang, f.path);
                        }
                        if file_list.len() > 30 {
                            println!("  ... and {} more", file_list.len() - 30);
                        }
                    },
                );
            }
        }

        RepoCommand::Unindex { path } => {
            let canonical = path.canonicalize().unwrap_or(path);
            if db.repo_remove(&canonical.to_string_lossy())? {
                print_output(format, &serde_json::json!({"removed": canonical}), || {
                    println!("Removed {} from registry", canonical.display())
                });
            } else {
                print_miss(
                    format,
                    &format!("Repo not found in registry: {}", canonical.display()),
                );
            }
        }

        RepoCommand::SearchSymbols { query } => {
            heal_repos(db, false)?;

            let repos = db.repo_list()?;
            if repos.is_empty() {
                print_output(format, &Vec::<SearchResult>::new(), || {
                    println!("No repos indexed. Run `sclerox repo index [path]` first.")
                });
                return Ok(());
            }

            // Results as `sclerox search` emits symbols.
            let mut hits: Vec<SearchResult> = Vec::new();
            for repo in &repos {
                let db_path = PathBuf::from(&repo.db_path);
                if !db_path.exists() {
                    continue;
                }
                let repo_db = match RepoDb::open(&db_path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for s in repo_db.search_symbols(&query)? {
                    hits.push(SearchResult::Symbol {
                        repo_name: repo.name.clone(),
                        repo_path: repo.path.clone(),
                        kind: s.kind,
                        name: s.name,
                        signature: s.signature,
                        file_path: s.file_path,
                        start_line: s.start_line,
                    });
                }
            }
            print_output(format, &hits, || {
                for h in &hits {
                    if let SearchResult::Symbol {
                        repo_path,
                        kind,
                        name,
                        signature,
                        file_path,
                        start_line,
                        ..
                    } = h
                    {
                        let sig = signature.as_deref().unwrap_or(name);
                        let loc = crate::cli::format::code_location(
                            repo_path,
                            file_path,
                            Some(*start_line),
                        );
                        println!("{}", crate::cli::format::symbol_line(kind, sig, &loc));
                    }
                }
                if hits.is_empty() {
                    println!("No symbols match: {query}");
                    println!("Tip: run `sclerox repo index [path]` to index a repository first.");
                }
            });
        }

        RepoCommand::Sync { force } => {
            let actions = heal_repos(db, force)?;
            if format == OutputFormat::Json {
                print_output(format, &actions, || {});
            }
        }

        RepoCommand::Reembed { repo, force, wait } => {
            let result = reembed_repos(db, repo.as_deref(), force, wait)?;
            if format == OutputFormat::Json {
                print_output(format, &result, || {});
            }
        }
    }
    Ok(())
}

/// Backfill (or force-recompute) embeddings for indexed code chunks across
/// registered repos, embedding the stored chunk text in place.
/// Returns the result `--output json` reports.
fn reembed_repos(
    db: &Database,
    repo_filter: Option<&str>,
    force: bool,
    wait: bool,
) -> Result<serde_json::Value> {
    for removed in db.repo_prune_gone()? {
        progress(format_args!(
            "Removing stale entry: {} (directory gone)",
            removed.name
        ));
    }
    let repos = db.repo_list()?;
    if repos.is_empty() {
        progress("No repos indexed. Run `sclerox repo index [path]` first.");
        return Ok(serde_json::json!({"embedded": 0, "repos": 0}));
    }
    if crate::embed::enabled() {
        let spec = crate::embed::models::configured()?;
        if super::queue::server_takes_work(spec) {
            return queue_repos(&repos, repo_filter, spec, force, wait);
        }
    }

    let mut embedder = crate::embed::required()?;
    let mut total_embedded = 0usize;
    let mut touched_repos = 0usize;
    let mut failed = 0usize;

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
                progress(format_args!("  {}: could not open ({e})", entry.name));
                failed += 1;
                continue;
            }
        };

        // A full re-embed (forced, or an index built with another model) goes
        // through the all-or-nothing rebuild, which also records the model.
        // It redoes the chunks that had vectors; the backfill below then
        // embeds the rest.
        let mismatched = match repo_db.embedding_model_check() {
            Ok(state) => matches!(state, crate::embed::models::ModelCheck::Mismatch { .. }),
            Err(e) => {
                progress(format_args!(
                    "  {}: could not read its embedding model ({e:#})",
                    entry.name
                ));
                failed += 1;
                continue;
            }
        };
        let mut touched = false;
        if force || mismatched {
            let model_id = embedder.model().id;
            match repo_db.reembed_all(
                model_id,
                &mut |texts: &[&str]| embedder.embed_each(texts),
                &mut |_, _, _| {},
            ) {
                Ok(n) => {
                    if n > 0 {
                        progress(format_args!("  {}: re-embedded {n} chunks", entry.name));
                        total_embedded += n;
                        touched = true;
                    }
                }
                Err(e) => {
                    // Still on its old model, so the backfill would be refused.
                    progress(format_args!(
                        "  {}: failed, left on its previous model: {e:#}",
                        entry.name
                    ));
                    failed += 1;
                    continue;
                }
            }
        }

        let mut embedded = 0usize;
        if let Err(e) = backfill_chunks(&repo_db, &mut embedder, &mut embedded) {
            // Batches embedded before the failure are kept and counted below.
            progress(format_args!(
                "  {}: embedding chunks failed: {e:#}",
                entry.name
            ));
            failed += 1;
        }
        if embedded > 0 {
            progress(format_args!("  {}: embedded {embedded} chunks", entry.name));
            total_embedded += embedded;
            touched = true;
        }
        touched_repos += usize::from(touched);
    }

    if failed > 0 {
        progress(format_args!(
            "Embedded {total_embedded} chunks across {touched_repos} repos; {failed} failed."
        ));
        anyhow::bail!("{failed} repo(s) could not be embedded; run again to retry");
    }
    if total_embedded == 0 {
        progress("All indexed chunks already have embeddings.");
    } else {
        progress(format_args!(
            "Embedded {total_embedded} chunks across {touched_repos} repos."
        ));
    }
    Ok(serde_json::json!({"embedded": total_embedded, "repos": touched_repos}))
}

/// `repo reembed` through the model server: queue a rebuild where one is
/// due and a fill of what has no vector, in each matching repo.
fn queue_repos(
    repos: &[crate::db::repos::RepoEntry],
    repo_filter: Option<&str>,
    spec: &'static crate::embed::ModelSpec,
    force: bool,
    wait: bool,
) -> Result<serde_json::Value> {
    use crate::db::jobs::{DbKind, Kind};

    let mut queued = Vec::new();
    let mut failed = 0usize;
    for entry in repos {
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
                progress(format_args!("  {}: could not open ({e})", entry.name));
                failed += 1;
                continue;
            }
        };
        let mismatched = match repo_db.embedding_model_check() {
            Ok(state) => matches!(state, crate::embed::models::ModelCheck::Mismatch { .. }),
            Err(e) => {
                progress(format_args!(
                    "  {}: could not read its embedding model ({e:#})",
                    entry.name
                ));
                failed += 1;
                continue;
            }
        };
        let mut todo = Vec::new();
        if force || mismatched {
            todo.push((Kind::Rebuild, None));
        }
        if !todo.is_empty() || repo_db.has_chunks_without_embedding()? {
            todo.push((Kind::Fill, None));
        }
        if !todo.is_empty() {
            queued.push(super::queue::submit(
                &repo_db.conn,
                spec,
                &entry.name,
                DbKind::Repo,
                &todo,
            )?);
        }
    }
    if queued.is_empty() && failed == 0 {
        progress("All indexed chunks already have embeddings.");
        return Ok(super::queue::summary(&queued, wait));
    }
    if wait {
        super::queue::wait(spec, &queued)?;
    } else {
        super::queue::report(&queued);
    }
    if failed > 0 {
        anyhow::bail!("{failed} repo(s) could not be queued; run again to retry");
    }
    Ok(super::queue::summary(&queued, wait))
}

/// After `repo index` without vectors: queue them for the model server, in
/// the repos indexed under `root` (nested ones included) and, for the repo
/// descriptions, in the main database.
fn queue_index_embedding(
    db: &Database,
    root: &std::path::Path,
    spec: &'static crate::embed::ModelSpec,
    wait: bool,
) -> Result<serde_json::Value> {
    use crate::db::jobs::{DbKind, Kind};

    let mut queued = Vec::new();
    for entry in db.repo_list()? {
        if !std::path::Path::new(&entry.path).starts_with(root) {
            continue;
        }
        let db_path = PathBuf::from(&entry.db_path);
        if !db_path.exists() {
            continue;
        }
        let repo_db = match RepoDb::open(&db_path) {
            Ok(r) => r,
            Err(e) => {
                progress(format_args!("  {}: could not open ({e})", entry.name));
                continue;
            }
        };
        // One on another model takes no new vectors until it is rebuilt.
        if repo_db.semantic_ready() && repo_db.has_chunks_without_embedding()? {
            queued.push(super::queue::submit(
                &repo_db.conn,
                spec,
                &entry.name,
                DbKind::Repo,
                &[(Kind::Fill, None)],
            )?);
        }
    }
    if db.semantic_ready() && db.missing_vectors()? > 0 {
        queued.push(super::queue::submit(
            &db.conn,
            spec,
            "main database",
            DbKind::Main,
            &[(Kind::Fill, None)],
        )?);
    }
    if wait {
        super::queue::wait(spec, &queued)?;
    } else {
        super::queue::report(&queued);
    }
    Ok(super::queue::summary(&queued, wait))
}

/// Embed a repo's chunks that have no vector, in batches to bound memory.
/// Each batch is kept as it goes; `embedded` counts them even on failure.
pub(crate) fn backfill_chunks(
    repo_db: &RepoDb,
    embedder: &mut Embedder,
    embedded: &mut usize,
) -> Result<()> {
    let targets = repo_db.chunks_without_embedding()?;
    for batch in targets.chunks(64) {
        let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = embedder.embed_each(&texts)?;
        for ((chunk_id, _), vec) in batch.iter().zip(vectors) {
            if repo_db.set_chunk_embedding(*chunk_id, &vec)? {
                *embedded += 1;
            }
        }
    }
    Ok(())
}

/// Check all registered repos, apply automatic repairs, and print a summary.
/// If `force` is true, reindex every repo regardless of current state.
/// Returns what it did, one `{repo, action, error?}` per repair.
fn heal_repos(db: &Database, force: bool) -> anyhow::Result<Vec<serde_json::Value>> {
    use crate::db::repos::RepoHealthStatus;

    let mut actions = Vec::new();
    let mut record = |repo: &str, action: &str, error: Option<String>| {
        actions.push(serde_json::json!({"repo": repo, "action": action, "error": error}));
    };

    // First consolidate nested registrations: a folder indexed inside another
    // indexed folder is redundant (or a spurious non-git workspace parent).
    for removed in crate::index::prune_nested_repos(db)? {
        progress(format_args!("Removing nested/redundant entry: {removed}"));
        record(&removed.to_string(), "removed_nested", None);
    }

    let health = db.repo_health_check()?;

    for h in &health {
        match &h.status {
            RepoHealthStatus::Ok if !force => {} // nothing to do

            RepoHealthStatus::DirectoryGone => {
                progress(format_args!(
                    "Removing stale entry: {} (directory gone)",
                    h.repo.name
                ));
                db.repo_remove(&h.repo.path)?;
                record(&h.repo.name, "removed_stale", None);
            }

            RepoHealthStatus::DbMissing | RepoHealthStatus::Ok => {
                // DbMissing: reindex from scratch.  Ok + force: reindex anyway.
                let action = if force {
                    "Re-indexing"
                } else {
                    "Reindexing missing DB for"
                };
                progress(format_args!("{action}: {}", h.repo.name));
                let path = std::path::Path::new(&h.repo.path);
                // Re-indexing a known repo: don't let the cap drop one that was
                // previously indexed fine.
                let mut indexer = crate::index::RepoIndexer::new(None).with_force(true);
                match indexer.index_repo(db, path, h.repo.description.as_deref()) {
                    Ok(r) => {
                        progress(format_args!(
                            "  {} files indexed, {} symbols",
                            r.files_indexed, r.symbols
                        ));
                        record(&h.repo.name, "reindexed", None);
                    }
                    Err(e) => {
                        progress(format_args!("  failed: {e}"));
                        record(&h.repo.name, "reindexed", Some(e.to_string()));
                    }
                }
            }

            RepoHealthStatus::SchemaBehind { current, target } => {
                progress(format_args!(
                    "Migrating {}: schema v{current} → v{target}",
                    h.repo.name
                ));
                // Opening the repo DB runs migrations automatically.
                let db_path = std::path::Path::new(&h.repo.db_path);
                match RepoDb::open(db_path) {
                    Ok(_) => {
                        progress("  migrated");
                        record(&h.repo.name, "migrated", None);
                    }
                    Err(e) => {
                        progress(format_args!("  failed: {e}"));
                        record(&h.repo.name, "migrated", Some(e.to_string()));
                    }
                }
            }

            RepoHealthStatus::Error(e) => {
                progress(format_args!("Error with {}: {e}", h.repo.name));
                record(&h.repo.name, "error", Some(e.to_string()));
            }
        }
    }

    Ok(actions)
}
