//! Registry of the embedding models sclerox knows how to load.
//!
//! Each entry carries everything the loader, the database and the similarity
//! thresholds need to know about a model. The stored `id` is what gets written
//! into a database's `embedding_model` meta key, so it must never change for an
//! existing entry: changing it would make every database embedded with that
//! model look like it needs re-embedding.

use super::download::{Pinned, RepoFile};
use anyhow::{bail, Result};
use fastembed::Pooling;

/// Env var carrying the configured model name from `config::init()` into this
/// settings-free library module. Unset means [`DEFAULT_MODEL`].
pub const MODEL_ENV: &str = "SCLEROX_EMBED_MODEL";

/// Config name of the default model.
pub const DEFAULT_MODEL: &str = "granite-small-r2";

/// Every `*_vec` vec0 table is declared `float[384]`. A model with another
/// dimension would need those tables rebuilt, which is not supported.
pub const SUPPORTED_DIM: usize = 384;

/// Tokenizer files every supported model ships at its repo root.
pub const TOKENIZER_FILES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// Model-specific defaults for the similarity cutoffs. Scores from different
/// models live on different scales (MiniLM puts unrelated text near 0.10,
/// Granite small near 0.75), so a single fixed number cannot serve both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Cosine floor for a semantic search hit to be shown.
    pub semantic: f64,
    /// Cosine score at or above which a distilled memory counts as a
    /// near-duplicate of an existing one.
    pub dedup_cosine: f64,
    /// Cosine score at or above which a distilled memory merges into its best
    /// match automatically. `None` turns score-based merging off: matches are
    /// flagged for review instead.
    pub merge: Option<f64>,
    /// Loose floor for the related memories shown to the distiller as context.
    pub context: f64,
    /// Drop semantic search hits scoring more than this below the best hit for
    /// the same query. For models whose relevant and unrelated scores overlap
    /// across queries, where no fixed `semantic` floor separates them. `None`
    /// = the floor alone decides.
    pub top_margin: Option<f64>,
}

/// The score a semantic hit needs: at least `floor` and, with a `margin`,
/// within `margin` of the best score in `scores`.
pub fn semantic_cutoff(
    scores: impl IntoIterator<Item = f32>,
    floor: f32,
    margin: Option<f32>,
) -> f32 {
    let top = scores.into_iter().fold(f32::NEG_INFINITY, f32::max);
    match margin {
        Some(m) if top.is_finite() => floor.max(top - m),
        _ => floor,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelSpec {
    /// Short name used in `embed.model` in config.toml.
    pub name: &'static str,
    /// Stable identifier recorded in each database. Includes the weights
    /// variant: int8 and full-precision vectors of one model are not comparable.
    pub id: &'static str,
    /// Hugging Face repo the files are downloaded from.
    pub repo: &'static str,
    /// Commit the files are pinned to. Bumping it changes what gets embedded,
    /// so it needs new `files` hashes and, if vectors change, a new `id`.
    pub revision: &'static str,
    /// Every file the model needs (graph, external weights, the
    /// [`TOKENIZER_FILES`]), each with its expected size and sha256.
    pub files: &'static [RepoFile],
    /// ONNX graph, as a path inside the repo.
    pub onnx_file: &'static str,
    /// External weight files the graph references, as paths inside the repo.
    /// The graph references each one by its base name.
    pub external_data: &'static [&'static str],
    pub pooling: Pooling,
    /// Graph output to pool over; `None` lets fastembed pick.
    pub output_name: Option<&'static str>,
    pub dim: usize,
    /// Prepended to search queries before embedding.
    pub query_prefix: &'static str,
    /// Prepended to stored text before embedding.
    pub document_prefix: &'static str,
    pub thresholds: Thresholds,
}

pub const GRANITE_SMALL_R2: ModelSpec = ModelSpec {
    name: "granite-small-r2",
    id: "onnx-community/granite-embedding-small-english-r2-ONNX:model_quantized",
    repo: "onnx-community/granite-embedding-small-english-r2-ONNX",
    revision: "1dc7835ba0cb9c76a3618d0bf0c427c97671b3c8",
    files: &[
        RepoFile {
            path: "onnx/model_quantized.onnx",
            size: 598_902,
            sha256: "a3fad524afc3f060216a8ddbb1ac89c9b6498fba8995b5718bde879076a2e9ba",
        },
        RepoFile {
            path: "onnx/model_quantized.onnx_data",
            size: 51_885_568,
            sha256: "1f4cf47e4adec7f7ae09db03d071ba8667e07f9a4203142c7efa8d37fe453597",
        },
        RepoFile {
            path: "tokenizer.json",
            size: 2_128_614,
            sha256: "feeb83348dcb033bc6b9d2e1f7906ca9eb2d122845000c9416d894d7c2927149",
        },
        RepoFile {
            path: "config.json",
            size: 1_526,
            sha256: "1a1710c20911da8c96179716bf44058e54cca6fa7952cce77be83ef05edae3ee",
        },
        RepoFile {
            path: "special_tokens_map.json",
            size: 694,
            sha256: "ea97ecdbcc73713039d8d64dbb05e3689495c96657fbd9a18f5bed381be81049",
        },
        RepoFile {
            path: "tokenizer_config.json",
            size: 20_839,
            sha256: "ce06781b38bb393db68c9e0709bddd31ef5d88f2c6fbb3fd9f369778fb85e451",
        },
    ],
    onnx_file: "onnx/model_quantized.onnx",
    external_data: &["onnx/model_quantized.onnx_data"],
    // CLS, not mean: mean pooling gives plausible-looking but wrong vectors
    // (every cosine lands around 0.6-0.7).
    pooling: Pooling::Cls,
    output_name: Some("last_hidden_state"),
    dim: 384,
    query_prefix: "",
    document_prefix: "",
    // Granite's scores sit high and close together, and where the bands fall
    // depends on the text. Code search (the eval): correct chunk 0.85 mean,
    // random chunk 0.75 mean, 0.79 at the 95th percentile. Short memories
    // (a hand-made probe, not a calibration): true query matches 0.74-0.81,
    // unrelated 0.65-0.71, so a floor at the code band's 0.79 would drop most
    // real memory hits. `semantic` sits at the memory band's edge instead;
    // hits are still ranked and capped by `search.semantic_limit`.
    //
    // Dedup, on real memory pairs: unrelated pairs score median 0.770 (p99
    // 0.851), so `dedup_cosine` at 0.90 is a safe near-duplicate floor. Real
    // duplicates score 0.953-0.985 but some distinct pairs reach 0.961, so no
    // cutoff is clean. `merge` at 0.97 merges only the most confident
    // duplicates and leaves 0.90-0.97 to `sclerox memory conflicts`.
    //
    // The bands also overlap across queries: "Rust" scores an unrelated
    // "Python for scripts" memory 0.73, above some true matches elsewhere. What
    // does separate them is the gap to the best hit (0.87 there; 0.04-0.14 in
    // every probe), hence `top_margin`.
    thresholds: Thresholds {
        semantic: 0.72,
        dedup_cosine: 0.90,
        merge: Some(0.97),
        context: 0.70,
        top_margin: Some(0.05),
    },
};

pub const MINILM_L6_V2: ModelSpec = ModelSpec {
    name: "minilm-l6-v2",
    id: "Qdrant/all-MiniLM-L6-v2-onnx:model",
    repo: "Qdrant/all-MiniLM-L6-v2-onnx",
    // The weights date from 2024; the tokenizer changed in September 2026
    // (truncation 128 -> 256 tokens, then padding to the batch's longest).
    // Text past 128 tokens embeds differently before and after, but the ID is
    // unchanged: the old vectors are the same model, just truncated earlier,
    // and still compare fine against new ones.
    revision: "d13954661f83248295ba75c1ed411eef3b7b936e",
    files: &[
        RepoFile {
            path: "model.onnx",
            size: 90_387_630,
            sha256: "bbd7b466f6d58e646fdc2bd5fd67b2f5e93c0b687011bd4548c420f7bd46f0c5",
        },
        MINILM_TOKENIZER,
        RepoFile {
            path: "config.json",
            size: 650,
            sha256: "1b4d8e2a3988377ed8b519a31d8d31025a25f1c5f8606998e8014111438efcd7",
        },
        RepoFile {
            path: "special_tokens_map.json",
            size: 695,
            sha256: "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
        },
        RepoFile {
            path: "tokenizer_config.json",
            size: 1_412,
            sha256: "abda01c8c14c5151ae498aceb30db406d6b91242c394fd88d3b6fd5a63a101e6",
        },
    ],
    onnx_file: "model.onnx",
    external_data: &[],
    pooling: Pooling::Mean,
    output_name: None,
    dim: 384,
    query_prefix: "",
    document_prefix: "",
    // Calibrated against real MiniLM scores; see `DedupSettings` in config.rs.
    thresholds: Thresholds {
        semantic: 0.45,
        dedup_cosine: 0.85,
        merge: Some(0.88),
        context: 0.30,
        top_margin: None,
    },
};

const MINILM_TOKENIZER: RepoFile = RepoFile {
    path: "tokenizer.json",
    size: 711_649,
    sha256: "851ca67100d372ca3ae031a6abd168f53489eebfd7d89523f35c5c9b4d372c3c",
};

/// The tokenizer `embed::count_tokens` counts with: MiniLM's, whatever model
/// embeds, so token budgets don't move when `embed.model` changes. The same
/// file MiniLM itself loads, so the two share one cached copy.
pub const TOKEN_COUNTER: Pinned<'static> = Pinned {
    repo: MINILM_L6_V2.repo,
    revision: MINILM_L6_V2.revision,
    files: &[MINILM_TOKENIZER],
};

/// All known models. The first entry is the default.
pub const MODELS: &[ModelSpec] = &[GRANITE_SMALL_R2, MINILM_L6_V2];

/// The registry entry for a config name, or an error naming the valid ones.
pub fn lookup(name: &str) -> Result<&'static ModelSpec> {
    match MODELS.iter().find(|m| m.name == name) {
        Some(m) => Ok(m),
        None => bail!(
            "unknown embedding model '{name}' (known: {})",
            names().join(", ")
        ),
    }
}

/// The registry entry with this stored model ID, if it is one we know.
pub fn by_id(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn names() -> Vec<&'static str> {
    MODELS.iter().map(|m| m.name).collect()
}

pub fn default_model() -> &'static ModelSpec {
    &MODELS[0]
}

/// The model selected by `SCLEROX_EMBED_MODEL` (bridged from `embed.model`),
/// or the default when it is unset or empty.
pub fn configured() -> Result<&'static ModelSpec> {
    match std::env::var(MODEL_ENV) {
        Ok(name) if !name.is_empty() => lookup(&name),
        _ => Ok(default_model()),
    }
}

/// The stored ID of the configured model, falling back to the default model's
/// ID when the configured name is unknown. An unknown name already fails
/// loading the embedder, so nothing gets embedded under it either way.
pub fn configured_id() -> &'static str {
    configured().unwrap_or_else(|_| default_model()).id
}

/// Meta key (in `meta` on the primary database, `repo_meta` in repo
/// databases) recording which model produced that database's vectors.
pub const META_KEY: &str = "embedding_model";

/// How a database's recorded model relates to the configured one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCheck {
    /// Same model: vectors are comparable.
    Match,
    /// No vectors written yet; the first write records the configured model.
    NotRecorded,
    /// Vectors came from another model. Scores against them are meaningless,
    /// so semantic search, dedup and vector writes are off until a rebuild.
    Mismatch { recorded: String },
}

/// Compare a database's recorded model ID against the configured model.
pub fn check(recorded: Option<&str>) -> ModelCheck {
    match recorded {
        None => ModelCheck::NotRecorded,
        Some(id) if id == configured_id() => ModelCheck::Match,
        Some(id) => ModelCheck::Mismatch {
            recorded: id.to_string(),
        },
    }
}

/// Human name for a stored model ID: its config name when known.
pub fn display_id(id: &str) -> &str {
    by_id(id).map_or(id, |m| m.name)
}

/// Whether a database may be searched semantically. Warns (once per process)
/// on a mismatch. A failed lookup also says no: a wrong "yes" mixes models.
pub fn semantic_allowed(label: &str, recorded: anyhow::Result<Option<String>>) -> bool {
    match recorded {
        Ok(r) => match check(r.as_deref()) {
            ModelCheck::Match | ModelCheck::NotRecorded => true,
            ModelCheck::Mismatch { recorded } => {
                warn_mismatch_once(label, &recorded);
                false
            }
        },
        Err(e) => {
            log::warn!("could not read the embedding model of {label}: {e:#}");
            false
        }
    }
}

/// Whether vectors may be written to a database, recording the configured
/// model through `record` if nothing is recorded yet.
pub fn claim_writes(
    label: &str,
    recorded: anyhow::Result<Option<String>>,
    record: impl FnOnce(&str) -> anyhow::Result<()>,
) -> bool {
    match recorded {
        Ok(r) => match check(r.as_deref()) {
            ModelCheck::Match => true,
            ModelCheck::NotRecorded => match record(configured_id()) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("could not record the embedding model of {label}: {e:#}");
                    false
                }
            },
            ModelCheck::Mismatch { recorded } => {
                warn_mismatch_once(label, &recorded);
                false
            }
        },
        Err(e) => {
            log::warn!("could not read the embedding model of {label}: {e:#}");
            false
        }
    }
}

/// Tell the user, once per process, that a database needs re-embedding.
fn warn_mismatch_once(label: &str, recorded: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let msg = format!(
        "{label} was embedded with '{}', but embed.model is '{}'; semantic search \
         and dedup are off for it until you run `sclerox db reembed`",
        display_id(recorded),
        display_id(configured_id()),
    );
    log::warn!("{msg}");
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("warning: {msg}");
    }
}

impl ModelSpec {
    /// Reject models the database cannot store.
    pub fn check_supported(&self) -> Result<()> {
        if self.dim != SUPPORTED_DIM {
            bail!(
                "embedding model '{}' has dimension {}, but sclerox's vector indexes \
                 are fixed at {SUPPORTED_DIM}; pick a {SUPPORTED_DIM}-dimension model",
                self.name,
                self.dim
            );
        }
        Ok(())
    }

    /// The model's files, pinned, for download.
    pub fn pinned(&self) -> Pinned<'static> {
        Pinned {
            repo: self.repo,
            revision: self.revision,
            files: self.files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_granite_small() {
        assert_eq!(DEFAULT_MODEL, "granite-small-r2");
        assert_eq!(default_model().name, DEFAULT_MODEL);
        assert_eq!(lookup(DEFAULT_MODEL).unwrap(), &GRANITE_SMALL_R2);
    }

    #[test]
    fn unknown_name_is_rejected() {
        let err = lookup("bert-giant").unwrap_err().to_string();
        assert!(err.contains("bert-giant"), "{err}");
        assert!(err.contains("granite-small-r2"), "lists valid names: {err}");
    }

    #[test]
    fn non_384_dimension_is_rejected() {
        let wide = ModelSpec {
            name: "wide",
            dim: 768,
            ..GRANITE_SMALL_R2
        };
        let err = wide.check_supported().unwrap_err().to_string();
        assert!(err.contains("768"), "{err}");
        for m in MODELS {
            m.check_supported().unwrap();
        }
    }

    #[test]
    fn names_and_ids_are_unique() {
        for (i, a) in MODELS.iter().enumerate() {
            for b in &MODELS[i + 1..] {
                assert_ne!(a.name, b.name);
                assert_ne!(a.id, b.id);
            }
        }
    }

    #[test]
    fn ids_round_trip() {
        for m in MODELS {
            assert_eq!(by_id(m.id), Some(m));
        }
        assert_eq!(by_id("nope"), None);
    }

    #[test]
    fn check_compares_against_the_configured_model() {
        // Tests run with SCLEROX_EMBED_MODEL unset, so the default is configured.
        assert_eq!(check(None), ModelCheck::NotRecorded);
        assert_eq!(check(Some(GRANITE_SMALL_R2.id)), ModelCheck::Match);
        assert_eq!(
            check(Some(MINILM_L6_V2.id)),
            ModelCheck::Mismatch {
                recorded: MINILM_L6_V2.id.to_string()
            }
        );
    }

    #[test]
    fn claim_writes_records_only_when_nothing_is_recorded() {
        let mut recorded = None;
        assert!(claim_writes("db", Ok(None), |id| {
            recorded = Some(id.to_string());
            Ok(())
        }));
        assert_eq!(recorded.as_deref(), Some(GRANITE_SMALL_R2.id));

        let never = |_: &str| -> anyhow::Result<()> { panic!("must not re-record") };
        assert!(claim_writes(
            "db",
            Ok(Some(GRANITE_SMALL_R2.id.into())),
            never
        ));
        assert!(!claim_writes("db", Ok(Some(MINILM_L6_V2.id.into())), never));
        assert!(!claim_writes("db", Err(anyhow::anyhow!("boom")), never));
    }

    #[test]
    fn semantic_allowed_only_without_a_mismatch() {
        assert!(semantic_allowed("db", Ok(None)));
        assert!(semantic_allowed("db", Ok(Some(GRANITE_SMALL_R2.id.into()))));
        assert!(!semantic_allowed("db", Ok(Some(MINILM_L6_V2.id.into()))));
    }

    #[test]
    fn cutoff_is_the_floor_without_a_margin() {
        assert_eq!(semantic_cutoff([0.9, 0.5], 0.45, None), 0.45);
    }

    #[test]
    fn cutoff_follows_the_top_score_with_a_margin() {
        // "Rust" against a Rust memory and an unrelated Python one.
        let cut = semantic_cutoff([0.872, 0.733], 0.72, Some(0.05));
        assert!(0.733 < cut && cut <= 0.872);
        // A weak best hit never lowers the cutoff below the floor.
        assert_eq!(semantic_cutoff([0.70], 0.72, Some(0.05)), 0.72);
        // No hits: nothing to anchor on.
        assert_eq!(semantic_cutoff([], 0.72, Some(0.05)), 0.72);
    }

    #[test]
    fn every_file_the_loader_reads_is_pinned() {
        for m in MODELS {
            let pinned: Vec<&str> = m.files.iter().map(|f| f.path).collect();
            let mut needed = vec![m.onnx_file];
            needed.extend_from_slice(m.external_data);
            needed.extend_from_slice(&TOKENIZER_FILES);
            for f in &needed {
                assert!(pinned.contains(f), "{}: {f} is not pinned", m.name);
            }
            assert_eq!(pinned.len(), needed.len(), "{}: unused pinned file", m.name);
        }
    }

    #[test]
    fn pins_are_commits_and_hashes() {
        let is_hex = |s: &str, len: usize| {
            s.len() == len
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        for m in MODELS {
            assert!(
                is_hex(m.revision, 40),
                "{}: revision must be a commit",
                m.name
            );
            for f in m.files {
                assert!(
                    is_hex(f.sha256, 64),
                    "{}: bad sha256 for {}",
                    m.name,
                    f.path
                );
                assert!(f.size > 0);
            }
        }
    }
}
