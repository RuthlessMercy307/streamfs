//! sfs-mount: mount a backend as a read-only Windows drive.
//!
//! uso: sfs-mount --backend sim|local|gdrive|http --mount X: [opciones]
//!   http:   --token-file F [--url http://127.0.0.1:8787] [--root /steam/Juego] [--no-compress]
//!   --cache-dir DIR  --cache-mb N  --for-secs N  --stats-out FILE
//!   --no-learn: don't record/replay the learned startup profile (<cache-dir>/learn.json)
//!   sim:    archivos sinteticos verificables (/game.pak 20 GB, /small.bin 8 MB, /dir/data.bin 64 MB) seed 7
//!   local:  --dir RUTA   (carpeta real servida con latencia simulada: sirve para probar .exe y mmap)
//!   gdrive: --root FOLDER_ID (por defecto "root")  [--client ruta.json]
//!   --latency-ms N --bw-mbps N   (solo sim/local)  --min-fetch-kb N --max-conns N --fi-timeout-ms N

use sfs_core::{Backend, Config, Engine};
use sfs_mount::SfsFs;
use sfs_sim::{LocalDirBackend, SimBackend, SimConfig};
use std::sync::Arc;
use std::time::Duration;
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == flag)
        .and_then(|i| a.get(i + 1))
        .cloned()
}
fn num(flag: &str, default: u64) -> u64 {
    arg(flag).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let _init = winfsp::winfsp_init_or_die();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();

    let kind = arg("--backend").unwrap_or_else(|| "sim".into());
    let mut sim = SimConfig::instant();
    sim.latency = Duration::from_millis(num("--latency-ms", 0));
    let bw = num("--bw-mbps", 0) << 20;
    sim.per_conn_bps = bw;

    let mut http_metrics: Option<Arc<sfs_http::HttpStore>> = None;
    let backend: Arc<dyn Backend> = match kind.as_str() {
        "sim" => {
            let b = SimBackend::new(sim);
            b.add_file("game.pak", 20 << 30, 7);
            b.add_file("small.bin", 8 << 20, 11);
            b.add_file("dir/data.bin", 64 << 20, 13);
            let seq_mb = num("--sim-seq-mb", 0);
            if seq_mb > 0 {
                b.add_file("seq.bin", seq_mb << 20, 17);
            }
            Arc::new(b)
        }
        "local" => Arc::new(LocalDirBackend::new(
            arg("--dir").expect("--dir requerido"),
            sim,
        )),
        "gdrive" => {
            use sfs_gdrive::auth::{ClientConfig, OAuthTokens, RefreshStore};
            let cp = arg("--client")
                .map(Into::into)
                .unwrap_or_else(ClientConfig::default_path);
            let client = ClientConfig::from_file(&cp).expect("client_secret.json");
            let tokens = Arc::new(OAuthTokens::new(client, RefreshStore::new("default")));
            Arc::new(sfs_gdrive::DriveBackend::new(
                tokens,
                &arg("--root").unwrap_or_else(|| "root".into()),
            ))
        }
        "http" => {
            // sfs-server reached through the SSH tunnel; sfz1 wire compression on unless --no-compress
            let tf = arg("--token-file").expect("--token-file requerido");
            let token = std::fs::read_to_string(tf).expect("token file").trim().to_string();
            let url = arg("--url").unwrap_or_else(|| "http://127.0.0.1:8787".into());
            let compress = !std::env::args().any(|a| a == "--no-compress");
            let st = Arc::new(
                sfs_http::HttpStore::new(&url, &token, "ro")
                    .expect("http store")
                    .with_compression(compress)
                    .with_root(&arg("--root").unwrap_or_default()),
            );
            http_metrics = Some(st.clone());
            st
        }
        other => panic!("backend desconocido: {other}"),
    };

    let cache_dir = arg("--cache-dir").unwrap_or_else(|| {
        std::env::var("LOCALAPPDATA")
            .map(|d| format!("{d}\\streamfs\\cache"))
            .unwrap_or_else(|_| "cache".into())
    });
    let mut cfg = Config::new(&cache_dir);
    cfg.cache_bytes = num("--cache-mb", 1024) << 20;
    let cfg_cache_bytes = cfg.cache_bytes;
    cfg.min_fetch = (num("--min-fetch-kb", 512) << 10) as usize;
    cfg.max_conns = num("--max-conns", 8) as usize;
    cfg.readahead_init = (num("--ra-init-kb", 1024) << 10) as usize;
    cfg.readahead_max = (num("--ra-max-mb", 32) << 20) as usize;
    cfg.max_fetch = (num("--max-fetch-mb", 4) << 20) as usize;
    if let Some(n) = arg("--spec-conns").and_then(|v| v.parse::<usize>().ok()) {
        cfg.spec_conns = Some(n);
    }
    cfg.promote = !std::env::args().any(|a| a == "--no-promote");
    let eng = {
        let _g = rt.enter();
        Engine::new(backend, cfg).expect("engine")
    };
    // Full read log (one line per read) for analysis and replay: <cache-dir>/traces/reads-<unix>.log
    if !std::env::args().any(|a| a == "--no-read-log") {
        let p = arg("--read-log").map(std::path::PathBuf::from).unwrap_or_else(|| {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            std::path::Path::new(&cache_dir).join("traces").join(format!("reads-{t}.log"))
        });
        match sfs_core::readlog::ReadLog::create(&p) {
            Ok(l) => {
                println!("registro de lecturas: {}", p.display());
                let l = std::sync::Arc::new(l);
                eng.set_read_log(l.clone());
                rt.spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        l.flush();
                    }
                });
            }
            Err(e) => eprintln!("sin registro de lecturas ({}): {e}", p.display()),
        }
    }

    let profile_out = arg("--profile-out");
    let learn = !std::env::args().any(|a| a == "--no-learn") && profile_out.is_none();
    if profile_out.is_some() || learn {
        eng.start_trace(Duration::from_secs(num("--trace-secs", if learn { 1800 } else { 120 })), 200_000);
    }
    // Copy-on-write overlay: on by default, kept apart from the (evictable) block cache.
    let read_only = std::env::args().any(|a| a == "--read-only");
    let overlay = if read_only {
        None
    } else {
        let dir = arg("--overlay-dir").unwrap_or_else(|| {
            let tag = format!(
                "{}-{}",
                kind,
                arg("--root").unwrap_or_else(|| "root".into())
            )
            .replace(['\\', '/', ':'], "_");
            std::env::var("LOCALAPPDATA")
                .map(|d| format!("{d}\\streamfs\\overlay\\{tag}"))
                .unwrap_or_else(|_| "overlay".into())
        });
        println!("capa de escritura local (copy-on-write): {dir}");
        Some(sfs_mount::overlay::Overlay::open(std::path::Path::new(&dir)).expect("overlay"))
    };
    let fs = SfsFs::new(
        eng.clone(),
        handle,
        Duration::from_secs(300),
        overlay,
        num("--cow-max-mb", 512) << 20,
    );
    let cow = fs.cow_stats();
    let mut vp = VolumeParams::new();
    vp.filesystem_name(arg("--fs-name").unwrap_or_else(|| "NTFS".into()))
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_serial_number(0x5F5F)
        .file_info_timeout(num("--fi-timeout-ms", 1000) as u32)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(read_only);
    let mut host: FileSystemHost<SfsFs, FineGuard> =
        FileSystemHost::new_async(vp, fs).expect("FileSystemHost");
    let mount = arg("--mount").unwrap_or_else(|| "X:".into());
    host.mount(mount.clone()).expect("mount");
    host.start().expect("start");
    println!("montado en {mount} (backend {kind}); Ctrl+C o --for-secs para desmontar");

    if let Some(pin) = arg("--profile-in") {
        let budget = num("--prefetch-mb", 512) << 20;
        let eng3 = eng.clone();
        rt.spawn(async move {
            match sfs_core::Profile::load(std::path::Path::new(&pin)) {
                Ok(p) => {
                    let n = p.entries.len();
                    let held = eng3.prefetch(&p, budget).await;
                    println!(
                        "precarga encolada: {n} regiones ({:.1} MB perfil, tope {} MB)",
                        p.total_bytes() as f64 / 1e6,
                        budget >> 20
                    );
                    let _keep = held; // dropping the handles would cancel unstarted prefetches
                    std::future::pending::<()>().await;
                }
                Err(e) => println!("no se pudo leer el perfil {pin}: {e}"),
            }
        });
    }
    if learn {
        // Learned startup prefetch: replay what recurring sessions read, then record this one.
        let path = std::path::Path::new(&cache_dir).join("learn.json");
        let mut hist = sfs_core::learn::History::load(&path);
        let budget = (cfg_cache_bytes / 4).min(num("--learn-max-mb", 4096) << 20);
        let plan = hist.plan(budget);
        let slot = hist.begin();
        let eng5 = eng.clone();
        rt.spawn(async move {
            let n = plan.entries.len();
            let held = eng5.prefetch(&plan, budget).await;
            println!("precarga aprendida: {n} regiones, {:.1} MB", plan.total_bytes() as f64 / 1e6);
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                hist.sessions[slot] = sfs_core::learn::History::compact(&eng5.trace_snapshot());
                let _ = hist.save(&path);
                let _ = &held; // keep prefetch handles alive (dropping cancels queued guesses)
            }
        });
    }
    if let Some(path) = profile_out.clone() {
        let eng4 = eng.clone();
        rt.spawn(async move {
            loop {
                let _ = eng4.trace_snapshot().save(std::path::Path::new(&path));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
    if let Some(path) = arg("--stats-out") {
        // Periodic dump so benchmarks can kill the process and still read fresh numbers.
        let eng2 = eng.clone();
        let cow2 = cow.clone();
        let hm = http_metrics.clone();
        rt.spawn(async move {
            loop {
                let st = serde_json::json!({"engine": eng2.stats(), "store": eng2.store_stats(), "sched": eng2.sched_snapshot(), "http": hm.as_ref().map(|h| http_json(h)), "conn_limit": eng2.concurrency_limit(), "cow": cow_json(&cow2)});
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&st).unwrap());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let for_secs = arg("--for-secs").and_then(|s| s.parse::<u64>().ok());
    rt.block_on(async {
        match for_secs {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    });
    host.unmount();
    host.stop();
    let st = serde_json::json!({"engine": eng.stats(), "store": eng.store_stats(), "conn_limit": eng.concurrency_limit(), "cow": cow_json(&cow)});
    println!("{}", serde_json::to_string_pretty(&st).unwrap());
    if let Some(p) = arg("--stats-out") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&st).unwrap());
    }
}

fn http_json(h: &sfs_http::HttpStore) -> serde_json::Value {
    h.metrics.json()
}

fn cow_json(c: &sfs_mount::CowStats) -> serde_json::Value {
    use std::sync::atomic::Ordering::Relaxed;
    serde_json::json!({
        "copy_ups": c.copy_ups.load(Relaxed), "copy_up_bytes": c.copy_up_bytes.load(Relaxed),
        "creates": c.creates.load(Relaxed), "writes": c.writes.load(Relaxed), "write_bytes": c.write_bytes.load(Relaxed),
        "renames": c.renames.load(Relaxed), "deletes": c.deletes.load(Relaxed), "fsyncs": c.fsyncs.load(Relaxed),
        "write_errors": c.write_errors.load(Relaxed)
    })
}
