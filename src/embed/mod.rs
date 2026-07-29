use anyhow::Result;

pub mod tokens;
pub use tokens::count_tokens;

/// Path to the pre-downloaded model cache, baked in at compile time.
/// Only set when the model was downloaded during `cargo build`.
#[cfg(bundled_model)]
const MODEL_CACHE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.model-cache");

/// Generate embeddings using fastembed (AllMiniLML6V2, 384 dims).
pub struct Embedder {
    inner: fastembed::TextEmbedding,
}

impl Embedder {
    /// Initialize the embedder.
    ///
    /// When the model was downloaded at build time (`cargo build`), loads from the
    /// compile-time `.model-cache/` path instantly (no network). If that path is
    /// inaccessible (binary moved to another machine), falls back to fastembed's
    /// standard runtime download into `~/.cache/huggingface/`.
    pub fn new() -> Result<Self> {
        #[cfg(bundled_model)]
        if let Ok(inner) = Self::try_load_from_cache() {
            return Ok(Self { inner });
        }

        // Runtime download fallback
        let inner = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2)
                .with_show_download_progress(true),
        )
        .map_err(|e| anyhow::anyhow!("failed to initialize embedder: {e}"))?;
        Ok(Self { inner })
    }

    #[cfg(bundled_model)]
    fn try_load_from_cache() -> anyhow::Result<fastembed::TextEmbedding> {
        use fastembed::{
            InitOptionsUserDefined, Pooling, QuantizationMode, TokenizerFiles,
            UserDefinedEmbeddingModel,
        };
        use std::path::Path;

        let dir = Path::new(MODEL_CACHE_DIR);
        let read = |name: &str| -> anyhow::Result<Vec<u8>> {
            std::fs::read(dir.join(name))
                .map_err(|e| anyhow::anyhow!("model cache missing {name}: {e}"))
        };

        let model_def = UserDefinedEmbeddingModel {
            onnx_file: read("model.onnx")?,
            external_initializers: vec![],
            tokenizer_files: TokenizerFiles {
                tokenizer_file: read("tokenizer.json")?,
                config_file: read("config.json")?,
                special_tokens_map_file: read("special_tokens_map.json")?,
                tokenizer_config_file: read("tokenizer_config.json")?,
            },
            pooling: Some(Pooling::Mean),
            quantization: QuantizationMode::None,
            output_key: None,
        };

        fastembed::TextEmbedding::try_new_from_user_defined(
            model_def,
            InitOptionsUserDefined::new(),
        )
        .map_err(|e| anyhow::anyhow!("failed to load model from cache: {e}"))
    }

    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        log::debug!("embedding {} chars", text.len());
        let mut results = self
            .inner
            .embed(std::slice::from_ref(&text), None)
            .map_err(|e| {
                log::error!("embedding failed: {e}");
                anyhow::anyhow!("embedding failed: {e}")
            })?;
        results
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned empty result"))
    }

    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner
            .embed(texts, None)
            .map_err(|e| anyhow::anyhow!("batch embedding failed: {e}"))
    }
}

/// Chunk text into overlapping windows of ~`max_chars` characters,
/// splitting on paragraph boundaries where possible.
pub fn chunk_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_string()];
    }

    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut last_para = String::new();

    for para in &paragraphs {
        if current.len() + para.len() + 2 > max_chars && !current.is_empty() {
            chunks.push(current.trim().to_string());
            current = if last_para.len() <= overlap_chars {
                format!("{}\n\n", last_para)
            } else {
                let start = last_para.len().saturating_sub(overlap_chars);
                format!("...{}\n\n", &last_para[start..])
            };
        }
        last_para = para.to_string();
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_short_text_is_single() {
        let text = "Hello world";
        let chunks = chunk_text(text, 500, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_chunk_long_text_splits() {
        let para = "A".repeat(300);
        let text = format!("{para}\n\n{para}\n\n{para}\n\n{para}");
        let chunks = chunk_text(&text, 500, 100);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 700, "chunk too large: {}", chunk.len());
        }
    }

    #[test]
    fn test_chunk_preserves_all_content() {
        let text = "Para one.\n\nPara two.\n\nPara three.\n\nPara four.";
        let chunks = chunk_text(text, 20, 5);
        let joined = chunks.join(" ");
        assert!(joined.contains("Para one"));
        assert!(joined.contains("Para four"));
    }
}
