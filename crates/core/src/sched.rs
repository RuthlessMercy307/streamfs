//! Request scheduler: three priority classes, slot limits and a delay-based byte budget for
//! speculative traffic.
//!
//! * `Urgent`: a reader is blocked on it (small demand reads, promoted prefetches). Has its
//!   own `limit` slots that other classes never occupy, so it never queues behind a transfer
//!   already in flight; nothing else starts while one is waiting.
//! * `Bulk`: demand data a reader will need shortly (the tail of a large read). At most half
//!   the slots and twice the speculative byte budget (always at least one request), so one
//!   huge read cannot fill the bottleneck queue that small urgent reads must cross.
//! * `Spec`: guesses (read-ahead, learned prefetch). Starts only when no demand request waits,
//!   and its bytes on the wire are capped by an adaptive budget.
//!
//! Why a byte budget and not just a slot cap: on a long thin link the bottleneck queue is
//! shared. A small urgent request that gets a free slot instantly still waits behind every
//! speculative byte already in flight (measured: 1 KiB reads taking 11 s while 7 x 8 MiB
//! prefetches drained). The budget keeps speculative bytes in flight near what the link can
//! drain quickly. It is controlled like LEDBAT: urgent request latency above the observed floor
//! is queueing delay caused by our own traffic, so the budget halves; while urgent requests
//! stay near the floor (or no demand traffic exists) it grows additively.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Urgent,
    Bulk,
    Spec,
}

/// Queueing delay over the latency floor that counts as congestion.
const TARGET_EXCESS: Duration = Duration::from_millis(150);
pub const SPEC_BUDGET_MIN: u64 = 1 << 20;
pub const SPEC_BUDGET_MAX: u64 = 48 << 20;
const SPEC_BUDGET_INIT: u64 = 8 << 20;
/// Bulk is demand data: it always keeps at least this much in flight, however congested.
const BULK_BYTES_MIN: u64 = 4 << 20;
/// The latency floor is the min over the current and previous window, so it follows route changes.
const FLOOR_WINDOW: Duration = Duration::from_secs(60);

struct St {
    limit: usize,
    inflight: usize,
    urgent_inflight: usize,
    bulk_inflight: usize,
    spec_inflight: usize,
    spec_bytes: u64,
    bulk_bytes: u64,
    budget: u64,
    urgent_wait: usize,
    bulk_wait: usize,
    spec_wait: usize,
    demand_inflight: usize,
    last_demand: Instant,
    last_cut: Instant,
    floor_cur: Duration,
    floor_prev: Duration,
    floor_since: Instant,
    ok_streak: u32,
    budget_cuts: u64,
}

pub struct Scheduler {
    st: Mutex<St>,
    notify: Notify,
    max: usize,
    spec_cap: Option<usize>,
}

pub struct Permit {
    s: Arc<Scheduler>,
    class: Class,
    bytes: u64,
}

impl Permit {
    pub fn class(&self) -> Class {
        self.class
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SchedSnapshot {
    pub limit: usize,
    pub inflight: usize,
    pub spec_inflight: usize,
    pub spec_bytes_inflight: u64,
    pub spec_budget: u64,
    pub budget_cuts: u64,
    pub urgent_waiting: usize,
    pub bulk_waiting: usize,
    pub spec_waiting: usize,
    pub latency_floor_ms: u64,
}

struct WaitGuard<'a>(&'a Scheduler, Option<Class>);
impl WaitGuard<'_> {
    fn set(&mut self, st: &mut St, c: Class) {
        if self.1 == Some(c) {
            return;
        }
        if let Some(old) = self.1 {
            dec(st, old);
        }
        match c {
            Class::Urgent => st.urgent_wait += 1,
            Class::Bulk => st.bulk_wait += 1,
            Class::Spec => st.spec_wait += 1,
        }
        self.1 = Some(c);
    }
}
fn dec(st: &mut St, c: Class) {
    match c {
        Class::Urgent => st.urgent_wait -= 1,
        Class::Bulk => st.bulk_wait -= 1,
        Class::Spec => st.spec_wait -= 1,
    }
}
impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.1 {
            dec(&mut self.0.st.lock(), c);
            // a waiter leaving can unblock lower classes
            self.0.notify.notify_waiters();
        }
    }
}

impl Scheduler {
    /// `spec_cap`: max concurrent speculative requests (default: all but one slot).
    pub fn new(max: usize, spec_cap: Option<usize>) -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Scheduler {
            st: Mutex::new(St {
                limit: max,
                inflight: 0,
                urgent_inflight: 0,
                bulk_inflight: 0,
                spec_inflight: 0,
                spec_bytes: 0,
                bulk_bytes: 0,
                budget: SPEC_BUDGET_INIT,
                urgent_wait: 0,
                bulk_wait: 0,
                spec_wait: 0,
                demand_inflight: 0,
                last_demand: now,
                last_cut: now,
                floor_cur: Duration::MAX,
                floor_prev: Duration::MAX,
                floor_since: now,
                ok_streak: 0,
                budget_cuts: 0,
            }),
            notify: Notify::new(),
            max,
            spec_cap,
        })
    }

    /// Re-evaluate waiters (a speculative request was promoted to urgent).
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    pub fn limit(&self) -> usize {
        self.st.lock().limit
    }

    /// Speculative requests queued but not started: read-ahead stops adding work above a bound
    /// (backpressure instead of an unbounded task queue).
    pub fn spec_backlog(&self) -> usize {
        self.st.lock().spec_wait
    }

    pub fn snapshot(&self) -> SchedSnapshot {
        let st = self.st.lock();
        let floor = st.floor_cur.min(st.floor_prev);
        SchedSnapshot {
            limit: st.limit,
            inflight: st.inflight,
            spec_inflight: st.spec_inflight,
            spec_bytes_inflight: st.spec_bytes,
            spec_budget: st.budget,
            budget_cuts: st.budget_cuts,
            urgent_waiting: st.urgent_wait,
            bulk_waiting: st.bulk_wait,
            spec_waiting: st.spec_wait,
            latency_floor_ms: if floor == Duration::MAX { 0 } else { floor.as_millis() as u64 },
        }
    }

    fn admit(&self, st: &St, class: Class, bytes: u64) -> bool {
        let non_urgent_slots = st.limit.saturating_sub(1).max(1);
        let non_urgent = st.bulk_inflight + st.spec_inflight;
        match class {
            Class::Urgent => st.urgent_inflight < st.limit,
            Class::Bulk => {
                st.urgent_wait == 0
                    && st.bulk_inflight < (st.limit / 2).max(1)
                    && non_urgent < non_urgent_slots
                    && (st.bulk_bytes == 0 || st.bulk_bytes + bytes <= (st.budget * 2).max(BULK_BYTES_MIN))
            }
            Class::Spec => {
                let cap = self.spec_cap.unwrap_or(non_urgent_slots).clamp(1, non_urgent_slots);
                st.urgent_wait == 0
                    && st.bulk_wait == 0
                    && non_urgent < non_urgent_slots
                    && st.spec_inflight < cap
                    // one request is always allowed, even if larger than the budget
                    && (st.spec_bytes == 0 || st.spec_bytes + bytes <= st.budget)
            }
        }
    }

    /// Wait for a slot. `promoted` turns a queued speculative request into an urgent one the
    /// moment a blocked reader joins it. A speculative request that waits longer than
    /// `spec_timeout` gives up (`None`): stale guesses must not pile up.
    pub async fn acquire(self: &Arc<Self>, class: Class, bytes: u64, promoted: Option<&AtomicBool>, spec_timeout: Option<Duration>) -> Option<Permit> {
        let mut guard = WaitGuard(self, None);
        let t0 = Instant::now();
        loop {
            let class = if class == Class::Spec && promoted.map_or(false, |p| p.load(Relaxed)) { Class::Urgent } else { class };
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.st.lock();
                if self.admit(&st, class, bytes) {
                    st.inflight += 1;
                    match class {
                        Class::Spec => {
                            st.spec_inflight += 1;
                            st.spec_bytes += bytes;
                        }
                        c => {
                            if c == Class::Urgent {
                                st.urgent_inflight += 1;
                            } else {
                                st.bulk_inflight += 1;
                                st.bulk_bytes += bytes;
                            }
                            st.demand_inflight += 1;
                            st.last_demand = Instant::now();
                        }
                    }
                    if let Some(c) = guard.1.take() {
                        dec(&mut st, c);
                    }
                    return Some(Permit { s: self.clone(), class, bytes });
                }
                guard.set(&mut st, class);
            }
            match (class, spec_timeout) {
                (Class::Spec, Some(limit)) => {
                    let left = limit.saturating_sub(t0.elapsed());
                    if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                        return None;
                    }
                }
                _ => notified.await,
            }
        }
    }

    /// Feed the latency of a finished urgent request (small, so dominated by RTT + queueing).
    pub fn on_urgent_latency(&self, lat: Duration) {
        let mut st = self.st.lock();
        let now = Instant::now();
        if now.duration_since(st.floor_since) > FLOOR_WINDOW {
            st.floor_prev = st.floor_cur;
            st.floor_cur = Duration::MAX;
            st.floor_since = now;
        }
        st.floor_cur = st.floor_cur.min(lat);
        let floor = st.floor_cur.min(st.floor_prev);
        if lat > floor + TARGET_EXCESS {
            // at most one cut per floor-RTT, so one burst of slow replies halves once
            if st.spec_bytes + st.bulk_bytes > 0 && now.duration_since(st.last_cut) > floor.max(Duration::from_millis(100)) {
                st.budget = (st.budget / 2).max(SPEC_BUDGET_MIN);
                st.last_cut = now;
                st.budget_cuts += 1;
            }
        } else {
            st.budget = (st.budget + (512 << 10)).min(SPEC_BUDGET_MAX);
        }
    }

    pub fn on_ok(&self) {
        let mut st = self.st.lock();
        st.ok_streak += 1;
        if st.ok_streak >= 16 && st.limit < self.max {
            st.limit += 1;
            st.ok_streak = 0;
            drop(st);
            self.notify.notify_waiters();
        }
    }

    pub fn on_rate_limited(&self) {
        let mut st = self.st.lock();
        st.limit = (st.limit / 2).max(1);
        st.budget = (st.budget / 2).max(SPEC_BUDGET_MIN);
        st.ok_streak = 0;
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut st = self.s.st.lock();
            st.inflight -= 1;
            match self.class {
                Class::Spec => {
                    st.spec_inflight -= 1;
                    st.spec_bytes -= self.bytes;
                    // no demand traffic for a while: the link is ours to fill (bounded by the max)
                    if st.demand_inflight == 0 && st.last_demand.elapsed() > Duration::from_secs(1) {
                        st.budget = (st.budget + (1 << 20)).min(SPEC_BUDGET_MAX);
                    }
                }
                c => {
                    if c == Class::Urgent {
                        st.urgent_inflight -= 1;
                    } else {
                        st.bulk_inflight -= 1;
                        st.bulk_bytes -= self.bytes;
                    }
                    st.demand_inflight -= 1;
                }
            }
        }
        self.s.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn urgent_overtakes_queued_spec_and_bulk() {
        let s = Scheduler::new(2, None);
        // bulk fills every slot it may use; urgent still starts immediately
        let b = s.acquire(Class::Bulk, 1, None, None).await.unwrap();
        let s2 = s.clone();
        let spec = tokio::spawn(async move { s2.acquire(Class::Spec, 1 << 20, None, None).await.map(|p| p.class()) });
        tokio::task::yield_now().await;
        assert!(!spec.is_finished(), "spec must not take the last non-urgent slot");
        let u1 = s.acquire(Class::Urgent, 1, None, Some(Duration::from_millis(10))).await;
        let u2 = s.acquire(Class::Urgent, 1, None, Some(Duration::from_millis(10))).await;
        assert!(u1.is_some() && u2.is_some(), "urgent has its own slots");
        drop(b);
        assert_eq!(spec.await.unwrap(), Some(Class::Spec));
    }

    #[tokio::test]
    async fn spec_bytes_are_budgeted() {
        let s = Scheduler::new(8, None);
        let a = s.acquire(Class::Spec, 6 << 20, None, None).await.unwrap();
        // 6 + 6 MiB > 8 MiB initial budget: second one times out
        assert!(s.acquire(Class::Spec, 6 << 20, None, Some(Duration::from_millis(50))).await.is_none());
        drop(a);
        assert!(s.acquire(Class::Spec, 6 << 20, None, Some(Duration::from_millis(50))).await.is_some());
    }

    #[tokio::test]
    async fn congestion_halves_budget() {
        let s = Scheduler::new(8, None);
        let _p = s.acquire(Class::Spec, 1 << 20, None, None).await.unwrap();
        s.on_urgent_latency(Duration::from_millis(100));
        let before = s.snapshot().spec_budget;
        std::thread::sleep(Duration::from_millis(150));
        s.on_urgent_latency(Duration::from_millis(900));
        assert_eq!(s.snapshot().spec_budget, (before / 2).max(SPEC_BUDGET_MIN));
    }

    #[tokio::test]
    async fn promotion_lets_queued_spec_through() {
        let s = Scheduler::new(2, Some(1));
        let _a = s.acquire(Class::Spec, 1, None, None).await.unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let (s2, f2) = (s.clone(), flag.clone());
        let t = tokio::spawn(async move { s2.acquire(Class::Spec, 1, Some(&f2), None).await.map(|p| p.class()) });
        tokio::task::yield_now().await;
        assert!(!t.is_finished());
        flag.store(true, Relaxed);
        s.wake();
        assert_eq!(t.await.unwrap(), Some(Class::Urgent));
    }
}
