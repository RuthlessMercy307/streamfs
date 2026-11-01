//! Verifies a mounted `sfs-mount --backend sim` drive byte-for-byte against the
//! deterministic content function: random reads, memory-mapped views, and
//! concurrent readers through the normal Win32 file API.
//! uso: verify-mount --root S:\

use sfs_sim::expected;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn check(name: &str, ok: bool, detail: String) -> bool {
    println!("[{}] {name}: {detail}", if ok { "OK  " } else { "FAIL" });
    ok
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.iter().position(|a| a == "--root").and_then(|i| args.get(i + 1)).expect("--root"));
    let mut all_ok = true;
    let mb = 1u64 << 20;
    if let Some(phase) = args.iter().position(|a| a == "--cow").and_then(|i| args.get(i + 1)) {
        let ok = cow_phase(&root, phase);
        println!("\nRESULTADO COW {phase}: {}", if ok { "TODO OK" } else { "HAY FALLOS" });
        std::process::exit(if ok { 0 } else { 1 });
    }

    // 1. Metadata: logical size must be reported without being materialised.
    let md = std::fs::metadata(root.join("game.pak")).expect("stat game.pak");
    all_ok &= check("size game.pak", md.len() == 20 << 30, format!("{} bytes", md.len()));
    let names: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    all_ok &= check("listing /", names.contains(&"game.pak".into()) && names.contains(&"dir".into()), format!("{names:?}"));

    // 2. Random reads on a 20 GB file via seek+read.
    let mut f = std::fs::File::open(root.join("game.pak")).expect("open game.pak");
    let mut s = 0xDEADBEEFu64;
    let mut lats = vec![];
    let mut bad = 0;
    for _ in 0..40 {
        let off = xorshift(&mut s) % ((20 << 30) - 4 * mb);
        let len = (xorshift(&mut s) % (3 * mb) + 1) as usize;
        let mut buf = vec![0u8; len];
        let t = Instant::now();
        f.seek(SeekFrom::Start(off)).unwrap();
        f.read_exact(&mut buf).unwrap();
        lats.push(t.elapsed().as_secs_f64() * 1e3);
        if buf != expected(7, off, len) {
            bad += 1;
        }
    }
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    all_ok &= check("random reads 20GB", bad == 0, format!("{} mismatches; p50 {:.1} ms p95 {:.1} ms max {:.1} ms", bad, lats[lats.len() / 2], lats[(lats.len() as f64 * 0.95) as usize - 1], lats[lats.len() - 1]));

    // 3. Memory-mapped file: whole small file + a 64 MB view in the middle of the 20 GB file.
    let sf = std::fs::File::open(root.join("small.bin")).unwrap();
    let m = unsafe { memmap2::Mmap::map(&sf) };
    match m {
        Ok(m) => all_ok &= check("mmap small.bin (8MB)", m[..] == expected(11, 0, (8 * mb) as usize)[..], format!("{} bytes mapped", m.len())),
        Err(e) => all_ok &= check("mmap small.bin", false, format!("map failed: {e}")),
    }
    let off = 5 * (1u64 << 30) + 12 * mb; // multiple of the 64 KiB allocation granularity
    let view = unsafe { memmap2::MmapOptions::new().offset(off).len((16 * mb) as usize).map(&f) };
    match view {
        Ok(v) => all_ok &= check("mmap view 16MB @5GB of game.pak", v[..] == expected(7, off, (16 * mb) as usize)[..], format!("{} bytes", v.len())),
        Err(e) => all_ok &= check("mmap view of game.pak", false, format!("map failed: {e}")),
    }

    // 4. Concurrent readers (4 threads x 20 reads) on different handles.
    let root2 = root.clone();
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let r = root2.clone();
            std::thread::spawn(move || {
                let mut f = std::fs::File::open(r.join("dir").join("data.bin")).unwrap();
                let mut s = 0x1234 + t * 77;
                let mut bad = 0;
                for _ in 0..20 {
                    let off = xorshift(&mut s) % (63 * mb);
                    let mut buf = vec![0u8; 512 << 10];
                    f.seek(SeekFrom::Start(off)).unwrap();
                    f.read_exact(&mut buf).unwrap();
                    if buf != expected(13, off, 512 << 10) {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    let bad: i32 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    all_ok &= check("4 concurrent readers dir/data.bin", bad == 0, format!("{bad} mismatches"));

    // 5. Read past EOF and writes must fail cleanly.
    let mut f2 = std::fs::File::open(root.join("small.bin")).unwrap();
    f2.seek(SeekFrom::Start(8 * mb - 10)).unwrap();
    let mut tail = vec![0u8; 100];
    let n = f2.read(&mut tail).unwrap();
    all_ok &= check("EOF clamp", n == 10 && tail[..10] == expected(11, 8 * mb - 10, 10)[..], format!("read {n} bytes at EOF-10"));
    if args.iter().any(|a| a == "--expect-readonly") {
        let w = std::fs::OpenOptions::new().write(true).open(root.join("small.bin"));
        all_ok &= check("write rejected (read-only volume)", w.is_err(), format!("{:?}", w.as_ref().err().map(|e| e.to_string())));
    }

    println!("\nRESULTADO: {}", if all_ok { "TODO OK" } else { "HAY FALLOS" });
    std::process::exit(if all_ok { 0 } else { 1 });
}

/// Copy-on-write checks. "write" mutates; "check" (after remount, same overlay)
/// verifies persistence; "base" (fresh overlay) verifies the remote base is untouched.
fn cow_phase(root: &std::path::Path, phase: &str) -> bool {
    use std::io::Write;
    use std::os::windows::fs::FileExt;
    let mb = 1u64 << 20;
    let mut ok = true;
    let rd = |p: &str| std::fs::read(root.join(p));
    let save3: &[u8] = b"[run]\nlevel=3\n";
    let save4: &[u8] = b"[run]\nlevel=4\n";
    match phase {
        "write" => {
            std::fs::create_dir(root.join("saves")).unwrap();
            std::fs::write(root.join("saves").join("Save.ini"), save3).unwrap();
            ok &= check("create dir+file, read back", rd("saves/Save.ini").map(|b| b == save3).unwrap_or(false), String::new());

            // GameMaker-style save: keep a backup by rename, then write a new file.
            std::fs::rename(root.join("saves/Save.ini"), root.join("saves/Save_backup.ini")).unwrap();
            std::fs::write(root.join("saves/Save.ini"), save4).unwrap();
            ok &= check(
                "rename to backup + rewrite",
                rd("saves/Save_backup.ini").map(|b| b == save3).unwrap_or(false) && rd("saves/Save.ini").map(|b| b == save4).unwrap_or(false),
                String::new(),
            );

            // Rename over an existing file (MoveFileEx REPLACE_EXISTING).
            std::fs::write(root.join("saves/new.tmp"), b"replacement").unwrap();
            std::fs::rename(root.join("saves/new.tmp"), root.join("saves/Save_backup.ini")).unwrap();
            ok &= check("rename replacing existing", rd("saves/Save_backup.ini").map(|b| b == b"replacement").unwrap_or(false), String::new());

            // Random write into a REMOTE file => lazy copy-up, rest of content preserved.
            {
                let f = std::fs::OpenOptions::new().read(true).write(true).open(root.join("small.bin")).unwrap();
                f.seek_write(b"PATCH", 1000).unwrap();
                f.sync_all().unwrap();
            }
            let mut want = expected(11, 0, (8 * mb) as usize);
            want[1000..1005].copy_from_slice(b"PATCH");
            ok &= check("random write into remote file (copy-up)", rd("small.bin").map(|b| b == want).unwrap_or(false), String::new());

            // Truncate / extend.
            {
                let f = std::fs::File::create(root.join("big.tmp")).unwrap();
                f.set_len(10 * mb).unwrap();
                f.seek_write(b"END", 9 * mb).unwrap();
                f.set_len(5 * mb).unwrap();
            }
            let len = std::fs::metadata(root.join("big.tmp")).map(|m| m.len()).unwrap_or(0);
            ok &= check("extend to 10MB then truncate to 5MB", len == 5 * mb, format!("{len} bytes"));

            // Rename a remote file (copy-up + whiteout of the old name).
            std::fs::rename(root.join("dir/data.bin"), root.join("dir/data2.bin")).unwrap();
            ok &= check(
                "rename remote file",
                rd("dir/data2.bin").map(|b| b == expected(13, 0, (64 * mb) as usize)).unwrap_or(false) && !root.join("dir/data.bin").exists(),
                String::new(),
            );

            // Delete a huge remote file: must NOT download it.
            std::fs::remove_file(root.join("game.pak")).unwrap();
            ok &= check("delete remote 20GB file (whiteout, no download)", !root.join("game.pak").exists(), String::new());

            // Directory create/remove, non-empty removal refused.
            std::fs::create_dir(root.join("tmpdir")).unwrap();
            std::fs::write(root.join("tmpdir/x"), b"x").unwrap();
            let refused = std::fs::remove_dir(root.join("tmpdir")).is_err();
            std::fs::remove_file(root.join("tmpdir/x")).unwrap();
            std::fs::remove_dir(root.join("tmpdir")).unwrap();
            ok &= check("mkdir/rmdir (non-empty refused)", refused && !root.join("tmpdir").exists(), String::new());

            // Two handles on the same file.
            {
                let mut a = std::fs::OpenOptions::new().write(true).open(root.join("saves/Save.ini")).unwrap();
                let b = std::fs::File::open(root.join("saves/Save.ini")).unwrap();
                a.write_all(b"[RUN]").unwrap();
                a.flush().unwrap();
                let mut s = String::new();
                use std::io::Read;
                (&b).read_to_string(&mut s).unwrap();
                ok &= check("two handles see each other's writes", s.starts_with("[RUN]"), s.replace('\n', " "));
            }
            let names: Vec<String> = std::fs::read_dir(root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            ok &= check(
                "listing merged, no internal files",
                !names.iter().any(|n| n.starts_with(".sfs-")) && names.contains(&"saves".to_string()) && !names.contains(&"game.pak".to_string()),
                format!("{names:?}"),
            );
        }
        "check" => {
            ok &= check("Save.ini persisted", rd("saves/Save.ini").map(|b| b.starts_with(b"[RUN]")).unwrap_or(false), String::new());
            ok &= check("Save_backup.ini persisted", rd("saves/Save_backup.ini").map(|b| b == b"replacement").unwrap_or(false), String::new());
            let mut want = expected(11, 0, (8 * mb) as usize);
            want[1000..1005].copy_from_slice(b"PATCH");
            ok &= check("patched remote file persisted", rd("small.bin").map(|b| b == want).unwrap_or(false), String::new());
            ok &= check(
                "renamed file persisted",
                rd("dir/data2.bin").map(|b| b.len() == (64 * mb) as usize).unwrap_or(false) && !root.join("dir/data.bin").exists(),
                String::new(),
            );
            ok &= check("deleted remote file stays deleted", !root.join("game.pak").exists(), String::new());
            ok &= check("truncated file size persisted", std::fs::metadata(root.join("big.tmp")).map(|m| m.len() == 5 * mb).unwrap_or(false), String::new());
        }
        "base" => {
            ok &= check("remote small.bin untouched", rd("small.bin").map(|b| b == expected(11, 0, (8 * mb) as usize)).unwrap_or(false), String::new());
            ok &= check("remote game.pak still there", std::fs::metadata(root.join("game.pak")).map(|m| m.len() == 20 << 30).unwrap_or(false), String::new());
            ok &= check("remote dir/data.bin present, data2 absent", root.join("dir/data.bin").exists() && !root.join("dir/data2.bin").exists(), String::new());
            ok &= check("no saves in a fresh overlay", !root.join("saves").exists(), String::new());
        }
        _ => panic!("phase write|check|base"),
    }
    ok
}
