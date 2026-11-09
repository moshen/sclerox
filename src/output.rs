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

/// Print a serializable value as pretty JSON if format is Json. In text it
/// calls the text closure, or under `--full` prints every field of the value.
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
        OutputFormat::Text if full() => match serde_json::to_value(value) {
            Ok(value) => print!("{}", full_text(&value)),
            Err(_) => text_fn(),
        },
        OutputFormat::Text => text_fn(),
    }
}

/// `value` as labelled text, nothing omitted or cut. Objects print one
/// `field: value` line each, nesting and multi-line text are indented, and
/// the records of a list are separated by a blank line.
pub fn full_text(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_full(value, 0, &mut out);
    out
}

fn write_full(value: &serde_json::Value, indent: usize, out: &mut String) {
    use serde_json::Value;
    let pad = " ".repeat(indent);
    match value {
        Value::Array(items) if !items.is_empty() => {
            for (i, item) in items.iter().enumerate() {
                if item.is_object() || item.is_array() {
                    if i > 0 {
                        out.push('\n');
                    }
                    write_full(item, indent, out);
                } else {
                    out.push_str(&format!("{pad}- {}\n", scalar_text(item)));
                }
            }
        }
        Value::Object(fields) if !fields.is_empty() => {
            for (name, field) in fields {
                let text = scalar_text(field);
                let nested = matches!(field, Value::Array(a) if !a.is_empty())
                    || matches!(field, Value::Object(o) if !o.is_empty());
                if nested {
                    out.push_str(&format!("{pad}{name}:\n"));
                    write_full(field, indent + 2, out);
                } else if text.contains('\n') {
                    out.push_str(&format!("{pad}{name}:\n"));
                    for line in text.lines() {
                        out.push_str(&format!("{pad}  {line}\n"));
                    }
                } else {
                    out.push_str(&format!("{pad}{name}: {text}\n"));
                }
            }
        }
        other => {
            for line in scalar_text(other).lines() {
                out.push_str(&format!("{pad}{line}\n"));
            }
        }
    }
}

/// A leaf value as text: `-` for nothing, and a string safe for a terminal
/// but with its tabs and line breaks kept.
fn scalar_text(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null | Value::Array(_) | Value::Object(_) => "-".to_string(),
        Value::String(s) => {
            let mut text = String::with_capacity(s.len());
            for c in s.replace("\r\n", "\n").chars() {
                match c {
                    '\n' | '\t' => text.push(c),
                    c if c.is_control() => text.extend(c.escape_default()),
                    c => text.push(c),
                }
            }
            text
        }
        other => other.to_string(),
    }
}

/// Report a miss that isn't a failure ("not found", "no changes"): the message
/// as-is in text, `{"error": msg}` in JSON. The exit status is unchanged.
pub fn print_miss(format: OutputFormat, msg: &str) {
    match format {
        OutputFormat::Json => print_output(format, &serde_json::json!({ "error": msg }), || {}),
        OutputFormat::Text => println!("{msg}"),
    }
}

static FORMAT: std::sync::OnceLock<OutputFormat> = std::sync::OnceLock::new();

/// Record the `--output` format for the process, so helpers deep in a command
/// can keep stdout clean for JSON without threading it through. Set once, by
/// the CLI entry point.
pub fn set_format(format: OutputFormat) {
    let _ = FORMAT.set(format);
}

/// The `--output` format the process was started with.
pub fn format() -> OutputFormat {
    FORMAT.get().copied().unwrap_or_default()
}

static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Record `--full` for the process. Set once, by the CLI entry point.
pub fn set_full(full: bool) {
    let _ = FULL.set(full);
}

/// Whether `--full` was given: text shows every field and nothing is cut.
pub fn full() -> bool {
    FULL.get().copied().unwrap_or_default()
}

/// Print a progress or diagnostic line. Under `--output json` it goes to
/// stderr, so stdout stays a single JSON document.
pub fn progress(msg: impl fmt::Display) {
    match format() {
        OutputFormat::Json => eprintln!("{msg}"),
        OutputFormat::Text => println!("{msg}"),
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

/// Cut `s` to at most `max` bytes plus a `...` marker, unless `--full` was
/// given. The cut backs off to a char boundary, so a multibyte character
/// straddling `max` can't panic.
pub fn truncate(s: &str, max: usize) -> String {
    if full() {
        s.to_string()
    } else {
        truncate_bounded(s, max)
    }
}

/// [`truncate`] regardless of `--full`, for text with a real size limit
/// rather than text being displayed.
pub fn truncate_bounded(s: &str, max: usize) -> String {
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
    fn full_text_labels_every_field_and_keeps_all_content() {
        let long = "y".repeat(500);
        let value = serde_json::json!([
            {"id": 1, "notes": "one\r\ntwo", "gone": null, "long": long},
            {"id": 2, "tags": ["a"], "people": [{"name": "Sam"}], "none": []},
        ]);
        let expected = format!(
            "gone: -\nid: 1\nlong: {long}\nnotes:\n  one\n  two\n\n\
             id: 2\nnone: -\npeople:\n  name: Sam\ntags:\n  - a\n"
        );
        assert_eq!(full_text(&value), expected);
    }

    #[test]
    fn full_text_escapes_control_characters_but_keeps_tabs() {
        let out = full_text(&serde_json::json!({"a": "x\ty\u{1b}[2J"}));
        assert_eq!(out, "a: x\ty\\u{1b}[2J\n");
    }

    #[test]
    fn test_truncate_short_and_exact_passthrough() {
        assert_eq!(truncate("héllo", 6), "héllo");
        assert_eq!(truncate("héllo", 100), "héllo");
        assert_eq!(truncate("héllo", 2), "h...");
    }
}
