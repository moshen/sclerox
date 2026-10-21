//! Compare today's grouped search with ranking across types, on your own
//! knowledge base: the database and settings `sclerox` uses, and queries
//! from a local TOML file that stays out of the repo.
//!
//! ```text
//! SCLEROX_SEARCH_EVAL=~/search-eval.toml \
//!     cargo test --release --bin sclerox search_live_eval -- --ignored --nocapture
//! ```
//!
//! The file lists queries, each with the hits that answer it, if you know
//! them:
//!
//! ```toml
//! [[query]]
//! q = "why did we drop CoreML"
//! relevant = ["memory:gpu-coreml", "todo:12", "code:src/embed/mod.rs"]
//! ```
//!
//! A label is `<type>:<id>`, printed beside every hit, so a first run with no
//! `relevant` lists them to copy from. Types: `memory:<key>`, `person:<id>`,
//! `meeting:<id>`, `project:<id>`, `todo:<id>`, `research:<slug>`,
//! `repo:<name>`, `symbol:<name>` and `code:<file>`, which matches any chunk
//! of that file. Queries with labels are scored: the share of them in the
//! top 5 and 10, and the mean reciprocal rank of the first one.
//!
//! "today" is grouped by type with 5 semantic hits per type, as before this
//! change; "ranked" is `sclerox search` now, with your `search.semantic_limit`.

use std::collections::HashSet;

use serde::Deserialize;

use crate::search::{rank, search_hits, Hit, SearchResult};

/// Semantic hits per type before ranking across types.
const OLD_SEMANTIC_LIMIT: usize = 5;
/// Hits printed per query and ranking.
const SHOWN: usize = 10;

#[derive(Deserialize)]
struct Eval {
    query: Vec<Query>,
}

#[derive(Deserialize)]
struct Query {
    q: String,
    #[serde(default)]
    relevant: Vec<String>,
}

fn label(r: &SearchResult) -> String {
    match r {
        SearchResult::Memory { key, .. } => format!("memory:{key}"),
        SearchResult::Person { id, .. } => format!("person:{id}"),
        SearchResult::Meeting { id, .. } => format!("meeting:{id}"),
        SearchResult::Project { id, .. } => format!("project:{id}"),
        SearchResult::Todo { id, .. } => format!("todo:{id}"),
        SearchResult::Investigation { slug, .. } => format!("research:{slug}"),
        SearchResult::Repo { name, .. } => format!("repo:{name}"),
        SearchResult::Symbol { name, .. } => format!("symbol:{name}"),
        SearchResult::CodeChunk { file_path, .. } => format!("code:{file_path}"),
    }
}

fn title(r: &SearchResult) -> &str {
    match r {
        SearchResult::Memory { snippet, .. } => snippet,
        SearchResult::Person { name, .. } => name,
        SearchResult::Meeting { title, .. } => title,
        SearchResult::Project { name, .. } => name,
        SearchResult::Todo { title, .. } => title,
        SearchResult::Investigation { name, .. } => name,
        SearchResult::Repo { name, .. } => name,
        SearchResult::Symbol {
            signature, name, ..
        } => signature.as_deref().unwrap_or(name),
        SearchResult::CodeChunk { snippet, .. } => snippet,
    }
}

/// Recall at 5 and 10, and the reciprocal rank of the first relevant hit.
#[derive(Default)]
struct Metrics {
    queries: usize,
    r5: f64,
    r10: f64,
    mrr: f64,
}

impl Metrics {
    fn add(&mut self, hits: &[Hit], relevant: &HashSet<&str>) {
        let labels: Vec<String> = hits.iter().map(|h| label(&h.result)).collect();
        let found = |n: usize| {
            let top: HashSet<&str> = labels.iter().take(n).map(String::as_str).collect();
            relevant.iter().filter(|r| top.contains(*r)).count() as f64 / relevant.len() as f64
        };
        self.queries += 1;
        self.r5 += found(5);
        self.r10 += found(10);
        if let Some(i) = labels.iter().position(|l| relevant.contains(l.as_str())) {
            self.mrr += 1.0 / (i + 1) as f64;
        }
    }

    fn print(&self, name: &str) {
        let n = self.queries as f64;
        println!(
            "{name:<8} R@5 {:.3}  R@10 {:.3}  MRR {:.3}",
            self.r5 / n,
            self.r10 / n,
            self.mrr / n
        );
    }
}

fn print_hits(name: &str, hits: &[Hit], relevant: &HashSet<&str>) {
    println!("  {name} ({} hits)", hits.len());
    for (i, h) in hits.iter().take(SHOWN).enumerate() {
        let label = label(&h.result);
        let mark = if relevant.contains(label.as_str()) {
            '*'
        } else {
            ' '
        };
        let score = h.score.map_or("  -  ".to_string(), |s| format!("{s:.3}"));
        let title: String = title(&h.result).chars().take(70).collect();
        println!("   {mark}{:>2}. {score}  {label}  {title}", i + 1);
    }
}

#[test]
#[ignore = "runs against your real knowledge base; see the module docs"]
fn search_live_eval() {
    let Ok(path) = std::env::var("SCLEROX_SEARCH_EVAL") else {
        panic!("set SCLEROX_SEARCH_EVAL to your queries file; see src/cli/search_live_eval.rs");
    };
    let path = match path.strip_prefix("~/") {
        Some(rest) => crate::xdg::home_dir().expect("home dir").join(rest),
        None => path.into(),
    };
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let eval: Eval = toml::from_str(&text).expect("parse the queries file");

    // As `main` does: bridge the embedding settings to the library.
    crate::config::init();
    let s = crate::config::settings();
    let db = crate::db::Database::open(&s.db_path).expect("open the database");
    let threshold = s.semantic_threshold() as f32;
    let margin = s.semantic_margin().map(|m| m as f32);
    println!(
        "database {}, semantic_limit {}",
        s.db_path.display(),
        s.search.semantic_limit
    );

    let (mut today_m, mut ranked_m) = (Metrics::default(), Metrics::default());
    for q in &eval.query {
        let today = search_hits(&db, &q.q, threshold, margin, OLD_SEMANTIC_LIMIT).unwrap();
        let ranked =
            rank(search_hits(&db, &q.q, threshold, margin, s.search.semantic_limit).unwrap());
        let relevant: HashSet<&str> = q.relevant.iter().map(String::as_str).collect();

        println!("\n{}", q.q);
        print_hits("today", &today, &relevant);
        print_hits("ranked", &ranked, &relevant);
        if !relevant.is_empty() {
            today_m.add(&today, &relevant);
            ranked_m.add(&ranked, &relevant);
        }
    }

    if today_m.queries > 0 {
        println!("\n{} queries with labels", today_m.queries);
        today_m.print("today");
        ranked_m.print("ranked");
    }
}
