//! WinFsp front-end for the write-back block volume (`sfs_volume::Volume`):
//! every byte written lands in a bounded local write cache and is uploaded in packs;
//! reads come from the cache or the remote objects. Free space shown to Windows is
//! the remote usable space; the local SSD is protected by backpressure instead.

use parking_lot::Mutex;
use sfs_core::{BackendKind, Error as CoreError};
use sfs_volume::{Attr, Volume};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;
use winfsp::filesystem::{
    AsyncFileSystemContext, DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_NORMAL: u32 = 0x80;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const CLEANUP_DELETE: u32 = 0x1;
const NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
const COLLISION: i32 = 0xC000_0035u32 as i32;
const END_OF_FILE: i32 = 0xC000_0011u32 as i32;
const IO_ERROR: i32 = 0xC000_0185u32 as i32;
const NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
const IS_DIR: i32 = 0xC000_00BAu32 as i32;
const NOT_DIR: i32 = 0xC000_0103u32 as i32;

fn nt(c: i32) -> FspError {
    FspError::NTSTATUS(c)
}

fn map(e: CoreError) -> FspError {
    let s = e.to_string();
    nt(match e {
        CoreError::Backend { kind: BackendKind::NotFound, .. } => NOT_FOUND,
        _ if s.contains("not empty") => NOT_EMPTY,
        _ if s.contains("exists") => COLLISION,
        _ if s.contains("not a directory") => NOT_DIR,
        _ => {
            eprintln!("[vol] io error: {s}");
            IO_ERROR
        }
    })
}

fn norm(name: &U16CStr) -> String {
    let s = name.to_string_lossy().replace('\\', "/");
    let t = s.trim_matches('/');
    t.to_string()
}

fn filetime_ms(ms: u64) -> u64 {
    (ms / 1000 + 11_644_473_600) * 10_000_000 + (ms % 1000) * 10_000
}

fn fill(info: &mut FileInfo, a: &Attr) {
    info.file_attributes = if a.is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL };
    info.reparse_tag = 0;
    info.file_size = if a.is_dir { 0 } else { a.size };
    info.allocation_size = (info.file_size + 4095) & !4095;
    let t = filetime_ms(a.mtime);
    info.creation_time = t;
    info.last_access_time = t;
    info.last_write_time = t;
    info.change_time = t;
    info.index_number = a.ino;
    info.hard_links = 0;
    info.ea_size = 0;
}

pub struct VCtx {
    ino: u64,
    is_dir: bool,
    path: Mutex<String>,
    modified: AtomicBool,
    buf: DirBuffer,
}

pub struct VolFs {
    pub vol: Volume,
    rt: tokio::runtime::Handle,
}

impl VolFs {
    pub fn new(vol: Volume, rt: tokio::runtime::Handle) -> Self {
        VolFs { vol, rt }
    }

    fn attr(&self, ctx: &VCtx) -> Result<Attr, FspError> {
        self.vol.attr(ctx.ino).ok_or(nt(NOT_FOUND))
    }

    fn ctx(&self, a: &Attr, path: String) -> VCtx {
        VCtx { ino: a.ino, is_dir: a.is_dir, path: Mutex::new(path), modified: AtomicBool::new(false), buf: DirBuffer::new() }
    }
}

impl FileSystemContext for VolFs {
    type FileContext = VCtx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _sd: Option<&mut [std::ffi::c_void]>,
        _r: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let a = self.vol.lookup(&norm(file_name)).ok_or(nt(NOT_FOUND))?;
        Ok(FileSecurity { reparse: false, sz_security_descriptor: 0, attributes: if a.is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL } })
    }

    fn open(&self, file_name: &U16CStr, _co: u32, _ga: u32, file_info: &mut OpenFileInfo) -> winfsp::Result<VCtx> {
        let path = norm(file_name);
        let a = self.vol.lookup(&path).ok_or(nt(NOT_FOUND))?;
        fill(file_info.as_mut(), &a);
        Ok(self.ctx(&a, path))
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _ga: u32,
        _fa: u32,
        _sd: Option<&[std::ffi::c_void]>,
        _alloc: u64,
        _eb: Option<&[u8]>,
        _ebr: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<VCtx> {
        let path = norm(file_name);
        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        match self.vol.lookup(parent) {
            Some(p) if p.is_dir => {}
            _ => return Err(nt(PATH_NOT_FOUND)),
        }
        let a = self.vol.create(&path, create_options & FILE_DIRECTORY_FILE != 0).map_err(map)?;
        fill(file_info.as_mut(), &a);
        let c = self.ctx(&a, path);
        c.modified.store(true, Relaxed);
        Ok(c)
    }

    fn overwrite(&self, ctx: &VCtx, _fa: u32, _rfa: bool, _alloc: u64, _eb: Option<&[u8]>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.rt.block_on(self.vol.set_size(ctx.ino, 0)).map_err(map)?;
        ctx.modified.store(true, Relaxed);
        fill(file_info, &self.attr(ctx)?);
        Ok(())
    }

    fn close(&self, _ctx: VCtx) {}

    fn cleanup(&self, ctx: &VCtx, _file_name: Option<&U16CStr>, flags: u32) {
        let path = ctx.path.lock().clone();
        if flags & CLEANUP_DELETE != 0 {
            // By inode: the cached path may be stale if another handle renamed the file.
            if let Err(e) = self.vol.unlink_ino(ctx.ino) {
                eprintln!("[vol] delete {path} failed: {e}");
            }
            return;
        }
        if ctx.modified.swap(false, Relaxed) {
            // Last handle of a modified file: seal its blocks for upload and make it
            // durable on the local disk (a closed save survives a crash or power loss).
            self.vol.close(ctx.ino);
            if let Err(e) = self.vol.flush() {
                eprintln!("[vol] flush after close of {path} failed: {e}");
            }
        }
    }

    fn flush(&self, ctx: Option<&VCtx>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.vol.flush().map_err(map)?;
        if let Some(ctx) = ctx {
            fill(file_info, &self.attr(ctx)?);
        }
        Ok(())
    }

    fn get_file_info(&self, ctx: &VCtx, file_info: &mut FileInfo) -> winfsp::Result<()> {
        fill(file_info, &self.attr(ctx)?);
        Ok(())
    }

    fn set_basic_info(&self, ctx: &VCtx, _fa: u32, _ct: u64, _la: u64, _lw: u64, _lc: u64, file_info: &mut FileInfo) -> winfsp::Result<()> {
        fill(file_info, &self.attr(ctx)?); // attributes/explicit times not persisted (documented)
        Ok(())
    }

    fn set_file_size(&self, ctx: &VCtx, new_size: u64, set_allocation_size: bool, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if ctx.is_dir {
            return Err(nt(IS_DIR));
        }
        let cur = self.attr(ctx)?.size;
        if !set_allocation_size || new_size < cur {
            self.rt.block_on(self.vol.set_size(ctx.ino, new_size)).map_err(map)?;
            ctx.modified.store(true, Relaxed);
        }
        fill(file_info, &self.attr(ctx)?);
        Ok(())
    }

    fn set_delete(&self, ctx: &VCtx, _file_name: &U16CStr, delete_file: bool) -> winfsp::Result<()> {
        if delete_file && ctx.is_dir {
            let path = ctx.path.lock().clone();
            if !self.vol.readdir(&path).map_err(map)?.is_empty() {
                return Err(nt(NOT_EMPTY));
            }
        }
        Ok(())
    }

    fn rename(&self, ctx: &VCtx, _file_name: &U16CStr, new_file_name: &U16CStr, replace_if_exists: bool) -> winfsp::Result<()> {
        let old = ctx.path.lock().clone();
        let new = norm(new_file_name);
        self.vol.rename(&old, &new, replace_if_exists).map_err(map)?;
        *ctx.path.lock() = new;
        Ok(())
    }

    fn read(&self, ctx: &VCtx, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        self.rt.block_on(self.read_async(ctx, buffer, offset))
    }

    fn write(&self, ctx: &VCtx, buffer: &[u8], offset: u64, wte: bool, cio: bool, file_info: &mut FileInfo) -> winfsp::Result<u32> {
        self.rt.block_on(self.write_async(ctx, buffer, offset, wte, cio, file_info))
    }

    fn read_directory(&self, ctx: &VCtx, _pattern: Option<&U16CStr>, marker: DirMarker, buffer: &mut [u8]) -> winfsp::Result<u32> {
        if !ctx.is_dir {
            return Err(nt(NOT_DIR));
        }
        if let Ok(lock) = ctx.buf.acquire(marker.is_none(), None) {
            let path = ctx.path.lock().clone();
            let mut entries = self.vol.readdir(&path).map_err(map)?;
            entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            let mut di = DirInfo::<255>::new();
            if !path.is_empty() {
                let me = self.attr(ctx)?;
                for dot in [".", ".."] {
                    di.reset();
                    fill(di.file_info_mut(), &me);
                    di.set_name(dot)?;
                    lock.write(&mut di)?;
                }
            }
            for (name, a) in &entries {
                di.reset();
                fill(di.file_info_mut(), a);
                di.set_name(name)?;
                lock.write(&mut di)?;
            }
        }
        Ok(ctx.buf.read(marker, buffer))
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, free) = tokio::task::block_in_place(|| self.rt.block_on(self.vol.space()));
        out.total_size = total;
        out.free_size = free;
        out.set_volume_label("streamfs");
        Ok(())
    }
}

impl AsyncFileSystemContext for VolFs {
    fn read_async(&self, ctx: &VCtx, buffer: &mut [u8], offset: u64) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move {
            if ctx.is_dir {
                return Err(nt(IS_DIR));
            }
            let size = self.attr(ctx)?.size;
            if offset >= size {
                return Err(nt(END_OF_FILE));
            }
            let data = self.vol.read(ctx.ino, offset, buffer.len()).await.map_err(map)?;
            buffer[..data.len()].copy_from_slice(&data);
            Ok(data.len() as u32)
        }
    }

    fn write_async(
        &self,
        ctx: &VCtx,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move {
            if ctx.is_dir {
                return Err(nt(IS_DIR));
            }
            // EOF and the no-extend limit are resolved inside the volume under its writer
            // lock, so concurrent appenders cannot collide. May wait for uploads to free
            // cache space (backpressure) without blocking a WinFsp thread.
            let n = self.vol.write_opts(ctx.ino, offset, buffer, write_to_eof, constrained_io).await.map_err(map)?;
            ctx.modified.store(true, Relaxed);
            fill(file_info, &self.attr(ctx)?);
            Ok(n as u32)
        }
    }

    fn read_directory_async(
        &self,
        ctx: &VCtx,
        pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> impl std::future::Future<Output = winfsp::Result<u32>> + Send {
        async move { Self::read_directory(self, ctx, pattern, marker, buffer) }
    }

    fn spawn_task(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.rt.spawn(future);
    }
}

/// Seconds to wait for pending uploads at unmount before giving up (data stays local and resumes next mount).
pub const UNMOUNT_SYNC_WAIT: Duration = Duration::from_secs(30);
