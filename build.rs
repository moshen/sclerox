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
        return;
    }

    if all_files_present(&cache_dir) {
        println!("cargo:warning=Using cached model from .model-cache/");
        println!("cargo:rustc-cfg=bundled_model");
        return;
    }

    println!("cargo:warning=Downloading AllMiniLML6V2 model (~22MB) from HuggingFace...");

    if let Err(e) = download_model(&cache_dir) {
        println!("cargo:warning=Model download failed: {e}");
        println!("cargo:warning=Embeddings will download at runtime on first use.");
        // Don't set bundled_model cfg - embed/mod.rs will fall back to runtime download
        return;
    }

    println!("cargo:warning=Model downloaded and cached to .model-cache/");
    println!("cargo:rustc-cfg=bundled_model");
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
