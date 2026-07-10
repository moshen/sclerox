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
    /// A semantically-matched code chunk (not a named symbol) from a repo's
    /// indexed content — catches comments, strings, and logic FTS symbol search
    /// misses.
    CodeChunk {
        repo_name: String,
        file_path: String,
        start_line: Option<i64>,
        snippet: String,
    },
}

/// Search every table for `query`. FTS/LIKE always runs; semantic (cosine)
/// tiers run when an embedder is available, keeping hits at/above
/// `semantic_threshold` and capping each entity type at `semantic_limit`.
/// Both tunables are passed in so this library function stays config-free.
pub fn global_search(
    db: &Database,
    query: &str,
    semantic_threshold: f32,
    semantic_limit: usize,
) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();

    // Initialise embedder once; embed query once. Both are None if unavailable.
    let query_emb: Option<Vec<f32>> = crate::embed::Embedder::new()
        .ok()
        .and_then(|mut emb| emb.embed_one(query).ok());

    let memory_fts = db.memory_search(query)?;
    let memory_fts_ids: std::collections::HashSet<i64> = memory_fts.iter().map(|m| m.id).collect();
    for m in memory_fts {
        results.push(SearchResult::Memory {
            id: m.id,
            key: m.key,
            snippet: truncate(&m.value, 120),
        });
    }
    if let Some(ref qe) = query_emb {
        for r in db.memory_similar(qe, semantic_limit).unwrap_or_default() {
            if !memory_fts_ids.contains(&r.entry.id) && r.score >= semantic_threshold {
                results.push(SearchResult::Memory {
                    id: r.entry.id,
                    key: r.entry.key,
                    snippet: truncate(&r.entry.value, 120),
                });
            }
        }
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
        for r in db.meeting_similar(qe, semantic_limit).unwrap_or_default() {
            if !meeting_fts_ids.contains(&r.meeting.id) && r.score >= semantic_threshold {
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
        for r in db.todo_similar(qe, semantic_limit).unwrap_or_default() {
            if !todo_fts_ids.contains(&r.todo.id) && r.score >= semantic_threshold {
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
        for r in db
            .investigation_similar(qe, semantic_limit)
            .unwrap_or_default()
        {
            if !inv_fts_ids.contains(&r.investigation.id) && r.score >= semantic_threshold {
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
        for r in db.repo_similar(qe, semantic_limit).unwrap_or_default() {
            if !repo_fts_ids.contains(&r.repo.id) && r.score >= semantic_threshold {
                results.push(SearchResult::Repo {
                    id: r.repo.id,
                    name: r.repo.name,
                    path: r.repo.path,
                    description: r.repo.description,
                });
            }
        }
    }

    // Fan out to all registered repo DBs: FTS symbol search always, plus a
    // semantic code-chunk pass (ranked globally across repos) when embeddings
    // are available.
    let mut code_chunks: Vec<(f32, String, crate::index::repo_db::SimilarChunk)> = Vec::new();
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
        if let Some(ref qe) = query_emb {
            for c in repo_db
                .similar_chunks(qe, semantic_limit)
                .unwrap_or_default()
            {
                if c.score >= semantic_threshold {
                    code_chunks.push((c.score, repo.name.clone(), c));
                }
            }
        }
    }
    // Rank code chunks across all repos and keep the global top N.
    code_chunks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    code_chunks.truncate(semantic_limit);
    for (_, repo_name, c) in code_chunks {
        results.push(SearchResult::CodeChunk {
            repo_name,
            file_path: c.file_path,
            start_line: c.start_line,
            snippet: truncate(c.chunk_text.trim(), 120),
        });
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

        let results = global_search(&db, "Rust", 0.45, 5).unwrap();
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
                SearchResult::CodeChunk { .. } => "code",
            })
            .collect();
        assert!(types.contains(&"todo"), "missing todo results");
        assert!(
            types.contains(&"investigation"),
            "missing investigation results"
        );
    }
}
