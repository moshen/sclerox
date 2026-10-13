#[cfg(target_os = "windows")]
use std::path::PathBuf;

// Nothing is downloaded at build time: the embedding models and the
// token-counting tokenizer are fetched (pinned and hash-checked) at runtime
// into $XDG_CACHE_HOME/sclerox/models/. See src/embed/download.rs.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");

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
