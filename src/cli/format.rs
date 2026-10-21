/// Shared single-line formatters used by list, search, and any other output.
/// One place to change means every command stays in sync.
use crate::db::{investigations::Investigation, todos::Todo};

pub fn todo_line(t: &Todo) -> String {
    let checkbox = match t.status.as_str() {
        "done" => "[x]",
        "watch" => "[~]",
        _ => "[ ]",
    };
    let id_col = format!("{:<5}", format!("#{}", t.id));
    let cat_col = format!("{:<9}", format!("[{}]", t.category));
    let deadline = t
        .deadline_date
        .as_deref()
        .map(|d| format!("  deadline:{d}"))
        .unwrap_or_default();
    let source = t
        .source_url
        .as_deref()
        .map(|u| format!("  {u}"))
        .unwrap_or_default();
    format!(
        "{checkbox} {id_col} {cat_col} {}{deadline}{source}",
        t.title
    )
}

pub fn research_line(inv: &Investigation) -> String {
    let checkbox = if inv.status == "concluded" {
        "[x]"
    } else {
        "[ ]"
    };
    let id_col = format!("{:<5}", format!("#{}", inv.id));
    format!("{checkbox} {id_col} {}  ({})", inv.name, inv.slug)
}

/// When a search hit was last changed, appended to its line:
/// `(updated <at>)`, or `(created <at>)` for records never updated. `at` is
/// shown as stored (UTC).
pub fn stamp(label: &str, at: &str) -> String {
    format!("({label} {at})")
}

/// Map a status string to its checkbox.
pub fn status_checkbox(status: &str) -> &'static str {
    match status {
        "done" | "concluded" => "[x]",
        "watch" => "[~]",
        _ => "[ ]",
    }
}

/// A code hit's location, `<repo root>/<file>:<line>`: a path that can be
/// opened directly and that tells clones of one repo apart.
/// Made [`terminal_safe`], since a file name may hold control characters.
pub fn code_location(repo_root: &str, file_path: &str, line: Option<i64>) -> String {
    let line = line.map(|l| format!(":{l}")).unwrap_or_default();
    terminal_safe(&format!("{repo_root}/{file_path}{line}"))
}

/// A symbol hit: `[kind] signature (location)`, made [`terminal_safe`] (a
/// signature is source text).
pub fn symbol_line(kind: &str, signature: &str, location: &str) -> String {
    terminal_safe(&format!("[{kind}] {signature} ({location})"))
}

/// `text` safe to print: repo content (file names, source) is untrusted, so
/// a control character, which as part of an escape sequence could rewrite the
/// terminal, is shown escaped, and a tab as a space. Its output holds no
/// control characters, so applying it twice changes nothing.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push(' '),
            c if c.is_control() => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    out
}

/// `text` cut to the snippet length, with `...` when cut, and made
/// [`terminal_safe`].
fn snippet(text: &str) -> String {
    let cut: String = text.chars().take(CODE_SNIPPET_CHARS).collect();
    let mut out = terminal_safe(&cut);
    if text.chars().nth(CODE_SNIPPET_CHARS).is_some() {
        out.push_str("...");
    }
    out
}

/// A full-text code hit: `location`, then the matching line, indented and
/// cut to the snippet length.
pub fn code_text_lines(location: &str, text: &str) -> String {
    format!("{location}\n        {}", snippet(text))
}

/// A semantic code-chunk hit: `(score) location`, then an indented one-line
/// snippet of the chunk.
pub fn code_chunk_lines(score: f32, location: &str, text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("({score:.2}) {location}\n        {}", snippet(&flat))
}

const CODE_SNIPPET_CHARS: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_location_appends_the_line_only_when_known() {
        assert_eq!(code_location("/r", "src/a.rs", Some(7)), "/r/src/a.rs:7");
        assert_eq!(code_location("/r", "src/a.rs", None), "/r/src/a.rs");
    }

    #[test]
    fn code_text_lines_shows_location_then_bounded_line() {
        assert_eq!(
            code_text_lines("/r/a.rs:3", "let x = 1;"),
            "/r/a.rs:3\n        let x = 1;"
        );
        let long = code_text_lines("/r/a.rs:3", &"y".repeat(200));
        assert!(long.ends_with(&format!("{}...", "y".repeat(CODE_SNIPPET_CHARS))));
    }

    #[test]
    fn snippets_escape_control_characters() {
        let line = "a\tb \u{1b}[2Jgone\u{7}";
        assert_eq!(
            code_text_lines("/r/a.rs:1", line),
            "/r/a.rs:1\n        a b \\u{1b}[2Jgone\\u{7}"
        );
        assert!(!code_chunk_lines(0.5, "/r/a.rs:1", line).contains('\u{1b}'));

        // File names and signatures are repo content too.
        let loc = code_location("/r", "x\u{1b}]0;pwned\u{7}.rs", Some(3));
        assert_eq!(loc, "/r/x\\u{1b}]0;pwned\\u{7}.rs:3");
        assert_eq!(terminal_safe(&loc), loc, "idempotent");
        let sym = symbol_line("fn", "fn a() { \"\u{1b}[2J\" }", &loc);
        assert!(!sym.chars().any(char::is_control), "{sym}");
    }

    #[test]
    fn code_chunk_snippet_is_one_line_and_bounded() {
        let text = format!("fn a() {{\n    {}\n}}", "x".repeat(200));
        let out = code_chunk_lines(0.861, "/r/a.rs:1", &text);
        let (head, snippet) = out.split_once('\n').unwrap();
        assert_eq!(head, "(0.86) /r/a.rs:1");
        assert!(!snippet.trim().contains('\n'));
        assert!(snippet.trim().starts_with("fn a() { xxx"));
        assert!(snippet.ends_with("..."));
        assert_eq!(snippet.trim().chars().count(), CODE_SNIPPET_CHARS + 3);
    }
}
