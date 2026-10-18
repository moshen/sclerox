//! Where the embedding model runs: the CPU, or an accelerator onnxruntime can
//! hand the model to (an execution provider).
//!
//! Which accelerators exist depends on the platform: CoreML on Apple Silicon
//! Macs, DirectML on Windows. `embed.device` picks one, or `auto`. Starting an
//! accelerator costs more than embedding one search query on the CPU, so
//! `auto` uses one only where that cost is paid once: in the model server, or
//! for bulk work; see [`candidates`].

use anyhow::{Context, Result};
use fastembed::ExecutionProviderDispatch;

/// Env var carrying `embed.device` from `config::init()` into this
/// settings-free module. Unset means [`Choice::Auto`].
pub const DEVICE_ENV: &str = "SCLEROX_EMBED_DEVICE";

/// Something the model can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    CoreMl,
    DirectMl,
}

impl Device {
    /// Accelerators in the order `auto` tries them.
    const ACCELERATORS: [Device; 2] = [Device::CoreMl, Device::DirectMl];

    /// The name `embed.device` takes.
    pub fn name(self) -> &'static str {
        match self {
            Device::Cpu => "cpu",
            Device::CoreMl => "coreml",
            Device::DirectMl => "directml",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [Device::Cpu]
            .into_iter()
            .chain(Self::ACCELERATORS)
            .find(|d| d.name() == name)
    }

    /// Whether sclerox can run the model on it on this platform. CoreML
    /// only on Apple Silicon: on Intel Macs it would mostly fall back to the
    /// CPU.
    pub fn built_in(self) -> bool {
        match self {
            Device::Cpu => true,
            Device::CoreMl => cfg!(all(target_os = "macos", target_arch = "aarch64")),
            Device::DirectMl => cfg!(windows),
        }
    }

    /// What it needs, for the error when this platform lacks it.
    fn requirement(self) -> &'static str {
        match self {
            Device::Cpu => "",
            Device::CoreMl => "an Apple Silicon Mac",
            Device::DirectMl => "Windows",
        }
    }

    /// The execution provider to register, set to fail session creation
    /// rather than quietly fall back to the CPU. `None` for the CPU, which
    /// onnxruntime always has.
    pub(crate) fn provider(self) -> Option<ExecutionProviderDispatch> {
        match self {
            Device::Cpu => None,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Device::CoreMl => {
                // The compiled model is cached there, under the key
                // `with_coreml_cache_key` gives it, so only the first load
                // pays for compiling it.
                let cache = super::model_cache_dir().join("coreml");
                if let Err(e) = std::fs::create_dir_all(&cache) {
                    log::warn!("couldn't create {}: {e}", cache.display());
                }
                Some(
                    // MLProgram covers more of a transformer's operators than
                    // the older NeuralNetwork format.
                    ort::ep::CoreML::default()
                        .with_model_format(ort::ep::coreml::ModelFormat::MLProgram)
                        .with_model_cache_dir(cache.display())
                        .build()
                        .error_on_failure(),
                )
            }
            #[cfg(windows)]
            Device::DirectMl => Some(
                // A hardware GPU, the fastest when there are several; never
                // the software adapter Windows has without one.
                ort::ep::DirectML::default()
                    .with_device_filter(ort::ep::directml::DeviceFilter::Gpu)
                    .with_performance_preference(
                        ort::ep::directml::PerformancePreference::HighPerformance,
                    )
                    .build()
                    .error_on_failure(),
            ),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl std::fmt::Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The `embed.device` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// An accelerator this platform has, where it pays off; else the CPU.
    Auto,
    /// This device for everything, or an error.
    Only(Device),
}

/// Read an `embed.device` value: `auto` or a [`Device::name`], any case.
/// `None` for anything else.
pub fn parse_choice(v: &str) -> Option<Choice> {
    match v.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Choice::Auto),
        name => Device::from_name(name).map(Choice::Only),
    }
}

/// The configured choice. An unreadable value counts as `auto`; settings
/// warn about it when they load.
pub fn configured() -> Choice {
    std::env::var(DEVICE_ENV)
        .ok()
        .and_then(|v| parse_choice(&v))
        .unwrap_or(Choice::Auto)
}

/// The accelerators this platform has, in the order `auto` tries them.
pub fn built_in_accelerators() -> Vec<Device> {
    Device::ACCELERATORS
        .into_iter()
        .filter(|d| d.built_in())
        .collect()
}

/// The devices to load `model` on, in order. `long_lived` is for a model
/// that will embed many texts (the model server's, or bulk work), where an
/// accelerator's startup cost is paid once. `auto` accelerates only such a
/// model, and only one that runs well on an accelerator (see
/// [`super::ModelSpec::accelerates`]). Errors when `choice` names a device
/// this platform lacks.
pub fn candidates(
    choice: Choice,
    model: &super::ModelSpec,
    long_lived: bool,
) -> Result<Vec<Device>> {
    match choice {
        Choice::Only(d) if !d.built_in() => anyhow::bail!(
            "embed.device = {d}, but running on {d} needs {}",
            d.requirement()
        ),
        Choice::Only(d) => Ok(vec![d]),
        Choice::Auto if long_lived && model.accelerates => {
            let mut devices = built_in_accelerators();
            devices.push(Device::Cpu);
            Ok(devices)
        }
        Choice::Auto => Ok(vec![Device::Cpu]),
    }
}

/// The accelerator a model would run on here: the configured one, or with
/// `auto` the first this platform has that starts. `None` for the CPU. Run
/// by `sclerox install` to pick the default model.
pub fn detect(choice: Choice) -> Option<Device> {
    let tried = match choice {
        Choice::Only(Device::Cpu) => return None,
        Choice::Only(d) => vec![d],
        Choice::Auto => built_in_accelerators(),
    };
    tried.into_iter().find(|&d| match probe(d) {
        Ok(()) => true,
        Err(e) => {
            log::info!("{d} is unavailable: {e:#}");
            false
        }
    })
}

/// Start `device` on a one-operator model: fails when onnxruntime can't use
/// it here (no GPU, say).
pub fn probe(device: Device) -> Result<()> {
    if !device.built_in() {
        anyhow::bail!("needs {}", device.requirement());
    }
    let builder = ort::session::Session::builder().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut builder = match device.provider() {
        Some(ep) => builder
            .with_execution_providers([ep])
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        None => builder,
    };
    builder
        .commit_from_memory(&probe_model())
        .map(drop)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("failed to start {device}"))
}

/// An ONNX model computing `y = relu(x)` for one float, encoded by hand so
/// probing needs no file.
fn probe_model() -> Vec<u8> {
    let tensor = |name: &str| {
        let dim = message(1, &varint_field(1, 1));
        let shape = message(2, &dim);
        let tensor_type = [varint_field(1, 1), shape].concat(); // FLOAT
        let ty = message(2, &message(1, &tensor_type));
        [string_field(1, name), ty].concat()
    };
    let node = [
        string_field(1, "x"),
        string_field(2, "y"),
        string_field(4, "Relu"),
    ]
    .concat();
    let graph = [
        message(1, &node),
        string_field(2, "probe"),
        message(11, &tensor("x")),
        message(12, &tensor("y")),
    ]
    .concat();
    [
        varint_field(1, 7),               // ir_version
        message(7, &graph),               // graph
        message(8, &varint_field(2, 13)), // opset_import: default domain, 13
    ]
    .concat()
}

/// `onnx` with a cache key for CoreML's compiled-model cache added to its
/// metadata. onnxruntime caches a model loaded from memory only under such
/// a key, which must be at most 64 letters and digits. `key` names the
/// model's weights, so a model changes key when they change.
pub(crate) fn with_coreml_cache_key(mut onnx: Vec<u8>, key: &str) -> Vec<u8> {
    let key: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(64)
        .collect();
    // Serialized protobuf messages merge when concatenated, so appending a
    // `metadata_props` entry (field 14) adds it.
    let entry = [string_field(1, "COREML_CACHE_KEY"), string_field(2, &key)].concat();
    onnx.extend(message(14, &entry));
    onnx
}

fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn varint_field(field: u64, value: u64) -> Vec<u8> {
    [varint(field << 3), varint(value)].concat()
}

fn message(field: u64, body: &[u8]) -> Vec<u8> {
    [
        varint(field << 3 | 2),
        varint(body.len() as u64),
        body.to_vec(),
    ]
    .concat()
}

fn string_field(field: u64, s: &str) -> Vec<u8> {
    message(field, s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::models::{GRANITE_SMALL_R2, GRANITE_SMALL_R2_FP32};

    #[test]
    fn parses_auto_and_device_names() {
        assert_eq!(parse_choice("auto"), Some(Choice::Auto));
        assert_eq!(parse_choice(" CoreML "), Some(Choice::Only(Device::CoreMl)));
        assert_eq!(
            parse_choice("directml"),
            Some(Choice::Only(Device::DirectMl))
        );
        assert_eq!(parse_choice("cpu"), Some(Choice::Only(Device::Cpu)));
        assert_eq!(parse_choice("cuda"), None);
        assert_eq!(parse_choice(""), None);
    }

    #[test]
    fn auto_accelerates_only_long_lived_full_precision_models() {
        let fp32 = &GRANITE_SMALL_R2_FP32;
        assert_eq!(
            candidates(Choice::Auto, fp32, false).unwrap(),
            [Device::Cpu]
        );
        let long = candidates(Choice::Auto, fp32, true).unwrap();
        assert_eq!(long.last(), Some(&Device::Cpu));
        assert_eq!(long[..long.len() - 1], built_in_accelerators()[..]);
        // The int8 model's operators mostly have no accelerator kernels.
        assert_eq!(
            candidates(Choice::Auto, &GRANITE_SMALL_R2, true).unwrap(),
            [Device::Cpu]
        );
    }

    #[test]
    fn a_named_device_is_used_for_everything() {
        let cpu = Choice::Only(Device::Cpu);
        let fp32 = &GRANITE_SMALL_R2_FP32;
        assert_eq!(candidates(cpu, fp32, true).unwrap(), [Device::Cpu]);
        assert_eq!(candidates(cpu, fp32, false).unwrap(), [Device::Cpu]);
        for d in built_in_accelerators() {
            assert_eq!(
                candidates(Choice::Only(d), &GRANITE_SMALL_R2, false).unwrap(),
                [d]
            );
        }
    }

    #[test]
    fn a_device_the_platform_lacks_is_an_error() {
        let missing = Device::ACCELERATORS
            .into_iter()
            .find(|d| !d.built_in())
            .expect("no platform has every accelerator");
        let err = candidates(Choice::Only(missing), &GRANITE_SMALL_R2, true).unwrap_err();
        assert!(err.to_string().contains(missing.requirement()), "{err}");
        assert!(probe(missing).is_err());
        assert_eq!(detect(Choice::Only(missing)), None);
    }

    #[test]
    fn the_probe_model_runs_on_the_cpu() {
        probe(Device::Cpu).unwrap();
        assert_eq!(detect(Choice::Only(Device::Cpu)), None);
    }

    #[test]
    fn a_cache_key_leaves_the_model_loadable() {
        let onnx = with_coreml_cache_key(probe_model(), "granite fp32: 86a3a705d459");
        ort::session::Session::builder()
            .unwrap()
            .commit_from_memory(&onnx)
            .unwrap();
        let tail = String::from_utf8_lossy(&onnx[onnx.len() - 40..]).into_owned();
        assert!(tail.ends_with("granitefp3286a3a705d459"), "{tail}");
    }
}
