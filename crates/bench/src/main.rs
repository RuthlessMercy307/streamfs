//! Reproducible benchmark against the SIMULATED far-cloud backend.
//! Numbers here characterise the engine's behaviour (request counts, bytes,
//! hit rates, scheduling) -- NOT Google Drive. Re-run against the real backend
//! before drawing any conclusion about Drive.

use sfs_core::{Config, Engine};
use sfs_sim::{SimBackend, SimConfig};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn fresh(be: &Arc<SimBackend>, name: &str, mut tweak: impl FnMut(&mut Config)) -> (Engine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("sfs-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = Config::new(&dir);
    c.cache_bytes = 2 * GB;
    tweak(&mut c);
    (Engine::new(be.clone(), c).unwrap(), dir)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let be = Arc::new(SimBackend::new(SimConfig::far_cloud()));
    be.add_file("game.pak", 60 * GB, 42);
    let mut report = Vec::new();

    println!("== random reads, cold cache (sim: 120ms+jitter, 12MB/s/conn, 40MB/s total) ==");
    println!("{:>8} {:>8} {:>9} {:>9} {:>9} {:>10} {:>9}", "size", "reads", "p50 ms", "p95 ms", "p99 ms", "net MB", "reqs");
    for size in [256u64 << 10, MB, 4 * MB, 16 * MB] {
        let (eng, dir) = fresh(&be, "rand", |_| {});
        let h = eng.open("game.pak").await.unwrap();
        be.reset_counters();
        let mut s = 0x9E3779B97F4A7C15u64;
        let n = 40;
        for _ in 0..n {
            let off = xorshift(&mut s) % (60 * GB - size);
            h.read(off, size as usize).await.unwrap();
        }
        let st = eng.stats();
        let (reqs, bytes) = (be.requests.load(Relaxed), be.bytes.load(Relaxed));
        println!(
            "{:>7}K {:>8} {:>9.1} {:>9.1} {:>9.1} {:>10.1} {:>9}",
            size >> 10, n, st.lat_p50_us as f64 / 1e3, st.lat_p95_us as f64 / 1e3, st.lat_p99_us as f64 / 1e3,
            bytes as f64 / MB as f64, reqs
        );
        report.push(serde_json::json!({"scenario":"random_cold","read_size":size,"reads":n,"stats":st,"net_bytes":bytes,"requests":reqs}));
        let _ = std::fs::remove_dir_all(dir);
    }

    println!("\n== sequential 256 MB, 1 MB reads: read-ahead on vs off ==");
    for (label, ra) in [("readahead 32MB", 32usize << 20), ("no readahead", 0)] {
        let (eng, dir) = fresh(&be, "seq", |c| {
            if ra == 0 {
                c.readahead_max = 0;
            } else {
                c.readahead_max = ra;
            }
        });
        let h = eng.open("game.pak").await.unwrap();
        be.reset_counters();
        let t = Instant::now();
        let mut off = 0;
        while off < 256 * MB {
            h.read(off, MB as usize).await.unwrap();
            off += MB;
        }
        let secs = t.elapsed().as_secs_f64();
        let st = eng.stats();
        println!(
            "{label:>16}: {:6.1} MB/s  net {:6.1} MB  reqs {:4}  spec reqs {:4}  p99 {:.0} ms",
            256.0 / secs, be.bytes.load(Relaxed) as f64 / MB as f64, be.requests.load(Relaxed),
            st.speculative_requests, st.lat_p99_us as f64 / 1e3
        );
        report.push(serde_json::json!({"scenario":"sequential","label":label,"mb_s":256.0/secs,"stats":st,"net_bytes":be.bytes.load(Relaxed)}));
        let _ = std::fs::remove_dir_all(dir);
    }

    println!("\n== cold vs warm: same 200 random 1 MB reads twice ==");
    let (eng, dir) = fresh(&be, "warm", |_| {});
    let h = eng.open("game.pak").await.unwrap();
    for pass in ["cold", "warm"] {
        eng.reset_latency();
        be.reset_counters();
        let mut s = 12345u64;
        for _ in 0..200 {
            let off = xorshift(&mut s) % (60 * GB - MB);
            h.read(off, MB as usize).await.unwrap();
        }
        let st = eng.stats();
        println!(
            "{pass:>5}: p50 {:7.2} ms  p99 {:7.2} ms  net {:7.1} MB  reqs {}",
            st.lat_p50_us as f64 / 1e3, st.lat_p99_us as f64 / 1e3,
            be.bytes.load(Relaxed) as f64 / MB as f64, be.requests.load(Relaxed)
        );
        report.push(serde_json::json!({"scenario":"cold_warm","pass":pass,"stats":st,"net_bytes":be.bytes.load(Relaxed)}));
    }
    println!("cache on disk: {:?}", eng.store_stats());
    let _ = std::fs::remove_dir_all(dir);

    println!("\n== fault injection: 1/4 requests 429, 1/7 reset; 100 random 1 MB reads ==");
    let mut faulty = SimConfig::far_cloud();
    faulty.ratelimit_every = Some(4);
    faulty.fail_every = Some(7);
    be.set_config(faulty);
    let (eng, dir) = fresh(&be, "fault", |c| c.base_backoff = std::time::Duration::from_millis(50));
    let h = eng.open("game.pak").await.unwrap();
    be.reset_counters();
    let mut s = 777u64;
    let mut failed = 0;
    for _ in 0..100 {
        let off = xorshift(&mut s) % (60 * GB - MB);
        if h.read(off, MB as usize).await.is_err() {
            failed += 1;
        }
    }
    let st = eng.stats();
    println!(
        "failed reads {failed}/100  retries {}  429s {}  p99 {:.0} ms  final concurrency limit {}",
        st.retries, st.rate_limited, st.lat_p99_us as f64 / 1e3, eng.concurrency_limit()
    );
    report.push(serde_json::json!({"scenario":"faults","failed":failed,"stats":st}));
    let _ = std::fs::remove_dir_all(dir);

    let out = std::env::args().nth(1).unwrap_or_else(|| "bench-results.json".into());
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("\nraw results -> {out}");
}
