//! Write-back block volume: a full read/write filesystem whose file data lives in
//! immutable remote objects ("packs"), with a strictly bounded local write cache.
//!
//! Model
//! * A file is a size plus a sparse map `block index -> location`; missing blocks are
//!   holes (zeros), so preallocation (SetEndOfFile) costs nothing.
//! * Location is either `Dirty` (a slot in the local write-cache slab) or `Remote`
//!   (object, offset, crc32). Writes always land in a dirty slot (copy-on-write: a
//!   partial write over a remote block first fetches it — read-modify-write).
//! * Sealed dirty blocks are packed into objects of `pack_bytes` and uploaded in the
//!   background. Drive can only store whole objects, so packs are immutable; a later
//!   overwrite creates a new dirty block and the old object region becomes garbage.
//! * Metadata (tree, sizes, block map) is a local append-only journal, compacted on open.
//!
//! Invariants
//! 1. A slot is reused only when no block maps to it AND no in-flight upload reads it.
//! 2. A dirty block becomes `Remote` only after the object is confirmed remotely
//!    (md5 verified by the store) AND the commit is fsynced to the journal.
//! 3. A commit applies only if the block still maps to the uploaded slot; otherwise
//!    newer data has been written meanwhile and wins.
//! 4. Writes never exceed the slab: when it is full they wait for uploads (backpressure).
//! 5. `flush()` = local durability: slab data + journal fsynced. Remote durability is
//!    reported separately (`pending_upload_bytes == 0`).

use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sfs_core::{BackendKind, Config, Engine, Error, Handle, ObjRef, ObjectStore, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Notify;

pub const ROOT: u64 = 1;

#[derive(Clone, Debug)]
pub struct VolConfig {
    /// Local state: write-cache slab, journal, read cache.
    pub state_dir: PathBuf,
    pub block_size: usize,
    /// Hard cap of locally held dirty data (the SSD budget for writes).
    pub write_cache_bytes: u64,
    /// Target size of one uploaded object.
    pub pack_bytes: usize,
    pub upload_parallel: usize,
    /// An unsealed dirty block idle this long is sealed for upload.
    pub seal_idle: Duration,
    /// Optional cap on logical bytes stored (volume quota).
    pub quota_bytes: Option<u64>,
    /// Read-cache budget for remote data.
    pub read_cache_bytes: u64,
    /// Upload a metadata snapshot at most this often (when something changed), so the
    /// volume can be rebuilt from the remote alone if the local disk is lost.
    pub meta_every: Duration,
    /// Never let the local disk holding the state drop below this many free bytes:
    /// writes wait (backpressure) instead, whatever else is consuming the disk.
    pub min_local_free: u64,
    /// Ignore the local journal (kept aside as a .bak) and rebuild from the newest remote snapshot.
    pub restore_from_remote: bool,
}

impl VolConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        VolConfig {
            state_dir: state_dir.into(),
            block_size: 1 << 20,
            write_cache_bytes: 1 << 30,
            pack_bytes: 16 << 20,
            upload_parallel: 2,
            seal_idle: Duration::from_secs(2),
            quota_bytes: None,
            read_cache_bytes: 1 << 30,
            meta_every: Duration::from_secs(120),
            min_local_free: 15 << 30,
            restore_from_remote: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Attr {
    pub ino: u64,
    pub is_dir: bool,
    pub size: u64,
    /// unix milliseconds
    pub mtime: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VolStats {
    pub dirty_bytes: u64,
    pub dirty_peak_bytes: u64,
    pub write_cache_limit: u64,
    pub slab_file_bytes: u64,
    pub pending_upload_bytes: u64,
    pub uploaded_bytes: u64,
    pub upload_objects: u64,
    pub upload_ms_total: u64,
    pub upload_errors: u64,
    pub backpressure_waits: u64,
    pub backpressure_ms_total: u64,
    pub rmw_fetches: u64,
    pub objects_live: u64,
    pub objects_deleted: u64,
    pub logical_bytes: u64,
    pub meta_snapshots: u64,
    pub zero_blocks_elided: u64,
    pub reserve_waits: u64,
    pub local_free_bytes: u64,
    pub orphans_at_open: u64,
    pub missing_vs_remote: u64,
}

#[derive(Default)]
struct Counters {
    dirty_peak: AtomicU64,
    uploaded_bytes: AtomicU64,
    upload_objects: AtomicU64,
    upload_ms: AtomicU64,
    upload_errors: AtomicU64,
    bp_waits: AtomicU64,
    bp_ms: AtomicU64,
    rmw: AtomicU64,
    objects_deleted: AtomicU64,
    meta_snapshots: AtomicU64,
    zero_blocks: AtomicU64,
    reserve_waits: AtomicU64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Open,
    Sealed,
    Uploading,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Loc {
    Dirty { s: u32, written: u32, phase: Phase },
    /// `len`: valid bytes stored for this block (the tail of a file's last block is not uploaded).
    Remote { o: u32, off: u64, crc: u32, len: u32 },
}

struct Inode {
    parent: u64,
    name: String,
    is_dir: bool,
    size: u64,
    mtime: u64,
    children: BTreeMap<String, u64>, // lowercase name -> ino
    blocks: BTreeMap<u32, Loc>,
}

struct ObjInfo {
    r: ObjRef,
    live: u64, // number of Remote blocks pointing into it
    /// In-progress reads of this object; GC waits for them (review #6).
    readers: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
enum J {
    Hdr { block: u32 },
    Ino { ino: u64, parent: u64, name: String, dir: bool, mtime: u64 },
    Size { ino: u64, size: u64, mtime: u64 },
    Mv { ino: u64, parent: u64, name: String },
    Rm { ino: u64 },
    Dirty { ino: u64, b: u32, s: u32 },
    Obj { o: u32, r: ObjRef },
    Commit { ino: u64, b: u32, o: u32, off: u64, crc: u32, s: u32, #[serde(default)] len: u32 },
    Rem { ino: u64, b: u32, o: u32, off: u64, crc: u32, #[serde(default)] len: u32 },
    Drop { ino: u64, b: u32 },
    ObjGone { o: u32 },
    /// Several records that must apply together (e.g. rename-over = Rm + Mv).
    /// One line in the journal: a torn write loses the whole batch, never half of it.
    Batch { recs: Vec<J> },
}

struct State {
    inodes: HashMap<u64, Inode>,
    next_ino: u64,
    objs: Vec<Option<ObjInfo>>,
    slot_refs: Vec<u32>,
    free: Vec<u32>,
    /// Slots released by records not yet fsynced: reusable only after the next
    /// journal sync, or a crash could replay the old mapping onto new data (review #4).
    pending_free: Vec<u32>,
    /// A journal write failed: nothing can be proven durable any more (review #8).
    jfail: bool,
    /// Journal records since the last remote metadata snapshot.
    meta_changes: u64,
    /// Journal bytes written but not yet fsynced (group commit flushes them every 200 ms).
    jdirty: bool,
    /// Objects found unreferenced when the volume was opened: never deleted automatically
    /// (they may belong to metadata lost in a crash; see the 2026-10-30 incident).
    orphans_at_open: u64,
    /// Inodes present in the newest remote snapshot but absent locally after replay.
    missing_vs_remote: u64,
    /// Garbage objects kept until a newer remote snapshot no longer references them.
    deferred_gc: Vec<u32>,
    /// Objects referenced by the two newest remote snapshots: GC must keep them, or a
    /// restore from those snapshots would point at deleted objects.
    protected: VecDeque<std::collections::HashSet<u32>>,
    hw: u32,
    max_slots: u32,
    sealed: VecDeque<(u64, u32)>,
    open_dirty: HashMap<(u64, u32), Instant>,
    gc: Vec<u32>,
    /// Block currently being written by the (single) writer: never sealed or packed
    /// until its bytes are in the slab, or the upload could miss them.
    writing: Option<(u64, u32)>,
    journal: Option<BufWriter<File>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn fatal(m: impl Into<String>) -> Error {
    Error::backend(BackendKind::Fatal, m)
}

pub fn not_found(p: &str) -> Error {
    Error::backend(BackendKind::NotFound, p.to_string())
}

impl State {
    fn j(&mut self, rec: &J) {
        if let Some(w) = self.journal.as_mut() {
            let ok = serde_json::to_writer(&mut *w, rec).is_ok() && w.write_all(b"\n").is_ok();
            if !ok {
                self.jfail = true;
            }
            self.meta_changes += 1;
            self.jdirty = true;
        }
    }

    fn sync_journal(&mut self) -> std::io::Result<()> {
        if self.jfail {
            return Err(std::io::Error::other("journal write failed earlier"));
        }
        if let Some(w) = self.journal.as_mut() {
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        self.jdirty = false;
        // Every record that released these slots is now durable.
        let pf = std::mem::take(&mut self.pending_free);
        self.free.extend(pf);
        Ok(())
    }

    fn slot_ref(&mut self, s: u32) {
        let i = s as usize;
        if self.slot_refs.len() <= i {
            self.slot_refs.resize(i + 1, 0);
        }
        self.slot_refs[i] += 1;
    }

    fn slot_unref(&mut self, s: u32) {
        let r = &mut self.slot_refs[s as usize];
        *r -= 1;
        if *r == 0 {
            if self.journal.is_some() {
                self.pending_free.push(s);
            } else {
                self.free.push(s); // replay: nothing to make durable
            }
        }
    }

    fn alloc_slot(&mut self) -> Option<u32> {
        if self.free.is_empty() && !self.pending_free.is_empty() {
            if let Err(e) = self.sync_journal() {
                eprintln!("[volume] journal sync failed: {e}");
                return None;
            }
        }
        if let Some(s) = self.free.pop() {
            return Some(s);
        }
        if self.hw < self.max_slots {
            self.hw += 1;
            return Some(self.hw - 1);
        }
        None
    }

    fn dirty_slots(&self) -> u64 {
        self.slot_refs.iter().filter(|&&r| r > 0).count() as u64
    }

    fn obj_unref(&mut self, o: u32) {
        if let Some(Some(info)) = self.objs.get_mut(o as usize) {
            info.live = info.live.saturating_sub(1);
            if info.live == 0 {
                self.gc.push(o);
            }
        }
    }

    fn pin_obj(&mut self, o: u32) {
        if let Some(Some(info)) = self.objs.get_mut(o as usize) {
            info.readers += 1;
        }
    }

    fn unpin_obj(&mut self, o: u32) {
        if let Some(Some(info)) = self.objs.get_mut(o as usize) {
            info.readers -= 1;
            if info.readers == 0 && info.live == 0 {
                self.gc.push(o);
            }
        }
    }

    /// Detach a block's current location (refcounts), leaving it absent (hole).
    fn release_loc(&mut self, loc: Loc, ino: u64, b: u32) {
        match loc {
            Loc::Dirty { s, .. } => {
                self.open_dirty.remove(&(ino, b));
                self.slot_unref(s);
            }
            Loc::Remote { o, .. } => self.obj_unref(o),
        }
    }

    /// Apply a journal record (used both on replay and at runtime, so they cannot diverge).
    fn apply(&mut self, rec: &J) {
        match rec {
            J::Hdr { .. } => {}
            J::Ino { ino, parent, name, dir, mtime } => {
                self.inodes.insert(
                    *ino,
                    Inode { parent: *parent, name: name.clone(), is_dir: *dir, size: 0, mtime: *mtime, children: BTreeMap::new(), blocks: BTreeMap::new() },
                );
                if let Some(p) = self.inodes.get_mut(parent) {
                    p.children.insert(name.to_lowercase(), *ino);
                }
                self.next_ino = self.next_ino.max(ino + 1);
            }
            J::Size { ino, size, mtime } => {
                if let Some(i) = self.inodes.get_mut(ino) {
                    i.size = *size;
                    i.mtime = *mtime;
                }
            }
            J::Mv { ino, parent, name } => {
                let Some(i) = self.inodes.get(ino) else { return };
                let (old_parent, old_name) = (i.parent, i.name.to_lowercase());
                if let Some(p) = self.inodes.get_mut(&old_parent) {
                    p.children.remove(&old_name);
                }
                if let Some(p) = self.inodes.get_mut(parent) {
                    p.children.insert(name.to_lowercase(), *ino);
                }
                let i = self.inodes.get_mut(ino).unwrap();
                i.parent = *parent;
                i.name = name.clone();
            }
            J::Rm { ino } => {
                if let Some(i) = self.inodes.remove(ino) {
                    if let Some(p) = self.inodes.get_mut(&i.parent) {
                        p.children.remove(&i.name.to_lowercase());
                    }
                    for (b, loc) in i.blocks {
                        self.release_loc(loc, *ino, b);
                    }
                }
            }
            J::Dirty { ino, b, s } => {
                let Some(prev) = self.inodes.get(ino).map(|i| i.blocks.get(b).copied()) else { return };
                if let Some(Loc::Dirty { s: ps, .. }) = prev {
                    if ps == *s {
                        // Same slot re-mapped (review #13): keep the reference as is.
                        self.inodes.get_mut(ino).unwrap().blocks.insert(*b, Loc::Dirty { s: *s, written: 0, phase: Phase::Open });
                        return;
                    }
                }
                if let Some(prev) = prev {
                    self.release_loc(prev, *ino, *b);
                }
                self.slot_ref(*s);
                self.inodes.get_mut(ino).unwrap().blocks.insert(*b, Loc::Dirty { s: *s, written: 0, phase: Phase::Open });
            }
            J::Obj { o, r } => {
                let i = *o as usize;
                if self.objs.len() <= i {
                    self.objs.resize_with(i + 1, || None);
                }
                self.objs[i] = Some(ObjInfo { r: r.clone(), live: 0, readers: 0 });
            }
            J::Commit { ino, b, o, off, crc, s, len } => {
                let cur = self.inodes.get(ino).and_then(|i| i.blocks.get(b).copied());
                if let Some(Loc::Dirty { s: cs, .. }) = cur {
                    if cs == *s {
                        self.open_dirty.remove(&(*ino, *b));
                        self.slot_unref(*s);
                        self.inodes.get_mut(ino).unwrap().blocks.insert(*b, Loc::Remote { o: *o, off: *off, crc: *crc, len: *len });
                        if let Some(Some(info)) = self.objs.get_mut(*o as usize) {
                            info.live += 1;
                        }
                    }
                }
            }
            J::Rem { ino, b, o, off, crc, len } => {
                if let Some(i) = self.inodes.get_mut(ino) {
                    i.blocks.insert(*b, Loc::Remote { o: *o, off: *off, crc: *crc, len: *len });
                    if let Some(Some(info)) = self.objs.get_mut(*o as usize) {
                        info.live += 1;
                    }
                }
            }
            J::Drop { ino, b } => {
                if let Some(loc) = self.inodes.get_mut(ino).and_then(|i| i.blocks.remove(b)) {
                    self.release_loc(loc, *ino, *b);
                }
            }
            J::ObjGone { o } => {
                if let Some(slot) = self.objs.get_mut(*o as usize) {
                    *slot = None;
                }
            }
            J::Batch { recs } => {
                for r in recs {
                    self.apply(r);
                }
            }
        }
    }

    /// Apply and journal several records as one atomic line.
    fn apply_batch(&mut self, recs: Vec<J>) {
        if recs.is_empty() {
            return;
        }
        for r in &recs {
            self.apply(r);
        }
        if recs.len() == 1 {
            self.j(&recs[0]);
        } else {
            self.j(&J::Batch { recs });
        }
    }

    fn snapshot(&self, block: u32) -> Vec<J> {
        let mut out = vec![J::Hdr { block }];
        for (o, info) in self.objs.iter().enumerate() {
            if let Some(info) = info {
                out.push(J::Obj { o: o as u32, r: info.r.clone() });
            }
        }
        let mut q = VecDeque::from([ROOT]);
        while let Some(ino) = q.pop_front() {
            let i = &self.inodes[&ino];
            if ino != ROOT {
                out.push(J::Ino { ino, parent: i.parent, name: i.name.clone(), dir: i.is_dir, mtime: i.mtime });
                out.push(J::Size { ino, size: i.size, mtime: i.mtime });
            }
            for (&b, loc) in &i.blocks {
                out.push(match *loc {
                    Loc::Dirty { s, .. } => J::Dirty { ino, b, s },
                    Loc::Remote { o, off, crc, len } => J::Rem { ino, b, o, off, crc, len },
                });
            }
            q.extend(i.children.values().copied());
        }
        out
    }

    /// Snapshot for the remote: tree + blocks already confirmed remotely. Blocks still
    /// local are left out (they are not in Drive yet), counted in the second value.
    fn remote_snapshot(&self, block: u32) -> (Vec<J>, u64, std::collections::HashSet<u32>) {
        let mut skipped = 0;
        let mut used = std::collections::HashSet::new();
        let recs: Vec<J> = self
            .snapshot(block)
            .into_iter()
            .filter(|r| match r {
                J::Dirty { .. } => {
                    skipped += 1;
                    false
                }
                J::Rem { o, .. } => {
                    used.insert(*o);
                    true
                }
                _ => true,
            })
            .collect();
        (recs, skipped, used)
    }
}

struct VInner {
    cfg: VolConfig,
    store: Arc<dyn ObjectStore>,
    eng: Engine,
    st: Mutex<State>,
    slab: File,
    wlock: tokio::sync::Mutex<()>,
    space: Notify,
    work: Notify,
    handles: Mutex<HashMap<u32, Arc<Handle>>>,
    c: Counters,
    stop: AtomicBool,
    /// Set when durability can no longer be guaranteed (journal fsync failed): the
    /// volume refuses writes until remounted; replay then decides what is durable.
    broken: AtomicBool,
    force_seal: AtomicBool,
    remote_free: Mutex<(Instant, Option<u64>)>,
    last_meta: Mutex<Instant>,
    /// (checked at, free bytes) of the disk holding `state_dir`.
    local_free: Mutex<(Instant, u64)>,
}

#[derive(Clone)]
pub struct Volume {
    v: Arc<VInner>,
}

struct Pack {
    entries: Vec<(u64, u32, u32, u32)>, // ino, block, slot, valid bytes
}

impl Volume {
    /// Open (or recover) a volume. Dirty blocks found in the journal are re-queued for upload.
    pub async fn open(cfg: VolConfig, store: Arc<dyn ObjectStore>) -> Result<Self> {
        std::fs::create_dir_all(&cfg.state_dir)?;
        let bs = cfg.block_size as u32;
        let jpath = cfg.state_dir.join("volume.jsonl");
        let mut st = State {
            inodes: HashMap::new(),
            next_ino: ROOT + 1,
            objs: Vec::new(),
            slot_refs: Vec::new(),
            free: Vec::new(),
            pending_free: Vec::new(),
            jfail: false,
            meta_changes: 0,
            jdirty: false,
            orphans_at_open: 0,
            missing_vs_remote: 0,
            deferred_gc: Vec::new(),
            protected: VecDeque::new(),
            hw: 0,
            max_slots: (cfg.write_cache_bytes / cfg.block_size as u64).max(4) as u32,
            sealed: VecDeque::new(),
            open_dirty: HashMap::new(),
            gc: Vec::new(),
            writing: None,
            journal: None,
        };
        st.inodes.insert(ROOT, Inode { parent: ROOT, name: String::new(), is_dir: true, size: 0, mtime: now_ms(), children: BTreeMap::new(), blocks: BTreeMap::new() });
        if cfg.restore_from_remote && jpath.exists() {
            let bak = cfg.state_dir.join(format!("volume.jsonl.bak-{}", now_ms()));
            std::fs::rename(&jpath, &bak)?;
            eprintln!("[volume] local journal set aside as {}; rebuilding from the remote snapshot", bak.display());
        }
        if !jpath.exists() {
            // Fresh local state: rebuild from the newest remote metadata snapshot, if any.
            if let Some((name, r)) = store.list_named("meta-").await.unwrap_or_default().into_iter().next() {
                let data = store.read_range(&store.object_meta(&r), 0, r.size).await?;
                let text = String::from_utf8_lossy(&data).to_string();
                let mut used = std::collections::HashSet::new();
                for line in text.lines() {
                    let Ok(rec) = serde_json::from_str::<J>(line) else { break };
                    if let J::Rem { o, .. } = &rec {
                        used.insert(*o);
                    }
                    st.apply(&rec);
                }
                st.protected.push_back(used);
                eprintln!("[volume] restored metadata from remote snapshot {name}");
            }
        }
        if let Ok(f) = File::open(&jpath) {
            let mut first = true;
            for line in BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                // The first unparsable line is the torn tail of a crash: nothing after it is trusted.
                let Ok(rec) = serde_json::from_str::<J>(&line) else { break };
                if first {
                    first = false;
                    if let J::Hdr { block } = rec {
                        if block != bs {
                            return Err(fatal(format!("volume was created with block size {block}, config says {bs}")));
                        }
                    }
                }
                st.apply(&rec);
            }
        }
        // Recovery: every surviving dirty block is re-sealed for upload; the slab
        // high-water mark covers every referenced slot; unreferenced slots are free.
        let mut max_s = 0;
        let mut requeue = Vec::new();
        for (&ino, i) in st.inodes.iter_mut() {
            for (&b, loc) in i.blocks.iter_mut() {
                if let Loc::Dirty { s, phase, .. } = loc {
                    *phase = Phase::Sealed;
                    max_s = max_s.max(*s + 1);
                    requeue.push((ino, b));
                }
            }
        }
        st.sealed.extend(requeue);
        st.hw = max_s.max(st.slot_refs.len() as u32).min(st.max_slots.max(max_s));
        st.free = (0..st.hw).filter(|&s| st.slot_refs.get(s as usize).copied().unwrap_or(0) == 0).collect();
        // Objects nobody references after replay are NOT garbage-collected automatically:
        // they may hold data whose metadata was lost (crash) and the remote snapshot may
        // still need them. Only objects released by operations in this session are GC'd.
        st.orphans_at_open = st.objs.iter().filter(|x| x.as_ref().map_or(false, |i| i.live == 0)).count() as u64;
        // Protect everything the newest remote snapshots reference, and compare trees.
        let snaps = store.list_named("meta-").await.unwrap_or_default();
        for (k, (name, r)) in snaps.iter().take(2).enumerate() {
            let Ok(data) = store.read_range(&store.object_meta(r), 0, r.size).await else { continue };
            let mut used = std::collections::HashSet::new();
            let mut missing = 0u64;
            for line in String::from_utf8_lossy(&data).lines() {
                let Ok(rec) = serde_json::from_str::<J>(line) else { break };
                match &rec {
                    J::Rem { o, .. } => {
                        used.insert(*o);
                    }
                    J::Ino { ino, .. } if k == 0 && !st.inodes.contains_key(ino) => missing += 1,
                    _ => {}
                }
            }
            if k == 0 {
                st.missing_vs_remote = missing;
                if missing > 0 {
                    eprintln!("[volume] WARNING: {missing} entries of the newest remote snapshot ({name}) are missing locally; nothing will be deleted. If unexpected, remount with --restore-from-remote");
                }
            }
            st.protected.push_back(used);
        }
        if st.orphans_at_open > 0 {
            eprintln!("[volume] {} unreferenced objects kept (not auto-deleted)", st.orphans_at_open);
        }

        // Compact the journal atomically.
        let tmp = cfg.state_dir.join("volume.jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for r in st.snapshot(bs) {
                serde_json::to_writer(&mut w, &r).map_err(|e| Error::Io(e.to_string()))?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, &jpath)?;
        st.journal = Some(BufWriter::new(OpenOptions::new().append(true).open(&jpath)?));

        let slab = OpenOptions::new().read(true).write(true).create(true).open(cfg.state_dir.join("wcache.slab"))?;
        let mut rc = Config::new(cfg.state_dir.join("rcache"));
        rc.cache_bytes = cfg.read_cache_bytes;
        rc.block_size = cfg.block_size.min(256 << 10);
        let backend: Arc<dyn sfs_core::Backend> = store.clone();
        let eng = Engine::new(backend, rc)?;
        let v = Arc::new(VInner {
            cfg,
            store,
            eng,
            st: Mutex::new(st),
            slab,
            wlock: tokio::sync::Mutex::new(()),
            space: Notify::new(),
            work: Notify::new(),
            handles: Mutex::new(HashMap::new()),
            c: Counters::default(),
            stop: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            force_seal: AtomicBool::new(false),
            remote_free: Mutex::new((Instant::now() - Duration::from_secs(3600), None)),
            last_meta: Mutex::new(Instant::now()),
            local_free: Mutex::new((Instant::now() - Duration::from_secs(60), u64::MAX)),
        });
        tokio::spawn(uploader(v.clone()));
        tokio::spawn(journal_syncer(v.clone()));
        Ok(Volume { v })
    }

    pub fn shutdown(&self) {
        self.v.stop.store(true, Relaxed);
        self.v.work.notify_waiters();
    }

    fn walk(st: &State, path: &str) -> Option<u64> {
        let mut cur = ROOT;
        for c in path.split(['/', '\\']).filter(|c| !c.is_empty()) {
            cur = *st.inodes.get(&cur)?.children.get(&c.to_lowercase())?;
        }
        Some(cur)
    }

    fn split(path: &str) -> (String, String) {
        let p = path.trim_end_matches(['/', '\\']);
        match p.rfind(['/', '\\']) {
            Some(i) => (p[..i].to_string(), p[i + 1..].to_string()),
            None => (String::new(), p.to_string()),
        }
    }

    fn attr_of(ino: u64, i: &Inode) -> Attr {
        Attr { ino, is_dir: i.is_dir, size: i.size, mtime: i.mtime }
    }

    pub fn lookup(&self, path: &str) -> Option<Attr> {
        let st = self.v.st.lock();
        let ino = Self::walk(&st, path)?;
        Some(Self::attr_of(ino, &st.inodes[&ino]))
    }

    /// Display name of an inode (canonical casing).
    pub fn name_of(&self, ino: u64) -> Option<String> {
        self.v.st.lock().inodes.get(&ino).map(|i| i.name.clone())
    }

    pub fn attr(&self, ino: u64) -> Option<Attr> {
        self.v.st.lock().inodes.get(&ino).map(|i| Self::attr_of(ino, i))
    }

    pub fn readdir(&self, path: &str) -> Result<Vec<(String, Attr)>> {
        let st = self.v.st.lock();
        let ino = Self::walk(&st, path).ok_or_else(|| not_found(path))?;
        let d = &st.inodes[&ino];
        if !d.is_dir {
            return Err(fatal("not a directory"));
        }
        Ok(d.children.values().map(|c| (st.inodes[c].name.clone(), Self::attr_of(*c, &st.inodes[c]))).collect())
    }

    pub fn create(&self, path: &str, is_dir: bool) -> Result<Attr> {
        let (parent, name) = Self::split(path);
        if name.is_empty() {
            return Err(fatal("empty name"));
        }
        let mut st = self.v.st.lock();
        let p = Self::walk(&st, &parent).ok_or_else(|| not_found(&parent))?;
        if !st.inodes[&p].is_dir {
            return Err(fatal("parent is not a directory"));
        }
        if st.inodes[&p].children.contains_key(&name.to_lowercase()) {
            return Err(Error::backend(BackendKind::Fatal, "exists"));
        }
        let ino = st.next_ino;
        let rec = J::Ino { ino, parent: p, name, dir: is_dir, mtime: now_ms() };
        st.apply(&rec);
        st.j(&rec);
        Ok(Self::attr_of(ino, &st.inodes[&ino]))
    }

    /// Remove a file or an empty directory.
    pub fn unlink(&self, path: &str) -> Result<()> {
        let mut st = self.v.st.lock();
        let ino = Self::walk(&st, path).ok_or_else(|| not_found(path))?;
        if ino == ROOT {
            return Err(fatal("cannot remove root"));
        }
        if !st.inodes[&ino].children.is_empty() {
            return Err(fatal("directory not empty"));
        }
        let rec = J::Rm { ino };
        st.apply(&rec);
        st.j(&rec);
        drop(st);
        self.v.space.notify_waiters();
        self.v.work.notify_waiters();
        Ok(())
    }

    /// Remove by inode (a handle's cached path can be stale after a rename).
    pub fn unlink_ino(&self, ino: u64) -> Result<()> {
        let mut st = self.v.st.lock();
        let i = st.inodes.get(&ino).ok_or_else(|| not_found("ino"))?;
        if ino == ROOT {
            return Err(fatal("cannot remove root"));
        }
        if !i.children.is_empty() {
            return Err(fatal("directory not empty"));
        }
        let rec = J::Rm { ino };
        st.apply(&rec);
        st.j(&rec);
        drop(st);
        self.v.space.notify_waiters();
        self.v.work.notify_waiters();
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str, replace: bool) -> Result<()> {
        let (tparent, tname) = Self::split(to);
        let mut st = self.v.st.lock();
        let ino = Self::walk(&st, from).ok_or_else(|| not_found(from))?;
        if ino == ROOT || tname.is_empty() {
            return Err(fatal("cannot move the root"));
        }
        let p = Self::walk(&st, &tparent).ok_or_else(|| not_found(&tparent))?;
        // Every check first; only then mutate (a rejected rename must change nothing).
        let mut a = p;
        loop {
            if a == ino {
                return Err(fatal("cannot move a directory into itself"));
            }
            if a == ROOT {
                break;
            }
            a = st.inodes[&a].parent;
        }
        let existing = st.inodes[&p].children.get(&tname.to_lowercase()).copied().filter(|&e| e != ino);
        if let Some(existing) = existing {
            if !replace {
                return Err(fatal("exists"));
            }
            if !st.inodes[&existing].children.is_empty() {
                return Err(fatal("target directory not empty"));
            }
        }
        let mut recs = Vec::new();
        if let Some(existing) = existing {
            recs.push(J::Rm { ino: existing });
        }
        recs.push(J::Mv { ino, parent: p, name: tname });
        st.apply_batch(recs); // replace + move: atomic in the journal
        Ok(())
    }

    fn handle_for(&self, o: u32, r: &ObjRef) -> Arc<Handle> {
        let mut g = self.v.handles.lock();
        g.entry(o).or_insert_with(|| Arc::new(self.v.eng.open_meta(self.v.store.object_meta(r)))).clone()
    }

    /// Full content of one remote block, crc-verified.
    async fn fetch_remote_block(&self, o: u32, off: u64, crc: u32, stored: u32) -> Result<Vec<u8>> {
        let r = {
            let st = self.v.st.lock();
            st.objs.get(o as usize).and_then(|x| x.as_ref()).map(|i| i.r.clone()).ok_or_else(|| fatal("object missing"))?
        };
        // len 0 = record written before partial blocks existed: a full block.
        let want = if stored == 0 { self.v.cfg.block_size as u64 } else { stored as u64 };
        let len = want.min(r.size.saturating_sub(off)) as usize;
        let data = self.handle_for(o, &r).read(off, len).await?;
        if crc32fast::hash(&data) != crc {
            return Err(fatal(format!("remote block crc mismatch in object {}", r.id)));
        }
        let mut v = data.to_vec();
        v.resize(self.v.cfg.block_size, 0);
        Ok(v)
    }

    pub async fn read(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>> {
        let bs = self.v.cfg.block_size as u64;
        let (size, locs) = {
            let mut st = self.v.st.lock();
            let i = st.inodes.get(&ino).ok_or_else(|| not_found("ino"))?;
            let size = i.size;
            if offset >= size || len == 0 {
                return Ok(Vec::new());
            }
            let end = (offset + len as u64).min(size);
            let (b0, b1) = ((offset / bs) as u32, ((end - 1) / bs) as u32);
            let locs: Vec<(u32, Option<Loc>)> = (b0..=b1).map(|b| (b, i.blocks.get(&b).copied())).collect();
            for (_, l) in &locs {
                match l {
                    Some(Loc::Dirty { s, .. }) => st.slot_ref(*s), // pin against reuse while we read it
                    Some(Loc::Remote { o, .. }) => st.pin_obj(*o), // pin against GC while we read it
                    None => {}
                }
            }
            (size, locs)
        };
        let end = (offset + len as u64).min(size);
        let mut out = vec![0u8; (end - offset) as usize];
        let mut result = Ok(());
        let mut buf = vec![0u8; bs as usize];
        for (b, loc) in &locs {
            let bstart = *b as u64 * bs;
            let (lo, hi) = (offset.max(bstart), end.min(bstart + bs));
            let dst = &mut out[(lo - offset) as usize..(hi - offset) as usize];
            match loc {
                None => {} // hole: zeros
                Some(Loc::Dirty { s, .. }) => {
                    if let Err(e) = self.v.slab.seek_read(&mut buf, *s as u64 * bs) {
                        result = Err(e.into());
                    }
                    dst.copy_from_slice(&buf[(lo - bstart) as usize..(hi - bstart) as usize]);
                }
                Some(Loc::Remote { o, off, crc, len }) => match self.fetch_remote_block(*o, *off, *crc, *len).await {
                    Ok(data) => dst.copy_from_slice(&data[(lo - bstart) as usize..(hi - bstart) as usize]),
                    Err(e) => result = Err(e),
                },
            }
        }
        {
            let mut st = self.v.st.lock();
            for (_, l) in &locs {
                match l {
                    Some(Loc::Dirty { s, .. }) => st.slot_unref(*s),
                    Some(Loc::Remote { o, .. }) => st.unpin_obj(*o),
                    None => {}
                }
            }
        }
        result.map(|_| out)
    }

    /// Get a writable slot for block `b`, preserving its current content.
    /// Waits (backpressure) while the write cache is full.
    async fn writable_slot(&self, ino: u64, b: u32, full_cover: bool) -> Result<u32> {
        let bs = self.v.cfg.block_size;
        loop {
            enum Need {
                Copy(u32),
                Fetch(u32, u64, u32, u32),
                Zero,
            }
            let need = {
                let mut st = self.v.st.lock();
                if self.v.broken.load(Relaxed) {
                    return Err(fatal("volume stopped after a journal failure; remount to recover"));
                }
                let cur = st.inodes.get(&ino).ok_or_else(|| not_found("ino"))?.blocks.get(&b).copied();
                match cur {
                    Some(Loc::Dirty { s, written, phase: Phase::Open | Phase::Sealed }) => {
                        // Re-open AND mark as being written in the same critical section, so the
                        // uploader cannot seal+pack it before our bytes land (review #2).
                        st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Open });
                        st.open_dirty.insert((ino, b), Instant::now());
                        st.writing = Some((ino, b));
                        return Ok(s);
                    }
                    Some(Loc::Dirty { s, phase: Phase::Uploading, .. }) => {
                        st.slot_ref(s); // keep it readable while we copy it
                        Need::Copy(s)
                    }
                    Some(Loc::Remote { o, off, crc, len }) if !full_cover => {
                        st.pin_obj(o);
                        Need::Fetch(o, off, crc, len)
                    }
                    _ => Need::Zero,
                }
            };
            let base: Option<Vec<u8>> = match need {
                Need::Copy(s) => {
                    let mut v = vec![0u8; bs];
                    let r = self.v.slab.seek_read(&mut v, s as u64 * bs as u64);
                    self.v.st.lock().slot_unref(s);
                    r?;
                    Some(v)
                }
                Need::Fetch(o, off, crc, len) => {
                    self.v.c.rmw.fetch_add(1, Relaxed);
                    let r = self.fetch_remote_block(o, off, crc, len).await;
                    self.v.st.lock().unpin_obj(o);
                    Some(r?)
                }
                Need::Zero => None,
            };
            // Allocate, or wait for the uploader to free space. Below the local free-space
            // reserve we do not allocate at all: the writer waits like any backpressure.
            let low = self.v.local_disk_low();
            if low {
                self.v.c.reserve_waits.fetch_add(1, Relaxed);
                if self.v.c.reserve_waits.load(Relaxed) % 300 == 1 {
                    eprintln!("[volume] local disk below the reserve ({} GB): writes are paused", self.v.cfg.min_local_free >> 30);
                }
            }
            let slot = if low {
                None
            } else {
                let mut st = self.v.st.lock();
                st.alloc_slot()
            };
            let Some(s) = slot else {
                let t = Instant::now();
                self.v.c.bp_waits.fetch_add(1, Relaxed);
                self.v.force_seal.store(true, Relaxed);
                self.v.work.notify_waiters();
                let notified = self.v.space.notified();
                let _ = tokio::time::timeout(Duration::from_millis(200), notified).await;
                self.v.c.bp_ms.fetch_add(t.elapsed().as_millis() as u64, Relaxed);
                continue;
            };
            let data = base.unwrap_or_else(|| vec![0u8; bs]);
            if let Err(e) = self.v.slab.seek_write(&data, s as u64 * bs as u64) {
                let mut st = self.v.st.lock();
                st.free.push(s);
                return Err(e.into());
            }
            let mut st = self.v.st.lock();
            let still_exists = st.inodes.contains_key(&ino);
            if !still_exists {
                st.free.push(s);
                return Err(not_found("ino"));
            }
            let rec = J::Dirty { ino, b, s };
            st.apply(&rec);
            st.j(&rec);
            st.open_dirty.insert((ino, b), Instant::now());
            st.writing = Some((ino, b));
            let dirty = st.dirty_slots() * bs as u64;
            self.v.c.dirty_peak.fetch_max(dirty, Relaxed);
            return Ok(s);
        }
    }

    pub async fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.write_opts(ino, offset, data, false, false).await
    }

    /// `to_eof`: append at the current end; `constrained`: never extend the file.
    /// Both are resolved under the writer lock, so concurrent appenders cannot collide.
    pub async fn write_opts(&self, ino: u64, offset: u64, data: &[u8], to_eof: bool, constrained: bool) -> Result<usize> {
        let _w = self.v.wlock.lock().await;
        let bs = self.v.cfg.block_size as u64;
        let size = self.v.st.lock().inodes.get(&ino).map(|i| i.size).ok_or_else(|| not_found("ino"))?;
        let offset = if to_eof { size } else { offset };
        let data = if constrained {
            if offset >= size {
                return Ok(0);
            }
            &data[..data.len().min((size - offset) as usize)]
        } else {
            data
        };
        let mut pos = 0usize;
        while pos < data.len() {
            let off = offset + pos as u64;
            let b = (off / bs) as u32;
            let in_block = (off % bs) as usize;
            let n = (bs as usize - in_block).min(data.len() - pos);
            let s = self.writable_slot(ino, b, in_block == 0 && n == bs as usize).await?;
            let wr = self.v.slab.seek_write(&data[pos..pos + n], s as u64 * bs + in_block as u64);
            let mut st = self.v.st.lock();
            st.writing = None;
            wr?;
            // Grow the size per block BEFORE the block can be sealed: the uploader sizes a
            // block's valid bytes from the file size (review #1, #9).
            let blk_end = off + n as u64;
            if st.inodes.get(&ino).map_or(false, |i| blk_end > i.size) {
                let rec = J::Size { ino, size: blk_end, mtime: now_ms() };
                st.apply(&rec);
                st.j(&rec);
            }
            if let Some(i) = st.inodes.get_mut(&ino) {
                if let Some(Loc::Dirty { s: cs, written, phase }) = i.blocks.get_mut(&b) {
                    if *cs == s {
                        *written = written.saturating_add(n as u32);
                        // Fully written blocks are sealed right away: no re-upload later.
                        if *written as u64 >= bs && *phase == Phase::Open {
                            *phase = Phase::Sealed;
                            st.sealed.push_back((ino, b));
                            st.open_dirty.remove(&(ino, b));
                        }
                    }
                }
            }
            pos += n;
        }
        self.v.work.notify_waiters();
        Ok(data.len())
    }

    /// Truncate or extend. Extending creates holes (no data, no upload).
    pub async fn set_size(&self, ino: u64, size: u64) -> Result<()> {
        let _w = self.v.wlock.lock().await;
        let bs = self.v.cfg.block_size as u64;
        let tail = (size % bs) as usize;
        let cut = (size / bs) as u32;
        // Zero the tail of a partially kept block so a later extension reads zeros.
        let partial = {
            let st = self.v.st.lock();
            let i = st.inodes.get(&ino).ok_or_else(|| not_found("ino"))?;
            size < i.size && tail != 0 && i.blocks.contains_key(&cut)
        };
        if partial {
            let s = self.writable_slot(ino, cut, false).await?;
            let zeros = vec![0u8; bs as usize - tail];
            let wr = self.v.slab.seek_write(&zeros, s as u64 * bs + tail as u64);
            self.v.st.lock().writing = None;
            wr?;
        }
        let mut st = self.v.st.lock();
        let first_gone = if tail == 0 { cut } else { cut + 1 };
        let gone: Vec<u32> = st.inodes.get(&ino).map(|i| i.blocks.range(first_gone..).map(|(&b, _)| b).collect()).unwrap_or_default();
        let mut recs: Vec<J> = gone.into_iter().map(|b| J::Drop { ino, b }).collect();
        recs.push(J::Size { ino, size, mtime: now_ms() });
        st.apply_batch(recs);
        drop(st);
        self.v.space.notify_waiters();
        Ok(())
    }

    /// Seal every unsealed block of a file (its handle was closed).
    pub fn close(&self, ino: u64) {
        let mut st = self.v.st.lock();
        let blocks: Vec<u32> = st.inodes.get(&ino).map(|i| i.blocks.keys().copied().collect()).unwrap_or_default();
        for b in blocks {
            if let Some(Loc::Dirty { s, written, phase: Phase::Open }) = st.inodes[&ino].blocks.get(&b).copied() {
                st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Sealed });
                st.sealed.push_back((ino, b));
                st.open_dirty.remove(&(ino, b));
            }
        }
        drop(st);
        self.v.work.notify_waiters();
    }

    /// Local durability: dirty data and metadata are on the local disk when this returns.
    pub fn flush(&self) -> Result<()> {
        self.v.slab.sync_data()?;
        self.v.st.lock().sync_journal()?;
        Ok(())
    }

    /// Remote durability: returns once every dirty block is uploaded and committed.
    pub async fn sync_remote(&self) -> Result<()> {
        self.flush()?;
        loop {
            let pending = {
                let mut st = self.v.st.lock();
                let open: Vec<(u64, u32)> = st.open_dirty.keys().copied().collect();
                for (ino, b) in open {
                    if let Some(Loc::Dirty { s, written, phase: Phase::Open }) = st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)).copied() {
                        st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Sealed });
                        st.sealed.push_back((ino, b));
                    }
                }
                st.open_dirty.clear();
                st.dirty_slots()
            };
            if pending == 0 {
                if self.v.st.lock().meta_changes > 0 {
                    upload_meta(&self.v).await?;
                }
                return Ok(());
            }
            self.v.force_seal.store(true, Relaxed);
            self.v.work.notify_waiters();
            let n = self.v.space.notified();
            let _ = tokio::time::timeout(Duration::from_millis(200), n).await;
        }
    }

    /// (total, free) bytes to present to applications: remote usable space, capped by quota.
    pub async fn space(&self) -> (u64, u64) {
        let used = self.stats().logical_bytes;
        let free_remote = {
            let cached = *self.v.remote_free.lock();
            if cached.0.elapsed() < Duration::from_secs(60) {
                cached.1
            } else {
                let f = self.v.store.remote_free_bytes().await.ok().flatten();
                *self.v.remote_free.lock() = (Instant::now(), f);
                f
            }
        };
        let mut free = free_remote.unwrap_or(0);
        if let Some(q) = self.v.cfg.quota_bytes {
            free = free.min(q.saturating_sub(used));
        }
        (used + free, free)
    }

    pub fn stats(&self) -> VolStats {
        let st = self.v.st.lock();
        let bs = self.v.cfg.block_size as u64;
        let dirty = st.dirty_slots() * bs;
        let logical = st.inodes.values().filter(|i| !i.is_dir).map(|i| i.size).sum();
        VolStats {
            dirty_bytes: dirty,
            dirty_peak_bytes: self.v.c.dirty_peak.load(Relaxed),
            write_cache_limit: st.max_slots as u64 * bs,
            slab_file_bytes: self.v.slab.metadata().map(|m| m.len()).unwrap_or(0),
            pending_upload_bytes: dirty,
            uploaded_bytes: self.v.c.uploaded_bytes.load(Relaxed),
            upload_objects: self.v.c.upload_objects.load(Relaxed),
            upload_ms_total: self.v.c.upload_ms.load(Relaxed),
            upload_errors: self.v.c.upload_errors.load(Relaxed),
            backpressure_waits: self.v.c.bp_waits.load(Relaxed),
            backpressure_ms_total: self.v.c.bp_ms.load(Relaxed),
            rmw_fetches: self.v.c.rmw.load(Relaxed),
            objects_live: st.objs.iter().filter(|o| o.is_some()).count() as u64,
            objects_deleted: self.v.c.objects_deleted.load(Relaxed),
            logical_bytes: logical,
            meta_snapshots: self.v.c.meta_snapshots.load(Relaxed),
            zero_blocks_elided: self.v.c.zero_blocks.load(Relaxed),
            reserve_waits: self.v.c.reserve_waits.load(Relaxed),
            local_free_bytes: self.v.local_free.lock().1,
            orphans_at_open: st.orphans_at_open,
            missing_vs_remote: st.missing_vs_remote,
        }
    }

    pub fn read_engine(&self) -> &Engine {
        &self.v.eng
    }
}

fn disk_free(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: NUL-terminated wide path, valid out pointers.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

impl VInner {
    /// True when the local disk is below the configured reserve (re-checked every 2 s).
    fn local_disk_low(&self) -> bool {
        let mut g = self.local_free.lock();
        if g.0.elapsed() >= Duration::from_secs(2) {
            *g = (Instant::now(), disk_free(&self.cfg.state_dir).unwrap_or(u64::MAX));
        }
        g.1 < self.cfg.min_local_free
    }

    /// Seal idle blocks, and under pressure the oldest open blocks too.
    fn seal_some(&self) {
        let mut st = self.st.lock();
        // Slots awaiting the next journal sync count as free: they become free on the next allocation.
        let free_slots = (st.free.len() + st.pending_free.len()) as u64 + (st.max_slots - st.hw) as u64;
        let pressure = self.force_seal.swap(false, Relaxed) || free_slots * 8 < st.max_slots as u64;
        let mut cands: Vec<((u64, u32), Instant)> = st.open_dirty.iter().map(|(k, t)| (*k, *t)).collect();
        cands.sort_by_key(|(_, t)| *t);
        let idle = self.cfg.seal_idle;
        let mut sealed_now = 0;
        for ((ino, b), t) in cands {
            if st.writing == Some((ino, b)) {
                continue;
            }
            if !(t.elapsed() >= idle || (pressure && sealed_now < 64)) {
                continue;
            }
            if let Some(Loc::Dirty { s, written, phase: Phase::Open }) = st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)).copied() {
                st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Sealed });
                st.sealed.push_back((ino, b));
                sealed_now += 1;
            }
            st.open_dirty.remove(&(ino, b));
        }
    }

    /// Take up to one pack of sealed blocks, filled by *bytes* (many small files share
    /// one object), or a partial pack when forced / nothing else will come.
    fn take_pack(&self, force: bool) -> Option<Pack> {
        const MAX_ENTRIES: usize = 4096;
        let target = self.cfg.pack_bytes as u64;
        let bs = self.cfg.block_size as u64;
        let mut st = self.st.lock();
        let valid_bytes = |st: &State, ino: u64, b: u32| -> Option<u64> {
            match st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)) {
                Some(Loc::Dirty { phase: Phase::Sealed, .. }) => Some(st.inodes[&ino].size.saturating_sub(b as u64 * bs).clamp(1, bs)),
                _ => None,
            }
        };
        let (mut ready, mut count) = (0u64, 0usize);
        for &(ino, b) in st.sealed.iter() {
            if let Some(n) = valid_bytes(&st, ino, b) {
                ready += n;
                count += 1;
            }
        }
        if count == 0 || (ready < target && count < MAX_ENTRIES && !force) {
            return None;
        }
        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        let mut bytes = 0u64;
        while bytes < target && entries.len() < MAX_ENTRIES {
            let Some((ino, b)) = st.sealed.pop_front() else { break };
            if st.writing == Some((ino, b)) {
                skipped.push((ino, b));
                continue;
            }
            if let Some(Loc::Dirty { s, written, phase: Phase::Sealed }) = st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)).copied() {
                let valid = valid_bytes(&st, ino, b).unwrap_or(bs) as u32;
                st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Uploading });
                st.slot_ref(s); // the in-flight upload holds the slot
                entries.push((ino, b, s, valid));
                bytes += valid as u64;
            }
        }
        for k in skipped {
            st.sealed.push_back(k);
        }
        (!entries.is_empty()).then_some(Pack { entries })
    }
}

/// Group commit: metadata operations (create, rename, unlink, truncate) become durable
/// within ~200 ms even if nobody calls flush, so a killed process loses at most the last
/// fraction of a second of metadata, and never half an operation (see J::Batch).
async fn journal_syncer(v: Arc<VInner>) {
    while !v.stop.load(Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut st = v.st.lock();
        if st.jdirty {
            if let Err(e) = st.sync_journal() {
                eprintln!("[volume] journal sync failed ({e}); volume stopped, remount to recover");
                v.broken.store(true, Relaxed);
                v.stop.store(true, Relaxed);
            }
        }
    }
}

async fn uploader(v: Arc<VInner>) {
    let sem = Arc::new(tokio::sync::Semaphore::new(v.cfg.upload_parallel.max(1)));
    let mut seq = 0u64;
    let mut last_progress = Instant::now();
    while !v.stop.load(Relaxed) {
        v.seal_some();
        let gc: Vec<u32> = {
            let mut st = v.st.lock();
            let gc = std::mem::take(&mut st.gc);
            // The records that dropped the last references must be durable before the
            // object disappears, or a crash would replay pointers into nothing (review #3).
            if !gc.is_empty() {
                if let Err(e) = st.sync_journal() {
                    eprintln!("[volume] journal sync failed before GC ({e}); stopping");
                    v.broken.store(true, Relaxed);
                    v.stop.store(true, Relaxed);
                    break;
                }
            }
            gc
        };
        for o in gc {
            let r = {
                let mut st = v.st.lock();
                match st.objs.get(o as usize).and_then(|x| x.as_ref()) {
                    Some(i) if i.live == 0 && i.readers > 0 => {
                        st.gc.push(o); // a read is in progress: retry later
                        None
                    }
                    Some(i) if i.live == 0 && st.protected.iter().any(|p| p.contains(&o)) => {
                        st.deferred_gc.push(o); // still referenced by a remote snapshot
                        None
                    }
                    Some(i) if i.live == 0 => Some(i.r.clone()),
                    _ => None,
                }
            };
            if let Some(r) = r {
                match v.store.delete_object(&r).await {
                    Ok(()) => {
                        let mut st = v.st.lock();
                        let rec = J::ObjGone { o };
                        st.apply(&rec);
                        st.j(&rec);
                        v.c.objects_deleted.fetch_add(1, Relaxed);
                        v.handles.lock().remove(&o);
                    }
                    Err(e) => {
                        eprintln!("[volume] delete of object {} failed ({e}); will retry", r.id);
                        v.st.lock().gc.push(o); // review #11
                    }
                }
            }
        }
        let due = {
            let st = v.st.lock();
            st.meta_changes > 0 && v.last_meta.lock().elapsed() >= v.cfg.meta_every && st.sealed.is_empty()
        };
        if due {
            if let Err(e) = upload_meta(&v).await {
                eprintln!("[volume] metadata snapshot failed ({e}); will retry");
                *v.last_meta.lock() = Instant::now();
            }
        }
        // Small packs are allowed once nothing has filled a pack for a while.
        let force = last_progress.elapsed() > Duration::from_millis(800);
        match v.take_pack(force) {
            Some(pack) => {
                last_progress = Instant::now();
                let permit = sem.clone().acquire_owned().await.unwrap();
                seq += 1;
                let v2 = v.clone();
                tokio::spawn(async move {
                    upload_pack(&v2, pack, seq).await;
                    drop(permit);
                    v2.work.notify_waiters();
                });
            }
            None => {
                if force {
                    last_progress = Instant::now();
                }
                let n = v.work.notified();
                let _ = tokio::time::timeout(Duration::from_millis(250), n).await;
            }
        }
    }
}

async fn upload_pack(v: &Arc<VInner>, pack: Pack, seq: u64) {
    let bs = v.cfg.block_size;
    // Only the valid bytes of each block are stored (a 53-byte file costs 53 bytes, not 1 MiB).
    let total: usize = pack.entries.iter().map(|e| e.3 as usize).sum();
    let mut data = Vec::with_capacity(total);
    let mut crcs = Vec::with_capacity(pack.entries.len());
    let mut offs = Vec::with_capacity(pack.entries.len());
    let mut kept = Vec::with_capacity(pack.entries.len());
    let mut zero = Vec::new();
    let mut buf = vec![0u8; bs];
    for &(ino, b, s, valid) in pack.entries.iter() {
        let chunk = &mut buf[..valid as usize];
        if v.slab.seek_read(chunk, s as u64 * bs as u64).is_err() {
            requeue(v, &pack);
            return;
        }
        // All-zero block (e.g. preallocation written as zeros): store it as a hole.
        if chunk.iter().all(|&x| x == 0) {
            zero.push((ino, b, s));
            continue;
        }
        crcs.push(crc32fast::hash(chunk));
        offs.push(data.len() as u64);
        data.extend_from_slice(chunk);
        kept.push((ino, b, s, valid));
    }
    if !zero.is_empty() {
        let mut st = v.st.lock();
        for &(ino, b, s) in &zero {
            // Only if the block still holds exactly the bytes we inspected (invariant 3).
            let still = matches!(st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)).copied(),
                Some(Loc::Dirty { s: cs, phase: Phase::Uploading, .. }) if cs == s);
            if still {
                let rec = J::Drop { ino, b };
                st.apply(&rec);
                st.j(&rec);
            }
            st.slot_unref(s); // this upload's hold
        }
        v.c.zero_blocks.fetch_add(zero.len() as u64, Relaxed);
        drop(st);
        v.space.notify_waiters();
    }
    let pack = Pack { entries: kept };
    if pack.entries.is_empty() {
        return;
    }
    let name = format!("pack-{}-{seq:08}.bin", now_ms());
    let t = Instant::now();
    let mut backoff = Duration::from_millis(500);
    let r = loop {
        match v.store.put_object(&name, Bytes::from(data.clone())).await {
            Ok(r) => break r,
            Err(e) => {
                v.c.upload_errors.fetch_add(1, Relaxed);
                eprintln!("[volume] upload of {name} failed: {e}; data stays local, retrying in {backoff:?}");
                if v.stop.load(Relaxed) {
                    requeue(v, &pack);
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    };
    let ms = t.elapsed().as_millis() as u64;
    eprintln!("[upload] {name}: {:.2} MB in {} ms = {:.2} MB/s ({} blocks)", r.size as f64 / 1e6, ms, r.size as f64 / 1e3 / ms.max(1) as f64, pack.entries.len());
    v.c.upload_ms.fetch_add(ms, Relaxed);
    v.c.uploaded_bytes.fetch_add(r.size, Relaxed);
    // Keep what was just uploaded in the (bounded, evictable) read cache: applications
    // such as Steam re-read freshly written files to verify them, and those reads must
    // not go back to the remote.
    {
        let (eng, meta) = (v.eng.clone(), v.store.object_meta(&r));
        let _ = tokio::task::spawn_blocking(move || eng.seed(&meta, &data)).await;
    }
    v.c.upload_objects.fetch_add(1, Relaxed);
    // Commit: journal first (fsync), then release local slots. Invariant 2.
    let mut st = v.st.lock();
    let o = st.objs.len() as u32;
    let rec = J::Obj { o, r };
    st.apply(&rec);
    st.j(&rec);
    let commits: Vec<J> = pack
        .entries
        .iter()
        .enumerate()
        .map(|(k, &(ino, b, s, valid))| J::Commit { ino, b, o, off: offs[k], crc: crcs[k], s, len: valid })
        .collect();
    for c in &commits {
        st.j(c);
    }
    if let Err(e) = st.sync_journal() {
        // The commit lines may or may not reach the disk later: requeueing could let new
        // writes go into slots a stale commit will claim on replay. Stop instead; the
        // local data stays in place and replay on the next mount decides (review #5).
        eprintln!("[volume] journal fsync failed ({e}); volume stopped, remount to recover");
        v.broken.store(true, Relaxed);
        v.stop.store(true, Relaxed);
        return;
    }
    for c in &commits {
        st.apply(c);
    }
    for &(_, _, s, _) in &pack.entries {
        st.slot_unref(s); // the upload's own hold
    }
    if st.objs[o as usize].as_ref().map_or(false, |i| i.live == 0) {
        st.gc.push(o); // every block was rewritten meanwhile
    }
    drop(st);
    v.space.notify_waiters();
}

/// Upload a metadata snapshot (only remotely-confirmed blocks), keep the newest two,
/// and release GC holds on objects that no remaining snapshot references.
async fn upload_meta(v: &Arc<VInner>) -> Result<()> {
    let (body, skipped, used, changes) = {
        let st = v.st.lock();
        let (recs, skipped, used) = st.remote_snapshot(v.cfg.block_size as u32);
        let mut body = String::new();
        for r in recs {
            body.push_str(&serde_json::to_string(&r).map_err(|e| Error::Io(e.to_string()))?);
            body.push('\n');
        }
        (body, skipped, used, st.meta_changes)
    };
    let name = format!("meta-{:013}.jsonl", now_ms());
    let len = body.len();
    v.store.put_object(&name, Bytes::from(body)).await?;
    {
        let mut st = v.st.lock();
        st.meta_changes = st.meta_changes.saturating_sub(changes);
        st.protected.push_back(used);
        while st.protected.len() > 2 {
            st.protected.pop_front();
        }
        let deferred = std::mem::take(&mut st.deferred_gc);
        st.gc.extend(deferred); // re-evaluated against the new protection set
    }
    *v.last_meta.lock() = Instant::now();
    v.c.meta_snapshots.fetch_add(1, Relaxed);
    eprintln!("[volume] metadata snapshot {name}: {len} bytes; {skipped} blocks still local (not in this snapshot)");
    if let Ok(list) = v.store.list_named("meta-").await {
        for (_, old) in list.into_iter().skip(2) {
            let _ = v.store.delete_object(&old).await;
        }
    }
    Ok(())
}

fn requeue(v: &Arc<VInner>, pack: &Pack) {
    let mut st = v.st.lock();
    for &(ino, b, s, _) in &pack.entries {
        if let Some(Loc::Dirty { s: cs, written, phase: Phase::Uploading }) = st.inodes.get(&ino).and_then(|i| i.blocks.get(&b)).copied() {
            if cs == s {
                st.inodes.get_mut(&ino).unwrap().blocks.insert(b, Loc::Dirty { s, written, phase: Phase::Sealed });
                st.sealed.push_back((ino, b));
            }
        }
        st.slot_unref(s);
    }
}
