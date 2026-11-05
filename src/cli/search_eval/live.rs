//! Evaluate search on your own knowledge base: the database and settings
//! `sclerox` uses, and queries from a local TOML file that stays out of the
//! repo (the format is in [`super`]).
//!
//! ```text
//! SCLEROX_SEARCH_EVAL=~/search-eval.toml \
//!     cargo test --release --bin sclerox search_live_eval -- --ignored --nocapture
//! ```
//!
//! prints every query's top hits per ranker, a table per query kind, and
//! each ranker against the baseline. To label without favouring one ranker,
//! `search_live_pool` prints, per query, the union of every ranker's top
//! `SCLEROX_SEARCH_EVAL_DEPTH` hits (10 by default), with the rankers that
//! returned each one:
//!
//! ```text
//! SCLEROX_SEARCH_EVAL=~/search-eval.toml \
//!     cargo test --release --bin sclerox search_live_pool -- --ignored --nocapture
//! ```

use super::{evaluate, parse_queries, pool, print_query, rankers, report, EvalQuery, Params};
use crate::db::Database;

fn load() -> (Database, Params, Vec<EvalQuery>) {
    let Ok(path) = std::env::var("SCLEROX_SEARCH_EVAL") else {
        panic!("set SCLEROX_SEARCH_EVAL to your queries file; see src/cli/search_eval/");
    };
    let path = match path.strip_prefix("~/") {
        Some(rest) => crate::xdg::home_dir().expect("home dir").join(rest),
        None => path.into(),
    };
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let queries = parse_queries(&text).expect("parse the queries file");

    // As `main` does: bridge the embedding settings to the library.
    crate::config::init();
    let s = crate::config::settings();
    let db = Database::open(&s.db_path).expect("open the database");
    let params = Params::from_settings(s);
    println!("database {}, {params:?}", s.db_path.display());
    (db, params, queries)
}

fn run(db: &Database, params: Params, queries: &[EvalQuery]) -> super::Evaluation {
    let mut embedder = crate::embed::optional(true);
    evaluate(
        db,
        queries,
        &rankers(params),
        &mut |q| embedder.as_mut().and_then(|e| e.embed_query(q).ok()),
        &crate::cli::search::render,
    )
    .expect("run the eval")
}

#[test]
#[ignore = "runs against your real knowledge base; see the module docs"]
fn search_live_eval() {
    let (db, params, queries) = load();
    let ev = run(&db, params, &queries);
    for (i, q) in queries.iter().enumerate() {
        print_query(q, &ev, i);
    }
    println!("{}", report(&queries, &ev));
}

#[test]
#[ignore = "runs against your real knowledge base; see the module docs"]
fn search_live_pool() {
    let (db, params, queries) = load();
    let depth = std::env::var("SCLEROX_SEARCH_EVAL_DEPTH")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(10);
    let ev = run(&db, params, &queries);
    for (i, q) in queries.iter().enumerate() {
        println!("\n{}", q.q);
        for (label, title, from, _) in pool(&ev, i, depth) {
            let mark = if q.relevant.contains(&label) {
                '*'
            } else {
                ' '
            };
            println!("  {mark} {label:<40} {:<24} {title}", from.join(","));
        }
    }
}
