//! The `word_parts` FTS5 tokenizer: `unicode61`, plus the parts of each
//! camelCase word, so `document` finds `FinancialDocument`. snake_case needs
//! nothing, as `unicode61` already splits at `_`.
//!
//! A part is indexed at the same position as its word (a colocated token,
//! as FTS5 indexes synonyms), so the word still matches whole and phrases
//! still match. Queries are tokenized without parts: a term matches a whole
//! word or one of its parts. The prose indexes use `porter word_parts`, so
//! parts are stemmed too.
//!
//! SQLite knows the tokenizer only on connections given to [`register`], so
//! another connection (the `sqlite3` shell) can't search those indexes or
//! write to the tables that feed them.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use rusqlite::ffi::{
    fts5_api, fts5_tokenizer, sqlite3, Fts5Tokenizer, FTS5_TOKENIZE_QUERY, FTS5_TOKEN_COLOCATED,
    SQLITE_ERROR, SQLITE_OK,
};

/// The byte ranges of the parts of a camelCase `word`, each at least two
/// characters: a new part starts at an upper-case letter after a lower-case
/// letter or digit (`utf8Decoder`), and at the last of a run of capitals
/// that a lower-case letter follows (`HTTPServer`). Empty for a word of one
/// part.
pub fn parts(word: &str) -> Vec<std::ops::Range<usize>> {
    let chars: Vec<(usize, char)> = word.char_indices().collect();
    let mut starts = vec![0];
    // The letters and digits either side, past any combining mark
    // (`e\u{301}`).
    let mut prev: Option<char> = None;
    for (i, &(_, c)) in chars.iter().enumerate() {
        let next_lower = chars[i + 1..]
            .iter()
            .find(|(_, n)| n.is_alphanumeric())
            .is_some_and(|(_, n)| n.is_lowercase());
        if let Some(prev) = prev.filter(|_| c.is_uppercase()) {
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                starts.push(i);
            }
        }
        if c.is_alphanumeric() {
            prev = Some(c);
        }
    }
    if starts.len() < 2 {
        return Vec::new();
    }
    starts.push(chars.len());
    let byte = |i: usize| chars.get(i).map_or(word.len(), |(b, _)| *b);
    starts
        .windows(2)
        .filter(|w| w[1] - w[0] >= 2)
        .map(|w| byte(w[0])..byte(w[1]))
        .collect()
}

/// Make `word_parts` available on `conn`, which must happen before the
/// prose indexes are read or written. It reads the schema, so it waits out a
/// locked database as long as `conn`'s busy timeout allows.
pub fn register(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // SAFETY: the handle is open for as long as `conn`, and the tokenizer
    // lives with it: FTS5 copies the struct and drops the tokenizer when the
    // connection closes.
    unsafe {
        let db = conn.handle();
        let api = fts5(db)?;
        let Some(create) = (*api).xCreateTokenizer else {
            anyhow::bail!("FTS5 can't take a tokenizer");
        };
        let mut tokenizer = fts5_tokenizer {
            xCreate: Some(create_tokenizer),
            xDelete: Some(delete_tokenizer),
            xTokenize: Some(tokenize),
        };
        // `create_tokenizer` gets the api back, to find unicode61 with.
        let rc = create(
            api,
            c"word_parts".as_ptr(),
            api.cast(),
            &mut tokenizer,
            None,
        );
        if rc != SQLITE_OK {
            anyhow::bail!("could not add the word_parts tokenizer: {}", error(db, rc));
        }
    }
    Ok(())
}

/// `db`'s FTS5 API, through the pointer `SELECT fts5(?)` hands back.
unsafe fn fts5(db: *mut sqlite3) -> anyhow::Result<*mut fts5_api> {
    use rusqlite::ffi::{
        sqlite3_bind_pointer, sqlite3_finalize, sqlite3_prepare_v2, sqlite3_step, sqlite3_stmt,
        SQLITE_ROW,
    };
    let mut api: *mut fts5_api = ptr::null_mut();
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(
        db,
        c"SELECT fts5(?1)".as_ptr(),
        -1,
        &mut stmt,
        ptr::null_mut(),
    );
    if rc != SQLITE_OK {
        anyhow::bail!("could not find FTS5: {}", error(db, rc));
    }
    sqlite3_bind_pointer(
        stmt,
        1,
        (&mut api as *mut *mut fts5_api).cast(),
        c"fts5_api_ptr".as_ptr(),
        None,
    );
    let rc = sqlite3_step(stmt);
    sqlite3_finalize(stmt);
    if rc != SQLITE_ROW || api.is_null() {
        anyhow::bail!("could not find FTS5: {}", error(db, rc));
    }
    Ok(api)
}

/// `db`'s last error, as `rc` and its message.
unsafe fn error(db: *mut sqlite3, rc: c_int) -> String {
    let message = std::ffi::CStr::from_ptr(rusqlite::ffi::sqlite3_errmsg(db));
    format!("{} (code {rc})", message.to_string_lossy())
}

/// An instance: the unicode61 tokenizer it wraps.
struct WordParts {
    inner: *mut Fts5Tokenizer,
    x: fts5_tokenizer,
}

/// FTS5's `xCreate`: a unicode61 instance with the table's options, wrapped.
unsafe extern "C" fn create_tokenizer(
    api: *mut c_void,
    args: *mut *const c_char,
    n_args: c_int,
    out: *mut *mut Fts5Tokenizer,
) -> c_int {
    let api: *mut fts5_api = api.cast();
    let Some(find) = (*api).xFindTokenizer else {
        return SQLITE_ERROR;
    };
    let mut user_data = ptr::null_mut();
    let mut x = fts5_tokenizer {
        xCreate: None,
        xDelete: None,
        xTokenize: None,
    };
    let rc = find(api, c"unicode61".as_ptr(), &mut user_data, &mut x);
    if rc != SQLITE_OK {
        return rc;
    }
    let (Some(create), Some(_), Some(_)) = (x.xCreate, x.xDelete, x.xTokenize) else {
        return SQLITE_ERROR;
    };
    // Any options (`remove_diacritics 2`, say) are unicode61's.
    let mut inner = ptr::null_mut();
    let rc = create(user_data, args, n_args, &mut inner);
    if rc != SQLITE_OK {
        return rc;
    }
    *out = Box::into_raw(Box::new(WordParts { inner, x })).cast();
    SQLITE_OK
}

/// FTS5's `xDelete`: drop the instance and the unicode61 one it wraps.
unsafe extern "C" fn delete_tokenizer(tokenizer: *mut Fts5Tokenizer) {
    let tokenizer = Box::from_raw(tokenizer.cast::<WordParts>());
    if let Some(delete) = tokenizer.x.xDelete {
        delete(tokenizer.inner);
    }
}

type TokenFn =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int;

/// What [`on_token`] needs of one [`tokenize`] call.
struct Call {
    text: *const u8,
    len: usize,
    query: bool,
    token: TokenFn,
    ctx: *mut c_void,
}

/// FTS5's `xTokenize`: run unicode61 over `text`, with [`on_token`] adding
/// the parts of each word unless this is a query.
unsafe extern "C" fn tokenize(
    tokenizer: *mut Fts5Tokenizer,
    ctx: *mut c_void,
    flags: c_int,
    text: *const c_char,
    len: c_int,
    token: Option<TokenFn>,
) -> c_int {
    let tokenizer = &*tokenizer.cast::<WordParts>();
    let (Some(inner), Some(token)) = (tokenizer.x.xTokenize, token) else {
        return SQLITE_ERROR;
    };
    let mut call = Call {
        text: text.cast(),
        len: len.max(0) as usize,
        query: flags & FTS5_TOKENIZE_QUERY != 0,
        token,
        ctx,
    };
    inner(
        tokenizer.inner,
        (&mut call as *mut Call).cast(),
        flags,
        text,
        len,
        Some(on_token),
    )
}

/// Pass a unicode61 token on, then, in a document, its parts.
unsafe extern "C" fn on_token(
    call: *mut c_void,
    flags: c_int,
    token: *const c_char,
    len: c_int,
    start: c_int,
    end: c_int,
) -> c_int {
    let call = &*call.cast::<Call>();
    let rc = (call.token)(call.ctx, flags, token, len, start, end);
    if rc != SQLITE_OK || call.query || flags & FTS5_TOKEN_COLOCATED != 0 {
        return rc;
    }
    let (Ok(s), Ok(e)) = (usize::try_from(start), usize::try_from(end)) else {
        return rc;
    };
    if s >= e || e > call.len {
        return rc;
    }
    let text = std::slice::from_raw_parts(call.text, call.len);
    let folded = std::slice::from_raw_parts(token.cast::<u8>(), len.max(0) as usize);
    let (Ok(word), Ok(folded)) = (
        std::str::from_utf8(&text[s..e]),
        std::str::from_utf8(folded),
    ) else {
        return rc;
    };
    for (part, text) in folded_parts(word, folded) {
        let rc = (call.token)(
            call.ctx,
            FTS5_TOKEN_COLOCATED,
            text.as_ptr().cast(),
            text.len() as c_int,
            start + part.start as c_int,
            start + part.end as c_int,
        );
        if rc != SQLITE_OK {
            return rc;
        }
    }
    SQLITE_OK
}

/// The [`parts`] of `word` with their text as unicode61 folded the whole
/// word to `folded`: the same characters of it when folding kept one per
/// character (it drops diacritics), else the part's letters and digits
/// lower-cased, which drops combining marks as unicode61 does. Linear in the
/// word, however many parts it has.
fn folded_parts(word: &str, folded: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let parts = parts(word);
    if parts.is_empty() {
        return Vec::new();
    }
    let starts: Vec<usize> = word.char_indices().map(|(b, _)| b).collect();
    let folded: Vec<char> = folded.chars().collect();
    let same = folded.len() == starts.len();
    // A part's bounds are character boundaries, so each is found.
    let char_at = |b: usize| starts.binary_search(&b).unwrap_or(starts.len());
    parts
        .into_iter()
        .map(|part| {
            let text = if same {
                folded[char_at(part.start)..char_at(part.end)]
                    .iter()
                    .collect()
            } else {
                word[part.clone()]
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect()
            };
            (part, text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(word: &str) -> Vec<&str> {
        parts(word).into_iter().map(|r| &word[r]).collect()
    }

    #[test]
    fn camel_case_words_split_into_their_parts() {
        assert_eq!(split("FinancialDocument"), ["Financial", "Document"]);
        assert_eq!(split("parseInvoiceLines"), ["parse", "Invoice", "Lines"]);
        assert_eq!(split("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(split("utf8Decoder"), ["utf8", "Decoder"]);
        assert_eq!(split("CoreML"), ["Core", "ML"]);
        assert_eq!(split("Cafe\u{301}Menu"), ["Cafe\u{301}", "Menu"]);
        assert_eq!(split("HTTPŚerver"), ["HTTP", "Śerver"]);
        assert_eq!(split("HTTPS\u{301}erver"), ["HTTP", "S\u{301}erver"]);
        // A one-letter part isn't worth indexing.
        assert_eq!(split("getX"), ["get"]);
        // Nothing to split.
        assert!(split("Document").is_empty());
        assert!(split("HTTP").is_empty());
        assert!(split("ipv6").is_empty());
        assert!(split("").is_empty());
    }

    #[test]
    fn parts_take_the_folded_form() {
        let texts = |word, folded| -> Vec<String> {
            folded_parts(word, folded)
                .into_iter()
                .map(|(_, text)| text)
                .collect()
        };
        assert_eq!(texts("CaféMenu", "cafemenu"), ["cafe", "menu"]);
        // A decomposed é folds to one character fewer.
        assert_eq!(texts("Cafe\u{301}Menu", "cafemenu"), ["cafe", "menu"]);
        assert!(texts("Menu", "menu").is_empty());
    }

    #[test]
    fn a_long_joined_word_folds_in_linear_time() {
        let word = "Ab".repeat(50_000);
        let folded = word.to_lowercase();
        let started = std::time::Instant::now();
        assert_eq!(folded_parts(&word, &folded).len(), 50_000);
        // A few milliseconds; folding each part from the start of the word
        // took minutes, so a busy runner can't make this fail.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    fn matches(tokenize: &str, text: &str, query: &str) -> bool {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='{tokenize}');"
        ))
        .unwrap();
        conn.execute("INSERT INTO t VALUES (?1)", [text]).unwrap();
        conn.query_row("SELECT count(*) FROM t WHERE t MATCH ?1", [query], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            == 1
    }

    #[test]
    fn a_part_finds_the_word_it_is_in() {
        let t = "porter word_parts";
        let text = "Priya reviews every change to FinancialDocument.";
        assert!(matches(t, text, "document"));
        assert!(matches(t, text, "\"documents\""), "parts are stemmed");
        assert!(matches(t, text, "financial"));
        assert!(matches(t, text, "financialdocument"), "the whole word too");
        assert!(
            matches(t, text, "\"change to FinancialDocument\""),
            "phrases"
        );
        assert!(!matches(t, text, "\"document\"* \"parser\"*"));
        assert!(matches(t, "a DocumentParser", "\"document\"* \"parser\"*"));
        // Without the tokenizer, only the whole word matches.
        assert!(!matches("porter unicode61", text, "document"));
    }

    #[test]
    fn options_pass_through_to_unicode61() {
        let t = "porter word_parts remove_diacritics 2";
        assert!(matches(t, "the CaféMenu page", "cafe"));
        assert!(matches(t, "the CaféMenu page", "menu"));
        assert!(matches(t, "the Cafe\u{301}Menu page", "cafe"));
        assert!(matches(t, "an HTTPŚerver", "server"));
        assert!(matches(t, "an HTTPS\u{301}erver", "server"));
    }

    #[test]
    fn snippets_mark_the_word_a_part_matched() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='porter word_parts');
             INSERT INTO t VALUES ('changes to FinancialDocument');",
        )
        .unwrap();
        let hl: String = conn
            .query_row(
                "SELECT highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'document'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hl, "changes to [FinancialDocument]");
    }
}
