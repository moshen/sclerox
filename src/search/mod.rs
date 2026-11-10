use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::dates::Dates;
use crate::db::Database;
use crate::db::SnippetTable;
use crate::output::{excerpt, full, truncate};

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
        /// When the file last changed: its last commit, or its time on disk
        /// if it has uncommitted changes. `None` until it is next indexed.
        last_modified: Option<String>,
    },
    /// A line of an indexed file holding every term of the query (substring,
    /// any case): the full-text tier of code search.
    CodeText {
        repo_name: String,
        repo_path: String,
        file_path: String,
        line: i64,
        text: String,
        last_modified: Option<String>,
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
        last_modified: Option<String>,
    },
}

impl SearchResult {
    /// The record type and reference that name this hit across runs, as in
    /// `todo` and `12`: a memory by key, research by slug, a repo by name, a
    /// symbol by name and code by file, so any chunk of a file is the same
    /// item. Evaluation labels and usage logs both use it.
    pub fn item(&self) -> (&'static str, String) {
        match self {
            SearchResult::Memory { key, .. } => ("memory", key.clone()),
            SearchResult::Person { id, .. } => ("person", id.to_string()),
            SearchResult::Meeting { id, .. } => ("meeting", id.to_string()),
            SearchResult::Project { id, .. } => ("project", id.to_string()),
            SearchResult::Todo { id, .. } => ("todo", id.to_string()),
            SearchResult::Investigation { slug, .. } => ("research", slug.clone()),
            SearchResult::Repo { name, .. } => ("repo", name.clone()),
            SearchResult::Symbol { name, .. } => ("symbol", name.clone()),
            SearchResult::CodeText { file_path, .. }
            | SearchResult::CodeChunk { file_path, .. } => ("code", file_path.clone()),
        }
    }
}

/// Which tier of [`search_hits`] found a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitSource {
    /// Full-text or `LIKE` matching: memory, people, meetings, projects,
    /// todos, research, repos, symbols and code lines.
    Keyword,
    /// Nearest neighbours of the query vector that the keyword tier missed.
    Semantic,
}

/// A search result and its cosine similarity to the query, when it has one.
/// People, projects, symbols and rows without a stored vector have none.
#[derive(Debug, Clone)]
pub struct Hit {
    pub result: SearchResult,
    pub score: Option<f32>,
    pub source: HitSource,
}

/// How [`order`] puts hits of every type into one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ranking {
    /// [`fuse`]: keyword and cosine ranks together.
    #[default]
    Fusion,
    /// [`rank`]: cosine score alone.
    Cosine,
}

/// `hits` best first, by `ranking`.
pub fn order(hits: Vec<Hit>, query: &str, ranking: Ranking) -> Vec<Hit> {
    match ranking {
        Ranking::Fusion => fuse(hits, query),
        Ranking::Cosine => rank(hits),
    }
}

/// The `k` of reciprocal rank fusion: how far a rank's weight falls from the
/// top of a list. 60 is the value from the original paper and usual default.
const RRF_K: f64 = 60.0;

/// Order `hits` by reciprocal rank fusion: each hit scores
/// `Σ 1 / (RRF_K + rank)` over the lists it is in. Each type's keyword hits
/// are one list, in the order [`search_hits`] gives them, as bm25 scores from
/// different tables can't be compared; every hit with a cosine score is in
/// one more list across types. A hit whose key, title or name is the query
/// itself comes first.
pub fn fuse(hits: Vec<Hit>, query: &str) -> Vec<Hit> {
    let mut fused = vec![0.0; hits.len()];
    let mut keyword_ranks: std::collections::HashMap<std::mem::Discriminant<SearchResult>, f64> =
        Default::default();
    for (i, hit) in hits.iter().enumerate() {
        if hit.source == HitSource::Keyword {
            let rank = keyword_ranks
                .entry(std::mem::discriminant(&hit.result))
                .or_default();
            *rank += 1.0;
            fused[i] += 1.0 / (RRF_K + *rank);
        }
    }
    let mut scored: Vec<(usize, f32)> = hits
        .iter()
        .enumerate()
        .filter_map(|(i, h)| h.score.map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (rank, (i, _)) in scored.into_iter().enumerate() {
        fused[i] += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    let mut ordered: Vec<(bool, f64, Hit)> = hits
        .into_iter()
        .zip(fused)
        .map(|(hit, score)| (!names_query(&hit.result, query), score, hit))
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
    ordered.into_iter().map(|(_, _, hit)| hit).collect()
}

/// Whether `query` is the hit's key, title, name or slug, ignoring case.
fn names_query(result: &SearchResult, query: &str) -> bool {
    let names: Vec<&String> = match result {
        SearchResult::Memory { key, .. } => vec![key],
        SearchResult::Person { name, .. }
        | SearchResult::Project { name, .. }
        | SearchResult::Repo { name, .. }
        | SearchResult::Symbol { name, .. } => vec![name],
        SearchResult::Meeting { title, .. } | SearchResult::Todo { title, .. } => vec![title],
        SearchResult::Investigation { name, slug, .. } => vec![name, slug],
        SearchResult::CodeText { .. } | SearchResult::CodeChunk { .. } => vec![],
    };
    let query = query.trim().to_lowercase();
    names.iter().any(|name| name.trim().to_lowercase() == query)
}

/// Order `hits` best first across types: scored hits by score, then
/// unscored hits in the order given. With no embedder nothing is scored, so
/// that order is [`search_hits`]' grouping by type.
pub fn rank(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| match (a.score, b.score) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    hits
}

/// The settings [`search_hits`] runs with, passed in so this library stays
/// config-free.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Semantic hits below this cosine score are dropped.
    pub semantic_threshold: f32,
    /// Semantic hits more than this far below the best of their type are
    /// dropped too (see `embed::models::semantic_cutoff`).
    pub semantic_margin: Option<f32>,
    /// Most semantic hits of each type.
    pub semantic_limit: usize,
    /// Most code lines across all repos (see [`code_text_hits`]).
    pub code_text_limit: usize,
}

/// Every hit for `query` dated within `dates`, grouped by type in a fixed
/// order. FTS/LIKE always runs, and its hits are kept whatever their score;
/// semantic (cosine) tiers run when an embedder is available and keep the
/// hits `tuning` allows. Code is also searched word for word.
pub fn search_hits(db: &Database, query: &str, tuning: &Tuning, dates: &Dates) -> Result<Vec<Hit>> {
    // None if the embedder is unavailable. Each database checks its own
    // recorded model when searched.
    let query_emb: Option<Vec<f32>> =
        crate::embed::optional(true).and_then(|mut emb| emb.embed_query(query).ok());
    search_hits_embedded(db, query, query_emb.as_deref(), tuning, dates)
}

/// [`search_hits`] with the query already embedded, or `None` to search by
/// keyword only. Evaluation embeds each query once and runs every ranking
/// on that vector, instead of loading the model per search.
pub fn search_hits_embedded(
    db: &Database,
    query: &str,
    query_emb: Option<&[f32]>,
    tuning: &Tuning,
    dates: &Dates,
) -> Result<Vec<Hit>> {
    use crate::db::ScoredTable;

    let Tuning {
        semantic_threshold,
        semantic_margin,
        semantic_limit,
        code_text_limit,
    } = *tuning;
    let cutoff = |scores: &mut dyn Iterator<Item = f32>| {
        crate::embed::models::semantic_cutoff(scores, semantic_threshold, semantic_margin)
    };
    let mut results = Vec::new();
    // Where each keyword hit matched, unless --full shows the whole text.
    let snippets_of = |table: SnippetTable, ids: &std::collections::HashSet<i64>| {
        if full() {
            return Default::default();
        }
        let ids: Vec<i64> = ids.iter().copied().collect();
        db.keyword_snippets(table, query, &ids).unwrap_or_else(|e| {
            log::warn!("could not cut snippets for {table:?} hits: {e:#}");
            Default::default()
        })
    };
    // Cosine of each keyword hit, from its stored vector, so it ranks with
    // the semantic hits.
    let fts_scores = |table: ScoredTable, ids: &[i64]| match query_emb {
        Some(qe) => db.cosine_scores(table, ids, qe).unwrap_or_default(),
        None => Default::default(),
    };

    let memory_fts = db.memory_search(query, dates)?;
    let memory_fts_ids: std::collections::HashSet<i64> = memory_fts.iter().map(|m| m.id).collect();
    let scores = fts_scores(
        ScoredTable::Memory,
        &memory_fts.iter().map(|m| m.id).collect::<Vec<_>>(),
    );
    let snippets = snippets_of(SnippetTable::Memory, &memory_fts_ids);
    for m in memory_fts {
        results.push(Hit {
            score: scores.get(&m.id).copied(),
            source: HitSource::Keyword,
            result: SearchResult::Memory {
                id: m.id,
                key: m.key,
                snippet: keyword_snippet(snippets.get(&m.id), &[Some(&m.value)], query),
                created_at: m.created_at,
                updated_at: m.updated_at,
            },
        });
    }
    if let Some(qe) = query_emb {
        let hits = db
            .memory_similar(qe, semantic_limit, dates)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !memory_fts_ids.contains(&r.entry.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    source: HitSource::Semantic,
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

    for p in db.people_search(query, dates)? {
        results.push(Hit {
            score: None,
            source: HitSource::Keyword,
            result: SearchResult::Person {
                id: p.id,
                name: p.name,
                created_at: p.created_at,
                updated_at: p.updated_at,
            },
        });
    }

    let meeting_fts = db.meeting_search(query, dates)?;
    let meeting_fts_ids: std::collections::HashSet<i64> =
        meeting_fts.iter().map(|m| m.id).collect();
    let scores = fts_scores(
        ScoredTable::Meeting,
        &meeting_fts.iter().map(|m| m.id).collect::<Vec<_>>(),
    );
    let snippets = snippets_of(SnippetTable::Meeting, &meeting_fts_ids);
    for m in meeting_fts {
        let snippet = keyword_snippet(
            snippets.get(&m.id),
            &[m.notes.as_deref(), m.transcript.as_deref()],
            query,
        );
        results.push(Hit {
            score: scores.get(&m.id).copied(),
            source: HitSource::Keyword,
            result: SearchResult::Meeting {
                id: m.id,
                title: m.title,
                date: m.meeting_date,
                snippet,
                created_at: m.created_at,
            },
        });
    }
    if let Some(qe) = query_emb {
        let hits = db
            .meeting_similar(qe, semantic_limit, dates)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !meeting_fts_ids.contains(&r.meeting.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    source: HitSource::Semantic,
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

    for p in db.project_search(query, dates)? {
        results.push(Hit {
            score: None,
            source: HitSource::Keyword,
            result: SearchResult::Project {
                id: p.id,
                name: p.name,
                description: p.description,
                created_at: p.created_at,
                updated_at: p.updated_at,
            },
        });
    }

    let todo_fts: Vec<_> = db.todo_search(query, None, dates)?;
    let todo_fts_ids: std::collections::HashSet<i64> = todo_fts.iter().map(|t| t.id).collect();
    let scores = fts_scores(
        ScoredTable::Todo,
        &todo_fts.iter().map(|t| t.id).collect::<Vec<_>>(),
    );
    for t in todo_fts {
        results.push(Hit {
            score: scores.get(&t.id).copied(),
            source: HitSource::Keyword,
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
    if let Some(qe) = query_emb {
        let hits = db
            .todo_similar(qe, semantic_limit, None, dates)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !todo_fts_ids.contains(&r.todo.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    source: HitSource::Semantic,
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

    let inv_fts = db.investigation_search(query, dates)?;
    let inv_fts_ids: std::collections::HashSet<i64> = inv_fts.iter().map(|i| i.id).collect();
    let scores = fts_scores(
        ScoredTable::Investigation,
        &inv_fts.iter().map(|i| i.id).collect::<Vec<_>>(),
    );
    let snippets = snippets_of(SnippetTable::Investigation, &inv_fts_ids);
    for i in inv_fts {
        let snippet = keyword_snippet(
            snippets.get(&i.id),
            &[i.findings.as_deref(), i.plan.as_deref()],
            query,
        );
        results.push(Hit {
            score: scores.get(&i.id).copied(),
            source: HitSource::Keyword,
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
    if let Some(qe) = query_emb {
        let hits = db
            .investigation_similar(qe, semantic_limit, dates)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !inv_fts_ids.contains(&r.investigation.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    source: HitSource::Semantic,
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
    let repo_fts = db.repo_search(query, dates)?;
    let repo_fts_ids: std::collections::HashSet<i64> = repo_fts.iter().map(|r| r.id).collect();
    let scores = fts_scores(
        ScoredTable::Repo,
        &repo_fts.iter().map(|r| r.id).collect::<Vec<_>>(),
    );
    for r in repo_fts {
        results.push(Hit {
            score: scores.get(&r.id).copied(),
            source: HitSource::Keyword,
            result: SearchResult::Repo {
                id: r.id,
                name: r.name,
                path: r.path,
                description: r.description,
                created_at: r.created_at,
            },
        });
    }
    if let Some(qe) = query_emb {
        let hits = db
            .repo_similar(qe, semantic_limit, dates)
            .unwrap_or_default();
        let cut = cutoff(&mut hits.iter().map(|r| r.score));
        for r in hits {
            if !repo_fts_ids.contains(&r.repo.id) && r.score >= cut {
                results.push(Hit {
                    score: Some(r.score),
                    source: HitSource::Semantic,
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

    // Fan out to all registered repo DBs: symbol and full-text search always,
    // plus a semantic code-chunk pass (ranked globally across repos) when
    // embeddings are available.
    let mut symbols = Vec::new();
    let mut code_text = Vec::new();
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
        let Ok(repo_symbols) = repo_db.search_symbols(query, dates) else {
            log::error!("symbol search failed for repo '{}'", repo.name);
            continue;
        };
        log::debug!(
            "repo '{}': {} symbol hits for '{}'",
            repo.name,
            repo_symbols.len(),
            query
        );
        symbols.extend(repo_symbols.into_iter().map(|sym| SearchResult::Symbol {
            repo_name: repo.name.clone(),
            repo_path: repo.path.clone(),
            kind: sym.kind,
            name: sym.name,
            signature: sym.signature,
            file_path: sym.file_path,
            start_line: sym.start_line,
            last_modified: sym.last_modified,
        }));
        code_text_hits(
            &mut code_text,
            &repo,
            &repo_db,
            query,
            code_text_limit,
            dates,
        );
        if let Some(qe) = query_emb {
            for c in repo_db
                .similar_chunks(qe, semantic_limit, dates)
                .unwrap_or_default()
            {
                if c.score >= semantic_threshold {
                    code_chunks.push((c.score, repo.clone(), c));
                }
            }
        }
    }
    // Each repo's symbols come best first; keep that order across repos.
    symbols.sort_by_key(|s| match s {
        SearchResult::Symbol { name, .. } => crate::index::repo_db::symbol_match(name, query),
        _ => unreachable!(),
    });
    results.extend(symbols.into_iter().map(|result| Hit {
        score: None,
        source: HitSource::Keyword,
        result,
    }));
    results.extend(
        rank_code_text(code_text, code_text_limit)
            .into_iter()
            .map(|result| Hit {
                score: None,
                source: HitSource::Keyword,
                result,
            }),
    );
    // Rank code chunks across all repos and keep the global top N, dropping
    // any too far below the best of them.
    let cut = cutoff(&mut code_chunks.iter().map(|(s, _, _)| *s));
    code_chunks.retain(|(s, _, _)| *s >= cut);
    code_chunks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    code_chunks.truncate(semantic_limit);
    for (score, repo, c) in code_chunks {
        results.push(Hit {
            score: Some(score),
            source: HitSource::Semantic,
            result: SearchResult::CodeChunk {
                repo_name: repo.name,
                repo_path: repo.path,
                file_path: c.file_path,
                start_line: c.start_line,
                score,
                snippet: truncate(c.chunk_text.trim(), 120),
                last_modified: c.last_modified,
            },
        });
    }

    Ok(results)
}

/// What a keyword hit shows of its text: `found`, the passage where the query
/// matched (see [`Database::keyword_snippets`]), else an [`excerpt`] of the
/// first of `texts` holding the query, else the start of the first.
fn keyword_snippet(found: Option<&String>, texts: &[Option<&str>], query: &str) -> String {
    if let Some(found) = found {
        return found.clone();
    }
    let mut texts = texts.iter().flatten();
    texts
        .clone()
        .find_map(|t| excerpt(t, query, 120))
        .or_else(|| texts.next().map(|t| truncate(t, 120)))
        .unwrap_or_default()
}

/// A line holding the query word for word, from one repo, before the lines
/// of all repos are ranked together by [`rank_code_text`].
pub struct CodeTextHit {
    phrase: bool,
    result: SearchResult,
}

/// Add up to `limit` of `repo`'s lines holding `query` word for word to
/// `out`; [`rank_code_text`] then applies `limit` across all repos. Each line
/// is cut to a snippet, as a minified file can hold megabytes on one. A
/// failed lookup is logged and leaves the other search tiers running.
pub fn code_text_hits(
    out: &mut Vec<CodeTextHit>,
    repo: &crate::db::repos::RepoEntry,
    repo_db: &crate::index::repo_db::RepoDb,
    query: &str,
    limit: usize,
    dates: &Dates,
) {
    match repo_db.search_text(std::path::Path::new(&repo.path), query, limit, dates) {
        Ok(lines) => out.extend(lines.into_iter().map(|m| CodeTextHit {
            phrase: m.phrase,
            result: SearchResult::CodeText {
                repo_name: repo.name.clone(),
                repo_path: repo.path.clone(),
                file_path: m.file_path,
                line: m.line,
                text: excerpt(&m.text, query, 120).unwrap_or_else(|| truncate(&m.text, 120)),
                last_modified: m.last_modified,
            },
        })),
        Err(e) => log::error!("full-text search failed for repo '{}': {e:#}", repo.name),
    }
}

/// The best `limit` of every repo's [`code_text_hits`]: lines holding the
/// query as one phrase first, wherever they are, each group in repo order.
pub fn rank_code_text(mut hits: Vec<CodeTextHit>, limit: usize) -> Vec<SearchResult> {
    hits.sort_by_key(|h| !h.phrase);
    hits.into_iter().take(limit).map(|h| h.result).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TUNING: Tuning = Tuning {
        semantic_threshold: 0.45,
        semantic_margin: None,
        semantic_limit: 5,
        code_text_limit: 20,
    };

    #[test]
    fn rank_puts_scored_hits_first_best_first() {
        let person = |id| Hit {
            score: None,
            source: HitSource::Keyword,
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

    fn keyword(result: SearchResult) -> Hit {
        Hit {
            score: None,
            source: HitSource::Keyword,
            result,
        }
    }

    fn memory(key: &str) -> SearchResult {
        SearchResult::Memory {
            id: 0,
            key: key.into(),
            snippet: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn label(h: &Hit) -> String {
        h.result.item().1
    }

    #[test]
    fn fuse_puts_an_exact_name_first_past_many_similar_hits() {
        let person = SearchResult::Person {
            id: 7,
            name: "Ada Lovelace".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let project = SearchResult::Project {
            id: 8,
            name: "Invoice extraction".into(),
            description: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let symbol = SearchResult::Symbol {
            repo_name: String::new(),
            repo_path: String::new(),
            kind: "function".into(),
            name: "parse_invoice".into(),
            signature: None,
            file_path: String::new(),
            start_line: 1,
            last_modified: None,
        };
        for (named, query) in [
            (person, "ada lovelace"),
            (project, "Invoice Extraction"),
            (symbol, "parse_invoice"),
        ] {
            // Memories found both ways, every one scoring high on cosine.
            let mut hits: Vec<Hit> = (0..20)
                .map(|i| Hit {
                    score: Some(0.9 - i as f32 / 100.0),
                    ..keyword(memory(&format!("note-{i}")))
                })
                .collect();
            let want = named.item().1;
            hits.push(keyword(named));
            assert_eq!(label(&fuse(hits, query)[0]), want, "{query}");
        }
    }

    #[test]
    fn fuse_ranks_a_hit_found_both_ways_above_one_found_once() {
        let semantic = |key, score| Hit {
            score: Some(score),
            source: HitSource::Semantic,
            result: memory(key),
        };
        let both = Hit {
            score: Some(0.7),
            ..keyword(memory("both"))
        };
        let hits = vec![
            keyword(memory("keyword-only")),
            both,
            semantic("semantic-only", 0.9),
        ];
        // Each of the others is first in one list, so scores less.
        assert_eq!(label(&fuse(hits, "query")[0]), "both");
    }

    #[test]
    fn keyword_hits_show_the_passage_that_matched() {
        let db = Database::open_in_memory().unwrap();
        let filler = "word ".repeat(60);
        let text = |what| format!("{filler}the zebrafish {what} {filler}");
        db.memory_set("long-note", &text("tank"), "feedback", None)
            .unwrap();
        db.meeting_add("Planning", None, Some(&text("rollout")), None)
            .unwrap();
        db.investigation_start("Fish", "fish", Some(&text("plan")))
            .unwrap();
        // A whole word through full-text search, and part of one through the
        // substring tier.
        for query in ["zebrafish", "ebrafis"] {
            let snippets: Vec<String> =
                search_hits_embedded(&db, query, None, &TUNING, &Dates::default())
                    .unwrap()
                    .into_iter()
                    .filter_map(|h| match h.result {
                        SearchResult::Memory { snippet, .. }
                        | SearchResult::Meeting { snippet, .. }
                        | SearchResult::Investigation { snippet, .. } => Some(snippet),
                        _ => None,
                    })
                    .collect();
            assert_eq!(snippets.len(), 3, "{query}: {snippets:?}");
            for snippet in snippets {
                assert!(snippet.contains(query), "{query}: {snippet}");
                assert!(snippet.chars().count() < 150, "{query}: {snippet}");
            }
        }
    }

    #[test]
    fn rank_code_text_puts_phrase_lines_first_across_repos() {
        let hit = |repo: &str, phrase| CodeTextHit {
            phrase,
            result: SearchResult::CodeText {
                repo_name: repo.into(),
                repo_path: String::new(),
                file_path: String::new(),
                line: 1,
                text: String::new(),
                last_modified: None,
            },
        };
        let repos: Vec<String> =
            rank_code_text(vec![hit("a1", false), hit("a2", false), hit("b1", true)], 2)
                .into_iter()
                .map(|r| match r {
                    SearchResult::CodeText { repo_name, .. } => repo_name,
                    _ => unreachable!(),
                })
                .collect();
        assert_eq!(repos, ["b1", "a1"]);
    }

    #[test]
    fn test_search_across_tables() {
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

        let results: Vec<SearchResult> =
            rank(search_hits(&db, "Rust", &TUNING, &Dates::default()).unwrap())
                .into_iter()
                .map(|h| h.result)
                .collect();
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
                SearchResult::CodeText { .. } => "code-text",
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
