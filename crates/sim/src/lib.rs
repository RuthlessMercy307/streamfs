//! Simulated remote: deterministic synthetic files (no disk needed, so a 20 GB
//! "game file" costs nothing) behind configurable latency, per-connection and
//! global bandwidth, and injected failures. Content is a pure function of
//! (seed, offset) so any read can be verified byte-for-byte via `expected`.

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use sfs_core::{Backend, BackendKind, Error, FileMeta, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub latency: Duration,
    pub jitter: Duration,
    /// bytes/s per request (0 = unlimited)
    pub per_conn_bps: u64,
    /// bytes/s across all requests (0 = unlimited)
    pub total_bps: u64,
    /// every Nth request fails with a transient error
    pub fail_every: Option<u64>,
    /// every Nth request is rate limited (429-like)
    pub ratelimit_every: Option<u64>,
    /// concurrent requests above this get rate limited
    pub max_parallel: Option<usize>,
    /// The Nth request hangs for the extra duration (remote tail latency / stuck connection).
    pub stall_nth: Option<(u64, Duration)>,
    /// Every Nth request hangs for the extra duration (a flaky link with recurring stalls).
    pub stall_every: Option<(u64, Duration)>,
}

impl SimConfig {
    pub fn instant() -> Self {
        SimConfig {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            per_conn_bps: 0,
            total_bps: 0,
            fail_every: None,
            ratelimit_every: None,
            max_parallel: None,
            stall_nth: None,
            stall_every: None,
        }
    }
    /// Rough "far away HDD/cloud" profile. Placeholder numbers until measured
    /// against the real provider; benchmarks must label results as simulated.
    pub fn far_cloud() -> Self {
        SimConfig {
            latency: Duration::from_millis(120),
            jitter: Duration::from_millis(40),
            per_conn_bps: 12 << 20,
            total_bps: 40 << 20,
            ..Self::instant()
        }
    }
}

struct SimFile {
    size: u64,
    seed: u64,
    version: String,
}

pub struct SimBackend {
    files: Mutex<HashMap<String, SimFile>>,
    cfg: Mutex<SimConfig>,
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub max_seen_parallel: AtomicUsize,
    active: AtomicUsize,
    global_next: tokio::sync::Mutex<Instant>,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Byte content of synthetic file `seed` at [off, off+len).
pub fn expected(seed: u64, off: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut w = off / 8;
    let skip = (off % 8) as usize;
    while out.len() < len + skip {
        out.extend_from_slice(&splitmix(seed ^ w.wrapping_mul(0xA24B_AED4_963E_E407)).to_le_bytes());
        w += 1;
    }
    out.drain(..skip);
    out.truncate(len);
    out
}

pub fn norm(p: &str) -> String {
    format!("/{}", p.replace('\\', "/").trim_matches('/'))
}

impl SimBackend {
    pub fn new(cfg: SimConfig) -> Self {
        SimBackend {
            files: Mutex::new(HashMap::new()),
            cfg: Mutex::new(cfg),
            requests: 0.into(),
            bytes: 0.into(),
            max_seen_parallel: 0.into(),
            active: 0.into(),
            global_next: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    pub fn add_file(&self, path: &str, size: u64, seed: u64) {
        self.files.lock().insert(norm(path), SimFile { size, seed, version: format!("v1-{seed}") });
    }

    /// Simulate a remote update: new content and version for the same path.
    pub fn replace_file(&self, path: &str, new_seed: u64) {
        let mut g = self.files.lock();
        let f = g.get_mut(&norm(path)).expect("file exists");
        f.seed = new_seed;
        f.version = format!("v2-{new_seed}");
    }

    pub fn seed_of(&self, path: &str) -> u64 {
        self.files.lock()[&norm(path)].seed
    }

    pub fn set_config(&self, cfg: SimConfig) {
        *self.cfg.lock() = cfg;
    }

    pub fn reset_counters(&self) {
        self.requests.store(0, Relaxed);
        self.bytes.store(0, Relaxed);
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

#[async_trait]
impl Backend for SimBackend {
    fn name(&self) -> &str {
        "sim"
    }

    async fn stat(&self, path: &str) -> Result<FileMeta> {
        let path = norm(path);
        let g = self.files.lock();
        if let Some(f) = g.get(&path) {
            return Ok(FileMeta { path: path.clone(), id: path, size: f.size, version: f.version.clone(), is_dir: false });
        }
        let prefix = format!("{}/", path.trim_end_matches('/'));
        if path == "/" || g.keys().any(|k| k.starts_with(&prefix)) {
            return Ok(FileMeta { path: path.clone(), id: path, size: 0, version: String::new(), is_dir: true });
        }
        Err(Error::backend(BackendKind::NotFound, path))
    }

    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        let dir = norm(dir);
        let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
        let g = self.files.lock();
        let mut out: Vec<FileMeta> = Vec::new();
        let mut seen_dirs = std::collections::BTreeSet::new();
        for (k, f) in g.iter() {
            let Some(rest) = k.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((d, _)) => {
                    if seen_dirs.insert(d.to_string()) {
                        let p = format!("{prefix}{d}");
                        out.push(FileMeta { path: p.clone(), id: p, size: 0, version: String::new(), is_dir: true });
                    }
                }
                None => out.push(FileMeta { path: k.clone(), id: k.clone(), size: f.size, version: f.version.clone(), is_dir: false }),
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        let n = self.requests.fetch_add(1, Relaxed) + 1;
        let cfg = self.cfg.lock().clone();
        let act = self.active.fetch_add(1, Relaxed) + 1;
        let _g = ActiveGuard(&self.active);
        self.max_seen_parallel.fetch_max(act, Relaxed);

        if let Some(m) = cfg.max_parallel {
            if act > m {
                return Err(Error::Backend {
                    kind: BackendKind::RateLimited,
                    msg: "sim: too many parallel requests (429)".into(),
                    retry_after: Some(Duration::from_millis(50)),
                });
            }
        }
        if cfg.ratelimit_every.map_or(false, |k| n % k == 0) {
            return Err(Error::Backend {
                kind: BackendKind::RateLimited,
                msg: "sim: rate limited (429)".into(),
                retry_after: Some(Duration::from_millis(20)),
            });
        }
        if cfg.fail_every.map_or(false, |k| n % k == 0) {
            return Err(Error::backend(BackendKind::Transient, "sim: connection reset"));
        }

        let (seed, size, version) = {
            let g = self.files.lock();
            let f = g.get(&meta.path).ok_or_else(|| Error::backend(BackendKind::NotFound, meta.path.clone()))?;
            (f.seed, f.size, f.version.clone())
        };
        if version != meta.version {
            return Err(Error::backend(BackendKind::VersionChanged, "sim: remote changed"));
        }
        if offset >= size {
            return Ok(Bytes::new());
        }
        let len = len.min(size - offset);

        let jitter = if cfg.jitter.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_nanos(splitmix(n) % cfg.jitter.as_nanos() as u64)
        };
        let mut delay = cfg.latency + jitter;
        if let Some((k, extra)) = cfg.stall_nth {
            if n == k {
                delay += extra;
            }
        }
        if let Some((k, extra)) = cfg.stall_every {
            if n % k == 0 {
                delay += extra;
            }
        }
        if cfg.per_conn_bps > 0 {
            delay += Duration::from_secs_f64(len as f64 / cfg.per_conn_bps as f64);
        }
        if cfg.total_bps > 0 {
            let xfer = Duration::from_secs_f64(len as f64 / cfg.total_bps as f64);
            let mut next = self.global_next.lock().await;
            let start = (*next).max(Instant::now());
            *next = start + xfer;
            delay = delay.max((*next).saturating_duration_since(Instant::now()));
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.bytes.fetch_add(len, Relaxed);
        Ok(Bytes::from(expected(seed, offset, len as usize)))
    }
}

/// A real directory served through the Backend trait, with the same simulated
/// latency/bandwidth shaping. Lets us mount real files (executables, mmap
/// targets) while still behaving like a distant remote.
pub struct LocalDirBackend {
    root: std::path::PathBuf,
    cfg: SimConfig,
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    global_next: tokio::sync::Mutex<Instant>,
}

impl LocalDirBackend {
    pub fn new(root: impl Into<std::path::PathBuf>, cfg: SimConfig) -> Self {
        LocalDirBackend { root: root.into(), cfg, requests: 0.into(), bytes: 0.into(), global_next: tokio::sync::Mutex::new(Instant::now()) }
    }
    fn real(&self, p: &str) -> std::path::PathBuf {
        self.root.join(norm(p).trim_start_matches('/'))
    }
    fn meta_of(&self, vpath: &str, md: &std::fs::Metadata) -> FileMeta {
        let mt = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        FileMeta { path: vpath.to_string(), id: vpath.to_string(), size: if md.is_dir() { 0 } else { md.len() }, version: format!("{}-{mt}", md.len()), is_dir: md.is_dir() }
    }
}

#[async_trait]
impl Backend for LocalDirBackend {
    fn name(&self) -> &str {
        "localdir"
    }
    async fn stat(&self, path: &str) -> Result<FileMeta> {
        let p = norm(path);
        let md = std::fs::metadata(self.real(&p)).map_err(|_| Error::backend(BackendKind::NotFound, p.clone()))?;
        Ok(self.meta_of(&p, &md))
    }
    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        let d = norm(dir);
        let rd = std::fs::read_dir(self.real(&d)).map_err(|_| Error::backend(BackendKind::NotFound, d.clone()))?;
        let mut out = Vec::new();
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                let vp = format!("{}/{}", d.trim_end_matches('/'), e.file_name().to_string_lossy());
                out.push(self.meta_of(&vp, &md));
            }
        }
        Ok(out)
    }
    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        self.requests.fetch_add(1, Relaxed);
        if offset >= meta.size {
            return Ok(Bytes::new());
        }
        let len = len.min(meta.size - offset);
        let mut delay = self.cfg.latency;
        if !self.cfg.jitter.is_zero() {
            delay += Duration::from_nanos(splitmix(self.requests.load(Relaxed) ^ offset) % self.cfg.jitter.as_nanos() as u64);
        }
        if self.cfg.per_conn_bps > 0 {
            delay += Duration::from_secs_f64(len as f64 / self.cfg.per_conn_bps as f64);
        }
        if self.cfg.total_bps > 0 {
            let xfer = Duration::from_secs_f64(len as f64 / self.cfg.total_bps as f64);
            let mut next = self.global_next.lock().await;
            let start = (*next).max(Instant::now());
            *next = start + xfer;
            delay = delay.max((*next).saturating_duration_since(Instant::now()));
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let path = self.real(&meta.path);
        let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::os::windows::fs::FileExt;
            let f = std::fs::File::open(path)?;
            let mut buf = vec![0u8; len as usize];
            let mut done = 0;
            while done < buf.len() {
                let n = f.seek_read(&mut buf[done..], offset + done as u64)?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            buf.truncate(done);
            Ok(buf)
        })
        .await
        .map_err(|e| Error::Io(e.to_string()))??;
        self.bytes.fetch_add(buf.len() as u64, Relaxed);
        Ok(Bytes::from(buf))
    }
}

/// Object store backed by a local directory, with simulated upload bandwidth and
/// latency, and switchable upload failures. Stands in for Drive/S3 in volume tests.
pub struct LocalObjectStore {
    dir: std::path::PathBuf,
    pub upload_bps: AtomicU64,
    latency: Duration,
    free: Option<u64>,
    pub fail_puts: std::sync::atomic::AtomicBool,
    pub puts: AtomicU64,
    pub put_bytes: AtomicU64,
    pub gets: AtomicU64,
    pub deletes: AtomicU64,
    next: AtomicU64,
}

impl LocalObjectStore {
    pub fn new(dir: impl Into<std::path::PathBuf>, upload_bps: u64, latency: Duration, free: Option<u64>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        LocalObjectStore {
            dir,
            upload_bps: upload_bps.into(),
            latency,
            free,
            fail_puts: false.into(),
            puts: 0.into(),
            put_bytes: 0.into(),
            gets: 0.into(),
            deletes: 0.into(),
            next: 0.into(),
        }
    }
    /// Bytes currently stored "remotely".
    pub fn stored_bytes(&self) -> u64 {
        std::fs::read_dir(&self.dir).map(|d| d.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0)
    }
    pub fn object_count(&self) -> usize {
        std::fs::read_dir(&self.dir).map(|d| d.count()).unwrap_or(0)
    }
}

#[async_trait]
impl Backend for LocalObjectStore {
    fn name(&self) -> &str {
        "localobj"
    }
    async fn stat(&self, path: &str) -> Result<FileMeta> {
        Err(Error::backend(BackendKind::NotFound, path))
    }
    async fn list(&self, _dir: &str) -> Result<Vec<FileMeta>> {
        Ok(Vec::new())
    }
    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        self.gets.fetch_add(1, Relaxed);
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        let path = self.dir.join(&meta.id);
        let len = len.min(meta.size.saturating_sub(offset)) as usize;
        let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::os::windows::fs::FileExt;
            let f = std::fs::File::open(path)?;
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                let n = f.seek_read(&mut buf[done..], offset + done as u64)?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            buf.truncate(done);
            Ok(buf)
        })
        .await
        .map_err(|e| Error::Io(e.to_string()))??;
        Ok(Bytes::from(buf))
    }
}

#[async_trait]
impl sfs_core::ObjectStore for LocalObjectStore {
    async fn put_object(&self, _name: &str, data: Bytes) -> Result<sfs_core::ObjRef> {
        if self.fail_puts.load(Relaxed) {
            return Err(Error::backend(BackendKind::Transient, "localobj: upload failure injected"));
        }
        let bps = self.upload_bps.load(Relaxed);
        let mut delay = self.latency;
        if bps > 0 {
            delay += Duration::from_secs_f64(data.len() as f64 / bps as f64);
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        // The object name is kept in the file name so `list_named` can find it.
        let safe: String = _name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
        let id = format!("obj{:08}-{safe}", self.next.fetch_add(1, Relaxed));
        let md5 = {
            use md5::{Digest, Md5};
            format!("{:x}", Md5::digest(&data))
        };
        let (dir, id2, size) = (self.dir.clone(), id.clone(), data.len() as u64);
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let tmp = dir.join(format!("{id2}.tmp"));
            std::fs::write(&tmp, &data)?;
            std::fs::rename(tmp, dir.join(&id2))
        })
        .await
        .map_err(|e| Error::Io(e.to_string()))??;
        self.puts.fetch_add(1, Relaxed);
        self.put_bytes.fetch_add(size, Relaxed);
        Ok(sfs_core::ObjRef { id, size, md5 })
    }
    async fn delete_object(&self, obj: &sfs_core::ObjRef) -> Result<()> {
        self.deletes.fetch_add(1, Relaxed);
        match std::fs::remove_file(self.dir.join(&obj.id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
    async fn remote_free_bytes(&self) -> Result<Option<u64>> {
        Ok(self.free)
    }
    async fn list_named(&self, prefix: &str) -> Result<Vec<(String, sfs_core::ObjRef)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            let Some((_, name)) = id.split_once('-') else { continue };
            if !name.starts_with(prefix) || id.ends_with(".tmp") {
                continue;
            }
            let data = std::fs::read(e.path())?;
            let md5 = {
                use md5::{Digest, Md5};
                format!("{:x}", Md5::digest(&data))
            };
            out.push((name.to_string(), sfs_core::ObjRef { id, size: data.len() as u64, md5 }));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }
}
