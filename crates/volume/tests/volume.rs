use sfs_sim::{expected, LocalObjectStore};
use sfs_volume::{VolConfig, Volume};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MB: u64 = 1 << 20;

fn dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sfs-vol-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    (base.join("state"), base.join("remote"))
}

fn cfg(state: &PathBuf, cache: u64) -> VolConfig {
    let mut c = VolConfig::new(state);
    c.write_cache_bytes = cache;
    c.pack_bytes = (4 * MB) as usize;
    c.seal_idle = Duration::from_millis(300);
    c.read_cache_bytes = 256 * MB;
    c
}

async fn read_all_equals(v: &Volume, ino: u64, size: u64, seed: u64) -> bool {
    let mut off = 0;
    while off < size {
        let n = (8 * MB).min(size - off) as usize;
        let got = v.read(ino, off, n).await.unwrap();
        if got != expected(seed, off, n) {
            eprintln!("mismatch at {off}");
            return false;
        }
        off += n as u64;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_much_larger_than_write_cache_out_of_order_unaligned() {
    let (state, remote) = dirs("big");
    let store = Arc::new(LocalObjectStore::new(&remote, 400 << 20, Duration::from_millis(2), Some(1 << 40)));
    let cache = 16 * MB;
    let v = Volume::open(cfg(&state, cache), store.clone()).await.unwrap();
    let size = 512 * MB;
    let a = v.create("big.bin", false).unwrap();
    v.set_size(a.ino, size).await.unwrap(); // preallocation: holes, nothing uploaded
    assert_eq!(store.puts.load(Relaxed), 0);

    // Steam-like: chunks not aligned to blocks, written in shuffled order.
    let chunk = 768 << 10;
    let mut offs: Vec<u64> = (0..size.div_ceil(chunk)).map(|i| i * chunk).collect();
    let mut s = 12345u64;
    for i in (1..offs.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        offs.swap(i, (s % (i as u64 + 1)) as usize);
    }
    let t = Instant::now();
    let mut max_slab = 0;
    for off in offs {
        let n = chunk.min(size - off) as usize;
        v.write(a.ino, off, &expected(77, off, n)).await.unwrap();
        let st = v.stats();
        assert!(st.dirty_bytes <= cache, "dirty {} > cache {}", st.dirty_bytes, cache);
        max_slab = max_slab.max(st.slab_file_bytes);
    }
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    let st = v.stats();
    println!("512 MiB through a 16 MiB cache in {:?}: {st:?}", t.elapsed());
    println!("remote objects {} stored {} MB, puts {}, local slab max {} MB", store.object_count(), store.stored_bytes() / MB, store.puts.load(Relaxed), max_slab / MB);
    assert!(max_slab <= cache, "slab file grew to {max_slab}");
    assert!(st.dirty_peak_bytes <= cache);
    assert_eq!(st.dirty_bytes, 0, "everything confirmed remotely");
    assert!(read_all_equals(&v, a.ino, size, 77).await, "content mismatch");
    // Worst case (shuffled AND unaligned through a tiny cache): partially written blocks
    // get uploaded early and redone later. Measured, bounded, documented (not hidden).
    let amp = st.uploaded_bytes as f64 / size as f64;
    println!("upload amplification {amp:.2}x, remote stored {:.2}x", store.stored_bytes() as f64 / size as f64);
    assert!(amp <= 2.5, "upload amplification {amp:.2}x");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_upload_applies_backpressure_and_reads_see_pending_data() {
    let (state, remote) = dirs("bp");
    let store = Arc::new(LocalObjectStore::new(&remote, 32 << 20, Duration::from_millis(5), Some(1 << 40)));
    let cache = 8 * MB;
    let mut c = cfg(&state, cache);
    c.upload_parallel = 1;
    let v = Volume::open(c, store.clone()).await.unwrap();
    let a = v.create("f.bin", false).unwrap();
    v.write(a.ino, 0, &expected(5, 0, (4 * MB) as usize)).await.unwrap();
    // Not uploaded yet, but readable immediately.
    assert_eq!(v.read(a.ino, 0, (4 * MB) as usize).await.unwrap(), expected(5, 0, (4 * MB) as usize));
    let t = Instant::now();
    let mut off = 4 * MB;
    while off < 96 * MB {
        v.write(a.ino, off, &expected(5, off, MB as usize)).await.unwrap();
        assert!(v.stats().dirty_bytes <= cache);
        off += MB;
    }
    let st = v.stats();
    println!("96 MiB at 32 MB/s upload through 8 MiB cache: writes took {:?}; {st:?}", t.elapsed());
    assert!(st.backpressure_waits > 0, "writer was never throttled");
    assert!(t.elapsed() >= Duration::from_secs(2), "writes outran a 32 MB/s link without waiting");
    v.sync_remote().await.unwrap();
    assert!(read_all_equals(&v, a.ino, 96 * MB, 5).await);
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_with_failing_uploads_keeps_flushed_data_and_resumes() {
    let (state, remote) = dirs("crash");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    store.fail_puts.store(true, Relaxed);
    let ino;
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        v.create("dir", true).unwrap();
        let a = v.create("dir/save.dat", false).unwrap();
        ino = a.ino;
        v.write(a.ino, 0, &expected(9, 0, (20 * MB) as usize)).await.unwrap();
        v.close(a.ino);
        v.flush().unwrap(); // local durability point
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let st = v.stats();
        assert_eq!(st.dirty_bytes, 20 * MB, "nothing may be freed without remote confirmation");
        assert!(st.upload_errors > 0);
        v.shutdown(); // "crash": no sync, uploads never succeeded
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    store.fail_puts.store(false, Relaxed);
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    let a = v.lookup("DIR/Save.dat").expect("file survives restart (case-insensitive lookup)");
    assert_eq!(a.ino, ino);
    assert_eq!(a.size, 20 * MB);
    assert!(read_all_equals(&v, a.ino, 20 * MB, 9).await, "recovered data differs");
    v.sync_remote().await.unwrap(); // re-queued uploads complete
    assert_eq!(v.stats().dirty_bytes, 0);
    assert!(store.object_count() > 0);
    v.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // And after a clean restart everything is read from the remote.
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    assert!(read_all_equals(&v, ino, 20 * MB, 9).await);
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_truncate_extend_unlink_and_garbage_collection() {
    let (state, remote) = dirs("ops");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(10 << 30)));
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    v.create("downloading", true).unwrap();
    v.create("common", true).unwrap();
    let a = v.create("downloading/pak.bin", false).unwrap();
    v.write(a.ino, 0, &expected(1, 0, (8 * MB) as usize)).await.unwrap();
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    // Full overwrite: old objects become garbage. They must survive while one of the
    // two newest remote metadata snapshots still references them (restore safety)...
    v.write(a.ino, 0, &expected(2, 0, (8 * MB) as usize)).await.unwrap();
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(v.stats().objects_deleted, 0, "garbage deleted while a remote snapshot still references it");
    // ...and are deleted once newer snapshots no longer do.
    let t = v.create("tick", false).unwrap();
    v.write(t.ino, 0, b"x").await.unwrap();
    v.close(t.ino);
    v.sync_remote().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(v.stats().objects_deleted >= 1, "garbage not collected: {:?}", v.stats());
    v.unlink("tick").unwrap();
    // Steam-style move downloading -> common is metadata only (no upload).
    let puts = store.puts.load(Relaxed);
    v.rename("downloading/pak.bin", "common/pak.bin", false).unwrap();
    assert!(v.lookup("downloading/pak.bin").is_none());
    assert_eq!(store.puts.load(Relaxed), puts);
    assert!(read_all_equals(&v, a.ino, 8 * MB, 2).await);
    // Truncate inside a remote block, then extend: the tail must read as zeros.
    v.set_size(a.ino, 3 * MB / 2).await.unwrap();
    v.set_size(a.ino, 4 * MB).await.unwrap();
    let got = v.read(a.ino, 0, (4 * MB) as usize).await.unwrap();
    assert_eq!(&got[..(3 * MB / 2) as usize], &expected(2, 0, (3 * MB / 2) as usize)[..]);
    assert!(got[(3 * MB / 2) as usize..].iter().all(|&b| b == 0), "stale bytes after truncate+extend");
    // Rename over an existing file requires replace.
    let b = v.create("common/new.bin", false).unwrap();
    v.write(b.ino, 0, b"fresh").await.unwrap();
    assert!(v.rename("common/new.bin", "common/pak.bin", false).is_err());
    v.rename("common/new.bin", "common/pak.bin", true).unwrap();
    let p = v.lookup("common/pak.bin").unwrap();
    assert_eq!(v.read(p.ino, 0, 5).await.unwrap(), b"fresh");
    // Non-empty directory removal refused, then cleanup.
    assert!(v.unlink("common").is_err());
    v.unlink("common/pak.bin").unwrap();
    v.unlink("common").unwrap();
    v.sync_remote().await.unwrap();
    assert_eq!(v.stats().logical_bytes, 0);
    let (total, free) = v.space().await;
    assert!(free > 0 && total >= free);
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_unaligned_writes_have_low_amplification() {
    // Likely Steam pattern: a file written front to back in chunks not aligned to blocks.
    let (state, remote) = dirs("seq");
    let store = Arc::new(LocalObjectStore::new(&remote, 400 << 20, Duration::from_millis(2), Some(1 << 40)));
    let cache = 16 * MB;
    let v = Volume::open(cfg(&state, cache), store.clone()).await.unwrap();
    let size = 256 * MB;
    let a = v.create("seq.bin", false).unwrap();
    v.set_size(a.ino, size).await.unwrap();
    let chunk = 700 << 10;
    let mut off = 0;
    while off < size {
        let n = chunk.min(size - off) as usize;
        v.write(a.ino, off, &expected(31, off, n)).await.unwrap();
        assert!(v.stats().dirty_bytes <= cache);
        off += n as u64;
    }
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    let st = v.stats();
    let amp = st.uploaded_bytes as f64 / size as f64;
    println!("sequential unaligned 256 MiB: amplification {amp:.3}x, rmw {}, objects {}, puts {}", st.rmw_fetches, store.object_count(), store.puts.load(Relaxed));
    assert!(amp < 1.05, "amplification {amp:.3}x");
    assert!(read_all_equals(&v, a.ino, size, 31).await);
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_files_store_only_their_bytes_and_reread_correctly() {
    let (state, remote) = dirs("small");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    v.create("cfg", true).unwrap();
    let mut total = 0u64;
    for i in 0..200u64 {
        let a = v.create(&format!("cfg/f{i}.ini"), false).unwrap();
        let n = (50 + i * 7) as usize;
        v.write(a.ino, 0, &expected(1000 + i, 0, n)).await.unwrap();
        v.close(a.ino);
        total += n as u64;
    }
    v.sync_remote().await.unwrap();
    let st = v.stats();
    println!("200 small files ({total} bytes): uploaded {} bytes in {} objects", st.uploaded_bytes, st.upload_objects);
    assert!(st.upload_objects <= 5, "small files not packed together: {} objects", st.upload_objects);
    assert!(st.uploaded_bytes <= total + 4096, "small files padded to blocks: {} bytes uploaded", st.uploaded_bytes);
    v.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    for i in 0..200u64 {
        let a = v.lookup(&format!("cfg/f{i}.ini")).unwrap();
        let n = (50 + i * 7) as usize;
        assert_eq!(v.read(a.ino, 0, n + 100).await.unwrap(), expected(1000 + i, 0, n));
    }
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extending_multi_block_write_through_tiny_cache_is_not_truncated() {
    // Review finding #1: blocks sealed mid-write were packed using the file's old size.
    let (state, remote) = dirs("extend");
    let store = Arc::new(LocalObjectStore::new(&remote, 64 << 20, Duration::from_millis(2), Some(1 << 40)));
    let v = Volume::open(cfg(&state, 4 * MB), store.clone()).await.unwrap();
    let a = v.create("grow.bin", false).unwrap(); // no preallocation
    let data = expected(55, 0, (12 * MB) as usize);
    v.write(a.ino, 0, &data).await.unwrap(); // one call, 3x the cache
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    v.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = Volume::open(cfg(&state, 4 * MB), store.clone()).await.unwrap();
    let a = v.lookup("grow.bin").unwrap();
    assert_eq!(a.size, 12 * MB);
    assert!(read_all_equals(&v, a.ino, 12 * MB, 55).await, "data lost/truncated in upload");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_rename_does_not_delete_the_target() {
    // Review finding #7.
    let (state, remote) = dirs("rename-reject");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    v.create("d", true).unwrap();
    v.create("d/sub", true).unwrap();
    let t = v.create("d/sub/keep.txt", false).unwrap();
    v.write(t.ino, 0, b"keep").await.unwrap();
    // Moving "d" over "d/sub/keep.txt" (inside itself) must fail and leave keep.txt intact.
    assert!(v.rename("d", "d/sub/keep.txt", true).is_err());
    let k = v.lookup("d/sub/keep.txt").expect("target deleted by a failed rename");
    assert_eq!(v.read(k.ino, 0, 4).await.unwrap(), b"keep");
    assert!(v.rename("", "x", false).is_err(), "root must not be movable");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volume_is_rebuilt_from_remote_snapshot_after_losing_local_state() {
    let (state, remote) = dirs("restore");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        v.create("steamapps", true).unwrap();
        v.create("steamapps/common", true).unwrap();
        let a = v.create("steamapps/common/game.pak", false).unwrap();
        v.write(a.ino, 0, &expected(71, 0, (10 * MB) as usize)).await.unwrap();
        let b = v.create("steamapps/appmanifest_1.acf", false).unwrap();
        v.write(b.ino, 0, b"\"AppState\" {}").await.unwrap();
        v.close(a.ino);
        v.close(b.ino);
        v.sync_remote().await.unwrap(); // uploads data AND a metadata snapshot
        assert!(v.stats().meta_snapshots >= 1);
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    std::fs::remove_dir_all(&state).unwrap(); // the local disk is gone
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    let a = v.lookup("steamapps/common/game.pak").expect("tree restored from the remote snapshot");
    assert_eq!(a.size, 10 * MB);
    assert!(read_all_equals(&v, a.ino, 10 * MB, 71).await, "restored data differs");
    let b = v.lookup("steamapps/appmanifest_1.acf").unwrap();
    assert_eq!(v.read(b.ino, 0, 100).await.unwrap(), b"\"AppState\" {}");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freshly_uploaded_data_is_reread_without_remote_requests() {
    // Steam re-reads what it just wrote to verify it: that must not hit the remote.
    let (state, remote) = dirs("seed");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    let a = v.create("game.pak", false).unwrap();
    v.write(a.ino, 0, &expected(88, 0, (24 * MB) as usize)).await.unwrap();
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    assert_eq!(v.stats().dirty_bytes, 0, "data is remote now, local slots freed");
    let gets = store.gets.load(Relaxed);
    assert!(read_all_equals(&v, a.ino, 24 * MB, 88).await);
    assert_eq!(store.gets.load(Relaxed), gets, "re-read of just-uploaded data went to the remote");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_filled_preallocation_is_not_uploaded() {
    let (state, remote) = dirs("zeros");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let v = Volume::open(cfg(&state, 32 * MB), store.clone()).await.unwrap();
    let a = v.create("pak.bin", false).unwrap();
    let zeros = vec![0u8; MB as usize];
    for i in 0..64u64 {
        v.write(a.ino, i * MB, &zeros).await.unwrap(); // "allocating disk space" written as zeros
    }
    let data = expected(5, 0, (3 * MB) as usize);
    v.write(a.ino, 10 * MB, &data).await.unwrap(); // real content arrives later
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    let st = v.stats();
    println!("64 MiB zeros + 3 MiB data: uploaded {} bytes, {} zero blocks elided", st.uploaded_bytes, st.zero_blocks_elided);
    assert!(st.uploaded_bytes <= 4 * MB, "zeros were uploaded: {} bytes", st.uploaded_bytes);
    assert!(st.zero_blocks_elided >= 50);
    let got = v.read(a.ino, 0, (64 * MB) as usize).await.unwrap();
    assert!(got[..(10 * MB) as usize].iter().all(|&b| b == 0));
    assert_eq!(&got[(10 * MB) as usize..(13 * MB) as usize], &data[..]);
    assert!(got[(13 * MB) as usize..].iter().all(|&b| b == 0));
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_pause_when_local_disk_is_below_the_reserve() {
    let (state, remote) = dirs("reserve");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let mut c = cfg(&state, 32 * MB);
    c.min_local_free = u64::MAX / 2; // impossible to satisfy: simulates a nearly full SSD
    let v = Volume::open(c, store.clone()).await.unwrap();
    let a = v.create("f.bin", false).unwrap();
    let r = tokio::time::timeout(Duration::from_millis(1500), v.write(a.ino, 0, &[1u8; 4096])).await;
    assert!(r.is_err(), "write went through although the local disk is below the reserve");
    assert!(v.stats().reserve_waits > 0);
    assert_eq!(v.stats().slab_file_bytes, 0, "nothing may be written locally");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steam_style_moves_survive_reopen() {
    // Real incident: files written to downloading/<appid> and moved into common/<game>
    // vanished from the tree after a remount.
    let (state, remote) = dirs("steam-moves");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let names = ["data.win", "Risk of Rain.exe", "steam_api.dll", "musicStage1.ogg"];
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        for d in ["SteamLibrary", "SteamLibrary/steamapps", "SteamLibrary/steamapps/common", "SteamLibrary/steamapps/downloading", "SteamLibrary/steamapps/common/Risk of Rain", "SteamLibrary/steamapps/downloading/248820"] {
            v.create(d, true).unwrap();
        }
        let s = v.create("SteamLibrary/steamapps/common/Risk of Rain/Save.ini", false).unwrap();
        v.write(s.ino, 0, b"save").await.unwrap();
        for (k, n) in names.iter().enumerate() {
            let a = v.create(&format!("SteamLibrary/steamapps/downloading/248820/{n}"), false).unwrap();
            v.write(a.ino, 0, &expected(300 + k as u64, 0, (3 * MB) as usize)).await.unwrap();
            v.close(a.ino);
        }
        for n in names {
            v.rename(&format!("SteamLibrary/steamapps/downloading/248820/{n}"), &format!("SteamLibrary/steamapps/common/Risk of Rain/{n}"), true).unwrap();
        }
        v.unlink("SteamLibrary/steamapps/downloading/248820").unwrap();
        // manifest replaced via temp + rename-over
        let m = v.create("SteamLibrary/steamapps/appmanifest_248820.acf", false).unwrap();
        v.write(m.ino, 0, b"old").await.unwrap();
        let t = v.create("SteamLibrary/steamapps/appmanifest_248820.acf.tmp", false).unwrap();
        v.write(t.ino, 0, b"new manifest").await.unwrap();
        v.rename("SteamLibrary/steamapps/appmanifest_248820.acf.tmp", "SteamLibrary/steamapps/appmanifest_248820.acf", true).unwrap();
        v.sync_remote().await.unwrap();
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    for round in 0..2 {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        for (k, n) in names.iter().enumerate() {
            let a = v.lookup(&format!("SteamLibrary/steamapps/common/Risk of Rain/{n}")).unwrap_or_else(|| panic!("round {round}: {n} vanished after reopen"));
            assert!(read_all_equals(&v, a.ino, 3 * MB, 300 + k as u64).await, "round {round}: {n} content");
        }
        let m = v.lookup("SteamLibrary/steamapps/appmanifest_248820.acf").expect("manifest vanished");
        assert_eq!(v.read(m.ino, 0, 100).await.unwrap(), b"new manifest");
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &dst.join(e.file_name()));
        } else {
            std::fs::copy(&p, dst.join(e.file_name())).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torn_rename_over_is_all_or_nothing() {
    let (state, remote) = dirs("torn-rename");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        let a = v.create("game.acf", false).unwrap();
        v.write(a.ino, 0, b"old").await.unwrap();
        let t = v.create("game.acf.tmp", false).unwrap();
        v.write(t.ino, 0, b"new").await.unwrap();
        v.flush().unwrap();
        v.rename("game.acf.tmp", "game.acf", true).unwrap();
        v.flush().unwrap();
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Crash mid-write of the last journal line (the rename-over batch).
    let j = state.join("volume.jsonl");
    let text = std::fs::read_to_string(&j).unwrap();
    let last = text.trim_end().rfind('\n').unwrap();
    assert!(text[last..].contains("Batch"), "rename-over must be a single batch line");
    std::fs::write(&j, &text[..last + 1 + (text.len() - last) / 2]).unwrap();
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    let a = v.lookup("game.acf").expect("target must survive a torn rename");
    let t = v.lookup("game.acf.tmp").expect("source must survive a torn rename");
    assert_eq!(v.read(a.ino, 0, 3).await.unwrap(), b"old");
    assert_eq!(v.read(t.ino, 0, 3).await.unwrap(), b"new");
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_is_durable_within_a_second_without_flush() {
    let (state, remote) = dirs("group-commit");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
    v.create("downloading", true).unwrap();
    v.create("common", true).unwrap();
    let a = v.create("downloading/data.pak", false).unwrap();
    v.write(a.ino, 0, &expected(9, 0, (2 * MB) as usize)).await.unwrap();
    v.close(a.ino);
    v.sync_remote().await.unwrap();
    v.rename("downloading/data.pak", "common/data.pak", false).unwrap(); // no flush afterwards
    tokio::time::sleep(Duration::from_millis(700)).await;
    // "kill": take the on-disk state as it is now, while the volume is still running.
    let crashed = state.parent().unwrap().join("state-crashed");
    copy_dir(&state, &crashed);
    let v2 = Volume::open(cfg(&crashed, 64 * MB), store.clone()).await.unwrap();
    let m = v2.lookup("common/data.pak").expect("rename lost after a kill despite group commit");
    assert!(read_all_equals(&v2, m.ino, 2 * MB, 9).await);
    v2.shutdown();
    v.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_local_metadata_never_deletes_remote_data_and_is_recoverable() {
    // Reproduces the 2026-10-30 incident: local journal lost file entries, GC deleted packs.
    let (state, remote) = dirs("incident");
    let store = Arc::new(LocalObjectStore::new(&remote, 0, Duration::ZERO, Some(1 << 40)));
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        v.create("common", true).unwrap();
        let a = v.create("common/data.win", false).unwrap();
        v.write(a.ino, 0, &expected(42, 0, (12 * MB) as usize)).await.unwrap();
        v.close(a.ino);
        v.sync_remote().await.unwrap(); // data + metadata snapshot in the remote
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let objects_before = store.object_count();
    // Lose the file's metadata locally: keep only header + object records.
    let j = state.join("volume.jsonl");
    let kept: Vec<String> = std::fs::read_to_string(&j).unwrap().lines().filter(|l| l.contains("\"Hdr\"") || l.contains("\"Obj\"")).map(String::from).collect();
    std::fs::write(&j, kept.join("\n") + "\n").unwrap();
    {
        let v = Volume::open(cfg(&state, 64 * MB), store.clone()).await.unwrap();
        assert!(v.lookup("common/data.win").is_none());
        let st = v.stats();
        assert!(st.orphans_at_open > 0 && st.missing_vs_remote > 0, "{st:?}");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(store.object_count(), objects_before, "remote data deleted after a local metadata loss");
        v.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let mut c = cfg(&state, 64 * MB);
    c.restore_from_remote = true;
    let v = Volume::open(c, store.clone()).await.unwrap();
    let a = v.lookup("common/data.win").expect("restore from remote snapshot");
    assert!(read_all_equals(&v, a.ino, 12 * MB, 42).await);
    v.shutdown();
}
