pub mod parser;
pub mod repo_db;

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::Database;
use crate::embed::Embedder;

use parser::{detect_language, parse_file};
use repo_db::RepoDb;

/// Walk up from `dir` to find the root of the git repository.
/// Returns the ancestor directory that contains a `.git` entry (file or dir).
/// Falls back to `dir` itself if no git root is found.
pub fn find_git_root(dir: &Path) -> std::path::PathBuf {
    let mut current = dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => return dir.to_path_buf(),
        }
    }
}

// Fallback line count for languages without a tree-sitter grammar.
// ~15 lines × 50 chars/line ≈ 750 chars, within AllMiniLML6V2's 256-token window.
pub const CHUNK_SIZE_LINES: usize = 15;

// Maximum chars per embedded chunk. Anything larger gets split by split_large_chunk().
// AllMiniLML6V2 max tokens = 256 ≈ 1024 chars; 800 gives a safety margin.
pub const MAX_EMBED_CHARS: usize = 800;

// Overlap when splitting an oversized function chunk into sub-chunks.
const SPLIT_OVERLAP_LINES: usize = 3;

// Directories always excluded regardless of .gitignore.
// .gitignore is the primary source of truth (via the ignore crate), but these
// cover universal build/cache dirs that repos commonly omit from .gitignore,
// plus our own .ol dir which should never appear in .gitignore.
const HARDCODED_IGNORED: &[&str] = &[
    ".ol",
    ".git",
    "node_modules",
    "target", // Rust build output
    "__pycache__",
    ".venv",
    "vendor",
    "obj",
    "bin", // .NET build output
];

// Default tree-sitter file size limit. Override with OL_MAX_INDEX_FILE_BYTES env var.
// Files above this fall back to line-based chunking (still indexed, no symbol extraction).
const DEFAULT_MAX_TREE_SITTER_BYTES: usize = 1_000_000; // 1 MB

fn max_tree_sitter_bytes() -> usize {
    std::env::var("OL_MAX_INDEX_FILE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_TREE_SITTER_BYTES)
}

pub struct RepoIndexer<'a> {
    embedder: Option<&'a mut Embedder>,
    /// Bypass the max-files cap (set by explicit `ol repo index --force`).
    /// The unsafe-root refusal is never bypassed — home/root are always off-limits.
    force: bool,
    /// Override the max-files cap; `None` reads it from the env (bridged config).
    max_files: Option<usize>,
}

impl<'a> RepoIndexer<'a> {
    pub fn new(embedder: Option<&'a mut Embedder>) -> Self {
        Self {
            embedder,
            force: false,
            max_files: None,
        }
    }

    /// Bypass the max-files cap for this indexer (explicit, deliberate indexing).
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Override the max-files cap (otherwise read from `OL_MAX_INDEX_FILES`).
    /// Test-only: production callers set the cap via config/env.
    #[cfg(test)]
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = Some(max_files);
        self
    }

    /// Index a repo at `repo_root`, store results in `repo_root/.ol/repo.db`,
    /// and register it in the primary `db`.
    pub fn index_repo(
        &mut self,
        db: &Database,
        repo_root: &Path,
        description: Option<&str>,
    ) -> Result<IndexResult> {
        // Canonicalize up front so the registry, the index db path, and every
        // ancestry check use one normalized form regardless of caller (hooks
        // pass a logical cwd; `ol repo index` canonicalizes). This keeps the
        // same directory from being registered twice under different spellings.
        let repo_root = canonical_or_self(repo_root);
        let repo_root = repo_root.as_path();
        let name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Never index the filesystem/drive root, the home directory, or a
        // shallow top-level dir. This is the backstop that stops a stray
        // `~/.git` (dotfiles) from turning a session anywhere under $HOME into
        // an index of the entire home directory. Not bypassable by --force.
        if is_unsafe_index_root(repo_root) {
            log::warn!(
                "refusing to index '{}' — too high in the filesystem (home/root/top-level)",
                repo_root.display()
            );
            return Ok(IndexResult::default());
        }

        // Honor a per-folder opt-out before touching anything so hooks silently
        // skip excluded folders. Also self-heal: if this folder was indexed
        // before being opted out, retract the now-stale index (repo.db + its
        // WAL/SHM sidecars) and the primary registry entry. Keep .ol/config.toml
        // — it is the opt-out marker itself.
        if !repo_config(repo_root).index {
            log::info!("skipping '{name}' — indexing disabled in .ol/config.toml");
            let ol_dir = repo_root.join(".ol");
            let mut retracted = false;
            for fname in ["repo.db", "repo.db-wal", "repo.db-shm"] {
                let p = ol_dir.join(fname);
                if p.exists() {
                    match std::fs::remove_file(&p) {
                        Ok(()) => retracted = true,
                        Err(e) => log::warn!("failed to remove stale {}: {e}", p.display()),
                    }
                }
            }
            if db.repo_remove(&repo_root.to_string_lossy())? {
                retracted = true;
            }
            if retracted {
                log::info!("retracted stale index for opted-out '{name}'");
            }
            return Ok(IndexResult::default());
        }

        // Don't create a nested index when an ancestor folder is already
        // indexed — the parent's index already covers this subtree. Only a
        // *proper* ancestor counts, so re-indexing the same root still works.
        if let Some(parent) = parent_indexed_repo(db, repo_root)? {
            log::info!("skipping '{name}' — already covered by parent index at {parent}");
            return Ok(IndexResult::default());
        }

        // Gather the indexable files first, bounded by the max-files cap. A
        // folder over the cap is REJECTED outright (no partial, misleading
        // index) unless this indexer was created with --force. Gathering stops
        // as soon as the cap is exceeded, so a giant tree can't run the walk
        // away before we bail.
        let max_files = self.max_files.unwrap_or_else(max_index_files);
        let (files, over_cap) = collect_indexable_files(repo_root, max_files);
        if over_cap && !self.force {
            log::warn!(
                "refusing to index '{name}' — more than {max_files} indexable files. \
                 Re-run with `ol repo index --force` or raise [index].max_files.",
            );
            return Ok(IndexResult::default());
        }

        log::info!("indexing repo '{}' at {}", name, repo_root.display());
        let db_path = repo_root.join(".ol").join("repo.db");
        // Track whether the index existed before this run so we can clean up a
        // stub repo.db if the folder turns out to have nothing to index.
        let db_existed = db_path.exists();
        let repo_db = RepoDb::open(&db_path)?;

        repo_db.set_meta("repo_root", &repo_root.to_string_lossy())?;
        repo_db.set_meta("name", &name)?;

        // Retract any nested child index this parent now covers: a subfolder was
        // indexed on its own before the parent was, so its .ol/repo.db is now
        // redundant. A subfolder that explicitly opted out keeps its own index.
        retract_nested_child_indexes(db, repo_root)?;

        let mut result = IndexResult::default();

        for path in &files {
            let path = path.as_path();
            // Store repo-relative paths with forward slashes on every platform
            // (git's convention) so stored paths, search output, and the
            // `/`-based ignore checks stay consistent on Windows too. Building
            // from components avoids the native separator that to_string_lossy
            // would emit (backslashes on Windows).
            let rel_path = match path.strip_prefix(repo_root) {
                Ok(p) => p
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                Err(_) => continue,
            };

            let lang = match detect_language(path) {
                Some(l) => l,
                None => continue,
            };

            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // For very large files, skip tree-sitter to avoid unbounded memory
            // usage (tree-sitter requires the whole file in memory at once).
            // The file is still indexed via line-based chunking - it will be
            // searchable but without symbol extraction.
            let effective_lang = if source.len() > max_tree_sitter_bytes() {
                log::debug!(
                    "{rel_path} is {}KB, skipping tree-sitter (line-based only)",
                    source.len() / 1024
                );
                "text" // no tree-sitter grammar → falls back to chunk_by_lines
            } else {
                lang
            };

            let hash = sha256_hex(source.as_bytes());

            // Skip unchanged files - but if --embed is active, also re-index
            // files whose chunks have no embeddings (previously indexed without --embed).
            let hash_matches = repo_db.file_hash(&rel_path)?.as_deref() == Some(&hash);
            let needs_embed = self.embedder.is_some()
                && hash_matches
                && !repo_db.file_chunks_all_embedded(&rel_path)?;

            if hash_matches && !needs_embed {
                result.skipped += 1;
                continue;
            }

            let file_id = repo_db.upsert_file(&rel_path, Some(lang), &hash, None)?;
            repo_db.delete_file_data(file_id)?;

            let (symbols, raw_chunks, edges) =
                parse_file(&source, effective_lang, CHUNK_SIZE_LINES);

            // Split any chunk that exceeds the embedding model's context window.
            let chunks: Vec<parser::CodeChunk> = raw_chunks
                .into_iter()
                .flat_map(|c| parser::split_large_chunk(c, MAX_EMBED_CHARS, SPLIT_OVERLAP_LINES))
                .collect();

            // Insert symbols and build a name→id map for resolving call edges.
            let mut name_to_id: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            for sym in &symbols {
                let sym_id = repo_db.insert_symbol(
                    file_id,
                    &sym.kind,
                    &sym.name,
                    sym.signature.as_deref(),
                    sym.start_line as i64,
                    sym.end_line as i64,
                )?;
                name_to_id.insert(sym.name.clone(), sym_id);
                result.symbols += 1;
            }

            // Insert call/inheritance edges for symbols found in this file.
            for edge in &edges {
                if let Some(&from_id) = name_to_id.get(&edge.from_name) {
                    repo_db.insert_edge(
                        from_id,
                        &edge.to_name,
                        &edge.kind,
                        edge.line,
                        edge.confidence.as_str(),
                    )?;
                    result.edges += 1;
                }
            }

            for (i, chunk) in chunks.iter().enumerate() {
                let embedding = if let Some(ref mut emb) = self.embedder {
                    emb.embed_one(&chunk.text).ok()
                } else {
                    None
                };
                repo_db.insert_chunk(
                    file_id,
                    i as i64,
                    &chunk.text,
                    Some(chunk.start_line as i64),
                    Some(chunk.end_line as i64),
                    embedding.as_deref(),
                )?;
                result.chunks += 1;
            }

            result.files_indexed += 1;
            log::debug!(
                "indexed {} ({}, {} symbols, {} chunks, {} edges)",
                rel_path,
                lang,
                symbols.len(),
                chunks.len(),
                edges.len(),
            );
        }

        log::info!(
            "repo '{}' done: {} indexed, {} skipped, {} symbols, {} chunks, {} edges",
            name,
            result.files_indexed,
            result.skipped,
            result.symbols,
            result.chunks,
            result.edges,
        );

        // Nothing to index: a fresh run over a folder with no indexable source
        // files. Don't leave a stub repo.db or a registry entry behind — drop
        // the handle, remove the db (and its WAL/SHM sidecars), and skip
        // registration. Only do this when we created the db this run
        // (`!db_existed`); an existing index whose files are all unchanged
        // reports zero indexed too, and must be left intact.
        if !db_existed && result.files_indexed == 0 && result.skipped == 0 {
            drop(repo_db);
            let ol_dir = repo_root.join(".ol");
            for fname in ["repo.db", "repo.db-wal", "repo.db-shm"] {
                let p = ol_dir.join(fname);
                if p.exists() {
                    if let Err(e) = std::fs::remove_file(&p) {
                        log::warn!("failed to remove empty index {}: {e}", p.display());
                    }
                }
            }
            // Remove .ol too if it's now empty; a no-op when anything else
            // (e.g. a config.toml) still lives there.
            let _ = std::fs::remove_dir(&ol_dir);
            log::info!("skipping '{name}' — nothing to index");
            return Ok(result);
        }

        // Embed the description (or repo name as fallback) whenever the embedder
        // is present. The model is already loaded for file chunks so the extra
        // call is cheap. COALESCE in repo_register preserves any stored embedding
        // when we pass None (i.e. no embedder).
        let desc_embedding = if let Some(ref mut emb) = self.embedder {
            log::debug!("generating repo description embedding");
            description
                .and_then(|d| emb.embed_one(d).ok())
                .or_else(|| emb.embed_one(&name).ok())
        } else {
            None
        };

        db.repo_register(
            &repo_root.to_string_lossy(),
            &name,
            description,
            &db_path.to_string_lossy(),
            desc_embedding.as_deref(),
        )?;

        Ok(result)
    }
}

#[derive(Debug, Default)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub symbols: usize,
    pub chunks: usize,
    pub edges: usize,
    pub skipped: usize,
}

/// Per-repo ol config, read from `<root>/.ol/config.toml` (the per-repo analog
/// of `~/.ol/config.toml`, alongside the repo's index db). Controls whether a
/// folder is indexed. A missing or malformed file means "index by default".
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub index: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self { index: true }
    }
}

/// Read `<root>/.ol/config.toml`. Tolerant: any read/parse error yields
/// defaults so a stray file never breaks indexing.
pub fn repo_config(root: &Path) -> RepoConfig {
    let path = root.join(".ol").join("config.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return RepoConfig::default();
    };
    #[derive(serde::Deserialize)]
    struct Raw {
        index: Option<bool>,
    }
    match toml::from_str::<Raw>(&contents) {
        Ok(r) => RepoConfig {
            index: r.index.unwrap_or(true),
        },
        Err(e) => {
            log::warn!("ignoring malformed {}: {e}", path.display());
            RepoConfig::default()
        }
    }
}

/// Default cap on indexable files per folder. A folder with more than this many
/// files is rejected (rather than partially indexed) unless forced. Override
/// with the `OL_MAX_INDEX_FILES` env var (bridged from `[index].max_files`).
const DEFAULT_MAX_INDEX_FILES: usize = 50_000;

fn max_index_files() -> usize {
    std::env::var("OL_MAX_INDEX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_INDEX_FILES)
}

/// True if `root` sits too high in the filesystem to ever be a sensible index
/// target: the filesystem/drive root, a direct child of it (e.g. `/Users`,
/// `C:\Users`), the home directory itself, or any ancestor of home. This is the
/// backstop against accidentally indexing an entire machine (e.g. a `~/.git`
/// dotfiles repo making `find_git_root` resolve a subdir session up to `$HOME`).
fn is_unsafe_index_root(root: &Path) -> bool {
    // `/` and `/Users` (or `C:\` and `C:\Users`) have < 3 components; a real
    // project path like `/Users/me/code/proj` has 5. Anything this shallow is
    // never a deliberate repo root.
    if root.components().count() < 3 {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        // The home dir itself, or any ancestor of it (`/Users`, `/home`).
        if root == home || home.starts_with(root) {
            return true;
        }
    }
    false
}

/// Walk `repo_root` (respecting .gitignore + hardcoded excludes + per-folder
/// opt-outs) and collect the indexable files, stopping as soon as the count
/// exceeds `max_files`. Returns the collected paths and whether the cap was
/// exceeded (in which case the caller decides to reject or force through).
fn collect_indexable_files(repo_root: &Path, max_files: usize) -> (Vec<std::path::PathBuf>, bool) {
    // Use the `ignore` crate (same engine as ripgrep) so .gitignore, .ignore,
    // and global gitignore are all automatically respected.
    let walker = ignore::WalkBuilder::new(repo_root)
        .follow_links(false)
        .hidden(false) // don't skip dot-files by default (we handle .ol ourselves)
        .git_ignore(true) // respect .gitignore
        .git_global(true) // respect ~/.config/git/ignore
        .git_exclude(true) // respect .git/info/exclude
        .require_git(false) // respect .gitignore even outside a git repo
        .filter_entry(|e| {
            // Exclude our own .ol dir and other universal build/cache noise.
            let name = e.file_name().to_str().unwrap_or("");
            if HARDCODED_IGNORED.contains(&name) {
                return false;
            }
            // Directory rules below apply only to *sub*directories; depth() > 0
            // skips re-checking the root itself.
            if e.depth() > 0 && e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                // A nested git repo is its own index target — treat its .git as
                // a boundary and don't absorb its files into this parent index.
                // Each .git level gets its own .ol.
                if is_git_repo(e.path()) {
                    return false;
                }
                // Prune a subfolder that explicitly opted out of indexing.
                return repo_config(e.path()).index;
            }
            true
        })
        .build();

    let mut files = Vec::new();
    let mut over_cap = false;
    for entry in walker
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
    {
        files.push(entry.path().to_path_buf());
        if files.len() > max_files {
            over_cap = true;
            break;
        }
    }
    (files, over_cap)
}

fn is_git_repo(p: &Path) -> bool {
    p.join(".git").exists()
}

/// How to resolve a nested (ancestor, descendant) pair of registered folders.
#[derive(Debug, PartialEq, Eq)]
enum NestedResolution {
    /// Both are real repos (nested git) — each keeps its own index.
    KeepBoth,
    /// Drop the ancestor (a spurious non-git "workspace" holding a real repo).
    DropAncestor,
    /// Drop the descendant (a plain subdir already covered by the ancestor).
    DropDescendant,
}

/// Decide what to do when one registered folder sits inside another.
/// Two git repos nested one inside the other are independent — the parent's
/// walk stops at the inner .git boundary, so each keeps its own index. A non-git
/// ancestor holding a git descendant is a spurious "workspace" folder (indexed
/// by an over-eager hook) and loses to its real repo child. Otherwise the
/// broader ancestor covers a plain descendant, so the descendant is dropped.
fn nested_resolution(ancestor_is_git: bool, descendant_is_git: bool) -> NestedResolution {
    match (ancestor_is_git, descendant_is_git) {
        (true, true) => NestedResolution::KeepBoth,
        (false, true) => NestedResolution::DropAncestor,
        (true, false) | (false, false) => NestedResolution::DropDescendant,
    }
}

/// Consolidate registry entries made redundant by nesting: when one registered
/// folder sits inside another, only one should own the overlapping code (see
/// `nested_loser`). Retracts each loser's `.ol` index and deregisters it. A
/// descendant that explicitly opted out keeps its independent index. Returns the
/// stored paths that were removed. Used by `ol repo sync` to heal registries
/// polluted by the old hook that indexed non-git folders.
pub fn prune_nested_repos(db: &Database) -> Result<Vec<String>> {
    let repos = db.repo_list()?;
    // (stored path, canonical path) for each entry.
    let canon: Vec<(String, std::path::PathBuf)> = repos
        .iter()
        .map(|r| (r.path.clone(), canonical_or_self(Path::new(&r.path))))
        .collect();

    let mut to_remove: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (i, (_, ci)) in canon.iter().enumerate() {
        for (j, (_, cj)) in canon.iter().enumerate() {
            // Only consider ci as a *proper* ancestor of cj. `starts_with` is
            // component-wise. `ci == cj` (two rows for the same real dir) is
            // left alone here — canonical registration prevents new ones.
            if i == j || ci == cj || !cj.starts_with(ci) {
                continue;
            }
            match nested_resolution(is_git_repo(ci), is_git_repo(cj)) {
                NestedResolution::KeepBoth => {}
                NestedResolution::DropDescendant => {
                    // Keep a descendant that deliberately opted out.
                    if repo_config(cj).index {
                        to_remove.insert(canon[j].0.clone());
                    }
                }
                NestedResolution::DropAncestor => {
                    to_remove.insert(canon[i].0.clone());
                }
            }
        }
    }

    let removed: Vec<String> = to_remove.into_iter().collect();
    for path in &removed {
        let ol_dir = Path::new(path).join(".ol");
        for fname in ["repo.db", "repo.db-wal", "repo.db-shm"] {
            let p = ol_dir.join(fname);
            if p.exists() {
                if let Err(e) = std::fs::remove_file(&p) {
                    log::warn!("failed to remove redundant index {}: {e}", p.display());
                }
            }
        }
        let _ = std::fs::remove_dir(&ol_dir);
        let _ = db.repo_remove(path);
    }
    Ok(removed)
}

/// Canonicalize `p`, falling back to `p` as-is if it can't be resolved.
/// Ancestry checks between a live `repo_root` and registry paths must compare
/// the same normalized form: the hooks index the session's *logical* cwd
/// (symlinks unresolved) while `ol repo index` canonicalizes, so `/tmp/x` and
/// `/private/tmp/x` denote the same dir but won't `starts_with`-match raw.
fn canonical_or_self(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Retract every registered index whose folder is a proper descendant of
/// `repo_root`: the parent now covers those files, so the nested `.ol/repo.db`
/// and its registry entry are redundant. A descendant that explicitly opted out
/// (`index = false`) keeps its own index — that is a deliberate independent one.
fn retract_nested_child_indexes(db: &Database, repo_root: &Path) -> Result<()> {
    let repo_root = canonical_or_self(repo_root);
    for r in db.repo_list()? {
        let child_raw = Path::new(&r.path);
        // Compare canonicalized; a registry entry whose dir is gone fails to
        // canonicalize and is skipped. `starts_with` is component-wise.
        let Ok(child) = child_raw.canonicalize() else {
            continue;
        };
        if child == repo_root || !child.starts_with(&repo_root) {
            continue;
        }
        if is_git_repo(&child) {
            continue; // a nested git repo is its own index, not covered here
        }
        if !repo_config(child_raw).index {
            continue; // explicit opt-out: leave the independent child index be
        }
        // fs / registry ops use the stored path form (where the index lives).
        let ol_dir = child_raw.join(".ol");
        let mut retracted = false;
        for fname in ["repo.db", "repo.db-wal", "repo.db-shm"] {
            let p = ol_dir.join(fname);
            if p.exists() {
                match std::fs::remove_file(&p) {
                    Ok(()) => retracted = true,
                    Err(e) => log::warn!("failed to remove nested index {}: {e}", p.display()),
                }
            }
        }
        // Remove .ol too if it's now empty (a no-op when e.g. a config.toml stays).
        let _ = std::fs::remove_dir(&ol_dir);
        if db.repo_remove(&r.path)? {
            retracted = true;
        }
        if retracted {
            log::info!(
                "retracted nested index under {} — covered by parent {}",
                child_raw.display(),
                repo_root.display()
            );
        }
    }
    Ok(())
}

/// Find a registered repo whose directory is a proper ancestor of `repo_root`.
/// Used to skip indexing a subfolder that a parent index already covers.
/// Returns the ancestor's (stored) path if one is registered, else `None`.
fn parent_indexed_repo(db: &Database, repo_root: &Path) -> Result<Option<String>> {
    // A git repo is its own index even when nested inside another repo — each
    // .git level gets its own .ol, and the parent's walk stops at the boundary
    // so it never covered this repo anyway. Only a non-git folder is deemed
    // covered by a parent index.
    if is_git_repo(repo_root) {
        return Ok(None);
    }
    let repo_root = canonical_or_self(repo_root);
    for r in db.repo_list()? {
        // Canonicalize both sides so differing path forms still match. A
        // registry entry whose directory is gone fails to canonicalize and is
        // skipped — that doubles as the stale-parent guard. `starts_with` is
        // component-wise, so `/a/bc` is not treated as under `/a/b`, and the
        // equality check excludes the repo itself (only a proper ancestor
        // short-circuits, so re-indexing the same root still works).
        let Ok(ancestor) = Path::new(&r.path).canonicalize() else {
            continue;
        };
        if repo_root != ancestor && repo_root.starts_with(&ancestor) {
            return Ok(Some(r.path));
        }
    }
    Ok(None)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            r#"fn main() { println!("hello"); }
fn add(a: i32, b: i32) -> i32 { a + b }
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("lib.py"),
            r#"def process(data):
    return data.strip()
class Handler:
    pass
"#,
        )
        .unwrap();
        // File that should be ignored
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target").join("main.rs"), "ignored").unwrap();
        dir
    }

    #[test]
    fn test_index_repo_basic() {
        let repo_dir = make_test_repo();
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None); // no embedder for tests

        let result = indexer
            .index_repo(&primary_db, repo_dir.path(), Some("Test repo"))
            .unwrap();

        assert!(result.files_indexed >= 2, "should index .rs and .py");
        assert!(result.symbols > 0, "should find symbols");
        assert!(result.chunks > 0, "should produce chunks");

        // Check it's registered in primary db
        let repos = primary_db.repo_list().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].description.as_deref(), Some("Test repo"));
    }

    #[test]
    fn test_opt_out_retracts_stale_index() {
        // Index a repo, then opt it out and re-run: the stale repo.db and the
        // registry entry must be retracted, while .ol/config.toml survives.
        let repo_dir = make_test_repo();
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);

        indexer
            .index_repo(&primary_db, repo_dir.path(), None)
            .unwrap();
        let db_path = repo_dir.path().join(".ol").join("repo.db");
        assert!(db_path.exists(), "index built");
        assert_eq!(primary_db.repo_list().unwrap().len(), 1, "registered");

        // Opt out and re-run (as a hook would).
        let cfg = repo_dir.path().join(".ol").join("config.toml");
        std::fs::write(&cfg, "index = false\n").unwrap();
        let result = indexer
            .index_repo(&primary_db, repo_dir.path(), None)
            .unwrap();

        assert_eq!(result.files_indexed, 0, "opted-out: nothing indexed");
        assert!(!db_path.exists(), "stale repo.db retracted");
        assert!(cfg.exists(), "opt-out marker config.toml preserved");
        assert!(
            primary_db.repo_list().unwrap().is_empty(),
            "registry entry deregistered"
        );
    }

    #[test]
    fn test_empty_folder_creates_no_index() {
        // A folder with no indexable source files must not leave a stub repo.db
        // or a registry entry behind.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("README"), "just prose, no code\n").unwrap();

        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);
        let result = indexer.index_repo(&primary_db, dir.path(), None).unwrap();

        assert_eq!(result.files_indexed, 0, "nothing indexable");
        assert_eq!(result.skipped, 0);
        assert!(
            !dir.path().join(".ol").join("repo.db").exists(),
            "stub repo.db should not be left behind"
        );
        assert!(
            primary_db.repo_list().unwrap().is_empty(),
            "empty folder should not be registered"
        );
    }

    #[test]
    fn test_subfolder_skipped_when_parent_indexed() {
        // Index a parent repo, then attempt to index a subfolder: the parent's
        // index already covers it, so no nested index is created.
        let parent = make_test_repo();
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);
        indexer
            .index_repo(&primary_db, parent.path(), None)
            .unwrap();
        assert_eq!(primary_db.repo_list().unwrap().len(), 1, "parent indexed");

        // A subfolder with its own indexable file.
        let sub = parent.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("nested.rs"), "fn nested() {}").unwrap();

        let result = indexer.index_repo(&primary_db, &sub, None).unwrap();
        assert_eq!(result.files_indexed, 0, "subfolder indexing skipped");
        assert!(
            !sub.join(".ol").join("repo.db").exists(),
            "no nested repo.db under an already-indexed parent"
        );
        assert_eq!(
            primary_db.repo_list().unwrap().len(),
            1,
            "still only the parent is registered"
        );
    }

    #[test]
    fn test_parent_indexed_repo_ignores_stale_entry() {
        // A registry entry whose directory no longer exists is stale and must
        // not be reported as a covering parent.
        let db = Database::open_in_memory().unwrap();
        let missing = std::env::temp_dir().join("ol-nonexistent-parent-xyz-123");
        assert!(!missing.exists(), "test precondition: path must not exist");
        db.repo_register(
            &missing.to_string_lossy(),
            "ghost",
            None,
            &missing.join(".ol").join("repo.db").to_string_lossy(),
            None,
        )
        .unwrap();

        // A descendant of the stale entry: not covered, because the parent is gone.
        let child = missing.join("sub");
        assert_eq!(parent_indexed_repo(&db, &child).unwrap(), None);
    }

    #[test]
    fn test_parent_indexed_repo_matches_live_ancestor() {
        // A registry entry whose directory exists and is a proper ancestor is
        // reported as the covering parent.
        let parent = TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();
        db.repo_register(
            &parent.path().to_string_lossy(),
            "live",
            None,
            &parent.path().join(".ol").join("repo.db").to_string_lossy(),
            None,
        )
        .unwrap();

        // The child must exist so both sides canonicalize (as in the real flow,
        // where repo_root is always a real directory being indexed).
        let child = parent.path().join("sub");
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(
            parent_indexed_repo(&db, &child).unwrap().as_deref(),
            Some(parent.path().to_string_lossy().as_ref())
        );
        // The parent itself is not its own parent.
        assert_eq!(parent_indexed_repo(&db, parent.path()).unwrap(), None);
    }

    #[test]
    fn test_is_unsafe_index_root() {
        // Filesystem/drive root and shallow top-level dirs are refused.
        assert!(is_unsafe_index_root(Path::new("/")));
        assert!(is_unsafe_index_root(Path::new("/Users")));
        assert!(is_unsafe_index_root(Path::new("/home")));
        // A normal deep project path is fine (not home on any realistic CI).
        assert!(!is_unsafe_index_root(Path::new(
            "/Users/nobody/code/some-project"
        )));
        // The home dir itself is refused.
        if let Some(home) = dirs::home_dir() {
            assert!(is_unsafe_index_root(&home));
        }
    }

    #[test]
    fn test_collect_files_respects_cap() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "fn a() {}").unwrap();
        }
        // Over the cap: collection stops just past it and flags over_cap.
        let (files, over) = collect_indexable_files(dir.path(), 3);
        assert!(over, "5 files should exceed a cap of 3");
        assert!(files.len() <= 4, "collection stops right after the cap");
        // Under the cap: everything collected, no flag.
        let (files, over) = collect_indexable_files(dir.path(), 100);
        assert!(!over);
        assert_eq!(files.len(), 5);
    }

    #[test]
    fn test_index_rejects_over_cap_but_force_indexes() {
        let dir = TempDir::new().unwrap();
        for i in 0..4 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "fn a() {}").unwrap();
        }
        let db = Database::open_in_memory().unwrap();

        // Cap of 1: the folder is rejected, nothing created or registered.
        let mut indexer = RepoIndexer::new(None).with_max_files(1);
        let result = indexer.index_repo(&db, dir.path(), None).unwrap();
        assert_eq!(result.files_indexed, 0, "over-cap folder rejected");
        assert!(
            !dir.path().join(".ol").join("repo.db").exists(),
            "no index created for a rejected folder"
        );
        assert!(db.repo_list().unwrap().is_empty(), "not registered");

        // --force bypasses the cap.
        let mut indexer = RepoIndexer::new(None).with_max_files(1).with_force(true);
        let result = indexer.index_repo(&db, dir.path(), None).unwrap();
        assert!(result.files_indexed > 0, "force indexes past the cap");
        assert!(dir.path().join(".ol").join("repo.db").exists());
    }

    #[test]
    fn test_nested_child_index_retracted_on_parent_index() {
        // Index a subfolder on its own, then index the parent: the parent now
        // covers the subtree, so the nested child index is retracted.
        let parent = TempDir::new().unwrap();
        std::fs::write(parent.path().join("main.rs"), "fn main() {}").unwrap();
        let sub = parent.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("nested.rs"), "fn nested() {}").unwrap();

        let db = Database::open_in_memory().unwrap();
        RepoIndexer::new(None).index_repo(&db, &sub, None).unwrap();
        assert!(sub.join(".ol").join("repo.db").exists(), "child indexed");
        assert_eq!(db.repo_list().unwrap().len(), 1);

        let result = RepoIndexer::new(None)
            .index_repo(&db, parent.path(), None)
            .unwrap();
        assert!(result.files_indexed > 0, "parent indexed its files");
        assert!(
            !sub.join(".ol").join("repo.db").exists(),
            "nested child repo.db retracted"
        );
        let repos = db.repo_list().unwrap();
        assert_eq!(repos.len(), 1, "only the parent remains registered");
        // index_repo canonicalizes before registering, so compare canonical.
        assert_eq!(
            repos[0].path,
            parent.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn test_nested_opted_out_child_preserved_and_excluded() {
        // A subfolder that explicitly opts out keeps its own index AND is
        // excluded from the parent's index.
        let parent = TempDir::new().unwrap();
        std::fs::write(parent.path().join("main.rs"), "fn main() {}").unwrap();
        let sub = parent.path().join("vendored");
        std::fs::create_dir_all(sub.join(".ol")).unwrap();
        std::fs::write(sub.join(".ol").join("config.toml"), "index = false\n").unwrap();
        std::fs::write(sub.join("thing.rs"), "fn thing() {}").unwrap();
        std::fs::write(sub.join(".ol").join("repo.db"), b"stub").unwrap();

        let db = Database::open_in_memory().unwrap();
        db.repo_register(
            &sub.to_string_lossy(),
            "vendored",
            None,
            &sub.join(".ol").join("repo.db").to_string_lossy(),
            None,
        )
        .unwrap();

        RepoIndexer::new(None)
            .index_repo(&db, parent.path(), None)
            .unwrap();

        // Opted-out child index untouched.
        assert!(
            sub.join(".ol").join("repo.db").exists(),
            "opted-out child index preserved"
        );
        // Its files were not absorbed into the parent index.
        let repo_db = RepoDb::open(&parent.path().join(".ol").join("repo.db")).unwrap();
        let files = repo_db.list_files().unwrap();
        assert!(
            !files.iter().any(|f| f.path.starts_with("vendored/")),
            "opted-out subtree excluded from parent index"
        );
    }

    #[test]
    fn test_parent_detected_across_path_forms() {
        // The parent is registered under one path form (e.g. the hook's logical
        // cwd) while the subfolder is queried under the canonicalized form (e.g.
        // `ol repo index`). They must still match. On macOS TempDir lives under
        // /var → /private/var, giving two real forms for the same directory.
        let parent = TempDir::new().unwrap();
        let raw = parent.path();
        let canonical = raw.canonicalize().unwrap();
        // Only meaningful when the two forms actually differ on this platform.
        if raw == canonical {
            return;
        }

        let db = Database::open_in_memory().unwrap();
        // Register the parent under the RAW (non-canonical) form.
        db.repo_register(
            &raw.to_string_lossy(),
            "parent",
            None,
            &raw.join(".ol").join("repo.db").to_string_lossy(),
            None,
        )
        .unwrap();

        // Query a subfolder under the CANONICAL form.
        let child = canonical.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        assert!(
            parent_indexed_repo(&db, &child).unwrap().is_some(),
            "parent must be detected despite differing path forms"
        );
    }

    #[test]
    fn test_nested_resolution_rule() {
        // Two nested git repos are independent → keep both.
        assert_eq!(nested_resolution(true, true), NestedResolution::KeepBoth);
        // git ancestor with a plain (non-git) subdir → drop the subdir.
        assert_eq!(
            nested_resolution(true, false),
            NestedResolution::DropDescendant
        );
        // non-git workspace parent with a real repo child → drop the parent.
        assert_eq!(
            nested_resolution(false, true),
            NestedResolution::DropAncestor
        );
        // neither is a repo → keep the broader ancestor.
        assert_eq!(
            nested_resolution(false, false),
            NestedResolution::DropDescendant
        );
    }

    /// Register `path` with a stub `.ol/repo.db` so pruning has something to
    /// retract. Optionally mark it a git repo.
    fn register_with_index(db: &Database, path: &Path, git: bool) {
        std::fs::create_dir_all(path.join(".ol")).unwrap();
        std::fs::write(path.join(".ol").join("repo.db"), b"stub").unwrap();
        if git {
            std::fs::create_dir_all(path.join(".git")).unwrap();
        }
        db.repo_register(
            &path.to_string_lossy(),
            path.file_name().unwrap().to_str().unwrap(),
            None,
            &path.join(".ol").join("repo.db").to_string_lossy(),
            None,
        )
        .unwrap();
    }

    #[test]
    fn test_prune_drops_spurious_non_git_parent() {
        // A non-git workspace folder holding a git-repo child: the parent is
        // the over-eager-hook artifact and must be dropped, the child kept.
        let ws = TempDir::new().unwrap();
        let child = ws.path().join("repo-a");
        std::fs::create_dir_all(&child).unwrap();

        let db = Database::open_in_memory().unwrap();
        register_with_index(&db, ws.path(), false); // non-git parent
        register_with_index(&db, &child, true); // git child

        let removed = prune_nested_repos(&db).unwrap();
        assert_eq!(removed.len(), 1, "only the parent removed");
        assert_eq!(removed[0], ws.path().to_string_lossy());

        let remaining = db.repo_list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, child.to_string_lossy());
        assert!(
            !ws.path().join(".ol").join("repo.db").exists(),
            "spurious parent index retracted"
        );
        assert!(
            child.join(".ol").join("repo.db").exists(),
            "child index preserved"
        );
    }

    #[test]
    fn test_prune_keeps_nested_git_repos() {
        // Two git repos nested one inside the other are independent: the parent
        // walk stops at the inner .git boundary, so both keep their own index.
        let parent = TempDir::new().unwrap();
        std::fs::create_dir_all(parent.path().join(".git")).unwrap();
        let child = parent.path().join("sub");
        std::fs::create_dir_all(&child).unwrap();

        let db = Database::open_in_memory().unwrap();
        register_with_index(&db, parent.path(), true); // git parent
        register_with_index(&db, &child, true); // nested git child

        let removed = prune_nested_repos(&db).unwrap();
        assert!(removed.is_empty(), "nested git repos are both kept");
        assert_eq!(db.repo_list().unwrap().len(), 2);
    }

    #[test]
    fn test_prune_drops_plain_subdir_under_git_parent() {
        // A git repo with a plain (non-git) subdir spuriously registered: the
        // subdir is covered by the parent, so its entry is dropped.
        let parent = TempDir::new().unwrap();
        std::fs::create_dir_all(parent.path().join(".git")).unwrap();
        let child = parent.path().join("sub");
        std::fs::create_dir_all(&child).unwrap();

        let db = Database::open_in_memory().unwrap();
        register_with_index(&db, parent.path(), true); // git parent
        register_with_index(&db, &child, false); // plain subdir

        let removed = prune_nested_repos(&db).unwrap();
        assert_eq!(removed, vec![child.to_string_lossy().to_string()]);
        let remaining = db.repo_list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, parent.path().to_string_lossy());
    }

    #[test]
    fn test_prune_keeps_opted_out_descendant() {
        // A git parent with a child that explicitly opted out: the child is an
        // intentional independent index and must survive consolidation.
        let parent = TempDir::new().unwrap();
        std::fs::create_dir_all(parent.path().join(".git")).unwrap();
        let child = parent.path().join("vendored");
        std::fs::create_dir_all(child.join(".ol")).unwrap();
        std::fs::write(child.join(".ol").join("config.toml"), "index = false\n").unwrap();

        let db = Database::open_in_memory().unwrap();
        register_with_index(&db, parent.path(), true);
        register_with_index(&db, &child, false);

        let removed = prune_nested_repos(&db).unwrap();
        assert!(
            removed.is_empty(),
            "opted-out descendant must not be pruned"
        );
        assert_eq!(db.repo_list().unwrap().len(), 2);
    }

    #[test]
    fn test_nested_git_repos_index_separately() {
        // A git repo containing a nested git repo: the outer index must stop at
        // the inner .git boundary (not absorb inner files), and the inner repo
        // must get its own index — one .ol per .git level.
        let outer = TempDir::new().unwrap();
        std::fs::create_dir_all(outer.path().join(".git")).unwrap();
        std::fs::write(outer.path().join("outer.rs"), "fn outer() {}").unwrap();
        let inner = outer.path().join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        std::fs::write(inner.join("inner.rs"), "fn inner() {}").unwrap();

        let db = Database::open_in_memory().unwrap();

        // Index the outer repo: its index must NOT contain inner/inner.rs.
        RepoIndexer::new(None)
            .index_repo(&db, outer.path(), None)
            .unwrap();
        let outer_db = RepoDb::open(&outer.path().join(".ol").join("repo.db")).unwrap();
        let outer_files = outer_db.list_files().unwrap();
        assert!(
            outer_files.iter().any(|f| f.path == "outer.rs"),
            "outer indexes its own file"
        );
        assert!(
            !outer_files.iter().any(|f| f.path.starts_with("inner/")),
            "outer must not absorb the nested repo's files"
        );

        // Index the inner repo: it is NOT skipped and gets its own .ol.
        let result = RepoIndexer::new(None)
            .index_repo(&db, &inner, None)
            .unwrap();
        assert!(result.files_indexed > 0, "nested repo indexed on its own");
        assert!(
            inner.join(".ol").join("repo.db").exists(),
            "inner repo has its own index"
        );
        // Both levels registered.
        assert_eq!(db.repo_list().unwrap().len(), 2);
    }

    #[test]
    fn test_index_skips_ignored_dirs() {
        let repo_dir = make_test_repo();
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);

        let result = indexer
            .index_repo(&primary_db, repo_dir.path(), None)
            .unwrap();

        // target/main.rs should be skipped
        let db_path = repo_dir.path().join(".ol").join("repo.db");
        let repo_db = RepoDb::open(&db_path).unwrap();
        let files = repo_db.list_files().unwrap();
        assert!(
            !files.iter().any(|f| f.path.starts_with("target/")),
            "target/ should be excluded"
        );
        let _ = result;
    }

    #[test]
    fn test_index_incremental_skips_unchanged() {
        let repo_dir = make_test_repo();
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);

        let r1 = indexer
            .index_repo(&primary_db, repo_dir.path(), None)
            .unwrap();
        let r2 = indexer
            .index_repo(&primary_db, repo_dir.path(), None)
            .unwrap();

        assert!(r1.files_indexed > 0);
        // Second run: no files should be re-indexed (all hashes match)
        assert_eq!(r2.files_indexed, 0);
        assert!(r2.skipped >= r1.files_indexed);
    }

    #[test]
    fn test_sha256_is_deterministic() {
        let data = b"hello world";
        assert_eq!(sha256_hex(data), sha256_hex(data));
        assert_ne!(sha256_hex(data), sha256_hex(b"different"));
    }

    #[test]
    fn test_gitignore_excludes_dirs() {
        let dir = TempDir::new().unwrap();
        // Write source files
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        // Write a .gitignore that excludes a build-output dir
        std::fs::write(dir.path().join(".gitignore"), "build_output/\n").unwrap();
        // Create the excluded dir with a source file inside
        std::fs::create_dir_all(dir.path().join("build_output")).unwrap();
        std::fs::write(
            dir.path().join("build_output").join("gen.rs"),
            "fn gen() {}",
        )
        .unwrap();

        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);
        let _ = indexer.index_repo(&primary_db, dir.path(), None).unwrap();

        let db_path = dir.path().join(".ol").join("repo.db");
        let repo_db = RepoDb::open(&db_path).unwrap();
        let files = repo_db.list_files().unwrap();
        assert!(
            !files.iter().any(|f| f.path.starts_with("build_output/")),
            ".gitignore exclusion should be respected"
        );
        assert!(
            files.iter().any(|f| f.path == "main.rs"),
            "main.rs should still be indexed"
        );
    }

    #[test]
    fn test_large_file_falls_back_to_line_chunks() {
        let dir = TempDir::new().unwrap();

        // ~480 bytes - small file, but we set a tiny limit so the fallback triggers
        // without needing to write a huge file in a test.
        let source = "def func_a():\n    pass\n\n".repeat(20);
        std::fs::write(dir.path().join("normal.py"), &source).unwrap();

        // Set limit to 100 bytes so our ~480-byte file triggers the fallback
        std::env::set_var("OL_MAX_INDEX_FILE_BYTES", "100");
        let primary_db = Database::open_in_memory().unwrap();
        let mut indexer = RepoIndexer::new(None);
        let result = indexer.index_repo(&primary_db, dir.path(), None).unwrap();
        std::env::remove_var("OL_MAX_INDEX_FILE_BYTES");

        assert_eq!(
            result.files_indexed, 1,
            "file should be indexed via fallback"
        );
        assert!(result.chunks > 0, "should produce line-based chunks");
        assert_eq!(result.symbols, 0, "no symbols when tree-sitter is skipped");
    }
}
