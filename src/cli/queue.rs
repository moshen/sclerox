//! Bulk embedding handed to the model server: queue the jobs in each
//! database, tell the server, and, with `--wait`, follow them to the end.
//!
//! Only with `embed.server = auto` and a server that runs; otherwise the
//! commands embed in-process as before.

use anyhow::Result;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::db::jobs::{self, DbKind, Job, Kind};
use crate::embed::{server, ModelSpec};

/// One database's jobs, handed to the server.
pub struct Queued {
    /// How the commands name it: "main database", or the repo's name.
    pub label: String,
    pub path: PathBuf,
    pub kind: DbKind,
    pub jobs: Vec<i64>,
}

/// The model server, when bulk work should go to it: `embed.server = auto`
/// and a server is running or starts (which loads `spec`).
pub fn server_takes_work(spec: &'static ModelSpec) -> bool {
    server::mode() == server::Mode::Auto && server::connect_or_start(spec).is_some()
}

/// Queue `todo` (each a kind and, for a rebuild, the one table it's limited
/// to) in the database open on `conn`, and hand it to the server. If the
/// server can't take it after all, run it here before returning.
pub fn submit(
    conn: &rusqlite::Connection,
    spec: &'static ModelSpec,
    label: &str,
    kind: DbKind,
    todo: &[(Kind, Option<&str>)],
) -> Result<Queued> {
    let path = PathBuf::from(conn.path().unwrap_or_default());
    let mut ids = Vec::new();
    for (k, table) in todo {
        ids.push(jobs::queue(conn, *k, spec.id, *table)?);
    }
    let what: Vec<&str> = todo
        .iter()
        .map(|(k, _)| match k {
            Kind::Rebuild => "re-embed",
            Kind::Fill => "embed what has no vector",
        })
        .collect();
    if server::enqueue(spec, &path, kind) {
        println!("  {label}: queued ({})", what.join(", then "));
    } else {
        println!("  {label}: the model server is unavailable; embedding here");
        run_here(conn, kind)?;
    }
    Ok(Queued {
        label: label.to_string(),
        path,
        kind,
        jobs: ids,
    })
}

/// Run a database's queued jobs in this process, unless another process
/// (the server, still at it after all) already is.
fn run_here(conn: &rusqlite::Connection, kind: DbKind) -> Result<()> {
    let mut loaded: Option<crate::embed::Embedder> = None;
    let mut embed = |model_id: &str, texts: &[&str]| -> Result<Vec<Vec<f32>>> {
        if loaded.as_ref().is_none_or(|e| e.model().id != model_id) {
            let spec = crate::embed::models::by_id(model_id)
                .ok_or_else(|| anyhow::anyhow!("unknown embedding model '{model_id}'"))?;
            loaded = Some(crate::embed::Embedder::with_model(
                spec,
                crate::embed::server::Lane::Bulk,
            )?);
        }
        loaded.as_mut().expect("loaded above").embed_each(texts)
    };
    // As `db reembed` does without the server.
    let mut before_rebuild = || -> Result<()> {
        if kind == DbKind::Main && !super::db::wait_for_distillations() {
            anyhow::bail!(
                "a session distillation is still running and could write vectors from the \
                 old model after the switch; run `sclerox db reembed` once it finishes"
            );
        }
        Ok(())
    };
    match jobs::run(conn, kind, &mut embed, &mut before_rebuild) {
        Err(e) if jobs::is_interrupted(&e) => {
            log::info!("not embedding here: {e:#}");
            Ok(())
        }
        other => other,
    }
}

/// Say what was queued and how to follow it.
pub fn report(queued: &[Queued]) {
    if queued.is_empty() {
        return;
    }
    println!(
        "\nQueued for the model server, which embeds in the background: {}.",
        queued
            .iter()
            .map(|q| q.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Until then, keyword search still finds what has no vector yet. Follow it with \
         `sclerox server status`, or run with --wait."
    );
}

/// How often `--wait` looks at the jobs, and at the server.
const POLL: Duration = Duration::from_millis(500);
const CHECK_SERVER: Duration = Duration::from_secs(2);

/// Follow `queued` until every job has finished, restarting the server if it
/// exits first (over its memory limit, say) and running what's left here if
/// it can't start. Fails when any job failed.
pub fn wait(spec: &'static ModelSpec, queued: &[Queued]) -> Result<()> {
    use std::io::{IsTerminal, Write};

    let interactive = std::io::stdout().is_terminal();
    let mut conns = Vec::new();
    for q in queued {
        conns.push(q.kind.open(&q.path)?);
    }
    let mut last_check = Instant::now();
    let mut shown = String::new();
    loop {
        let mut lines = Vec::new();
        let mut pending = Vec::new();
        let mut failed = Vec::new();
        for (q, conn) in queued.iter().zip(&conns) {
            for id in &q.jobs {
                let Some(job) = jobs::get(conn, *id)? else {
                    continue;
                };
                if !job.finished() {
                    lines.push(format!("{}: {}", q.label, describe(&job)));
                    if !pending.contains(&q.label) {
                        pending.push(q.label.clone());
                    }
                } else if job.state == jobs::FAILED || job.state == jobs::CANCELLED {
                    failed.push(format!(
                        "{}: {} {} ({})",
                        q.label,
                        job.kind,
                        job.state,
                        job.error.as_deref().unwrap_or("no reason recorded")
                    ));
                }
            }
        }
        if pending.is_empty() {
            if interactive && !shown.is_empty() {
                println!();
            }
            for f in &failed {
                println!("  {f}");
            }
            if !failed.is_empty() {
                anyhow::bail!("{} embedding job(s) didn't finish", failed.len());
            }
            println!("Embedding done.");
            return Ok(());
        }
        let line = lines.join("; ");
        if line != shown {
            if interactive {
                print!("\r  {line}\x1b[K");
                let _ = std::io::stdout().flush();
            } else {
                println!("  {line}");
            }
            shown = line;
        }
        if last_check.elapsed() >= CHECK_SERVER {
            last_check = Instant::now();
            // The databases a server is working on, if one is running.
            let working: Option<Vec<PathBuf>> = match server::status() {
                Ok(Some(s)) => Some(s.queue.into_iter().map(|d| d.path.into()).collect()),
                _ => None,
            };
            // Gone before finishing, or no longer on it: hand the work to a
            // server again, or do it here.
            for (q, conn) in queued.iter().zip(&conns) {
                let path = std::fs::canonicalize(&q.path).unwrap_or_else(|_| q.path.clone());
                let owned = working.as_ref().is_some_and(|w| w.contains(&path));
                if pending.contains(&q.label) && !owned && !server::enqueue(spec, &q.path, q.kind) {
                    println!(
                        "\n  {}: the model server is unavailable; embedding here",
                        q.label
                    );
                    run_here(conn, q.kind)?;
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// A job's work and how far it has got.
pub fn describe(job: &Job) -> String {
    let what = match (job.kind.as_str(), &job.table) {
        ("rebuild", Some(t)) => format!("re-embedding {t}"),
        ("rebuild", None) => "re-embedding".to_string(),
        _ => "embedding".to_string(),
    };
    match (job.state.as_str(), job.total) {
        (jobs::RUNNING, Some(total)) => format!("{what} {}/{total}", job.progress),
        (jobs::RUNNING, None) => what,
        _ => format!("{what} (queued)"),
    }
}
