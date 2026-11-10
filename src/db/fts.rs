use anyhow::Result;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, Row};

use super::dates::Dates;

/// Words too common to say what a query or text is about.
pub const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "been",
    "being", "but", "by", "can", "could", "did", "do", "does", "each", "for", "from", "get", "had",
    "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "must", "my",
    "not", "now", "of", "off", "on", "one", "or", "our", "out", "per", "set", "should", "so",
    "some", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this",
    "those", "to", "us", "use", "used", "uses", "very", "via", "was", "we", "were", "what", "when",
    "where", "which", "who", "why", "will", "with", "would", "you", "your",
];

/// Most hits the [`loose_query`] tier adds per table: it matches any one term,
/// so a common term could otherwise bring in most of the table.
const LOOSE_LIMIT: usize = 10;

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

/// A MATCH query for records holding any significant term of `query`, for
/// when no record holds them all. Stopwords are left out unless that would
/// leave nothing. `None` when it would be the same search as [`sanitize`].
/// bm25 ranks the matches, so a record with rarer terms comes first.
pub fn loose_query(query: &str) -> Option<String> {
    let terms: Vec<(String, String)> = query
        .split_whitespace()
        .map(|term| {
            let word = term.trim_matches(|c: char| !c.is_alphanumeric());
            (term.replace('"', ""), word.to_lowercase())
        })
        .filter(|(_, word)| !word.is_empty())
        .collect();
    let significant: Vec<&String> = terms
        .iter()
        .filter(|(_, word)| !STOPWORDS.contains(&word.as_str()))
        .map(|(term, _)| term)
        .collect();
    let kept = if significant.is_empty() {
        terms.iter().map(|(term, _)| term).collect()
    } else {
        significant
    };
    (kept.len() > 1 || (kept.len() == 1 && terms.len() > 1)).then(|| {
        kept.iter()
            .map(|term| format!("\"{term}\"*"))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// A table and its full-text index `fts`, and how [`search`] ranks matches.
pub struct Source<'a> {
    pub fts: &'a str,
    pub table: &'a str,
    pub columns: &'a str,
    /// bm25 weight of each indexed column, in the index's order, so a match in
    /// a key, title or name counts for more than one in body text.
    pub weights: &'a [f64],
    /// Substring match on `table`, with `?1` as the [`like_contains`] pattern.
    pub like: &'a str,
    /// Orders substring matches, which have no bm25 score, and breaks ties.
    pub then_by: &'a str,
    /// Keep only rows with this status.
    pub status: Option<&'a str>,
    /// Keep only rows dated in this range: by the `changed` column, or under
    /// `--created` the `created` one.
    pub dates: &'a Dates,
    pub changed: &'a str,
    pub created: &'a str,
}

impl Source<'_> {
    fn bm25(&self) -> String {
        let weights: Vec<String> = self.weights.iter().map(|w| w.to_string()).collect();
        format!("bm25({}, {})", self.fts, weights.join(", "))
    }

    /// `table` joined to its rows matching `?1`, best first. `?2` is a row
    /// limit; see [`Source::filter`] for the rest.
    fn matching(&self) -> String {
        let (columns, table, fts) = (self.columns, self.table, self.fts);
        format!(
            "SELECT {columns} FROM {table}
             JOIN (SELECT rowid AS hit_id, {bm25} AS hit_rank FROM {fts} WHERE {fts} MATCH ?1) hits
               ON hits.hit_id = {table}.id
             WHERE {filter}
             ORDER BY hits.hit_rank, {then_by} LIMIT ?2",
            bm25 = self.bm25(),
            filter = self.filter(),
            then_by = self.then_by,
        )
    }

    /// The status (`?3`, when kept) and date range (`?4`, `?5`, NULL for an
    /// open end) rows must have.
    fn filter(&self) -> String {
        let dates = self
            .dates
            .sql(self.dates.pick(self.changed, self.created), 4);
        match self.status {
            Some(_) => format!("{}.status = ?3 AND {dates}", self.table),
            None => dates,
        }
    }

    fn query<T>(
        &self,
        conn: &Connection,
        sql: &str,
        pattern: String,
        limit: Option<usize>,
        row: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let [from, to] = self.dates.values();
        let text = |v: Option<String>| v.map_or(Value::Null, Value::Text);
        let values = [
            Value::Text(pattern),
            Value::Integer(limit.map_or(-1, |n| n as i64)),
            text(self.status.map(str::to_string)),
            text(from),
            text(to),
        ];
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params_from_iter(values), row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Records holding every term of `query`, by weighted bm25, then those
/// holding it as a substring, which full-text matching misses mid-word.
pub fn search_all_terms<T>(
    conn: &Connection,
    query: &str,
    source: &Source<'_>,
    id: impl Fn(&T) -> i64,
    mut row: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut hits = source.query(conn, &source.matching(), sanitize(query), None, &mut row)?;
    let seen: std::collections::HashSet<i64> = hits.iter().map(&id).collect();
    let like = format!(
        "SELECT {columns} FROM {table} WHERE ({like}) AND {filter} ORDER BY {then_by} LIMIT ?2",
        columns = source.columns,
        table = source.table,
        like = source.like,
        filter = source.filter(),
        then_by = source.then_by,
    );
    let substring = source.query(conn, &like, like_contains(query), None, &mut row)?;
    hits.extend(substring.into_iter().filter(|hit| !seen.contains(&id(hit))));
    Ok(hits)
}

/// [`search_all_terms`], then the best few records holding only some of the
/// terms (see [`loose_query`]).
pub fn search<T>(
    conn: &Connection,
    query: &str,
    source: &Source<'_>,
    id: impl Fn(&T) -> i64,
    mut row: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut hits = search_all_terms(conn, query, source, &id, &mut row)?;
    let Some(loose) = loose_query(query) else {
        return Ok(hits);
    };
    let seen: std::collections::HashSet<i64> = hits.iter().map(&id).collect();
    let rows = source.query(
        conn,
        &source.matching(),
        loose,
        Some(seen.len() + LOOSE_LIMIT),
        row,
    )?;
    hits.extend(
        rows.into_iter()
            .filter(|hit| !seen.contains(&id(hit)))
            .take(LOOSE_LIMIT),
    );
    Ok(hits)
}

/// A MATCH query for a `tokenize='trigram'` FTS5 table: every term of
/// `query` as a quoted (so literal) substring, all required. The trigram
/// index can't look up a term under three characters, so those are left out;
/// `None` when that leaves nothing to look up.
pub fn trigram_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.chars().count() >= 3)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
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
    fn trigram_query_quotes_terms_and_drops_short_ones() {
        assert_eq!(trigram_query("fn sanitize"), Some("\"sanitize\"".into()));
        assert_eq!(
            trigram_query(r#"a"b"c x_y.z"#),
            Some(r#""a""b""c" "x_y.z""#.into())
        );
        assert_eq!(trigram_query("fn if"), None);
        assert_eq!(trigram_query(""), None);
    }

    #[test]
    fn loose_query_drops_stopwords_and_matches_any_term() {
        assert_eq!(
            loose_query("why did we drop CoreML?").as_deref(),
            Some(r#""drop"* OR "CoreML?"*"#)
        );
        assert_eq!(loose_query("what is retry").as_deref(), Some(r#""retry"*"#));
    }

    #[test]
    fn loose_query_keeps_the_terms_when_only_stopwords_remain() {
        assert_eq!(
            loose_query("what is").as_deref(),
            Some(r#""what"* OR "is"*"#)
        );
    }

    #[test]
    fn loose_query_is_none_when_it_would_repeat_the_strict_search() {
        assert_eq!(loose_query("retry"), None);
        assert_eq!(loose_query("the"), None);
        assert_eq!(loose_query("- ?"), None);
        assert_eq!(loose_query(""), None);
    }

    #[test]
    fn test_empty_is_empty() {
        assert_eq!(sanitize(""), "");
    }
}
