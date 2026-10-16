//! The model server: one long-running `sclerox server` process per model that
//! keeps the model loaded, so commands get vectors without loading it
//! themselves (about 0.27 s a command on the CPU).
//!
//! The server only embeds. Commands still open their databases and print
//! their own output, and anything that goes wrong with the server - not
//! running, another build, a dropped connection - falls back to loading the
//! model in-process, as every command did before the server existed.
//!
//! Each model gets its own socket and lock in the state directory
//! (`server-<model>.sock` / `.lock`), so sessions configured for different
//! models never replace each other's server.

pub mod protocol;
pub mod runs;
mod serve;

use anyhow::{bail, Context, Result};
use interprocess::local_socket::{prelude::*, Name, Stream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::ModelSpec;
use protocol::{read_msg, write_msg, Build, ExitReason, Request, Response, ServerStatus};
pub use serve::{serve, ServeOptions, Served};

/// Env var carrying `embed.server` from `config::init()` into this
/// settings-free module.
pub const MODE_ENV: &str = "SCLEROX_EMBED_SERVER";

/// Whether commands use the model server (`embed.server`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Use the server, starting it when it isn't running.
    Auto,
    /// Always load the model in-process.
    Off,
}

/// Read an `embed.server` value: `auto` or `off`, any case.
pub fn parse_mode(v: &str) -> Option<Mode> {
    match v.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Mode::Auto),
        "off" => Some(Mode::Off),
        _ => None,
    }
}

/// The configured mode. Unset means off: only the sclerox binary turns the
/// server on (by bridging `embed.server`), so tests and other users of this
/// library never start one from whatever executable they run in.
pub fn mode() -> Mode {
    std::env::var(MODE_ENV)
        .ok()
        .and_then(|v| parse_mode(&v))
        .unwrap_or(Mode::Off)
}

/// How long a command waits for a server it started to load the model.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a command waits for a reply before giving up on the server.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// Texts per embed request, so a long bulk job doesn't hold the model for
/// minutes while other commands wait to embed a query.
const TEXTS_PER_REQUEST: usize = 16;
/// Failed starts or crashes in [`FAILURE_WINDOW`] after which commands stop
/// starting a server for the model and embed in-process.
const MAX_RECENT_FAILURES: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// `$XDG_STATE_HOME/sclerox`: the socket, lock and `server.db` live here.
fn state_dir() -> PathBuf {
    crate::xdg::state_home().join("sclerox")
}

/// The run history, `server.db`.
pub fn runs_path() -> PathBuf {
    state_dir().join("server.db")
}

fn lock_path(spec: &ModelSpec) -> PathBuf {
    state_dir().join(format!("server-{}.lock", spec.name))
}

/// Whether this OS lets a listener create its socket owner-only. Linux
/// does; where it doesn't (macOS), the socket goes in a private directory
/// instead, which keeps other users out the same way.
#[cfg(unix)]
pub(super) const SOCKET_MODE_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "android"));

/// A Unix socket file: in the state directory when the socket itself can be
/// owner-only and the path fits (about 100 bytes), else in
/// [`private_socket_dir`].
#[cfg(unix)]
fn socket_path(spec: &ModelSpec) -> std::io::Result<PathBuf> {
    // sun_path is 104 bytes on macOS and 108 on Linux, including the NUL.
    const MAX_PATH: usize = 103;
    let dir = state_dir();
    let path = dir.join(format!("server-{}.sock", spec.name));
    if SOCKET_MODE_SUPPORTED && path.as_os_str().len() <= MAX_PATH {
        return Ok(path);
    }
    let path = private_socket_dir()?.join(format!("{}-{}.sock", dir_hash(&dir), spec.name));
    if path.as_os_str().len() > MAX_PATH {
        return Err(std::io::Error::other(format!(
            "no socket path short enough for {}",
            path.display()
        )));
    }
    Ok(path)
}

/// `$XDG_RUNTIME_DIR/sclerox`, else `sclerox-<uid>` in the temp directory,
/// created owner-only. An existing directory must be ours and private.
#[cfg(unix)]
fn private_socket_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("sclerox"),
        _ => std::env::temp_dir().join(format!("sclerox-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a private directory of this user",
            dir.display()
        )));
    }
    Ok(dir)
}

#[cfg(unix)]
fn socket_name(spec: &ModelSpec) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    Ok(socket_path(spec)?
        .to_fs_name::<GenericFilePath>()?
        .into_owned())
}

/// A short stand-in for a directory path in names that can't hold the path.
fn dir_hash(dir: &std::path::Path) -> String {
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(dir.to_string_lossy().as_bytes()));
    hash[..12].to_string()
}

/// A named pipe. Pipe names are global, so the name carries a hash of the
/// state directory: two users, or a test with its own state directory,
/// each get their own server.
#[cfg(windows)]
fn socket_name(spec: &ModelSpec) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    let name = format!("sclerox-{}-{}", spec.name, dir_hash(&state_dir()));
    Ok(name.to_ns_name::<GenericNamespaced>()?.into_owned())
}

/// A connection to a running server, past the handshake.
pub struct Client {
    stream: Stream,
}

/// What a connection attempt found.
enum Found {
    Ready(Client),
    /// A server from another build, or for another model.
    Mismatch(Client, Build),
    /// Nothing listening.
    Absent,
}

impl Client {
    fn connect(spec: &ModelSpec) -> Result<Found> {
        let stream = match Stream::connect(socket_name(spec)?) {
            Ok(s) => s,
            Err(e) => {
                log::trace!("no model server for {}: {e}", spec.name);
                return Ok(Found::Absent);
            }
        };
        // Windows named pipes have no timeouts; there a hung server hangs
        // the command, as a hung in-process model would.
        if let Err(e) = stream.set_recv_timeout(Some(REPLY_TIMEOUT)) {
            if e.kind() != std::io::ErrorKind::Unsupported || !cfg!(windows) {
                return Err(e.into());
            }
        }
        let mut client = Client { stream };
        let hello = Request::Hello {
            build: Build::current(),
            model_id: spec.id.to_string(),
        };
        let reply = match client.call(&hello) {
            Ok(reply) => reply,
            // Accepted and dropped: the server is on its way out.
            Err(e) => {
                log::debug!(
                    "model server for {} closed the connection: {e:#}",
                    spec.name
                );
                return Ok(Found::Absent);
            }
        };
        match reply {
            Response::Ready => Ok(Found::Ready(client)),
            Response::Mismatch { build, model_id } => {
                log::debug!(
                    "model server runs {model_id} from {}, not {} from {}",
                    build.label(),
                    spec.id,
                    Build::current().label()
                );
                Ok(Found::Mismatch(client, build))
            }
            other => bail!("unexpected reply to hello: {other:?}"),
        }
    }

    fn call(&mut self, req: &Request) -> Result<Response> {
        write_msg(&mut self.stream, req).context("model server write failed")?;
        read_msg(&mut self.stream).context("model server read failed")
    }

    /// Embed texts exactly as given, in requests of at most
    /// [`TEXTS_PER_REQUEST`].
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(TEXTS_PER_REQUEST) {
            let req = Request::Embed {
                texts: batch.to_vec(),
            };
            match self.call(&req)? {
                Response::Vectors(v) if v.len() == batch.len() => vectors.extend(v),
                Response::Vectors(v) => {
                    bail!(
                        "model server returned {} vectors for {}",
                        v.len(),
                        batch.len()
                    )
                }
                Response::Error(e) => bail!("model server: {e}"),
                other => bail!("unexpected reply to embed: {other:?}"),
            }
        }
        Ok(vectors)
    }

    pub fn status(&mut self) -> Result<ServerStatus> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(s),
            other => bail!("unexpected reply to status: {other:?}"),
        }
    }

    fn shutdown(mut self, reason: ExitReason) -> Result<()> {
        match self.call(&Request::Shutdown { reason })? {
            Response::Stopping => Ok(()),
            other => bail!("unexpected reply to shutdown: {other:?}"),
        }
    }
}

/// A client for `spec`'s server, starting the server when none is running.
/// `None` means embed in-process: the server is off, couldn't start, or
/// belongs to an older build that this one doesn't replace. Why is logged.
pub fn connect_or_start(spec: &'static ModelSpec) -> Option<Client> {
    match try_connect_or_start(spec, false) {
        Ok(client) => client,
        Err(e) => {
            log::warn!("model server unavailable, loading the model in-process: {e:#}");
            None
        }
    }
}

/// `explicit` is `sclerox server start`, which tries even after recent
/// failures.
fn try_connect_or_start(spec: &'static ModelSpec, explicit: bool) -> Result<Option<Client>> {
    match Client::connect(spec)? {
        Found::Ready(client) => return Ok(Some(client)),
        Found::Mismatch(client, theirs) => {
            if !Build::current().newer_than(&theirs) {
                log::info!(
                    "model server is {}, not this build; embedding in-process",
                    theirs.label()
                );
                return Ok(None);
            }
            log::info!("replacing model server {} with this build", theirs.label());
            client.shutdown(ExitReason::Version)?;
            wait_until_stopped(spec, Duration::from_secs(10))?;
        }
        Found::Absent => {}
    }
    start(spec, explicit)
}

/// Start `spec`'s server in the background and connect to it once the model
/// has loaded. `None` when the model isn't downloaded yet (loading it
/// in-process shows the download) or, unless `explicit`, the server keeps
/// failing.
fn start(spec: &'static ModelSpec, explicit: bool) -> Result<Option<Client>> {
    if !super::is_downloaded(spec) {
        log::debug!("{} not downloaded yet; not starting a server", spec.name);
        return Ok(None);
    }
    let failures = runs::Runs::open(&runs_path())?.recent_failures(spec.name, FAILURE_WINDOW)?;
    if failures >= MAX_RECENT_FAILURES && !explicit {
        log::warn!(
            "model server for {} failed {failures} times in the last {} minutes; \
             embedding in-process",
            spec.name,
            FAILURE_WINDOW.as_secs() / 60
        );
        return Ok(None);
    }
    let mut child = spawn_detached(spec)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match Client::connect(spec)? {
            Found::Ready(client) => return Ok(Some(client)),
            // Another build won the race to start one.
            Found::Mismatch(..) => return Ok(None),
            Found::Absent => {}
        }
        // Success means another server already held the lock; keep waiting
        // for that one to load.
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("model server exited with {status} while starting");
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "model server did not start within {}s",
                START_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run `sclerox server start --foreground` for `spec`, detached from this
/// process: no shared stdio (a hook's caller waits on its pipes closing), its
/// own session on Unix, and out of this process's job on Windows, so it
/// outlives whatever started it.
fn spawn_detached(spec: &ModelSpec) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().context("cannot find the sclerox executable")?;
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "start", "--foreground", "--model", spec.name])
        // Don't hold the caller's working directory open (Windows won't
        // delete a directory a process is in).
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const ACCESS_DENIED: i32 = 5;
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // The job doesn't allow breaking away; the server then ends with
            // it, and the next command starts another.
            Err(e) if e.raw_os_error() == Some(ACCESS_DENIED) => {
                cmd.creation_flags(flags);
            }
            other => return other.context("failed to start the model server"),
        }
    }
    cmd.spawn().context("failed to start the model server")
}

/// Wait for `spec`'s server to exit, which is when it releases its lock.
fn wait_until_stopped(spec: &ModelSpec, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while is_running(spec) {
        if Instant::now() >= deadline {
            bail!(
                "model server for {} did not stop within {}s",
                spec.name,
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Whether a server for `spec` is running (holds its lock), whether or not
/// it has finished loading.
fn is_running(spec: &ModelSpec) -> bool {
    let Ok(file) = std::fs::File::open(lock_path(spec)) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

/// The status of `spec`'s server, or `None` when none is answering.
pub fn status(spec: &ModelSpec) -> Result<Option<ServerStatus>> {
    match Client::connect(spec)? {
        Found::Ready(mut c) | Found::Mismatch(mut c, _) => Ok(Some(c.status()?)),
        Found::Absent => Ok(None),
    }
}

/// Stop `spec`'s server and wait for it to exit. `false` when none was
/// running.
pub fn stop(spec: &ModelSpec) -> Result<bool> {
    match Client::connect(spec)? {
        Found::Ready(c) | Found::Mismatch(c, _) => {
            c.shutdown(ExitReason::Stopped)?;
            wait_until_stopped(spec, Duration::from_secs(30))?;
            Ok(true)
        }
        Found::Absent => Ok(false),
    }
}

/// Start `spec`'s server in the background, as commands do, and return its
/// status once it is ready.
pub fn start_detached(spec: &'static ModelSpec) -> Result<ServerStatus> {
    super::ensure_downloaded(spec)?;
    match try_connect_or_start(spec, true)? {
        Some(mut client) => client.status(),
        None => bail!(
            "a model server from another build is running; stop it with \
             `sclerox server stop` first"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(parse_mode("auto"), Some(Mode::Auto));
        assert_eq!(parse_mode(" OFF "), Some(Mode::Off));
        assert_eq!(parse_mode("on"), None);
    }
}
