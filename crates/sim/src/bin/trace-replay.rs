//! Replays a recorded read log (sfs-core readlog format) against the engine over a simulated
//! link, so engine variants can be compared on exactly the same application reads.
//!
//! usage: trace-replay --trace reads.log [--warm-index index.jsonl] [--label L] [--out res.jsonl]
//!        [--min-fetch-kb 512] [--think-cap-ms 1000] [--skip-large-mb 16]
//!        [--rtt-ms 220] [--jitter-ms 40] [--conn-mbps 3.0] [--total-mbps 10] [--stall-every N]
//!
//! Timing model: the application waited for read i before doing anything else (session 4: the
//! union of blocked time was 99% of the sum, i.e. reads are serial). Read i+1 is issued
//! `think` after read i completed, where think = its original start - original completion of
//! read i (capped by --think-cap-ms so idle minutes don't make a run take an hour; capping
//! can only shorten the time prefetch has, so results are conservative). A negative think
//! (the original reads overlapped) issues it without waiting.
//! Cache states: cold (empty), or warm = the blocks listed in a cache index.jsonl snapshot
//! taken before the recorded session, restricted to the blocks this trace touches.
//! Reads >= --skip-large-mb are not replayed (session 4: 379 such reads, 85% of bytes, all
//! cache hits, 0% of the wait); they are counted and reported.
//! Content is synthetic (SimBackend): this measures timing and traffic, not game data.

use sfs_core::{Config, Engine, Handle};
use sfs_sim::{SimBackend, SimConfig};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}
fn num(name: &str, d: f64) -> f64 {
    arg(name).and_then(|v| v.parse().ok()).unwrap_or(d)
}

struct FileRec {
    path: String,
    size: u64,
}
#[derive(Clone)]
struct Read {
    t_us: u64,
    fid: u32,
    off: u64,
    len: u64,
    lat_us: u64,
    missed: u32,
}

fn parse(path: &str) -> (HashMap<u32, FileRec>, Vec<Read>) {
    let mut files = HashMap::new();
    let mut reads = Vec::new();
    for line in std::fs::read_to_string(path).expect("trace").lines() {
        let mut it = line.splitn(5, ' ');
        match it.next() {
            Some("F") => {
                let fid: u32 = it.next().unwrap().parse().unwrap();
                let size: u64 = it.next().unwrap().parse().unwrap();
                let _ver = it.next();
                let p = it.next().unwrap_or("").to_string();
                files.insert(fid, FileRec { path: p, size });
            }
            Some("R") => {
                let v: Vec<u64> = line[2..].split(' ').map(|x| x.parse().unwrap_or(0)).collect();
                if v.len() == 6 {
                    reads.push(Read { t_us: v[0], fid: v[1] as u32, off: v[2], len: v[3], lat_us: v[4], missed: v[5] as u32 });
                }
            }
            _ => {}
        }
    }
    (files, reads)
}

/// (path, block) pairs resident in a cache index snapshot.
fn warm_blocks(index: &str) -> HashSet<(String, u32)> {
    let mut idx: HashMap<u64, String> = HashMap::new();
    let mut out = HashSet::new();
    for line in std::fs::read_to_string(index).expect("index").lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["t"].as_str() {
            Some("File") => {
                let key = v["key"].as_str().unwrap_or("");
                let p = key.split_once(':').map(|(_, p)| p).unwrap_or(key);
                idx.insert(v["idx"].as_u64().unwrap_or(0), p.to_string());
            }
            Some("Block") => {
                if let Some(p) = idx.get(&v["f"].as_u64().unwrap_or(u64::MAX)) {
                    out.insert((p.clone(), v["b"].as_u64().unwrap_or(0) as u32));
                }
            }
            _ => {}
        }
    }
    out
}

fn pct(v: &mut Vec<u64>, q: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let trace = arg("--trace").expect("--trace");
    let label = arg("--label").unwrap_or_else(|| "replay".into());
    let think_cap = Duration::from_millis(num("--think-cap-ms", 1000.0) as u64);
    let skip_large = (num("--skip-large-mb", 16.0) * 1048576.0) as u64;
    let bs: u64 = 256 << 10;
    let (files, reads) = parse(&trace);

    let link = SimConfig {
        latency: Duration::from_millis(num("--rtt-ms", 220.0) as u64),
        jitter: Duration::from_millis(num("--jitter-ms", 40.0) as u64),
        per_conn_bps: (num("--conn-mbps", 3.0) * 1e6) as u64,
        total_bps: (num("--total-mbps", 10.0) * 1e6) as u64,
        stall_every: arg("--stall-every").and_then(|v| v.parse().ok()).map(|n| (n, Duration::from_millis(2500))),
        ..SimConfig::instant()
    };
    let be = Arc::new(SimBackend::new(SimConfig::instant()));
    for (fid, f) in &files {
        be.add_file(&f.path, f.size.max(1), *fid as u64 + 1);
    }
    let dir = std::env::temp_dir().join(format!("sfs-replay-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mk_cfg = || {
        let mut c = Config::new(&dir);
        c.cache_bytes = 16 << 30;
        c.min_fetch = (num("--min-fetch-kb", 512.0) as usize) << 10;
        c
    };

    // Warm state: preload the snapshot's blocks that this trace touches, over an instant link.
    let mut warm_n = 0u64;
    if let Some(ix) = arg("--warm-index") {
        let wb = warm_blocks(&ix);
        let mut touched: HashSet<(String, u32)> = HashSet::new();
        for r in &reads {
            if r.len >= skip_large || r.len == 0 {
                continue;
            }
            let f = &files[&r.fid];
            for b in (r.off / bs)..=((r.off + r.len - 1) / bs) {
                touched.insert((f.path.clone(), b as u32));
            }
        }
        let mut cfg = mk_cfg();
        cfg.readahead_max = 0;
        cfg.min_fetch = bs as usize;
        let eng = Engine::new(be.clone(), cfg).unwrap();
        let mut hs: HashMap<String, Handle> = HashMap::new();
        for (p, b) in touched.intersection(&wb) {
            if !hs.contains_key(p) {
                hs.insert(p.clone(), eng.open(p).await.unwrap());
            }
            let h = &hs[p];
            let off = *b as u64 * bs;
            if off < h.meta().size {
                h.read(off, bs.min(h.meta().size - off) as usize).await.unwrap();
                warm_n += 1;
            }
        }
        drop(hs);
        drop(eng);
    }

    be.set_config(link);
    be.reset_counters();
    let eng = Engine::new(be.clone(), mk_cfg()).unwrap();
    let mut hs: HashMap<u32, Arc<Handle>> = HashMap::new();
    for (fid, f) in &files {
        hs.insert(*fid, Arc::new(eng.open(&f.path).await.unwrap()));
    }

    // Original timeline -> per-read class: "load" if its original 2-s bucket had >= 1 s of waiting.
    let mut bucket_wait: HashMap<u64, u64> = HashMap::new();
    for r in &reads {
        if r.missed > 0 {
            *bucket_wait.entry(r.t_us / 2_000_000).or_default() += r.lat_us;
        }
    }
    let is_load = |r: &Read| bucket_wait.get(&(r.t_us / 2_000_000)).copied().unwrap_or(0) >= 1_000_000;

    let t_run = Instant::now();
    let mut prev_end_orig = 0u64;
    let mut skipped = (0u64, 0u64);
    // (class, latency, blocked?) per replayed read
    let mut res: Vec<(bool, u64, bool)> = Vec::with_capacity(reads.len());
    let mut orig = [(0u64, 0u64); 2]; // (blocked reads, wait us) in the original, [play, load]
    let mut pending: Vec<tokio::task::JoinHandle<(bool, u64, bool)>> = Vec::new();
    let mut owed = Duration::ZERO;
    for r in &reads {
        let load = is_load(r);
        if r.len >= skip_large {
            skipped.0 += 1;
            skipped.1 += r.len;
            continue;
        }
        if r.missed > 0 {
            orig[load as usize].0 += 1;
            orig[load as usize].1 += r.lat_us;
        }
        let think = r.t_us as i64 - prev_end_orig as i64;
        prev_end_orig = prev_end_orig.max(r.t_us + r.lat_us);
        let h = hs[&r.fid].clone();
        let (off, len) = (r.off, r.len as usize);
        let job = async move {
            let t = Instant::now();
            let (_, missed) = h.read_detailed(off, len).await.expect("replay read");
            (load, t.elapsed().as_micros() as u64, missed > 0)
        };
        if think >= 0 {
            for p in pending.drain(..) {
                res.push(p.await.unwrap());
            }
            // Windows timers tick at ~15 ms: sleeping for each tiny gap would stretch the run
            // by minutes. Gaps are accumulated and slept once they are worth a timer tick.
            owed += Duration::from_micros(think as u64).min(think_cap);
            if owed >= Duration::from_millis(20) {
                let t = Instant::now();
                tokio::time::sleep(owed).await;
                owed = owed.saturating_sub(t.elapsed());
            }
            res.push(job.await);
        } else {
            pending.push(tokio::spawn(job));
        }
    }
    for p in pending.drain(..) {
        res.push(p.await.unwrap());
    }
    let run_s = t_run.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let st = eng.stats();
    let ss = eng.store_stats();
    let mut out = serde_json::json!({
        "label": label, "trace": trace, "warm_blocks": warm_n, "run_s": run_s,
        "reads_replayed": res.len(), "skipped_large": skipped.0, "skipped_large_mb": skipped.1 as f64 / 1e6,
        "blocked_wall_s": st.blocked_wall_ms as f64 / 1e3,
        "backend_mb": st.backend_bytes as f64 / 1e6, "spec_mb": st.speculative_bytes as f64 / 1e6,
        "spec_used_mb": ss.spec_used_bytes as f64 / 1e6, "probation_mb": ss.probation_bytes as f64 / 1e6,
        "backend_requests": st.backend_requests,
        "min_fetch_kb": num("--min-fetch-kb", 512.0),
    });
    for (cls, name) in [(false, "play"), (true, "load")] {
        let mut lat: Vec<u64> = res.iter().filter(|x| x.0 == cls && x.2).map(|x| x.1).collect();
        let n = lat.len();
        let sum: u64 = lat.iter().sum();
        out[name] = serde_json::json!({
            "blocked": n, "wait_s": sum as f64 / 1e6,
            "p50_ms": pct(&mut lat, 0.5) / 1000, "p95_ms": pct(&mut lat, 0.95) / 1000, "p99_ms": pct(&mut lat, 0.99) / 1000, "max_ms": pct(&mut lat, 1.0) / 1000,
            "orig_blocked": orig[cls as usize].0, "orig_wait_s": orig[cls as usize].1 as f64 / 1e6,
        });
    }
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if let Some(o) = arg("--out") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(o).unwrap();
        writeln!(f, "{}", serde_json::to_string(&out).unwrap()).unwrap();
    }
    drop(hs);
    drop(eng);
    let _ = std::fs::remove_dir_all(&dir);
}
