//! A small made-up knowledge base and queries about it
//! (`synthetic.toml`), scored on every `cargo test` against the committed
//! `synthetic-baseline.toml`. A change that moves the numbers rewrites the
//! baseline and commits it, so the diff shows the effect:
//!
//! ```text
//! SCLEROX_EVAL_BLESS=1 cargo test --bin sclerox synthetic_search_eval -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::{baseline, evaluate, parse_queries, print_query, rankers, report, Params};
use crate::db::todos::TodoStatus;
use crate::db::Database;

const CORPUS: &str = include_str!("synthetic.toml");
const BASELINE: &str = include_str!("synthetic-baseline.toml");
const BASELINE_PATH: &str = "src/cli/search_eval/synthetic-baseline.toml";
/// How far a metric over all queries may move from the baseline before the
/// test fails. Scores differ slightly between CPUs (ONNX Runtime picks its
/// SIMD code by CPU), enough to swap near-tied hits: on CI runners that
/// moved these by up to 0.043.
const TOLERANCE: f64 = 0.05;

#[derive(Deserialize)]
struct Corpus {
    memory: Vec<Memory>,
    person: Vec<Person>,
    meeting: Vec<Meeting>,
    project: Vec<Project>,
    todo: Vec<Todo>,
    research: Vec<Research>,
    file: Vec<File>,
}

#[derive(Deserialize)]
struct Memory {
    key: String,
    #[serde(rename = "type")]
    memory_type: String,
    value: String,
}

#[derive(Deserialize)]
struct Person {
    name: String,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct Meeting {
    title: String,
    date: Option<String>,
    transcript: Option<String>,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct Project {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Todo {
    title: String,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct Research {
    slug: String,
    name: String,
    plan: Option<String>,
    findings: Option<String>,
}

#[derive(Deserialize)]
struct File {
    path: String,
    content: String,
}

/// Write the corpus into `db`, index its files as a repo under `dir`, and
/// embed everything.
fn build(db: &Database, dir: &Path, embedder: &mut crate::embed::Embedder) {
    let c: Corpus = toml::from_str(CORPUS).expect("parse synthetic.toml");
    for m in &c.memory {
        db.memory_set(&m.key, &m.value, &m.memory_type, None)
            .unwrap();
    }
    for p in &c.person {
        db.people_add(&p.name, p.notes.as_deref()).unwrap();
    }
    for m in &c.meeting {
        db.meeting_add(
            &m.title,
            m.date.as_deref(),
            m.transcript.as_deref(),
            m.notes.as_deref(),
        )
        .unwrap();
    }
    for p in &c.project {
        db.project_add(&p.name, p.description.as_deref(), &[])
            .unwrap();
    }
    for t in &c.todo {
        db.todo_add(
            &t.title,
            t.notes.as_deref(),
            TodoStatus::Open,
            None,
            "general",
            None,
            None,
        )
        .unwrap();
    }
    for r in &c.research {
        let id = db
            .investigation_start(&r.name, &r.slug, r.plan.as_deref())
            .unwrap();
        db.investigation_update(id, None, None, r.findings.as_deref(), None)
            .unwrap();
    }

    let repo = dir.join("synthetic");
    for f in &c.file {
        let path = repo.join(&f.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, f.content.trim_start()).unwrap();
    }
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    crate::index::RepoIndexer::new(Some(embedder))
        .index_repo(db, &repo, None)
        .expect("index the synthetic repo");
    // Cuts meetings, todos and research into chunks and embeds every row.
    crate::cli::db::reembed(db, true).expect("embed the synthetic corpus");
}

/// Metrics that moved more than `tolerance`, and keys only one side has.
fn compare(
    expected: &BTreeMap<String, BTreeMap<String, f64>>,
    actual: &BTreeMap<String, BTreeMap<String, f64>>,
    tolerance: f64,
) -> Vec<String> {
    let mut out = Vec::new();
    for (group, metrics) in actual {
        let Some(want) = expected.get(group) else {
            out.push(format!("{group}: not in the baseline"));
            continue;
        };
        for (name, v) in metrics {
            match want.get(name) {
                Some(w) if (v - w).abs() > tolerance => {
                    out.push(format!("{group}.{name}: {w:.3} -> {v:.3}"));
                }
                Some(_) => {}
                None => out.push(format!("{group}.{name}: not in the baseline")),
            }
        }
        for name in want.keys().filter(|k| !metrics.contains_key(*k)) {
            out.push(format!("{group}.{name}: no longer measured"));
        }
    }
    for group in expected.keys().filter(|k| !actual.contains_key(*k)) {
        out.push(format!("{group}: no longer measured"));
    }
    out
}

#[test]
fn synthetic_search_eval() {
    // Without the model (offline on a cold cache, say) there is nothing to
    // measure. That skips the eval locally, but fails it in CI, which keeps
    // the model cached, so a broken model load can't pass unnoticed.
    let Some(mut embedder) = crate::embed::optional(true) else {
        assert!(
            std::env::var_os("CI").is_none(),
            "the synthetic search eval needs the embedding model"
        );
        eprintln!("skipping synthetic_search_eval: the embedding model is unavailable");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("sclerox.db")).unwrap();
    build(&db, dir.path(), &mut embedder);

    let queries = parse_queries(CORPUS).unwrap();
    let settings = crate::config::Settings::from_toml_str("").unwrap();
    let ev = evaluate(
        &db,
        &queries,
        &rankers(Params::from_settings(&settings)),
        &mut |q| embedder.embed_query(q).ok(),
        &crate::cli::search::render,
    )
    .unwrap();
    for (i, q) in queries.iter().enumerate() {
        print_query(q, &ev, i);
    }
    println!("{}", report(&queries, &ev));

    let actual = baseline(&queries, &ev);
    if std::env::var_os("SCLEROX_EVAL_BLESS").is_some() {
        let text = format!(
            "# Written by synthetic_search_eval with SCLEROX_EVAL_BLESS=1; see synthetic.rs.\n\n{}",
            toml::to_string(&actual).unwrap()
        );
        std::fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE_PATH),
            text,
        )
        .unwrap();
        return;
    }
    let expected: BTreeMap<String, BTreeMap<String, f64>> =
        toml::from_str(BASELINE).expect("parse synthetic-baseline.toml");
    // Over all queries, a change shows past the noise between CPUs. A kind
    // has only a handful of queries, where one swapped near-tie moves a
    // metric by 0.1 or more, so those are printed but not checked.
    let (all, kinds): (BTreeMap<_, _>, BTreeMap<_, _>) = actual
        .into_iter()
        .partition(|(group, _)| group.ends_with(".all"));
    let (expected_all, expected_kinds): (BTreeMap<_, _>, BTreeMap<_, _>) = expected
        .into_iter()
        .partition(|(group, _)| group.ends_with(".all"));
    let shifted = compare(&expected_kinds, &kinds, 0.0);
    if !shifted.is_empty() {
        println!("per kind, not checked:\n  {}", shifted.join("\n  "));
    }
    let moved = compare(&expected_all, &all, TOLERANCE);
    assert!(
        moved.is_empty(),
        "search results moved from {BASELINE_PATH}:\n  {}\n\
         If the change is intended, rewrite the baseline with SCLEROX_EVAL_BLESS=1 \
         (see src/cli/search_eval/synthetic.rs) and commit it.",
        moved.join("\n  ")
    );
}
