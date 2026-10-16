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
    /// The server keeps the embedding model loaded so commands don't each
    /// load it. Commands start it themselves when `embed.server = auto`, so
    /// this is rarely needed. It exits after `server.idle_minutes` unused.
    Start {
        /// Run in this terminal instead of the background, until idle or stopped.
        #[arg(long)]
        foreground: bool,
        /// Model to serve (default: embed.model).
        #[arg(long)]
        model: Option<String>,
    },

    /// Stop running model servers, after any embedding in progress.
    Stop,

    /// Show running model servers and recent runs.
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
            let mut stopped = 0;
            for spec in models::MODELS {
                if server::stop(spec)? {
                    println!("stopped the model server for {}", spec.name);
                    stopped += 1;
                }
            }
            if stopped == 0 {
                println!("no model server running");
            }
            Ok(())
        }
        ServerCommand::Status => {
            let mut running: Vec<ServerStatus> = Vec::new();
            for spec in models::MODELS {
                match server::status(spec) {
                    Ok(Some(status)) => running.push(status),
                    Ok(None) => {}
                    // There, but stuck or not provably this user's.
                    Err(e) => eprintln!("warning: model server for {}: {e:#}", spec.name),
                }
            }
            let recent = recent_runs()?;
            let value = serde_json::json!({ "running": running, "recent_runs": recent });
            print_output(format, &value, || {
                if running.is_empty() {
                    println!("no model server running");
                }
                for status in &running {
                    print_status(status);
                }
                let pids: Vec<u32> = running.iter().map(|s| s.pid).collect();
                print_runs(&recent, &pids);
            });
            Ok(())
        }
    }
}

fn start_foreground(spec: &'static ModelSpec) -> Result<()> {
    let options = server::ServeOptions {
        idle: Duration::from_secs(settings().server.idle_minutes * 60),
    };
    let served = server::serve(spec, options, |status| {
        eprintln!(
            "model server for {} ready: pid {}, {} MB",
            status.model, status.pid, status.loaded_mb
        );
    })?;
    match served {
        server::Served::AlreadyRunning => {
            eprintln!("a model server for {} is already running", spec.name)
        }
        server::Served::Exited(reason) => {
            eprintln!("model server for {} exited: {}", spec.name, reason.as_str())
        }
    }
    Ok(())
}

fn print_status(s: &ServerStatus) {
    println!("{}: running, pid {}", s.model, s.pid);
    let this = Build::current();
    let note = if s.build == this {
        String::new()
    } else {
        format!(" - not this build, {}", this.label())
    };
    println!("  build     {}{note}", s.build.label());
    println!("  device    {}", s.device);
    println!(
        "  memory    {} MB now, {} MB after loading, {} MB peak",
        s.rss_mb, s.loaded_mb, s.peak_mb
    );
    println!(
        "  requests  {} (last {}; exits after {} idle)",
        s.requests,
        ago(s.idle_secs),
        minutes(s.idle_limit_secs)
    );
    println!("  started   {}", s.started_at);
}

fn recent_runs() -> Result<Vec<runs::Run>> {
    let path = server::runs_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    runs::Runs::open(&path)?.recent(10)
}

/// `running` are the pids of servers that answered; an unfinished run that
/// isn't one of them ended without recording why.
fn print_runs(recent: &[runs::Run], running: &[u32]) {
    if recent.is_empty() {
        return;
    }
    println!("\nrecent runs (UTC):");
    println!(
        "  {:<19}  {:<19}  {:<16}  {:>7}  {:<8}  {:>8}  {:>7}  {:>7}",
        "started", "ended", "model", "pid", "reason", "requests", "load MB", "peak MB"
    );
    let mb = |n: Option<u64>| n.map_or_else(|| "-".to_string(), |n| n.to_string());
    for r in recent {
        println!(
            "  {:<19}  {:<19}  {:<16}  {:>7}  {:<8}  {:>8}  {:>7}  {:>7}",
            r.started_at,
            r.ended_at.as_deref().unwrap_or("-"),
            r.model,
            r.pid,
            r.exit_reason
                .as_deref()
                .unwrap_or(if running.contains(&r.pid) {
                    "running"
                } else {
                    "unknown"
                }),
            r.requests,
            mb(r.loaded_mb),
            mb(r.peak_mb),
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
