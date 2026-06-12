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
        email: Option<String>,
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
        kind: String,
        name: String,
        signature: Option<String>,
        file_path: String,
        start_line: i64,
    },
}

pub fn global_search(db: &Database, query: &str) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();

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
            email: p.email,
        });
    }

    for m in db.meeting_search(query)? {
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

    for p in db.project_search(query)? {
        results.push(SearchResult::Project {
            id: p.id,
            name: p.name,
            description: p.description,
        });
    }

    for t in db.todo_search(query)? {
        results.push(SearchResult::Todo {
            id: t.id,
            title: t.title,
            status: t.status,
            category: t.category,
        });
    }

    for i in db.investigation_search(query)? {
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

    for r in db.repo_search(query)? {
        results.push(SearchResult::Repo {
            id: r.id,
            name: r.name,
            path: r.path,
            description: r.description,
        });
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
        db.people_add("Rustacean Bob", None, None, None, None, None, None)
            .unwrap();
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
