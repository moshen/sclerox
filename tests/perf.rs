/// Performance benchmarks: measure FTS search and embedding similarity search
/// at realistic data scales. Run with:
///   cargo test --test perf -- --nocapture
///
/// Similarity search requires the embedding model. It is skipped automatically
/// if the model cache is absent (e.g. in CI without model download).
use assert_cmd::Command;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    db: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("ol.db");
        Self { _dir: dir, db }
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::cargo_bin("ol")
            .unwrap()
            .env("OL_DB", &self.db)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ol {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn time(&self, args: &[&str]) -> Duration {
        let start = Instant::now();
        self.run(args);
        start.elapsed()
    }
}

// ─── FTS search performance ───────────────────────────────────────────────────

/// Populate the DB with N records of various types, then measure how long
/// a global search query takes.
#[test]
fn fts_search_100_records() {
    // Budget includes fastembed model load for semantic tier (~300ms debug, ~1s release).
    fts_search_at_scale(100, Duration::from_millis(2000));
}

#[test]
fn fts_search_1000_records() {
    fts_search_at_scale(1000, Duration::from_millis(3000));
}

fn fts_search_at_scale(n: usize, budget: Duration) {
    let e = Env::new();

    // Populate with a mix of todos, memory entries, and meetings
    for i in 0..n {
        match i % 3 {
            0 => {
                e.run(&[
                    "todo",
                    "add",
                    "--title",
                    &format!("Task {i}: fix the authentication handler"),
                    "--category",
                    "github",
                ]);
            }
            1 => {
                e.run(&[
                    "memory",
                    "set",
                    &format!("key-{i}"),
                    &format!("prefer async auth handlers in service {i}"),
                    "--type",
                    "project",
                ]);
            }
            _ => {
                e.run(&[
                    "meeting",
                    "add",
                    "--title",
                    &format!("Sync {i}"),
                    "--date",
                    "2026-06-09",
                    "--notes",
                    &format!("discussed authentication approach for service {i}"),
                ]);
            }
        }
    }

    // Warm up (first query opens the DB WAL)
    e.run(&["search", "auth"]);

    // Measure 3 runs and take the median
    let mut times: Vec<Duration> = (0..3)
        .map(|_| e.time(&["search", "authentication"]))
        .collect();
    times.sort();
    let median = times[1];

    println!(
        "\n[perf] FTS search across {n} records: {:?} (budget: {:?})",
        median, budget
    );
    assert!(
        median < budget,
        "FTS search over {n} records took {:?}, expected < {:?}",
        median,
        budget,
    );
}

// ─── Per-table FTS performance ────────────────────────────────────────────────

#[test]
fn per_table_search_500_todos() {
    let e = Env::new();
    for i in 0..500 {
        e.run(&[
            "todo",
            "add",
            "--title",
            &format!("Review PR #{i} for the authentication service"),
        ]);
    }
    e.run(&["todo", "search", "authentication"]); // warm up

    let t = e.time(&["todo", "search", "authentication"]);
    println!("\n[perf] todo search (500 records): {:?}", t);
    assert!(t < Duration::from_millis(200), "took {:?}", t);
}

#[test]
fn per_table_search_500_research() {
    let e = Env::new();
    for i in 0..500 {
        e.run(&[
            "research",
            "start",
            "--name",
            &format!("Investigation {i}: auth latency spike"),
            "--slug",
            &format!("auth-spike-{i}"),
        ]);
    }
    e.run(&["research", "search", "latency"]); // warm up

    let t = e.time(&["research", "search", "latency"]);
    println!("\n[perf] research search (500 records): {:?}", t);
    assert!(t < Duration::from_millis(200), "took {:?}", t);
}

// ─── Similarity search performance ───────────────────────────────────────────

/// Test similarity search at scale using mock (pre-computed) embeddings
/// injected directly via the DB. This avoids needing the fastembed model.
#[test]
fn similarity_search_1000_chunks_unit_level() {
    use ol::search::similarity::cosine_similarity;

    // Simulate what happens inside `db.meeting_similar()` with 1000 chunks
    let dims = 384usize;
    let n_chunks = 1000usize;

    // Build fake embeddings: random-ish using deterministic values
    let chunks: Vec<Vec<f32>> = (0..n_chunks)
        .map(|i| {
            (0..dims)
                .map(|j| ((i * dims + j) as f32 * 0.001).sin())
                .collect()
        })
        .collect();

    let query: Vec<f32> = (0..dims).map(|j| ((j as f32) * 0.002).cos()).collect();

    let start = Instant::now();
    let mut scored: Vec<(f32, usize)> = chunks
        .iter()
        .enumerate()
        .map(|(i, emb)| (cosine_similarity(&query, emb), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let elapsed = start.elapsed();

    println!(
        "\n[perf] cosine similarity scan ({n_chunks} x {dims}d): {:?}",
        elapsed
    );
    assert_eq!(scored.len(), n_chunks);

    // Release: ~1ms. Debug: ~20ms (no LLVM vectorisation).
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(200)
    } else {
        Duration::from_millis(10)
    };
    assert!(
        elapsed < budget,
        "cosine scan of {n_chunks} chunks took {:?} (budget {budget:?})",
        elapsed
    );
}

#[test]
fn similarity_search_10k_chunks_unit_level() {
    use ol::search::similarity::cosine_similarity;

    let dims = 384usize;
    let n_chunks = 10_000usize;

    let chunks: Vec<Vec<f32>> = (0..n_chunks)
        .map(|i| {
            (0..dims)
                .map(|j| ((i * dims + j) as f32 * 0.0001).sin())
                .collect()
        })
        .collect();

    let query: Vec<f32> = (0..dims).map(|j| ((j as f32) * 0.0002).cos()).collect();

    let start = Instant::now();
    let mut scored: Vec<(f32, usize)> = chunks
        .iter()
        .enumerate()
        .map(|(i, emb)| (cosine_similarity(&query, emb), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let elapsed = start.elapsed();

    println!(
        "\n[perf] cosine similarity scan ({n_chunks} x {dims}d): {:?}",
        elapsed
    );

    // Release: ~10ms. Debug: ~150ms.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(2000)
    } else {
        Duration::from_millis(100)
    };
    assert!(
        elapsed < budget,
        "cosine scan of {n_chunks} chunks took {:?} (budget {budget:?})",
        elapsed
    );
}
