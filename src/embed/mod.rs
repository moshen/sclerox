use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub mod download;
pub mod models;
pub mod tokens;
pub use models::ModelSpec;
pub use tokens::count_tokens;

/// Env var carrying `embed.enabled` from `config::init()` into this
/// settings-free module. Unset means enabled; see [`parse_switch`].
pub const ENABLED_ENV: &str = "SCLEROX_EMBED_ENABLED";

/// Read an on/off value: `1`/`true`/`on`/`yes` or `0`/`false`/`off`/`no`,
/// any case. `None` for anything else.
pub fn parse_switch(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Whether embeddings are on (`embed.enabled`). When off, nothing loads the
/// model: no vectors are written and searches are keyword-only.
pub fn enabled() -> bool {
    std::env::var(ENABLED_ENV)
        .ok()
        .and_then(|v| parse_switch(&v))
        .unwrap_or(true)
}

/// The embedder for a command whose whole job is embedding (`db reembed`,
/// `memory reembed`, `repo reembed`). Fails when embeddings are off or the
/// model cannot be loaded.
pub fn required() -> Result<Embedder> {
    if !enabled() {
        anyhow::bail!(
            "embeddings are turned off (embed.enabled = false or {ENABLED_ENV}); \
             turn them on to embed"
        );
    }
    Embedder::new()
}

/// The embedder for everything else, where vectors are a bonus on top of
/// keyword search and plain storage. `None` when embeddings are off, when the
/// database being used is `!ready` (its vectors came from another model, see
/// `Database::semantic_ready`), or when the model fails to load - in which
/// case the caller carries on without vectors and the user is told once.
pub fn optional(ready: bool) -> Option<Embedder> {
    if !enabled() || !ready {
        return None;
    }
    match Embedder::new() {
        Ok(e) => Some(e),
        Err(e) => {
            let msg = format!("embedding model unavailable, continuing without vectors: {e:#}");
            log::warn!("{msg}");
            eprintln!("warning: {msg}");
            None
        }
    }
}

/// Generate embeddings with the configured model (see [`models`]). Obtained
/// through [`required`] or [`optional`], which apply `embed.enabled`.
pub struct Embedder {
    inner: fastembed::TextEmbedding,
    spec: &'static ModelSpec,
}

impl Embedder {
    /// Load the model selected by `embed.model` (bridged through
    /// `SCLEROX_EMBED_MODEL`), downloading it on first use.
    fn new() -> Result<Self> {
        Self::with_model(models::configured()?)
    }

    /// Load `spec`, downloading its files into the sclerox model cache
    /// (`$XDG_CACHE_HOME/sclerox/models/`) if they are not there yet.
    pub fn with_model(spec: &'static ModelSpec) -> Result<Self> {
        spec.check_supported()?;
        let files = ensure_downloaded(spec)?;
        let inner = load(spec, &files)
            .with_context(|| format!("failed to load embedding model '{}'", spec.name))?;
        Ok(Self { inner, spec })
    }

    /// The model this embedder runs.
    pub fn model(&self) -> &'static ModelSpec {
        self.spec
    }

    /// Embed text that will be stored and searched against.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        log::debug!("embedding {} chars", text.len());
        let prefixed = format!("{}{text}", self.spec.document_prefix);
        self.embed_single(&prefixed)
    }

    /// Embed a search query. Differs from [`Self::embed_one`] only for models
    /// that expect a query prefix.
    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        log::debug!("embedding query of {} chars", text.len());
        let prefixed = format!("{}{text}", self.spec.query_prefix);
        self.embed_single(&prefixed)
    }

    fn embed_single(&mut self, text: &str) -> Result<Vec<f32>> {
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

    /// Embed stored texts one at a time, for bulk work over many chunks.
    ///
    /// onnxruntime pads a batch to its longest text and keeps an arena sized
    /// for the largest batch, so batching costs memory without saving time.
    /// Batching also makes a vector depend on its neighbours, since the int8
    /// graph quantizes over the whole padded batch; this matches `embed_one`.
    pub fn embed_each(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = self.spec.document_prefix;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.inner
            .embed(&prefixed, Some(1))
            .map_err(|e| anyhow::anyhow!("embedding failed: {e}"))
    }

    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = self.spec.document_prefix;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.inner
            .embed(&prefixed, None)
            .map_err(|e| anyhow::anyhow!("batch embedding failed: {e}"))
    }
}

pub use download::cache_dir as model_cache_dir;

/// Whether every file `spec` needs is already in the model cache.
pub fn is_downloaded(spec: &ModelSpec) -> bool {
    spec.pinned().is_cached()
}

/// Make sure every file `spec` needs is in the model cache, downloading and
/// verifying any that are missing, and return their local paths keyed by repo
/// path.
pub fn ensure_downloaded(spec: &ModelSpec) -> Result<Vec<(&'static str, PathBuf)>> {
    let paths = spec
        .pinned()
        .fetch(&format!("embedding model '{}'", spec.name))?;
    Ok(spec.files.iter().map(|f| f.path).zip(paths).collect())
}

fn load(spec: &ModelSpec, files: &[(&'static str, PathBuf)]) -> Result<fastembed::TextEmbedding> {
    use fastembed::{
        InitOptionsUserDefined, OutputKey, QuantizationMode, TokenizerFiles,
        UserDefinedEmbeddingModel,
    };

    let read = |name: &str| -> Result<Vec<u8>> {
        let path = files
            .iter()
            .find(|(f, _)| *f == name)
            .map(|(_, p)| p)
            .with_context(|| format!("model file {name} was not downloaded"))?;
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))
    };

    let [tokenizer, config, special_tokens_map, tokenizer_config] = models::TOKENIZER_FILES;
    let mut model_def = UserDefinedEmbeddingModel::new(
        read(spec.onnx_file)?,
        TokenizerFiles {
            tokenizer_file: read(tokenizer)?,
            config_file: read(config)?,
            special_tokens_map_file: read(special_tokens_map)?,
            tokenizer_config_file: read(tokenizer_config)?,
        },
    )
    .with_pooling(spec.pooling.clone())
    .with_quantization(QuantizationMode::None);
    model_def.output_key = spec.output_name.map(OutputKey::ByName);

    // The graph names its external weights by base name; that is the name
    // onnxruntime must find them under.
    for data in spec.external_data {
        let graph_name = Path::new(data)
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("bad external data path {data}"))?;
        model_def = model_def.with_external_initializer(graph_name.to_string(), read(data)?);
    }

    fastembed::TextEmbedding::try_new_from_user_defined(model_def, InitOptionsUserDefined::new())
        .map_err(|e| anyhow::anyhow!("{e}"))
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

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (norm(a) * norm(b))
    }

    /// Granite through fastembed must match onnxruntime in Python on the same
    /// int8 model. Cosines all near 0.6-0.7 would mean mean pooling crept in
    /// where CLS belongs.
    ///
    /// Queries and passages are each embedded as one batch, as the reference
    /// was. The int8 graph quantizes activations dynamically, with a scale
    /// taken over the whole padded batch, so the same text embedded alone
    /// comes out slightly different (~0.995 cosine) and drifts past the
    /// tolerance.
    #[test]
    #[ignore = "downloads Granite small (~52 MB) into the model cache"]
    fn granite_parity_with_reference() {
        let mut e = Embedder::with_model(&models::GRANITE_SMALL_R2).unwrap();
        let queries = ["Who made the song My achy breaky heart?", "summit define"];
        let passages = [
            "Achy Breaky Heart is a country song written by Don Von Tress. Originally \
             titled Don't Tell My Heart and performed by The Marcy Brothers in 1991.",
            "Definition of summit for English Language Learners. : 1 the highest point \
             of a mountain : the top of a mountain. : 2 the highest level.",
        ];
        let reference = [[0.888, 0.638], [0.686, 0.826]];

        let q = e.embed_batch(&queries).unwrap();
        let p = e.embed_batch(&passages).unwrap();
        for (i, qv) in q.iter().enumerate() {
            assert_eq!(qv.len(), 384);
            for (j, pv) in p.iter().enumerate() {
                let got = cosine(qv, pv);
                assert!(
                    (got - reference[i][j]).abs() <= 0.005,
                    "query {i} / passage {j}: got {got:.4}, reference {}",
                    reference[i][j]
                );
            }
        }
    }

    #[test]
    #[ignore = "downloads MiniLM (~90 MB) into the model cache"]
    fn minilm_still_loads() {
        let mut e = Embedder::with_model(&models::MINILM_L6_V2).unwrap();
        let a = e.embed_one("the cat sat on the mat").unwrap();
        let b = e.embed_one("a cat was sitting on a mat").unwrap();
        let c = e.embed_one("quarterly revenue projections").unwrap();
        assert_eq!(a.len(), 384);
        assert!(cosine(&a, &b) > cosine(&a, &c));
    }

    #[test]
    fn switch_values() {
        for on in ["1", "true", "ON", " yes "] {
            assert_eq!(parse_switch(on), Some(true), "{on}");
        }
        for off in ["0", "false", "Off", "NO"] {
            assert_eq!(parse_switch(off), Some(false), "{off}");
        }
        assert_eq!(parse_switch("maybe"), None);
    }

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
