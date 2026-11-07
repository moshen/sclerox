//! Where memories came from, and the repos and projects they belong to (see
//! `MIGRATION_V17`). Writers record a [`Provenance`] after storing a memory;
//! supersedes and in-place updates carry the old rows over themselves (see
//! `memory.rs`).

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::Database;

/// One source of a memory. Every field but `kind` is optional: a source
/// records what it knows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// `manual` (`memory set` and `supersede`), `session` (distilled from an
    /// agent session), `distill` (`memory distill`) or `import`.
    pub kind: String,
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub repo_name: Option<String>,
    pub repo_path: Option<String>,
    /// First commit of the repo, see [`RepoScope`].
    pub repo_root: Option<String>,
    pub repo_remote: Option<String>,
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    /// The file imported or distilled from.
    pub source_path: Option<String>,
    pub transcript_path: Option<String>,
    /// 1-based lines of the transcript the memory was distilled from (for
    /// OpenCode, message positions in the session).
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    /// Ids of the first and last message in that range, which still find it
    /// if the transcript's lines ever move.
    pub message_start: Option<String>,
    pub message_end: Option<String>,
    /// SHA-256 of the text the distiller read, after `<private>` stripping.
    pub excerpt_sha256: Option<String>,
    /// When the source said it: the last message's time, for a session.
    pub observed_at: Option<String>,
}

/// A stored [`Provenance`] with the time it was recorded.
#[derive(Debug, Clone, Serialize)]
pub struct ProvenanceRow {
    #[serde(flatten)]
    pub provenance: Provenance,
    pub created_at: String,
}

/// A repo a memory is scoped to. Two scopes name the same repo when their
/// first commits or their remotes match; the name decides only when a scope
/// has neither.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoScope {
    pub repo_name: String,
    pub root_commit: Option<String>,
    pub remote: Option<String>,
}

const PROVENANCE_COLS: &str = "kind, session_id, agent, repo_name, repo_path, repo_root, \
     repo_remote, commit_sha, branch, source_path, transcript_path, line_start, line_end, \
     message_start, message_end, excerpt_sha256, observed_at";

/// Copy `from`'s provenance rows onto memory `to`, skipping any it already
/// has. Used when a memory is merged into another and when an update keeps
/// the old version as history.
pub(super) fn copy_provenance(conn: &Connection, from: i64, to: i64) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO memory_provenance (memory_id, {PROVENANCE_COLS}, created_at)
             SELECT ?2, {PROVENANCE_COLS}, created_at FROM memory_provenance p
             WHERE p.memory_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM memory_provenance q WHERE q.memory_id = ?2
                   AND {same})",
            same = same_provenance("p", "q")
        ),
        params![from, to],
    )?;
    Ok(())
}

/// Copy `from`'s repo scopes and project links onto memory `to`, when `from`
/// is merged into it.
pub(super) fn copy_links(conn: &Connection, from: i64, to: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_repos (memory_id, repo_name, root_commit, remote)
         SELECT ?2, repo_name, root_commit, remote FROM memory_repos WHERE memory_id = ?1",
        params![from, to],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_projects (memory_id, project_id)
         SELECT ?2, project_id FROM memory_projects WHERE memory_id = ?1",
        params![from, to],
    )?;
    Ok(())
}

/// SQL true when rows `a` and `b` record the same source.
fn same_provenance(a: &str, b: &str) -> String {
    PROVENANCE_COLS
        .split(", ")
        .map(|c| format!("{a}.{c} IS {b}.{c}", c = c.trim()))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// SQL over `memory_repos` true when its row names the repo bound to
/// ?1 (name), ?2 (first commit) and ?3 (remote).
const SCOPE_MATCHES: &str = "((root_commit IS NOT NULL AND root_commit = ?2)
      OR (remote IS NOT NULL AND remote = ?3)
      OR (root_commit IS NULL AND remote IS NULL AND repo_name = ?1))";

impl Database {
    /// Record a source of memory `memory_id`. Recording the same source twice
    /// (a session distilled again) adds nothing. Returns whether a row was
    /// added.
    pub fn memory_provenance_add(&self, memory_id: i64, p: &Provenance) -> Result<bool> {
        let n = self.conn.execute(
            &format!(
                "INSERT INTO memory_provenance (memory_id, {PROVENANCE_COLS})
                 SELECT * FROM (SELECT ?1 AS memory_id, ?2 AS kind, ?3 AS session_id,
                     ?4 AS agent, ?5 AS repo_name, ?6 AS repo_path, ?7 AS repo_root,
                     ?8 AS repo_remote, ?9 AS commit_sha, ?10 AS branch, ?11 AS source_path,
                     ?12 AS transcript_path, ?13 AS line_start, ?14 AS line_end,
                     ?15 AS message_start, ?16 AS message_end, ?17 AS excerpt_sha256,
                     ?18 AS observed_at) p
                 WHERE NOT EXISTS (
                     SELECT 1 FROM memory_provenance q WHERE q.memory_id = ?1 AND {same})",
                same = same_provenance("p", "q")
            ),
            params![
                memory_id,
                p.kind,
                p.session_id,
                p.agent,
                p.repo_name,
                p.repo_path,
                p.repo_root,
                p.repo_remote,
                p.commit_sha,
                p.branch,
                p.source_path,
                p.transcript_path,
                p.line_start,
                p.line_end,
                p.message_start,
                p.message_end,
                p.excerpt_sha256,
                p.observed_at,
            ],
        )?;
        Ok(n > 0)
    }

    /// Every recorded source of memory `memory_id`, oldest first.
    pub fn memory_provenance(&self, memory_id: i64) -> Result<Vec<ProvenanceRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROVENANCE_COLS}, created_at FROM memory_provenance
             WHERE memory_id = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([memory_id], |r| {
            Ok(ProvenanceRow {
                provenance: Provenance {
                    kind: r.get(0)?,
                    session_id: r.get(1)?,
                    agent: r.get(2)?,
                    repo_name: r.get(3)?,
                    repo_path: r.get(4)?,
                    repo_root: r.get(5)?,
                    repo_remote: r.get(6)?,
                    commit_sha: r.get(7)?,
                    branch: r.get(8)?,
                    source_path: r.get(9)?,
                    transcript_path: r.get(10)?,
                    line_start: r.get(11)?,
                    line_end: r.get(12)?,
                    message_start: r.get(13)?,
                    message_end: r.get(14)?,
                    excerpt_sha256: r.get(15)?,
                    observed_at: r.get(16)?,
                },
                created_at: r.get(17)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Scope memory `memory_id` to `repo`, in addition to any repos it is
    /// already scoped to.
    pub fn memory_scope_add(&self, memory_id: i64, repo: &RepoScope) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_repos (memory_id, repo_name, root_commit, remote)
             VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, repo.repo_name, repo.root_commit, repo.remote],
        )?;
        Ok(())
    }

    /// Make memory `memory_id` global again. Returns whether it had a scope.
    pub fn memory_scope_clear(&self, memory_id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memory_repos WHERE memory_id = ?1", [memory_id])?;
        Ok(n > 0)
    }

    /// The repos memory `memory_id` is scoped to; empty for a global memory.
    pub fn memory_scopes(&self, memory_id: i64) -> Result<Vec<RepoScope>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_name, root_commit, remote FROM memory_repos
             WHERE memory_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([memory_id], |r| {
            Ok(RepoScope {
                repo_name: r.get(0)?,
                root_commit: r.get(1)?,
                remote: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Ids of memories scoped to `repo`, and of those scoped only to other
    /// repos.
    pub fn memory_scope_ids(
        &self,
        repo: Option<&RepoScope>,
    ) -> Result<(HashSet<i64>, HashSet<i64>)> {
        let empty = RepoScope::default();
        let repo = repo.unwrap_or(&empty);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT memory_id, MAX(CASE WHEN {SCOPE_MATCHES} THEN 1 ELSE 0 END)
             FROM memory_repos GROUP BY memory_id"
        ))?;
        let rows = stmt.query_map(
            params![repo.repo_name, repo.root_commit, repo.remote],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
        )?;
        let (mut here, mut elsewhere) = (HashSet::new(), HashSet::new());
        for row in rows {
            let (id, matches) = row?;
            if matches {
                here.insert(id);
            } else {
                elsewhere.insert(id);
            }
        }
        Ok((here, elsewhere))
    }

    /// Link memory `memory_id` to project `project_id`. Returns false when
    /// either does not exist.
    pub fn memory_link_project(&self, memory_id: i64, project_id: i64) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM memory WHERE id = ?1)
                AND EXISTS (SELECT 1 FROM projects WHERE id = ?2)",
            params![memory_id, project_id],
            |r| r.get(0),
        )?;
        if exists {
            self.conn.execute(
                "INSERT OR IGNORE INTO memory_projects (memory_id, project_id) VALUES (?1, ?2)",
                params![memory_id, project_id],
            )?;
        }
        Ok(exists)
    }

    /// Ids and names of the projects memory `memory_id` is linked to.
    pub fn memory_projects(&self, memory_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name FROM memory_projects mp JOIN projects p ON p.id = mp.project_id
             WHERE mp.memory_id = ?1 ORDER BY p.id",
        )?;
        let rows = stmt.query_map([memory_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keys of the active memories linked to project `project_id`.
    pub fn project_memory_keys(&self, project_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.key FROM memory_projects mp JOIN memory m ON m.id = mp.memory_id
             WHERE mp.project_id = ?1 AND m.status = 'active' ORDER BY m.key",
        )?;
        let rows = stmt.query_map([project_id], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The first commit cached for the clone at `path`, and the stamp it was
    /// cached under.
    pub fn repo_root_cached(&self, path: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT root_commit, clone_stamp FROM repo_roots WHERE path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Cache the first commit of the clone at `path`, created at `stamp`.
    pub fn repo_root_store(
        &self,
        path: &str,
        stamp: Option<&str>,
        root_commit: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repo_roots (path, clone_stamp, root_commit) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET clone_stamp = excluded.clone_stamp,
                                             root_commit = excluded.root_commit,
                                             checked_at = datetime('now')",
            params![path, stamp, root_commit],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(lines: (i64, i64)) -> Provenance {
        Provenance {
            kind: "session".into(),
            session_id: Some("s1".into()),
            line_start: Some(lines.0),
            line_end: Some(lines.1),
            ..Default::default()
        }
    }

    #[test]
    fn the_same_source_is_recorded_once() {
        let db = Database::open_in_memory().unwrap();
        let id = db.memory_set("k", "v", "project", None).unwrap();
        assert!(db.memory_provenance_add(id, &session((1, 9))).unwrap());
        assert!(!db.memory_provenance_add(id, &session((1, 9))).unwrap());
        assert!(db.memory_provenance_add(id, &session((10, 20))).unwrap());
        let rows = db.memory_provenance(id).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].provenance, session((10, 20)));
    }

    #[test]
    fn deleting_a_memory_drops_its_provenance_scope_and_projects() {
        let db = Database::open_in_memory().unwrap();
        let id = db.memory_set("k", "v", "project", None).unwrap();
        let project = db.project_add("p", None, &[]).unwrap();
        db.memory_provenance_add(id, &session((1, 2))).unwrap();
        db.memory_scope_add(id, &repo("a", Some("r1"), None))
            .unwrap();
        assert!(db.memory_link_project(id, project).unwrap());
        db.memory_delete("k").unwrap();
        for table in ["memory_provenance", "memory_repos", "memory_projects"] {
            let n: i64 = db
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }

    fn repo(name: &str, root: Option<&str>, remote: Option<&str>) -> RepoScope {
        RepoScope {
            repo_name: name.into(),
            root_commit: root.map(Into::into),
            remote: remote.map(Into::into),
        }
    }

    #[test]
    fn a_scope_matches_on_first_commit_or_remote_and_on_name_only_without_either() {
        let db = Database::open_in_memory().unwrap();
        let by_root = db.memory_set("a", "v", "project", None).unwrap();
        let by_remote = db.memory_set("b", "v", "project", None).unwrap();
        let by_name = db.memory_set("c", "v", "project", None).unwrap();
        let other = db.memory_set("d", "v", "project", None).unwrap();
        let named_alike = db.memory_set("e", "v", "project", None).unwrap();
        db.memory_scope_add(by_root, &repo("old-name", Some("r1"), None))
            .unwrap();
        db.memory_scope_add(by_remote, &repo("x", None, Some("github.com/o/x")))
            .unwrap();
        db.memory_scope_add(by_name, &repo("x", None, None))
            .unwrap();
        db.memory_scope_add(other, &repo("y", Some("r2"), None))
            .unwrap();
        // Same name, but a first commit that says it is another repo.
        db.memory_scope_add(named_alike, &repo("x", Some("r3"), None))
            .unwrap();

        let here = repo("x", Some("r1"), Some("github.com/o/x"));
        let (matched, elsewhere) = db.memory_scope_ids(Some(&here)).unwrap();
        assert_eq!(matched, HashSet::from([by_root, by_remote, by_name]));
        assert_eq!(elsewhere, HashSet::from([other, named_alike]));

        let (matched, elsewhere) = db.memory_scope_ids(None).unwrap();
        assert!(matched.is_empty());
        assert_eq!(elsewhere.len(), 5);
    }

    #[test]
    fn history_keeps_the_sources_it_had_and_a_merge_keeps_all() {
        let db = Database::open_in_memory().unwrap();
        let id = db.memory_set("k", "v1", "project", None).unwrap();
        db.memory_provenance_add(id, &session((1, 2))).unwrap();
        db.memory_scope_add(id, &repo("a", Some("r1"), None))
            .unwrap();
        let project = db.project_add("p", None, &[]).unwrap();
        db.memory_link_project(id, project).unwrap();

        // Updated in place: the row keeps its id, the old version its source.
        assert_eq!(db.memory_set("k", "v2", "project", None).unwrap(), id);
        db.memory_provenance_add(id, &session((3, 4))).unwrap();
        let history = db.memory_history("k").unwrap();
        let old = history.iter().find(|e| e.value == "v1").unwrap();
        let kept: Vec<_> = db
            .memory_provenance(old.id)
            .unwrap()
            .into_iter()
            .map(|r| r.provenance)
            .collect();
        assert_eq!(kept, [session((1, 2))]);

        // Merged into another key: every source, scope and project moves along.
        let other = db.memory_set("k2", "w", "project", None).unwrap();
        db.memory_provenance_add(other, &session((5, 6))).unwrap();
        assert!(db
            .memory_supersede("k", "k2", "merged", "project", "session")
            .unwrap());
        let mut lines: Vec<_> = db
            .memory_provenance(other)
            .unwrap()
            .into_iter()
            .map(|r| r.provenance.line_start.unwrap())
            .collect();
        lines.sort();
        assert_eq!(lines, [1, 3, 5]);
        assert_eq!(
            db.memory_scopes(other).unwrap(),
            [repo("a", Some("r1"), None)]
        );
        assert_eq!(db.memory_projects(other).unwrap(), [(project, "p".into())]);
    }

    #[test]
    fn linking_a_missing_project_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let id = db.memory_set("k", "v", "project", None).unwrap();
        assert!(!db.memory_link_project(id, 99).unwrap());
        assert!(db.memory_projects(id).unwrap().is_empty());
    }
}
