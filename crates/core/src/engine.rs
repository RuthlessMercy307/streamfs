//! On-demand read engine.
//!
//! read(offset, len) -> blocks -> (hit | join in-flight fetch | claim + fetch).
//! A demand miss fetches only the missing blocks, rounded up to `min_fetch`
//! (latency-bound backends make 256 KiB vs 1 MiB nearly free), coalesced into
//! contiguous runs of at most `max_fetch`. Sequential access grows a
//! speculative read-ahead window; random access never triggers read-ahead.
//! A whole file is never fetched because a small region was requested.

use crate::backend::{Backend, FileMeta};
use crate::error::{BackendKind, Error, Result};
use crate::sched::{Class, Scheduler};

/// Demand spans up to this many blocks (4 MiB) are urgent; larger ones are bulk.
const URGENT_SPAN_BLOCKS: u32 = 16;
use crate::stats::{Stats, StatsSnapshot};
use crate::store::{BlockStore, StoreStats};
use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Config {
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
    pub block_size: usize,
    pub min_fetch: usize,
    pub max_fetch: usize,
    pub readahead_max: usize,
    /// First read-ahead window once sequential access is detected.
    pub readahead_init: usize,
    pub max_conns: usize,
    /// Max concurrent speculative requests (None = half of the live limit).
    pub spec_conns: Option<usize>,
    /// Promote a queued speculative fetch to critical when a blocked read joins it.
    pub promote: bool,
    /// Race a duplicate request when a request a reader is blocked on is slower than this
    /// (adaptive: at least 2x the observed mean). None disables hedging.
    pub hedge_after: Option<Duration>,
    /// Hard bound for one backend request; a timeout is retried like a network error.
    pub request_timeout: Duration,
    /// First-attempt deadline = this + 1 s per MiB (bounded by request_timeout): a request
    /// stuck on a stalled connection is abandoned and retried on another tunnel instead of
    /// holding its slot for the whole request_timeout. None = only request_timeout.
    pub stall_timeout: Option<Duration>,
    /// Hedge budget: at most this many duplicates in flight...
    pub hedge_max_inflight: usize,
    /// ...and at most this percentage of all requests (plus a burst of 4).
    pub hedge_max_percent: u64,
    pub max_retries: u32,
    pub base_backoff: Duration,
    /// Largest single speculative request: small requests drain fast, so demand never waits
    /// long behind a guess already on the wire.
    pub spec_max_fetch: usize,
    /// A speculative request still queued after this long is dropped (the guess went stale).
    pub spec_timeout: Duration,
    /// Read-ahead adds no work while this many speculative requests are queued (backpressure).
    pub spec_backlog_max: usize,
    /// Large reads: how many windows past the one being awaited are requested in parallel.
    pub bulk_lookahead: u32,
}

impl Config {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Config {
            cache_dir: cache_dir.into(),
            cache_bytes: 4 << 30,
            block_size: 256 << 10,
            min_fetch: 512 << 10,
            max_fetch: 4 << 20,
            readahead_max: 32 << 20,
            readahead_init: 1 << 20,
            max_conns: 8,
            spec_conns: None,
            promote: true,
            hedge_after: Some(Duration::from_secs(3)),
            request_timeout: Duration::from_secs(30),
            stall_timeout: Some(Duration::from_secs(8)),
            hedge_max_inflight: 4,
            hedge_max_percent: 5,
            max_retries: 6,
            base_backoff: Duration::from_millis(200),
            spec_max_fetch: 2 << 20,
            spec_timeout: Duration::from_secs(10),
            spec_backlog_max: 24,
            bulk_lookahead: 3,
        }
    }
}

/// One contiguous region read during a recorded session, in order of first access.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProfEntry {
    pub path: String,
    pub id: String,
    pub size: u64,
    /// Remote version when recorded; a different current version makes the entry stale and it is skipped.
    pub version: String,
    pub offset: u64,
    pub len: u64,
    pub t_ms: u64,
}

/// Startup read profile: which regions a program reads right after it launches.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Profile {
    pub entries: Vec<ProfEntry>,
}

impl Profile {
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).map_err(std::io::Error::other)
    }
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.len).sum()
    }
}

struct Trace {
    start: Instant,
    window: Duration,
    max_events: usize,
    events: Vec<ProfEntry>,
}

type Fin = Option<std::result::Result<(), Error>>;

struct Core {
    backend: Arc<dyn Backend>,
    store: Arc<BlockStore>,
    cfg: Config,
    inflight: Mutex<HashMap<(u32, u32), (watch::Receiver<Fin>, Arc<AtomicBool>)>>,
    sched: Arc<Scheduler>,
    stats: Stats,
    min_blocks: u32,
    max_blocks: u32,
    spec_blocks: u32,
    /// run size for bulk data and for splitting large blocked reads (~1 MiB)
    split_blocks: u32,
    win_blocks: u32,
    trace: Mutex<Option<Trace>>,
    read_log: std::sync::OnceLock<Arc<crate::readlog::ReadLog>>,
    started: Instant,
    hedges_inflight: std::sync::atomic::AtomicUsize,
    /// ms since `started` of the last rate-limit response (0 = never).
    last_rate_limit_ms: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct Engine {
    core: Arc<Core>,
}

struct Run {
    start: u32,
    senders: Vec<watch::Sender<Fin>>,
    promoted: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(backend: Arc<dyn Backend>, cfg: Config) -> Result<Self> {
        let store = Arc::new(BlockStore::open(&cfg.cache_dir, cfg.block_size, cfg.cache_bytes)?);
        let bs = cfg.block_size;
        let max_slots = (cfg.cache_bytes / bs as u64).max(1);
        let core = Core {
            sched: Scheduler::new(cfg.max_conns.max(1), cfg.spec_conns),
            min_blocks: (cfg.min_fetch / bs).max(1) as u32,
            max_blocks: (cfg.max_fetch / bs).max(1) as u32,
            spec_blocks: (cfg.spec_max_fetch / bs).max(1) as u32,
            split_blocks: ((1 << 20) / bs).clamp(1, (cfg.max_fetch / bs).max(1)) as u32,
            win_blocks: (max_slots / 4).clamp(1, 64) as u32,
            inflight: Mutex::new(HashMap::new()),
            trace: Mutex::new(None),
            read_log: std::sync::OnceLock::new(),
            started: Instant::now(),
            hedges_inflight: 0.into(),
            last_rate_limit_ms: 0.into(),
            stats: Stats::new(),
            backend,
            store,
            cfg,
        };
        Ok(Engine { core: Arc::new(core) })
    }

    pub async fn open(&self, path: &str) -> Result<Handle> {
        let meta = self.core.backend.stat(path).await?;
        Ok(self.open_meta(meta))
    }

    /// Put the full content of a remote object into the read cache (best effort: a
    /// full cache just stops seeding). Used right after an upload, so data that was
    /// just written is not fetched back from the remote when it is re-read.
    pub fn seed(&self, meta: &FileMeta, data: &[u8]) -> usize {
        let key = format!("{}:{}", self.core.backend.name(), meta.id);
        let f = self.core.store.open_file(&key, &meta.version, meta.size);
        let mut seeded = 0;
        for (i, chunk) in data.chunks(self.core.cfg.block_size).enumerate() {
            if self.core.store.insert(f, i as u32, chunk).is_err() {
                break;
            }
            seeded += chunk.len();
        }
        seeded
    }

    pub fn open_meta(&self, meta: FileMeta) -> Handle {
        let key = format!("{}:{}", self.core.backend.name(), meta.id);
        let f = self.core.store.open_file(&key, &meta.version, meta.size);
        Handle {
            core: self.core.clone(),
            meta: Arc::new(meta),
            f,
            seq: Mutex::new(Seq { next: 0, window: 0, ra_hi: 0, ra_lo: 0 }),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.core.backend
    }
    pub fn stats(&self) -> StatsSnapshot {
        self.core.stats.snapshot()
    }
    pub fn reset_latency(&self) {
        self.core.stats.reset_latency()
    }
    pub fn store_stats(&self) -> StoreStats {
        self.core.store.stats()
    }
    pub fn store(&self) -> &Arc<BlockStore> {
        &self.core.store
    }
    pub fn concurrency_limit(&self) -> usize {
        self.core.sched.limit()
    }
    pub fn sched_snapshot(&self) -> crate::sched::SchedSnapshot {
        self.core.sched.snapshot()
    }

    /// Record reads for `window` (the startup phase), keeping at most `max_events` merged regions.
    pub fn start_trace(&self, window: Duration, max_events: usize) {
        *self.core.trace.lock() = Some(Trace { start: Instant::now(), window, max_events, events: Vec::new() });
    }

    /// Log every application read from now on (see readlog). Set once.
    pub fn set_read_log(&self, log: Arc<crate::readlog::ReadLog>) {
        let _ = self.core.read_log.set(log);
    }

    pub fn read_log(&self) -> Option<Arc<crate::readlog::ReadLog>> {
        self.core.read_log.get().cloned()
    }

    pub fn trace_snapshot(&self) -> Profile {
        Profile { entries: self.core.trace.lock().as_ref().map(|t| t.events.clone()).unwrap_or_default() }
    }

    /// Queue speculative fetches for the regions in `profile`, in recorded order, up to `budget_bytes`.
    /// Entries whose remote version changed are skipped (profile invalidated by update).
    /// The returned handles must be kept alive while the prefetch should proceed:
    /// dropping them cancels speculative requests that have not started yet.
    pub async fn prefetch(&self, profile: &Profile, budget_bytes: u64) -> Vec<Handle> {
        let mut handles: HashMap<String, Option<Handle>> = HashMap::new();
        let mut spent = 0u64;
        for e in &profile.entries {
            if spent >= budget_bytes {
                break;
            }
            if !handles.contains_key(&e.path) {
                let h = match self.core.backend.stat(&e.path).await {
                    Ok(m) if m.version == e.version && m.size == e.size => Some(self.open_meta(m)),
                    _ => None, // missing or changed since recording
                };
                handles.insert(e.path.clone(), h);
            }
            if let Some(Some(h)) = handles.get(&e.path) {
                let len = e.len.min(budget_bytes - spent);
                h.prefetch_range(e.offset, len);
                spent += len;
            }
        }
        handles.into_values().flatten().collect()
    }
}

impl Core {
    fn ms_since_start(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn record(&self, meta: &FileMeta, offset: u64, len: u64) {
        let mut g = self.trace.lock();
        let Some(t) = g.as_mut() else { return };
        // The window opens at the first read, not at mount time: a drive is often mounted
        // long before the program that uses it starts.
        if t.events.is_empty() {
            t.start = Instant::now();
        }
        let el = t.start.elapsed();
        if el > t.window {
            return;
        }
        // Merge with the previous region when it continues or overlaps it (games read in small chunks).
        if let Some(last) = t.events.last_mut() {
            if last.path == meta.path && offset <= last.offset + last.len + (64 << 10) && offset + len >= last.offset {
                let end = (last.offset + last.len).max(offset + len);
                last.offset = last.offset.min(offset);
                last.len = end - last.offset;
                return;
            }
        }
        if t.events.len() < t.max_events {
            t.events.push(ProfEntry {
                path: meta.path.clone(),
                id: meta.id.clone(),
                size: meta.size,
                version: meta.version.clone(),
                offset,
                len,
                t_ms: el.as_millis() as u64,
            });
        }
    }
}

struct Seq {
    next: u64,
    window: u64,
    /// Highest block already claimed by read-ahead for the current sequential run.
    ra_hi: u32,
    /// First block of the last read-ahead batch not yet reached by the reader (0 = none).
    ra_lo: u32,
}

pub struct Handle {
    core: Arc<Core>,
    meta: Arc<FileMeta>,
    f: u32,
    seq: Mutex<Seq>,
    closed: Arc<AtomicBool>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Pending speculative fetches for this handle stop before hitting the network.
        self.closed.store(true, Relaxed);
    }
}

impl Handle {
    pub fn meta(&self) -> &FileMeta {
        &self.meta
    }

    /// Never evict this file's blocks (boot-critical files, user pins).
    pub fn set_pinned(&self, pinned: bool) {
        self.core.store.set_pinned(self.f, pinned);
    }

    fn last_block(&self) -> u32 {
        let bs = self.core.cfg.block_size as u64;
        ((self.meta.size + bs - 1) / bs).saturating_sub(1) as u32
    }

    /// Claim every missing, not-in-flight block in [lo, hi]; returns the new
    /// fetch runs and receivers to await for blocks <= wait_hi.
    fn claim(&self, lo: u32, hi: u32, wait_hi: Option<u32>, run_max: u32) -> (Vec<Run>, Vec<watch::Receiver<Fin>>) {
        let mut runs: Vec<Run> = Vec::new();
        let mut waits = Vec::new();
        let mut prev_claimed: Option<u32> = None;
        let mut promoted_any = false;
        let mut inf = self.core.inflight.lock();
        for b in lo..=hi {
            let key = (self.f, b);
            // In-flight first, then store: a fetch inserts into the store
            // before it leaves the map, so "not in flight" + "not stored" is a true miss.
            if let Some((rx, pr)) = inf.get(&key) {
                if wait_hi.map_or(false, |w| b <= w) {
                    waits.push(rx.clone());
                    // A blocked reader now depends on this fetch: it is critical work.
                    if self.core.cfg.promote && !pr.swap(true, Relaxed) {
                        promoted_any = true;
                        self.core.stats.promoted_joins.fetch_add(1, Relaxed);
                    }
                }
                prev_claimed = None;
                continue;
            }
            if self.core.store.contains(self.f, b) {
                prev_claimed = None;
                continue;
            }
            let (tx, rx) = watch::channel(None);
            let extend = matches!((prev_claimed, runs.last()), (Some(p), Some(run)) if p + 1 == b && (run.senders.len() as u32) < run_max);
            if !extend {
                runs.push(Run { start: b, senders: Vec::new(), promoted: Arc::new(AtomicBool::new(false)) });
            }
            let run = runs.last_mut().unwrap();
            run.senders.push(tx);
            inf.insert(key, (rx.clone(), run.promoted.clone()));
            if wait_hi.map_or(false, |w| b <= w) {
                waits.push(rx);
            }
            prev_claimed = Some(b);
        }
        drop(inf);
        if promoted_any {
            self.core.sched.wake();
        }
        (runs, waits)
    }

    fn spawn_runs(&self, runs: Vec<Run>, class: Class) {
        for run in runs {
            let core = self.core.clone();
            let meta = self.meta.clone();
            let closed = self.closed.clone();
            let f = self.f;
            tokio::spawn(async move { fetch_run(core, meta, f, run, class, closed).await });
        }
    }

    /// Claim and start fetching [lo, hi] at `class` without waiting for it.
    fn start(&self, lo: u32, hi: u32, class: Class) {
        let run_max = match class {
            Class::Spec => self.core.spec_blocks,
            Class::Bulk => self.core.split_blocks,
            Class::Urgent => self.core.max_blocks,
        };
        let (runs, _) = self.claim(lo, hi, None, run_max);
        self.spawn_runs(runs, class);
    }

    /// Speculatively fetch [offset, offset+len) without waiting. Never blocks a reader.
    pub fn prefetch_range(&self, offset: u64, len: u64) {
        if len == 0 || offset >= self.meta.size {
            return;
        }
        let bs = self.core.cfg.block_size as u64;
        let end = (offset + len).min(self.meta.size);
        let (lo, hi) = ((offset / bs) as u32, ((end - 1) / bs) as u32);
        self.start(lo, hi, Class::Spec);
    }

    /// Make blocks [wb0, wb1] resident. Returns how many were missing.
    /// A small span is urgent; a large one is bulk, so a big read never starves the small
    /// reads other threads are blocked on.
    async fn ensure(&self, wb0: u32, wb1: u32) -> Result<u32> {
        let hi = wb1.max(wb0 + self.core.min_blocks - 1).min(self.last_block());
        let class = if wb1 - wb0 < URGENT_SPAN_BLOCKS { Class::Urgent } else { Class::Bulk };
        // A blocked multi-megabyte read is split into ~1 MiB runs fetched in parallel over the
        // tunnels, instead of one large request that moves at a single connection's speed.
        let run_max = if hi - wb0 + 1 > self.core.split_blocks * 2 { self.core.split_blocks } else { self.core.max_blocks };
        let mut missing_first = None;
        // A speculative fetch we joined can be cancelled (stale/closed) in the instant before
        // our join promoted it: claim again instead of failing the read.
        for _ in 0..4 {
            let (runs, waits) = self.claim(wb0, hi, Some(wb1), run_max);
            missing_first.get_or_insert(waits.len() as u32);
            self.spawn_runs(runs, class);
            let mut cancelled = false;
            let _blocked = (!waits.is_empty()).then(|| self.core.stats.blocked_begin());
            for mut rx in waits {
                let v = rx.wait_for(|v| v.is_some()).await.map_err(|_| Error::Cancelled)?;
                match v.as_ref() {
                    Some(Err(Error::Cancelled)) => cancelled = true,
                    Some(Err(e)) => return Err(e.clone()),
                    _ => {}
                }
            }
            if !cancelled {
                break;
            }
        }
        Ok(missing_first.unwrap_or(0))
    }

    pub async fn read(&self, offset: u64, len: usize) -> Result<Bytes> {
        self.read_detailed(offset, len).await.map(|(b, _)| b)
    }

    /// Like `read`, also returning how many blocks the read had to wait for (0 = served
    /// entirely from the cache).
    pub async fn read_detailed(&self, offset: u64, len: usize) -> Result<(Bytes, u32)> {
        let t0 = Instant::now();
        self.core.record(&self.meta, offset, len as u64);
        let size = self.meta.size;
        if offset >= size || len == 0 {
            return Ok((Bytes::new(), 0));
        }
        let end = (offset + len as u64).min(size);
        let bs = self.core.cfg.block_size as u64;
        let (b0, b1) = ((offset / bs) as u32, ((end - 1) / bs) as u32);
        let win = self.core.win_blocks;
        let mut out = Vec::with_capacity((end - offset) as usize);

        let mut wb0 = b0;
        let (mut total_missed, mut attempts) = (0u32, 0u32);
        let mut ahead_hi = 0u32; // highest block already requested by bulk lookahead
        while wb0 <= b1 {
            let wb1 = (wb0 + win - 1).min(b1);
            // Large read: keep the next windows in flight while this one is awaited, so a
            // 200 MiB read moves at link speed instead of one window per round trip.
            if wb1 < b1 && self.core.cfg.bulk_lookahead > 0 {
                let la_hi = (wb1 + win * self.core.cfg.bulk_lookahead).min(b1);
                if la_hi > ahead_hi {
                    self.start((wb1 + 1).max(ahead_hi + 1), la_hi, Class::Bulk);
                    ahead_hi = la_hi;
                }
            }
            let (s, e) = (offset.max(wb0 as u64 * bs), end.min((wb1 as u64 + 1) * bs));
            let mut piece = None;
            for attempt in 0..5 {
                let missed = self.ensure(wb0, wb1).await?;
                total_missed += missed;
                attempts += 1;
                if attempt == 0 {
                    self.core.stats.blocks_missed.fetch_add(missed as u64, Relaxed);
                    self.core.stats.blocks_hit.fetch_add((wb1 - wb0 + 1 - missed) as u64, Relaxed);
                }
                let store = self.core.store.clone();
                let f = self.f;
                let (p, corrupt) = tokio::task::spawn_blocking(move || assemble(&store, f, wb0, wb1, s, e, bs))
                    .await
                    .map_err(|e| Error::Io(e.to_string()))?;
                self.core.stats.corrupt_blocks.fetch_add(corrupt, Relaxed);
                if p.is_some() {
                    piece = p;
                    break;
                }
            }
            match piece {
                Some(p) => out.extend_from_slice(&p),
                None => return Err(Error::Io("blocks evicted or corrupt repeatedly during read".into())),
            }
            wb0 = wb1 + 1;
        }

        self.readahead(offset, end);
        let st = &self.core.stats;
        st.reads.fetch_add(1, Relaxed);
        st.bytes_served.fetch_add(out.len() as u64, Relaxed);
        let us = t0.elapsed().as_micros() as u64;
        st.record_latency(us);
        st.record_read(end - offset, us, total_missed > 0);
        if let Some(l) = self.core.read_log.get() {
            l.log(&self.meta, t0, offset, end - offset, us, total_missed);
        }
        if total_missed > 0 {
            // Time a caller actually spent blocked on remote data.
            st.blocked_reads.fetch_add(1, Relaxed);
            st.blocked_us_sum.fetch_add(us, Relaxed);
            st.record_blocked(us);
        }
        if us >= 2_000_000 {
            eprintln!(
                "{} [slow-read] {} off={} len={} took={}ms waited_blocks={} attempts={} limit={}",
                ts(), self.meta.path, offset, len, us / 1000, total_missed, attempts, self.core.sched.limit()
            );
        }
        Ok((Bytes::from(out), total_missed))
    }

    fn readahead(&self, offset: u64, end: u64) {
        let cfg = &self.core.cfg;
        if cfg.readahead_max == 0 {
            return;
        }
        let bs = cfg.block_size as u64;
        let cur = ((end - 1) / bs) as u32;
        let (lo, hi) = {
            let mut s = self.seq.lock();
            // The window starts small and doubles only after the reader actually consumed the
            // previous batch of guesses; a jump resets it. Programs that read a little and then
            // seek (archive formats: header, entry, jump) cost one small guess, not 8-32 MiB.
            let cap = (cfg.readahead_max as u64).min((cfg.cache_bytes / 8).max(cfg.block_size as u64));
            if offset == s.next {
                if s.window == 0 {
                    s.window = (cfg.readahead_init as u64).min(cap);
                } else if s.ra_lo > 0 && cur >= s.ra_lo {
                    s.window = (s.window * 2).min(cap);
                    s.ra_lo = 0;
                }
            } else {
                s.window = 0;
                s.ra_hi = 0;
                s.ra_lo = 0;
            }
            s.next = end;
            if s.window == 0 || end >= self.meta.size {
                return;
            }
            let win_blocks = (s.window / bs).max(1) as u32;
            let have_ahead = s.ra_hi.saturating_sub(cur);
            // Refill only when the prefetched lead drops below half the window,
            // so requests stay large (latency-bound backends) instead of 1 block each.
            if s.ra_hi >= cur && have_ahead >= win_blocks / 2 {
                return;
            }
            let lo = s.ra_hi.max(cur) + 1;
            let hi = (cur + win_blocks).min(self.last_block());
            if lo > hi {
                return;
            }
            // Backpressure: while guesses are already queued, don't add more; the window is
            // refilled on a later read (ra_hi is left unchanged).
            if self.core.sched.spec_backlog() >= cfg.spec_backlog_max {
                return;
            }
            s.ra_hi = hi;
            s.ra_lo = lo;
            (lo, hi)
        };
        self.start(lo, hi, Class::Spec);
    }
}

/// Copy [s, e) out of resident blocks. `None` => a block vanished or failed its CRC.
fn assemble(store: &BlockStore, f: u32, b0: u32, b1: u32, s: u64, e: u64, bs: u64) -> (Option<Vec<u8>>, u64) {
    let mut out = vec![0u8; (e - s) as usize];
    let mut buf = vec![0u8; bs as usize];
    for b in b0..=b1 {
        let Some(p) = store.pin(f, b) else { return (None, 0) };
        let blen = p.len as usize;
        let ok = store.read_pinned(&p, &mut buf[..blen]);
        store.unpin(p, !ok);
        if !ok {
            return (None, 1);
        }
        let bstart = b as u64 * bs;
        let (lo, hi) = (s.max(bstart), e.min(bstart + blen as u64));
        if lo < hi {
            out[(lo - s) as usize..(hi - s) as usize]
                .copy_from_slice(&buf[(lo - bstart) as usize..(hi - bstart) as usize]);
        }
    }
    (Some(out), 0)
}

async fn fetch_run(core: Arc<Core>, meta: Arc<FileMeta>, f: u32, run: Run, class: Class, closed: Arc<AtomicBool>) {
    let n = run.senders.len() as u32;
    let res = do_fetch(&core, &meta, f, run.start, n, class, &closed, &run.promoted).await;
    {
        let mut inf = core.inflight.lock();
        for i in 0..n {
            inf.remove(&(f, run.start + i));
        }
    }
    for tx in run.senders {
        let _ = tx.send(Some(res.clone()));
    }
}

fn backoff(core: &Core, attempt: u32) -> Duration {
    let exp = core.cfg.base_backoff * 2u32.saturating_pow(attempt.min(6));
    let capped = exp.min(Duration::from_secs(8));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    capped / 2 + Duration::from_nanos(nanos % (capped.as_nanos() as u64 / 2 + 1)) // equal jitter
}

/// One backend request, bounded by `request_timeout`. If a blocked reader depends
/// on it (critical, or speculative-but-promoted) and it is slower than the hedge
/// delay, a duplicate request is raced against it and the first answer wins.
/// Remote tail latency (single requests taking several times the average) then
/// costs ~hedge delay + one request instead of the whole tail.
async fn request_with_hedge(
    core: &Arc<Core>,
    meta: &Arc<FileMeta>,
    off: u64,
    len: u64,
    spec: bool,
    promoted: &AtomicBool,
    lane_urgent: bool,
    attempt: u32,
    bulk: bool,
) -> Result<Bytes> {
    // A request stuck on a stalled connection would hold its slot for the whole
    // request_timeout. The first attempt gets a deadline sized for a slow-but-alive link
    // (8 s + 1 s/MiB, well past the 3 s hedge, so a link that is merely slow everywhere is
    // not made slower by restarts); a retry goes out on another tunnel with the full
    // timeout, so a link slower than that costs one retry, never a failed read.
    let timeout = match core.cfg.stall_timeout {
        Some(base) if attempt == 0 => {
            core.cfg.request_timeout.min(base + Duration::from_micros(len))
        }
        _ => core.cfg.request_timeout,
    };
    let timed = |fut| async move {
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::backend(BackendKind::Transient, format!("request timed out after {timeout:?}"))),
        }
    };
    let primary = timed(core.backend.read_range_class(meta, off, len, lane_urgent));
    tokio::pin!(primary);
    let Some(base) = core.cfg.hedge_after else { return primary.await };
    // Adaptive delay: never below `base`, and at least 2x the observed mean.
    let n = core.stats.backend_requests.load(Relaxed).max(1);
    let mean = Duration::from_micros(core.stats.backend_req_us_sum.load(Relaxed) / n);
    let delay = base.max(mean * 2);
    tokio::select! {
        r = &mut primary => r,
        _ = tokio::time::sleep(delay) => {
            if spec && !promoted.load(Relaxed) {
                return primary.await; // nobody is blocked on it: don't spend quota
            }
            if bulk {
                // the few hedge slots are for small blocked reads; a stuck bulk run is
                // recovered by the stall deadline above
                return primary.await;
            }
            // Budget: duplicates must never become a request storm or provoke throttling.
            let hedged = core.stats.hedged_requests.load(Relaxed);
            let total = core.stats.backend_requests.load(Relaxed);
            let recent_429 = core.ms_since_start().saturating_sub(core.last_rate_limit_ms.load(Relaxed)) < 60_000
                && core.last_rate_limit_ms.load(Relaxed) != 0;
            let over_ratio = hedged * 100 > total * core.cfg.hedge_max_percent + 400; // pct + burst of 4
            if recent_429 || over_ratio || core.hedges_inflight.load(Relaxed) >= core.cfg.hedge_max_inflight {
                core.stats.hedges_skipped.fetch_add(1, Relaxed);
                return primary.await;
            }
            core.hedges_inflight.fetch_add(1, Relaxed);
            core.stats.hedged_requests.fetch_add(1, Relaxed);
            eprintln!("{} [hedge] {} off={} len={} primary still running after {}ms; racing a duplicate", ts(), meta.path, off, len, delay.as_millis());
            // the duplicate goes to another lane: if the urgent lane is the slow one, this wins
            let second = timed(core.backend.read_range_class(meta, off, len, false));
            tokio::pin!(second);
            // The first *successful* answer wins: if one copy fails (e.g. its stall deadline
            // fired) the other may still be about to deliver, so wait for it instead.
            let r = tokio::select! {
                r = &mut primary => match r {
                    Ok(v) => Ok(v),
                    Err(_) => { let r2 = second.await; if r2.is_ok() { core.stats.hedge_wins.fetch_add(1, Relaxed); } r2 }
                },
                r = &mut second => match r {
                    Ok(v) => { core.stats.hedge_wins.fetch_add(1, Relaxed); Ok(v) }
                    Err(_) => primary.await,
                }
            };
            core.hedges_inflight.fetch_sub(1, Relaxed);
            r
        }
    }
}

async fn do_fetch(
    core: &Arc<Core>,
    meta: &Arc<FileMeta>,
    f: u32,
    start: u32,
    n: u32,
    class: Class,
    closed: &AtomicBool,
    promoted: &AtomicBool,
) -> Result<()> {
    let spec = class == Class::Spec;
    let bs = core.cfg.block_size as u64;
    let off = start as u64 * bs;
    let len = (n as u64 * bs).min(meta.size - off);
    let mut attempt = 0u32;
    loop {
        // A speculative fetch that a blocked reader joined is no longer speculative.
        let spec_now = spec && !promoted.load(Relaxed);
        if spec_now && closed.load(Relaxed) {
            core.stats.spec_cancelled.fetch_add(1, Relaxed);
            return Err(Error::Cancelled);
        }
        let t_acq = Instant::now();
        let Some(permit) = core.sched.acquire(class, len, Some(promoted), spec.then_some(core.cfg.spec_timeout)).await else {
            core.stats.spec_cancelled.fetch_add(1, Relaxed);
            return Err(Error::Cancelled); // stale guess: never started, nobody waits on it
        };
        let acq_us = t_acq.elapsed().as_micros() as u64;
        let acq_ms = acq_us / 1000;
        core.stats.req_queue_us_sum.fetch_add(acq_us, Relaxed);
        let ran_as = permit.class();
        core.stats.backend_requests.fetch_add(1, Relaxed);
        match class {
            Class::Spec => core.stats.speculative_requests.fetch_add(1, Relaxed),
            Class::Bulk => core.stats.bulk_requests.fetch_add(1, Relaxed),
            Class::Urgent => 0,
        };
        let t_req = Instant::now();
        // The reserved urgent lane carries only small demand requests: a megabyte-sized run on
        // it would queue the next small blocked read behind it inside the same connection.
        let lane_urgent = (!spec || promoted.load(Relaxed)) && class != Class::Bulk && len <= core.cfg.min_fetch as u64;
        let r = request_with_hedge(core, meta, off, len, spec, promoted, lane_urgent, attempt, class == Class::Bulk).await;
        let us = t_req.elapsed().as_micros() as u64;
        if acq_ms >= 2000 || us >= 3_000_000 {
            eprintln!(
                "{} [slow-fetch] {} off={} len={} queued={}ms request={}ms class={:?} ran_as={:?} ok={}",
                ts(), meta.path, off, len, acq_ms, us / 1000, class, ran_as, r.is_ok()
            );
        }
        if ran_as == Class::Urgent && r.is_ok() {
            // a promoted guess queued as speculative before anyone waited: that wait isn't urgent queueing
            core.stats.record_urgent(if class == Class::Urgent { acq_us } else { 0 }, us);
            // only requests of the usual demand size feed the congestion signal
            if len <= core.cfg.min_fetch as u64 * 2 {
                core.sched.on_urgent_latency(Duration::from_micros(us));
            }
        }
        core.stats.backend_req_us_max.fetch_max(us, Relaxed);
        core.stats.backend_req_us_sum.fetch_add(us, Relaxed);
        drop(permit);
        match r {
            Ok(data) => {
                if data.len() as u64 != len {
                    return Err(Error::ShortRead { want: len, got: data.len() as u64 });
                }
                core.sched.on_ok();
                core.stats.backend_bytes.fetch_add(len, Relaxed);
                if spec {
                    core.stats.speculative_bytes.fetch_add(len, Relaxed);
                }
                let store = core.store.clone();
                // guesses enter the cache on probation: never read => evicted first
                let probation = spec && !promoted.load(Relaxed);
                let t_store = Instant::now();
                let r = tokio::task::spawn_blocking(move || -> Result<()> {
                    for i in 0..n as u64 {
                        let a = (i * bs) as usize;
                        let z = ((i + 1) * bs).min(data.len() as u64) as usize;
                        store.insert_class(f, start + i as u32, &data[a..z], probation)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|e| Error::Io(e.to_string()))?;
                core.stats.store_us_sum.fetch_add(t_store.elapsed().as_micros() as u64, Relaxed);
                return r;
            }
            Err(e) => {
                core.stats.backend_errors.fetch_add(1, Relaxed);
                let wait = match &e {
                    Error::Backend { kind: BackendKind::RateLimited, retry_after, .. } => {
                        core.sched.on_rate_limited();
                        core.last_rate_limit_ms.store(core.ms_since_start().max(1), Relaxed);
                        core.stats.rate_limited.fetch_add(1, Relaxed);
                        retry_after.unwrap_or_else(|| backoff(core, attempt))
                    }
                    Error::Backend { kind: BackendKind::Transient, .. } => backoff(core, attempt),
                    Error::Backend { kind: BackendKind::VersionChanged, .. } => {
                        core.stats.version_changed.fetch_add(1, Relaxed);
                        return Err(e); // the file changed remotely: never mix versions
                    }
                    _ => return Err(e), // NotFound / Quota / VersionChanged / Fatal: surface, don't hammer
                };
                if (spec && !promoted.load(Relaxed)) || attempt >= core.cfg.max_retries {
                    return Err(e);
                }
                attempt += 1;
                core.stats.retries.fetch_add(1, Relaxed);
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// UTC wall clock for log lines (HH:MM:SS.mmm), to correlate with game logs and crashes.
fn ts() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", s / 3600, s / 60 % 60, s % 60, d.subsec_millis())
}
