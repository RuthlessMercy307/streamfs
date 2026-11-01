//! engine-bench: reproducible mixed workload through the full read engine (cache, read-ahead,
//! scheduler) against a real sfs-server. Models what a game does while loading:
//!   * `--streams N` sequential readers (256 KiB reads, no think time): trigger read-ahead
//!   * one "urgent" reader: small random reads (4-64 KiB) with think time; each one blocks the
//!     game while it waits, so its latency distribution is THE metric
//!   * one bulk read of `--bulk-mb` MiB issued at t = secs/3 (game reading a whole file at once)
//! Fresh cache per run; same `--seed` = same offsets, so runs of different builds are comparable.
//!
//! uso: engine-bench --url u1,u2,.. --token-file F --root /dir --label L [--secs 60] [--seed 1]
//!        [--streams 2] [--bulk-mb 64] [--out results.jsonl] [--cache-dir D]

use sfs_core::{Config, Engine};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}
fn num(flag: &str, d: u64) -> u64 {
    arg(flag).and_then(|v| v.parse().ok()).unwrap_or(d)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * q).round() as usize]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let url = arg("--url").unwrap_or_else(|| "http://127.0.0.1:8787".into());
    let token = std::fs::read_to_string(arg("--token-file").expect("--token-file")).unwrap().trim().to_string();
    let root = arg("--root").unwrap_or_default();
    let label = arg("--label").unwrap_or_else(|| "run".into());
    let secs = num("--secs", 60);
    let seed = num("--seed", 1);
    let streams = num("--streams", 2) as usize;
    let bulk_mb = num("--bulk-mb", 64);
    let cache_dir = arg("--cache-dir").unwrap_or_else(|| std::env::temp_dir().join(format!("sfs-bench-cache-{label}-{seed}")).to_string_lossy().into_owned());
    let _ = std::fs::remove_dir_all(&cache_dir);

    let store = Arc::new(sfs_http::HttpStore::new(&url, &token, "ro").unwrap().with_compression(true).with_root(&root));
    let mut cfg = Config::new(&cache_dir);
    cfg.cache_bytes = 4 << 30;
    let eng = Engine::new(store.clone(), cfg).unwrap();

    let seq_files = ["Data/Cloth-part0.pak", "Data/Animations-part0.pak", "Data/Cloth-part1.pak", "Data/videos-part0.pak"];
    let rnd_files = ["Data/Objects.pak", "Data/Characters.pak", "Data/Buildings.pak", "Data/Heads.pak"];
    let mut rng = Rng(0x9E3779B97F4A7C15 ^ seed.wrapping_mul(0x2545F4914F6CDD1D));
    let mut rnd = Vec::new();
    for f in rnd_files {
        rnd.push(Arc::new(eng.open(f).await.expect("open random file")));
    }
    let t0 = Instant::now();
    let end = t0 + Duration::from_secs(secs);

    let mut tasks = Vec::new();
    for i in 0..streams {
        let h = eng.open(seq_files[i % seq_files.len()]).await.expect("open seq file");
        let start = (rng.next() % (h.meta().size / 2)) & !((1 << 20) - 1);
        tasks.push(tokio::spawn(async move {
            let mut off = start;
            let mut n = 0u64;
            while Instant::now() < end && off < h.meta().size {
                n += h.read(off, 256 << 10).await.expect("seq read").len() as u64;
                off += 256 << 10;
            }
            n
        }));
    }

    let bulk = {
        let eng = eng.clone();
        let off = (rng.next() % (512 << 20)) & !((1 << 20) - 1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs / 3)).await;
            let h = eng.open("Data/Cinematics.pak").await.expect("open bulk");
            let t = Instant::now();
            let n = h.read(off, (bulk_mb << 20) as usize).await.expect("bulk read").len();
            (n, t.elapsed().as_secs_f64())
        })
    };

    // urgent reader: the latency the player feels
    let mut lat = Vec::new();
    let mut slow = 0;
    while Instant::now() < end {
        let h = &rnd[(rng.next() % rnd.len() as u64) as usize];
        let len = 4096 << (rng.next() % 5); // 4..64 KiB
        let off = rng.next() % (h.meta().size - len);
        let t = Instant::now();
        let got = h.read(off, len as usize).await.expect("urgent read");
        assert_eq!(got.len() as u64, len);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms > 1000.0 {
            slow += 1;
        }
        lat.push(ms);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let mut seq_bytes = 0;
    for t in tasks {
        seq_bytes += t.await.unwrap();
    }
    let (bulk_n, bulk_s) = bulk.await.unwrap();
    let el = t0.elapsed().as_secs_f64();
    let st = eng.stats();
    let n = lat.len();
    let mean = lat.iter().sum::<f64>() / n.max(1) as f64;
    let res = serde_json::json!({
        "label": label, "seed": seed, "secs": el, "streams": streams,
        "urgent_n": n, "urgent_mean_ms": mean,
        "urgent_p50_ms": pct(&mut lat, 0.5), "urgent_p95_ms": pct(&mut lat, 0.95),
        "urgent_p99_ms": pct(&mut lat, 0.99), "urgent_max_ms": pct(&mut lat, 1.0), "urgent_over_1s": slow,
        "seq_mb_s": seq_bytes as f64 / 1e6 / el,
        "bulk_mb": bulk_n as f64 / 1e6, "bulk_s": bulk_s,
        "backend_mb": st.backend_bytes as f64 / 1e6, "spec_mb": st.speculative_bytes as f64 / 1e6,
        "served_mb": st.bytes_served as f64 / 1e6, "engine": st, "http": store.metrics.json(), "sched": eng.sched_snapshot(), "store": eng.store_stats(),
    });
    println!("{}", serde_json::to_string_pretty(&res).unwrap());
    if let Some(out) = arg("--out") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(out).unwrap();
        writeln!(f, "{}", serde_json::to_string(&res).unwrap()).unwrap();
    }
    drop(rnd);
    drop(eng);
    let _ = std::fs::remove_dir_all(&cache_dir);
}
