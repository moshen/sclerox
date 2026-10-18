//! The server process: take the lock, load the model it was started for,
//! answer requests on the socket with a thread per connection, loading other
//! models as they are asked for, and exit when idle, over its memory limits,
//! or asked to.
//!
//! Requests take turns on the CPU one text at a time (see [`Gate`]): queries
//! go before bulk work, and clients doing bulk work alternate.
//!
//! Work queued in a database's `embed_jobs` runs on a thread of its own per
//! database, as bulk work, so databases take turns too. The databases with
//! work are listed in `server.db`, and a new server goes on with them.

use anyhow::{bail, Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, TryLockError};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::protocol::{
    read_msg, write_msg, Build, DbKind, ExitReason, Lane, ModelStatus, QueuedDb, Request, Response,
    ServerStatus,
};
use super::runs::{Load, Runs};
use super::{lock_path, runs_path, socket_name, state_dir, Client, Found};
use crate::db::jobs;
use crate::embed::{models, ModelSpec};

/// What `sclerox server start --foreground` reads from its settings.
pub struct ServeOptions {
    /// Exit after this long with no connections and no requests.
    pub idle: Duration,
    /// Loaded before the server listens; it fails to start without it.
    pub preload: &'static ModelSpec,
    /// `server.max_memory_mb`: over it, exit once nothing is using the server.
    pub soft_limit_mb: Option<u64>,
    /// `server.hard_memory_mb`: over it, exit after the request in progress.
    pub hard_limit_mb: Option<u64>,
    /// Session distillations running now. A rebuild of a main database
    /// waits for them.
    pub distillations: fn() -> usize,
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
/// How long a server over its soft memory limit waits with no connections
/// and no requests before exiting.
const RESTART_QUIET: Duration = Duration::from_secs(60);
/// With this many memory exits within [`MEMORY_EXIT_WINDOW`], a new server
/// ignores the soft limit, so a workload that keeps crossing it doesn't
/// reload the models over and over.
const MAX_MEMORY_EXITS: usize = 3;
const MEMORY_EXIT_WINDOW: Duration = Duration::from_secs(30 * 60);
/// How long a job waits before trying a locked database again, and between
/// checks for session distillations before rebuilding the main database.
const RETRY_WORK: Duration = Duration::from_secs(5);

/// Serve until idle or stopped. `on_ready` runs once the preloaded model has
/// loaded and the socket is listening.
pub fn serve(options: ServeOptions, on_ready: impl FnOnce(&ServerStatus)) -> Result<Served> {
    std::fs::create_dir_all(state_dir())?;
    let lock_file = lock_path().context("no private directory for the model server")?;
    let mut open = File::options();
    open.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let lock = open
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
    let mut soft_limit_mb = options.soft_limit_mb;
    if soft_limit_mb.is_some()
        && runs.recent_exits(&[ExitReason::Memory], MEMORY_EXIT_WINDOW)? >= MAX_MEMORY_EXITS
    {
        log::warn!(
            "the model server went over server.max_memory_mb {MAX_MEMORY_EXITS} times in \
             {} min; ignoring that limit for this run",
            MEMORY_EXIT_WINDOW.as_secs() / 60
        );
        soft_limit_mb = None;
    }

    // Before loading the model, so a server that couldn't listen fails fast.
    let name = match socket_name() {
        Ok(name) => name,
        Err(e) => {
            let e = anyhow::Error::new(e).context("no usable model server socket");
            let _ = runs.failed(run_id, &format!("{e:#}"));
            return Err(e);
        }
    };

    let state = Arc::new(State {
        build,
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
        gate: Gate::default(),
        base_mb: rss_mb(),
        soft_limit_mb: AtomicU64::new(soft_limit_mb.unwrap_or(0)),
        hard_limit_mb: AtomicU64::new(options.hard_limit_mb.unwrap_or(0)),
        restart_due: AtomicBool::new(false),
        workers: Mutex::new(HashMap::new()),
        distillations: options.distillations,
    });
    let failed = |e: anyhow::Error| -> Result<Served> {
        let _ = state.runs().failed(run_id, &format!("{e:#}"));
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
    // Bound first, so the `runs` lock is let go before `start_worker` takes
    // `workers`: workers take them the other way round.
    let queued = state.runs().with_work();
    match queued {
        Ok(dbs) => {
            for (path, kind) in dbs {
                start_worker(&state, path, kind);
            }
        }
        Err(e) => log::warn!("model server couldn't read its queued work: {e:#}"),
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
    // Let embeddings in progress reply, and queued work reach a point it can
    // go on from.
    let deadline = Instant::now() + Duration::from_secs(60);
    while (state.busy.load(Ordering::SeqCst) > 0 || !self::lock(&state.workers).is_empty())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    state.save_progress()?;
    state.runs().end(run_id, reason)?;
    log::info!("model server exiting: {}", reason.as_str());
    drop(lock);
    Ok(Served::Exited(reason))
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
        if matches!(Client::connect(), Ok(Found::Ready(_) | Found::Mismatch(..))) {
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
    gate: Gate,
    /// Resident memory before any model loaded.
    base_mb: u64,
    /// The memory limits in MB, 0 when off or ignored for this run.
    soft_limit_mb: AtomicU64,
    hard_limit_mb: AtomicU64,
    /// Over the soft limit: exit after [`RESTART_QUIET`] unused.
    restart_due: AtomicBool,
    /// Databases whose jobs a thread is running, by path.
    workers: Mutex<HashMap<String, Worker>>,
    distillations: fn() -> usize,
}

struct Worker {
    kind: DbKind,
    /// More was queued while it ran: look again before finishing.
    again: bool,
}

/// Turns on the CPU: one text at a time across all models, since each
/// already uses every core. Waiting queries go first; bulk requests go in
/// the order they asked, so two clients doing bulk work alternate.
#[derive(Default)]
struct Gate {
    turns: Mutex<Turns>,
    changed: Condvar,
}

#[derive(Default)]
struct Turns {
    busy: bool,
    queries: usize,
    /// Tickets of waiting bulk requests, oldest first.
    bulk: VecDeque<u64>,
    next_ticket: u64,
}

impl Gate {
    /// Wait for a turn in `lane`; it lasts until the [`Turn`] is dropped.
    fn enter(&self, lane: Lane) -> Turn<'_> {
        let mut turns = lock(&self.turns);
        match lane {
            Lane::Query => {
                turns.queries += 1;
                while turns.busy {
                    turns = self.wait(turns);
                }
                turns.queries -= 1;
            }
            Lane::Bulk => {
                let ticket = turns.next_ticket;
                turns.next_ticket += 1;
                turns.bulk.push_back(ticket);
                while turns.busy || turns.queries > 0 || turns.bulk.front() != Some(&ticket) {
                    turns = self.wait(turns);
                }
                turns.bulk.pop_front();
            }
        }
        turns.busy = true;
        Turn(self)
    }

    fn wait<'a>(&self, turns: MutexGuard<'a, Turns>) -> MutexGuard<'a, Turns> {
        self.changed.wait(turns).unwrap_or_else(|e| e.into_inner())
    }

    /// Requests waiting: queries, bulk.
    fn waiting(&self) -> (usize, usize) {
        let turns = lock(&self.turns);
        (turns.queries, turns.bulk.len())
    }
}

/// A turn on the CPU; ends when dropped, including during a panic.
struct Turn<'a>(&'a Gate);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        lock(&self.0.turns).busy = false;
        self.0.changed.notify_all();
    }
}

/// One model: loaded on first use, then kept until the server exits.
struct Slot {
    spec: &'static ModelSpec,
    /// Held while loading and during a turn embedding with it.
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
    /// What it runs on, once loaded.
    device: Mutex<&'static str>,
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
            device: Mutex::new("cpu"),
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
        // Loaded for the server's lifetime, so where `embed.device` is
        // `auto` it may go on an accelerator.
        let result = crate::embed::load_model(spec, true);
        let took = started.elapsed();
        let device = match &result {
            Ok((_, device)) => device.name(),
            Err(_) => "none",
        };
        let record = Load {
            model: spec.name,
            device,
            max_tokens: spec.max_tokens,
        };
        match result {
            Ok((model, device)) => {
                *lock(&slot.device) = device.name();
                let mb = rss_mb().saturating_sub(before);
                log::info!(
                    "model server loaded {} on {device} in {} ms, {mb} MB",
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
                self.check_limits_reachable();
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

    /// Ignore a memory limit that the models loaded so far already fill:
    /// restarting would load them again and land over it again.
    fn check_limits_reachable(&self) {
        let loaded: u64 = lock(&self.models)
            .iter()
            .map(|s| s.loaded_mb.load(Ordering::Relaxed))
            .sum();
        let needed = self.base_mb + loaded;
        for (limit, setting) in [
            (&self.soft_limit_mb, "server.max_memory_mb"),
            (&self.hard_limit_mb, "server.hard_memory_mb"),
        ] {
            let mb = limit.load(Ordering::Relaxed);
            if mb != 0 && needed >= mb {
                log::warn!(
                    "the model server needs {needed} MB with its models loaded, over \
                     {setting} ({mb}); ignoring that limit for this run"
                );
                limit.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Over the soft limit, and it hasn't been ignored since.
    fn restart_due(&self) -> bool {
        self.soft_limit_mb.load(Ordering::Relaxed) != 0 && self.restart_due.load(Ordering::Relaxed)
    }

    /// After a request: exit after it when over the hard limit, and once
    /// unused when over the soft one.
    fn check_memory(&self) {
        let mb = rss_mb();
        self.peak_mb.fetch_max(mb, Ordering::Relaxed);
        let hard = self.hard_limit_mb.load(Ordering::Relaxed);
        if hard != 0 && mb > hard {
            log::warn!(
                "model server is using {mb} MB, over server.hard_memory_mb ({hard}); exiting"
            );
            self.request_stop(ExitReason::HardMemory);
            return;
        }
        let soft = self.soft_limit_mb.load(Ordering::Relaxed);
        if soft != 0 && mb > soft && !self.restart_due.swap(true, Ordering::Relaxed) {
            log::info!(
                "model server is using {mb} MB, over server.max_memory_mb ({soft}); \
                 exiting once unused for {}s",
                RESTART_QUIET.as_secs()
            );
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
        self.check_memory();
        *lock(&self.last_request) = Some(Instant::now());
        self.touch();
        reply
    }

    fn embed(&self, model: &str, texts: Vec<String>, lane: Lane) -> Response {
        self.request(model, |spec| {
            // Load it before taking turns, so a load doesn't hold up others.
            // Only then: the model's lock is taken during others' turns, and
            // waiting on it here, outside the queue, could wait for many.
            if !self.slot(spec).loaded.load(Ordering::Relaxed) {
                if let Err(e) = self.with_model(spec, |_| ()) {
                    return Response::Error(e);
                }
            }
            let mut vectors = Vec::with_capacity(texts.len());
            for text in &texts {
                // Bulk work stops between texts; the client sends what's
                // left to the next server.
                if let Some(reason) = self.stopping().filter(|_| lane == Lane::Bulk) {
                    return Response::Stopping { reason };
                }
                let _turn = self.gate.enter(lane);
                // One at a time, as the in-process embedder runs them, so
                // the vectors come out identical.
                match self.with_model(spec, |m| m.embed(std::slice::from_ref(text), Some(1))) {
                    Ok(Ok(v)) => vectors.extend(v),
                    Ok(Err(e)) => {
                        log::error!("model server embedding failed: {e}");
                        return Response::Error(format!("embedding failed: {e}"));
                    }
                    Err(e) => return Response::Error(e),
                }
            }
            Response::Vectors(vectors)
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
        let (waiting_queries, waiting_bulk) = self.gate.waiting();
        let limit = |mb: &AtomicU64| Some(mb.load(Ordering::Relaxed)).filter(|&mb| mb != 0);
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
                    device: lock(&slot.device).to_string(),
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
            soft_limit_mb: limit(&self.soft_limit_mb),
            hard_limit_mb: limit(&self.hard_limit_mb),
            restart_due: self.restart_due(),
            waiting_queries,
            waiting_bulk,
            models,
            queue: lock(&self.workers)
                .iter()
                .map(|(path, w)| QueuedDb {
                    path: path.clone(),
                    kind: w.kind,
                })
                .collect(),
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
fn handle(state: &Arc<State>, mut stream: Stream) {
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
                    Response::Ready
                } else {
                    Response::Mismatch {
                        build: state.build.clone(),
                    }
                }
            }
            Request::Embed { .. } | Request::Load { .. } | Request::Enqueue { .. } if !greeted => {
                Response::Error("model request before a matching hello".to_string())
            }
            // Refused so the server can exit instead of serving a bulk
            // client's next batch, and the next.
            Request::Embed { .. } | Request::Load { .. } | Request::Enqueue { .. }
                if state.stopping().is_some() =>
            {
                Response::Stopping {
                    reason: state.stopping().unwrap_or(ExitReason::Stopped),
                }
            }
            Request::Embed { model, texts, lane } => state.embed(&model, texts, lane),
            Request::Load { model } => state.load_only(&model),
            Request::Status => Response::Status(state.status()),
            Request::Enqueue { path, kind } => {
                state.touch();
                // Bound first; see the startup loop.
                let noted = state.runs().set_work(&path, kind, true);
                match noted {
                    Ok(()) => {
                        start_worker(state, path, kind);
                        Response::Queued
                    }
                    Err(e) => Response::Error(format!("couldn't note the queued work: {e:#}")),
                }
            }
            Request::Shutdown { reason } => {
                state.request_stop(reason);
                let reason = state.stopping().unwrap_or(reason);
                let _ = write_msg(&mut stream, &Response::Stopping { reason });
                break;
            }
        };
        if write_msg(&mut stream, &reply).is_err() {
            break;
        }
    }
    state.touch();
}

/// Run the jobs queued in the database at `path` on a thread of its own,
/// unless one is already at it, in which case it looks again before
/// finishing.
fn start_worker(state: &Arc<State>, path: String, kind: DbKind) {
    {
        let mut workers = lock(&state.workers);
        if let Some(worker) = workers.get_mut(&path) {
            worker.again = true;
            return;
        }
        workers.insert(path.clone(), Worker { kind, again: false });
        // A worker finishing just now may have cleared it. `runs` after
        // `workers`, as everywhere.
        if let Err(e) = state.runs().set_work(&path, kind, true) {
            log::debug!("model server couldn't note work in {path}: {e:#}");
        }
    }
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        loop {
            // Left marked as having work when it failed, for the next server.
            let finished = match run_jobs(&state, &path, kind) {
                Ok(()) => true,
                Err(e) if state.stopping().is_some() => {
                    // Left queued for the next server.
                    log::info!("model server stopped work on {path}: {e:#}");
                    lock(&state.workers).remove(&path);
                    return;
                }
                Err(e) if jobs::is_interrupted(&e) => {
                    log::debug!("model server will retry {path}: {e:#}");
                    std::thread::sleep(RETRY_WORK);
                    continue;
                }
                Err(e) => {
                    log::warn!("model server couldn't run the jobs in {path}: {e:#}");
                    false
                }
            };
            let mut workers = lock(&state.workers);
            match workers.get_mut(&path) {
                Some(worker) if worker.again => worker.again = false,
                _ => {
                    if finished {
                        if let Err(e) = state.runs().set_work(&path, kind, false) {
                            log::debug!("model server couldn't note {path} done: {e:#}");
                        }
                    }
                    workers.remove(&path);
                    state.touch();
                    return;
                }
            }
        }
    });
}

/// Run the jobs queued in one database, as bulk work.
fn run_jobs(state: &State, path: &str, kind: DbKind) -> Result<()> {
    let path = std::path::Path::new(path);
    if !jobs::has_queue(path)? {
        // Removed since, or not one of ours: opening it would make or
        // migrate one.
        log::warn!(
            "model server ignored {}: no embedding queue there",
            path.display()
        );
        return Ok(());
    }
    let conn = kind.open(path)?;
    let mut embed = |model_id: &str, texts: &[&str]| -> Result<Vec<Vec<f32>>> {
        let spec = models::by_id(model_id)
            .with_context(|| format!("unknown embedding model '{model_id}'"))?;
        // Prefixed as `Embedder::embed_each` does, so the vectors match.
        let texts = texts
            .iter()
            .map(|t| format!("{}{t}", spec.document_prefix))
            .collect();
        match state.embed(spec.name, texts, Lane::Bulk) {
            Response::Vectors(v) => Ok(v),
            Response::Stopping { reason } => Err(jobs::Interrupted(format!(
                "the server is stopping ({})",
                reason.as_str()
            ))
            .into()),
            Response::Error(e) => bail!("{e}"),
            other => bail!("unexpected reply to embed: {other:?}"),
        }
    };
    // A distiller from before an upgrade would write the old model's vectors
    // into the rebuilt database, as `db reembed` explains.
    let mut before_rebuild = || -> Result<()> {
        while kind == DbKind::Main && (state.distillations)() > 0 {
            if let Some(reason) = state.stopping() {
                return Err(jobs::Interrupted(format!(
                    "the server is stopping ({})",
                    reason.as_str()
                ))
                .into());
            }
            std::thread::sleep(RETRY_WORK);
        }
        Ok(())
    };
    jobs::run(&conn, kind, &mut embed, &mut before_rebuild)
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
        let quiet = state.connections.load(Ordering::SeqCst) == 0
            && state.busy.load(Ordering::SeqCst) == 0
            && lock(&state.workers).is_empty();
        let unused = lock(&state.last_activity).elapsed();
        if quiet && unused >= state.idle {
            state.request_stop(ExitReason::Idle);
            return;
        }
        if quiet && unused >= RESTART_QUIET && state.restart_due() {
            state.request_stop(ExitReason::Memory);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait until `gate` has `queries` and `bulk` requests waiting.
    fn until_waiting(gate: &Gate, queries: usize, bulk: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while gate.waiting() != (queries, bulk) {
            assert!(Instant::now() < deadline, "waiting: {:?}", gate.waiting());
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_waiting_query_goes_before_waiting_bulk_work() {
        let gate = Arc::new(Gate::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let turn = gate.enter(Lane::Bulk);
        let take = |lane, name| {
            let (gate, order) = (Arc::clone(&gate), Arc::clone(&order));
            std::thread::spawn(move || {
                let _turn = gate.enter(lane);
                lock(&order).push(name);
            })
        };
        let bulk = take(Lane::Bulk, "bulk");
        until_waiting(&gate, 0, 1);
        let query = take(Lane::Query, "query");
        until_waiting(&gate, 1, 1);
        drop(turn);
        bulk.join().unwrap();
        query.join().unwrap();
        assert_eq!(*lock(&order), ["query", "bulk"]);
    }

    #[test]
    fn bulk_clients_take_turns() {
        let gate = Arc::new(Gate::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let turn = gate.enter(Lane::Query);
        let client = |name| {
            let (gate, order) = (Arc::clone(&gate), Arc::clone(&order));
            std::thread::spawn(move || {
                for _ in 0..3 {
                    let _turn = gate.enter(Lane::Bulk);
                    lock(&order).push(name);
                }
            })
        };
        let a = client("a");
        until_waiting(&gate, 0, 1);
        let b = client("b");
        until_waiting(&gate, 0, 2);
        drop(turn);
        a.join().unwrap();
        b.join().unwrap();
        // b was waiting when a finished its first turn, so a asked again
        // behind it. (Later turns alternate too unless a thread is
        // descheduled between two of its turns, which a busy machine does.)
        let order = lock(&order);
        assert_eq!(order[..2], ["a", "b"]);
        assert_eq!(order.len(), 6);
    }

    #[test]
    fn a_panic_mid_turn_gives_the_turn_back() {
        let gate = Arc::new(Gate::default());
        let panicked = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let _turn = gate.enter(Lane::Bulk);
                panic!("inside the model");
            })
            .join()
        };
        assert!(panicked.is_err());
        // Would wait forever if the turn were still held.
        drop(gate.enter(Lane::Query));
    }
}
