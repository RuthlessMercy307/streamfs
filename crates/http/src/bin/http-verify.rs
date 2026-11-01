//! End-to-end exactness check of sfz1: reads random ranges (random offsets and lengths, unaligned,
//! including file tails) through sfs-server with and without compression and compares every byte
//! with the same file read locally. The server must serve `--files` = the local `--dir`.
//! uso: http-verify --url U --token-file F --dir LOCALDIR [--n 300]

use sfs_core::Backend;
use sfs_http::HttpStore;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering::Relaxed;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, u64)>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, base, out);
        } else if let Ok(m) = e.metadata() {
            if m.len() > 0 {
                out.push((p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"), m.len()));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let url = arg("--url").unwrap_or_else(|| "http://127.0.0.1:8787".into());
    let token = std::fs::read_to_string(arg("--token-file").expect("--token-file")).unwrap().trim().to_string();
    let dir = std::path::PathBuf::from(arg("--dir").expect("--dir"));
    let n: usize = arg("--n").and_then(|v| v.parse().ok()).unwrap_or(300);
    let mut files = vec![];
    walk(&dir, &dir, &mut files);
    let raw = HttpStore::new(&url, &token, "verify").unwrap();
    let z = HttpStore::new(&url, &token, "verify").unwrap().with_compression(true);
    let mut s = 0x1234_5678_u64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut checked = 0u64;
    for i in 0..n {
        let (rel, size) = &files[(next() % files.len() as u64) as usize];
        let off = if i % 10 == 0 { size.saturating_sub(1 + next() % 3000) } else { next() % size };
        let len = 1 + next() % (if i % 3 == 0 { 9 << 20 } else { 300_000 });
        // every 4th read is engine-shaped: 256 KiB aligned, whole blocks (exercises the server cache)
        let (off, len) = if i % 4 == 1 { ((off >> 18) << 18, ((len >> 18) + 1) << 18) } else { (off, len) };
        let len = len.min(size - off);
        let mut want = vec![0u8; len as usize];
        let mut f = std::fs::File::open(dir.join(rel)).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        f.read_exact(&mut want).unwrap();
        let meta = raw.stat(rel).await.unwrap();
        let a = raw.read_range(&meta, off, len).await.unwrap();
        let b = z.read_range(&meta, off, len).await.unwrap();
        assert!(a[..] == want[..], "RAW mismatch {rel}@{off}+{len}");
        assert!(b[..] == want[..], "SFZ1 mismatch {rel}@{off}+{len}");
        checked += len;
    }
    let (wi, bi) = (z.metrics.wire_in.load(Relaxed), z.metrics.bytes_in.load(Relaxed));
    println!("OK: {n} lecturas, {:.1} MB comparados byte a byte (raw y sfz1); sfz1 recibio {:.1} MB por la red para {:.1} MB ({:.3}); decode {} ms; errores {}", checked as f64 / 1e6, wi as f64 / 1e6, bi as f64 / 1e6, wi as f64 / bi as f64, z.metrics.decode_us.load(Relaxed) / 1000, z.metrics.errors.load(Relaxed));
}
