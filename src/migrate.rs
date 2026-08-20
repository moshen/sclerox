//! Legacy `ol` → `sclerox` per-repo directory migration.
//!
//! This is the lib-visible half of the rename/XDG migration (see
//! `src/cli/migrate.rs` for the `sclerox migrate` subcommand, which handles the
//! global `~/.ol/*` → XDG move and stale tool-integration cleanup). Kept
//! separate and minimal because it's called from `index::RepoIndexer`, which
//! is shared by both the `sclerox` binary and the library test target.
//!
//! Self-contained on purpose: once upgraders have had a couple of releases to
//! run `sclerox repo index` / `sclerox repo sync` at least once, this whole module
//! (and its one call site in `index/mod.rs`) can be deleted.

use std::path::Path;

/// If `repo_root/.ol` exists and `repo_root/.sclerox` doesn't, rename it in place
/// so `repo.db` / `config.toml` become `.sclerox`'s without re-indexing. No-op
/// (returns `false`) if there's nothing to migrate — e.g. this repo was
/// always `.sclerox`-only, or was already migrated.
pub fn migrate_legacy_repo_dir(repo_root: &Path) -> bool {
    let legacy = repo_root.join(".ol");
    let current = repo_root.join(".sclerox");
    if !legacy.exists() || current.exists() {
        return false;
    }
    match std::fs::rename(&legacy, &current) {
        Ok(()) => {
            log::info!(
                "migrated legacy {} -> {}",
                legacy.display(),
                current.display()
            );
            true
        }
        Err(e) => {
            log::warn!("failed to migrate legacy {}: {e}", legacy.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migrates_legacy_dir_in_place() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join(".ol");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("repo.db"), b"stub").unwrap();

        assert!(migrate_legacy_repo_dir(dir.path()));
        assert!(!legacy.exists());
        assert!(dir.path().join(".sclerox").join("repo.db").exists());
    }

    #[test]
    fn noop_when_no_legacy_dir() {
        let dir = TempDir::new().unwrap();
        assert!(!migrate_legacy_repo_dir(dir.path()));
    }

    #[test]
    fn noop_when_both_exist_leaves_sclerox_untouched() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".ol")).unwrap();
        std::fs::create_dir_all(dir.path().join(".sclerox")).unwrap();
        std::fs::write(dir.path().join(".sclerox").join("repo.db"), b"real").unwrap();

        assert!(!migrate_legacy_repo_dir(dir.path()));
        assert_eq!(
            std::fs::read(dir.path().join(".sclerox").join("repo.db")).unwrap(),
            b"real"
        );
    }
}
