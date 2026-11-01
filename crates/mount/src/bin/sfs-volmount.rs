//! sfs-volmount: mount a read/write write-back volume (data stored as packs in an object store).
//!
//! uso: sfs-volmount --store local|gdrive --mount U: --state-dir DIR [opciones]
//!   local:  --remote-dir DIR [--upload-mbps N] [--latency-ms N] [--free-gb N]   (simulated remote)
//!   gdrive: --vol-folder "streamfs-volumes/test1" [--client ruta.json] [--object-gap-ms 1000]
//!   --write-cache-mb N (default 512)  --pack-mb N (default 16)  --upload-parallel N (default 2)
//!   --quota-gb N  --for-secs N  --stats-out FILE  --read-cache-mb N (default 1024)
//!   --meta-every-secs N (default 600)  --min-free-gb N (reserve on the state disk, default 15)

use sfs_core::ObjectStore;
use sfs_mount::vol::{VolFs, UNMOUNT_SYNC_WAIT};
use sfs_volume::{VolConfig, Volume};
use std::sync::Arc;
use std::time::Duration;
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}
fn num(flag: &str, default: u64) -> u64 {
    arg(flag).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let _init = winfsp::winfsp_init_or_die();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let kind = arg("--store").unwrap_or_else(|| "local".into());
    let (store, drive_metrics): (Arc<dyn ObjectStore>, Option<Arc<sfs_gdrive::DriveBackend>>) = match kind.as_str() {
        "local" => {
            let dir = arg("--remote-dir").expect("--remote-dir");
            let s = sfs_sim::LocalObjectStore::new(
                dir,
                num("--upload-mbps", 0) << 20,
                Duration::from_millis(num("--latency-ms", 0)),
                Some(num("--free-gb", 1024) << 30),
            );
            (Arc::new(s), None)
        }
        "gdrive" => {
            use sfs_gdrive::auth::{ClientConfig, OAuthTokens, RefreshStore};
            let cp = arg("--client").map(Into::into).unwrap_or_else(ClientConfig::default_path);
            let client = ClientConfig::from_file(&cp).expect("client_secret.json");
            let tokens = Arc::new(OAuthTokens::new(client, RefreshStore::new("default")));
            let drive = Arc::new(sfs_gdrive::DriveBackend::new(tokens, "root"));
            let folder_path = arg("--vol-folder").unwrap_or_else(|| "streamfs-volumes/test".into());
            let folder = rt.block_on(drive.ensure_folder_path(&folder_path)).expect("volume folder");
            println!("objetos del volumen en Drive: {folder_path} ({folder})");
            let gap = Duration::from_millis(num("--object-gap-ms", 1000));
            (Arc::new(sfs_gdrive::DriveObjectStore::new(drive.clone(), &folder, gap)), Some(drive))
        }
        other => panic!("store desconocido: {other}"),
    };

    let mut cfg = VolConfig::new(arg("--state-dir").expect("--state-dir"));
    cfg.write_cache_bytes = num("--write-cache-mb", 512) << 20;
    cfg.pack_bytes = (num("--pack-mb", 16) << 20) as usize;
    cfg.upload_parallel = num("--upload-parallel", 2) as usize;
    cfg.read_cache_bytes = num("--read-cache-mb", 1024) << 20;
    cfg.meta_every = Duration::from_secs(num("--meta-every-secs", 600));
    cfg.min_local_free = num("--min-free-gb", 15) << 30;
    if let Some(q) = arg("--quota-gb").and_then(|v| v.parse::<u64>().ok()) {
        cfg.quota_bytes = Some(q << 30);
    }
    let vol = rt.block_on(Volume::open(cfg, store)).expect("volume");
    println!("volumen abierto; cache de escritura {} MB", num("--write-cache-mb", 512));

    let fs = VolFs::new(vol.clone(), rt.handle().clone());
    let mut vp = VolumeParams::new();
    vp.filesystem_name("NTFS")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_serial_number(0x5F60)
        .file_info_timeout(1000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(false)
        // Backpressure can hold a write while a slow uplink drains a pack: allow the
        // maximum IRP timeout WinFsp supports (10 min) so Windows does not cancel it.
        .irp_timeout(600_000);
    let mut host: FileSystemHost<VolFs, FineGuard> = FileSystemHost::new_async(vp, fs).expect("host");
    let mount = arg("--mount").unwrap_or_else(|| "U:".into());
    host.mount(mount.clone()).expect("mount");
    host.start().expect("start");
    println!("montado en {mount}");

    if let Some(path) = arg("--stats-out") {
        let v2 = vol.clone();
        let dm = drive_metrics.clone();
        rt.spawn(async move {
            loop {
                let mut j = serde_json::json!({"volume": v2.stats(), "read_engine": v2.read_engine().stats(), "space": v2.space().await});
                if let Some(d) = &dm {
                    use std::sync::atomic::Ordering::Relaxed;
                    j["drive"] = serde_json::json!({
                        "upload_requests": d.metrics.upload_requests.load(Relaxed), "upload_bytes": d.metrics.upload_bytes.load(Relaxed),
                        "media_requests": d.metrics.media_requests.load(Relaxed), "media_bytes": d.metrics.media_bytes.load(Relaxed),
                        "metadata_requests": d.metrics.metadata_requests.load(Relaxed),
                        "http_403": d.metrics.http_403.load(Relaxed), "http_429": d.metrics.http_429.load(Relaxed)});
                }
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&j).unwrap());
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
    let synced = rt.block_on(async { tokio::time::timeout(UNMOUNT_SYNC_WAIT, vol.sync_remote()).await });
    match synced {
        Ok(Ok(())) => println!("todo subido y confirmado"),
        _ => println!("quedan datos pendientes de subir: estan a salvo en local y se reanudan en el proximo montaje"),
    }
    vol.shutdown();
    println!("{}", serde_json::to_string_pretty(&vol.stats()).unwrap());
}
