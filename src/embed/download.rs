//! Fetch pinned model files into sclerox's own cache, verifying each one.
//!
//! Every file is named by repo, commit and path, and carries its expected size
//! and sha256. Hugging Face repos are mutable (MiniLM's tokenizer changed
//! twice in September 2026), so downloading from `main` would silently change
//! the vectors under an unchanged model ID. Pinning the commit fixes what we
//! ask for; the hash proves what we got.
//!
//! A download streams into a temporary file beside its destination, hashing as
//! it goes, and is renamed into place only once size and hash both match. So a
//! file in the cache was verified when it arrived. Later loads check only its
//! size, which is free; a file of the wrong size (truncated, or replaced) counts
//! as missing and is downloaded again. Re-hashing on every load would add
//! ~0.13 s (Granite) to ~0.22 s (MiniLM) to every hook, and would not stop
//! deliberate tampering: whoever can write this cache can replace the sclerox
//! binary too.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One file in a Hugging Face repo, pinned by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoFile {
    /// Path inside the repo.
    pub path: &'static str,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: &'static str,
}

/// A set of files from one repo at one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pinned<'a> {
    pub repo: &'static str,
    /// Full commit hash. Never a branch: a branch moves.
    pub revision: &'static str,
    pub files: &'a [RepoFile],
}

/// Root of the model cache: `$XDG_CACHE_HOME/sclerox/models`. Files land under
/// `<org>--<name>/<revision>/<path>`. Deliberately not `~/.cache/huggingface/`:
/// that cache is shared with other tools, and sclerox never writes to it.
pub fn cache_dir() -> PathBuf {
    crate::xdg::cache_home().join("sclerox").join("models")
}

impl Pinned<'_> {
    fn dir(&self) -> PathBuf {
        cache_dir()
            .join(self.repo.replace('/', "--"))
            .join(self.revision)
    }

    fn local_path(&self, file: &RepoFile) -> PathBuf {
        self.dir().join(file.path)
    }

    /// Whether `file` is in the cache at its pinned size.
    fn is_present(&self, file: &RepoFile) -> bool {
        has_pinned_size(&self.local_path(file), file)
    }

    /// Whether every file is already in the cache.
    pub fn is_cached(&self) -> bool {
        self.files.iter().all(|f| self.is_present(f))
    }

    /// Total download size of the files not yet cached.
    fn missing_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter(|f| !self.is_present(f))
            .map(|f| f.size)
            .sum()
    }

    /// Make sure every file is cached, downloading and verifying any that are
    /// missing, and return their local paths in `files` order. `what` names
    /// the download in the one-line notice printed before it starts.
    pub fn fetch(&self, what: &str) -> Result<Vec<PathBuf>> {
        if !self.is_cached() {
            let dir = cache_dir();
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            // Serialize first downloads across processes (two session hooks
            // starting at once, parallel tests), so the loser waits for the
            // winner's files instead of fetching its own copy. Once the files
            // are present nothing is locked.
            // Never truncate: on Windows that can fail while another process
            // holds the lock.
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(".download.lock"))
                .context("failed to create the model download lock")?;
            lock.lock()
                .context("failed to take the model download lock")?;
            // Another process may have finished the download while we waited.
            if !self.is_cached() {
                let msg = format!(
                    "Downloading {what} ({:.1} MB) to {} (one time)...",
                    self.missing_bytes() as f64 / 1e6,
                    dir.display()
                );
                log::info!("{msg}");
                eprintln!("{msg}");
                let agent = agent();
                for file in self.files {
                    if self.is_present(file) {
                        continue;
                    }
                    let dest = self.local_path(file);
                    if dest.exists() {
                        log::warn!(
                            "{} is not its pinned size; downloading it again",
                            dest.display()
                        );
                    }
                    // The rename at the end replaces a wrong-size file.
                    self.download(&agent, file, &dest)?;
                }
            }
        }
        Ok(self.files.iter().map(|f| self.local_path(f)).collect())
    }

    fn url(&self, file: &RepoFile) -> String {
        let endpoint = std::env::var("HF_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "https://huggingface.co".to_string());
        format!(
            "{}/{}/resolve/{}/{}",
            endpoint.trim_end_matches('/'),
            self.repo,
            self.revision,
            file.path
        )
    }

    fn download(&self, agent: &ureq::Agent, file: &RepoFile, dest: &Path) -> Result<()> {
        let url = self.url(file);
        log::info!("downloading {url}");
        let parent = dest.parent().context("cache path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let partial = dest.with_file_name(format!(
            "{}.part",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download")
        ));
        let result = agent
            .get(&url)
            .call()
            .with_context(|| format!("failed to download {url}"))
            .and_then(|resp| {
                let mut out = std::fs::File::create(&partial)
                    .with_context(|| format!("failed to create {}", partial.display()))?;
                let body = resp.into_body().into_reader();
                let progress = std::io::stderr().is_terminal().then_some(file);
                copy_verified(body, &mut out, file, progress)
                    .with_context(|| format!("failed to download {url}"))?;
                out.sync_all()?;
                Ok(())
            })
            .and_then(|()| {
                std::fs::rename(&partial, dest)
                    .with_context(|| format!("failed to move {} into place", dest.display()))
            });
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    }
}

/// Whether `path` is a file of `file`'s pinned size.
fn has_pinned_size(path: &Path, file: &RepoFile) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == file.size)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        // Per read, not for the whole file: a slow link still finishes, a dead
        // one does not hang a session hook forever.
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .user_agent(concat!("sclerox/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Copy `body` into `out`, hashing as it goes. Fails on the first byte past the
/// expected size, and at the end on a short read or a hash mismatch.
fn copy_verified(
    mut body: impl Read,
    out: &mut impl Write,
    file: &RepoFile,
    progress: Option<&RepoFile>,
) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    let mut shown = 0;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > file.size {
            bail!(
                "{} is larger than the expected {} bytes",
                file.path,
                file.size
            );
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        if let Some(f) = progress {
            let pct = (done * 100 / f.size.max(1)) as usize;
            if pct >= shown + 10 {
                shown = pct - pct % 10;
                eprint!("\r  {}: {shown}%", f.path);
            }
        }
    }
    if progress.is_some() {
        eprintln!();
    }
    if done != file.size {
        bail!(
            "{} is {done} bytes, expected {} (truncated download?)",
            file.path,
            file.size
        );
    }
    let got = hex::encode(hasher.finalize());
    if got != file.sha256 {
        bail!(
            "{} failed its integrity check: sha256 {got}, expected {}",
            file.path,
            file.sha256
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: RepoFile = RepoFile {
        path: "hello.txt",
        size: 5,
        // sha256("hello")
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    };

    #[test]
    fn accepts_the_pinned_content() {
        let mut out = Vec::new();
        copy_verified(&b"hello"[..], &mut out, &HELLO, None).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn rejects_altered_content() {
        let err = copy_verified(&b"jello"[..], &mut Vec::new(), &HELLO, None).unwrap_err();
        assert!(err.to_string().contains("integrity check"), "{err}");
    }

    #[test]
    fn rejects_a_truncated_body() {
        let err = copy_verified(&b"hell"[..], &mut Vec::new(), &HELLO, None).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    #[test]
    fn stops_reading_past_the_expected_size() {
        let err = copy_verified(&b"hello world"[..], &mut Vec::new(), &HELLO, None).unwrap_err();
        assert!(err.to_string().contains("larger"), "{err}");
    }

    #[test]
    fn a_file_of_the_wrong_size_counts_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        assert!(!has_pinned_size(&path, &HELLO), "absent");
        std::fs::write(&path, b"hell").unwrap();
        assert!(!has_pinned_size(&path, &HELLO), "truncated");
        std::fs::write(&path, b"hello, world").unwrap();
        assert!(!has_pinned_size(&path, &HELLO), "replaced");
        std::fs::write(&path, b"hello").unwrap();
        assert!(has_pinned_size(&path, &HELLO));
        assert!(!has_pinned_size(dir.path(), &HELLO), "a directory");
    }

    #[test]
    fn layout_is_repo_then_revision_then_path() {
        let p = Pinned {
            repo: "org/name",
            revision: "abc123",
            files: &[HELLO],
        };
        let path = p.local_path(&HELLO);
        assert!(
            path.ends_with(Path::new("org--name").join("abc123").join("hello.txt")),
            "{}",
            path.display()
        );
        assert!(p
            .url(&HELLO)
            .ends_with("/org/name/resolve/abc123/hello.txt"));
    }
}
