//! Measures the REAL Drive backend on one existing file, under a hard transfer budget.
//!
//! uso: drive-probe --path "/Carpeta/archivo.bin" [--budget-mb 256] [--root ID] [--client ruta.json] [--out drive-probe.json]
//!
//! Every ranged read goes through BudgetBackend, so total bytes pulled can never
//! exceed the budget (default 256 MB). Reads are spaced out to avoid hammering.

use sfs_core::{Backend, Config, Engine};
use sfs_gdrive::auth::{ClientConfig, OAuthTokens, RefreshStore};
use sfs_gdrive::{BudgetBackend, DriveBackend};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn pct(v: &mut Vec<f64>, q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * q).round() as usize]
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[tokio::main]
async fn main() {
    let (path, by_id) = match (arg("--path"), arg("--id")) {
        (Some(p), _) => (p, None),
        (None, Some(id)) => (String::new(), Some(id)),
        _ => {
            eprintln!("falta --path \"/ruta/en/Drive/archivo\" o --id <fileId>");
            std::process::exit(2);
        }
    };
    let budget = arg("--budget-mb").and_then(|s| s.parse::<u64>().ok()).unwrap_or(256) << 20;
    let root = arg("--root").unwrap_or_else(|| "root".into());
    let client_path = arg("--client").map(Into::into).unwrap_or_else(ClientConfig::default_path);
    let out = arg("--out").unwrap_or_else(|| "drive-probe.json".into());

    let client = ClientConfig::from_file(&client_path).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let tokens = Arc::new(OAuthTokens::new(client, RefreshStore::new("default")));
    let drive = Arc::new(DriveBackend::new(tokens, &root));
    let budgeted = Arc::new(BudgetBackend::new(drive.clone(), budget));
    let be: Arc<dyn Backend> = budgeted.clone();

    let stat = match &by_id {
        Some(id) => drive.stat_id(id).await,
        None => be.stat(&path).await,
    };
    let meta = stat.unwrap_or_else(|e| {
        eprintln!("stat fallo: {e}");
        std::process::exit(1)
    });
    println!("archivo: {}  tamano: {:.2} GB  version: {}", meta.path, meta.size as f64 / 1e9, meta.version);
    println!("presupuesto de transferencia: {} MB\n", budget >> 20);
    let mut report = serde_json::Map::new();
    let mb = 1u64 << 20;

    // 1. Latencia de lecturas aleatorias por tamano (sin cache, sin motor).
    println!("== lecturas aleatorias directas (1 conexion) ==");
    println!("{:>8} {:>5} {:>9} {:>9} {:>9} {:>9}", "tamano", "n", "p50 ms", "p95 ms", "max ms", "MB/s");
    let mut s = 0x1234_5678_9ABC_DEF1u64;
    let mut rand_rows = vec![];
    for size in [256u64 << 10, mb, 4 * mb, 16 * mb] {
        if meta.size < size * 2 {
            continue;
        }
        let n = 6;
        let (mut lat, mut bytes) = (vec![], 0u64);
        let t0 = Instant::now();
        for _ in 0..n {
            let off = xorshift(&mut s) % (meta.size - size);
            let t = Instant::now();
            match be.read_range(&meta, off, size).await {
                Ok(b) => {
                    bytes += b.len() as u64;
                    lat.push(t.elapsed().as_secs_f64() * 1e3);
                }
                Err(e) => {
                    println!("  error: {e}");
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let mbps = bytes as f64 / 1e6 / t0.elapsed().as_secs_f64();
        let (p50, p95, mx) = (pct(&mut lat.clone(), 0.5), pct(&mut lat.clone(), 0.95), pct(&mut lat.clone(), 1.0));
        println!("{:>7}K {:>5} {:>9.0} {:>9.0} {:>9.0} {:>9.1}", size >> 10, lat.len(), p50, p95, mx, mbps);
        rand_rows.push(serde_json::json!({"size":size,"n":lat.len(),"p50_ms":p50,"p95_ms":p95,"max_ms":mx,"mb_s_incl_pauses":mbps}));
    }
    report.insert("random_direct".into(), rand_rows.into());

    // 2. Concurrencia: lecturas de 2 MB en paralelo. Mas conexiones != mas velocidad.
    println!("\n== concurrencia (lecturas de 2 MB) ==");
    println!("{:>6} {:>10} {:>9} {:>6} {:>6}", "conns", "MB/s", "req/s", "403", "429");
    let mut conc_rows = vec![];
    for conns in [1u64, 2, 4, 8] {
        if budgeted.used() + conns * 2 * mb * 2 > budget {
            break;
        }
        let (h403, h429) = (drive.metrics.http_403.load(Relaxed), drive.metrics.http_429.load(Relaxed));
        let t0 = Instant::now();
        let mut tasks = vec![];
        for i in 0..conns * 2 {
            let (be, meta) = (be.clone(), meta.clone());
            let off = (xorshift(&mut s) % (meta.size - 2 * mb)) & !0xFFFF;
            tasks.push(tokio::spawn(async move {
                let _ = i;
                be.read_range(&meta, off, 2 * mb).await.map(|b| b.len() as u64)
            }));
        }
        let mut bytes = 0;
        for t in tasks {
            if let Ok(Ok(n)) = t.await {
                bytes += n;
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let (d403, d429) = (drive.metrics.http_403.load(Relaxed) - h403, drive.metrics.http_429.load(Relaxed) - h429);
        println!("{:>6} {:>10.1} {:>9.1} {:>6} {:>6}", conns, bytes as f64 / 1e6 / secs, (conns * 2) as f64 / secs, d403, d429);
        conc_rows.push(serde_json::json!({"conns":conns,"mb_s":bytes as f64/1e6/secs,"http_403":d403,"http_429":d429}));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    report.insert("concurrency".into(), conc_rows.into());

    // 3. El motor sobre Drive real: leer una fraccion del archivo, frio y caliente.
    println!("\n== motor sobre Drive real: 10 MB aleatorios, frio y caliente ==");
    let dir = std::env::temp_dir().join(format!("sfs-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = Config::new(&dir);
    cfg.cache_bytes = 1 << 30;
    let eng = Engine::new(be.clone(), cfg).unwrap();
    let h = eng.open_meta(meta.clone());
    let before = budgeted.used();
    let off = (xorshift(&mut s) % (meta.size.saturating_sub(10 * mb).max(1))) & !0xFFFF;
    let mut engine_rows = vec![];
    for pass in ["frio", "caliente"] {
        let t = Instant::now();
        let r = h.read(off, (10 * mb) as usize).await;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        match r {
            Ok(b) => println!("{pass:>9}: {} bytes en {ms:.0} ms; descargado hasta ahora {:.1} MB", b.len(), (budgeted.used() - before) as f64 / 1e6),
            Err(e) => println!("{pass:>9}: error {e}"),
        }
        engine_rows.push(serde_json::json!({"pass":pass,"ms":ms,"net_bytes_total":budgeted.used()-before}));
    }
    let st = eng.stats();
    println!("fraccion del archivo descargada: {:.4}%", (budgeted.used() - before) as f64 / meta.size as f64 * 100.0);
    report.insert("engine".into(), serde_json::json!({"rows":engine_rows,"stats":st,"file_size":meta.size}));

    let m = &drive.metrics;
    println!(
        "\n== cuota consumida en esta ejecucion ==\nmetadata: {}  media: {}  bytes media: {:.1} MB  403: {}  429: {}  5xx: {}  401: {}  (presupuesto usado {:.1}/{} MB)",
        m.metadata_requests.load(Relaxed), m.media_requests.load(Relaxed), m.media_bytes.load(Relaxed) as f64 / 1e6,
        m.http_403.load(Relaxed), m.http_429.load(Relaxed), m.http_5xx.load(Relaxed), m.http_401.load(Relaxed),
        budgeted.used() as f64 / 1e6, budget >> 20
    );
    report.insert("quota".into(), serde_json::json!({
        "metadata_requests": m.metadata_requests.load(Relaxed), "media_requests": m.media_requests.load(Relaxed),
        "media_bytes": m.media_bytes.load(Relaxed), "http_403": m.http_403.load(Relaxed),
        "http_429": m.http_429.load(Relaxed), "http_5xx": m.http_5xx.load(Relaxed)}));
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("resultados -> {out}");
    let _ = std::fs::remove_dir_all(dir);
}
