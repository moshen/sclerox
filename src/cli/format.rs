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

/// Map a status string to its checkbox.
pub fn status_checkbox(status: &str) -> &'static str {
    match status {
        "done" | "concluded" => "[x]",
        "watch" => "[~]",
        _ => "[ ]",
    }
}
