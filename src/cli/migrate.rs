//! `sclerox migrate` — one-time cleanup for a machine that has an old, pre-rename
//! `ol` install: moves `~/.ol/*` onto the new XDG layout and strips
//! old-marker tool integrations (hooks, skill dir, OpenCode plugin, doc
//! sections) that `sclerox install` doesn't recognize as its own, and renames
//! per-repo `.ol/` index directories. See `src/skill/reference/migration.md`
//! for the user-facing procedure.
//!
//! Self-contained on purpose: everything specific to the `ol` → `sclerox`
//! transition lives here (plus `crate::migrate` for the per-repo half), so
//! this file — and its one `Commands::Migrate` arm in `cli/mod.rs` — can be
//! deleted wholesale once upgraders have had a couple of releases to run it.
//!
//! Explicit command, not automatic-on-startup: an automatic migration could
//! race a concurrent `ol` process still writing to `~/.ol/ol.db` (the session
//! hooks spawn background processes). Dry-run capable and prints every
//! action, mirroring `install.rs`.

use anyhow::{Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};

use super::install::{claude_dir, codex_dir, opencode_dir, read_json, write_json};

#[derive(Args)]
pub struct MigrateArgs {
    /// Show what would be moved/removed without changing anything
    #[arg(long)]
    dry_run: bool,
}

pub fn run_migrate(args: MigrateArgs) -> Result<()> {
    let mut did_anything = false;

    match legacy_home_dir() {
        Some(legacy_home) => {
            did_anything |= migrate_global_paths(&legacy_home, args.dry_run)?;
        }
        None => println!("No home directory found; skipping global path migration."),
    }

    did_anything |= strip_legacy_integrations(args.dry_run)?;
    did_anything |= migrate_registered_repos(args.dry_run)?;

    if !did_anything {
        println!("Nothing to migrate — already on the sclerox / XDG layout.");
    } else if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    } else {
        println!("\nMigration complete.");
    }

    warn_about_stale_ol_binary();
    Ok(())
}

/// Tell the user about an old `ol` binary still on PATH. Printed after every
/// migrate run (including "nothing to migrate"), because the binary outliving
/// the data is precisely the state that produces silent empty results.
fn warn_about_stale_ol_binary() {
    let Some(path) = stale_ol_binary() else {
        return;
    };
    println!(
        "\nWarning: an old `ol` binary is still on your PATH:\n  \
         {}\n\
         Its database has moved. Running it now does not fail — it creates a new\n\
         EMPTY ~/.ol/ol.db and returns no results with exit 0, so anything still\n\
         calling `ol ...` will silently read an empty knowledge base.\n\
         Remove it once you are happy with this migration, and update any of your\n\
         own docs, skills, or aliases that still invoke `ol` to use `sclerox`.",
        path.display()
    );
}

/// `~/.ol`, the pre-rename flat layout every real install actually wrote
/// (nothing shipped under the `sclerox` name before the XDG move existed).
fn legacy_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ol"))
}

/// True if migration still has work to do. Used by `sclerox install` to point
/// at `sclerox migrate` when it finds an old install.
///
/// Deliberately NOT "does `~/.ol` exist": that directory routinely outlives a
/// successful migration because it holds files migration never claims (an
/// `ol.db.pre-v12-backup`, say). Reporting on mere existence made `install`
/// advise a migrate that `migrate` itself then reported as already done.
pub fn legacy_data_present() -> bool {
    let paths_pending = legacy_home_dir().is_some_and(|home| {
        global_path_moves(&home)
            .iter()
            .any(|(_, dst)| !dst.exists())
    });
    if paths_pending || legacy_integrations_present() || !legacy_repos().is_empty() {
        return true;
    }
    let ancestors = crate::db::Database::open(&crate::config::settings().db_path)
        .ok()
        .and_then(|d| d.repo_list().ok())
        .map(|rs| rs.into_iter().map(|r| r.path).collect::<Vec<_>>())
        .unwrap_or_default();
    !legacy_ancestor_dirs(&ancestors).is_empty()
}

/// True if any old-marker tool integration is still installed. These are
/// independent of `~/.ol`: a machine whose data moved can still carry an
/// `ol-kb` skill dir or an `# ol-kb-hook` entry.
fn legacy_integrations_present() -> bool {
    let skill_or_plugin = [
        claude_dir().map(|d| d.join("skills").join(LEGACY_SKILL_DIR_NAME)),
        opencode_dir().map(|d| d.join("skills").join(LEGACY_SKILL_DIR_NAME)),
        opencode_dir().map(|d| d.join("plugins").join(LEGACY_OPENCODE_PLUGIN_FILE)),
    ]
    .into_iter()
    .flatten()
    .any(|p| p.exists());

    let sections = [
        claude_dir().map(|d| d.join("CLAUDE.md")),
        opencode_dir().map(|d| d.join("AGENTS.md")),
        codex_dir().map(|d| d.join("instructions.md")),
    ]
    .into_iter()
    .flatten()
    .any(|p| {
        std::fs::read_to_string(&p)
            .map(|c| c.contains(LEGACY_SECTION_MARKER))
            .unwrap_or(false)
    });

    skill_or_plugin || sections || legacy_hook_present()
}

fn legacy_hook_present() -> bool {
    let Ok(dir) = claude_dir() else {
        return false;
    };
    std::fs::read_to_string(dir.join("settings.json"))
        .map(|c| c.contains(LEGACY_HOOK_MARKER))
        .unwrap_or(false)
}

/// Find an executable named `ol` on PATH, if one is still installed.
///
/// Worth a warning because the old binary does not fail once its database has
/// moved: it silently creates a fresh empty `~/.ol/ol.db` and returns no
/// results with exit 0. Any doc, skill, or habit still invoking `ol ...` then
/// reads an empty knowledge base and reports "not found" rather than erroring.
pub fn stale_ol_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_legacy_binary(&path, |p| p.is_file())
}

/// Pure half of [`stale_ol_binary`] so it is testable without touching the
/// process environment.
fn find_legacy_binary(
    path_var: &std::ffi::OsStr,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(legacy_binary_name()))
        .find(|cand| exists(cand))
}

fn legacy_binary_name() -> &'static str {
    if cfg!(windows) {
        "ol.exe"
    } else {
        "ol"
    }
}

// ─── Per-repo index directories (<repo>/.ol → <repo>/.sclerox) ──────────────

/// A registered repo still carrying a pre-rename `.ol/` index directory.
struct LegacyRepo {
    path: String,
    name: String,
    new_db_path: PathBuf,
}

/// True if a recorded `db_path` still points inside a `.ol/` index directory.
///
/// Compares whole path components so a repo that merely has `.ol` inside a
/// longer name (`~/code/tools.old/...`) is not mistaken for a legacy index.
fn points_at_legacy_index(db_path: &str) -> bool {
    Path::new(db_path)
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".ol"))
}

/// Registered repos whose index directory is still `.ol/`.
///
/// Keyed off the recorded `db_path` rather than the directory alone, because
/// the registry is what `sclerox code search` actually reads: a repo whose
/// folder was renamed but whose row still points into `.ol/` is the broken
/// state worth reporting.
fn legacy_repos() -> Vec<LegacyRepo> {
    let Ok(db) = crate::db::Database::open(&crate::config::settings().db_path) else {
        return Vec::new();
    };
    let Ok(repos) = db.repo_list() else {
        return Vec::new();
    };
    repos
        .into_iter()
        .filter_map(|r| {
            let legacy_dir = Path::new(&r.path).join(".ol");
            (points_at_legacy_index(&r.db_path) || legacy_dir.exists()).then(|| LegacyRepo {
                new_db_path: Path::new(&r.path).join(".sclerox").join("repo.db"),
                path: r.path,
                name: r.name,
            })
        })
        .collect()
}

/// Unregistered folders carrying a legacy `.ol/` directory, found by walking up
/// from each registered repo to the home directory.
///
/// These are almost always per-folder opt-out markers (`.ol/config.toml` with
/// `index = false`) on a catch-all parent that is deliberately not indexed, so
/// it never appears in the registry and the repo sweep cannot see it. Bounded
/// to ancestors of known repos rather than scanning the filesystem.
fn legacy_ancestor_dirs(repo_paths: &[String]) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut seen = std::collections::BTreeSet::new();

    for path in repo_paths {
        for ancestor in Path::new(path).ancestors().skip(1) {
            // Stop at (and do not include) the home directory: `~/.ol` is the
            // global install, handled by the global path migration.
            if home.as_deref().is_some_and(|h| ancestor == h) {
                break;
            }
            if ancestor.parent().is_none() {
                break;
            }
            if ancestor.join(".ol").is_dir() && !ancestor.join(".sclerox").exists() {
                seen.insert(ancestor.to_path_buf());
            }
        }
    }
    seen.into_iter().collect()
}

/// Rename legacy `.ol/` directories on unregistered ancestor folders.
///
/// Left alone these silently change behaviour rather than break: `repo_config`
/// falls back to reading the legacy marker, but the folder keeps a stale
/// directory name that nothing else recognises.
fn migrate_ancestor_dirs(repo_paths: &[String], dry_run: bool) -> Result<bool> {
    let mut did_anything = false;
    for dir in legacy_ancestor_dirs(repo_paths) {
        if dry_run {
            println!(
                "  would migrate folder config: {}/.ol -> .sclerox",
                dir.display()
            );
            did_anything = true;
            continue;
        }
        if crate::migrate::migrate_legacy_repo_dir(&dir) {
            println!(
                "  migrated folder config: {}/.ol -> .sclerox",
                dir.display()
            );
            did_anything = true;
        }
    }
    Ok(did_anything)
}

/// Rename each registered repo's `.ol/` index directory to `.sclerox/` and
/// repoint the registry at it.
///
/// `sclerox repo sync` will not do this: a repo whose `.ol/repo.db` still
/// exists reports healthy, so sync skips it. Left alone, these only migrate
/// when each repo next happens to be re-indexed, which can be months. The
/// index itself stays valid across the rename, so this is a directory rename
/// plus a path update, never a re-index.
fn migrate_registered_repos(dry_run: bool) -> Result<bool> {
    let db = crate::db::Database::open(&crate::config::settings().db_path).ok();
    let all_paths: Vec<String> = db
        .as_ref()
        .and_then(|d| d.repo_list().ok())
        .map(|rs| rs.into_iter().map(|r| r.path).collect())
        .unwrap_or_default();

    // Ancestors first: a catch-all parent's opt-out marker should be under its
    // current name before anything walks back through that folder.
    let mut did_anything = migrate_ancestor_dirs(&all_paths, dry_run)?;

    let legacy = legacy_repos();
    if legacy.is_empty() {
        return Ok(did_anything);
    }

    for repo in &legacy {
        let new_db = repo.new_db_path.to_string_lossy().into_owned();
        if dry_run {
            println!("  would migrate repo index: {}/.ol -> .sclerox", repo.path);
            did_anything = true;
            continue;
        }

        // Rename is a no-op when `.sclerox` already exists; the registry still
        // needs repointing in that case, so don't gate the update on it.
        crate::migrate::migrate_legacy_repo_dir(Path::new(&repo.path));

        if !repo.new_db_path.exists() {
            // Distinguish the two ways this happens: the repo is gone entirely
            // (a stale registry row `sclerox repo sync` prunes), versus present
            // but missing its index (which a re-index rebuilds).
            if Path::new(&repo.path).exists() {
                println!(
                    "  skipped repo index {}: no .sclerox/repo.db after rename \
                     (rebuild with `sclerox repo index {}`)",
                    repo.name, repo.path
                );
            } else {
                println!(
                    "  skipped repo index {}: directory no longer exists \
                     (stale registry entry; prune with `sclerox repo sync`)",
                    repo.name
                );
            }
            continue;
        }
        if let Some(db) = &db {
            match db.repo_set_db_path(&repo.path, &new_db) {
                Ok(true) => {
                    println!("  migrated repo index: {} -> .sclerox", repo.name);
                    did_anything = true;
                }
                Ok(false) => println!("  repo {} not in registry; left as-is", repo.name),
                Err(e) => println!("  repo {}: failed to update registry ({e})", repo.name),
            }
        }
    }

    Ok(did_anything)
}

// ─── Global path relocation (~/.ol/* → XDG) ─────────────────────────────────

/// Every global (src, dst) pair migration would move, in order.
///
/// Single source of truth for both the mover and [`legacy_data_present`], so
/// "is anything left to migrate?" can never drift from what actually moves.
/// That drift is exactly what made `sclerox install` keep advising a migrate
/// that `sclerox migrate` then reported as already done.
fn global_path_moves(legacy_home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let config_dst = crate::xdg::config_home()
        .join("sclerox")
        .join("config.toml");
    let db_dst = crate::xdg::data_home().join("sclerox").join("sclerox.db");
    let logs_dst = crate::xdg::state_home().join("sclerox").join("logs");
    let distilled_dst = crate::xdg::state_home().join("sclerox").join("distilled");

    let mut moves = vec![
        (legacy_home.join("config.toml"), config_dst),
        (legacy_home.join("ol.db"), db_dst.clone()),
    ];

    // Best-effort sidecars from an interrupted write. SQLite deletes -wal/-shm
    // on a clean close, so these are usually already gone by migrate time.
    for suffix in ["-journal", "-wal", "-shm"] {
        moves.push((
            legacy_home.join(format!("ol.db{suffix}")),
            db_dst.with_extension(format!("db{suffix}")),
        ));
    }

    // `ol-YYYY-MM-DD.log` -> `sclerox-YYYY-MM-DD.log` (the prefix changed too).
    if let Ok(entries) = std::fs::read_dir(legacy_home.join("logs")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix("ol-") else {
                continue;
            };
            moves.push((
                legacy_home.join("logs").join(name),
                logs_dst.join(format!("sclerox-{rest}")),
            ));
        }
    }

    // `distilled/` keeps its session-id-keyed filenames.
    if let Ok(entries) = std::fs::read_dir(legacy_home.join("distilled")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            moves.push((
                legacy_home.join("distilled").join(&name),
                distilled_dst.join(&name),
            ));
        }
    }

    moves.push((
        legacy_home.join("completions").join("ol.ps1"),
        crate::xdg::data_home()
            .join("sclerox")
            .join("completions")
            .join("sclerox.ps1"),
    ));

    moves.retain(|(src, _)| src.exists());
    moves
}

fn migrate_global_paths(legacy_home: &Path, dry_run: bool) -> Result<bool> {
    let mut moved_any = false;
    for (src, dst) in global_path_moves(legacy_home) {
        moved_any |= move_file(&src, &dst, dry_run)?;
    }
    if !dry_run {
        remove_if_empty(legacy_home);
    }
    Ok(moved_any)
}

/// Move a single file, skipping (and warning) if the destination already
/// exists — never clobber something that's already at the new location.
fn move_file(src: &Path, dst: &Path, dry_run: bool) -> Result<bool> {
    if !src.exists() {
        return Ok(false);
    }
    if dst.exists() {
        println!(
            "  kept existing {} — left old {} in place (remove it manually once you've confirmed the new one is correct)",
            dst.display(),
            src.display()
        );
        return Ok(false);
    }
    if dry_run {
        println!("  would move {} -> {}", src.display(), dst.display());
        return Ok(true);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::rename(src, dst).is_err() {
        // Cross-device (e.g. XDG_DATA_HOME pointed at another drive): copy + remove.
        std::fs::copy(src, dst)
            .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        std::fs::remove_file(src).with_context(|| format!("removing {}", src.display()))?;
    }
    println!("  moved {} -> {}", src.display(), dst.display());
    Ok(true)
}

/// Best-effort cleanup of the old `~/.ol` tree once everything's been moved
/// out of it. `remove_dir` only succeeds on an empty directory, so this is a
/// silent no-op for anything migration didn't fully claim.
fn remove_if_empty(legacy_home: &Path) {
    for sub in ["logs", "distilled", "completions"] {
        let _ = std::fs::remove_dir(legacy_home.join(sub));
    }
    let _ = std::fs::remove_dir(legacy_home);
}

// ─── Stale tool-integration cleanup (old markers, not just old paths) ──────

const LEGACY_HOOK_MARKER: &str = "# ol-kb-hook";
const LEGACY_SECTION_MARKER: &str = "<!-- ol-kb -->";
const LEGACY_SECTION_END_MARKER: &str = "<!-- /ol-kb -->";
const LEGACY_SKILL_DIR_NAME: &str = "ol-kb";
const LEGACY_OPENCODE_PLUGIN_FILE: &str = "ol-session.js";

/// Every marker/filename that gated "is this already installed" changed name
/// in the rename, so a plain `sclerox install` on a machine with an old `ol`
/// install doesn't recognize its artifacts and writes new ones alongside them
/// (duplicate hooks, an orphaned doc section, two competing skill dirs). This
/// strips the old ones the same way `install.rs`'s uninstall does, just
/// matching the old markers instead of the new ones.
fn strip_legacy_integrations(dry_run: bool) -> Result<bool> {
    let mut did_anything = false;

    if let Ok(dir) = claude_dir() {
        did_anything |= strip_legacy_skill(&dir.join("skills"), dry_run)?;
        did_anything |= strip_legacy_hook(&dir, dry_run)?;
        did_anything |= strip_legacy_section(&dir.join("CLAUDE.md"), dry_run)?;
    }
    if let Ok(dir) = opencode_dir() {
        did_anything |= strip_legacy_skill(&dir.join("skills"), dry_run)?;
        did_anything |= strip_legacy_file(
            &dir.join("plugins").join(LEGACY_OPENCODE_PLUGIN_FILE),
            dry_run,
        )?;
        did_anything |= strip_legacy_section(&dir.join("AGENTS.md"), dry_run)?;
    }
    if let Ok(dir) = codex_dir() {
        did_anything |= strip_legacy_section(&dir.join("instructions.md"), dry_run)?;
    }

    Ok(did_anything)
}

fn strip_legacy_skill(skills_dir: &Path, dry_run: bool) -> Result<bool> {
    let legacy = skills_dir.join(LEGACY_SKILL_DIR_NAME);
    if !legacy.exists() {
        return Ok(false);
    }
    if dry_run {
        println!("  would remove legacy skill: {}/", legacy.display());
    } else {
        std::fs::remove_dir_all(&legacy)
            .with_context(|| format!("failed to remove {}", legacy.display()))?;
        println!("  removed legacy skill: {}/", legacy.display());
    }
    Ok(true)
}

fn strip_legacy_file(path: &Path, dry_run: bool) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if dry_run {
        println!("  would remove legacy file: {}", path.display());
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
        println!("  removed legacy file: {}", path.display());
    }
    Ok(true)
}

fn strip_legacy_hook(claude_dir: &Path, dry_run: bool) -> Result<bool> {
    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        return Ok(false);
    }
    let mut settings = read_json(&settings_path)?;
    let mut modified = false;

    for event in ["SessionStart", "Stop"] {
        let key = format!("/hooks/{event}");
        if let Some(arr) = settings.pointer_mut(&key).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|entry| {
                !entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains(LEGACY_HOOK_MARKER))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            });
            if arr.len() < before {
                modified = true;
            }
        }
    }

    if !modified {
        return Ok(false);
    }
    if dry_run {
        println!(
            "  would remove legacy SessionStart + Stop hooks from: {}",
            settings_path.display()
        );
    } else {
        write_json(&settings_path, &settings)?;
        println!("  removed legacy hooks: {}", settings_path.display());
    }
    Ok(true)
}

/// Remove the `<!-- ol-kb -->` … `<!-- /ol-kb -->` section (or, for the
/// pre-end-marker legacy format, everything from the start marker to EOF),
/// preserving any content before/after it. Mirrors `install.rs`'s
/// `uninstall_section`, parametrized on the old markers.
fn strip_legacy_section(path: &Path, dry_run: bool) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)?;
    let Some(start) = content.find(LEGACY_SECTION_MARKER) else {
        return Ok(false);
    };
    if dry_run {
        println!(
            "  would remove legacy knowledge-base section from {}",
            path.display()
        );
        return Ok(true);
    }
    let before = content[..start].trim_end();
    let rest = &content[start..];
    let after = match rest.find(LEGACY_SECTION_END_MARKER) {
        Some(rel) => content[start + rel + LEGACY_SECTION_END_MARKER.len()..].trim(),
        None => "",
    };
    let rebuilt = match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{after}\n"),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}\n"),
    };
    std::fs::write(path, rebuilt)?;
    println!(
        "  removed legacy knowledge-base section from {}",
        path.display()
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_file_skips_when_destination_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, "old").unwrap();
        std::fs::write(&dst, "new").unwrap();

        assert!(!move_file(&src, &dst, false).unwrap());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
        assert!(src.exists(), "source left in place when dest exists");
    }

    #[test]
    fn move_file_moves_when_destination_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("nested").join("dst.txt");
        std::fs::write(&src, "content").unwrap();

        assert!(move_file(&src, &dst, false).unwrap());
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "content");
    }

    #[test]
    fn move_file_dry_run_changes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, "content").unwrap();

        assert!(move_file(&src, &dst, true).unwrap());
        assert!(src.exists(), "dry-run must not move anything");
        assert!(!dst.exists());
    }

    /// Build a `~/.ol` containing exactly `files` (relative paths).
    fn legacy_home_with(dir: &Path, files: &[&str]) -> PathBuf {
        let home = dir.join(".ol");
        for rel in files {
            let path = home.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();
        }
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn dst_names(moves: &[(PathBuf, PathBuf)]) -> Vec<String> {
        moves
            .iter()
            .map(|(_, d)| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn legacy_ancestor_dirs_finds_optout_on_unregistered_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("work");
        let repo = parent.join("repo");
        std::fs::create_dir_all(repo.join(".sclerox")).unwrap();
        std::fs::create_dir_all(parent.join(".ol")).unwrap();
        std::fs::write(parent.join(".ol").join("config.toml"), "index = false").unwrap();

        let found = legacy_ancestor_dirs(&[repo.to_string_lossy().into_owned()]);
        assert_eq!(found, vec![parent]);
    }

    #[test]
    fn legacy_ancestor_dirs_skips_already_migrated_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("work");
        let repo = parent.join("repo");
        std::fs::create_dir_all(repo).unwrap();
        std::fs::create_dir_all(parent.join(".ol")).unwrap();
        std::fs::create_dir_all(parent.join(".sclerox")).unwrap();

        let found = legacy_ancestor_dirs(&[parent.join("repo").to_string_lossy().into_owned()]);
        assert!(found.is_empty(), "a parent with .sclerox is already done");
    }

    #[test]
    fn legacy_ancestor_dirs_dedupes_shared_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("work");
        std::fs::create_dir_all(parent.join("a")).unwrap();
        std::fs::create_dir_all(parent.join("b")).unwrap();
        std::fs::create_dir_all(parent.join(".ol")).unwrap();

        let found = legacy_ancestor_dirs(&[
            parent.join("a").to_string_lossy().into_owned(),
            parent.join("b").to_string_lossy().into_owned(),
        ]);
        assert_eq!(found.len(), 1, "shared parent reported once, got {found:?}");
    }

    #[test]
    fn points_at_legacy_index_matches_whole_component_only() {
        assert!(points_at_legacy_index("/home/me/code/repo/.ol/repo.db"));
        assert!(!points_at_legacy_index(
            "/home/me/code/repo/.sclerox/repo.db"
        ));
        // Directories that merely contain ".ol" in their name are not indexes.
        assert!(!points_at_legacy_index("/home/me/code/tools.old/repo.db"));
        assert!(!points_at_legacy_index("/home/me/.olive/repo.db"));
    }

    #[test]
    fn global_path_moves_renames_log_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = legacy_home_with(dir.path(), &["logs/ol-2026-01-01.log"]);
        let names = dst_names(&global_path_moves(&home));
        assert!(
            names.contains(&"sclerox-2026-01-01.log".to_string()),
            "got {names:?}"
        );
    }

    #[test]
    fn global_path_moves_covers_db_config_and_distilled() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = legacy_home_with(
            dir.path(),
            &["config.toml", "ol.db", "distilled/session-abc"],
        );
        let names = dst_names(&global_path_moves(&home));
        assert!(names.contains(&"sclerox.db".to_string()), "got {names:?}");
        assert!(names.contains(&"config.toml".to_string()), "got {names:?}");
        assert!(names.contains(&"session-abc".to_string()), "got {names:?}");
    }

    #[test]
    fn global_path_moves_ignores_files_migration_never_claims() {
        // The regression: a `~/.ol` holding only an old hand-made backup has
        // nothing left to migrate, but used to keep `sclerox install` advising
        // a migrate that `sclerox migrate` reported as already done.
        let dir = tempfile::TempDir::new().unwrap();
        let home = legacy_home_with(dir.path(), &["ol.db.pre-v12-backup"]);
        assert!(
            global_path_moves(&home).is_empty(),
            "unclaimed leftovers must not count as pending migration"
        );
    }

    #[test]
    fn global_path_moves_ignores_non_ol_log_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = legacy_home_with(dir.path(), &["logs/unrelated.log"]);
        assert!(global_path_moves(&home).is_empty());
    }

    #[test]
    fn find_legacy_binary_locates_ol_on_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join(legacy_binary_name());
        let path_var = std::env::join_paths([dir.path()]).unwrap();

        assert_eq!(
            find_legacy_binary(&path_var, |p| p == bin),
            Some(bin.clone())
        );
        assert_eq!(find_legacy_binary(&path_var, |_| false), None);
    }

    #[test]
    fn strip_legacy_section_preserves_surrounding_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(
            &path,
            format!(
                "# Mine\n\n{LEGACY_SECTION_MARKER}\nOLD\n{LEGACY_SECTION_END_MARKER}\n\n# After\n"
            ),
        )
        .unwrap();

        assert!(strip_legacy_section(&path, false).unwrap());
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("OLD"));
        assert!(out.contains("# Mine"));
        assert!(out.contains("# After"));
    }

    #[test]
    fn strip_legacy_section_noop_without_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(&path, "# Nothing to see here\n").unwrap();
        assert!(!strip_legacy_section(&path, false).unwrap());
    }

    #[test]
    fn strip_legacy_hook_removes_only_marked_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings_path = dir.path().join("settings.json");
        let cmd = format!("{LEGACY_HOOK_MARKER}\nol hook stop 2>/dev/null || true");
        std::fs::write(
            &settings_path,
            serde_json::json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": cmd }] },
                        { "hooks": [{ "type": "command", "command": "some-other-tool" }] }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(strip_legacy_hook(dir.path(), false).unwrap());
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "only the legacy-marked entry removed");
    }
}
