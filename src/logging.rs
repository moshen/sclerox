use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub use log::LevelFilter;

/// File-based logger. Each day gets its own file: ~/.local/state/sclerox/logs/sclerox-YYYY-MM-DD.log.
/// Multiple `sclerox` processes running concurrently safely append to the same file.
pub struct ScleroxLogger {
    level: LevelFilter,
    file: Mutex<std::fs::File>,
    pid: u32,
}

impl ScleroxLogger {
    pub fn new(level: LevelFilter, log_dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(log_dir)?;
        let date = chrono::Local::now().format("%Y-%m-%d");
        let path = log_dir.join(format!("sclerox-{date}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            level,
            file: Mutex::new(file),
            pid: std::process::id(),
        })
    }
}

impl log::Log for ScleroxLogger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        if meta.level() > self.level {
            return false;
        }
        // Dependency crates (globset, ignore, ort, ...) are noisy at debug —
        // they were ~5% of a debug-level day. Keep them to warn and stronger;
        // our own sclerox::* targets log at the configured level.
        let t = meta.target();
        t == "sclerox" || t.starts_with("sclerox::") || meta.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let rss = rss_mb();
        let line = format!(
            "{} {:5} pid={} rss={:>4}MB [{}] {}\n",
            now,
            record.level(),
            self.pid,
            rss,
            record.target(),
            record.args()
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// Resident set size in MB, or 0 if unavailable.
fn rss_mb() -> u64 {
    memory_stats::memory_stats()
        .map(|s| (s.physical_mem / (1024 * 1024)) as u64)
        .unwrap_or(0)
}

/// Initialise the logger. Call once at startup.
/// Level is resolved in order: `--log-level` arg → `[log].level` (which folds
/// in the `SCLEROX_LOG` env var) → default off.
pub fn init(level: Option<LevelFilter>) {
    let level = level
        .or_else(|| crate::config::settings().log.level.parse().ok())
        .unwrap_or(LevelFilter::Off);

    if level == LevelFilter::Off {
        return;
    }

    let log_dir = crate::xdg::state_home().join("sclerox").join("logs");

    prune_old_logs(&log_dir, crate::config::settings().log.retain_days);

    match ScleroxLogger::new(level, &log_dir) {
        Ok(logger) => {
            if log::set_boxed_logger(Box::new(logger)).is_ok() {
                log::set_max_level(level);
            }
        }
        Err(e) => {
            eprintln!("[sclerox] could not open log file: {e}");
        }
    }
}

/// Delete `sclerox-YYYY-MM-DD.log` files older than `retain_days` (0 = keep all).
/// Best-effort: any error is ignored — retention must never break startup.
fn prune_old_logs(log_dir: &Path, retain_days: u32) {
    if retain_days == 0 {
        return;
    }
    let cutoff = chrono::Local::now().date_naive() - chrono::Days::new(u64::from(retain_days));
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(date_part) = name
            .to_str()
            .and_then(|n| n.strip_prefix("sclerox-"))
            .and_then(|n| n.strip_suffix(".log"))
        else {
            continue;
        };
        if let Ok(date) = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
            if date < cutoff {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}
