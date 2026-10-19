use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub mod device;
pub mod download;
pub mod models;
pub mod server;
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
/// `memory reembed`, `repo reembed`), as bulk work (see [`Embedder::bulk`]).
/// Fails when embeddings are off or the model cannot be loaded.
pub fn required() -> Result<Embedder> {
    if !enabled() {
        anyhow::bail!(
            "embeddings are turned off (embed.enabled = false or {ENABLED_ENV}); \
             turn them on to embed"
        );
    }
    Embedder::new(server::Lane::Bulk)
}

/// The embedder for everything else, where vectors are a bonus on top of
/// keyword search and plain storage. `None` when embeddings are off, when the
/// database being used is `!ready` (its vectors came from another model, see
/// `Database::semantic_ready`), or when the model fails to load - in which
/// case the caller carries on without vectors and the user is told once.
///
/// Uses the model server when `embed.server` allows it (see [`server`]),
/// starting one if needed; otherwise loads the model in-process.
pub fn optional(ready: bool) -> Option<Embedder> {
    optional_in(ready, server::Lane::Query)
}

/// [`optional`], for bulk work (see [`Embedder::new`]).
pub fn optional_bulk(ready: bool) -> Option<Embedder> {
    optional_in(ready, server::Lane::Bulk)
}

fn optional_in(ready: bool, lane: server::Lane) -> Option<Embedder> {
    if !enabled() || !ready {
        return None;
    }
    match Embedder::new(lane) {
        Ok(e) => Some(e),
        Err(e) => {
            let msg = format!("embedding model unavailable, continuing without vectors: {e:#}");
            log::warn!("{msg}");
            eprintln!("warning: {msg}");
            None
        }
    }
}

/// Where an [`Embedder`] runs the model: here, on a device, or in the
/// model server.
enum Backend {
    Local(Box<LocalModel>),
    Server(server::Client),
}

/// Generate embeddings with the configured model (see [`models`]). Obtained
/// through [`required`] or [`optional`], which apply `embed.enabled`.
pub struct Embedder {
    backend: Backend,
    spec: &'static ModelSpec,
    lane: server::Lane,
}

impl Embedder {
    /// The model selected by `embed.model` (bridged through
    /// `SCLEROX_EMBED_MODEL`): through the model server when it is on and
    /// running (or can be started), else loaded here, downloading it on
    /// first use. With [`server::Lane::Bulk`], the server runs other
    /// commands' queries before this one's work and takes turns between bulk
    /// commands, and a model loaded here may go on an accelerator.
    fn new(lane: server::Lane) -> Result<Self> {
        let spec = models::configured()?;
        spec.check_supported()?;
        if server::mode() == server::Mode::Auto {
            if let Some(client) = server::connect_or_start(spec) {
                return Ok(Self {
                    backend: Backend::Server(client),
                    spec,
                    lane,
                });
            }
        }
        Self::with_model(spec, lane)
    }

    /// Load `spec` in-process, downloading its files into the sclerox model
    /// cache (`$XDG_CACHE_HOME/sclerox/models/`) if they are not there yet.
    pub fn with_model(spec: &'static ModelSpec, lane: server::Lane) -> Result<Self> {
        let model = load_model(spec, lane == server::Lane::Bulk)?;
        Ok(Self {
            backend: Backend::Local(Box::new(model)),
            spec,
            lane,
        })
    }

    /// The model this embedder runs.
    pub fn model(&self) -> &'static ModelSpec {
        self.spec
    }

    /// The device the model runs on: here, or in the model server, which
    /// reports it. `None` when the server doesn't say.
    pub fn device(&self) -> Option<String> {
        match &self.backend {
            Backend::Local(model) => Some(model.device().to_string()),
            Backend::Server(_) => server::device_of(self.spec),
        }
    }

    /// Embed text that will be stored and searched against.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        log::debug!("embedding {} chars", text.len());
        let prefixed = format!("{}{text}", self.spec.document_prefix);
        self.embed_single(prefixed)
    }

    /// Embed a search query. Differs from [`Self::embed_one`] only for models
    /// that expect a query prefix.
    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        log::debug!("embedding query of {} chars", text.len());
        let prefixed = format!("{}{text}", self.spec.query_prefix);
        self.embed_single(prefixed)
    }

    fn embed_single(&mut self, text: String) -> Result<Vec<f32>> {
        let mut results = self.run(vec![text]).map_err(|e| {
            log::error!("embedding failed: {e:#}");
            e
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
        self.run(prefixed)
    }

    /// Embed prefixed texts one at a time. When the server exits to give
    /// back memory or for a newer build, go on with the next one from where
    /// it stopped; when it fails, load the model here and carry on without it.
    fn run(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        /// Servers in a row that may be found exiting, with no progress
        /// between them, before giving up on them.
        const RECONNECTS: usize = 5;
        let mut reconnects = 0;
        let mut vectors = Vec::with_capacity(texts.len());
        while let Backend::Server(client) = &mut self.backend {
            let done = vectors.len();
            let e = match client.embed(self.spec, &texts[done..], self.lane, &mut vectors) {
                Ok(()) => return Ok(vectors),
                Err(e) => e,
            };
            if vectors.len() > done {
                reconnects = 0;
            }
            let restarting = e
                .downcast_ref::<server::Stopping>()
                .is_some_and(|s| s.0.restarts());
            if restarting && reconnects < RECONNECTS {
                reconnects += 1;
                log::info!("{e:#}; moving to the next model server");
                // Until the old server stops listening, connecting finds it.
                std::thread::sleep(std::time::Duration::from_millis(100));
                if let Some(client) = server::connect_or_start(self.spec) {
                    self.backend = Backend::Server(client);
                    continue;
                }
            }
            log::warn!("model server failed, loading the model in-process: {e:#}");
            let model = load_model(self.spec, self.lane == server::Lane::Bulk)?;
            self.backend = Backend::Local(Box::new(model));
        }
        let Backend::Local(model) = &mut self.backend else {
            unreachable!("the server backend was just replaced");
        };
        let rest = model.embed(&texts[vectors.len()..])?;
        vectors.extend(rest);
        Ok(vectors)
    }

    /// Embed several texts as one padded batch. Only the reference parity
    /// test uses it; see [`Self::embed_each`] for why nothing else should.
    #[cfg(test)]
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = self.spec.document_prefix;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let Backend::Local(model) = &mut self.backend else {
            anyhow::bail!("batch embedding needs the model in-process");
        };
        model
            .model
            .embed(&prefixed, None)
            .map_err(|e| anyhow::anyhow!("batch embedding failed: {e}"))
    }
}

pub use download::cache_dir as model_cache_dir;

/// Env var carrying `embed.memory_chars` from `config::init()` into this
/// settings-free module. Unset means the configured model's
/// [`ModelSpec::memory_chars`].
pub const MEMORY_CHARS_ENV: &str = "SCLEROX_EMBED_MEMORY_CHARS";

/// How many characters of a memory's value get embedded.
pub fn memory_chars() -> usize {
    let model = models::configured().unwrap_or_else(|_| models::default_model());
    resolve_memory_chars(std::env::var(MEMORY_CHARS_ENV).ok().as_deref(), model)
}

/// [`memory_chars`] for vectors from the model with ID `model_id`, which
/// may not be the configured one: a rebuild run by the model server, say.
pub fn memory_chars_for(model_id: &str) -> usize {
    match models::by_id(model_id) {
        Some(model) => resolve_memory_chars(std::env::var(MEMORY_CHARS_ENV).ok().as_deref(), model),
        None => memory_chars(),
    }
}

/// The env value when it is a positive number, else `model`'s default.
fn resolve_memory_chars(env: Option<&str>, model: &ModelSpec) -> usize {
    env.and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(model.memory_chars)
}

/// The part of a memory's value that gets embedded: its first
/// [`memory_chars`] characters.
pub fn memory_embed_text(value: &str) -> String {
    first_chars(value, memory_chars())
}

fn first_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

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

/// A model loaded in this process, on a device.
pub(crate) struct LocalModel {
    model: fastembed::TextEmbedding,
    device: device::Device,
    spec: &'static ModelSpec,
}

/// Models whose accelerator failed to embed in this process, which load on
/// the CPU from then on.
static FAILED_ON: std::sync::Mutex<Vec<(&str, device::Device)>> = std::sync::Mutex::new(Vec::new());

impl LocalModel {
    pub(crate) fn device(&self) -> device::Device {
        self.device
    }

    /// Embed `texts` one at a time (see [`Embedder::embed_each`]). An
    /// accelerator can start yet fail on some inputs; then the model moves to
    /// the CPU for good and embeds them there.
    pub(crate) fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let failed = match self.model.embed(texts, Some(1)) {
            Ok(vectors) => return Ok(vectors),
            Err(e) => e,
        };
        if self.device == device::Device::Cpu {
            anyhow::bail!("embedding failed: {failed}");
        }
        log::warn!(
            "{} failed on {}, moving it to the cpu: {failed}",
            self.spec.name,
            self.device
        );
        FAILED_ON
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((self.spec.name, self.device));
        let files = ensure_downloaded(self.spec)?;
        self.model = load(self.spec, &files, device::Device::Cpu)
            .with_context(|| format!("failed to load '{}' on the cpu", self.spec.name))?;
        self.device = device::Device::Cpu;
        self.model
            .embed(texts, Some(1))
            .map_err(|e| anyhow::anyhow!("embedding failed: {e}"))
    }
}

/// Load `spec` in this process, downloading its files first if needed, on
/// the first device of `embed.device`'s [`device::candidates`] that starts.
/// `long_lived` is for a model that will embed many texts: the model
/// server's, or bulk work.
pub(crate) fn load_model(spec: &'static ModelSpec, long_lived: bool) -> Result<LocalModel> {
    spec.check_supported()?;
    let mut devices = device::candidates(device::configured(), spec, long_lived)?;
    {
        let failed = FAILED_ON.lock().unwrap_or_else(|e| e.into_inner());
        devices.retain(|d| !failed.contains(&(spec.name, *d)));
    }
    if devices.is_empty() {
        // A named accelerator that failed: only the CPU is left.
        devices.push(device::Device::Cpu);
    }
    let files = ensure_downloaded(spec)?;
    let mut failed = None;
    for device in devices {
        match load(spec, &files, device) {
            Ok(model) => {
                log::info!("loaded {} on {device}", spec.name);
                return Ok(LocalModel {
                    model,
                    device,
                    spec,
                });
            }
            Err(e) => {
                log::warn!("couldn't load {} on {device}: {e:#}", spec.name);
                failed = Some(e);
            }
        }
    }
    let e = failed.unwrap_or_else(|| anyhow::anyhow!("no device to load it on"));
    Err(e.context(format!("failed to load embedding model '{}'", spec.name)))
}

fn load(
    spec: &ModelSpec,
    files: &[(&'static str, PathBuf)],
    device: device::Device,
) -> Result<fastembed::TextEmbedding> {
    load_with(spec, files, device, device.provider())
}

/// [`load`] with `provider` in place of `device`'s own.
fn load_with(
    spec: &ModelSpec,
    files: &[(&'static str, PathBuf)],
    device: device::Device,
    provider: Option<fastembed::ExecutionProviderDispatch>,
) -> Result<fastembed::TextEmbedding> {
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
    let mut onnx = read(spec.onnx_file)?;
    if device == device::Device::CoreMl {
        // Name the compiled model after the graph and weights it was
        // compiled from.
        let hash = |path: &str| {
            let file = spec.files.iter().find(|f| f.path == path);
            file.map_or("", |f| &f.sha256[..16])
        };
        let weights = spec.external_data.first().map_or("", |w| hash(w));
        let key = format!("{}{}{weights}", spec.name, hash(spec.onnx_file));
        onnx = device::with_coreml_cache_key(onnx, &key);
    }
    let mut model_def = UserDefinedEmbeddingModel::new(
        onnx,
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

    let mut options = InitOptionsUserDefined::new().with_max_length(spec.max_tokens);
    if let Some(ep) = provider {
        options = options.with_execution_providers(vec![ep]);
    }
    fastembed::TextEmbedding::try_new_from_user_defined(model_def, options)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Bump when [`chunk_text`] changes how it cuts text, so databases re-chunk
/// their stored text on the next `sclerox db reembed`.
const TEXT_CHUNKER_VERSION: u32 = 1;

/// Meta key recording which chunker cut a database's meeting, todo and
/// research chunks. Absent with chunks stored: cut before it was recorded.
pub const TEXT_CHUNKER_KEY: &str = "text_chunker";

/// The signature of [`chunk_text`] at these sizes, as recorded under
/// [`TEXT_CHUNKER_KEY`].
pub fn text_chunker_signature(max_chars: usize, overlap_chars: usize) -> String {
    format!("v{TEXT_CHUNKER_VERSION}:size={max_chars}:overlap={overlap_chars}")
}

/// Chunk text into overlapping windows of at most `max_chars` characters,
/// splitting on paragraph boundaries where possible. A paragraph longer than
/// `max_chars` is cut at a line break or space near the limit (mid-word only
/// when it has neither). Each chunk after the first opens with up to
/// `overlap_chars` of the previous paragraph's tail, shortened when the next
/// paragraph leaves less room.
pub fn chunk_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut last_para = "";

    let paras = text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .flat_map(|p| split_long(p, max_chars));
    for para in paras {
        let para_len = para.chars().count();
        if !current.is_empty() && current.chars().count() + 2 + para_len > max_chars {
            chunks.push(current.trim().to_string());
            // Room left beside this paragraph, less the separator.
            let room = max_chars.saturating_sub(para_len + 2);
            current = overlap_tail(last_para, overlap_chars, room);
        }
        last_para = para;
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

/// Cut `para` into pieces of at most `max_chars` characters, each ending at
/// the last line break or space inside the limit when there is one.
fn split_long(para: &str, max_chars: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = para;
    while let Some((end, _)) = rest.char_indices().nth(max_chars) {
        let window = &rest[..end];
        let cut = match window.rfind(['\n', ' ']) {
            Some(i) if i > 0 => i + 1,
            _ => end,
        };
        let piece = rest[..cut].trim_end();
        if !piece.is_empty() {
            pieces.push(piece);
        }
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

/// The tail of `para` carried into the next chunk: all of it when it fits in
/// `overlap_chars`, otherwise its last `overlap_chars` characters after "...".
/// Nothing when that would not fit in `room` characters.
fn overlap_tail(para: &str, overlap_chars: usize, room: usize) -> String {
    let len = para.chars().count();
    if overlap_chars == 0 || len == 0 {
        return String::new();
    }
    if len <= overlap_chars && len <= room {
        return para.to_string();
    }
    // "..." plus the kept characters must fit in `room`.
    let keep = overlap_chars.min(len).min(room.saturating_sub(3));
    if keep == 0 {
        return String::new();
    }
    let start = para
        .char_indices()
        .nth(len - keep)
        .map_or(para.len(), |(i, _)| i);
    format!("...{}", &para[start..])
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
        let mut e = Embedder::with_model(&models::GRANITE_SMALL_R2, server::Lane::Query).unwrap();
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

    /// Full-precision Granite ranks like the int8 reference and scores a
    /// little higher (see its thresholds in `models`).
    #[test]
    #[ignore = "downloads Granite small at full precision (~195 MB) into the model cache"]
    fn granite_fp32_is_close_to_int8() {
        let mut e =
            Embedder::with_model(&models::GRANITE_SMALL_R2_FP32, server::Lane::Query).unwrap();
        let queries = ["Who made the song My achy breaky heart?", "summit define"];
        let passages = [
            "Achy Breaky Heart is a country song written by Don Von Tress. Originally \
             titled Don't Tell My Heart and performed by The Marcy Brothers in 1991.",
            "Definition of summit for English Language Learners. : 1 the highest point \
             of a mountain : the top of a mountain. : 2 the highest level.",
        ];
        let reference = [[0.888, 0.638], [0.686, 0.826]];
        let q = e.embed_each(&queries).unwrap();
        let p = e.embed_each(&passages).unwrap();
        for (i, qv) in q.iter().enumerate() {
            for (j, pv) in p.iter().enumerate() {
                let got = cosine(qv, pv);
                eprintln!("query {i} / passage {j}: {got:.4}");
                assert!(
                    (got - reference[i][j]).abs() <= 0.03,
                    "query {i} / passage {j}: got {got:.4}, int8 reference {}",
                    reference[i][j]
                );
            }
        }
    }

    /// Which CoreML settings run full-precision Granite over texts of
    /// varying length, how fast, and how close to the CPU. Run on an Apple
    /// Silicon Mac:
    ///   cargo test --release --lib coreml_variants -- --ignored --nocapture
    #[test]
    #[ignore = "diagnostic: needs an Apple Silicon Mac and Granite fp32 (~195 MB)"]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn coreml_variants() {
        use ort::ep::coreml::{ComputeUnits, ModelFormat};
        use std::time::Instant;

        let spec = &models::GRANITE_SMALL_R2_FP32;
        let files = ensure_downloaded(spec).unwrap();
        // Short and long, mixed, since the failure is in resizing between
        // sequence lengths. About 4 characters per token.
        let sentence = "The quick brown fox jumps over the lazy dog near the river bank. ";
        let texts: Vec<String> = [16, 600, 64, 4000, 8, 1500, 7800, 200, 32]
            .iter()
            .map(|&chars| sentence.repeat(chars / sentence.len() + 1)[..chars].to_string())
            .collect();

        let started = Instant::now();
        let mut cpu = load(spec, &files, device::Device::Cpu).unwrap();
        let reference = cpu.embed(&texts, Some(1)).unwrap();
        let cpu_load_and_run = started.elapsed();
        let started = Instant::now();
        cpu.embed(&texts, Some(1)).unwrap();
        eprintln!(
            "cpu: {:.2}s for {} texts (first run with load {:.2}s)",
            started.elapsed().as_secs_f64(),
            texts.len(),
            cpu_load_and_run.as_secs_f64()
        );

        let formats = [
            ("MLProgram", ModelFormat::MLProgram),
            ("NeuralNetwork", ModelFormat::NeuralNetwork),
        ];
        let units = [
            ("All", ComputeUnits::All),
            ("CPUAndGPU", ComputeUnits::CPUAndGPU),
            ("CPUAndNeuralEngine", ComputeUnits::CPUAndNeuralEngine),
        ];
        for (format_name, format) in formats {
            for (units_name, unit) in units {
                let cache = tempfile::TempDir::new().unwrap();
                let ep = ort::ep::CoreML::default()
                    .with_model_format(format)
                    .with_compute_units(unit)
                    .with_model_cache_dir(cache.path().display())
                    .build()
                    .error_on_failure();
                let name = format!("{format_name} + {units_name}");
                let started = Instant::now();
                let mut model = match load_with(spec, &files, device::Device::CoreMl, Some(ep)) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("{name}: load failed: {e:#}");
                        continue;
                    }
                };
                let loaded = started.elapsed();
                // Twice: the first run may compile per shape.
                let mut results = Vec::new();
                for _ in 0..2 {
                    let started = Instant::now();
                    let mut failures = Vec::new();
                    let mut worst = 1.0f32;
                    for (text, cpu_vector) in texts.iter().zip(&reference) {
                        match model.embed(std::slice::from_ref(text), Some(1)) {
                            Ok(v) => worst = worst.min(cosine(&v[0], cpu_vector)),
                            Err(e) => failures.push(format!("{} chars: {e}", text.len())),
                        }
                    }
                    results.push((started.elapsed(), failures, worst));
                }
                let (first, first_failures, _) = &results[0];
                let (second, failures, worst) = &results[1];
                eprintln!(
                    "{name}: load {:.2}s, run {:.2}s then {:.2}s, failed {} then {} of {}, \
                     lowest cosine to cpu {worst:.4}",
                    loaded.as_secs_f64(),
                    first.as_secs_f64(),
                    second.as_secs_f64(),
                    first_failures.len(),
                    failures.len(),
                    texts.len()
                );
                for f in first_failures.iter().chain(failures).take(3) {
                    eprintln!("    {f}");
                }
            }
        }
    }

    #[test]
    #[ignore = "downloads MiniLM (~90 MB) into the model cache"]
    fn minilm_still_loads() {
        let mut e = Embedder::with_model(&models::MINILM_L6_V2, server::Lane::Query).unwrap();
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
    fn memory_chars_take_a_positive_env_value_else_the_model_default() {
        let (granite, minilm) = (&models::GRANITE_SMALL_R2, &models::MINILM_L6_V2);
        assert_eq!(resolve_memory_chars(None, granite), 4000);
        assert_eq!(resolve_memory_chars(None, minilm), 800);
        assert_eq!(resolve_memory_chars(Some("1500"), granite), 1500);
        for bad in ["0", "lots", ""] {
            assert_eq!(resolve_memory_chars(Some(bad), granite), 4000, "{bad:?}");
        }
    }

    #[test]
    fn memory_text_is_cut_in_chars() {
        let text = first_chars(&"é".repeat(5000), 4000);
        assert_eq!(text.chars().count(), 4000);
        assert_eq!(first_chars("short", 4000), "short");
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
            assert!(chunk.len() <= 500, "chunk too large: {}", chunk.len());
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

    #[test]
    fn test_chunk_overlap_cuts_on_char_boundaries() {
        // The overlap cut used to slice bytes and panic inside "é".
        let text = format!("{}\n\n{}", "é".repeat(300), "b".repeat(700));
        let chunks = chunk_text(&text, 800, 201);
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 800, "{}", chunk.chars().count());
        }
        assert!(chunks[1].starts_with("...é"));
    }

    #[test]
    fn test_chunk_splits_one_long_paragraph() {
        let text = "word ".repeat(2000);
        let chunks = chunk_text(text.trim_end(), 800, 200);
        assert!(chunks.len() >= 13, "{} chunks", chunks.len());
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 800, "{}", chunk.chars().count());
            // Cut at spaces, so every word survives whole.
            let body = chunk.trim_start_matches("...");
            assert!(body
                .split_whitespace()
                .all(|w| w == "word" || "word".ends_with(w)));
        }
    }

    #[test]
    fn test_chunk_cuts_mid_word_only_without_spaces() {
        let text = "x".repeat(2500);
        let chunks = chunk_text(&text, 1000, 0);
        let lens: Vec<usize> = chunks.iter().map(String::len).collect();
        assert_eq!(lens, [1000, 1000, 500]);
    }

    #[test]
    fn test_chunk_never_exceeds_max_with_overlap() {
        let paras: Vec<String> = (1..40).map(|i| "ab ".repeat(i * 7)).collect();
        let text = paras.join("\n\n");
        for (max, overlap) in [(100, 30), (300, 200), (800, 200), (50, 100)] {
            for chunk in chunk_text(&text, max, overlap) {
                assert!(
                    chunk.chars().count() <= max,
                    "{max}/{overlap}: {}",
                    chunk.len()
                );
            }
        }
    }

    #[test]
    fn test_chunk_skips_blank_paragraphs() {
        let chunks = chunk_text(" \n\nabcdefgh", 5, 0);
        assert_eq!(chunks, ["abcde", "fgh"]);
    }

    #[test]
    fn test_chunk_cuts_at_the_last_separator() {
        // The space after "dd" is later than the line break, so the first
        // piece keeps as much as fits.
        let chunks = chunk_text("a\nbb cc dd ee ff", 12, 0);
        assert_eq!(chunks, ["a\nbb cc dd", "ee ff"]);
    }

    #[test]
    fn test_chunk_zero_overlap_carries_nothing() {
        let text = format!("{}\n\n{}", "a".repeat(30), "b".repeat(30));
        assert_eq!(chunk_text(&text, 40, 0), ["a".repeat(30), "b".repeat(30)]);
    }
}
