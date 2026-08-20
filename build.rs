use std::path::PathBuf;
use std::process::Command;

const MODEL_REPO: &str = "Qdrant/all-MiniLM-L6-v2-onnx";
const FILES: &[&str] = &[
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SKIP_MODEL_DOWNLOAD");
    println!("cargo::rustc-check-cfg=cfg(bundled_model)");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let cache_dir = manifest_dir.join(".model-cache");

    if std::env::var("SKIP_MODEL_DOWNLOAD").is_ok() {
        if all_files_present(&cache_dir) {
            println!("cargo:rustc-cfg=bundled_model");
        }
    } else if all_files_present(&cache_dir) {
        println!("cargo:warning=Using cached model from .model-cache/");
        println!("cargo:rustc-cfg=bundled_model");
    } else {
        println!("cargo:warning=Downloading AllMiniLML6V2 model (~22MB) from HuggingFace...");

        match download_model(&cache_dir) {
            Ok(()) => {
                println!("cargo:warning=Model downloaded and cached to .model-cache/");
                println!("cargo:rustc-cfg=bundled_model");
            }
            Err(e) => {
                println!("cargo:warning=Model download failed: {e}");
                println!("cargo:warning=Embeddings will download at runtime on first use.");
                // Don't set bundled_model cfg - embed/mod.rs will fall back to runtime download
            }
        }
    }

    #[cfg(target_os = "windows")]
    copy_directml_dll();
}

#[cfg(target_os = "windows")]
fn copy_directml_dll() {
    // Pyke's prebuilt ORT for Windows always ships with DirectML enabled.
    // `ort-sys` statically links onnxruntime.lib but ships DirectML.dll next
    // to it in its cache dir; ORT loads DirectML.dll at runtime via
    // LoadLibrary, which searches the exe's directory. Copy it next to
    // sclerox.exe so a distributed binary finds it without the pyke cache.
    let Some(dll) = find_directml_dll() else {
        println!("cargo:warning=DirectML.dll not found in pyke cache; GPU EP will need it on PATH or next to sclerox.exe");
        return;
    };

    let target_dir = target_profile_dir();
    let dest = target_dir.join("DirectML.dll");
    if let Err(e) = std::fs::copy(&dll, &dest) {
        println!(
            "cargo:warning=Failed to copy DirectML.dll to {}: {e}",
            dest.display()
        );
        return;
    }
    println!("cargo:warning=Copied DirectML.dll to {}", dest.display());
}

#[cfg(target_os = "windows")]
fn find_directml_dll() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let root = PathBuf::from(local).join("ort.pyke.io").join("dfbin");
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        for sub in std::fs::read_dir(entry.path()).ok()?.flatten() {
            let dll = sub.path().join("DirectML.dll");
            if dll.exists() {
                return Some(dll);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn target_profile_dir() -> PathBuf {
    // OUT_DIR is `<target>/<profile>/build/<pkg>-<hash>/out`; walk up 3 dirs.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    out_dir
        .ancestors()
        .nth(3)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            // Fallback: manifest_dir/target/<profile>
            let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
            let profile = if std::env::var("PROFILE").as_deref() == Ok("release") {
                "release"
            } else {
                "debug"
            };
            manifest.join("target").join(profile)
        })
}

fn all_files_present(cache_dir: &std::path::Path) -> bool {
    FILES.iter().all(|f| cache_dir.join(f).exists())
}

fn download_model(cache_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(cache_dir)?;

    for file in FILES {
        let dest = cache_dir.join(file);
        if dest.exists() {
            continue;
        }
        let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{file}");
        println!("cargo:warning=  Downloading {file}...");
        download_file(&url, &dest)?;
    }
    Ok(())
}

fn download_file(url: &str, dest: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("curl")
        .args([
            "-L",
            "--fail",
            "--silent",
            "--show-error",
            "-o",
            dest.to_str().unwrap(),
            url,
        ])
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("curl exited with {s} for {url}").into()),
        Err(e) => {
            // curl not available - try wget
            let status = Command::new("wget")
                .args(["-q", "-O", dest.to_str().unwrap(), url])
                .status()
                .map_err(|_| format!("neither curl nor wget available: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("wget failed for {url}").into())
            }
        }
    }
}
