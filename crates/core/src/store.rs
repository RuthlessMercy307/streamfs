//! Persistent block store.
//!
//! Layout: one slab file of fixed-size slots (`slab.bin`, grows on demand, so
//! physical use is exactly high-water-slots * block_size and never depends on
//! sparse-file support) plus a JSONL journal (`index.jsonl`) mapping
//! (file, block) -> slot with a CRC32 of the data.
//!
//! Crash safety without fsync-per-block: data is written to the slab *before*
//! the journal record. After a crash a record may point at missing/garbage
//! data; every read verifies the CRC and treats a mismatch as a cache miss
//! (and drops the mapping). A torn final journal line is ignored.
//!
//! Eviction is segmented LRU over unpinned slots. Blocks fetched on a guess
//! (read-ahead, learned prefetch) enter on *probation*: their ticks sort below every
//! block an application actually read, so unused guesses are always evicted first and
//! a burst of prefetching cannot flush the working set. The first real read of a
//! probation block promotes it. Slots held by an in-flight reader and blocks of
//! "pinned" files are never evicted; if nothing is evictable the insert fails with
//! `CacheFull` instead of exceeding the cap or corrupting data.

use crate::error::{Error, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

const NONE: u32 = u32::MAX;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
enum Rec {
    Hdr { bs: u32 },
    File { idx: u32, key: String, version: String, size: u64 },
    Block { f: u32, b: u32, s: u32, crc: u32, len: u32 },
    Drop { f: u32 },
}

#[derive(Default, Clone)]
struct Slot {
    owner: Option<(u32, u32)>,
    pins: u32,
    /// Unmapped while pinned; freed when the last pin is released.
    zombie: bool,
    tick: u64,
    crc: u32,
    len: u32,
    /// Fetched on a guess and not read since.
    prob: bool,
}

struct FileEntry {
    key: String,
    version: String,
    size: u64,
    pinned: bool,
    blocks: Vec<u32>,
    resident: u32,
}

struct Inner {
    files: HashMap<u32, FileEntry>,
    by_key: HashMap<String, u32>,
    next_file: u32,
    slots: Vec<Slot>,
    free: Vec<u32>,
    lru: BTreeMap<u64, u32>,
    tick: u64,
    /// Probation ticks count up from 1 and stay below PROMO_BASE, where demand ticks start.
    ptick: u64,
    prob_blocks: u64,
    spec_used_bytes: u64,
    spec_evicted_unused_bytes: u64,
    journal: Option<BufWriter<File>>,
    bs: usize,
}

const PROMO_BASE: u64 = 1 << 48;

#[derive(Debug, Clone, Copy)]
pub struct Pinned {
    pub slot: u32,
    pub crc: u32,
    pub len: u32,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreStats {
    pub resident_blocks: u64,
    pub resident_bytes: u64,
    /// Bytes physically allocated in the slab file (high-water mark).
    pub slab_bytes: u64,
    pub max_bytes: u64,
    /// Guessed blocks not read yet.
    pub probation_bytes: u64,
    /// Guessed blocks that an application later read (prefetch that paid off).
    pub spec_used_bytes: u64,
    /// Guessed blocks evicted without ever being read (prefetch waste).
    pub spec_evicted_unused_bytes: u64,
}

pub struct BlockStore {
    slab: File,
    bs: usize,
    max_slots: u32,
    dir: PathBuf,
    inner: Mutex<Inner>,
}

fn nblocks(size: u64, bs: usize) -> usize {
    ((size + bs as u64 - 1) / bs as u64) as usize
}

#[cfg(windows)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}
#[cfg(windows)]
fn pwrite(f: &File, buf: &[u8], off: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(f, buf, off)
}
#[cfg(unix)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(unix)]
fn pwrite(f: &File, buf: &[u8], off: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(f, buf, off)
}

fn read_full(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<bool> {
    while !buf.is_empty() {
        let n = pread(f, buf, off)?;
        if n == 0 {
            return Ok(false); // slab shorter than journal claims (crash)
        }
        off += n as u64;
        buf = &mut buf[n..];
    }
    Ok(true)
}

fn write_full(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let n = pwrite(f, buf, off)?;
        off += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

impl Inner {
    fn journal(&mut self, rec: &Rec) {
        if let Some(j) = self.journal.as_mut() {
            // Best effort: a lost record only costs a future cache miss.
            let _ = serde_json::to_writer(&mut *j, rec);
            let _ = j.write_all(b"\n");
            let _ = j.flush();
        }
    }

    fn clear_prob(&mut self, s: u32) {
        let slot = &mut self.slots[s as usize];
        if slot.prob {
            slot.prob = false;
            self.prob_blocks -= 1;
        }
    }

    fn release_slot(&mut self, s: u32) {
        self.clear_prob(s);
        let slot = &mut self.slots[s as usize];
        slot.owner = None;
        self.lru.remove(&slot.tick);
        if slot.pins > 0 {
            slot.zombie = true;
        } else {
            self.free.push(s);
        }
    }

    fn drop_file(&mut self, idx: u32) {
        if let Some(e) = self.files.remove(&idx) {
            if self.by_key.get(&e.key) == Some(&idx) {
                self.by_key.remove(&e.key);
            }
            for &s in e.blocks.iter().filter(|&&s| s != NONE) {
                self.release_slot(s);
            }
        }
    }

    fn add_file(&mut self, idx: u32, key: String, version: String, size: u64) {
        if let Some(&old) = self.by_key.get(&key) {
            if old != idx {
                self.drop_file(old);
            }
        }
        self.by_key.insert(key.clone(), idx);
        self.next_file = self.next_file.max(idx + 1);
        self.files.insert(
            idx,
            FileEntry { key, version, size, pinned: false, blocks: vec![NONE; nblocks(size, self.bs)], resident: 0 },
        );
    }

    fn set_block(&mut self, f: u32, b: u32, s: u32, crc: u32, len: u32, prob: bool) {
        let tick = if prob {
            self.ptick += 1;
            self.prob_blocks += 1;
            self.ptick
        } else {
            self.tick += 1;
            self.tick
        };
        let slot = &mut self.slots[s as usize];
        slot.owner = Some((f, b));
        slot.tick = tick;
        slot.crc = crc;
        slot.len = len;
        slot.prob = prob;
        self.lru.insert(tick, s);
        let e = self.files.get_mut(&f).unwrap();
        e.blocks[b as usize] = s;
        e.resident += 1;
    }

    fn apply(&mut self, rec: Rec) {
        match rec {
            Rec::Hdr { .. } => {}
            Rec::File { idx, key, version, size } => self.add_file(idx, key, version, size),
            Rec::Drop { f } => self.drop_file(f),
            Rec::Block { f, b, s, crc, len } => {
                let ok = self.files.get(&f).map_or(false, |e| (b as usize) < e.blocks.len());
                if !ok {
                    return;
                }
                while self.slots.len() <= s as usize {
                    self.slots.push(Slot::default());
                }
                // Slot reassigned: previous owner silently loses it.
                if let Some((of, ob)) = self.slots[s as usize].owner {
                    self.clear_prob(s);
                    let t = self.slots[s as usize].tick;
                    self.lru.remove(&t);
                    if let Some(e) = self.files.get_mut(&of) {
                        if e.blocks[ob as usize] == s {
                            e.blocks[ob as usize] = NONE;
                            e.resident -= 1;
                        }
                    }
                }
                // Block previously mapped elsewhere: free that slot's ownership.
                let prev = self.files[&f].blocks[b as usize];
                if prev != NONE {
                    self.clear_prob(prev);
                    let t = self.slots[prev as usize].tick;
                    self.lru.remove(&t);
                    self.slots[prev as usize].owner = None;
                    self.files.get_mut(&f).unwrap().resident -= 1;
                }
                self.set_block(f, b, s, crc, len, false);
            }
        }
    }

    fn snapshot(&self) -> Vec<Rec> {
        let mut out = vec![Rec::Hdr { bs: self.bs as u32 }];
        for (&idx, e) in &self.files {
            out.push(Rec::File { idx, key: e.key.clone(), version: e.version.clone(), size: e.size });
        }
        let mut blocks: Vec<(u64, Rec)> = Vec::new();
        for (&idx, e) in &self.files {
            for (b, &s) in e.blocks.iter().enumerate() {
                if s != NONE {
                    let sl = &self.slots[s as usize];
                    blocks.push((sl.tick, Rec::Block { f: idx, b: b as u32, s, crc: sl.crc, len: sl.len }));
                }
            }
        }
        blocks.sort_by_key(|(t, _)| *t); // preserve LRU order across restarts
        out.extend(blocks.into_iter().map(|(_, r)| r));
        out
    }

    /// Evict the least recently used evictable slot. Returns it, unowned.
    fn evict_one(&mut self) -> Option<u32> {
        let mut victim = None;
        for (_, &s) in self.lru.iter() {
            let sl = &self.slots[s as usize];
            if sl.pins > 0 {
                continue;
            }
            let (f, _) = sl.owner.expect("lru slot has owner");
            if self.files.get(&f).map_or(false, |e| e.pinned) {
                continue;
            }
            victim = Some(s);
            break;
        }
        let s = victim?;
        if self.slots[s as usize].prob {
            self.spec_evicted_unused_bytes += self.slots[s as usize].len as u64;
            self.clear_prob(s);
        }
        let (f, b) = self.slots[s as usize].owner.unwrap();
        let t = self.slots[s as usize].tick;
        self.lru.remove(&t);
        self.slots[s as usize].owner = None;
        if let Some(e) = self.files.get_mut(&f) {
            e.blocks[b as usize] = NONE;
            e.resident -= 1;
        }
        Some(s)
    }
}

impl BlockStore {
    pub fn open(dir: &Path, block_size: usize, cache_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let jpath = dir.join("index.jsonl");
        let mut inner = Inner {
            files: HashMap::new(),
            by_key: HashMap::new(),
            next_file: 0,
            slots: Vec::new(),
            free: Vec::new(),
            lru: BTreeMap::new(),
            tick: PROMO_BASE,
            ptick: 0,
            prob_blocks: 0,
            spec_used_bytes: 0,
            spec_evicted_unused_bytes: 0,
            journal: None,
            bs: block_size,
        };
        let max_slots = (cache_bytes / block_size as u64).min(u32::MAX as u64 - 1) as u32;

        let mut usable = false;
        if let Ok(f) = File::open(&jpath) {
            let mut first = true;
            for line in BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                let Ok(rec) = serde_json::from_str::<Rec>(&line) else { continue }; // torn/garbled line
                if first {
                    first = false;
                    match rec {
                        Rec::Hdr { bs } if bs as usize == block_size => {
                            usable = true;
                            continue;
                        }
                        _ => break, // different block size: old cache is meaningless
                    }
                }
                inner.apply(rec);
            }
        }
        if !usable {
            inner = Inner { files: HashMap::new(), by_key: HashMap::new(), next_file: 0, slots: Vec::new(), free: Vec::new(), lru: BTreeMap::new(), tick: PROMO_BASE, ptick: 0, prob_blocks: 0, spec_used_bytes: 0, spec_evicted_unused_bytes: 0, journal: None, bs: block_size };
        }
        // Cache shrunk across restarts: forget slots beyond the new cap.
        if inner.slots.len() > max_slots as usize {
            for s in max_slots as usize..inner.slots.len() {
                if let Some((f, b)) = inner.slots[s].owner {
                    if let Some(e) = inner.files.get_mut(&f) {
                        e.blocks[b as usize] = NONE;
                        e.resident -= 1;
                    }
                }
            }
            inner.slots.truncate(max_slots as usize);
            inner.lru.retain(|_, &mut s| (s as usize) < max_slots as usize);
        }
        inner.free = (0..inner.slots.len() as u32).filter(|&s| inner.slots[s as usize].owner.is_none()).collect();

        // Compact journal atomically.
        let tmp = dir.join("index.jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for r in inner.snapshot() {
                serde_json::to_writer(&mut w, &r).map_err(|e| Error::Io(e.to_string()))?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, &jpath)?;
        inner.journal = Some(BufWriter::new(OpenOptions::new().append(true).open(&jpath)?));

        let slab = OpenOptions::new().read(true).write(true).create(true).open(dir.join("slab.bin"))?;
        Ok(BlockStore { slab, bs: block_size, max_slots, dir: dir.to_path_buf(), inner: Mutex::new(inner) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Register a remote file version. A different version/size for the same
    /// key discards every cached block of the old one.
    pub fn open_file(&self, key: &str, version: &str, size: u64) -> u32 {
        let mut g = self.inner.lock();
        if let Some(&idx) = g.by_key.get(key) {
            let e = &g.files[&idx];
            if e.version == version && e.size == size {
                return idx;
            }
            g.drop_file(idx);
            g.journal(&Rec::Drop { f: idx });
        }
        let idx = g.next_file;
        g.add_file(idx, key.to_string(), version.to_string(), size);
        g.journal(&Rec::File { idx, key: key.to_string(), version: version.to_string(), size });
        idx
    }

    pub fn set_pinned(&self, file: u32, pinned: bool) {
        if let Some(e) = self.inner.lock().files.get_mut(&file) {
            e.pinned = pinned;
        }
    }

    pub fn contains(&self, f: u32, b: u32) -> bool {
        self.inner.lock().files.get(&f).map_or(false, |e| e.blocks.get(b as usize).map_or(false, |&s| s != NONE))
    }

    pub fn pin(&self, f: u32, b: u32) -> Option<Pinned> {
        let mut g = self.inner.lock();
        let s = *g.files.get(&f)?.blocks.get(b as usize)?;
        if s == NONE {
            return None;
        }
        if g.slots[s as usize].prob {
            g.spec_used_bytes += g.slots[s as usize].len as u64;
            g.clear_prob(s);
        }
        g.tick += 1;
        let tick = g.tick;
        let old = g.slots[s as usize].tick;
        g.lru.remove(&old);
        g.lru.insert(tick, s);
        let sl = &mut g.slots[s as usize];
        sl.tick = tick;
        sl.pins += 1;
        Some(Pinned { slot: s, crc: sl.crc, len: sl.len })
    }

    /// Read a pinned slot and verify its CRC. `false` => data missing/corrupt.
    pub fn read_pinned(&self, p: &Pinned, out: &mut [u8]) -> bool {
        debug_assert_eq!(out.len(), p.len as usize);
        match read_full(&self.slab, out, p.slot as u64 * self.bs as u64) {
            Ok(true) => crc32fast::hash(out) == p.crc,
            _ => false,
        }
    }

    pub fn unpin(&self, p: Pinned, bad: bool) {
        let mut g = self.inner.lock();
        if bad {
            if let Some((f, b)) = g.slots[p.slot as usize].owner {
                if let Some(e) = g.files.get_mut(&f) {
                    e.blocks[b as usize] = NONE;
                    e.resident -= 1;
                }
                g.release_slot(p.slot); // pins>0 here => zombie, freed below
            }
        }
        let sl = &mut g.slots[p.slot as usize];
        sl.pins -= 1;
        if sl.pins == 0 && sl.zombie {
            sl.zombie = false;
            g.free.push(p.slot);
        }
    }

    /// Store a verified block. No-op if already present or the file version
    /// was replaced meanwhile. Errors with `CacheFull` rather than overshoot.
    pub fn insert(&self, f: u32, b: u32, data: &[u8]) -> Result<()> {
        self.insert_class(f, b, data, false)
    }

    /// `probation`: the block was fetched on a guess (see module docs).
    pub fn insert_class(&self, f: u32, b: u32, data: &[u8], probation: bool) -> Result<()> {
        let slot = {
            let mut g = self.inner.lock();
            match g.files.get(&f) {
                None => return Ok(()),
                Some(e) if e.blocks[b as usize] != NONE => return Ok(()),
                _ => {}
            }
            let s = if let Some(s) = g.free.pop() {
                s
            } else if (g.slots.len() as u32) < self.max_slots {
                g.slots.push(Slot::default());
                g.slots.len() as u32 - 1
            } else if let Some(s) = g.evict_one() {
                s
            } else {
                return Err(Error::CacheFull);
            };
            let sl = &mut g.slots[s as usize];
            sl.pins = 1; // reserved while we write
            sl.zombie = false;
            sl.owner = None;
            s
        };
        let wr = write_full(&self.slab, data, slot as u64 * self.bs as u64);
        let crc = crc32fast::hash(data);
        let mut g = self.inner.lock();
        g.slots[slot as usize].pins = 0;
        let still_valid = wr.is_ok()
            && g.files.get(&f).map_or(false, |e| e.blocks[b as usize] == NONE);
        if !still_valid {
            g.free.push(slot);
            return wr.map_err(Into::into);
        }
        g.set_block(f, b, slot, crc, data.len() as u32, probation);
        g.journal(&Rec::Block { f, b, s: slot, crc, len: data.len() as u32 });
        Ok(())
    }

    pub fn stats(&self) -> StoreStats {
        let g = self.inner.lock();
        let resident: u64 = g.files.values().map(|e| e.resident as u64).sum();
        StoreStats {
            resident_blocks: resident,
            resident_bytes: resident * self.bs as u64,
            slab_bytes: g.slots.len() as u64 * self.bs as u64,
            max_bytes: self.max_slots as u64 * self.bs as u64,
            probation_bytes: g.prob_blocks * self.bs as u64,
            spec_used_bytes: g.spec_used_bytes,
            spec_evicted_unused_bytes: g.spec_evicted_unused_bytes,
        }
    }

    pub fn slab_path(&self) -> PathBuf {
        self.dir.join("slab.bin")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_guesses_are_evicted_before_the_working_set() {
        let dir = std::env::temp_dir().join(format!("sfs-store-prob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = BlockStore::open(&dir, 16, 4 * 16).unwrap(); // 4 slots
        let f = st.open_file("k", "v", 16 * 100);
        st.insert(f, 0, &[1; 16]).unwrap(); // demand
        st.insert(f, 1, &[2; 16]).unwrap(); // demand
        st.insert_class(f, 2, &[3; 16], true).unwrap(); // guess
        st.insert_class(f, 3, &[4; 16], true).unwrap(); // guess, then read
        let p = st.pin(f, 3).unwrap();
        st.unpin(p, false);
        // full: the next insert must evict the unread guess (2), not the older demand blocks
        st.insert_class(f, 4, &[5; 16], true).unwrap();
        assert!(!st.contains(f, 2));
        assert!(st.contains(f, 0) && st.contains(f, 1) && st.contains(f, 3));
        let s = st.stats();
        assert_eq!(s.spec_used_bytes, 16);
        assert_eq!(s.spec_evicted_unused_bytes, 16);
        assert_eq!(s.probation_bytes, 16); // block 4
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
