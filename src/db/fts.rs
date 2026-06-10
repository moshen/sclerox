/// Sanitize a user-provided query for SQLite FTS5 MATCH.
///
/// FTS5 treats `-`, `AND`, `OR`, `NOT`, `*`, `(`, `)` as operators.
/// Wrapping each whitespace-separated term in double quotes turns them into
/// literal phrase queries, so "accounting-doc-worker" finds the phrase
/// [accounting, doc, worker] in sequence rather than `accounting NOT doc NOT worker`.
///
/// Every term is quoted, so FTS5 boolean operators are not available through
/// this interface - which is the safe default for user-provided queries.
pub fn sanitize(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_term() {
        assert_eq!(sanitize("rust"), "\"rust\"");
    }

    #[test]
    fn test_hyphenated_identifier() {
        assert_eq!(
            sanitize("accounting-doc-worker"),
            "\"accounting-doc-worker\""
        );
    }

    #[test]
    fn test_multi_word() {
        assert_eq!(sanitize("rust code"), "\"rust\" \"code\"");
    }

    #[test]
    fn test_strips_embedded_quotes() {
        assert_eq!(sanitize("hello\"world"), "\"helloworld\"");
    }

    #[test]
    fn test_empty_is_empty() {
        assert_eq!(sanitize(""), "");
    }
}
