/// Sanitize a user-provided query for SQLite FTS5 MATCH.
///
/// FTS5 treats `-`, `AND`, `OR`, `NOT`, `*`, `(`, `)` as operators.
/// Wrapping each term in `"term"*` syntax gives us:
///   - Literal matching (no operator interpretation, so "account-doc" is a phrase,
///     not `account NOT doc NOT worker`).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_term() {
        assert_eq!(sanitize("rust"), "\"rust\"*");
    }

    #[test]
    fn test_hyphenated_identifier() {
        assert_eq!(
            sanitize("accounting-doc-worker"),
            "\"accounting-doc-worker\"*"
        );
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
