use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{embedding_to_bytes, fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub description: Option<String>,
    pub db_path: String,
    pub last_indexed: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarRepo {
    pub repo: RepoEntry,
    pub score: f32,
}

impl Database {
    pub fn repo_register(
        &self,
        path: &str,
        name: &str,
        description: Option<&str>,
        db_path: &str,
        description_embedding: Option<&[f32]>,
    ) -> Result<i64> {
        // Another model's vectors in this database: keep the stored one (the
        // COALESCE below) rather than mixing in ours.
        self.write_with_vectors(description_embedding.is_some(), |keep| {
        let emb_bytes = description_embedding.filter(|_| keep).map(embedding_to_bytes);
        self.conn.execute(
            "INSERT INTO repos (path, name, description, db_path, last_indexed, description_embedding)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5)
             ON CONFLICT(path) DO UPDATE SET
               name = excluded.name,
               description = excluded.description,
               db_path = excluded.db_path,
               last_indexed = datetime('now'),
               description_embedding = COALESCE(excluded.description_embedding, repos.description_embedding)",
            params![path, name, description, db_path, emb_bytes],
        )?;
        Ok(self.conn.last_insert_rowid())
        })
    }

    pub fn repo_get_by_path(&self, path: &str) -> Result<Option<RepoEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, name, description, db_path, last_indexed, created_at
             FROM repos WHERE path = ?1",
        )?;
        match stmt.query_row(params![path], row_to_repo) {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn repo_list(&self) -> Result<Vec<RepoEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, name, description, db_path, last_indexed, created_at
             FROM repos ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_repo)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Repos whose name or description matches `query` in full text (word
    /// prefixes), or whose name, description or path holds it as a substring.
    pub fn repo_search(&self, query: &str) -> Result<Vec<RepoEntry>> {
        let fts_query = fts::sanitize(query);
        let like_pat = fts::like_contains(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, path, name, description, db_path, last_indexed, created_at
             FROM repos
             WHERE id IN (SELECT rowid FROM repos_fts WHERE repos_fts MATCH ?1)
                OR name LIKE ?2 ESCAPE '\\' OR description LIKE ?2 ESCAPE '\\'
                OR path LIKE ?2 ESCAPE '\\'
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![fts_query, like_pat], row_to_repo)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Repos whose description is nearest to `query_embedding`, via the
    /// sqlite-vec KNN index (`repos_vec`, cosine). `score` = 1 - cosine distance.
    pub fn repo_similar(&self, query_embedding: &[f32], limit: usize) -> Result<Vec<SimilarRepo>> {
        self.semantic_read(|| {
            let query = crate::db::embedding_to_bytes(query_embedding);
            let mut stmt = self.conn.prepare(
            "SELECT r.id, r.path, r.name, r.description, r.db_path, r.last_indexed, r.created_at,
                    v.distance
             FROM repos_vec v
             JOIN repos r ON r.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance",
        )?;
            let rows = stmt.query_map(params![query, limit as i64], |row| {
                let repo = RepoEntry {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    db_path: row.get(4)?,
                    last_indexed: row.get(5)?,
                    created_at: row.get(6)?,
                };
                let distance: f64 = row.get(7)?;
                Ok(SimilarRepo {
                    repo,
                    score: 1.0 - distance as f32,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Point a registered repo at a new `repo.db` without re-indexing it.
    ///
    /// Used by the `ol` -> `sclerox` migration: the per-repo index directory is
    /// renamed `.ol` -> `.sclerox` in place, so the index itself is still valid
    /// and only the recorded path needs to catch up.
    pub fn repo_set_db_path(&self, path: &str, db_path: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE repos SET db_path = ?1 WHERE path = ?2",
            params![db_path, path],
        )?;
        Ok(n > 0)
    }

    pub fn repo_remove(&self, path: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM repos WHERE path = ?1", params![path])?;
        Ok(n > 0)
    }

    /// Delete registry rows whose repo directory is gone and return them.
    /// A path that cannot be checked is kept, and a failed delete is logged
    /// rather than returned so the calling command still runs.
    pub fn repo_prune_gone(&self) -> Result<Vec<RepoEntry>> {
        let mut removed = Vec::new();
        for repo in self.repo_list()? {
            if !matches!(std::path::Path::new(&repo.path).try_exists(), Ok(false)) {
                continue;
            }
            match self.repo_remove(&repo.path) {
                Ok(_) => {
                    log::info!(
                        "removed stale repo entry '{}' (directory gone: {})",
                        repo.name,
                        repo.path
                    );
                    removed.push(repo);
                }
                Err(e) => log::warn!("could not remove stale repo entry '{}': {e:#}", repo.name),
            }
        }
        Ok(removed)
    }

    /// Check all registered repos and return health actions for each.
    /// Does not perform any writes - callers act on the returned report.
    pub fn repo_health_check(&self) -> Result<Vec<RepoHealth>> {
        let repos = self.repo_list()?;
        let mut report = Vec::new();

        for repo in repos {
            let path = std::path::Path::new(&repo.path);
            let db_path = std::path::Path::new(&repo.db_path);

            let status = if !path.exists() {
                RepoHealthStatus::DirectoryGone
            } else if !db_path.exists() {
                RepoHealthStatus::DbMissing
            } else {
                // Check repo DB schema version
                match crate::index::repo_db::RepoDb::open(db_path) {
                    Ok(rdb) => match rdb.migration_status() {
                        Ok((current, target, _)) if current < target => {
                            RepoHealthStatus::SchemaBehind { current, target }
                        }
                        Ok(_) => RepoHealthStatus::Ok,
                        Err(e) => RepoHealthStatus::Error(e.to_string()),
                    },
                    Err(e) => RepoHealthStatus::Error(e.to_string()),
                }
            };

            report.push(RepoHealth { repo, status });
        }

        Ok(report)
    }
}

#[derive(Debug)]
pub struct RepoHealth {
    pub repo: RepoEntry,
    pub status: RepoHealthStatus,
}

#[derive(Debug)]
pub enum RepoHealthStatus {
    /// Repo is indexed and schema is current.
    Ok,
    /// Repo directory no longer exists - should be removed from registry.
    DirectoryGone,
    /// Repo directory exists but the DB file is missing - needs reindex.
    DbMissing,
    /// DB exists but its schema is behind the current version.
    SchemaBehind { current: u32, target: u32 },
    /// Could not open the DB.
    Error(String),
}

fn row_to_repo(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoEntry> {
    Ok(RepoEntry {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        db_path: row.get(4)?,
        last_indexed: row.get(5)?,
        created_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_register_and_get() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .repo_register(
                "/home/user/myrepo",
                "myrepo",
                Some("A Rust web service"),
                "/home/user/myrepo/.sclerox/repo.db",
                None,
            )
            .unwrap();
        assert!(id >= 0);
        let entry = db.repo_get_by_path("/home/user/myrepo").unwrap().unwrap();
        assert_eq!(entry.name, "myrepo");
        assert_eq!(entry.description.as_deref(), Some("A Rust web service"));
    }

    #[test]
    fn prune_gone_removes_only_rows_whose_directory_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("kept");
        std::fs::create_dir(&kept).unwrap();
        let kept = kept.to_str().unwrap();
        let gone = dir.path().join("gone");
        let gone = gone.to_str().unwrap();

        let db = Database::open_in_memory().unwrap();
        // The kept repo has no repo.db: a missing index is `sync`'s job to
        // rebuild, not a reason to drop the row.
        db.repo_register(
            kept,
            "kept",
            None,
            &format!("{kept}/.sclerox/repo.db"),
            None,
        )
        .unwrap();
        db.repo_register(
            gone,
            "gone",
            None,
            &format!("{gone}/.sclerox/repo.db"),
            None,
        )
        .unwrap();

        let removed = db.repo_prune_gone().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "gone");
        let left: Vec<_> = db
            .repo_list()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(left, ["kept"]);

        assert!(db.repo_prune_gone().unwrap().is_empty(), "idempotent");
    }

    #[test]
    fn test_repo_register_upserts() {
        let db = Database::open_in_memory().unwrap();
        db.repo_register(
            "/path/repo",
            "repo",
            Some("old desc"),
            "/path/.sclerox/repo.db",
            None,
        )
        .unwrap();
        db.repo_register(
            "/path/repo",
            "repo",
            Some("new desc"),
            "/path/.sclerox/repo.db",
            None,
        )
        .unwrap();

        let repos = db.repo_list().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].description.as_deref(), Some("new desc"));
    }

    #[test]
    fn test_repo_search_fts() {
        let db = Database::open_in_memory().unwrap();
        db.repo_register(
            "/a",
            "auth-service",
            Some("handles OAuth and JWT"),
            "/a/.sclerox/repo.db",
            None,
        )
        .unwrap();
        db.repo_register(
            "/b",
            "payment-api",
            Some("Stripe payment integration"),
            "/b/.sclerox/repo.db",
            None,
        )
        .unwrap();

        let results = db.repo_search("OAuth").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "auth-service");

        // Mid-word substrings of the name, description and path match too.
        for q in ["service", "tripe", "/b"] {
            let names: Vec<_> = db
                .repo_search(q)
                .unwrap()
                .into_iter()
                .map(|r| r.name)
                .collect();
            let want = if q == "service" {
                "auth-service"
            } else {
                "payment-api"
            };
            assert_eq!(names, [want], "query {q:?}");
        }
    }

    #[test]
    fn test_repo_similar() {
        let db = Database::open_in_memory().unwrap();
        let unit = |i: usize| {
            let mut v = vec![0.0f32; 384];
            v[i] = 1.0;
            v
        };

        db.repo_register(
            "/a",
            "repo-a",
            Some("systems code"),
            "/a/.sclerox/repo.db",
            Some(&unit(0)),
        )
        .unwrap();
        db.repo_register(
            "/b",
            "repo-b",
            Some("web frontend"),
            "/b/.sclerox/repo.db",
            Some(&unit(1)),
        )
        .unwrap();

        let mut query = vec![0.0f32; 384];
        query[0] = 0.95;
        query[1] = 0.05;
        let results = db.repo_similar(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].repo.name, "repo-a");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_repo_remove() {
        let db = Database::open_in_memory().unwrap();
        db.repo_register("/x", "x", None, "/x/.sclerox/repo.db", None)
            .unwrap();
        assert!(db.repo_remove("/x").unwrap());
        assert!(!db.repo_remove("/x").unwrap());
        assert!(db.repo_get_by_path("/x").unwrap().is_none());
    }
}
