use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

// Nothing is downloaded at build time: the embedding models and the
// token-counting tokenizer are fetched (pinned and hash-checked) at runtime
// into $XDG_CACHE_HOME/sclerox/models/. See src/embed/download.rs.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_version();

    #[cfg(target_os = "windows")]
    copy_directml_dll();
}

/// Sets `SCLEROX_BUILD_VERSION` for `src/version.rs`: the release tag the
/// release workflow passes in `SCLEROX_VERSION`, or Cargo.toml's version,
/// plus `+<short commit>` when built from a git checkout.
fn build_version() {
    println!("cargo:rerun-if-env-changed=SCLEROX_VERSION");
    let version = std::env::var("SCLEROX_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap());
    let version = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(commit) => {
            rerun_on_new_commit();
            format!("{version}+{commit}")
        }
        None => version,
    };
    println!("cargo:rustc-env=SCLEROX_BUILD_VERSION={version}");
}

/// Rerun when HEAD moves: a checkout changes `HEAD`, a commit changes the
/// branch's ref (loose, or in `packed-refs` after a gc). In a worktree,
/// `HEAD` is its own but refs are shared with the main checkout.
fn rerun_on_new_commit() {
    let (Some(git_dir), Some(common)) = (
        git(&["rev-parse", "--git-dir"]),
        git(&["rev-parse", "--git-common-dir"]),
    ) else {
        return;
    };
    let (git_dir, common) = (Path::new(&git_dir), Path::new(&common));
    let mut watch = vec![git_dir.join("HEAD"), common.join("packed-refs")];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(common.join(branch));
    }
    for path in watch {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
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
