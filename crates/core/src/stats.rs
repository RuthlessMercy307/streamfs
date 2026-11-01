use hdrhistogram::Histogram;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub struct Stats {
    pub reads: AtomicU64,
    pub bytes_served: AtomicU64,
    pub blocks_hit: AtomicU64,
    pub blocks_missed: AtomicU64,
    pub backend_requests: AtomicU64,
    pub backend_bytes: AtomicU64,
    pub speculative_requests: AtomicU64,
    pub speculative_bytes: AtomicU64,
    pub retries: AtomicU64,
    pub rate_limited: AtomicU64,
    pub backend_errors: AtomicU64,
    pub corrupt_blocks: AtomicU64,
    pub promoted_joins: AtomicU64,
    pub backend_req_us_max: AtomicU64,
    pub backend_req_us_sum: AtomicU64,
    /// Reads that had to wait for remote data, and the total time they waited.
    pub blocked_reads: AtomicU64,
    pub blocked_us_sum: AtomicU64,
    pub hedged_requests: AtomicU64,
    pub hedge_wins: AtomicU64,
    pub hedges_skipped: AtomicU64,
    pub bulk_requests: AtomicU64,
    /// Speculative requests dropped before reaching the network (stale, handle closed).
    pub spec_cancelled: AtomicU64,
    pub version_changed: AtomicU64,
    lat_us: Mutex<Histogram<u64>>,
    /// Latency of reads that waited for remote data (what the application feels).
    blocked_us: Mutex<Histogram<u64>>,
    /// Urgent requests: time waiting for a slot vs time on the wire.
    urgent_queue_us: Mutex<Histogram<u64>>,
    urgent_xfer_us: Mutex<Histogram<u64>>,
    /// Application reads (one per Handle::read call = one filesystem read request) by size.
    sizes: Mutex<Vec<SizeClass>>,
    /// Wall time during which at least one read was blocked on remote data: the union of
    /// the blocked intervals, so parallel waits are not added up.
    blocked_wall: Mutex<BlockedWall>,
    /// Backend requests of every class: time waiting for a scheduler slot and on the wire.
    pub req_queue_us_sum: AtomicU64,
    /// Client-side time to verify/insert fetched blocks into the cache.
    pub store_us_sum: AtomicU64,
}

/// Size buckets of application reads (upper bounds, exclusive; the last is open).
pub const SIZE_BUCKETS: [(&str, u64); 7] = [
    ("<4K", 4 << 10),
    ("4K-64K", 64 << 10),
    ("64K-1M", 1 << 20),
    ("1M-4M", 4 << 20),
    ("4M-16M", 16 << 20),
    ("16M-64M", 64 << 20),
    (">=64M", u64::MAX),
];

struct SizeClass {
    count: u64,
    bytes: u64,
    blocked: u64,
    blocked_us_sum: u64,
    hit_us: Histogram<u64>,
    miss_us: Histogram<u64>,
}

#[derive(Default)]
struct BlockedWall {
    active: u32,
    since: Option<std::time::Instant>,
    total_us: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SizeClassSnapshot {
    pub class: &'static str,
    pub reads: u64,
    pub bytes: u64,
    /// reads that waited for remote data
    pub blocked: u64,
    pub blocked_ms_sum: u64,
    pub hit_p50_us: u64,
    pub hit_p95_us: u64,
    pub hit_p99_us: u64,
    pub hit_max_us: u64,
    pub miss_p50_us: u64,
    pub miss_p95_us: u64,
    pub miss_p99_us: u64,
    pub miss_max_us: u64,
}

/// Held while a read waits for remote data (see Stats::blocked_wall).
pub struct BlockedGuard<'a>(&'a Stats);
impl Drop for BlockedGuard<'_> {
    fn drop(&mut self) {
        let mut w = self.0.blocked_wall.lock();
        w.active -= 1;
        if w.active == 0 {
            if let Some(s) = w.since.take() {
                w.total_us += s.elapsed().as_micros() as u64;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    pub reads: u64,
    pub bytes_served: u64,
    pub blocks_hit: u64,
    pub blocks_missed: u64,
    pub backend_requests: u64,
    pub backend_bytes: u64,
    pub speculative_requests: u64,
    pub speculative_bytes: u64,
    pub retries: u64,
    pub rate_limited: u64,
    pub backend_errors: u64,
    pub corrupt_blocks: u64,
    pub promoted_joins: u64,
    pub backend_req_us_max: u64,
    pub backend_req_us_avg: u64,
    pub blocked_reads: u64,
    pub blocked_ms_total: u64,
    pub hedged_requests: u64,
    pub hedge_wins: u64,
    pub hedges_skipped: u64,
    pub bulk_requests: u64,
    pub spec_cancelled: u64,
    pub version_changed: u64,
    pub blocked_p50_us: u64,
    pub blocked_p95_us: u64,
    pub blocked_p99_us: u64,
    pub blocked_max_us: u64,
    pub urgent_queue_p50_us: u64,
    pub urgent_queue_p99_us: u64,
    pub urgent_queue_max_us: u64,
    pub urgent_xfer_p50_us: u64,
    pub urgent_xfer_p99_us: u64,
    pub urgent_xfer_max_us: u64,
    pub lat_p50_us: u64,
    pub lat_p95_us: u64,
    pub lat_p99_us: u64,
    pub lat_max_us: u64,
    /// union of blocked intervals (real "something is frozen on I/O" time)
    pub blocked_wall_ms: u64,
    pub req_queue_ms_total: u64,
    pub store_ms_total: u64,
    pub sizes: Vec<SizeClassSnapshot>,
}

impl Stats {
    pub fn new() -> Self {
        Stats {
            reads: 0.into(),
            bytes_served: 0.into(),
            blocks_hit: 0.into(),
            blocks_missed: 0.into(),
            backend_requests: 0.into(),
            backend_bytes: 0.into(),
            speculative_requests: 0.into(),
            speculative_bytes: 0.into(),
            retries: 0.into(),
            rate_limited: 0.into(),
            backend_errors: 0.into(),
            corrupt_blocks: 0.into(),
            promoted_joins: 0.into(),
            backend_req_us_max: 0.into(),
            backend_req_us_sum: 0.into(),
            blocked_reads: 0.into(),
            blocked_us_sum: 0.into(),
            hedged_requests: 0.into(),
            hedge_wins: 0.into(),
            hedges_skipped: 0.into(),
            bulk_requests: 0.into(),
            spec_cancelled: 0.into(),
            version_changed: 0.into(),
            lat_us: Mutex::new(hist()),
            blocked_us: Mutex::new(hist()),
            urgent_queue_us: Mutex::new(hist()),
            urgent_xfer_us: Mutex::new(hist()),
            sizes: Mutex::new(
                SIZE_BUCKETS
                    .iter()
                    .map(|_| SizeClass { count: 0, bytes: 0, blocked: 0, blocked_us_sum: 0, hit_us: hist(), miss_us: hist() })
                    .collect(),
            ),
            blocked_wall: Mutex::new(BlockedWall::default()),
            req_queue_us_sum: 0.into(),
            store_us_sum: 0.into(),
        }
    }

    /// One application read finished: `missed` = it waited for remote data.
    pub fn record_read(&self, len: u64, us: u64, missed: bool) {
        let i = SIZE_BUCKETS.iter().position(|(_, hi)| len < *hi).unwrap_or(SIZE_BUCKETS.len() - 1);
        let mut g = self.sizes.lock();
        let c = &mut g[i];
        c.count += 1;
        c.bytes += len;
        let us = us.clamp(1, 600_000_000);
        if missed {
            c.blocked += 1;
            c.blocked_us_sum += us;
            let _ = c.miss_us.record(us);
        } else {
            let _ = c.hit_us.record(us);
        }
    }

    pub fn blocked_begin(&self) -> BlockedGuard<'_> {
        let mut w = self.blocked_wall.lock();
        if w.active == 0 {
            w.since = Some(std::time::Instant::now());
        }
        w.active += 1;
        BlockedGuard(self)
    }

    pub fn record_latency(&self, us: u64) {
        let _ = self.lat_us.lock().record(us.clamp(1, 600_000_000));
    }

    pub fn record_blocked(&self, us: u64) {
        let _ = self.blocked_us.lock().record(us.clamp(1, 600_000_000));
    }

    pub fn record_urgent(&self, queue_us: u64, xfer_us: u64) {
        let _ = self.urgent_queue_us.lock().record(queue_us.clamp(1, 600_000_000));
        let _ = self.urgent_xfer_us.lock().record(xfer_us.clamp(1, 600_000_000));
    }

    pub fn reset_latency(&self) {
        self.lat_us.lock().reset();
        self.blocked_us.lock().reset();
        self.urgent_queue_us.lock().reset();
        self.urgent_xfer_us.lock().reset();
        for c in self.sizes.lock().iter_mut() {
            *c = SizeClass { count: 0, bytes: 0, blocked: 0, blocked_us_sum: 0, hit_us: hist(), miss_us: hist() };
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let h = self.lat_us.lock();
        let b = self.blocked_us.lock();
        let uq = self.urgent_queue_us.lock();
        let ux = self.urgent_xfer_us.lock();
        StatsSnapshot {
            reads: self.reads.load(Relaxed),
            bytes_served: self.bytes_served.load(Relaxed),
            blocks_hit: self.blocks_hit.load(Relaxed),
            blocks_missed: self.blocks_missed.load(Relaxed),
            backend_requests: self.backend_requests.load(Relaxed),
            backend_bytes: self.backend_bytes.load(Relaxed),
            speculative_requests: self.speculative_requests.load(Relaxed),
            speculative_bytes: self.speculative_bytes.load(Relaxed),
            retries: self.retries.load(Relaxed),
            rate_limited: self.rate_limited.load(Relaxed),
            backend_errors: self.backend_errors.load(Relaxed),
            corrupt_blocks: self.corrupt_blocks.load(Relaxed),
            promoted_joins: self.promoted_joins.load(Relaxed),
            backend_req_us_max: self.backend_req_us_max.load(Relaxed),
            backend_req_us_avg: self.backend_req_us_sum.load(Relaxed) / self.backend_requests.load(Relaxed).max(1),
            blocked_reads: self.blocked_reads.load(Relaxed),
            blocked_ms_total: self.blocked_us_sum.load(Relaxed) / 1000,
            hedged_requests: self.hedged_requests.load(Relaxed),
            hedge_wins: self.hedge_wins.load(Relaxed),
            hedges_skipped: self.hedges_skipped.load(Relaxed),
            bulk_requests: self.bulk_requests.load(Relaxed),
            spec_cancelled: self.spec_cancelled.load(Relaxed),
            version_changed: self.version_changed.load(Relaxed),
            blocked_p50_us: b.value_at_quantile(0.50),
            blocked_p95_us: b.value_at_quantile(0.95),
            blocked_p99_us: b.value_at_quantile(0.99),
            blocked_max_us: b.max(),
            urgent_queue_p50_us: uq.value_at_quantile(0.50),
            urgent_queue_p99_us: uq.value_at_quantile(0.99),
            urgent_queue_max_us: uq.max(),
            urgent_xfer_p50_us: ux.value_at_quantile(0.50),
            urgent_xfer_p99_us: ux.value_at_quantile(0.99),
            urgent_xfer_max_us: ux.max(),
            lat_p50_us: h.value_at_quantile(0.50),
            lat_p95_us: h.value_at_quantile(0.95),
            lat_p99_us: h.value_at_quantile(0.99),
            lat_max_us: h.max(),
            blocked_wall_ms: {
                let w = self.blocked_wall.lock();
                (w.total_us + w.since.map_or(0, |s| s.elapsed().as_micros() as u64)) / 1000
            },
            req_queue_ms_total: self.req_queue_us_sum.load(Relaxed) / 1000,
            store_ms_total: self.store_us_sum.load(Relaxed) / 1000,
            sizes: self
                .sizes
                .lock()
                .iter()
                .zip(SIZE_BUCKETS.iter())
                .map(|(c, (name, _))| SizeClassSnapshot {
                    class: name,
                    reads: c.count,
                    bytes: c.bytes,
                    blocked: c.blocked,
                    blocked_ms_sum: c.blocked_us_sum / 1000,
                    hit_p50_us: c.hit_us.value_at_quantile(0.50),
                    hit_p95_us: c.hit_us.value_at_quantile(0.95),
                    hit_p99_us: c.hit_us.value_at_quantile(0.99),
                    hit_max_us: c.hit_us.max(),
                    miss_p50_us: c.miss_us.value_at_quantile(0.50),
                    miss_p95_us: c.miss_us.value_at_quantile(0.95),
                    miss_p99_us: c.miss_us.value_at_quantile(0.99),
                    miss_max_us: c.miss_us.max(),
                })
                .collect(),
        }
    }
}

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 600_000_000, 3).unwrap()
}
