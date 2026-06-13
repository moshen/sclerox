pub mod parser;
pub mod repo_db;

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::Database;
use crate::embed::Embedder;

use parser::{detect_language, parse_file};
use repo_db::RepoDb;

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

            let (symbols, raw_chunks) = parse_file(&source, effective_lang, CHUNK_SIZE_LINES);

            // Split any chunk that exceeds the embedding model's context window.
            // Tree-sitter emits whole functions regardless of size; large ones get sub-chunked.
            let chunks: Vec<parser::CodeChunk> = raw_chunks
                .into_iter()
                .flat_map(|c| parser::split_large_chunk(c, MAX_EMBED_CHARS, SPLIT_OVERLAP_LINES))
                .collect();

            for sym in &symbols {
                repo_db.insert_symbol(
                    file_id,
                    &sym.kind,
                    &sym.name,
                    sym.signature.as_deref(),
                    sym.start_line as i64,
                    sym.end_line as i64,
                )?;
                result.symbols += 1;
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
                "indexed {} ({}, {} symbols, {} chunks)",
                rel_path,
                lang,
                symbols.len(),
                chunks.len()
            );
        }

        log::info!(
            "repo '{}' done: {} indexed, {} skipped, {} symbols, {} chunks",
            name,
            result.files_indexed,
            result.skipped,
            result.symbols,
            result.chunks
        );

        // Only (re-)embed the description when something actually changed.
        // Loading the ONNX model costs ~280MB RSS; skip it on no-op runs.
        // The upsert uses COALESCE so passing None preserves any existing embedding.
        let desc_embedding = if result.files_indexed > 0 {
            if let Some(ref mut emb) = self.embedder {
                log::debug!("generating repo description embedding");
                description
                    .and_then(|d| emb.embed_one(d).ok())
                    .or_else(|| emb.embed_one(&name).ok())
            } else {
                None
            }
        } else {
            log::debug!("no files changed, skipping description re-embedding");
            None // repo_register COALESCE preserves the stored embedding
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
    pub skipped: usize,
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
