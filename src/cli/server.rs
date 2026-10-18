use anyhow::Result;
use clap::Subcommand;
use std::time::Duration;

use crate::config::settings;
use crate::embed::server::{self, protocol::Build, protocol::ServerStatus, runs};
use crate::embed::{models, ModelSpec};
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Start the model server in the background.
    ///
    /// The server keeps embedding models loaded so commands don't each load
    /// one. Commands start it themselves when `embed.server = auto`, so this
    /// is rarely needed. It exits after `server.idle_minutes` unused.
    Start {
        /// Run in this terminal instead of the background, until idle or stopped.
        #[arg(long)]
        foreground: bool,
        /// Model to load now (default: embed.model). Others load when first
        /// asked for.
        #[arg(long)]
        model: Option<String>,
    },

    /// Stop the model server, after any embedding in progress.
    Stop,

    /// Show the running model server, its models, and recent runs.
    Status,
}

pub fn run(cmd: ServerCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        ServerCommand::Start { foreground, model } => {
            let spec = match model {
                Some(name) => models::lookup(&name)?,
                None => models::configured()?,
            };
            if !crate::embed::enabled() {
                anyhow::bail!("embeddings are turned off (embed.enabled = false)");
            }
            if foreground {
                return start_foreground(spec);
            }
            let status = server::start_detached(spec)?;
            print_output(format, &status, || print_status(&status));
            if server::mode() == server::Mode::Off {
                eprintln!("note: embed.server = off, so commands won't use it");
            }
            Ok(())
        }
        ServerCommand::Stop => {
            let stopped = server::stop()?;
            print_output(format, &serde_json::json!({ "stopped": stopped }), || {
                if stopped {
                    println!("stopped the model server");
                } else {
                    println!("no model server running");
                }
            });
            Ok(())
        }
        ServerCommand::Status => {
            let running = match server::status() {
                Ok(status) => status,
                // There, but stuck.
                Err(e) => {
                    eprintln!("warning: model server: {e:#}");
                    None
                }
            };
            let recent = recent_runs()?;
            let value = serde_json::json!({ "server": running, "recent_runs": recent });
            print_output(format, &value, || {
                match &running {
                    Some(status) => print_status(status),
                    None => println!("no model server running"),
                }
                print_runs(&recent, running.as_ref().map(|s| s.pid));
            });
            Ok(())
        }
    }
}

fn start_foreground(spec: &'static ModelSpec) -> Result<()> {
    let s = &settings().server;
    let limit = |mb: u64| Some(mb).filter(|&mb| mb != 0);
    let options = server::ServeOptions {
        idle: Duration::from_secs(s.idle_minutes * 60),
        preload: spec,
        soft_limit_mb: limit(s.max_memory_mb),
        hard_limit_mb: limit(s.hard_memory_mb),
        distillations: super::hook::distillations_in_progress,
    };
    let served = server::serve(options, |status| {
        eprintln!(
            "model server ready: pid {}, {} MB with {} loaded",
            status.pid, status.rss_mb, spec.name
        );
    })?;
    match served {
        server::Served::AlreadyRunning => eprintln!("a model server is already running"),
        server::Served::Exited(reason) => eprintln!("model server exited: {}", reason.as_str()),
    }
    Ok(())
}

fn print_status(s: &ServerStatus) {
    println!("model server: running, pid {}", s.pid);
    let this = Build::current();
    let note = if s.build == this {
        String::new()
    } else {
        format!(" - not this build, {}", this.label())
    };
    println!("  build     {}{note}", s.build.label());
    let limits = match (s.soft_limit_mb, s.hard_limit_mb) {
        (None, None) => "no limit".to_string(),
        (Some(soft), None) => format!("limit {soft} MB"),
        (None, Some(hard)) => format!("hard limit {hard} MB"),
        (Some(soft), Some(hard)) => format!("limit {soft} MB, hard {hard} MB"),
    };
    let due = if s.restart_due {
        "; over it, exits once unused"
    } else {
        ""
    };
    println!(
        "  memory    {} MB now, {} MB peak ({limits}{due})",
        s.rss_mb, s.peak_mb
    );
    println!(
        "  requests  {} (last {}; exits after {} idle)",
        s.requests,
        ago(s.idle_secs),
        minutes(s.idle_limit_secs)
    );
    println!("  started   {}", s.started_at);
    if s.waiting_queries + s.waiting_bulk > 0 {
        println!(
            "  waiting   {} queries, {} bulk",
            s.waiting_queries, s.waiting_bulk
        );
    }
    println!("  models");
    for m in &s.models {
        match &m.error {
            Some(error) => println!("    {:<16}  failed to load: {error}", m.name),
            None => println!(
                "    {:<16}  {}, {} MB, {} requests (last {})",
                m.name,
                m.device,
                m.loaded_mb,
                m.requests,
                ago(m.idle_secs)
            ),
        }
    }
    if !s.queue.is_empty() {
        println!("  queue");
    }
    for q in &s.queue {
        if !std::path::Path::new(&q.path).exists() {
            continue;
        }
        let jobs = q
            .kind
            .open(std::path::Path::new(&q.path))
            .and_then(|conn| crate::db::jobs::unfinished(&conn));
        match jobs {
            Ok(jobs) if jobs.is_empty() => println!("    {}  finishing", q.path),
            Ok(jobs) => {
                for job in jobs {
                    println!("    {}  {}", q.path, super::queue::describe(&job));
                }
            }
            Err(e) => println!("    {}  couldn't read its jobs: {e:#}", q.path),
        }
    }
}

fn recent_runs() -> Result<Vec<runs::Run>> {
    let path = server::runs_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    runs::Runs::open(&path)?.recent(10)
}

/// `running` is the pid of the server that answered; an unfinished run
/// that isn't it ended without recording why.
fn print_runs(recent: &[runs::Run], running: Option<u32>) {
    if recent.is_empty() {
        return;
    }
    println!("\nrecent runs (UTC):");
    println!(
        "  {:<19}  {:<19}  {:>7}  {:<8}  {:>8}  {:>7}  models",
        "started", "ended", "pid", "reason", "requests", "peak MB"
    );
    for r in recent {
        println!(
            "  {:<19}  {:<19}  {:>7}  {:<8}  {:>8}  {:>7}  {}",
            r.started_at,
            r.ended_at.as_deref().unwrap_or("-"),
            r.pid,
            r.exit_reason
                .as_deref()
                .unwrap_or(if running == Some(r.pid) {
                    "running"
                } else {
                    "unknown"
                }),
            r.requests,
            r.peak_mb.map_or_else(|| "-".to_string(), |n| n.to_string()),
            if r.models.is_empty() {
                "-".to_string()
            } else {
                r.models.join(", ")
            },
        );
        if let Some(error) = &r.error {
            println!("    {error}");
        }
    }
}

fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

fn minutes(secs: u64) -> String {
    format!("{} min", secs / 60)
}
