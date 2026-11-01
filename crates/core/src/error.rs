use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// 429 / userRateLimitExceeded: retry with backoff, shrink concurrency.
    RateLimited,
    /// Daily/per-file quota (e.g. Drive downloadQuotaExceeded): do not hammer.
    Quota,
    /// Network drop, 5xx, timeouts.
    Transient,
    NotFound,
    /// Remote object changed since the version we pinned.
    VersionChanged,
    Fatal,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("backend {kind:?}: {msg}")]
    Backend {
        kind: BackendKind,
        msg: String,
        retry_after: Option<Duration>,
    },
    #[error("short read from backend: wanted {want} bytes, got {got}")]
    ShortRead { want: u64, got: u64 },
    #[error("cache full: no evictable block (everything is pinned or in use)")]
    CacheFull,
    #[error("io: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    pub fn backend(kind: BackendKind, msg: impl Into<String>) -> Self {
        Error::Backend { kind, msg: msg.into(), retry_after: None }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
