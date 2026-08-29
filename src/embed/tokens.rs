//! Exact token counting using the bundled MiniLM WordPiece tokenizer.
//!
//! The embedding model (AllMiniLML6V2) ships a `tokenizer.json`. When it is
//! bundled at build time (see `build.rs`) we embed those bytes in the binary
//! and count tokens exactly; the count then works on any machine the binary is
//! copied to, with no disk path or network round-trip. When the model was not
//! bundled we fall back to a coarse byte-based estimate so callers always get a
//! usable number rather than a hard failure.

use std::sync::OnceLock;
use tokenizers::Tokenizer;

// Embedded at compile time only when the model was bundled. Embedding the bytes
// (rather than reading `.model-cache/` at runtime, as the embedder does) is what
// keeps counting working after the binary is moved off the build machine.
#[cfg(bundled_model)]
const TOKENIZER_JSON: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/.model-cache/tokenizer.json"
));

/// Process-wide tokenizer, loaded once. `None` means "unavailable" (a non-bundled
/// build, or a malformed tokenizer file) and callers fall back to an estimate.
fn tokenizer() -> Option<&'static Tokenizer> {
    static TOK: OnceLock<Option<Tokenizer>> = OnceLock::new();
    TOK.get_or_init(load_tokenizer).as_ref()
}

#[cfg(bundled_model)]
fn load_tokenizer() -> Option<Tokenizer> {
    let mut tok = match Tokenizer::from_bytes(TOKENIZER_JSON) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("failed to load bundled tokenizer, using estimate: {e}");
            return None;
        }
    };
    // The model's tokenizer.json enables padding (to a fixed length) and
    // truncation for *embedding*. For *counting* we want the true token length,
    // so disable both - otherwise every encoding comes back the padded length.
    tok.with_padding(None);
    let _ = tok.with_truncation(None);
    Some(tok)
}

#[cfg(not(bundled_model))]
fn load_tokenizer() -> Option<Tokenizer> {
    None
}

/// Count the tokens in `text` with the MiniLM tokenizer, or a coarse estimate
/// when it is unavailable. Special tokens ([CLS]/[SEP]) are not added - this is
/// a pure content-token count, which is what budgeting wants.
pub fn count_tokens(text: &str) -> usize {
    match tokenizer() {
        Some(tok) => match tok.encode(text, false) {
            Ok(enc) => enc.get_ids().len(),
            Err(e) => {
                log::warn!("tokenization failed, using estimate: {e}");
                estimate_tokens(text)
            }
        },
        None => estimate_tokens(text),
    }
}

/// Coarse fallback: ~4 bytes per token. Only used when the real tokenizer is
/// unavailable; intentionally simple, it just prevents a hard failure.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn counts_are_reasonable() {
        // Holds whether we use the real tokenizer or the /4 estimate.
        let n = count_tokens("hello world");
        assert!((1..=6).contains(&n), "unexpected token count: {n}");
        // Tokens should be well below the byte length for ordinary prose.
        let prose = "The quick brown fox jumps over the lazy dog.";
        assert!(count_tokens(prose) < prose.len());
    }

    #[test]
    fn longer_text_has_more_tokens() {
        let short = count_tokens("one two three");
        let long = count_tokens("one two three four five six seven eight nine ten");
        assert!(long > short);
    }

    #[test]
    fn estimate_is_ceil_of_quarter() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
