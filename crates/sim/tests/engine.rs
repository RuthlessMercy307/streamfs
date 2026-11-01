use sfs_core::{Config, Engine};
use sfs_sim::{expected, SimBackend, SimConfig};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sfs-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn cfg(dir: &PathBuf, cache: u64) -> Config {
    let mut c = Config::new(dir);
    c.cache_bytes = cache;
    c.base_backoff = Duration::from_millis(5);
    c
}

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_reads_on_20gb_file_are_correct_and_fetch_little() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("game.pak", 20 * GB, 7);
    let dir = tmpdir("rand");
    let eng = Engine::new(be.clone(), cfg(&dir, 512 * MB)).unwrap();
    let h = eng.open("game.pak").await.unwrap();

    let mut s = 88172645463325252u64;
    for _ in 0..300 {
        let off = rng(&mut s) % (20 * GB - 1);
        let len = (rng(&mut s) % (3 * MB) + 1) as usize;
        let got = h.read(off, len).await.unwrap();
        let want_len = len.min((20 * GB - off) as usize);
        assert_eq!(got.len(), want_len);
        assert_eq!(&got[..], &expected(7, off, want_len)[..], "mismatch at {off}+{len}");
    }
    // 300 random reads must never approach a full download.
    assert!(be.bytes.load(Relaxed) < 2 * GB, "fetched {} bytes", be.bytes.load(Relaxed));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_mb_read_of_20gb_file_does_not_download_the_file() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("big.bin", 20 * GB, 3);
    let dir = tmpdir("ten");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("big.bin").await.unwrap();
    let off = 9 * GB + 12345;
    let got = h.read(off, (10 * MB) as usize).await.unwrap();
    assert_eq!(&got[..], &expected(3, off, (10 * MB) as usize)[..]);
    let fetched = be.bytes.load(Relaxed);
    assert!(fetched <= 11 * MB + 2 * MB, "fetched {fetched}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_of_same_region_share_one_fetch() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(60);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("f", GB, 11);
    let dir = tmpdir("dedupe");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = Arc::new(eng.open("f").await.unwrap());
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move { h.read(5 * MB, (512 << 10) as usize).await.unwrap() }));
    }
    for t in tasks {
        assert_eq!(&t.await.unwrap()[..], &expected(11, 5 * MB, 512 << 10)[..]);
    }
    assert_eq!(be.requests.load(Relaxed), 1, "dedup failed");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_persists_across_restart_with_zero_backend_traffic() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("p", 2 * GB, 21);
    let dir = tmpdir("persist");
    {
        let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
        let h = eng.open("p").await.unwrap();
        h.read(100 * MB, (4 * MB) as usize).await.unwrap();
    }
    be.reset_counters();
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("p").await.unwrap();
    let got = h.read(100 * MB, (4 * MB) as usize).await.unwrap();
    assert_eq!(&got[..], &expected(21, 100 * MB, (4 * MB) as usize)[..]);
    assert_eq!(be.requests.load(Relaxed), 0, "warm cache must not touch the network");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_respects_cap_and_content_stays_correct() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("e", 4 * GB, 5);
    let dir = tmpdir("evict");
    let eng = Engine::new(be.clone(), cfg(&dir, 64 * MB)).unwrap();
    let h = eng.open("e").await.unwrap();
    // Read 400 MB sequentially through a 64 MB cache.
    let mut off = 0;
    while off < 400 * MB {
        let got = h.read(off, MB as usize).await.unwrap();
        assert_eq!(&got[..], &expected(5, off, MB as usize)[..]);
        off += MB;
    }
    let st = eng.store_stats();
    assert!(st.slab_bytes <= 64 * MB, "slab grew to {}", st.slab_bytes);
    assert!(std::fs::metadata(eng.store().slab_path()).unwrap().len() <= 64 * MB);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_cache_data_is_detected_and_refetched() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("c", GB, 9);
    let dir = tmpdir("corrupt");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("c").await.unwrap();
    h.read(0, (2 * MB) as usize).await.unwrap();
    // Flip bytes in the slab behind the engine's back.
    let slab = eng.store().slab_path();
    let mut raw = std::fs::read(&slab).unwrap();
    for b in raw.iter_mut().take(300_000).skip(1000) {
        *b ^= 0xFF;
    }
    std::fs::write(&slab, raw).unwrap();
    let got = h.read(0, (2 * MB) as usize).await.unwrap();
    assert_eq!(&got[..], &expected(9, 0, (2 * MB) as usize)[..], "served corrupt bytes!");
    assert!(eng.stats().corrupt_blocks >= 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_version_change_invalidates_cache() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("v", GB, 1);
    let dir = tmpdir("version");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("v").await.unwrap();
    assert_eq!(&h.read(0, 4096).await.unwrap()[..], &expected(1, 0, 4096)[..]);
    be.replace_file("v", 2);
    let h2 = eng.open("v").await.unwrap(); // re-stat sees the new version
    assert_eq!(&h2.read(0, 4096).await.unwrap()[..], &expected(2, 0, 4096)[..], "stale data served");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_and_ratelimit_errors_are_retried() {
    let mut sc = SimConfig::instant();
    sc.fail_every = Some(3);
    sc.ratelimit_every = Some(5);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("r", 2 * GB, 4);
    let dir = tmpdir("retry");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("r").await.unwrap();
    for i in 0..40u64 {
        let off = i * 37 * MB;
        assert_eq!(&h.read(off, 300_000).await.unwrap()[..], &expected(4, off, 300_000)[..]);
    }
    let s = eng.stats();
    assert!(s.retries > 0 && s.rate_limited > 0, "{s:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adaptive_concurrency_backs_off_under_parallel_limit() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(20);
    sc.max_parallel = Some(2);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("a", 8 * GB, 6);
    let dir = tmpdir("aimd");
    let mut c = cfg(&dir, 256 * MB);
    c.max_conns = 8;
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = Arc::new(eng.open("a").await.unwrap());
    let mut tasks = Vec::new();
    for i in 0..24u64 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let off = i * 100 * MB;
            assert_eq!(&h.read(off, 200_000).await.unwrap()[..], &expected(6, off, 200_000)[..]);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert!(eng.concurrency_limit() <= 8);
    assert!(eng.stats().rate_limited > 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torn_journal_tail_after_crash_is_tolerated() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("t", GB, 12);
    let dir = tmpdir("torn");
    {
        let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
        let h = eng.open("t").await.unwrap();
        h.read(0, (3 * MB) as usize).await.unwrap();
    }
    // Simulate a crash mid-append.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("index.jsonl")).unwrap();
    f.write_all(b"{\"t\":\"Block\",\"f\":0,\"b\":99,\"s\":").unwrap();
    drop(f);
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("t").await.unwrap();
    assert_eq!(&h.read(0, (3 * MB) as usize).await.unwrap()[..], &expected(12, 0, (3 * MB) as usize)[..]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_read_triggers_readahead_and_stays_correct() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(10);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("s", GB, 8);
    let dir = tmpdir("seq");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    let h = eng.open("s").await.unwrap();
    let mut off = 0;
    while off < 64 * MB {
        let got = h.read(off, (256 << 10) as usize).await.unwrap();
        assert_eq!(&got[..], &expected(8, off, 256 << 10)[..]);
        off += 256 << 10;
    }
    let s = eng.stats();
    assert!(s.speculative_requests > 0, "no read-ahead happened: {s:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_file_is_never_evicted_and_overflow_fails_loudly() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("pin", GB, 2);
    be.add_file("other", GB, 3);
    let dir = tmpdir("pin");
    let eng = Engine::new(be.clone(), cfg(&dir, 8 * MB)).unwrap();
    let hp = eng.open("pin").await.unwrap();
    hp.read(0, (6 * MB) as usize).await.unwrap();
    hp.set_pinned(true);
    let ho = eng.open("other").await.unwrap();
    // 8 MB cache, 6 MB pinned: 2 MB left. A 4 MB read of the other file cannot fit.
    let r = ho.read(0, (4 * MB) as usize).await;
    assert!(r.is_err(), "must fail loudly instead of exceeding the cap");
    assert!(eng.store_stats().slab_bytes <= 8 * MB);
    be.reset_counters();
    hp.read(0, (6 * MB) as usize).await.unwrap();
    assert_eq!(be.requests.load(Relaxed), 0, "pinned data was evicted");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_read_promotes_queued_speculative_fetch() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(300);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("p", 2 * GB, 14);
    let dir = tmpdir("promote");
    let mut c = cfg(&dir, 256 * MB);
    c.max_conns = 4;
    c.spec_conns = Some(1); // speculative runs queue up one at a time
    c.readahead_init = (24 * MB) as usize; // 24 MB ahead = 3 queued 8 MB runs
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = eng.open("p").await.unwrap();
    h.read(0, 256 << 10).await.unwrap(); // cold read; triggers the read-ahead claims
    // 20 MB sits in the 3rd queued speculative run. Without promotion it would
    // wait for runs 1 and 2 (~900 ms total); promoted, it starts immediately.
    let t = std::time::Instant::now();
    let got = h.read(20 * MB, 64 << 10).await.unwrap();
    let ms = t.elapsed().as_millis();
    assert_eq!(&got[..], &expected(14, 20 * MB, 64 << 10)[..]);
    assert!(ms < 650, "blocked read waited {ms} ms behind queued speculative work");
    assert!(eng.stats().promoted_joins >= 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// 24 scattered "boot" reads across a 4 GB file, like a program starting up.
fn boot_offsets() -> Vec<u64> {
    let mut s = 4242u64;
    (0..24).map(|_| (rng(&mut s) % (4 * GB - 2 * MB)) & !0xFFF).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_profile_prefetch_cuts_cold_boot_time_and_stays_correct() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(150);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("g.pak", 4 * GB, 31);
    let offs = boot_offsets();

    // Session 1: cold boot, recorded.
    let dir1 = tmpdir("prof1");
    let eng = Engine::new(be.clone(), cfg(&dir1, 512 * MB)).unwrap();
    eng.start_trace(Duration::from_secs(60), 10_000);
    let h = eng.open("g.pak").await.unwrap();
    let t = std::time::Instant::now();
    for &o in &offs {
        assert_eq!(&h.read(o, MB as usize).await.unwrap()[..], &expected(31, o, MB as usize)[..]);
    }
    let cold_ms = t.elapsed().as_millis();
    let profile = eng.trace_snapshot();
    assert_eq!(profile.entries.len(), offs.len(), "one merged region per scattered read");

    // Session 2: brand-new empty cache, same boot, with the profile prefetched at launch.
    let dir2 = tmpdir("prof2");
    let eng2 = Engine::new(be.clone(), cfg(&dir2, 512 * MB)).unwrap();
    let held = eng2.prefetch(&profile, 256 * MB).await;
    let h2 = eng2.open("g.pak").await.unwrap();
    let t = std::time::Instant::now();
    for &o in &offs {
        assert_eq!(&h2.read(o, MB as usize).await.unwrap()[..], &expected(31, o, MB as usize)[..], "wrong bytes");
    }
    let prof_ms = t.elapsed().as_millis();
    drop(held);
    println!("cold boot {cold_ms} ms vs profile-prefetched boot {prof_ms} ms");
    assert!(prof_ms * 2 < cold_ms, "prefetch should at least halve boot time: {cold_ms} -> {prof_ms}");
    // The prefetch fetched what the profile said, not the whole file.
    assert!(be.bytes.load(Relaxed) < 24 * 2 * 3 * MB + 8 * 24 * MB);
    let _ = std::fs::remove_dir_all(dir1);
    let _ = std::fs::remove_dir_all(dir2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_profile_is_ignored_after_remote_update() {
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    be.add_file("u.pak", GB, 1);
    let dir = tmpdir("stale");
    let eng = Engine::new(be.clone(), cfg(&dir, 256 * MB)).unwrap();
    eng.start_trace(Duration::from_secs(60), 100);
    let h = eng.open("u.pak").await.unwrap();
    h.read(10 * MB, MB as usize).await.unwrap();
    let profile = eng.trace_snapshot();
    be.replace_file("u.pak", 2); // game updated: recorded regions no longer describe the file
    be.reset_counters();
    let dir2 = tmpdir("stale2");
    let eng2 = Engine::new(be.clone(), cfg(&dir2, 256 * MB)).unwrap();
    let held = eng2.prefetch(&profile, 256 * MB).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(be.requests.load(Relaxed), 0, "stale profile must not trigger downloads");
    drop(held);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_request_is_hedged_instead_of_blocking_the_reader() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(100);
    sc.stall_nth = Some((1, Duration::from_secs(20))); // the first request hangs 20 s
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("h.pak", GB, 44);
    let dir = tmpdir("hedge");
    let mut c = cfg(&dir, 256 * MB);
    c.hedge_after = Some(Duration::from_millis(300));
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = eng.open("h.pak").await.unwrap();
    let t = std::time::Instant::now();
    let got = h.read(123 * MB, 64 << 10).await.unwrap();
    let ms = t.elapsed().as_millis();
    assert_eq!(&got[..], &expected(44, 123 * MB, 64 << 10)[..]);
    assert!(ms < 2000, "reader stayed blocked {ms} ms behind a stuck request");
    let s = eng.stats();
    assert!(s.hedged_requests >= 1 && s.hedge_wins >= 1, "{s:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_timeout_bounds_a_stuck_request_without_hedging() {
    let mut sc = SimConfig::instant();
    sc.stall_nth = Some((1, Duration::from_secs(60)));
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("t.pak", GB, 45);
    let dir = tmpdir("timeout");
    let mut c = cfg(&dir, 256 * MB);
    c.hedge_after = None;
    c.request_timeout = Duration::from_millis(500);
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = eng.open("t.pak").await.unwrap();
    let t = std::time::Instant::now();
    let got = h.read(0, 4096).await.unwrap();
    assert_eq!(&got[..], &expected(45, 0, 4096)[..]);
    assert!(t.elapsed().as_millis() < 3000, "timeout did not bound the stall");
    assert!(eng.stats().retries >= 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hedging_is_capped_and_cannot_become_a_request_storm() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(500); // every request is "slow" vs the 50 ms hedge delay
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("s.pak", 4 * GB, 46);
    let dir = tmpdir("hedgecap");
    let mut c = cfg(&dir, 512 * MB);
    c.hedge_after = Some(Duration::from_millis(50));
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = Arc::new(eng.open("s.pak").await.unwrap());
    let mut tasks = Vec::new();
    for i in 0..24u64 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let off = i * 150 * MB;
            assert_eq!(&h.read(off, 4096).await.unwrap()[..], &expected(46, off, 4096)[..]);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let s = eng.stats();
    // Budget: <= 4 in flight, <= 5% of requests + burst of 4.
    assert!(s.hedged_requests <= 6, "too many duplicates: {s:?}");
    assert!(s.hedges_skipped >= 1, "budget never kicked in: {s:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_piece_of_a_large_read_is_retried_without_refetching_the_rest() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(20);
    sc.stall_nth = Some((3, Duration::from_secs(60))); // one ~1 MiB piece hangs
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("big.pak", 2 * GB, 51);
    let dir = tmpdir("stallbulk");
    let mut c = cfg(&dir, 256 * MB);
    c.stall_timeout = Some(Duration::from_millis(300));
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = eng.open("big.pak").await.unwrap();
    let t = std::time::Instant::now();
    let off = 300 * MB + 777;
    let got = h.read(off, (8 * MB) as usize).await.unwrap();
    let ms = t.elapsed().as_millis();
    assert_eq!(&got[..], &expected(51, off, (8 * MB) as usize)[..]);
    assert!(ms < 3000, "large read stayed blocked {ms} ms behind one stalled piece");
    let s = eng.stats();
    assert!(s.retries >= 1, "{s:?}");
    // only the stalled piece is fetched again, not the whole read
    let fetched = be.bytes.load(Relaxed);
    eprintln!("stallbulk: {ms} ms fetched={fetched} reqs={} retries={}", s.backend_requests, s.retries);
    assert!(fetched <= 8 * MB + 2 * MB, "fetched {fetched}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flaky_link_with_stalls_and_resets_returns_exact_bytes_without_request_storms() {
    let mut sc = SimConfig::instant();
    sc.latency = Duration::from_millis(30);
    sc.jitter = Duration::from_millis(30);
    sc.stall_every = Some((7, Duration::from_secs(60)));
    sc.fail_every = Some(11);
    let be = Arc::new(SimBackend::new(sc));
    be.add_file("f.pak", 2 * GB, 52);
    let dir = tmpdir("flaky");
    let mut c = cfg(&dir, 512 * MB);
    c.stall_timeout = Some(Duration::from_millis(400));
    c.hedge_after = Some(Duration::from_millis(200));
    c.request_timeout = Duration::from_secs(3);
    let eng = Engine::new(be.clone(), c).unwrap();
    let h = Arc::new(eng.open("f.pak").await.unwrap());
    let t = std::time::Instant::now();
    let mut tasks = Vec::new();
    for w in 0..6u64 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let mut s = 0x9E37_79B9 ^ (w + 1) * 7919;
            let mut asked = 0u64;
            for _ in 0..10 {
                let off = rng(&mut s) % (2 * GB - 8 * MB);
                // mix of small, medium and large reads
                let len = match rng(&mut s) % 3 { 0 => 4096 + rng(&mut s) % (60 << 10), 1 => 256 << 10, _ => 1 * MB + rng(&mut s) % (5 * MB) } as usize;
                let got = h.read(off, len).await.unwrap();
                assert_eq!(&got[..], &expected(52, off, len)[..], "mismatch at {off}+{len}");
                asked += len as u64;
            }
            asked
        }));
    }
    let mut asked = 0;
    for t in tasks {
        asked += t.await.unwrap();
    }
    let s = eng.stats();
    assert!(t.elapsed() < Duration::from_secs(60), "took {:?}: {s:?}", t.elapsed());
    assert_eq!(s.corrupt_blocks, 0);
    // fetched bytes stay proportional to what was read (read-ahead + min_fetch + retried pieces)
    let fetched = be.bytes.load(Relaxed);
    eprintln!("flaky: {:?} asked={asked} fetched={fetched} reqs={} retries={} hedged={} wins={} errors={}", t.elapsed(), s.backend_requests, s.retries, s.hedged_requests, s.hedge_wins, s.backend_errors);
    assert!(fetched < asked * 2 + 64 * MB, "fetched {fetched} for {asked} asked: {s:?}");
    assert!(s.hedged_requests * 100 <= s.backend_requests * 5 + 400 + 100, "{s:?}");
    let _ = std::fs::remove_dir_all(dir);
}
