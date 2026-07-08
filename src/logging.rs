use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use log::LevelFilter;

/// File-based logger. Each day gets its own file: ~/.ol/logs/ol-YYYY-MM-DD.log.
/// Multiple `ol` processes running concurrently safely append to the same file.
pub struct OlLogger {
    level: LevelFilter,
    file: Mutex<std::fs::File>,
    pid: u32,
}

impl OlLogger {
    pub fn new(level: LevelFilter, log_dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(log_dir)?;
        let date = chrono::Local::now().format("%Y-%m-%d");
        let path = log_dir.join(format!("ol-{date}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            level,
            file: Mutex::new(file),
            pid: std::process::id(),
        })
    }
}

impl log::Log for OlLogger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= self.level
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
/// in the `OL_LOG` env var) → default off.
pub fn init(level: Option<LevelFilter>) {
    let level = level
        .or_else(|| crate::config::settings().log.level.parse().ok())
        .unwrap_or(LevelFilter::Off);

    if level == LevelFilter::Off {
        return;
    }

    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ol")
        .join("logs");

    match OlLogger::new(level, &log_dir) {
        Ok(logger) => {
            if log::set_boxed_logger(Box::new(logger)).is_ok() {
                log::set_max_level(level);
            }
        }
        Err(e) => {
            eprintln!("[ol] could not open log file: {e}");
        }
    }
}
