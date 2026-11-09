//! When each indexed file last changed, for `files.last_modified`: its last
//! commit, or its time on disk when it is uncommitted or not in git.
//!
//! Dates are looked up only for files whose date can have moved: ones this
//! run rewrote, ones with no date yet, and ones that differ between the HEAD
//! dated last time and now. The history walk stops once those are found.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::repo_db::RepoDb;
use crate::git::git_command;

/// The HEAD that `files.last_modified` was last brought up to date with.
const DATES_HEAD_KEY: &str = "file_dates_head";

/// Bring `files.last_modified` up to date; `rewritten` are the files this run
/// indexed anew. A row's date is left alone if neither git nor the file can
/// supply one.
pub fn refresh(repo_db: &RepoDb, root: &Path, rewritten: HashSet<String>) -> Result<()> {
    let head = current_head(root);
    let files = repo_db.list_files()?;
    let mut needed = rewritten;
    needed.extend(
        files
            .iter()
            .filter(|f| f.last_modified.is_none())
            .map(|f| f.path.clone()),
    );
    if let Some(head) = &head {
        let last = repo_db.get_meta(DATES_HEAD_KEY)?;
        if last.as_ref() != Some(head) {
            let moved = last.and_then(|last| {
                paths(
                    root,
                    &["diff", "--name-only", "--no-renames", "-z", "--relative"],
                    &[&last, head],
                )
            });
            let moved: Option<HashSet<String>> = moved.map(|m| m.into_iter().collect());
            needed.extend(
                files
                    .iter()
                    .filter(|f| moved.as_ref().is_none_or(|m| m.contains(&f.path)))
                    .map(|f| f.path.clone()),
            );
        }
    }

    let mut dates = HashMap::new();
    if let Some(head) = &head {
        let mut wanted = needed.clone();
        for path in uncommitted(root) {
            wanted.remove(&path);
        }
        commit_dates(root, head, wanted, &mut dates);
    }
    for path in &needed {
        if !dates.contains_key(path) {
            if let Some(at) = disk_date(&root.join(path)) {
                dates.insert(path.clone(), at);
            }
        }
    }
    repo_db.set_file_dates(&dates)?;
    if let Some(head) = &head {
        repo_db.set_meta(DATES_HEAD_KEY, head)?;
    }
    Ok(())
}

fn current_head(root: &Path) -> Option<String> {
    let out = git_command(root)
        .args(["rev-parse", "-q", "--verify", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The NUL-separated paths a git command prints, or `None` if it fails.
fn paths(root: &Path, args: &[&str], revs: &[&str]) -> Option<Vec<String>> {
    let out = git_command(root).args(args).args(revs).output().ok()?;
    out.status.success().then(|| {
        out.stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    })
}

/// Files whose content isn't their last commit's: changed since HEAD, or
/// untracked. Their date is their time on disk.
fn uncommitted(root: &Path) -> Vec<String> {
    let mut found = paths(
        root,
        &["diff", "--name-only", "--no-renames", "-z", "--relative"],
        &["HEAD"],
    )
    .unwrap_or_default();
    found.extend(
        paths(
            root,
            &["ls-files", "-z", "--others", "--exclude-standard"],
            &[],
        )
        .unwrap_or_default(),
    );
    found
}

/// Walk history from `head`, newest first, recording the first commit to
/// touch each of `wanted`, and stop as soon as all are found.
fn commit_dates(
    root: &Path,
    head: &str,
    mut wanted: HashSet<String>,
    dates: &mut HashMap<String, String>,
) {
    if wanted.is_empty() {
        return;
    }
    let spawned = git_command(root)
        .args([
            "log",
            "-z",
            "--relative",
            "--no-renames",
            "--no-show-signature",
            "--name-only",
            "--format=%x01%ct",
            head,
        ])
        .stdout(Stdio::piped())
        .spawn();
    let Ok(mut child) = spawned else { return };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let mut at = None;
    for token in BufReader::new(stdout).split(0) {
        let Ok(token) = token else { break };
        let token = String::from_utf8_lossy(&token);
        // Each commit prints `\x01<time>` NUL, then a newline before its
        // first path.
        let token = token.trim_start_matches('\n');
        if let Some(time) = token.strip_prefix('\u{1}') {
            at = time
                .parse()
                .ok()
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .map(stored_format);
        } else if wanted.remove(token) {
            if let Some(at) = &at {
                dates.insert(token.to_string(), at.clone());
            }
            if wanted.is_empty() {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn disk_date(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(stored_format(modified.into()))
}

/// The form timestamps are stored in everywhere: UTC, as `datetime('now')`.
fn stored_format(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const FIRST: &str = "2020-01-02 03:04:05";
    const SECOND: &str = "2021-06-07 08:09:10";

    /// Run git in `dir` with only this test's config, commits dated `at`.
    fn git(dir: &Path, at: &str, args: &[&str]) {
        let date = format!("{}+0000", at.replace(' ', "T"));
        let status = git_command(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join("no-such-gitconfig"))
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn write(root: &Path, path: &str, text: &str) {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn commit(root: &Path, at: &str) {
        git(root, at, &["add", "-A"]);
        git(root, at, &["commit", "-q", "-m", "c"]);
    }

    fn repo() -> (tempfile::TempDir, PathBuf, RepoDb) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir(&root).unwrap();
        git(&root, FIRST, &["init", "-q"]);
        (dir, root, RepoDb::open_in_memory().unwrap())
    }

    fn track(db: &RepoDb, paths: &[&str]) {
        for path in paths {
            db.upsert_file(path, None, "h").unwrap();
        }
    }

    fn date(db: &RepoDb, path: &str) -> Option<String> {
        db.list_files()
            .unwrap()
            .into_iter()
            .find(|f| f.path == path)
            .unwrap()
            .last_modified
    }

    fn all(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn today() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    #[test]
    fn a_file_is_dated_by_the_last_commit_to_touch_it() {
        let (_dir, root, db) = repo();
        write(&root, "a.rs", "1");
        write(&root, "src/b.rs", "1");
        commit(&root, FIRST);
        write(&root, "src/b.rs", "2");
        commit(&root, SECOND);
        track(&db, &["a.rs", "src/b.rs"]);
        refresh(&db, &root, all(&["a.rs", "src/b.rs"])).unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some(FIRST));
        assert_eq!(date(&db, "src/b.rs").as_deref(), Some(SECOND));
    }

    #[test]
    fn uncommitted_and_untracked_files_use_their_time_on_disk() {
        let (_dir, root, db) = repo();
        write(&root, "edited.rs", "1");
        write(&root, "staged.rs", "1");
        commit(&root, FIRST);
        write(&root, "edited.rs", "2");
        write(&root, "staged.rs", "2");
        git(&root, FIRST, &["add", "staged.rs"]);
        write(&root, "new.rs", "1");
        let names = ["edited.rs", "staged.rs", "new.rs"];
        track(&db, &names);
        refresh(&db, &root, all(&names)).unwrap();
        for name in names {
            let at = date(&db, name).unwrap();
            assert!(at.starts_with(&today()), "{name}: {at}");
        }
    }

    #[test]
    fn committing_an_unchanged_file_moves_it_to_the_commit_date() {
        let (_dir, root, db) = repo();
        write(&root, "keep.rs", "1");
        commit(&root, FIRST);
        write(&root, "a.rs", "1");
        track(&db, &["keep.rs", "a.rs"]);
        refresh(&db, &root, all(&["keep.rs", "a.rs"])).unwrap();
        assert!(date(&db, "a.rs").unwrap().starts_with(&today()));

        // The index sees the same content, so this run rewrites nothing.
        commit(&root, SECOND);
        refresh(&db, &root, HashSet::new()).unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some(SECOND));
        assert_eq!(date(&db, "keep.rs").as_deref(), Some(FIRST));
    }

    #[test]
    fn moving_head_back_restores_the_older_date() {
        let (_dir, root, db) = repo();
        write(&root, "a.rs", "1");
        commit(&root, FIRST);
        write(&root, "a.rs", "2");
        commit(&root, SECOND);
        track(&db, &["a.rs"]);
        refresh(&db, &root, all(&["a.rs"])).unwrap();
        git(&root, SECOND, &["reset", "-q", "--hard", "HEAD~1"]);
        refresh(&db, &root, HashSet::new()).unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some(FIRST));
    }

    #[test]
    fn an_up_to_date_file_keeps_its_date() {
        let (_dir, root, db) = repo();
        write(&root, "a.rs", "1");
        commit(&root, FIRST);
        track(&db, &["a.rs"]);
        refresh(&db, &root, all(&["a.rs"])).unwrap();
        db.set_file_dates(&HashMap::from([("a.rs".to_string(), "kept".to_string())]))
            .unwrap();
        refresh(&db, &root, HashSet::new()).unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some("kept"));
    }

    #[test]
    fn a_subfolder_of_a_repo_matches_its_own_relative_paths() {
        let (_dir, root, db) = repo();
        write(&root, "sub/a.rs", "1");
        write(&root, "sub/edited.rs", "1");
        commit(&root, FIRST);
        write(&root, "sub/edited.rs", "2");
        track(&db, &["a.rs", "edited.rs"]);
        refresh(&db, &root.join("sub"), all(&["a.rs", "edited.rs"])).unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some(FIRST));
        assert!(date(&db, "edited.rs").unwrap().starts_with(&today()));
    }

    #[test]
    fn outside_git_a_file_uses_its_time_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "1");
        let db = RepoDb::open_in_memory().unwrap();
        track(&db, &["a.rs"]);
        refresh(&db, dir.path(), all(&["a.rs"])).unwrap();
        assert!(date(&db, "a.rs").unwrap().starts_with(&today()));
    }

    #[test]
    fn re_upserting_a_file_keeps_its_date() {
        let db = RepoDb::open_in_memory().unwrap();
        track(&db, &["a.rs"]);
        db.set_file_dates(&HashMap::from([("a.rs".to_string(), FIRST.to_string())]))
            .unwrap();
        db.upsert_file("a.rs", None, "h2").unwrap();
        assert_eq!(date(&db, "a.rs").as_deref(), Some(FIRST));
    }
}
