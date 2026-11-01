//! WinFsp front-end over `sfs_core::Engine` with a local copy-on-write layer.
//!
//! * Reads of unmodified files stream from the remote through the engine
//!   (asynchronously: a blocked read parks a tokio task, not a dispatcher thread).
//! * Writes never reach the remote. The first write to a remote file copies it
//!   into the overlay (an overwrite/truncate skips the download); new files and
//!   directories are created directly in the overlay. Overlay files win over the
//!   remote base; deletes/renames of remote paths are recorded as whiteouts.
//! * A modified file is fsynced when its last handle closes, so a save written
//!   by a game survives a crash or power loss after close.
//! * Directory/metadata lookups use a TTL listing cache and block a dispatcher
//!   thread on a miss.

pub mod overlay;
pub mod vol;

use overlay::{Overlay, RESERVED_PREFIX};
use parking_lot::Mutex;
use sfs_core::{BackendKind, Engine, Error as CoreError, FileMeta, Handle};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::windows::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use winfsp::filesystem::{
    AsyncFileSystemContext, DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity,
    FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_NORMAL: u32 = 0x80;
const ATTR_READONLY: u32 = 0x1;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const CLEANUP_DELETE: u32 = 0x1;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC000_0185u32 as i32;
const STATUS_DISK_FULL: i32 = 0xC000_007Fu32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;

fn nt(code: i32) -> FspError {
    FspError::NTSTATUS(code)
}

fn map_err(e: CoreError) -> FspError {
    nt(match e {
        CoreError::Backend {
            kind: BackendKind::NotFound,
            ..
        } => STATUS_OBJECT_NAME_NOT_FOUND,
        CoreError::CacheFull => STATUS_DISK_FULL,
        _ => STATUS_IO_DEVICE_ERROR, // rate limit / quota / network: surfaced, never faked as data
    })
}

fn io_err(e: std::io::Error) -> FspError {
    use std::io::ErrorKind::*;
    nt(match e.kind() {
        NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        PermissionDenied => STATUS_ACCESS_DENIED,
        _ => STATUS_IO_DEVICE_ERROR,
    })
}

fn filetime(t: SystemTime) -> u64 {
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + d.subsec_nanos() as u64 / 100
}

fn norm(name: &U16CStr) -> String {
    let s = name.to_string_lossy().replace('\\', "/");
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        "/".into()
    } else if t.starts_with('/') {
        t.to_string()
    } else {
        format!("/{t}")
    }
}

fn base_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn parent_of(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &p[..i],
    }
}

fn index_number(id: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// (total, free) bytes of the volume holding `path`.
fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut free_avail, mut total, mut free_total) = (0u64, 0u64, 0u64);
    // SAFETY: valid NUL-terminated wide path and valid out-pointers.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut free_avail,
            &mut total,
            &mut free_total,
        )
    };
    (ok != 0).then_some((total, free_avail))
}

/// Log every failed mutating operation: this is how missing filesystem
/// features show up when an application (e.g. Steam) exercises the drive.
fn logged<T>(op: &str, path: &str, r: Result<T, FspError>) -> Result<T, FspError> {
    if let Err(e) = &r {
        let code = e.to_ntstatus() as u32;
        if code != STATUS_OBJECT_NAME_NOT_FOUND as u32 || op != "open" {
            eprintln!("[op-fail] {op} {path} -> NTSTATUS {code:#010x}");
        }
    }
    r
}

/// Where an entry currently comes from.
enum Entry {
    Remote(FileMeta),
    Local { is_dir: bool, md: std::fs::Metadata },
}

pub struct Ctx {
    path: Mutex<String>,
    is_dir: bool,
    /// Remote base, if this file existed remotely when opened.
    remote: Option<Handle>,
    /// Overlay file once materialized; wins over `remote`.
    local: Mutex<Option<File>>,
    dirty: AtomicBool,
    buf: DirBuffer,
}

type Listing = Arc<Vec<FileMeta>>;

#[derive(Default)]
pub struct CowStats {
    pub copy_ups: AtomicU64,
    pub copy_up_bytes: AtomicU64,
    pub creates: AtomicU64,
    pub writes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub renames: AtomicU64,
    pub deletes: AtomicU64,
    pub fsyncs: AtomicU64,
    pub write_errors: AtomicU64,
}

pub struct SfsFs {
    eng: Engine,
    rt: tokio::runtime::Handle,
    ov: Option<Overlay>,
    cow_max: u64,
    dirs: Mutex<HashMap<String, (Instant, Listing)>>,
    ttl: Duration,
    now: u64,
    pub cow: Arc<CowStats>,
}

impl SfsFs {
    /// `overlay = None` gives a strictly read-only volume.
    pub fn new(
        eng: Engine,
        rt: tokio::runtime::Handle,
        dir_ttl: Duration,
        overlay: Option<Overlay>,
        cow_max_bytes: u64,
    ) -> Self {
        SfsFs {
            eng,
            rt,
            ov: overlay,
            cow_max: cow_max_bytes,
            dirs: Mutex::new(HashMap::new()),
            ttl: dir_ttl,
            now: filetime(SystemTime::now()),
            cow: Arc::new(CowStats::default()),
        }
    }

    pub fn cow_stats(&self) -> Arc<CowStats> {
        self.cow.clone()
    }

    fn ov(&self) -> Result<&Overlay, FspError> {
        self.ov.as_ref().ok_or(nt(STATUS_MEDIA_WRITE_PROTECTED))
    }

    fn remote_list(&self, path: &str) -> Result<Listing, FspError> {
        if let Some((t, l)) = self.dirs.lock().get(path) {
            if t.elapsed() < self.ttl {
                return Ok(l.clone());
            }
        }
        let backend = self.eng.backend().clone();
        let p = path.to_string();
        let list = self
            .rt
            .block_on(async move { backend.list(&p).await })
            .map_err(map_err)?;
        let l: Listing = Arc::new(list);
        self.dirs
            .lock()
            .insert(path.to_string(), (Instant::now(), l.clone()));
        Ok(l)
    }

    /// Case-insensitive lookup, component by component (Windows may hand us any
    /// casing, e.g. upper-cased names on rename). The returned `meta.path` is the
    /// remote's canonical casing.
    fn remote_lookup(&self, path: &str) -> Result<FileMeta, FspError> {
        if path == "/" {
            return Ok(FileMeta {
                path: "/".into(),
                id: "/".into(),
                size: 0,
                version: String::new(),
                is_dir: true,
            });
        }
        let parent = parent_of(path);
        let parent_canon = if parent == "/" {
            "/".to_string()
        } else {
            self.remote_lookup(parent)?.path
        };
        let name = base_name(path);
        let l = self
            .remote_list(&parent_canon)
            .map_err(|_| nt(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let lname = name.to_lowercase();
        l.iter()
            .find(|m| base_name(&m.path) == name)
            .or_else(|| {
                l.iter()
                    .find(|m| base_name(&m.path).to_lowercase() == lname)
            })
            .cloned()
            .ok_or(nt(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    fn remote_exists(&self, path: &str) -> bool {
        self.remote_lookup(path).is_ok()
    }

    fn resolve(&self, path: &str) -> Result<Entry, FspError> {
        if let Some(ov) = &self.ov {
            if path != "/" && base_name(path).starts_with(RESERVED_PREFIX) {
                return Err(nt(STATUS_OBJECT_NAME_NOT_FOUND));
            }
            if path != "/" {
                if let Ok(md) = std::fs::metadata(ov.local(path)) {
                    return Ok(Entry::Local {
                        is_dir: md.is_dir(),
                        md,
                    });
                }
                if ov.is_whiteout(path) {
                    return Err(nt(STATUS_OBJECT_NAME_NOT_FOUND));
                }
            }
        }
        self.remote_lookup(path).map(Entry::Remote)
    }

    /// Remote listing minus whiteouts, overlaid with local entries.
    fn merged_list(&self, path: &str) -> Result<Vec<(String, Entry)>, FspError> {
        let mut out: Vec<(String, Entry)> = Vec::new();
        let mut idx: HashMap<String, usize> = HashMap::new();
        let local_dir = self
            .ov
            .as_ref()
            .map(|ov| ov.local(path))
            .filter(|p| p.is_dir());
        match self.remote_list(path) {
            Ok(l) => {
                for m in l.iter() {
                    if self.ov.as_ref().map_or(false, |ov| ov.is_whiteout(&m.path)) {
                        continue;
                    }
                    let name = base_name(&m.path).to_string();
                    idx.insert(name.to_lowercase(), out.len());
                    out.push((name, Entry::Remote(m.clone())));
                }
            }
            Err(e) if local_dir.is_none() => return Err(e),
            Err(_) => {} // directory exists only in the overlay
        }
        if let Some(dir) = local_dir {
            for e in std::fs::read_dir(dir).map_err(io_err)?.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(RESERVED_PREFIX) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let ent = (
                    name.clone(),
                    Entry::Local {
                        is_dir: md.is_dir(),
                        md,
                    },
                );
                match idx.get(&name.to_lowercase()) {
                    Some(&i) => out[i] = ent,
                    None => out.push(ent),
                }
            }
        }
        Ok(out)
    }

    fn fill(&self, info: &mut FileInfo, e: &Entry, id: &str) {
        match e {
            Entry::Remote(m) => {
                info.file_attributes = if m.is_dir {
                    ATTR_DIRECTORY
                } else if self.ov.is_some() {
                    ATTR_NORMAL
                } else {
                    ATTR_READONLY
                };
                info.file_size = if m.is_dir { 0 } else { m.size };
                for t in [
                    &mut info.creation_time,
                    &mut info.last_access_time,
                    &mut info.last_write_time,
                    &mut info.change_time,
                ] {
                    *t = self.now;
                }
            }
            Entry::Local { is_dir, md } => {
                info.file_attributes = if *is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL };
                info.file_size = if *is_dir { 0 } else { md.len() };
                let mt = md.modified().map(filetime).unwrap_or(self.now);
                info.creation_time = md.created().map(filetime).unwrap_or(mt);
                info.last_access_time = mt;
                info.last_write_time = mt;
                info.change_time = mt;
            }
        }
        info.reparse_tag = 0;
        info.allocation_size = (info.file_size + 4095) & !4095;
        info.index_number = index_number(id);
        info.hard_links = 0;
        info.ea_size = 0;
    }

    fn fill_ctx(&self, ctx: &Ctx, info: &mut FileInfo) -> Result<(), FspError> {
        let path = ctx.path.lock().clone();
        if let Some(f) = ctx.local.lock().as_ref() {
            let md = f.metadata().map_err(io_err)?;
            self.fill(info, &Entry::Local { is_dir: false, md }, &path);
            return Ok(());
        }
        if let Some(h) = &ctx.remote {
            self.fill(info, &Entry::Remote(h.meta().clone()), &path);
            return Ok(());
        }
        let e = self.resolve(&path)?;
        self.fill(info, &e, &path);
        Ok(())
    }

    /// Make the overlay copy of `ctx` exist. `copy=false` (truncate) skips the download.
    fn ensure_local(&self, ctx: &Ctx, copy: bool) -> Result<(), FspError> {
        let mut g = ctx.local.lock();
        if g.is_some() {
            return Ok(());
        }
        let ov = self.ov()?;
        let path = ctx.path.lock().clone();
        let lp = ov.local(&path);
        if let Some(parent) = lp.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let tmp = lp.with_file_name(format!("{RESERVED_PREFIX}tmp-{}", base_name(&path)));
        {
            let mut f = File::create(&tmp).map_err(io_err)?;
            if copy {
                if let Some(h) = &ctx.remote {
                    let size = h.meta().size;
                    if size > self.cow_max {
                        let _ = std::fs::remove_file(&tmp);
                        eprintln!(
                            "[cow] refusing copy-up of {path}: {size} bytes > limit {}",
                            self.cow_max
                        );
                        return Err(nt(STATUS_ACCESS_DENIED));
                    }
                    let mut off = 0u64;
                    while off < size {
                        let chunk = (size - off).min(4 << 20) as usize;
                        let data = self.rt.block_on(h.read(off, chunk)).map_err(map_err)?;
                        if data.is_empty() {
                            return Err(nt(STATUS_IO_DEVICE_ERROR));
                        }
                        std::io::Write::write_all(&mut f, &data).map_err(io_err)?;
                        off += data.len() as u64;
                    }
                    self.cow.copy_ups.fetch_add(1, Relaxed);
                    self.cow.copy_up_bytes.fetch_add(size, Relaxed);
                }
            }
            f.sync_all().map_err(io_err)?;
        }
        std::fs::rename(&tmp, &lp).map_err(io_err)?;
        *g = Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lp)
                .map_err(io_err)?,
        );
        Ok(())
    }

    fn read_local(f: &File, buffer: &mut [u8], offset: u64) -> Result<u32, FspError> {
        let len = f.metadata().map_err(io_err)?.len();
        if offset >= len {
            return Err(nt(STATUS_END_OF_FILE));
        }
        let want = buffer.len().min((len - offset) as usize);
        let mut done = 0;
        while done < want {
            let n = f
                .seek_read(&mut buffer[done..want], offset + done as u64)
                .map_err(io_err)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done as u32)
    }
}

impl FileSystemContext for SfsFs {
    type FileContext = Ctx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = norm(file_name);
        let e = self.resolve(&path)?;
        let mut info = FileInfo::default();
        self.fill(&mut info, &e, &path);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: info.file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Ctx> {
        let __op_path = norm(file_name);
        logged(
            "open",
            &__op_path,
            (|| -> winfsp::Result<Ctx> {
                let mut path = norm(file_name);
                let e = self.resolve(&path)?;
                if let Entry::Remote(m) = &e {
                    path = m.path.clone(); // canonical casing for everything that follows (rename, copy-up)
                }
                self.fill(file_info.as_mut(), &e, &path);
                if path != "/" {
                    let w: Vec<u16> = path.replace('/', "\\").encode_utf16().collect();
                    file_info.set_normalized_name(&w, None);
                }
                let ctx = match e {
                    Entry::Local { is_dir: true, .. }
                    | Entry::Remote(FileMeta { is_dir: true, .. }) => Ctx {
                        path: Mutex::new(path),
                        is_dir: true,
                        remote: None,
                        local: Mutex::new(None),
                        dirty: AtomicBool::new(false),
                        buf: DirBuffer::new(),
                    },
                    Entry::Local { .. } => {
                        let f = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(self.ov()?.local(&path))
                            .map_err(io_err)?;
                        Ctx {
                            path: Mutex::new(path),
                            is_dir: false,
                            remote: None,
                            local: Mutex::new(Some(f)),
                            dirty: AtomicBool::new(false),
                            buf: DirBuffer::new(),
                        }
                    }
                    Entry::Remote(m) => Ctx {
                        path: Mutex::new(path),
                        is_dir: false,
                        remote: Some(self.eng.open_meta(m)),
                        local: Mutex::new(None), // copy-up is lazy: only on the first write
                        dirty: AtomicBool::new(false),
                        buf: DirBuffer::new(),
                    },
                };
                Ok(ctx)
            })(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Ctx> {
        let __op_path = norm(file_name);
        logged(
            "create",
            &__op_path,
            (|| -> winfsp::Result<Ctx> {
                let ov = self.ov()?;
                let path = norm(file_name);
                if base_name(&path).starts_with(RESERVED_PREFIX) {
                    return Err(nt(STATUS_ACCESS_DENIED));
                }
                if self.resolve(&path).is_ok() {
                    return Err(nt(STATUS_OBJECT_NAME_COLLISION));
                }
                match self.resolve(parent_of(&path)) {
                    Ok(Entry::Local { is_dir: true, .. })
                    | Ok(Entry::Remote(FileMeta { is_dir: true, .. })) => {}
                    _ => return Err(nt(STATUS_OBJECT_PATH_NOT_FOUND)),
                }
                let lp = ov.local(&path);
                if let Some(parent) = lp.parent() {
                    std::fs::create_dir_all(parent).map_err(io_err)?;
                }
                let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
                let local = if is_dir {
                    std::fs::create_dir(&lp).map_err(io_err)?;
                    None
                } else {
                    Some(
                        OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create_new(true)
                            .open(&lp)
                            .map_err(io_err)?,
                    )
                };
                ov.remove_whiteout(&path).map_err(io_err)?;
                self.cow.creates.fetch_add(1, Relaxed);
                let md = std::fs::metadata(&lp).map_err(io_err)?;
                self.fill(file_info.as_mut(), &Entry::Local { is_dir, md }, &path);
                Ok(Ctx {
                    path: Mutex::new(path),
                    is_dir,
                    remote: None,
                    local: Mutex::new(local),
                    dirty: AtomicBool::new(!is_dir),
                    buf: DirBuffer::new(),
                })
            })(),
        )
    }

    fn overwrite(
        &self,
        ctx: &Ctx,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "overwrite",
            &__op_path,
            (|| -> winfsp::Result<()> {
                self.ensure_local(ctx, false)?; // truncating: never download the old content
                if let Some(f) = ctx.local.lock().as_ref() {
                    f.set_len(0).map_err(io_err)?;
                }
                ctx.dirty.store(true, Relaxed);
                self.fill_ctx(ctx, file_info)
            })(),
        )
    }

    fn close(&self, _context: Ctx) {}

    fn cleanup(&self, ctx: &Ctx, _file_name: Option<&U16CStr>, flags: u32) {
        let path = ctx.path.lock().clone();
        if flags & CLEANUP_DELETE != 0 {
            let Some(ov) = &self.ov else { return };
            let lp = ov.local(&path);
            let res = if ctx.is_dir {
                match std::fs::remove_dir(&lp) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                }
            } else {
                drop(ctx.local.lock().take());
                match std::fs::remove_file(&lp) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                }
            };
            let res = res.and_then(|_| {
                if self.remote_exists(&path) {
                    ov.add_whiteout(&path)
                } else {
                    Ok(())
                }
            });
            match res {
                Ok(()) => {
                    self.cow.deletes.fetch_add(1, Relaxed);
                }
                Err(e) => {
                    self.cow.write_errors.fetch_add(1, Relaxed);
                    eprintln!("[cow] delete of {path} failed: {e}");
                }
            }
            return;
        }
        // Durability: a modified file (e.g. a save) hits the disk when its handle closes.
        if ctx.dirty.swap(false, Relaxed) {
            if let Some(f) = ctx.local.lock().as_ref() {
                match f.sync_all() {
                    Ok(()) => {
                        self.cow.fsyncs.fetch_add(1, Relaxed);
                    }
                    Err(e) => {
                        self.cow.write_errors.fetch_add(1, Relaxed);
                        eprintln!("[cow] fsync of {path} failed: {e}");
                    }
                }
            }
        }
    }

    fn flush(&self, ctx: Option<&Ctx>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let Some(ctx) = ctx else { return Ok(()) };
        if let Some(f) = ctx.local.lock().as_ref() {
            f.sync_all().map_err(io_err)?;
            self.cow.fsyncs.fetch_add(1, Relaxed);
        }
        self.fill_ctx(ctx, file_info)
    }

    fn get_file_info(&self, ctx: &Ctx, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.fill_ctx(ctx, file_info)
    }

    fn set_basic_info(
        &self,
        ctx: &Ctx,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "set_basic_info",
            &__op_path,
            (|| -> winfsp::Result<()> {
                // Attributes and explicit timestamps are not persisted (documented limitation).
                self.fill_ctx(ctx, file_info)
            })(),
        )
    }

    fn set_file_size(
        &self,
        ctx: &Ctx,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "set_file_size",
            &__op_path,
            (|| -> winfsp::Result<()> {
                if ctx.is_dir {
                    return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
                }
                // Shrinking to zero needs no download.
                self.ensure_local(ctx, new_size != 0)?;
                if let Some(f) = ctx.local.lock().as_ref() {
                    let len = f.metadata().map_err(io_err)?.len();
                    if !set_allocation_size || new_size < len {
                        f.set_len(new_size).map_err(io_err)?;
                    }
                }
                ctx.dirty.store(true, Relaxed);
                self.fill_ctx(ctx, file_info)
            })(),
        )
    }

    fn set_delete(&self, ctx: &Ctx, _file_name: &U16CStr, delete_file: bool) -> winfsp::Result<()> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "set_delete",
            &__op_path,
            (|| -> winfsp::Result<()> {
                self.ov()?;
                if delete_file && ctx.is_dir {
                    let path = ctx.path.lock().clone();
                    if !self.merged_list(&path)?.is_empty() {
                        return Err(nt(STATUS_DIRECTORY_NOT_EMPTY));
                    }
                }
                Ok(())
            })(),
        )
    }

    fn rename(
        &self,
        ctx: &Ctx,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let ov = self.ov()?;
        let old = norm(file_name);
        let new = norm(new_file_name);
        if base_name(&new).starts_with(RESERVED_PREFIX) {
            return Err(nt(STATUS_ACCESS_DENIED));
        }
        // The handle's own path has the right casing; `file_name` may be upper-cased.
        let old = {
            let p = ctx.path.lock().clone();
            if p.eq_ignore_ascii_case(&old) {
                p
            } else {
                old
            }
        };
        let res = (|| -> Result<(), FspError> {
            if ctx.is_dir {
                // Moving a directory that has remote content would need a recursive copy-up.
                if self.remote_exists(&old) {
                    return Err(nt(STATUS_ACCESS_DENIED));
                }
            } else {
                self.ensure_local(ctx, true)?;
            }
            if let Ok(target) = self.resolve(&new) {
                if !replace_if_exists {
                    return Err(nt(STATUS_OBJECT_NAME_COLLISION));
                }
                if let Entry::Local { is_dir: false, .. } = target {
                    std::fs::remove_file(ov.local(&new)).map_err(io_err)?;
                }
            }
            let dst = ov.local(&new);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            std::fs::rename(ov.local(&old), &dst).map_err(io_err)?;
            if self.remote_exists(&old) {
                ov.add_whiteout(&old).map_err(io_err)?;
            }
            ov.remove_whiteout(&new).map_err(io_err)?;
            *ctx.path.lock() = new.clone();
            Ok(())
        })();
        match &res {
            Ok(()) => {
                self.cow.renames.fetch_add(1, Relaxed);
            }
            Err(_) => {
                self.cow.write_errors.fetch_add(1, Relaxed);
                eprintln!("[cow] rename {old} -> {new} failed");
            }
        }
        res
    }

    fn write(
        &self,
        ctx: &Ctx,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "write",
            &__op_path,
            (|| -> winfsp::Result<u32> {
                if ctx.is_dir {
                    return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
                }
                self.ensure_local(ctx, true)?;
                let n = {
                    let g = ctx.local.lock();
                    let f = g.as_ref().expect("materialized");
                    let len = f.metadata().map_err(io_err)?.len();
                    let off = if write_to_eof { len } else { offset };
                    let mut data = buffer;
                    if constrained_io {
                        if off >= len {
                            0
                        } else {
                            data = &buffer[..buffer.len().min((len - off) as usize)];
                            f.seek_write(data, off).map_err(io_err)?;
                            data.len()
                        }
                    } else {
                        let mut done = 0;
                        while done < data.len() {
                            done += f
                                .seek_write(&data[done..], off + done as u64)
                                .map_err(io_err)?;
                        }
                        done
                    }
                };
                ctx.dirty.store(true, Relaxed);
                self.cow.writes.fetch_add(1, Relaxed);
                self.cow.write_bytes.fetch_add(n as u64, Relaxed);
                self.fill_ctx(ctx, file_info)?;
                Ok(n as u32)
            })(),
        )
    }

    fn read(&self, ctx: &Ctx, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if ctx.is_dir {
            return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
        }
        if let Some(f) = ctx.local.lock().as_ref() {
            return Self::read_local(f, buffer, offset);
        }
        let h = ctx.remote.as_ref().ok_or(nt(STATUS_IO_DEVICE_ERROR))?;
        if offset >= h.meta().size {
            return Err(nt(STATUS_END_OF_FILE));
        }
        let b = self
            .rt
            .block_on(h.read(offset, buffer.len()))
            .map_err(map_err)?;
        buffer[..b.len()].copy_from_slice(&b);
        Ok(b.len() as u32)
    }

    fn read_directory(
        &self,
        ctx: &Ctx,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let __op_path = ctx.path.lock().clone();
        logged(
            "read_directory",
            &__op_path,
            (|| -> winfsp::Result<u32> {
                if !ctx.is_dir {
                    return Err(nt(STATUS_NOT_A_DIRECTORY));
                }
                if let Ok(lock) = ctx.buf.acquire(marker.is_none(), None) {
                    let path = ctx.path.lock().clone();
                    let mut entries = self.merged_list(&path)?;
                    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
                    let mut di = DirInfo::<255>::new();
                    if path != "/" {
                        for dot in [".", ".."] {
                            di.reset();
                            let m = FileMeta {
                                path: dot.into(),
                                id: dot.into(),
                                size: 0,
                                version: String::new(),
                                is_dir: true,
                            };
                            self.fill(di.file_info_mut(), &Entry::Remote(m), dot);
                            di.set_name(dot)?;
                            lock.write(&mut di)?;
                        }
                    }
                    for (name, e) in entries.iter() {
                        di.reset();
                        let full = format!("{}/{}", path.trim_end_matches('/'), name);
                        self.fill(di.file_info_mut(), e, &full);
                        di.set_name(name)?;
                        lock.write(&mut di)?;
                    }
                }
                Ok(ctx.buf.read(marker, buffer))
            })(),
        )
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        // Honest free space: new data currently lands in the local overlay, so the
        // local disk holding it is the real limit (not the remote quota).
        match self.ov.as_ref().and_then(|ov| disk_space(ov.root())) {
            Some((total, free)) => {
                out.total_size = total;
                out.free_size = free;
            }
            None => {
                out.total_size = 1 << 40;
                out.free_size = 0;
            }
        }
        out.set_volume_label("streamfs");
        Ok(())
    }
}

impl AsyncFileSystemContext for SfsFs {
    fn read_async(
        &self,
        ctx: &Ctx,
        buffer: &mut [u8],
        offset: u64,
    ) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move {
            if ctx.is_dir {
                return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
            }
            {
                let g = ctx.local.lock();
                if let Some(f) = g.as_ref() {
                    return Self::read_local(f, buffer, offset);
                }
            }
            let h = ctx.remote.as_ref().ok_or(nt(STATUS_IO_DEVICE_ERROR))?;
            if offset >= h.meta().size {
                return Err(nt(STATUS_END_OF_FILE));
            }
            let b = h.read(offset, buffer.len()).await.map_err(map_err)?;
            buffer[..b.len()].copy_from_slice(&b);
            Ok(b.len() as u32)
        }
    }

    // write/read_directory may block on the network (copy-up, uncached listing) via
    // `Handle::block_on`, which panics on a tokio worker unless wrapped in block_in_place.
    fn write_async(
        &self,
        ctx: &Ctx,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move {
            tokio::task::block_in_place(|| {
                Self::write(
                    self,
                    ctx,
                    buffer,
                    offset,
                    write_to_eof,
                    constrained_io,
                    file_info,
                )
            })
        }
    }

    fn read_directory_async(
        &self,
        ctx: &Ctx,
        pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move {
            tokio::task::block_in_place(|| Self::read_directory(self, ctx, pattern, marker, buffer))
        }
    }

    fn spawn_task(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.rt.spawn(future);
    }
}
