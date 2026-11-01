//! Measures an sfs-server endpoint (normally through the SSH tunnel) the same way
//! drive-probe measured Google Drive: random-read latency per size, parallel
//! throughput, object upload throughput, and a write->read integrity check.
//! uso: http-probe --url http://127.0.0.1:8787 --token-file F [--file /bench/probe.bin] [--out r.json]

use sfs_core::{Backend, ObjectStore};
use sfs_http::HttpStore;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn pct(v: &[f64], q: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[(((s.len() - 1) as f64) * q).round() as usize]
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

async fn read_part(st: &HttpStore, m: &sfs_core::FileMeta, off: u64, len: u64) -> (f64, bytes::Bytes) {
    let t = Instant::now();
    let got = st.read_range(m, off, len).await.unwrap();
    assert_eq!(got.len() as u64, len);
    (t.elapsed().as_secs_f64() * 1e3, got)
}

#[tokio::main]
async fn main() {
    let url = arg("--url").unwrap_or_else(|| "http://127.0.0.1:8787".into());
    let token = std::fs::read_to_string(arg("--token-file").expect("--token-file")).unwrap().trim().to_string();
    let file = arg("--file").unwrap_or_else(|| "/bench/probe.bin".into());
    let compress = std::env::args().any(|a| a == "--compress");
    let store = Arc::new(HttpStore::new(&url, &token, "probe").unwrap().with_compression(compress));
    println!("tuneles {}  compresion sfz1 {}", store.tunnels(), if compress { "SI" } else { "no" });
    let mut report = serde_json::Map::new();
    let mb = 1u64 << 20;

    // --tree DIR: A/B of sfz1 on real files. Random distinct files (<= 4 MiB) of DIR are read whole,
    // alternating compressed / raw per read so both see the same link conditions; every file is
    // read once, so every compressed read is a server cache miss (worst case for sfz1).
    if let Some(dir) = arg("--tree") {
        let raw = HttpStore::new(&url, &token, "probe").unwrap();
        let z = HttpStore::new(&url, &token, "probe").unwrap().with_compression(true);
        // files > 4 MiB (e.g. .pak archives) are sampled with one random 1 MiB range per pick
        let mut files: Vec<_> = raw.list(&dir).await.unwrap().into_iter().filter(|m| !m.is_dir && m.size > 0).collect();
        // warm the connection pools of both stores on every tunnel, so neither path pays connection setup
        for st in [&raw, &z] {
            let mut w = vec![];
            for _ in 0..raw.tunnels() * 3 {
                w.push(st.stat(&dir));
            }
            for f in w {
                let _ = f.await;
            }
        }
        let big = files.iter().any(|m| m.size > 4 << 20);
        let mut s = 0xA5A5u64;
        for i in (1..files.len()).rev() {
            files.swap(i, (xorshift(&mut s) % (i as u64 + 1)) as usize);
        }
        // each file is read both ways (paired), alternating which path goes first
        let n = arg("--n").and_then(|v| v.parse().ok()).unwrap_or(60usize);
        let n = if big { n } else { n.min(files.len()) };
        let (mut lr, mut lz, mut diff, mut br, mut bz) = (vec![], vec![], vec![], 0u64, 0u64);
        for i in 0..n {
            let m = &files[i % files.len()];
            let (off, len) = if m.size > 4 << 20 { ((xorshift(&mut s) % (m.size - (1 << 20))) & !0xFFF, 1 << 20) } else { (0, m.size) };
            let ((tz, gz), (tr, gr)) = if i % 2 == 0 {
                let a = read_part(&z, m, off, len).await;
                (a, read_part(&raw, m, off, len).await)
            } else {
                let b = read_part(&raw, m, off, len).await;
                (read_part(&z, m, off, len).await, b)
            };
            assert!(gz == gr, "sfz1 and raw differ on {}", m.path);
            lz.push(tz);
            lr.push(tr);
            diff.push(tz - tr);
            bz += len;
            br += len;
        }
        let wz = z.metrics.wire_in.load(Relaxed) as f64 / bz as f64;
        println!("arbol {dir}: {} archivos leidos por ambos caminos (identicos), media {:.0} KB/archivo; mediana(sfz1 - raw) por archivo {:+.0} ms", lr.len(), br as f64 / 1e3 / lr.len() as f64, pct(&diff, 0.5));
        println!("   raw : p50 {:>5.0}  p95 {:>5.0}  p99 {:>5.0}  media {:>5.0} ms  {:.2} MB/s", pct(&lr, 0.5), pct(&lr, 0.95), pct(&lr, 0.99), lr.iter().sum::<f64>() / lr.len() as f64, br as f64 / 1e3 / lr.iter().sum::<f64>());
        println!("   sfz1: p50 {:>5.0}  p95 {:>5.0}  p99 {:>5.0}  media {:>5.0} ms  {:.2} MB/s  bytes por red {:.3} del original, decode {} ms total", pct(&lz, 0.5), pct(&lz, 0.95), pct(&lz, 0.99), lz.iter().sum::<f64>() / lz.len() as f64, bz as f64 / 1e3 / lz.iter().sum::<f64>(), wz, z.metrics.decode_us.load(Relaxed) / 1000);
        return;
    }

    // small request latency (stat)
    let mut lat = vec![];
    for _ in 0..10 {
        let t = Instant::now();
        let _ = store.stat(&file).await;
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("stat (peticion pequena): p50 {:.0} ms  p95 {:.0} ms", pct(&lat, 0.5), pct(&lat, 0.95));
    report.insert("stat_ms".into(), serde_json::json!({"p50": pct(&lat, 0.5), "p95": pct(&lat, 0.95)}));

    let meta = store.stat(&file).await.expect("stat bench file (create it on the server first)");
    println!("archivo {} {:.2} GB", meta.path, meta.size as f64 / 1e9);
    let n: usize = arg("--n").and_then(|v| v.parse().ok()).unwrap_or(30);
    // --mixed B: B background streams of 8 MB reads (read-ahead traffic) while n blocking
    // 256 KiB reads are timed one after another: the latency a game sees under load.
    if let Some(b) = arg("--mixed").and_then(|v| v.parse::<u64>().ok()) {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bg_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut bg = vec![];
        for k in 0..b {
            let (st, m, stop, bb) = (store.clone(), meta.clone(), stop.clone(), bg_bytes.clone());
            bg.push(tokio::spawn(async move {
                let mut off = (k * 300 * mb) % (m.size - 8 * mb);
                while !stop.load(Relaxed) {
                    if let Ok(x) = st.read_range(&m, off, 8 * mb).await {
                        bb.fetch_add(x.len() as u64, Relaxed);
                    }
                    off = (off + 8 * mb) % (m.size - 8 * mb);
                }
            }));
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let t0 = Instant::now();
        let b0 = bg_bytes.load(Relaxed);
        let mut s = 0xBEEFu64;
        let mut lat = vec![];
        for _ in 0..n {
            let off = xorshift(&mut s) % (meta.size - (256 << 10));
            let t = Instant::now();
            store.read_range(&meta, off, 256 << 10).await.unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let bgmb = (bg_bytes.load(Relaxed) - b0) as f64 / 1e6 / t0.elapsed().as_secs_f64();
        stop.store(true, Relaxed);
        for h in bg {
            let _ = h.await;
        }
        println!("mixto tuneles {} fondo {b}x8MB: 256K bloqueante p50 {:.0} p95 {:.0} p99 {:.0} max {:.0} ms; fondo {bgmb:.1} MB/s", store.tunnels(), pct(&lat, 0.5), pct(&lat, 0.95), pct(&lat, 0.99), pct(&lat, 1.0));
        return;
    }
    println!("{:>8} {:>4} {:>8} {:>8} {:>8} {:>8}", "tamano", "n", "p50 ms", "p95 ms", "p99 ms", "max ms");
    let mut s = 0xC0FFEEu64;
    let mut rows = vec![];
    for size in [256u64 << 10, mb, 4 * mb, 16 * mb] {
        let mut lat = vec![];
        for _ in 0..n {
            let off = xorshift(&mut s) % (meta.size - size);
            let t = Instant::now();
            store.read_range(&meta, off, size).await.unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("{:>7}K {:>4} {:>8.0} {:>8.0} {:>8.0} {:>8.0}", size >> 10, lat.len(), pct(&lat, 0.5), pct(&lat, 0.95), pct(&lat, 0.99), pct(&lat, 1.0));
        rows.push(serde_json::json!({"size": size, "n": lat.len(), "p50": pct(&lat, 0.5), "p95": pct(&lat, 0.95), "p99": pct(&lat, 0.99), "max": pct(&lat, 1.0)}));
    }
    report.insert("random".into(), rows.into());

    // sequential single stream (8 MB requests back to back)
    let t = Instant::now();
    let mut off = 0;
    while off < 256 * mb {
        store.read_range(&meta, off, 8 * mb).await.unwrap();
        off += 8 * mb;
    }
    let seq = 256.0 / t.elapsed().as_secs_f64();
    println!("secuencial 1 flujo (8 MB/peticion): {seq:.1} MB/s");
    report.insert("sequential_mb_s".into(), seq.into());

    // parallel
    let mut prow = vec![];
    for conns in [2u64, 4, 8, 16] {
        let t = Instant::now();
        let mut tasks = vec![];
        for i in 0..conns * 4 {
            let (st, m) = (store.clone(), meta.clone());
            let off = (i * 37 * mb) % (meta.size - 4 * mb);
            tasks.push(tokio::spawn(async move { st.read_range(&m, off, 4 * mb).await.map(|b| b.len()) }));
        }
        let mut bytes = 0;
        for x in tasks {
            bytes += x.await.unwrap().unwrap_or(0);
        }
        let mbs = bytes as f64 / 1e6 / t.elapsed().as_secs_f64();
        println!("paralelo {conns:>2} conexiones (4 MB): {mbs:.1} MB/s");
        prow.push(serde_json::json!({"conns": conns, "mb_s": mbs}));
    }
    report.insert("parallel".into(), prow.into());

    println!("bytes leidos {:.1} MB, por la red {:.1} MB", store.metrics.bytes_in.load(Relaxed) as f64 / 1e6, store.metrics.wire_in.load(Relaxed) as f64 / 1e6);
    if std::env::args().any(|a| a == "--no-upload") {
        if let Some(o) = arg("--out") {
            std::fs::write(o, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        }
        return;
    }
    // upload (objects) + integrity round trip
    let data: Vec<u8> = (0..(16 * mb) as usize).map(|i| (i as u64).wrapping_mul(2654435761) as u8).collect();
    let t = Instant::now();
    let r = store.put_object("probe-pack.bin", bytes::Bytes::from(data.clone())).await.unwrap();
    let up = 16.0 / t.elapsed().as_secs_f64();
    let back = store.read_range(&store.object_meta(&r), 0, r.size).await.unwrap();
    let ok = back[..] == data[..];
    store.delete_object(&r).await.unwrap();
    println!("subida de objeto 16 MB: {up:.2} MB/s; ida y vuelta {}", if ok { "IDENTICA" } else { "DISTINTA" });
    report.insert("upload_mb_s".into(), up.into());
    report.insert("roundtrip_ok".into(), ok.into());
    println!("peticiones {}  errores {}", store.metrics.requests.load(Relaxed), store.metrics.errors.load(Relaxed));
    if let Some(o) = arg("--out") {
        std::fs::write(o, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}
