//! The model server: one long-running `sclerox server` process per user
//! that keeps models loaded, so commands get vectors without loading a model
//! themselves (about 0.27 s a command on the CPU).
//!
//! The server only runs models. Commands still open their databases and
//! print their own output, and anything that goes wrong with the server -
//! not running, another build, a dropped connection - falls back to loading
//! the model in-process, as every command did before the server existed.
//!
//! There is one socket (see `socket_path`; a named pipe on Windows) and one
//! lock, `server.lock` in the state directory. The server loads the model it
//! was started for, and any other a request names on first use, so sessions
//! configured for different models share it.

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
/// How long a command waits for the server to answer an embed request
/// before giving up on it and embedding in-process.
const EMBED_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a command waits for anything else: the handshake, status, stop.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Texts per embed request, so a long bulk job doesn't hold the model for
/// minutes while other commands wait to embed a query.
const TEXTS_PER_REQUEST: usize = 16;
/// Failed starts or crashes in [`FAILURE_WINDOW`] after which commands stop
/// starting a server and embed in-process.
const MAX_RECENT_FAILURES: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// `$XDG_STATE_HOME/sclerox`: the locks and `server.db` live here.
fn state_dir() -> PathBuf {
    crate::xdg::state_home().join("sclerox")
}

/// The run history, `server.db`.
pub fn runs_path() -> PathBuf {
    state_dir().join("server.db")
}

fn lock_path() -> PathBuf {
    state_dir().join("server.lock")
}

/// Whether this OS lets a listener create its socket owner-only (macOS
/// doesn't). Either way the socket's directory is private; this is a second
/// lock on the door where it's available.
#[cfg(unix)]
pub(super) const SOCKET_MODE_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "android"));

/// The Unix socket file, `$XDG_STATE_HOME/sclerox/run/server.sock`, in a
/// directory only this user can enter.
#[cfg(unix)]
fn socket_path() -> std::io::Result<PathBuf> {
    // sun_path is 104 bytes on macOS and 108 on Linux, including the NUL.
    const MAX_PATH: usize = 103;
    let path = run_dir()?.join("server.sock");
    if path.as_os_str().len() > MAX_PATH {
        return Err(std::io::Error::other(format!(
            "socket path {} is over the {MAX_PATH}-byte limit",
            path.display()
        )));
    }
    Ok(path)
}

/// `$XDG_STATE_HOME/sclerox/run`: the socket (on Unix) and the server's
/// token. Only this user can read it: on Unix it is made owner-only; on
/// Windows it inherits the user profile's ACL, which `~/.local/state` sits
/// under unless `XDG_STATE_HOME` points elsewhere.
fn run_dir() -> std::io::Result<PathBuf> {
    let dir = state_dir().join("run");
    #[cfg(unix)]
    return private_dir(&dir);
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// Where the server keeps the token it proves itself with; see
/// [`protocol::Response::Ready`].
fn token_path() -> std::io::Result<PathBuf> {
    Ok(run_dir()?.join("server.token"))
}

/// Create `dir` owner-only if it's missing, and check that it is a
/// directory of this user's that no one else can enter.
#[cfg(unix)]
fn private_dir(dir: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a private directory of this user",
            dir.display()
        )));
    }
    Ok(dir.to_path_buf())
}

#[cfg(unix)]
fn socket_name() -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    Ok(socket_path()?.to_fs_name::<GenericFilePath>()?.into_owned())
}

/// A short stand-in for a directory path in names that can't hold the path.
#[cfg(windows)]
fn dir_hash(dir: &std::path::Path) -> String {
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(dir.to_string_lossy().as_bytes()));
    hash[..12].to_string()
}

/// A named pipe. Pipe names are global, so the name carries a hash of the
/// state directory: two users, or a test with its own state directory,
/// each get their own server.
#[cfg(windows)]
fn socket_name() -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    let name = format!("sclerox-{}", dir_hash(&state_dir()));
    Ok(name.to_ns_name::<GenericNamespaced>()?.into_owned())
}

/// A connection to a running server, past the handshake.
pub struct Client {
    /// `None` once a call has timed out: its thread still owns the stream.
    stream: Option<Stream>,
}

/// A server that didn't reply in time. Unlike a closed connection, this
/// means a server is there but stuck, so starting another would only wait
/// on its lock.
#[derive(Debug)]
struct TimedOut(Duration);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model server didn't reply within {}s", self.0.as_secs())
    }
}

impl std::error::Error for TimedOut {}

/// What a connection attempt found.
enum Found {
    Ready(Client),
    /// A server from another build.
    Mismatch(Client, Build),
    /// A server that couldn't prove it is this user's (see
    /// [`protocol::Response::Ready`]). It gets no texts, but `stop` may still
    /// ask it to exit.
    Unproven(Client),
    /// Nothing listening.
    Absent,
}

impl Client {
    fn connect() -> Result<Found> {
        // No usable socket path means no server could have started either.
        let name = match socket_name() {
            Ok(name) => name,
            Err(e) => {
                log::debug!("no model server socket: {e}");
                return Ok(Found::Absent);
            }
        };
        let stream = match Stream::connect(name) {
            Ok(s) => s,
            Err(e) => {
                log::trace!("no model server: {e}");
                return Ok(Found::Absent);
            }
        };
        let mut client = Client {
            stream: Some(stream),
        };
        let hello = Request::Hello {
            build: Build::current(),
        };
        let reply = match client.call(&hello, REPLY_TIMEOUT) {
            Ok(reply) => reply,
            Err(e) if e.is::<TimedOut>() => return Err(e),
            // Accepted and dropped: the server is on its way out.
            Err(e) => {
                log::debug!("model server closed the connection: {e:#}");
                return Ok(Found::Absent);
            }
        };
        match reply {
            Response::Ready { token } => {
                // Anyone can create a Windows pipe with our name first. Only a
                // server of this user's can have written the token file.
                let expected = token_path().and_then(std::fs::read_to_string);
                if expected.ok().as_deref() != Some(token.as_str()) {
                    return Ok(Found::Unproven(client));
                }
                Ok(Found::Ready(client))
            }
            Response::Mismatch { build } => {
                log::debug!(
                    "model server is {}, not {}",
                    build.label(),
                    Build::current().label()
                );
                Ok(Found::Mismatch(client, build))
            }
            other => bail!("unexpected reply to hello: {other:?}"),
        }
    }

    /// Send `req` and wait up to `timeout` for the reply. The exchange runs on
    /// its own thread because Windows named pipes have no read timeout; on
    /// timeout that thread is left blocked and the connection abandoned.
    fn call(&mut self, req: &Request, timeout: Duration) -> Result<Response> {
        let mut stream = self
            .stream
            .take()
            .context("model server connection was abandoned after a timeout")?;
        let mut frame = Vec::new();
        write_msg(&mut frame, req)?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reply = std::io::Write::write_all(&mut stream, &frame)
                .and_then(|()| read_msg::<Response>(&mut stream));
            let _ = tx.send((stream, reply));
        });
        match rx.recv_timeout(timeout) {
            Ok((stream, reply)) => {
                self.stream = Some(stream);
                reply.context("model server connection failed")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(TimedOut(timeout).into()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("model server call ended without a reply")
            }
        }
    }

    /// Embed texts exactly as given with `spec`, in requests of at most
    /// [`TEXTS_PER_REQUEST`].
    pub fn embed(&mut self, spec: &ModelSpec, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(TEXTS_PER_REQUEST) {
            let req = Request::Embed {
                model: spec.name.to_string(),
                texts: batch.to_vec(),
            };
            match self.call(&req, EMBED_TIMEOUT)? {
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

    /// Have the server load `spec` if it hasn't.
    fn load(&mut self, spec: &ModelSpec) -> Result<()> {
        let req = Request::Load {
            model: spec.name.to_string(),
        };
        match self.call(&req, START_TIMEOUT)? {
            Response::Loaded => Ok(()),
            Response::Error(e) => bail!("model server: {e}"),
            other => bail!("unexpected reply to load: {other:?}"),
        }
    }

    pub fn status(&mut self) -> Result<ServerStatus> {
        match self.call(&Request::Status, REPLY_TIMEOUT)? {
            Response::Status(s) => Ok(s),
            other => bail!("unexpected reply to status: {other:?}"),
        }
    }

    fn shutdown(mut self, reason: ExitReason) -> Result<()> {
        match self.call(&Request::Shutdown { reason }, REPLY_TIMEOUT)? {
            Response::Stopping => Ok(()),
            other => bail!("unexpected reply to shutdown: {other:?}"),
        }
    }
}

const UNPROVEN: &str = "the model server couldn't prove it is this user's, so it gets no texts";

/// A client of the server, to embed with `spec`, starting the server when
/// none is running. `None` means embed in-process: `spec` isn't downloaded
/// yet (loading it in-process shows the download), or the server couldn't
/// start or belongs to an older build that this one doesn't replace. Why is
/// logged.
pub fn connect_or_start(spec: &'static ModelSpec) -> Option<Client> {
    if !super::is_downloaded(spec) {
        log::debug!(
            "{} not downloaded yet; not using the model server",
            spec.name
        );
        return None;
    }
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
    match Client::connect()? {
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
            wait_until_stopped(Duration::from_secs(10))?;
        }
        Found::Unproven(_) => bail!(UNPROVEN),
        Found::Absent => {}
    }
    start(spec, explicit)
}

/// Start the server in the background, loading `spec` first, and connect to
/// it once it is ready. `None` when, unless `explicit`, the server keeps
/// failing.
fn start(spec: &'static ModelSpec, explicit: bool) -> Result<Option<Client>> {
    if let Err(e) = socket_name() {
        if explicit {
            return Err(e).context("can't start a model server");
        }
        log::debug!("no model server socket: {e}");
        return Ok(None);
    }
    let failures = runs::Runs::open(&runs_path())?.recent_failures(FAILURE_WINDOW)?;
    if failures >= MAX_RECENT_FAILURES && !explicit {
        log::warn!(
            "model server failed {failures} times in the last {} minutes; embedding in-process",
            FAILURE_WINDOW.as_secs() / 60
        );
        return Ok(None);
    }
    let mut child = spawn_detached(spec)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match Client::connect()? {
            Found::Ready(client) => return Ok(Some(client)),
            // Another build won the race to start one.
            Found::Mismatch(..) => return Ok(None),
            Found::Unproven(_) => bail!(UNPROVEN),
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

/// Run `sclerox server start --foreground`, loading `spec`, detached from this
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
        let _held = windows_std::NotInherited::new();
        match cmd.spawn() {
            // The job doesn't allow breaking away; the server then ends with
            // it, and the next command starts another.
            Err(e) if e.raw_os_error() == Some(ACCESS_DENIED) => {
                cmd.creation_flags(flags);
            }
            other => return other.context("failed to start the model server"),
        }
        cmd.spawn().context("failed to start the model server")
    }
    #[cfg(not(windows))]
    {
        cmd.spawn().context("failed to start the model server")
    }
}

/// Windows hands a child every inheritable handle of its parent, not just
/// the stdio it is given. This command's own stdin, stdout and stderr are
/// usually inheritable (it inherited them), so without this the server
/// would hold the caller's pipes open, and whoever reads this command's
/// output would wait until the server exits.
#[cfg(windows)]
mod windows_std {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE: -10, -11, -12.
    const STD_HANDLES: [u32; 3] = [-10i32 as u32, -11i32 as u32, -12i32 as u32];

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetHandleInformation(handle: Handle, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    }

    /// This process's inheritable std handles, made uninheritable until
    /// dropped.
    pub struct NotInherited(Vec<Handle>);

    impl NotInherited {
        pub fn new() -> Self {
            let mut cleared = Vec::new();
            for which in STD_HANDLES {
                // SAFETY: plain Win32 calls on this process's own std
                // handles; a missing or invalid handle just fails the call.
                unsafe {
                    let handle = GetStdHandle(which);
                    if handle.is_null() || handle as isize == -1 {
                        continue;
                    }
                    let mut flags = 0;
                    if GetHandleInformation(handle, &mut flags) != 0
                        && flags & HANDLE_FLAG_INHERIT != 0
                        && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
                    {
                        cleared.push(handle);
                    }
                }
            }
            Self(cleared)
        }
    }

    impl Drop for NotInherited {
        fn drop(&mut self) {
            for &handle in &self.0 {
                // SAFETY: as above; restores the flag cleared in `new`.
                unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
                }
            }
        }
    }
}

/// Wait for the server to exit, which is when it releases its lock.
fn wait_until_stopped(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while is_running() {
        if Instant::now() >= deadline {
            bail!("model server did not stop within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Whether a server is running (holds the lock), whether or not it has
/// finished loading.
fn is_running() -> bool {
    let Ok(file) = std::fs::File::open(lock_path()) else {
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

/// The server's status, or `None` when none is answering.
pub fn status() -> Result<Option<ServerStatus>> {
    match Client::connect()? {
        Found::Ready(mut c) | Found::Mismatch(mut c, _) => Ok(Some(c.status()?)),
        Found::Unproven(_) => bail!(UNPROVEN),
        Found::Absent => Ok(None),
    }
}

/// Stop the server and wait for it to exit. `false` when none was running.
pub fn stop() -> Result<bool> {
    match Client::connect()? {
        Found::Ready(c) | Found::Mismatch(c, _) | Found::Unproven(c) => {
            c.shutdown(ExitReason::Stopped)?;
            wait_until_stopped(Duration::from_secs(30))?;
            Ok(true)
        }
        Found::Absent => Ok(false),
    }
}

/// Start the server in the background, as commands do, make sure it has
/// loaded `spec`, and return its status.
pub fn start_detached(spec: &'static ModelSpec) -> Result<ServerStatus> {
    super::ensure_downloaded(spec)?;
    match try_connect_or_start(spec, true)? {
        Some(mut client) => {
            client.load(spec)?;
            client.status()
        }
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
