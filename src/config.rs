//! Configuration: typed settings loaded from `~/.config/sclerox/config.toml`.
//!
//! Precedence (highest first): CLI flag > env var > config file > built-in
//! default. Every key is optional; a missing or malformed file falls back to
//! defaults with a warning rather than failing (the session hooks run this on
//! every session start and must never be bricked by a bad file).
//!
//! `config.rs` is binary-only. Library modules (search, index, db) stay
//! settings-free: `global_search` takes thresholds as parameters, the indexer
//! reads its limit from an env var (bridged from config in `init`), and the db
//! layer takes thresholds as call arguments. Only `cli/*` reads `settings()`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Path to the primary SQLite database. Env: `SCLEROX_DB`.
    pub db_path: PathBuf,
    pub ai: AiSettings,
    pub search: SearchSettings,
    pub dedup: DedupSettings,
    pub memory: MemorySettings,
    pub session_context: SessionContextSettings,
    pub distill: DistillSettings,
    pub embed: EmbedSettings,
    pub index: IndexSettings,
    pub install: InstallSettings,
    pub log: LogSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// Full distillation command (binary + flags), parsed shell-style; the
    /// transcript prompt is appended as the final argument. `None` = use the
    /// built-in default for whichever agent invoked sclerox. Env: `SCLEROX_AI_COMMAND`.
    pub command: Option<String>,
    /// Model for the DEFAULT command only (ignored when `command` is set - bake
    /// the model flag into a custom command yourself). Env: `SCLEROX_AI_MODEL`.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// Cosine floor for a semantic hit to be shown. Stored as f64 for clean
    /// TOML round-tripping; cast to f32 at the (f32) score comparison sites.
    pub semantic_threshold: f64,
    /// Max semantic hits per entity type.
    pub semantic_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DedupSettings {
    /// Cosine score at/above which a distilled memory supersedes an existing one.
    pub cosine_threshold: f64,
    /// Lexical token-overlap fallback threshold (used when no embedder).
    pub lexical_threshold: f64,
    /// Cosine score at/above which a distilled memory merges into the BEST
    /// match even when several existing memories match it.
    ///
    /// Without this, any fact matching 2+ existing memories is inserted and
    /// flagged, so a topic that already has two entries accumulates a third on
    /// every mention. That ratchet is what grows the conflict list.
    pub merge_threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySettings {
    /// Values longer than this warn on write (never rejected).
    pub max_value_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionContextSettings {
    /// Token budget for injected session-start context, enforced with the real
    /// MiniLM tokenizer (`embed::count_tokens`).
    pub max_tokens: usize,
    /// Hard byte backstop on the injected context - a coarse final guard so a
    /// tokenizer hiccup can never emit a runaway payload. Keep it comfortably
    /// above `max_tokens` × ~4 bytes/token.
    pub max_chars: usize,
    /// Full-value memories surfaced at session start.
    pub relevant_memories: usize,
    /// Slots reserved for feedback-type memories.
    pub feedback_reserved: usize,
    pub todos_shown: usize,
    pub research_shown: usize,
    pub sessions_shown: usize,
    pub memory_keys_shown: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillSettings {
    /// Transcript chars per AI call.
    pub chunk_chars: usize,
    /// Sessions shorter than this many turns are not distilled.
    pub min_turns: usize,
    /// Re-distill only after the session grows by this many turns.
    pub min_new_turns: usize,
    /// Existing related memories shown to the distiller so it can reuse a key
    /// and set `supersedes` instead of inventing a near-identical slug.
    pub context_memories: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedSettings {
    /// Entity-text chunk size for embeddings.
    pub chunk_size: usize,
    /// Overlap between adjacent chunks.
    pub chunk_overlap: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexSettings {
    /// Files larger than this skip tree-sitter symbol extraction (still indexed
    /// via line chunks). Env: `SCLEROX_MAX_INDEX_FILE_BYTES`.
    pub max_file_bytes: usize,
    /// Automatic (session-hook) indexing policy: `"git"` indexes the session's
    /// git repo root only; `"off"` disables auto-indexing entirely. Explicit
    /// `sclerox repo index` is unaffected. Unknown values fall back to `"git"`.
    pub auto: String,
    /// Reject indexing a folder with more than this many indexable files (unless
    /// `sclerox repo index --force`). Guards against indexing a giant tree. Env:
    /// `SCLEROX_MAX_INDEX_FILES`.
    pub max_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallSettings {
    /// Refresh `skills/sclerox-kb.md` on re-install. Set false to keep a customized
    /// skill file across upgrades (a fresh install still creates it if missing).
    pub overwrite_skill: bool,
    /// Refresh the SessionStart/Stop hooks and the OpenCode plugin on re-install.
    pub overwrite_hooks: bool,
    /// Refresh the `<!-- sclerox-kb -->` section body in the global instructions file.
    /// Content outside the markers is always preserved regardless.
    pub overwrite_instructions: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogSettings {
    /// Log level: off|error|warn|info|debug|trace. Logs go to
    /// `~/.local/state/sclerox/logs/sclerox-YYYY-MM-DD.log`. Env: `SCLEROX_LOG`; the `--log-level` flag
    /// overrides both.
    pub level: String,
    /// Delete daily log files older than this many days (0 = keep forever).
    pub retain_days: u32,
}

// ── Defaults ────────────────────────────────────────────────────────────────
// These are the single source of truth for built-in values; the previously
// scattered `const`s now live here.

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            ai: AiSettings::default(),
            search: SearchSettings::default(),
            dedup: DedupSettings::default(),
            memory: MemorySettings::default(),
            session_context: SessionContextSettings::default(),
            distill: DistillSettings::default(),
            embed: EmbedSettings::default(),
            index: IndexSettings::default(),
            install: InstallSettings::default(),
            log: LogSettings::default(),
        }
    }
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            semantic_threshold: 0.45_f64,
            semantic_limit: 5,
        }
    }
}

impl Default for DedupSettings {
    fn default() -> Self {
        Self {
            cosine_threshold: 0.85_f64,
            lexical_threshold: 0.7_f64,
            merge_threshold: 0.95_f64,
        }
    }
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            max_value_chars: 800,
        }
    }
}

impl Default for SessionContextSettings {
    fn default() -> Self {
        Self {
            max_tokens: 750,
            max_chars: 3000,
            relevant_memories: 5,
            feedback_reserved: 1,
            todos_shown: 5,
            research_shown: 3,
            sessions_shown: 3,
            memory_keys_shown: 30,
        }
    }
}

impl Default for DistillSettings {
    fn default() -> Self {
        Self {
            chunk_chars: 20_000,
            min_turns: 5,
            min_new_turns: 50,
            context_memories: 12,
        }
    }
}

impl Default for EmbedSettings {
    fn default() -> Self {
        Self {
            chunk_size: 800,
            chunk_overlap: 200,
        }
    }
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            max_file_bytes: 1_000_000,
            auto: "git".to_string(),
            max_files: 50_000,
        }
    }
}

impl Default for InstallSettings {
    fn default() -> Self {
        // Default true preserves the historical always-refresh behavior; users
        // opt out per-artifact to protect their customizations on upgrade.
        Self {
            overwrite_skill: true,
            overwrite_hooks: true,
            overwrite_instructions: true,
        }
    }
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "off".to_string(),
            retain_days: 30,
        }
    }
}

fn default_db_path() -> PathBuf {
    crate::xdg::data_home().join("sclerox").join("sclerox.db")
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// Resolved config file path: `$SCLEROX_CONFIG` if set, else
/// `$XDG_CONFIG_HOME/sclerox/config.toml` (`~/.config/sclerox/config.toml` by default,
/// on every platform including Windows).
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SCLEROX_CONFIG") {
        return PathBuf::from(p);
    }
    crate::xdg::config_home()
        .join("sclerox")
        .join("config.toml")
}

impl Settings {
    /// Parse settings from TOML text. Errors are surfaced so `load` can warn.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load effective settings: file (or defaults) + env overrides + validation.
    pub fn load() -> Self {
        let path = config_path();
        let mut settings = match std::fs::read_to_string(&path) {
            Ok(contents) => Self::from_toml_str(&contents).unwrap_or_else(|e| {
                eprintln!(
                    "warning: could not parse {}: {e}\n         using built-in defaults",
                    path.display()
                );
                Self::default()
            }),
            // Missing file is normal, not an error.
            Err(_) => Self::default(),
        };
        settings.expand_db_path();
        settings.apply_env_overrides();
        settings.validate();
        settings
    }

    /// Expand a leading `~/` in db_path to the home directory.
    fn expand_db_path(&mut self) {
        if let Ok(rest) = self.db_path.strip_prefix("~") {
            if let Some(home) = crate::xdg::home_dir() {
                self.db_path = home.join(rest);
            }
        }
    }

    /// Env vars beat the file (but not CLI flags, which win at their call sites).
    fn apply_env_overrides(&mut self) {
        if let Ok(p) = std::env::var("SCLEROX_DB") {
            if !p.is_empty() {
                self.db_path = PathBuf::from(p);
            }
        }
        if let Ok(c) = std::env::var("SCLEROX_AI_COMMAND") {
            if !c.is_empty() {
                self.ai.command = Some(c);
            }
        }
        if let Ok(m) = std::env::var("SCLEROX_AI_MODEL") {
            if !m.is_empty() {
                self.ai.model = Some(m);
            }
        }
        if let Ok(v) = std::env::var("SCLEROX_MAX_INDEX_FILE_BYTES") {
            if let Ok(n) = v.parse::<usize>() {
                self.index.max_file_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("SCLEROX_MAX_INDEX_FILES") {
            if let Ok(n) = v.parse::<usize>() {
                self.index.max_files = n;
            }
        }
        if let Ok(l) = std::env::var("SCLEROX_LOG") {
            if !l.is_empty() {
                self.log.level = l;
            }
        }
    }

    /// Clamp/repair out-of-range values, warning once per bad key. Never aborts.
    fn validate(&mut self) {
        clamp_unit(
            "search.semantic_threshold",
            &mut self.search.semantic_threshold,
        );
        clamp_unit("dedup.cosine_threshold", &mut self.dedup.cosine_threshold);
        clamp_unit("dedup.lexical_threshold", &mut self.dedup.lexical_threshold);
        clamp_unit("dedup.merge_threshold", &mut self.dedup.merge_threshold);

        require_positive("search.semantic_limit", &mut self.search.semantic_limit, 5);
        require_positive(
            "memory.max_value_chars",
            &mut self.memory.max_value_chars,
            800,
        );
        require_positive(
            "session_context.max_tokens",
            &mut self.session_context.max_tokens,
            750,
        );
        require_positive(
            "session_context.max_chars",
            &mut self.session_context.max_chars,
            3000,
        );
        require_positive(
            "session_context.relevant_memories",
            &mut self.session_context.relevant_memories,
            5,
        );
        // feedback_reserved may legitimately be 0 (no reserved slot) - don't force > 0.
        require_positive(
            "session_context.todos_shown",
            &mut self.session_context.todos_shown,
            5,
        );
        require_positive(
            "session_context.research_shown",
            &mut self.session_context.research_shown,
            3,
        );
        require_positive(
            "session_context.sessions_shown",
            &mut self.session_context.sessions_shown,
            3,
        );
        require_positive(
            "session_context.memory_keys_shown",
            &mut self.session_context.memory_keys_shown,
            30,
        );
        require_positive("distill.chunk_chars", &mut self.distill.chunk_chars, 20_000);
        require_positive("distill.min_turns", &mut self.distill.min_turns, 5);
        require_positive("distill.min_new_turns", &mut self.distill.min_new_turns, 50);
        require_positive("embed.chunk_size", &mut self.embed.chunk_size, 800);
        require_positive(
            "index.max_file_bytes",
            &mut self.index.max_file_bytes,
            1_000_000,
        );
        require_positive("index.max_files", &mut self.index.max_files, 50_000);

        // Auto-index policy must be a recognised value; otherwise warn and use "git".
        if !matches!(self.index.auto.as_str(), "git" | "off") {
            eprintln!(
                "warning: index.auto = \"{}\" is not valid (git|off); using git",
                self.index.auto
            );
            self.index.auto = "git".to_string();
        }

        // Normalise empty strings to None so consumers can treat them uniformly.
        if self.ai.command.as_deref() == Some("") {
            self.ai.command = None;
        }
        if self.ai.model.as_deref() == Some("") {
            self.ai.model = None;
        }

        // Log level must be a recognised filter; otherwise warn and disable.
        if self.log.level.parse::<log::LevelFilter>().is_err() {
            eprintln!(
                "warning: log.level = \"{}\" is not a valid level \
                 (off|error|warn|info|debug|trace); using off",
                self.log.level
            );
            self.log.level = "off".to_string();
        }
    }
}

fn clamp_unit(name: &str, v: &mut f64) {
    if !(0.0..=1.0).contains(v) {
        eprintln!("warning: {name} = {v} out of range [0.0, 1.0]; clamping");
        *v = v.clamp(0.0, 1.0);
    }
}

fn require_positive(name: &str, v: &mut usize, default: usize) {
    if *v == 0 {
        eprintln!("warning: {name} must be > 0; using default {default}");
        *v = default;
    }
}

// ── Global accessor ───────────────────────────────────────────────────────────

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// The process-wide effective settings, loaded once on first access.
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(Settings::load)
}

/// Eagerly load settings and bridge config-only values into the mechanisms that
/// can't read `settings()` directly (the library indexer reads an env var).
/// Call once at startup. Bridging respects precedence: it only sets the env var
/// when the user hasn't already set it AND has actually customized the value
/// (setting it unconditionally would make `config show` report a phantom env
/// override, and is unnecessary since the indexer's own default matches ours).
pub fn init() {
    let s = settings();
    let is_default = s.index.max_file_bytes == IndexSettings::default().max_file_bytes;
    if !is_default && std::env::var("SCLEROX_MAX_INDEX_FILE_BYTES").is_err() {
        // SAFETY: single-threaded startup, before any indexing thread is spawned.
        unsafe {
            std::env::set_var(
                "SCLEROX_MAX_INDEX_FILE_BYTES",
                s.index.max_file_bytes.to_string(),
            );
        }
    }
    // Same bridge for the max-files cap: the library indexer reads it from the
    // env var, so surface a customized config value there.
    let files_is_default = s.index.max_files == IndexSettings::default().max_files;
    if !files_is_default && std::env::var("SCLEROX_MAX_INDEX_FILES").is_err() {
        // SAFETY: single-threaded startup, before any indexing thread is spawned.
        unsafe {
            std::env::set_var("SCLEROX_MAX_INDEX_FILES", s.index.max_files.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let s = Settings::from_toml_str("").unwrap();
        assert_eq!(s.search.semantic_threshold, 0.45);
        assert_eq!(s.search.semantic_limit, 5);
        assert_eq!(s.dedup.cosine_threshold, 0.85);
        assert_eq!(s.memory.max_value_chars, 800);
        assert_eq!(s.session_context.max_chars, 3000);
        assert_eq!(s.embed.chunk_size, 800);
        assert!(s.ai.command.is_none());
        assert!(s.ai.model.is_none());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let toml = r#"
[search]
semantic_threshold = 0.7
"#;
        let s = Settings::from_toml_str(toml).unwrap();
        assert_eq!(s.search.semantic_threshold, 0.7);
        // untouched keys keep defaults
        assert_eq!(s.search.semantic_limit, 5);
        assert_eq!(s.dedup.cosine_threshold, 0.85);
    }

    #[test]
    fn unknown_section_absent_uses_defaults() {
        // A file setting only db_path leaves every section at defaults.
        let s = Settings::from_toml_str("db_path = \"/tmp/x.db\"").unwrap();
        assert_eq!(s.db_path, PathBuf::from("/tmp/x.db"));
        assert_eq!(s.distill.chunk_chars, 20_000);
    }

    #[test]
    fn validate_clamps_out_of_range() {
        let mut s = Settings::from_toml_str("[search]\nsemantic_threshold = 5.0\n").unwrap();
        s.validate();
        assert_eq!(s.search.semantic_threshold, 1.0);
    }

    #[test]
    fn validate_repairs_zero_counts() {
        let mut s = Settings::from_toml_str("[search]\nsemantic_limit = 0\n").unwrap();
        s.validate();
        assert_eq!(s.search.semantic_limit, 5);
    }

    #[test]
    fn log_level_defaults_off_and_validates() {
        let s = Settings::from_toml_str("").unwrap();
        assert_eq!(s.log.level, "off");

        let mut good = Settings::from_toml_str("[log]\nlevel = \"debug\"\n").unwrap();
        good.validate();
        assert_eq!(good.log.level, "debug");

        let mut bad = Settings::from_toml_str("[log]\nlevel = \"loud\"\n").unwrap();
        bad.validate();
        assert_eq!(bad.log.level, "off");
    }

    #[test]
    fn validate_normalises_empty_model() {
        let mut s = Settings::from_toml_str("[ai]\nmodel = \"\"\n").unwrap();
        s.validate();
        assert!(s.ai.model.is_none());
    }

    #[test]
    fn malformed_toml_errors() {
        assert!(Settings::from_toml_str("this is = = not toml").is_err());
    }

    #[test]
    fn install_defaults_and_override() {
        // Default: everything refreshes on install.
        let s = Settings::from_toml_str("").unwrap();
        assert!(s.install.overwrite_skill);
        assert!(s.install.overwrite_hooks);
        assert!(s.install.overwrite_instructions);

        // A partial override keeps the other keys at their defaults.
        let s = Settings::from_toml_str("[install]\noverwrite_skill = false\n").unwrap();
        assert!(!s.install.overwrite_skill);
        assert!(s.install.overwrite_hooks);
        assert!(s.install.overwrite_instructions);
    }

    #[test]
    fn index_defaults_and_validation() {
        // Defaults.
        let s = Settings::from_toml_str("").unwrap();
        assert_eq!(s.index.auto, "git");
        assert_eq!(s.index.max_files, 50_000);

        // Valid override kept.
        let mut ok = Settings::from_toml_str("[index]\nauto = \"off\"\nmax_files = 10\n").unwrap();
        ok.validate();
        assert_eq!(ok.index.auto, "off");
        assert_eq!(ok.index.max_files, 10);

        // Unknown auto value falls back to git; zero max_files repaired.
        let mut bad =
            Settings::from_toml_str("[index]\nauto = \"everything\"\nmax_files = 0\n").unwrap();
        bad.validate();
        assert_eq!(bad.index.auto, "git");
        assert_eq!(bad.index.max_files, 50_000);
    }
}
