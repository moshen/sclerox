pub mod similarity;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SearchResult {
    Memory {
        id: i64,
        key: String,
        snippet: String,
    },
    Person {
        id: i64,
        name: String,
    },
    Meeting {
        id: i64,
        title: String,
        date: Option<String>,
        snippet: String,
    },
    Project {
        id: i64,
        name: String,
        description: Option<String>,
    },
    Todo {
        id: i64,
        title: String,
        status: String,
        category: String,
    },
    Investigation {
        id: i64,
        name: String,
        slug: String,
        status: String,
        snippet: String,
    },
    Repo {
        id: i64,
        name: String,
        path: String,
        description: Option<String>,
    },
    Symbol {
        repo_name: String,
        repo_path: String,
        kind: String,
        name: String,
        signature: Option<String>,
        file_path: String,
        start_line: i64,
    },
}

pub fn global_search(db: &Database, query: &str) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();

    // Initialise embedder once; embed query once. Both are None if unavailable.
    let query_emb: Option<Vec<f32>> = crate::embed::Embedder::new()
        .ok()
        .and_then(|mut emb| emb.embed_one(query).ok());

    for m in db.memory_search(query)? {
        results.push(SearchResult::Memory {
            id: m.id,
            key: m.key,
            snippet: truncate(&m.value, 120),
        });
    }

    for p in db.people_search(query)? {
        results.push(SearchResult::Person {
            id: p.id,
            name: p.name,
        });
    }

    let meeting_fts = db.meeting_search(query)?;
    let meeting_fts_ids: std::collections::HashSet<i64> =
        meeting_fts.iter().map(|m| m.id).collect();
    for m in meeting_fts {
        let snippet = m
            .notes
            .as_deref()
            .or(m.transcript.as_deref())
            .map(|s| truncate(s, 120))
            .unwrap_or_default();
        results.push(SearchResult::Meeting {
            id: m.id,
            title: m.title,
            date: m.meeting_date,
            snippet,
        });
    }
    if let Some(ref qe) = query_emb {
        for r in db.meeting_similar(qe, 5).unwrap_or_default() {
            if !meeting_fts_ids.contains(&r.meeting.id) && r.score >= 0.45 {
                results.push(SearchResult::Meeting {
                    id: r.meeting.id,
                    title: r.meeting.title,
                    date: r.meeting.meeting_date,
                    snippet: truncate(&r.matched_chunk, 120),
                });
            }
        }
    }

    for p in db.project_search(query)? {
        results.push(SearchResult::Project {
            id: p.id,
            name: p.name,
            description: p.description,
        });
    }

    let todo_fts: Vec<_> = db.todo_search(query)?;
    let todo_fts_ids: std::collections::HashSet<i64> = todo_fts.iter().map(|t| t.id).collect();
    for t in todo_fts {
        results.push(SearchResult::Todo {
            id: t.id,
            title: t.title,
            status: t.status,
            category: t.category,
        });
    }
    if let Some(ref qe) = query_emb {
        for r in db.todo_similar(qe, 10).unwrap_or_default() {
            if !todo_fts_ids.contains(&r.todo.id) && r.score >= 0.45 {
                results.push(SearchResult::Todo {
                    id: r.todo.id,
                    title: r.todo.title,
                    status: r.todo.status,
                    category: r.todo.category,
                });
            }
        }
    }

    let inv_fts = db.investigation_search(query)?;
    let inv_fts_ids: std::collections::HashSet<i64> = inv_fts.iter().map(|i| i.id).collect();
    for i in inv_fts {
        let snippet = i
            .findings
            .as_deref()
            .or(i.plan.as_deref())
            .map(|s| truncate(s, 120))
            .unwrap_or_default();
        results.push(SearchResult::Investigation {
            id: i.id,
            name: i.name,
            slug: i.slug,
            status: i.status,
            snippet,
        });
    }
    if let Some(ref qe) = query_emb {
        for r in db.investigation_similar(qe, 5).unwrap_or_default() {
            if !inv_fts_ids.contains(&r.investigation.id) && r.score >= 0.45 {
                results.push(SearchResult::Investigation {
                    id: r.investigation.id,
                    name: r.investigation.name,
                    slug: r.investigation.slug,
                    status: r.investigation.status,
                    snippet: truncate(&r.matched_chunk, 120),
                });
            }
        }
    }

    let repo_fts = db.repo_search(query)?;
    let repo_fts_ids: std::collections::HashSet<i64> = repo_fts.iter().map(|r| r.id).collect();
    for r in repo_fts {
        results.push(SearchResult::Repo {
            id: r.id,
            name: r.name,
            path: r.path,
            description: r.description,
        });
    }
    if let Some(ref qe) = query_emb {
        for r in db.repo_similar(qe, 5).unwrap_or_default() {
            if !repo_fts_ids.contains(&r.repo.id) && r.score >= 0.45 {
                results.push(SearchResult::Repo {
                    id: r.repo.id,
                    name: r.repo.name,
                    path: r.repo.path,
                    description: r.repo.description,
                });
            }
        }
    }

    // Fan out to all registered repo DBs and search symbols
    for repo in db.repo_list()? {
        let db_path = std::path::Path::new(&repo.db_path);
        if !db_path.exists() {
            log::debug!("repo '{}' db missing, skipping symbol search", repo.name);
            continue;
        }
        let Ok(repo_db) = crate::index::repo_db::RepoDb::open(db_path) else {
            log::error!("could not open repo db for '{}'", repo.name);
            continue;
        };
        let Ok(symbols) = repo_db.search_symbols(query) else {
            log::error!("symbol search failed for repo '{}'", repo.name);
            continue;
        };
        log::debug!(
            "repo '{}': {} symbol hits for '{}'",
            repo.name,
            symbols.len(),
            query
        );
        for sym in symbols {
            results.push(SearchResult::Symbol {
                repo_name: repo.name.clone(),
                repo_path: repo.path.clone(),
                kind: sym.kind,
                name: sym.name,
                signature: sym.signature,
                file_path: sym.file_path,
                start_line: sym.start_line,
            });
        }
    }

    Ok(results)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_search_across_tables() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("rust-tip", "Rust lifetimes are key", "feedback", None)
            .unwrap();
        db.people_add("Rustacean Bob", None).unwrap();
        db.meeting_add("Rust Review", None, None, Some("discussed Rust async"))
            .unwrap();
        db.project_add("Rust Migration", Some("Moving to Rust"), &[])
            .unwrap();
        db.todo_add(
            "Migrate to Rust",
            None,
            crate::db::todos::TodoStatus::Open,
            None,
            "general",
            None,
            None,
        )
        .unwrap();
        db.investigation_start(
            "Rust performance",
            "rust-perf",
            Some("Investigate Rust perf"),
        )
        .unwrap();

        let results = global_search(&db, "Rust").unwrap();
        assert!(
            results.len() >= 5,
            "expected at least 5, got {}",
            results.len()
        );

        let types: Vec<&str> = results
            .iter()
            .map(|r| match r {
                SearchResult::Memory { .. } => "memory",
                SearchResult::Person { .. } => "person",
                SearchResult::Meeting { .. } => "meeting",
                SearchResult::Project { .. } => "project",
                SearchResult::Todo { .. } => "todo",
                SearchResult::Investigation { .. } => "investigation",
                SearchResult::Repo { .. } => "repo",
                SearchResult::Symbol { .. } => "symbol",
            })
            .collect();
        assert!(types.contains(&"todo"), "missing todo results");
        assert!(
            types.contains(&"investigation"),
            "missing investigation results"
        );
    }
}
