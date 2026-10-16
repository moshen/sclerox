//! The server process: take the lock, load the model it was started for,
//! answer requests on the socket with a thread per connection, loading other
//! models as they are asked for, and exit when idle or asked to.

use anyhow::{bail, Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::fs::{File, TryLockError};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::protocol::{
    read_msg, write_msg, Build, ExitReason, ModelStatus, Request, Response, ServerStatus,
};
use super::runs::{Load, Runs};
use super::{lock_path, runs_path, socket_name, state_dir, token_path, Client, Found};
use crate::embed::{models, ModelSpec};

/// What `sclerox server start --foreground` reads from its settings.
pub struct ServeOptions {
    /// Exit after this long with no connections and no requests.
    pub idle: Duration,
    /// Loaded before the server listens; it fails to start without it.
    pub preload: &'static ModelSpec,
}

/// How [`serve`] ended.
pub enum Served {
    /// Another server was already running or starting.
    AlreadyRunning,
    Exited(ExitReason),
}

/// How long a new server waits for the lock held by one that is exiting.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the run's memory and request counts are saved to `server.db`.
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// How long a model that failed to load is refused before it is tried again.
const RETRY_LOAD: Duration = Duration::from_secs(5 * 60);
/// Where models run, until `embed.device` exists.
const DEVICE: &str = "cpu";

/// Serve until idle or stopped. `on_ready` runs once the preloaded model has
/// loaded and the socket is listening.
pub fn serve(options: ServeOptions, on_ready: impl FnOnce(&ServerStatus)) -> Result<Served> {
    std::fs::create_dir_all(state_dir())?;
    let lock_file = lock_path();
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_file)
        .with_context(|| format!("failed to open {}", lock_file.display()))?;
    if !acquire(&lock)? {
        return Ok(Served::AlreadyRunning);
    }

    let runs = Runs::open(&runs_path())?;
    let build = Build::current();
    let run_id = runs.start(&build.label())?;
    // Holding the lock, no other server is running.
    let crashed = runs.mark_crashed(run_id)?;
    if crashed > 0 {
        log::warn!("{crashed} earlier model server run(s) ended without recording why");
    }
    runs.prune()?;

    // Before loading the model, so a server that couldn't listen fails fast.
    let name = match socket_name() {
        Ok(name) => name,
        Err(e) => {
            let e = anyhow::Error::new(e).context("no usable model server socket");
            let _ = runs.failed(run_id, &format!("{e:#}"));
            return Err(e);
        }
    };
    let (token, token_file) = match write_token() {
        Ok(t) => t,
        Err(e) => {
            let e = e.context("failed to write the model server token");
            let _ = runs.failed(run_id, &format!("{e:#}"));
            return Err(e);
        }
    };

    let state = Arc::new(State {
        build,
        token,
        name,
        run_id,
        runs: Mutex::new(runs),
        models: Mutex::new(Vec::new()),
        idle: options.idle,
        started_at: chrono::Utc::now().to_rfc3339(),
        started: Instant::now(),
        peak_mb: AtomicU64::new(rss_mb()),
        requests: AtomicU64::new(0),
        connections: AtomicUsize::new(0),
        busy: AtomicUsize::new(0),
        last_activity: Mutex::new(Instant::now()),
        last_request: Mutex::new(None),
        stop: Mutex::new(None),
    });
    let failed = |e: anyhow::Error| -> Result<Served> {
        let _ = state.runs().failed(run_id, &format!("{e:#}"));
        let _ = std::fs::remove_file(&token_file);
        Err(e)
    };
    if let Err(e) = state.with_model(options.preload, |_| ()) {
        return failed(anyhow::anyhow!(e));
    }

    // The lock says no live server owns the name, so a socket file left by
    // one that crashed can go.
    let listener = ListenerOptions::new()
        .name(state.name.clone())
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
            return failed(anyhow::Error::new(e).context("failed to open the model server socket"))
        }
    };

    log::info!("model server ready: pid {}", std::process::id());
    on_ready(&state.status());

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || housekeeping(&state));
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
    state.save_progress()?;
    state.runs().end(run_id, reason)?;
    log::info!("model server exiting: {}", reason.as_str());
    let _ = std::fs::remove_file(&token_file);
    drop(lock);
    Ok(Served::Exited(reason))
}

/// Make a random token and write it where only this user can read it,
/// replacing any left by an earlier server. Returns it and its file.
fn write_token() -> Result<(String, std::path::PathBuf)> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let token = hex::encode(bytes);
    let path = token_path()?;
    let tmp = path.with_extension("token.tmp");
    // The directory is already private; the file is too, in case it moves.
    // A mode applies only on creation, so clear any leftover first.
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(&tmp)?, token.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok((token, path))
}

/// Take the server lock. `false` when another server holds it and answers
/// on the socket; while the holder isn't answering yet (still loading) or
/// any more (exiting), keep trying.
fn acquire(lock: &File) -> Result<bool> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e).context("failed to lock"),
        }
        if matches!(
            Client::connect(),
            Ok(Found::Ready(_) | Found::Mismatch(..) | Found::Unproven(_))
        ) {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            bail!("another model server holds the lock but isn't answering");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct State {
    build: Build,
    token: String,
    name: Name<'static>,
    run_id: i64,
    runs: Mutex<Runs>,
    /// Every model asked for, in that order.
    models: Mutex<Vec<Arc<Slot>>>,
    idle: Duration,
    started_at: String,
    started: Instant,
    peak_mb: AtomicU64,
    requests: AtomicU64,
    /// Open client connections.
    connections: AtomicUsize,
    /// Requests in progress.
    busy: AtomicUsize,
    /// The last request or closed connection; the idle clock.
    last_activity: Mutex<Instant>,
    last_request: Mutex<Option<Instant>>,
    stop: Mutex<Option<ExitReason>>,
}

/// One model: loaded on first use, then kept until the server exits.
struct Slot {
    spec: &'static ModelSpec,
    /// Held while loading and while embedding, so requests for one model run
    /// one at a time while other models' run alongside.
    model: Mutex<Loaded>,
    // The figures below are kept outside `model` so status never waits on
    // an embedding.
    loaded: AtomicBool,
    error: Mutex<Option<String>>,
    loaded_mb: AtomicU64,
    requests: AtomicU64,
    last_used: Mutex<Instant>,
    /// Its row in `model_loads`, once loaded.
    load_id: AtomicI64,
}

enum Loaded {
    No,
    Yes(Box<fastembed::TextEmbedding>),
    Failed(Instant),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    fn stopping(&self) -> Option<ExitReason> {
        *lock(&self.stop)
    }

    /// Stop accepting connections, for `reason` unless one was already given.
    fn request_stop(&self, reason: ExitReason) {
        lock(&self.stop).get_or_insert(reason);
        // The accept loop only checks between connections, so make one.
        let _ = Stream::connect(self.name.borrow());
    }

    fn touch(&self) {
        *lock(&self.last_activity) = Instant::now();
    }

    fn runs(&self) -> MutexGuard<'_, Runs> {
        lock(&self.runs)
    }

    fn slot(&self, spec: &'static ModelSpec) -> Arc<Slot> {
        let mut models = lock(&self.models);
        if let Some(slot) = models.iter().find(|s| s.spec.name == spec.name) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(Slot {
            spec,
            model: Mutex::new(Loaded::No),
            loaded: AtomicBool::new(false),
            error: Mutex::new(None),
            loaded_mb: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            last_used: Mutex::new(Instant::now()),
            load_id: AtomicI64::new(0),
        });
        models.push(Arc::clone(&slot));
        slot
    }

    /// Run `f` on `spec`'s model, loading it first if it isn't loaded. A
    /// model that failed to load is refused for [`RETRY_LOAD`].
    fn with_model<R>(
        &self,
        spec: &'static ModelSpec,
        f: impl FnOnce(&mut fastembed::TextEmbedding) -> R,
    ) -> std::result::Result<R, String> {
        let slot = self.slot(spec);
        let mut model = lock(&slot.model);
        let retry = match &*model {
            Loaded::Yes(_) => false,
            Loaded::No => true,
            Loaded::Failed(at) => at.elapsed() >= RETRY_LOAD,
        };
        if retry {
            *model = self.load(&slot);
        }
        match &mut *model {
            Loaded::Yes(m) => Ok(f(m)),
            _ => Err(lock(&slot.error).clone().unwrap_or_default()),
        }
    }

    fn load(&self, slot: &Slot) -> Loaded {
        let spec = slot.spec;
        let before = rss_mb();
        let started = Instant::now();
        let result = crate::embed::load_model(spec);
        let took = started.elapsed();
        let record = Load {
            model: spec.name,
            device: DEVICE,
            max_tokens: spec.max_tokens,
        };
        match result {
            Ok(model) => {
                let mb = rss_mb().saturating_sub(before);
                log::info!(
                    "model server loaded {} in {} ms, {mb} MB",
                    spec.name,
                    took.as_millis()
                );
                match self.runs().loaded(self.run_id, &record, took, Ok(mb)) {
                    Ok(id) => slot.load_id.store(id, Ordering::Relaxed),
                    Err(e) => log::debug!("model server couldn't record a load: {e:#}"),
                }
                slot.loaded_mb.store(mb, Ordering::Relaxed);
                *lock(&slot.error) = None;
                *lock(&slot.last_used) = Instant::now();
                slot.loaded.store(true, Ordering::Relaxed);
                self.peak_mb.fetch_max(rss_mb(), Ordering::Relaxed);
                Loaded::Yes(Box::new(model))
            }
            Err(e) => {
                let error = format!("{e:#}");
                log::error!("model server couldn't load {}: {error}", spec.name);
                let _ = self.runs().loaded(self.run_id, &record, took, Err(&error));
                *lock(&slot.error) = Some(error);
                Loaded::Failed(Instant::now())
            }
        }
    }

    /// Run a request with `model`: counted as busy while it runs, and as a
    /// request after.
    fn request(&self, model: &str, f: impl FnOnce(&'static ModelSpec) -> Response) -> Response {
        let _busy = Counted::new(&self.busy);
        let reply = match models::lookup(model) {
            Ok(spec) => {
                let reply = f(spec);
                let slot = self.slot(spec);
                slot.requests.fetch_add(1, Ordering::Relaxed);
                *lock(&slot.last_used) = Instant::now();
                reply
            }
            Err(e) => Response::Error(format!("{e:#}")),
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.peak_mb.fetch_max(rss_mb(), Ordering::Relaxed);
        *lock(&self.last_request) = Some(Instant::now());
        self.touch();
        reply
    }

    fn embed(&self, model: &str, texts: Vec<String>) -> Response {
        self.request(model, |spec| {
            // Batches of one, as the in-process embedder runs them, so the
            // vectors come out identical.
            match self.with_model(spec, |m| m.embed(&texts, Some(1))) {
                Ok(Ok(vectors)) => Response::Vectors(vectors),
                Ok(Err(e)) => {
                    log::error!("model server embedding failed: {e}");
                    Response::Error(format!("embedding failed: {e}"))
                }
                Err(e) => Response::Error(e),
            }
        })
    }

    /// Load a model without counting a request.
    fn load_only(&self, model: &str) -> Response {
        let _busy = Counted::new(&self.busy);
        let reply = match models::lookup(model) {
            Ok(spec) => match self.with_model(spec, |_| ()) {
                Ok(()) => Response::Loaded,
                Err(e) => Response::Error(e),
            },
            Err(e) => Response::Error(format!("{e:#}")),
        };
        self.touch();
        reply
    }

    fn status(&self) -> ServerStatus {
        let last = lock(&self.last_request).unwrap_or(self.started);
        let models = lock(&self.models)
            .iter()
            .filter_map(|slot| {
                let error = lock(&slot.error).clone();
                if !slot.loaded.load(Ordering::Relaxed) && error.is_none() {
                    // Still loading for the first time.
                    return None;
                }
                Some(ModelStatus {
                    name: slot.spec.name.to_string(),
                    device: DEVICE.to_string(),
                    loaded_mb: slot.loaded_mb.load(Ordering::Relaxed),
                    requests: slot.requests.load(Ordering::Relaxed),
                    idle_secs: lock(&slot.last_used).elapsed().as_secs(),
                    error,
                })
            })
            .collect();
        ServerStatus {
            pid: std::process::id(),
            build: self.build.clone(),
            started_at: self.started_at.clone(),
            rss_mb: rss_mb(),
            peak_mb: self.peak_mb.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            connections: self.connections.load(Ordering::SeqCst),
            idle_secs: last.elapsed().as_secs(),
            idle_limit_secs: self.idle.as_secs(),
            models,
        }
    }

    /// Save the run's peak memory and request counts.
    fn save_progress(&self) -> Result<()> {
        let runs = self.runs();
        runs.progress(
            self.run_id,
            self.peak_mb.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
        )?;
        for slot in lock(&self.models).iter() {
            let id = slot.load_id.load(Ordering::Relaxed);
            if id != 0 {
                runs.model_progress(id, slot.requests.load(Ordering::Relaxed))?;
            }
        }
        Ok(())
    }
}

/// Answer one client until it disconnects.
fn handle(state: &State, mut stream: Stream) {
    let _connection = Counted::new(&state.connections);
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
            Request::Hello { build } => {
                greeted = build == state.build;
                if greeted {
                    Response::Ready {
                        token: state.token.clone(),
                    }
                } else {
                    Response::Mismatch {
                        build: state.build.clone(),
                    }
                }
            }
            Request::Embed { .. } | Request::Load { .. } if !greeted => {
                Response::Error("model request before a matching hello".to_string())
            }
            Request::Embed { model, texts } => state.embed(&model, texts),
            Request::Load { model } => state.load_only(&model),
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
}

/// One more in `count` while alive, including while unwinding from a panic
/// in the model, so a failed request never keeps the server from idling out.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Exit when idle, and save the run's figures now and then.
fn housekeeping(state: &State) {
    let mut saved = (0, 0);
    let mut last_save = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if state.stopping().is_some() {
            return;
        }
        let quiet =
            state.connections.load(Ordering::SeqCst) == 0 && state.busy.load(Ordering::SeqCst) == 0;
        if quiet && lock(&state.last_activity).elapsed() >= state.idle {
            state.request_stop(ExitReason::Idle);
            return;
        }
        let now = (
            state.peak_mb.load(Ordering::Relaxed),
            state.requests.load(Ordering::Relaxed),
        );
        if now != saved && last_save.elapsed() >= SAVE_EVERY {
            if let Err(e) = state.save_progress() {
                log::debug!("model server couldn't save its run: {e:#}");
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
