use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
        }
    }
}

/// Print a serializable value as pretty JSON if format is Json, otherwise call the text closure.
pub fn print_output<T, F>(format: OutputFormat, value: &T, text_fn: F)
where
    T: serde::Serialize,
    F: FnOnce(),
{
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(value)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
            );
        }
        OutputFormat::Text => text_fn(),
    }
}

/// Largest byte index `<= max` that lands on a UTF-8 char boundary (stable-Rust
/// stand-in for the unstable `str::floor_char_boundary`).
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Cut `s` to at most `max` bytes plus a `...` marker. The cut backs off to a
/// char boundary, so a multibyte character straddling `max` can't panic.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..floor_char_boundary(s, max)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_backs_off_multibyte_char_at_limit() {
        // 'é' occupies bytes 119..121, so a raw &s[..120] would panic.
        let s = format!("{}é caching", "a".repeat(119));
        assert_eq!(truncate(&s, 120), format!("{}...", "a".repeat(119)));
    }

    #[test]
    fn test_truncate_short_and_exact_passthrough() {
        assert_eq!(truncate("héllo", 6), "héllo");
        assert_eq!(truncate("héllo", 100), "héllo");
        assert_eq!(truncate("héllo", 2), "h...");
    }
}
