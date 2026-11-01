//! Compression study on real game blocks.
//! Samples blocks byte-weighted across the given trees (read-only), compresses each with every
//! candidate, decodes through the real frame path (CRC included) and models the time to hand the
//! block to the game: rtt + frame_bytes / bandwidth + decode (+ encode when the server has no cached copy).
//! uso: compbench --dir D [--dir D2 ...] [--block 1048576] [--samples 300] [--rtt-ms 131] [--out r.json]

use sfs_codec::{decode, encode, timed, Algo};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

fn args(flag: &str) -> Vec<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().enumerate().filter(|(_, x)| *x == flag).filter_map(|(i, _)| a.get(i + 1).cloned()).collect()
}
fn arg(flag: &str) -> Option<String> {
    args(flag).into_iter().next()
}

fn walk(dir: &std::path::Path, out: &mut Vec<(PathBuf, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), out);
        } else if ft.is_file() {
            if let Ok(m) = e.metadata() {
                if m.len() > 0 {
                    out.push((e.path(), m.len()));
                }
            }
        }
    }
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * q).round() as usize]
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

struct Sample {
    ext: String,
    raw: usize,
    // per algo: (frame bytes, encode s, decode s)
    r: Vec<(usize, f64, f64)>,
}

fn main() {
    let dirs = args("--dir");
    assert!(!dirs.is_empty(), "--dir required");
    let block: u64 = arg("--block").and_then(|v| v.parse().ok()).unwrap_or(1 << 20);
    let n: usize = arg("--samples").and_then(|v| v.parse().ok()).unwrap_or(300);
    let rtt = arg("--rtt-ms").and_then(|v| v.parse::<f64>().ok()).unwrap_or(131.0) / 1e3;
    let algos = [Algo::Raw, Algo::Lz4, Algo::Zstd(1), Algo::Zstd(2), Algo::Zstd(3)];

    let mut files = vec![];
    for d in &dirs {
        walk(std::path::Path::new(d), &mut files);
    }
    let total: u64 = files.iter().map(|f| f.1).sum();
    println!("{} archivos, {:.2} GB; bloque {} KiB; {} muestras ponderadas por bytes", files.len(), total as f64 / 1e9, block >> 10, n);
    let mut cum = Vec::with_capacity(files.len());
    let mut acc = 0u64;
    for f in &files {
        acc += f.1;
        cum.push(acc);
    }

    let mut s = 0x5EED_u64;
    let mut samples = vec![];
    let mut buf = vec![0u8; block as usize];
    while samples.len() < n {
        let pos = xorshift(&mut s) % total;
        let i = cum.partition_point(|&c| c <= pos);
        let (path, size) = &files[i];
        let start = cum[i] - size;
        let off = ((pos - start) / block) * block;
        let len = block.min(size - off) as usize;
        let Ok(mut fh) = std::fs::File::open(path) else { continue };
        if fh.seek(SeekFrom::Start(off)).is_err() || fh.read_exact(&mut buf[..len]).is_err() {
            continue;
        }
        let raw = &buf[..len];
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "-".into());
        let mut r = vec![];
        for &a in &algos {
            // min_gain 0: measure what each algorithm really produces (fallback is applied in the model)
            let ((frame, _), enc) = timed(|| encode(a, raw, -1.0));
            let (back, dec) = timed(|| decode(&frame, 64 << 20).expect("own frame decodes"));
            assert_eq!(back.0, raw, "roundtrip mismatch {path:?}@{off}");
            r.push((frame.len(), enc, dec));
        }
        samples.push(Sample { ext, raw: len, r });
    }

    // --- per-algorithm ratio and CPU speed
    let raw_total: usize = samples.iter().map(|x| x.raw).sum();
    println!("\n{:>7} {:>7} {:>11} {:>11} {:>9}", "algo", "ratio", "enc MB/s", "dec MB/s", "bloques<95%");
    let mut report = serde_json::Map::new();
    let mut arows = vec![];
    for (k, a) in algos.iter().enumerate() {
        let bytes: usize = samples.iter().map(|x| x.r[k].0).sum();
        let enc: f64 = samples.iter().map(|x| x.r[k].1).sum();
        let dec: f64 = samples.iter().map(|x| x.r[k].2).sum();
        let gain = samples.iter().filter(|x| (x.r[k].0 as f64) < 0.95 * x.raw as f64).count();
        let ratio = bytes as f64 / raw_total as f64;
        println!("{:>7} {:>7.3} {:>11.0} {:>11.0} {:>8.0}%", a.name(), ratio, raw_total as f64 / 1e6 / enc, raw_total as f64 / 1e6 / dec, 100.0 * gain as f64 / samples.len() as f64);
        arows.push(serde_json::json!({"algo": a.name(), "ratio": ratio, "enc_mb_s": raw_total as f64 / 1e6 / enc, "dec_mb_s": raw_total as f64 / 1e6 / dec, "blocks_gain_5pct": gain}));
    }
    report.insert("algos".into(), arows.into());

    // --- per extension (bytes-weighted), best zstd3 ratio
    let mut by_ext: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    for x in &samples {
        let e = by_ext.entry(x.ext.clone()).or_default();
        e.0 += 1;
        e.1 += x.raw;
        e.2 += x.r[1].0;
        e.3 += x.r[4].0;
    }
    let mut ev: Vec<_> = by_ext.into_iter().collect();
    ev.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    println!("\n{:>10} {:>5} {:>9} {:>9}", "ext", "n", "lz4", "zstd3");
    for (e, (c, raw, l, z)) in ev.iter().take(15) {
        println!("{:>10} {:>5} {:>9.3} {:>9.3}", e, c, *l as f64 / *raw as f64, *z as f64 / *raw as f64);
    }

    // --- delivery-time model: one block, blocking reader
    let bws: Vec<f64> = {
        let v: Vec<f64> = args("--bw-mb-s").iter().filter_map(|x| x.parse().ok()).collect();
        if v.is_empty() { vec![2.6, 5.0, 11.2, 19.75] } else { v }
    };
    let mut drows = vec![];
    for &bw in &bws {
        println!("\n== entrega de un bloque a {bw} MB/s, rtt {:.0} ms (servidor con cache comprimida | sin cache) ==", rtt * 1e3);
        println!("{:>9} {:>9} {:>9} {:>9} {:>9} {:>11}", "metodo", "MB env", "p50 ms", "p95 ms", "p99 ms", "p50 sinC");
        let bps = bw * 1e6;
        let mut methods: Vec<(String, Vec<f64>, Vec<f64>, f64)> = vec![];
        for (k, a) in algos.iter().enumerate() {
            let mut t = vec![];
            let mut tu = vec![];
            let mut sent = 0.0;
            for x in &samples {
                // apply the real RAW fallback: if it doesn't gain 5%, RAW is sent
                let (b, e, d) = if k > 0 && (x.r[k].0 as f64) > 0.95 * x.raw as f64 { (x.r[0].0, x.r[k].1, x.r[0].2) } else { x.r[k] };
                sent += b as f64;
                t.push(rtt + b as f64 / bps + d);
                tu.push(rtt + b as f64 / bps + d + if k > 0 { e } else { 0.0 });
            }
            methods.push((a.name(), t, tu, sent));
        }
        // adaptive oracle: per block the fastest (cached) delivery; this is the upper bound of any selector
        let mut t = vec![];
        let mut sent = 0.0;
        for x in &samples {
            let (best_b, best_t) = x.r.iter().map(|&(b, _, d)| (b, rtt + b as f64 / bps + d)).fold((0, f64::MAX), |m, v| if v.1 < m.1 { v } else { m });
            sent += best_b as f64;
            t.push(best_t);
        }
        methods.push(("adapt*".into(), t.clone(), t, sent));
        let mut rows = vec![];
        for (name, mut t, mut tu, sent) in methods {
            let row = serde_json::json!({"method": name, "mb_sent": sent / 1e6, "p50": pct(&mut t, 0.5) * 1e3, "p95": pct(&mut t, 0.95) * 1e3, "p99": pct(&mut t, 0.99) * 1e3, "mean": t.iter().sum::<f64>() / t.len() as f64 * 1e3, "p50_uncached": pct(&mut tu, 0.5) * 1e3});
            println!("{:>9} {:>9.1} {:>9.0} {:>9.0} {:>9.0} {:>11.0}", name, sent / 1e6, row["p50"].as_f64().unwrap(), row["p95"].as_f64().unwrap(), row["p99"].as_f64().unwrap(), row["p50_uncached"].as_f64().unwrap());
            rows.push(row);
        }
        drows.push(serde_json::json!({"bw_mb_s": bw, "rows": rows}));
    }
    report.insert("delivery".into(), drows.into());
    report.insert("samples".into(), samples.len().into());
    report.insert("block".into(), block.into());
    report.insert("raw_mb".into(), (raw_total as f64 / 1e6).into());
    if let Some(o) = arg("--out") {
        std::fs::write(o, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}
