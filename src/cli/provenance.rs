//! What a memory write knows about where it came from: the git repo it ran
//! in (see [`RepoInfo`]) and the calling session. Recording is best-effort:
//! a memory is stored even when its provenance can't be.

use crate::git::git_command;
use std::path::{Path, PathBuf};

use crate::db::memory::MemoryEntry;
use crate::db::provenance::{Provenance, ProvenanceRow, RepoScope};
use crate::db::Database;

/// The git repo a command ran in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub name: String,
    /// The repo's first commit (the oldest, if its history has several).
    /// `None` in a shallow clone, whose first commit is only where the clone
    /// was cut.
    pub root_commit: Option<String>,
    /// `origin`'s URL (else the first remote's), normalized: see
    /// [`normalize_remote`].
    pub remote: Option<String>,
    pub commit: Option<String>,
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
}

impl RepoInfo {
    /// The repo containing `dir`, if any. Git failing (not installed, no
    /// commits yet) leaves the fields it would have filled empty.
    pub fn at(db: &Database, dir: &Path) -> Option<Self> {
        let root = crate::index::find_git_root(dir);
        if !root.join(".git").exists() {
            return None;
        }
        let name = root.file_name()?.to_string_lossy().into_owned();
        let mut info = RepoInfo {
            name,
            ..Default::default()
        };
        let git_dir = GitDir::of(&root);
        let head = git_dir.as_ref().and_then(GitDir::head);
        let shallow = match head {
            Some((commit, branch)) => {
                info.commit = commit;
                info.branch = branch;
                git_dir
                    .as_ref()
                    .is_some_and(|g| g.common.join("shallow").exists())
            }
            // A layout the files don't explain (reftable, say): ask git.
            None => {
                let out = git(
                    &root,
                    &[
                        "rev-parse",
                        "--is-shallow-repository",
                        "HEAD",
                        "--abbrev-ref",
                        "HEAD",
                    ],
                );
                let mut lines = out.as_deref().unwrap_or_default().lines();
                let shallow = lines.next() == Some("true");
                info.commit = lines.next().map(str::to_string);
                info.branch = lines.next().filter(|b| *b != "HEAD").map(str::to_string);
                shallow
            }
        };
        if info.commit.is_some() && !shallow {
            let stamp = git_dir.as_ref().and_then(GitDir::created);
            info.root_commit = root_commit(db, &root, stamp.as_deref());
        }
        info.remote = git(&root, &["config", "--get-regexp", r"^remote\..*\.url$"])
            .and_then(|out| pick_remote(&out))
            .and_then(|url| normalize_remote(&url));
        info.root = root;
        Some(info)
    }

    /// The repo containing the working directory, if any.
    pub fn current(db: &Database) -> Option<Self> {
        Self::at(db, &std::env::current_dir().ok()?)
    }

    pub fn scope(&self) -> RepoScope {
        RepoScope {
            repo_name: self.name.clone(),
            root_commit: self.root_commit.clone(),
            remote: self.remote.clone(),
        }
    }

    /// Fill in `p`'s repo fields.
    pub fn describe(&self, p: &mut Provenance) {
        p.repo_name = Some(self.name.clone());
        p.repo_path = Some(self.root.to_string_lossy().into_owned());
        p.repo_root = self.root_commit.clone();
        p.repo_remote = self.remote.clone();
        p.commit_sha = self.commit.clone();
        p.branch = self.branch.clone();
    }
}

/// A source of `kind` for a command run by hand or by an agent: the calling
/// session and agent (see `cli::usage`) and the repo it ran in.
pub fn from_command(kind: &str, repo: Option<&RepoInfo>) -> Provenance {
    let ctx = crate::cli::usage::context();
    let mut p = Provenance {
        kind: kind.to_string(),
        session_id: ctx.session_id,
        agent: ctx.agent,
        ..Default::default()
    };
    if let Some(repo) = repo {
        repo.describe(&mut p);
    }
    p
}

/// Record `p` as a source of memory `key`'s active row, logging a failure.
pub fn record(db: &Database, key: &str, p: &Provenance) {
    let id = match db.memory_get(key) {
        Ok(Some(e)) if e.status == "active" => e.id,
        Ok(_) => return,
        Err(e) => {
            log::warn!("couldn't record where memory '{key}' came from: {e:#}");
            return;
        }
    };
    if let Err(e) = db.memory_provenance_add(id, p) {
        log::warn!("couldn't record where memory '{key}' came from: {e:#}");
    }
}

/// A memory with where it came from and what it belongs to, as `memory get`
/// shows it.
#[derive(Debug, serde::Serialize)]
pub struct MemoryDetail {
    #[serde(flatten)]
    pub entry: MemoryEntry,
    /// `hand-written`, `distilled` or `imported`: see [`MemoryEntry::origin`].
    pub origin: String,
    /// Every recorded source, oldest first; empty when none was recorded.
    pub provenance: Vec<ProvenanceRow>,
    /// Repos it is scoped to; empty for a global memory.
    pub repos: Vec<RepoScope>,
    pub projects: Vec<ProjectRef>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectRef {
    pub id: i64,
    pub name: String,
}

impl MemoryDetail {
    pub fn load(db: &Database, entry: MemoryEntry) -> anyhow::Result<Self> {
        Ok(MemoryDetail {
            origin: entry.origin().to_string(),
            provenance: db.memory_provenance(entry.id)?,
            repos: db.memory_scopes(entry.id)?,
            projects: db
                .memory_projects(entry.id)?
                .into_iter()
                .map(|(id, name)| ProjectRef { id, name })
                .collect(),
            entry,
        })
    }

    /// The text lines after a memory's own fields.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("Origin:  {}", self.origin)];
        let scope = if self.repos.is_empty() {
            "global".to_string()
        } else {
            let names: Vec<_> = self.repos.iter().map(|r| r.repo_name.as_str()).collect();
            format!("repo {}", names.join(", "))
        };
        out.push(format!("Scope:   {scope}"));
        if !self.projects.is_empty() {
            let names: Vec<_> = self
                .projects
                .iter()
                .map(|p| format!("#{} {}", p.id, p.name))
                .collect();
            out.push(format!("Projects: {}", names.join(", ")));
        }
        if self.provenance.is_empty() {
            out.push("Sources: unknown (written before sources were recorded)".to_string());
        } else {
            out.push("Sources:".to_string());
            for row in &self.provenance {
                out.push(format!(
                    "  {}  {}",
                    row.created_at,
                    describe(&row.provenance)
                ));
            }
        }
        out
    }
}

/// One source on one line: what wrote it, from where.
pub fn describe(p: &Provenance) -> String {
    let mut parts = vec![p.kind.clone()];
    if let Some(session) = &p.session_id {
        let agent = p
            .agent
            .as_deref()
            .map(|a| format!(" ({a})"))
            .unwrap_or_default();
        parts.push(format!("session {session}{agent}"));
    } else if let Some(agent) = &p.agent {
        parts.push(format!("by {agent}"));
    }
    if let Some(repo) = &p.repo_name {
        let mut at = format!("in {repo}");
        if let Some(sha) = &p.commit_sha {
            at.push_str(&format!(" @ {}", sha.get(..10).unwrap_or(sha)));
        }
        if let Some(branch) = &p.branch {
            at.push_str(&format!(" on {branch}"));
        }
        parts.push(at);
    }
    if let Some(path) = &p.source_path {
        parts.push(format!("from {path}"));
    }
    if let Some(path) = &p.transcript_path {
        let lines = match (p.line_start, p.line_end) {
            (Some(a), Some(b)) if a == b => format!(" line {a}"),
            (Some(a), Some(b)) => format!(" lines {a}-{b}"),
            _ => String::new(),
        };
        parts.push(format!("transcript {path}{lines}"));
    }
    parts.join(", ")
}

/// Run git in `dir`; its trimmed stdout when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(dir).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run git in `dir`; whether it succeeded.
fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    git_command(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The first commit of the clone at `root`, cached per clone: finding it
/// walks the whole history. A cached commit is used while the clone's git
/// directory has the creation time `stamp` it was cached under, so another
/// repo cloned to the same path isn't taken for the old one. Without a stamp
/// (a filesystem that doesn't record creation), git checks the commit is in
/// the clone instead.
fn root_commit(db: &Database, root: &Path, stamp: Option<&str>) -> Option<String> {
    let path = root.to_string_lossy();
    if let Ok(Some((cached, cached_stamp))) = db.repo_root_cached(&path) {
        let same_clone = match (stamp, cached_stamp.as_deref()) {
            (Some(now), Some(then)) => now == then,
            _ => git_succeeds(root, &["cat-file", "-e", &format!("{cached}^{{commit}}")]),
        };
        if same_clone {
            return Some(cached);
        }
    }
    let out = git(root, &["log", "--max-parents=0", "--format=%ct %H", "HEAD"])?;
    let first = oldest_root(&out)?;
    if let Err(e) = db.repo_root_store(&path, stamp, &first) {
        log::debug!("couldn't cache the first commit of {path}: {e:#}");
    }
    Some(first)
}

/// Where a clone keeps its git data. In a linked worktree `.git` is a file
/// naming its own directory, which shares refs with the main one.
struct GitDir {
    /// Holds HEAD.
    dir: PathBuf,
    /// Holds refs, objects and `shallow`.
    common: PathBuf,
}

impl GitDir {
    fn of(root: &Path) -> Option<Self> {
        let dot = root.join(".git");
        let dir = if dot.is_dir() {
            dot
        } else {
            let text = std::fs::read_to_string(&dot).ok()?;
            root.join(text.trim().strip_prefix("gitdir:")?.trim())
        };
        let common = match std::fs::read_to_string(dir.join("commondir")) {
            Ok(rel) => dir.join(rel.trim()),
            Err(_) => dir.clone(),
        };
        Some(GitDir { dir, common })
    }

    /// HEAD's commit and branch, read from the files: `None` when they don't
    /// say (the refs live in a reftable), so the caller asks git. A branch
    /// with no commits yet has no commit; a detached HEAD has no branch.
    fn head(&self) -> Option<(Option<String>, Option<String>)> {
        if self.common.join("reftable").exists() {
            return None;
        }
        let head = std::fs::read_to_string(self.dir.join("HEAD")).ok()?;
        let head = head.trim();
        let Some(name) = head.strip_prefix("ref:").map(str::trim) else {
            return is_hash(head).then(|| (Some(head.to_string()), None));
        };
        let branch = name.strip_prefix("refs/heads/").map(str::to_string);
        let loose = std::fs::read_to_string(self.common.join(name))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| is_hash(s));
        let commit = loose.or_else(|| {
            let packed = std::fs::read_to_string(self.common.join("packed-refs")).ok()?;
            packed.lines().find_map(|l| {
                let (sha, r) = l.split_once(' ')?;
                (r.trim() == name && is_hash(sha)).then(|| sha.to_string())
            })
        });
        Some((commit, branch))
    }

    /// When the clone's git directory was created, where the filesystem says.
    fn created(&self) -> Option<String> {
        let t = std::fs::metadata(&self.common).ok()?.created().ok()?;
        let since = t.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(format!("{}.{:09}", since.as_secs(), since.subsec_nanos()))
    }
}

/// A full commit hash (SHA-1 or SHA-256).
fn is_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The oldest of `git log --format='%ct %H'`'s commits, ties broken by hash so
/// every clone picks the same one.
fn oldest_root(log: &str) -> Option<String> {
    log.lines()
        .filter_map(|l| {
            let (time, sha) = l.split_once(' ')?;
            Some((time.parse::<i64>().ok()?, sha.trim()))
        })
        .min()
        .map(|(_, sha)| sha.to_string())
}

/// `origin`'s URL from `git config --get-regexp` output, else the first one.
fn pick_remote(config: &str) -> Option<String> {
    let remotes: Vec<(&str, &str)> = config.lines().filter_map(|l| l.split_once(' ')).collect();
    remotes
        .iter()
        .find(|(k, _)| *k == "remote.origin.url")
        .or(remotes.first())
        .map(|(_, url)| url.trim().to_string())
}

/// A remote URL as `host/owner/repo`, so the https and ssh forms of one
/// remote are equal: no scheme, user, port or `.git`, and a lowercase host.
/// `None` for a local path, which names a directory rather than a repo.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        if url.starts_with("file://") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        (authority, path)
    } else {
        // scp-like: [user@]host:path. A colon after a slash is a local path.
        let (host, path) = url.split_once(':')?;
        if host.contains('/') || host.contains('\\') || host.len() == 1 {
            return None;
        }
        (host, path)
    };
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?.to_lowercase();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run git in `dir` with a fixed identity, away from the caller's
    /// repository and git config; trimmed stdout.
    fn test_git(dir: &Path, args: &[&str]) -> String {
        let out = git_command(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join(".no-gitconfig"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn https_and_ssh_remotes_normalize_alike() {
        let want = Some("github.com/moshen/sclerox".to_string());
        for url in [
            "git@github.com:moshen/sclerox.git",
            "https://github.com/moshen/sclerox.git",
            "https://user@GitHub.com/moshen/sclerox/",
            "ssh://git@github.com:22/moshen/sclerox.git",
            "git://github.com/moshen/sclerox",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
    }

    #[test]
    fn local_remotes_are_not_identities() {
        for url in [
            "/srv/git/repo.git",
            "../repo",
            "file:///srv/repo.git",
            "C:\\repos\\x",
            "C:/repos/x",
        ] {
            assert_eq!(normalize_remote(url), None, "{url}");
        }
    }

    #[test]
    fn origin_wins_over_other_remotes() {
        let config = "remote.upstream.url https://a/x\nremote.origin.url https://b/y\n";
        assert_eq!(pick_remote(config).as_deref(), Some("https://b/y"));
        assert_eq!(
            pick_remote("remote.fork.url https://a/x").as_deref(),
            Some("https://a/x")
        );
    }

    #[test]
    fn the_oldest_first_commit_wins() {
        assert_eq!(
            oldest_root("200 bbb\n100 ccc\n100 aaa").as_deref(),
            Some("aaa")
        );
        assert_eq!(oldest_root(""), None);
    }

    #[test]
    fn a_repo_is_described_by_its_first_commit_remote_and_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| test_git(&repo, args);
        run(&["init", "-q", "-b", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "first"]);
        let first = run(&["rev-parse", "HEAD"]);
        run(&["commit", "-q", "--allow-empty", "-m", "second"]);
        let head = run(&["rev-parse", "HEAD"]);
        run(&["remote", "add", "origin", "git@example.com:o/proj.git"]);
        std::fs::create_dir(repo.join("sub")).unwrap();

        let db = Database::open_in_memory().unwrap();
        let info = RepoInfo::at(&db, &repo.join("sub")).unwrap();
        assert_eq!(info.name, "proj");
        assert_eq!(info.root_commit.as_deref(), Some(first.as_str()));
        assert_eq!(info.commit.as_deref(), Some(head.as_str()));
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.remote.as_deref(), Some("example.com/o/proj"));
        // Cached, and still right the second time.
        let path = info.root.to_string_lossy().into_owned();
        let (cached, _) = db.repo_root_cached(&path).unwrap().unwrap();
        assert_eq!(cached, first);
        assert_eq!(
            RepoInfo::at(&db, &repo).unwrap().root_commit,
            Some(first.clone())
        );

        assert!(RepoInfo::at(&db, tmp.path()).is_none());

        // Packed refs, a detached HEAD and a linked worktree read the same.
        run(&["pack-refs", "--all"]);
        let packed = RepoInfo::at(&db, &repo).unwrap();
        assert_eq!(packed.commit.as_deref(), Some(head.as_str()));
        assert_eq!(packed.branch.as_deref(), Some("main"));
        run(&["checkout", "-q", "--detach", &first]);
        let detached = RepoInfo::at(&db, &repo).unwrap();
        assert_eq!(detached.commit.as_deref(), Some(first.as_str()));
        assert_eq!(detached.branch, None);
        let tree = tmp.path().join("tree");
        run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            tree.to_str().unwrap(),
            "main",
        ]);
        let linked = RepoInfo::at(&db, &tree).unwrap();
        assert_eq!(linked.commit.as_deref(), Some(head.as_str()));
        assert_eq!(linked.branch.as_deref(), Some("side"));
        assert_eq!(linked.remote.as_deref(), Some("example.com/o/proj"));
        assert_eq!(linked.root_commit.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn a_shallow_clone_has_no_first_commit_but_keeps_its_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let git_in = |dir: &Path, args: &[&str]| {
            test_git(dir, args);
        };
        std::fs::create_dir(&src).unwrap();
        git_in(&src, &["init", "-q", "-b", "main"]);
        git_in(&src, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git_in(&src, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let url = format!("file://{}", src.to_string_lossy().replace('\\', "/"));
        git_in(
            tmp.path(),
            &["clone", "-q", "--depth", "1", &url, "shallow"],
        );
        let shallow = tmp.path().join("shallow");
        git_in(
            &shallow,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.com/o/src.git",
            ],
        );

        let db = Database::open_in_memory().unwrap();
        let info = RepoInfo::at(&db, &shallow).unwrap();
        assert!(info.commit.is_some());
        assert_eq!(info.root_commit, None);
        assert_eq!(info.remote.as_deref(), Some("example.com/o/src"));
    }
}
