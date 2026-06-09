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
    Repo {
        id: i64,
        name: String,
        path: String,
        description: Option<String>,
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

    for r in db.repo_search(query)? {
        results.push(SearchResult::Repo {
            id: r.id,
            name: r.name,
            path: r.path,
            description: r.description,
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
        db.people_add("Rustacean Bob", None, None, None, None, None, None)
            .unwrap();
        db.meeting_add("Rust Review", None, None, Some("discussed Rust async"))
            .unwrap();
        db.project_add("Rust Migration", Some("Moving to Rust"), &[])
            .unwrap();

        let results = global_search(&db, "Rust").unwrap();
        // Should find entries in memory, people, meetings, projects
        assert!(results.len() >= 3);
    }
}
