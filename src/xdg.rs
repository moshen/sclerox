//! XDG Base Directory resolution, applied the same way on every platform
//! (including Windows) rather than following each OS's native convention
//! (no `%APPDATA%`). Respects the standard `XDG_*_HOME` env vars; falls back
//! to the spec's default `~/.config`, `~/.local/share`, `~/.local/state`
//! otherwise.

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME`, else `~/.config`. Holds `config.toml`.
pub fn config_home() -> PathBuf {
    from_env_or_home("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME`, else `~/.local/share`. Holds the primary database.
pub fn data_home() -> PathBuf {
    from_env_or_home("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_STATE_HOME`, else `~/.local/state`. Holds logs and other
/// non-portable state (distillation markers) that isn't worth backing up
/// alongside real data but shouldn't be treated as disposable cache either.
pub fn state_home() -> PathBuf {
    from_env_or_home("XDG_STATE_HOME", ".local/state")
}

fn from_env_or_home(var: &str, fallback_rel: &str) -> PathBuf {
    if let Ok(v) = std::env::var(var) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(fallback_rel))
        .unwrap_or_else(|| PathBuf::from(fallback_rel))
}
