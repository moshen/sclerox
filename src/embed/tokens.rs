//! Exact token counting using MiniLM's WordPiece tokenizer.
//!
//! The tokenizer is pinned and downloaded on first use into the model cache,
//! like the models themselves (see [`super::models::TOKEN_COUNTER`]). When it
//! cannot be had (offline on first run, say) we fall back to a coarse
//! byte-based estimate so callers always get a usable number rather than a
//! hard failure; the next process tries the download again.
//!
//! This is independent of the embedding model (`embed.model`): the budget it
//! enforces is for the LLM's context window, so it does not need to match
//! whichever model produces the vectors, and keeping MiniLM's tokenizer keeps
//! budgets stable when the embedding model changes.

use std::sync::OnceLock;
use tokenizers::Tokenizer;

/// Process-wide tokenizer, loaded once. `None` means "unavailable" (download
/// failed, or a malformed file) and callers fall back to an estimate.
fn tokenizer() -> Option<&'static Tokenizer> {
    static TOK: OnceLock<Option<Tokenizer>> = OnceLock::new();
    TOK.get_or_init(|| match load_tokenizer() {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("token counter unavailable, using estimate: {e:#}");
            None
        }
    })
    .as_ref()
}

/// Download (first use only) and load the token-counting tokenizer.
pub fn load_tokenizer() -> anyhow::Result<Tokenizer> {
    let paths = super::models::TOKEN_COUNTER.fetch("the token-counting tokenizer")?;
    let path = paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("no tokenizer file pinned"))?;
    let mut tok = Tokenizer::from_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    // The model's tokenizer.json enables padding and truncation for
    // *embedding*. For *counting* we want the true token length, so disable
    // both - otherwise long texts come back truncated.
    tok.with_padding(None);
    let _ = tok.with_truncation(None);
    Ok(tok)
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
