//! XDG Base Directory resolution, applied the same way on every platform
//! (including Windows) rather than following each OS's native convention
//! (no `%APPDATA%`). Respects the standard `XDG_*_HOME` env vars; falls back
//! to the spec's default `~/.config`, `~/.local/share`, `~/.local/state`,
//! `~/.cache` otherwise.

use std::path::PathBuf;

/// The user's home directory.
///
/// `dirs::home_dir()` asks Windows for `FOLDERID_Profile` and ignores the
/// environment entirely, so on that platform a process cannot be pointed at a
/// different home. That silently breaks `~`-relative paths for anything that
/// sets one deliberately - a test pinning every path into a temp dir, a
/// service account, a `runas` shell. Consult the platform's home variable
/// first, the way `std::env::home_dir` does, and fall back to `dirs` when it
/// is unset.
///
/// On Unix this is what `dirs` already does; only Windows changes.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => dirs::home_dir(),
    }
}

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

/// `$XDG_CACHE_HOME`, else `~/.cache`. Holds downloaded embedding models:
/// large, re-fetchable, and safe to delete.
pub fn cache_home() -> PathBuf {
    from_env_or_home("XDG_CACHE_HOME", ".cache")
}

fn from_env_or_home(var: &str, fallback_rel: &str) -> PathBuf {
    resolve(std::env::var(var).ok(), home_dir(), fallback_rel)
}

/// The resolution rule, separated from the process environment so it can be
/// tested without mutating env vars that parallel tests also read.
fn resolve(env_value: Option<String>, home: Option<PathBuf>, fallback_rel: &str) -> PathBuf {
    match env_value {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home
            .map(|h| h.join(fallback_rel))
            .unwrap_or_else(|| PathBuf::from(fallback_rel)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_value_wins() {
        let got = resolve(
            Some("/xdg/cache".into()),
            Some(PathBuf::from("/home/u")),
            ".cache",
        );
        assert_eq!(got, PathBuf::from("/xdg/cache"));
    }

    #[test]
    fn empty_env_value_falls_back_to_home() {
        // The spec treats an empty XDG_*_HOME as unset.
        let got = resolve(
            Some(String::new()),
            Some(PathBuf::from("/home/u")),
            ".cache",
        );
        assert_eq!(got, PathBuf::from("/home/u").join(".cache"));
    }

    #[test]
    fn unset_env_falls_back_to_home() {
        let got = resolve(None, Some(PathBuf::from("/home/u")), ".cache");
        assert_eq!(got, PathBuf::from("/home/u").join(".cache"));
    }

    #[test]
    fn no_home_yields_relative_fallback() {
        assert_eq!(resolve(None, None, ".cache"), PathBuf::from(".cache"));
    }
}
