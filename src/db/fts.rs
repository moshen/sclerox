/// Sanitize a user-provided query for SQLite FTS5 MATCH.
///
/// FTS5 treats `-`, `AND`, `OR`, `NOT`, `*`, `(`, `)` as operators.
/// Wrapping each term in `"term"*` syntax gives us:
///   - Literal matching (no operator interpretation, so "report-gen" is a phrase,
///     not `report NOT gen NOT worker`).
///   - Prefix matching: `"split"*` matches "split", "splits", "splitting", etc.
///     This is the standard expectation for a search box.
///
/// Every term is independently prefix-quoted; FTS5 boolean operators are not
/// available through this interface, which is the safe default.
pub fn sanitize(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A `LIKE ... ESCAPE '\\'` pattern matching `query` as a literal substring.
///
/// `%` and `_` are LIKE wildcards, so an unescaped `_` matched every row and a
/// `%` in "95%" matched anything. Escapes them (and the escape char itself) so
/// the query text is only ever matched literally.
pub fn like_contains(query: &str) -> String {
    let mut pat = String::with_capacity(query.len() + 2);
    pat.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pat.push('\\');
        }
        pat.push(c);
    }
    pat.push('%');
    pat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_like_contains_escapes_wildcards() {
        assert_eq!(like_contains("rust"), "%rust%");
        assert_eq!(like_contains("95%_a\\b"), "%95\\%\\_a\\\\b%");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let hits = |q: &str, v: &str| -> bool {
            conn.query_row(
                "SELECT ?2 LIKE ?1 ESCAPE '\\'",
                rusqlite::params![like_contains(q), v],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!hits("_", "retry budget is 3"));
        assert!(hits("_", "snake_case"));
        assert!(!hits("5%", "cpu at 95 for an hour"));
        assert!(hits("5%", "cpu at 95% for an hour"));
        assert!(hits("a\\b", "path a\\b"));
    }

    #[test]
    fn test_plain_term() {
        assert_eq!(sanitize("rust"), "\"rust\"*");
    }

    #[test]
    fn test_hyphenated_identifier() {
        assert_eq!(sanitize("report-gen-worker"), "\"report-gen-worker\"*");
    }

    #[test]
    fn test_multi_word() {
        assert_eq!(sanitize("rust code"), "\"rust\"* \"code\"*");
    }

    #[test]
    fn test_prefix_matching() {
        // Verify the generated syntax actually works in SQLite FTS5
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(title);
             INSERT INTO t VALUES('Review ERD UK VAT Splits Extraction Q1 FY27');",
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM t WHERE t MATCH '{}'",
                    sanitize("split")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "prefix 'split' should match 'Splits'");
    }

    #[test]
    fn test_strips_embedded_quotes() {
        assert_eq!(sanitize("hello\"world"), "\"helloworld\"*");
    }

    #[test]
    fn test_empty_is_empty() {
        assert_eq!(sanitize(""), "");
    }
}
