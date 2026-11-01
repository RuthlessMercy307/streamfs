//! Reads a file on a mounted drive through the normal Win32 API and reports how
//! long the *calling process is blocked* per read (not just throughput).
//!
//! uso: mount-bench --file V:\archivo.bin [--reads 30] [--size 1048576] [--seq-mb 32] [--seed 1] [--label x] [--out r.json]
//! Pass 1 = random reads (cold cache). Pass 2 = same offsets again (warm).
//! Then a sequential scan from a fresh region. "stalls" = reads that blocked > 500 ms.

use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn pct(v: &[f64], q: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[(((s.len() - 1) as f64) * q).round() as usize]
}

fn summary(name: &str, lat: &[f64]) -> serde_json::Value {
    let stalls = lat.iter().filter(|&&x| x > 500.0).count();
    println!(
        "{name:>22}: n={:<3} p50 {:8.1} ms  p95 {:8.1} ms  p99 {:8.1} ms  max {:8.1} ms  stalls>500ms: {stalls}",
        lat.len(), pct(lat, 0.5), pct(lat, 0.95), pct(lat, 0.99), pct(lat, 1.0)
    );
    serde_json::json!({"pass":name,"n":lat.len(),"p50_ms":pct(lat,0.5),"p95_ms":pct(lat,0.95),"p99_ms":pct(lat,0.99),"max_ms":pct(lat,1.0),"stalls_over_500ms":stalls})
}

fn main() {
    let path = arg("--file").expect("--file");
    let reads: usize = arg("--reads").and_then(|v| v.parse().ok()).unwrap_or(30);
    let size: usize = arg("--size").and_then(|v| v.parse().ok()).unwrap_or(1 << 20);
    let seq_mb: u64 = arg("--seq-mb").and_then(|v| v.parse().ok()).unwrap_or(32);
    let mut seed: u64 = arg("--seed").and_then(|v| v.parse().ok()).unwrap_or(1) * 0x9E37_79B9_7F4A_7C15 | 1;
    let label = arg("--label").unwrap_or_default();

    let t_open = Instant::now();
    let mut f = std::fs::File::open(&path).expect("open");
    let flen = f.metadata().unwrap().len();
    println!("archivo {path}: {:.2} GB  (open+stat {:.0} ms) {label}", flen as f64 / 1e9, t_open.elapsed().as_secs_f64() * 1e3);

    if std::env::args().any(|a| a == "--boot") {
        // Boot scenario: read the regions once, in order, like a program starting up.
        let offs: Vec<u64> = (0..reads).map(|_| (xorshift(&mut seed) % (flen / 2 - size as u64)) & !0xFFF).collect();
        let mut buf = vec![0u8; size];
        let mut lat = vec![];
        let t0 = Instant::now();
        for &o in &offs {
            let t = Instant::now();
            f.seek(SeekFrom::Start(o)).unwrap();
            f.read_exact(&mut buf).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let total = t0.elapsed().as_secs_f64() * 1e3;
        println!("{:>22}: {total:.0} ms for {} reads (time until the program could continue)", "BOOT total", lat.len());
        let mut r = summary("boot reads", &lat);
        r["boot_total_ms"] = total.into();
        if let Some(out) = arg("--out") {
            std::fs::write(out, serde_json::to_string_pretty(&r).unwrap()).unwrap();
        }
        return;
    }

    let seq_bytes = seq_mb << 20;
    let offs: Vec<u64> = (0..reads).map(|_| (xorshift(&mut seed) % (flen / 2 - size as u64)) & !0xFFF).collect();
    let mut buf = vec![0u8; size];
    let mut report = vec![];

    // time to first byte = "first interaction" proxy: the very first read of the session
    let t = Instant::now();
    f.seek(SeekFrom::Start(offs[0])).unwrap();
    f.read_exact(&mut buf).unwrap();
    let first_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("{:>22}: {first_ms:.1} ms", "first read");

    for pass in ["random cold", "random repeat (warm)"] {
        let mut lat = vec![];
        for &o in &offs {
            let t = Instant::now();
            f.seek(SeekFrom::Start(o)).unwrap();
            f.read_exact(&mut buf).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        report.push(summary(pass, &lat));
    }

    // sequential scan in 256 KiB reads from the second half of the file (never touched above)
    let start = (flen / 2) & !0xFFF;
    let mut chunk = vec![0u8; 256 << 10];
    let mut lat = vec![];
    let t0 = Instant::now();
    let mut done = 0u64;
    f.seek(SeekFrom::Start(start)).unwrap();
    while done < seq_bytes {
        let t = Instant::now();
        let n = f.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        lat.push(t.elapsed().as_secs_f64() * 1e3);
        done += n as u64;
    }
    let secs = t0.elapsed().as_secs_f64();
    let mut seq = summary("sequential 256K reads", &lat);
    println!("{:>22}: {:.1} MB/s over {} MB", "sequential throughput", done as f64 / 1e6 / secs, done >> 20);
    seq["mb_s"] = (done as f64 / 1e6 / secs).into();
    report.push(seq);

    if let Some(out) = arg("--out") {
        let v = serde_json::json!({"label":label,"file_gb":flen as f64/1e9,"first_read_ms":first_ms,"passes":report});
        std::fs::write(out, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}
