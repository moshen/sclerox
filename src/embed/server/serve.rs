//! The server process: take the model's lock, load the model, answer
//! requests on the socket with a thread per connection, and exit when idle
//! or asked to.

use anyhow::{bail, Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::fs::{File, TryLockError};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::protocol::{read_msg, write_msg, Build, ExitReason, Request, Response, ServerStatus};
use super::runs::Runs;
use super::{lock_path, runs_path, socket_name, state_dir, Client, Found};
use crate::embed::ModelSpec;

/// What `sclerox server start --foreground` reads from its settings.
pub struct ServeOptions {
    /// Exit after this long with no connections and no requests.
    pub idle: Duration,
}

/// How [`serve`] ended.
pub enum Served {
    /// Another server for the model was already running or starting.
    AlreadyRunning,
    Exited(ExitReason),
}

/// How long a new server waits for the lock held by one that is exiting.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the run's memory and request counts are saved to `server.db`.
const SAVE_EVERY: Duration = Duration::from_secs(30);

/// Serve `spec` until idle or stopped. `on_ready` runs once the model has
/// loaded and the socket is listening.
pub fn serve(
    spec: &'static ModelSpec,
    options: ServeOptions,
    on_ready: impl FnOnce(&ServerStatus),
) -> Result<Served> {
    std::fs::create_dir_all(state_dir())?;
    let lock_file = lock_path(spec);
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_file)
        .with_context(|| format!("failed to open {}", lock_file.display()))?;
    if !acquire(&lock, spec)? {
        return Ok(Served::AlreadyRunning);
    }

    let runs = Runs::open(&runs_path())?;
    let build = Build::current();
    let run_id = runs.start(&build.label(), spec.name, "cpu", spec.max_tokens)?;
    // Holding the lock, no other server for this model is running.
    let crashed = runs.mark_crashed(spec.name, run_id)?;
    if crashed > 0 {
        log::warn!(
            "{crashed} earlier model server run(s) for {} ended without recording why",
            spec.name
        );
    }
    runs.prune()?;

    let model = match crate::embed::load_model(spec) {
        Ok(m) => m,
        Err(e) => {
            let _ = runs.failed(run_id, &format!("{e:#}"));
            return Err(e);
        }
    };
    let loaded_mb = rss_mb();
    runs.loaded(run_id, loaded_mb)?;

    let name = socket_name(spec)?;
    // The lock says no live server owns the name, so a socket file left by
    // one that crashed can go.
    let listener = ListenerOptions::new()
        .name(name.clone())
        .try_overwrite(true);
    #[cfg(unix)]
    let listener = if super::SOCKET_MODE_SUPPORTED {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        listener.mode(0o600)
    } else {
        listener
    };
    let listener = match listener.create_sync() {
        Ok(l) => l,
        Err(e) => {
            let e = anyhow::Error::new(e).context("failed to open the model server socket");
            let _ = runs.failed(run_id, &format!("{e:#}"));
            return Err(e);
        }
    };

    let state = Arc::new(State {
        spec,
        build,
        name,
        model: Mutex::new(model),
        idle: options.idle,
        started_at: chrono::Utc::now().to_rfc3339(),
        started: Instant::now(),
        loaded_mb,
        peak_mb: AtomicU64::new(loaded_mb),
        requests: AtomicU64::new(0),
        connections: AtomicUsize::new(0),
        busy: AtomicUsize::new(0),
        last_activity: Mutex::new(Instant::now()),
        last_request: Mutex::new(None),
        stop: Mutex::new(None),
    });
    log::info!(
        "model server for {} ready: pid {}, {loaded_mb} MB after loading",
        spec.name,
        std::process::id()
    );
    on_ready(&state.status());

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || housekeeping(&state, run_id));
    }
    for conn in listener.incoming() {
        if state.stopping().is_some() {
            break;
        }
        match conn {
            Ok(stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || handle(&state, stream));
            }
            Err(e) => log::warn!("model server accept failed: {e}"),
        }
    }
    let reason = state.stopping().unwrap_or(ExitReason::Stopped);
    // Removes the socket, so new commands start a fresh server (which waits
    // for this one's lock) instead of connecting here.
    drop(listener);
    // Let embeddings in progress reply.
    let deadline = Instant::now() + Duration::from_secs(60);
    while state.busy.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    runs.progress(
        run_id,
        state.peak_mb.load(Ordering::Relaxed),
        state.requests.load(Ordering::Relaxed),
    )?;
    runs.end(run_id, reason)?;
    log::info!(
        "model server for {} exiting: {}",
        spec.name,
        reason.as_str()
    );
    drop(lock);
    Ok(Served::Exited(reason))
}

/// Take the model's lock. `false` when another server holds it and answers
/// on the socket; while the holder isn't answering yet (still loading) or
/// any more (exiting), keep trying.
fn acquire(lock: &File, spec: &ModelSpec) -> Result<bool> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e).context("failed to lock"),
        }
        if matches!(
            Client::connect(spec),
            Ok(Found::Ready(_) | Found::Mismatch(..))
        ) {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            bail!(
                "another model server for {} holds the lock but isn't answering",
                spec.name
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct State {
    spec: &'static ModelSpec,
    build: Build,
    name: Name<'static>,
    model: Mutex<fastembed::TextEmbedding>,
    idle: Duration,
    started_at: String,
    started: Instant,
    loaded_mb: u64,
    peak_mb: AtomicU64,
    requests: AtomicU64,
    /// Open client connections.
    connections: AtomicUsize,
    /// Embed requests in progress.
    busy: AtomicUsize,
    /// The last embed request or closed connection; the idle clock.
    last_activity: Mutex<Instant>,
    last_request: Mutex<Option<Instant>>,
    stop: Mutex<Option<ExitReason>>,
}

impl State {
    fn stopping(&self) -> Option<ExitReason> {
        *self.stop.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop accepting connections, for `reason` unless one was already given.
    fn request_stop(&self, reason: ExitReason) {
        self.stop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(reason);
        // The accept loop only checks between connections, so make one.
        let _ = Stream::connect(self.name.borrow());
    }

    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn embed(&self, texts: Vec<String>) -> Response {
        self.busy.fetch_add(1, Ordering::SeqCst);
        let result = {
            let mut model = self.model.lock().unwrap_or_else(|e| e.into_inner());
            // Batches of one, as the in-process embedder runs them, so the
            // vectors come out identical.
            model.embed(&texts, Some(1))
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.peak_mb.fetch_max(rss_mb(), Ordering::Relaxed);
        *self.last_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        self.touch();
        self.busy.fetch_sub(1, Ordering::SeqCst);
        match result {
            Ok(vectors) => Response::Vectors(vectors),
            Err(e) => {
                log::error!("model server embedding failed: {e}");
                Response::Error(format!("embedding failed: {e}"))
            }
        }
    }

    fn status(&self) -> ServerStatus {
        let last = self
            .last_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(self.started);
        ServerStatus {
            pid: std::process::id(),
            build: self.build.clone(),
            model: self.spec.name.to_string(),
            device: "cpu".to_string(),
            started_at: self.started_at.clone(),
            loaded_mb: self.loaded_mb,
            rss_mb: rss_mb(),
            peak_mb: self.peak_mb.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            connections: self.connections.load(Ordering::SeqCst),
            idle_secs: last.elapsed().as_secs(),
            idle_limit_secs: self.idle.as_secs(),
        }
    }
}

/// Answer one client until it disconnects.
fn handle(state: &State, mut stream: Stream) {
    state.connections.fetch_add(1, Ordering::SeqCst);
    let mut greeted = false;
    loop {
        let req = match read_msg::<Request>(&mut stream) {
            Ok(req) => req,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    log::debug!("model server dropped a connection: {e}");
                }
                break;
            }
        };
        let reply = match req {
            Request::Hello { build, model_id } => {
                greeted = build == state.build && model_id == state.spec.id;
                if greeted {
                    Response::Ready
                } else {
                    Response::Mismatch {
                        build: state.build.clone(),
                        model_id: state.spec.id.to_string(),
                    }
                }
            }
            Request::Embed { .. } if !greeted => {
                Response::Error("embed before a matching hello".to_string())
            }
            Request::Embed { texts } => state.embed(texts),
            Request::Status => Response::Status(state.status()),
            Request::Shutdown { reason } => {
                let _ = write_msg(&mut stream, &Response::Stopping);
                state.request_stop(reason);
                break;
            }
        };
        if write_msg(&mut stream, &reply).is_err() {
            break;
        }
    }
    state.touch();
    state.connections.fetch_sub(1, Ordering::SeqCst);
}

/// Exit when idle, and save the run's figures now and then.
fn housekeeping(state: &State, run_id: i64) {
    let runs = Runs::open(&runs_path())
        .map_err(|e| log::warn!("model server can't record its run: {e:#}"))
        .ok();
    let mut saved = (state.loaded_mb, 0);
    let mut last_save = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if state.stopping().is_some() {
            return;
        }
        let quiet =
            state.connections.load(Ordering::SeqCst) == 0 && state.busy.load(Ordering::SeqCst) == 0;
        let since = state
            .last_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed();
        if quiet && since >= state.idle {
            state.request_stop(ExitReason::Idle);
            return;
        }
        let now = (
            state.peak_mb.load(Ordering::Relaxed),
            state.requests.load(Ordering::Relaxed),
        );
        if now != saved && last_save.elapsed() >= SAVE_EVERY {
            if let Some(runs) = &runs {
                if let Err(e) = runs.progress(run_id, now.0, now.1) {
                    log::debug!("model server couldn't save its run: {e:#}");
                }
            }
            saved = now;
            last_save = Instant::now();
        }
    }
}

/// Resident memory in MB, or 0 when unavailable.
fn rss_mb() -> u64 {
    memory_stats::memory_stats().map_or(0, |s| (s.physical_mem / (1024 * 1024)) as u64)
}
