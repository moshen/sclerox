//! Messages between a command (the client) and the model server, and how
//! they are framed on the socket: a little-endian `u32` length, then the
//! message encoded with postcard. Vectors travel as raw `f32`s.
//!
//! Client and server are always the same executable, so there is no version
//! negotiation beyond [`Build`]: a client that finds a server from another
//! build either replaces it or embeds in-process.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub use crate::db::jobs::DbKind;
use std::io::{self, Read, Write};

/// Largest frame either side reads. A request of 16 long texts or a reply of
/// 16 vectors is well under a megabyte; anything near this is corrupt.
const MAX_FRAME: u32 = 64 << 20;

/// Which build of sclerox is running: the package version plus the
/// executable's size and modification time, so a rebuild without a version
/// bump still counts as different.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub version: String,
    /// Seconds since the Unix epoch; 0 when unknown.
    pub exe_modified: u64,
    pub exe_len: u64,
}

impl Build {
    /// This process's build.
    pub fn current() -> Self {
        let meta = std::env::current_exe().and_then(std::fs::metadata).ok();
        let exe_modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            exe_modified,
            exe_len: meta.map_or(0, |m| m.len()),
        }
    }

    /// Whether this build is the one to keep when two meet: the more recently
    /// built executable. Two binaries in use at once (an installed one and a
    /// dev build) then settle on one server instead of replacing each other's
    /// on every command.
    pub fn newer_than(&self, other: &Build) -> bool {
        self.exe_modified > other.exe_modified
    }

    /// For `server status` and the run history.
    pub fn label(&self) -> String {
        let built = chrono::DateTime::from_timestamp(self.exe_modified as i64, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!("{} (built {built})", self.version)
    }
}

/// Why a server stopped, as recorded in `server.db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// Nothing to do for `server.idle_minutes`.
    Idle,
    /// Replaced by a newer build.
    Version,
    /// `sclerox server stop`.
    Stopped,
    /// Over `server.max_memory_mb`, once nothing was using it.
    Memory,
    /// Over `server.hard_memory_mb`, after the request in progress.
    HardMemory,
    /// Couldn't load the model it was started for, or open the socket.
    Failed,
    /// Ended without saying why (killed, crashed); recorded by the next server.
    Crashed,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Idle => "idle",
            ExitReason::Version => "version",
            ExitReason::Stopped => "stopped",
            ExitReason::Memory => "memory",
            ExitReason::HardMemory => "hard_memory",
            ExitReason::Failed => "failed",
            ExitReason::Crashed => "crashed",
        }
    }

    /// Whether a server that stopped for this reason should be replaced
    /// straight away by a client in the middle of its work, rather than the
    /// client going on in-process.
    pub fn restarts(self) -> bool {
        matches!(
            self,
            ExitReason::Memory | ExitReason::HardMemory | ExitReason::Version
        )
    }
}

/// Which work an embedding is: a query someone is waiting on, or bulk work
/// (re-embedding, indexing, background backfill) that can wait for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lane {
    Query,
    Bulk,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// The first message on a connection: the client's build.
    Hello {
        build: Build,
    },
    /// Embed texts exactly as given (prefixes already applied), one at a
    /// time, as the in-process embedder does. `model` is a model name
    /// (`ModelSpec::name`); the server loads it on first use.
    Embed {
        model: String,
        texts: Vec<String>,
        lane: Lane,
    },
    /// Load `model` now, so the first request for it doesn't wait.
    Load {
        model: String,
    },
    Status,
    /// Exit once any embedding in progress finishes.
    Shutdown {
        reason: ExitReason,
    },
    /// The database at `path` has jobs queued in its `embed_jobs`; run them.
    Enqueue {
        path: String,
        kind: DbKind,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// Matching build.
    Ready,
    /// The server is another build; it has nothing for this client.
    Mismatch {
        build: Build,
    },
    Vectors(Vec<Vec<f32>>),
    Loaded,
    Status(ServerStatus),
    /// The reply to `Shutdown`, and to a model request once the server is
    /// on its way out.
    Stopping {
        reason: ExitReason,
    },
    Error(String),
    /// The reply to `Enqueue`.
    Queued,
}

/// What `sclerox server status` shows about a running server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub pid: u32,
    pub build: Build,
    /// RFC 3339.
    pub started_at: String,
    pub rss_mb: u64,
    pub peak_mb: u64,
    pub requests: u64,
    pub connections: usize,
    pub idle_secs: u64,
    pub idle_limit_secs: u64,
    /// `server.max_memory_mb`, unless off or ignored for this run.
    pub soft_limit_mb: Option<u64>,
    /// `server.hard_memory_mb`, unless off or ignored for this run.
    pub hard_limit_mb: Option<u64>,
    /// Over the soft limit: the server exits once nothing is using it.
    pub restart_due: bool,
    /// Embedding requests waiting for their turn.
    pub waiting_queries: usize,
    pub waiting_bulk: usize,
    /// Models loaded, or that failed to load, in the order first asked for.
    pub models: Vec<ModelStatus>,
    /// Databases whose queued jobs the server is running.
    pub queue: Vec<QueuedDb>,
}

/// A database with jobs the server is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedDb {
    pub path: String,
    pub kind: DbKind,
}

/// One model in a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStatus {
    pub name: String,
    pub device: String,
    /// How much resident memory grew while it loaded.
    pub loaded_mb: u64,
    pub requests: u64,
    /// Since its last request, or since it loaded.
    pub idle_secs: u64,
    /// Why the last load failed; it is tried again after a while.
    pub error: Option<String>,
}

pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&n| n <= MAX_FRAME)
        .ok_or_else(|| io::Error::other(format!("message too large: {} bytes", body.len())))?;
    // One write per frame, so a frame is never split across two sends.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Read one message. `UnexpectedEof` when the other side closed between
/// messages.
pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is over the limit"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        let texts = vec!["query: ünïcode".to_string(), String::new()];
        write_msg(
            &mut buf,
            &Request::Embed {
                model: "granite-small-r2".into(),
                texts: texts.clone(),
                lane: Lane::Bulk,
            },
        )
        .unwrap();
        write_msg(
            &mut buf,
            &Response::Vectors(vec![vec![0.5, -1.25, f32::MIN]]),
        )
        .unwrap();

        let mut r = buf.as_slice();
        match read_msg(&mut r).unwrap() {
            Request::Embed {
                model,
                texts: got,
                lane,
            } => {
                assert_eq!(model, "granite-small-r2");
                assert_eq!(lane, Lane::Bulk);
                assert_eq!(got, texts);
            }
            other => panic!("{other:?}"),
        }
        match read_msg(&mut r).unwrap() {
            Response::Vectors(v) => assert_eq!(v, [[0.5, -1.25, f32::MIN]]),
            other => panic!("{other:?}"),
        }
        // Nothing left: the reader sees a clean end.
        let end = read_msg::<Request>(&mut r).unwrap_err();
        assert_eq!(end.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn vectors_travel_as_raw_floats() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Response::Vectors(vec![vec![1.0; 384]])).unwrap();
        // Length prefix, variant tag, two length varints, then 4 bytes a float.
        assert!(buf.len() < 4 + 8 + 384 * 4, "{} bytes", buf.len());
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 16]);
        let err = read_msg::<Request>(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_newer_build_wins() {
        let build = |exe_modified| Build {
            version: "0.1.0".into(),
            exe_modified,
            exe_len: 1,
        };
        assert!(build(200).newer_than(&build(100)));
        assert!(!build(100).newer_than(&build(200)));
        // Same time, different binary: neither replaces the other.
        assert!(!build(100).newer_than(&build(100)));
    }
}
