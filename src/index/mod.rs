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
}

impl<'a> RepoIndexer<'a> {
    pub fn new(embedder: Option<&'a mut Embedder>) -> Self {
        Self { embedder }
    }

    /// Index a repo at `repo_root`, store results in `repo_root/.ol/repo.db`,
    /// and register it in the primary `db`.
    pub fn index_repo(
        &mut self,
        db: &Database,
        repo_root: &Path,
        description: Option<&str>,
    ) -> Result<IndexResult> {
        let name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

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

        log::info!("indexing repo '{}' at {}", name, repo_root.display());
        let db_path = repo_root.join(".ol").join("repo.db");
        let repo_db = RepoDb::open(&db_path)?;

        repo_db.set_meta("repo_root", &repo_root.to_string_lossy())?;
        repo_db.set_meta("name", &name)?;

        let mut result = IndexResult::default();

        // Use the `ignore` crate (same engine as ripgrep) so .gitignore,
        // .ignore, and global gitignore are all automatically respected.
        let walker = ignore::WalkBuilder::new(repo_root)
            .follow_links(false)
            .hidden(false) // don't skip dot-files by default (we handle .ol ourselves)
            .git_ignore(true) // respect .gitignore
            .git_global(true) // respect ~/.config/git/ignore
            .git_exclude(true) // respect .git/info/exclude
            .require_git(false) // respect .gitignore even outside a git repo
            .filter_entry(|e| {
                // Still explicitly exclude our own .ol dir and other noise
                let name = e.file_name().to_str().unwrap_or("");
                !HARDCODED_IGNORED.contains(&name)
            })
            .build();

        for entry in walker
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            let path = entry.path();
            let rel_path = match path.strip_prefix(repo_root) {
                Ok(p) => p.to_string_lossy().to_string(),
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
