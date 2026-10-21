use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::output::truncate;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SearchResult {
    Memory {
        id: i64,
        key: String,
        snippet: String,
        created_at: String,
        updated_at: String,
    },
    Person {
        id: i64,
        name: String,
        created_at: String,
        updated_at: String,
    },
    Meeting {
        id: i64,
        title: String,
        date: Option<String>,
        snippet: String,
        created_at: String,
    },
    Project {
        id: i64,
        name: String,
        description: Option<String>,
        created_at: String,
        updated_at: String,
    },
    Todo {
        id: i64,
        title: String,
        status: String,
        category: String,
        created_at: String,
        updated_at: String,
    },
    Investigation {
        id: i64,
        name: String,
        slug: String,
        status: String,
        snippet: String,
        created_at: String,
        updated_at: String,
    },
    Repo {
        id: i64,
        name: String,
        path: String,
        description: Option<String>,
        created_at: String,
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
    /// indexed content - catches comments, strings, and logic FTS symbol search
    /// misses.
    CodeChunk {
        repo_name: String,
        repo_path: String,
        file_path: String,
        start_line: Option<i64>,
        score: f32,
        snippet: String,
    },
}

/// A search result and its cosine similarity to the query, when it has one.
/// People, projects, symbols and rows without a stored vector have none.
#[derive(Debug, Clone)]
pub struct Hit {
    pub result: SearchResult,
    pub score: Option<f32>,
}

/// Search every table for `query`, best first across types: hits with a
/// cosine score by that score, then the ones without, in [`search_hits`]
/// order. With no embedder nothing is scored, so that order is today's
/// grouping by type.
pub fn global_search(
    db: &Database,
    query: &str,
    semantic_threshold: f32,
    semantic_margin: Option<f32>,
    semantic_limit: usize,
) -> Result<Vec<SearchResult>> {
    let hits = search_hits(
        db,
        query,
        semantic_threshold,
        semantic_margin,
        semantic_limit,
    )?;
    Ok(rank(hits).into_iter().map(|h| h.result).collect())
}

/// Order `hits` best first: scored hits by score, then unscored hits in the
/// order given.
pub fn rank(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| match (a.score, b.score) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    hits
}

/// Every hit for `query`, grouped by type in a fixed order. FTS/LIKE always
/// runs, and its hits are kept whatever their score; semantic (cosine) tiers
/// run when an embedder is available, keeping hits at/above
/// `semantic_threshold` and capping each entity type at `semantic_limit`.
/// With a `semantic_margin`, hits more than that far below the best hit of
/// their entity type are dropped too (see `embed::models::semantic_cutoff`).
/// The tunables are passed in so this library function stays config-free.
pub fn search_hits(
    db: &Database,
    query: &str,
    semantic_threshold: f32,
    semantic_margin: Option<f32>,
    semantic_limit: usize,
) -> Result<Vec<Hit>> {
    use crate::db::ScoredTable;

    let cutoff = |scores: &mut dyn Iterator<Item = f32>| {
        crate::embed::models::semantic_cutoff(scores, semantic_threshold, semantic_margin)
    };
    let mut results = Vec::new();

    // Initialise embedder once; embed query once. Both are None if unavailable.
    // Each database checks its own recorded model when searched.
    let query_emb: Option<Vec<f32>> =
        crate::embed::optional(true).and_then(|mut emb| emb.embed_query(query).ok());
    // Cosine of each keyword hit, from its stored vector, so it ranks with
    // the semantic hits.
    let fts_scores = |table: ScoredTable, ids: &[i64]| match &query_emb {
        Some(qe) => db.cosine_scores(table, ids, qe).unwrap_or_default(),
        None => Default::default(),
    };

    let memory_fts = db.memory_search(query)?;
    let memory_fts_ids: std::collections::HashSet<i64> = memory_fts.iter().map(|m| m.id).collect();
    let scores = fts_scores(
        ScoredTable::Memory,
        &memory_fts.iter().map(|m| m.id).collect::<Vec<_>>(),
    );
    for m in memory_fts {
        results.push(Hit {
            score: scores.get(&m.id).copied(),
            result: SearchResult::Memory {
                id: m.id,
                key: m.key,
                snippet: truncate(&m.value, 120),
                created_at: m.created_at,
                updated_at: m.updated_at,
            },
        });
    }
    if let Some(ref qe) = query_emb {
        let hits = db.memory_similar(qe, semantic_limit).unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !memory_fts_ids.contains(&r.entry.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    result: SearchResult::Memory {
                        id: r.entry.id,
                        key: r.entry.key,
                        snippet: truncate(&r.entry.value, 120),
                        created_at: r.entry.created_at,
                        updated_at: r.entry.updated_at,
                    },
                });
            }
        }
    }

    for p in db.people_search(query)? {
        results.push(Hit {
            score: None,
            result: SearchResult::Person {
                id: p.id,
                name: p.name,
                created_at: p.created_at,
                updated_at: p.updated_at,
            },
        });
    }

    let meeting_fts = db.meeting_search(query)?;
    let meeting_fts_ids: std::collections::HashSet<i64> =
        meeting_fts.iter().map(|m| m.id).collect();
    let scores = fts_scores(
        ScoredTable::Meeting,
        &meeting_fts.iter().map(|m| m.id).collect::<Vec<_>>(),
    );
    for m in meeting_fts {
        let snippet = m
            .notes
            .as_deref()
            .or(m.transcript.as_deref())
            .map(|s| truncate(s, 120))
            .unwrap_or_default();
        results.push(Hit {
            score: scores.get(&m.id).copied(),
            result: SearchResult::Meeting {
                id: m.id,
                title: m.title,
                date: m.meeting_date,
                snippet,
                created_at: m.created_at,
            },
        });
    }
    if let Some(ref qe) = query_emb {
        let hits = db.meeting_similar(qe, semantic_limit).unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !meeting_fts_ids.contains(&r.meeting.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    result: SearchResult::Meeting {
                        id: r.meeting.id,
                        title: r.meeting.title,
                        date: r.meeting.meeting_date,
                        snippet: truncate(&r.matched_chunk, 120),
                        created_at: r.meeting.created_at,
                    },
                });
            }
        }
    }

    for p in db.project_search(query)? {
        results.push(Hit {
            score: None,
            result: SearchResult::Project {
                id: p.id,
                name: p.name,
                description: p.description,
                created_at: p.created_at,
                updated_at: p.updated_at,
            },
        });
    }

    let todo_fts: Vec<_> = db.todo_search(query)?;
    let todo_fts_ids: std::collections::HashSet<i64> = todo_fts.iter().map(|t| t.id).collect();
    let scores = fts_scores(
        ScoredTable::Todo,
        &todo_fts.iter().map(|t| t.id).collect::<Vec<_>>(),
    );
    for t in todo_fts {
        results.push(Hit {
            score: scores.get(&t.id).copied(),
            result: SearchResult::Todo {
                id: t.id,
                title: t.title,
                status: t.status,
                category: t.category,
                created_at: t.created_at,
                updated_at: t.updated_at,
            },
        });
    }
    if let Some(ref qe) = query_emb {
        let hits = db.todo_similar(qe, semantic_limit).unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !todo_fts_ids.contains(&r.todo.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    result: SearchResult::Todo {
                        id: r.todo.id,
                        title: r.todo.title,
                        status: r.todo.status,
                        category: r.todo.category,
                        created_at: r.todo.created_at,
                        updated_at: r.todo.updated_at,
                    },
                });
            }
        }
    }

    let inv_fts = db.investigation_search(query)?;
    let inv_fts_ids: std::collections::HashSet<i64> = inv_fts.iter().map(|i| i.id).collect();
    let scores = fts_scores(
        ScoredTable::Investigation,
        &inv_fts.iter().map(|i| i.id).collect::<Vec<_>>(),
    );
    for i in inv_fts {
        let snippet = i
            .findings
            .as_deref()
            .or(i.plan.as_deref())
            .map(|s| truncate(s, 120))
            .unwrap_or_default();
        results.push(Hit {
            score: scores.get(&i.id).copied(),
            result: SearchResult::Investigation {
                id: i.id,
                name: i.name,
                slug: i.slug,
                status: i.status,
                snippet,
                created_at: i.created_at,
                updated_at: i.updated_at,
            },
        });
    }
    if let Some(ref qe) = query_emb {
        let hits = db
            .investigation_similar(qe, semantic_limit)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !inv_fts_ids.contains(&r.investigation.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    result: SearchResult::Investigation {
                        id: r.investigation.id,
                        name: r.investigation.name,
                        slug: r.investigation.slug,
                        status: r.investigation.status,
                        snippet: truncate(&r.matched_chunk, 120),
                        created_at: r.investigation.created_at,
                        updated_at: r.investigation.updated_at,
                    },
                });
            }
        }
    }

    // Drop deleted clones before any repo rows are read.
    db.repo_prune_gone()?;
    let repo_fts = db.repo_search(query)?;
    let repo_fts_ids: std::collections::HashSet<i64> = repo_fts.iter().map(|r| r.id).collect();
    let scores = fts_scores(
        ScoredTable::Repo,
        &repo_fts.iter().map(|r| r.id).collect::<Vec<_>>(),
    );
    for r in repo_fts {
        results.push(Hit {
            score: scores.get(&r.id).copied(),
            result: SearchResult::Repo {
                id: r.id,
                name: r.name,
                path: r.path,
                description: r.description,
                created_at: r.created_at,
            },
        });
    }
    if let Some(ref qe) = query_emb {
        let hits = db.repo_similar(qe, semantic_limit).unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !repo_fts_ids.contains(&r.repo.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    result: SearchResult::Repo {
                        id: r.repo.id,
                        name: r.repo.name,
                        path: r.repo.path,
                        description: r.repo.description,
                        created_at: r.repo.created_at,
                    },
                });
            }
        }
    }

    // Fan out to all registered repo DBs: FTS symbol search always, plus a
    // semantic code-chunk pass (ranked globally across repos) when embeddings
    // are available.
    let mut code_chunks: Vec<(
        f32,
        crate::db::repos::RepoEntry,
        crate::index::repo_db::SimilarChunk,
    )> = Vec::new();
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
            results.push(Hit {
                score: None,
                result: SearchResult::Symbol {
                    repo_name: repo.name.clone(),
                    repo_path: repo.path.clone(),
                    kind: sym.kind,
                    name: sym.name,
                    signature: sym.signature,
                    file_path: sym.file_path,
                    start_line: sym.start_line,
                },
            });
        }
        if let Some(ref qe) = query_emb {
            for c in repo_db
                .similar_chunks(qe, semantic_limit)
                .unwrap_or_default()
            {
                if c.score >= semantic_threshold {
                    code_chunks.push((c.score, repo.clone(), c));
                }
            }
        }
    }
    // Rank code chunks across all repos and keep the global top N, dropping
    // any too far below the best of them.
    let cut = cutoff(&mut code_chunks.iter().map(|(s, _, _)| *s));
    code_chunks.retain(|(s, _, _)| *s >= cut);
    code_chunks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    code_chunks.truncate(semantic_limit);
    for (score, repo, c) in code_chunks {
        results.push(Hit {
            score: Some(score),
            result: SearchResult::CodeChunk {
                repo_name: repo.name,
                repo_path: repo.path,
                file_path: c.file_path,
                start_line: c.start_line,
                score,
                snippet: truncate(c.chunk_text.trim(), 120),
            },
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_puts_scored_hits_first_best_first() {
        let person = |id| Hit {
            score: None,
            result: SearchResult::Person {
                id,
                name: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            },
        };
        let scored = |id, score| Hit {
            score: Some(score),
            ..person(id)
        };
        let ids: Vec<i64> = rank(vec![person(1), scored(2, 0.7), person(3), scored(4, 0.9)])
            .into_iter()
            .map(|h| match h.result {
                SearchResult::Person { id, .. } => id,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, [4, 2, 1, 3]);
    }

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

        let results = global_search(&db, "Rust", 0.45, None, 5).unwrap();
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
