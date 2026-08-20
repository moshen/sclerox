//! `sclerox migrate` — one-time cleanup for a machine that has an old, pre-rename
//! `ol` install: moves `~/.ol/*` onto the new XDG layout and strips
//! old-marker tool integrations (hooks, skill dir, OpenCode plugin, doc
//! sections) that `sclerox install` doesn't recognize as its own. See
//! `MIGRATION.md` for the full picture.
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

    if !did_anything {
        println!("Nothing to migrate — already on the sclerox / XDG layout.");
    } else if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    } else {
        println!("\nMigration complete.");
    }
    Ok(())
}

/// `~/.ol`, the pre-rename flat layout every real install actually wrote
/// (nothing shipped under the `sclerox` name before the XDG move existed).
fn legacy_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ol"))
}

/// True if there's an old `ol` install left to migrate. Used by `sclerox install`
/// to print a one-line pointer at `sclerox migrate` when it finds one.
pub fn legacy_data_present() -> bool {
    legacy_home_dir().is_some_and(|p| p.exists())
}

// ─── Global path relocation (~/.ol/* → XDG) ─────────────────────────────────

fn migrate_global_paths(legacy_home: &Path, dry_run: bool) -> Result<bool> {
    let mut moved_any = false;

    moved_any |= move_file(
        &legacy_home.join("config.toml"),
        &crate::xdg::config_home().join("sclerox").join("config.toml"),
        dry_run,
    )?;

    let db_dst = crate::xdg::data_home().join("sclerox").join("sclerox.db");
    moved_any |= move_file(&legacy_home.join("ol.db"), &db_dst, dry_run)?;
    // Best-effort sidecar files left by an interrupted write (default rollback
    // journal mode only leaves these transiently, but a crash mid-write can
    // strand one).
    for suffix in ["-journal", "-wal", "-shm"] {
        let dst = db_dst.with_extension(format!("db{suffix}"));
        moved_any |= move_file(&legacy_home.join(format!("ol.db{suffix}")), &dst, dry_run)?;
    }

    moved_any |= move_renamed_logs(
        legacy_home,
        &crate::xdg::state_home().join("sclerox").join("logs"),
        dry_run,
    )?;

    moved_any |= move_dir_contents(
        &legacy_home.join("distilled"),
        &crate::xdg::state_home().join("sclerox").join("distilled"),
        dry_run,
    )?;

    moved_any |= move_file(
        &legacy_home.join("completions").join("ol.ps1"),
        &crate::xdg::data_home()
            .join("sclerox")
            .join("completions")
            .join("sclerox.ps1"),
        dry_run,
    )?;

    if !dry_run {
        remove_if_empty(legacy_home);
    }

    Ok(moved_any)
}

/// `~/.ol/logs/ol-YYYY-MM-DD.log` → `<state_home>/sclerox/logs/sclerox-YYYY-MM-DD.log`,
/// one file at a time (the `ol-` → `sclerox-` filename prefix changed too).
fn move_renamed_logs(legacy_home: &Path, dst_dir: &Path, dry_run: bool) -> Result<bool> {
    let src_dir = legacy_home.join("logs");
    let Ok(entries) = std::fs::read_dir(&src_dir) else {
        return Ok(false);
    };
    let mut moved_any = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("ol-") else {
            continue;
        };
        moved_any |= move_file(
            &src_dir.join(name),
            &dst_dir.join(format!("sclerox-{rest}")),
            dry_run,
        )?;
    }
    Ok(moved_any)
}

/// Move every entry of `src_dir` into `dst_dir`, same filenames (used for
/// `distilled/`, whose session-id-keyed marker/lock filenames didn't change).
fn move_dir_contents(src_dir: &Path, dst_dir: &Path, dry_run: bool) -> Result<bool> {
    let Ok(entries) = std::fs::read_dir(src_dir) else {
        return Ok(false);
    };
    let mut moved_any = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        moved_any |= move_file(&src_dir.join(&name), &dst_dir.join(&name), dry_run)?;
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

    #[test]
    fn move_renamed_logs_renames_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_home = dir.path().join(".ol");
        let logs = legacy_home.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("ol-2026-01-01.log"), "log").unwrap();

        let dst_dir = dir.path().join("logs-dst");
        assert!(move_renamed_logs(&legacy_home, &dst_dir, false).unwrap());
        assert!(dst_dir.join("sclerox-2026-01-01.log").exists());
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
