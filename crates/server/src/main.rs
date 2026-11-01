//! sfs-server: minimal storage service for StreamFS on a VPS.
//!
//! Security model: it listens on 127.0.0.1 only and is reached through an SSH
//! tunnel (encryption + key authentication by SSH); a bearer token adds a second
//! check so other local users cannot use it. Nothing is exposed publicly.
//!
//! API (all paths are relative to the configured roots; `..` is rejected):
//!   GET    /v1/stat/<path>             -> {size, version, is_dir}
//!   GET    /v1/list/<path>             -> [{name, size, version, is_dir}]
//!   GET    /v1/file/<path>   + Range   -> 206 exact bytes (game files, read-only)
//!   PUT    /v1/obj/<vol>/<name>        -> {id, size, md5}  (atomic: tmp + fsync + rename)
//!   GET    /v1/obj/<vol>/<id> + Range  -> 206 exact bytes
//!   DELETE /v1/obj/<vol>/<id>
//!   GET    /v1/objs/<vol>?prefix=P     -> [{name, id, size, md5}]  (md5 only for small objects)
//!   GET    /v1/space                   -> {free, total}
//!   GET    /v1/zstats                  -> wire-compression counters
//!   Range reads with `x-sfs-accept: sfz1` may answer with an sfz1 frame stream (`x-sfs-enc: sfz1`).
//!
//! uso: sfs-server --files DIR --objects DIR --token-file FILE [--listen 127.0.0.1:8787]

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

struct App {
    files: PathBuf,
    objects: PathBuf,
    token: String,
    seq: AtomicU64,
    z: Option<Arc<Z>>,
}

type Shared = Arc<App>;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, msg.into()).into_response()
}

/// Join a client-supplied relative path under `base`, refusing anything that escapes it.
fn safe_join(base: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in std::path::Path::new(rel.trim_start_matches('/')).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.len() < 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.')
}

fn version_of(md: &std::fs::Metadata) -> String {
    let mt = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{mt}", md.len())
}

async fn auth(State(app): State<Shared>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("x-sfs-token")
        .and_then(|v| v.to_str().ok())
        .map(|t| {
            // constant-time comparison
            let (a, b) = (t.as_bytes(), app.token.as_bytes());
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
        })
        .unwrap_or(false);
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "bad token");
    }
    next.run(req).await
}

#[derive(Serialize)]
struct Entry {
    name: String,
    size: u64,
    version: String,
    is_dir: bool,
}

async fn stat(State(app): State<Shared>, Path(p): Path<String>) -> Response {
    let Some(path) = safe_join(&app.files, &p) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    match tokio::fs::metadata(&path).await {
        Ok(md) => Json(serde_json::json!({"size": if md.is_dir() {0} else {md.len()}, "version": version_of(&md), "is_dir": md.is_dir()})).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn stat_root(state: State<Shared>) -> Response {
    stat(state, Path(String::new())).await
}

async fn list(State(app): State<Shared>, Path(p): Path<String>) -> Response {
    let Some(path) = safe_join(&app.files, &p) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)?.flatten() {
            if let Ok(md) = e.metadata() {
                out.push(Entry { name: e.file_name().to_string_lossy().to_string(), size: if md.is_dir() { 0 } else { md.len() }, version: version_of(&md), is_dir: md.is_dir() });
            }
        }
        Ok(out)
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        _ => err(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn list_root(state: State<Shared>) -> Response {
    list(state, Path(String::new())).await
}

fn parse_range(h: &HeaderMap, size: u64) -> Option<(u64, u64)> {
    let r = h.get(header::RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { size.saturating_sub(1) } else { b.parse().ok()? };
    (a <= b && a < size).then_some((a, b.min(size - 1)))
}

#[cfg(unix)]
fn pread(f: &std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(windows)]
fn pread(f: &std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}

/// Wire compression (opt-in with `x-sfs-accept: sfz1`). The range is cut at absolute 256 KiB
/// boundaries (= the client engine's block size, so its aligned fetches are all cacheable);
/// each piece becomes one sfz1 frame (zstd-1, or RAW when it gains < 5%). Measured on Project
/// Zomboid, 256 KiB chunks compress as well as 1 MiB ones (0.514 vs 0.522).
/// Whole chunks are cached compressed (keyed by file version) so repeated reads cost no CPU.
/// Only one thread compresses at a time and nobody waits for it: if the permit is busy, the
/// piece goes out RAW. Compression can make a read cheaper, never slower to start.
const ZCHUNK: u64 = 256 << 10;
const ZMIN_PIECE: usize = 64 << 10;

enum ZEntry {
    Frame(Arc<Vec<u8>>),
    Incompressible,
}

#[derive(Default)]
struct ZCache {
    map: HashMap<(PathBuf, String, u64), (ZEntry, u64)>,
    order: std::collections::BTreeMap<u64, (PathBuf, String, u64)>,
    tick: u64,
    bytes: usize,
    cap: usize,
}

impl ZCache {
    fn get(&mut self, k: &(PathBuf, String, u64)) -> Option<Option<Arc<Vec<u8>>>> {
        self.tick += 1;
        let t = self.tick;
        let (e, old) = self.map.get_mut(k)?;
        let r = match e {
            ZEntry::Frame(f) => Some(f.clone()),
            ZEntry::Incompressible => None,
        };
        let old = std::mem::replace(old, t);
        if let Some(key) = self.order.remove(&old) {
            self.order.insert(t, key);
        }
        Some(r)
    }
    fn put(&mut self, k: (PathBuf, String, u64), e: ZEntry) {
        let sz = match &e { ZEntry::Frame(f) => f.len(), ZEntry::Incompressible => 0 } + 64;
        if sz > self.cap {
            return;
        }
        self.tick += 1;
        if let Some((old, t)) = self.map.insert(k.clone(), (e, self.tick)) {
            self.order.remove(&t);
            self.bytes -= match old { ZEntry::Frame(f) => f.len(), ZEntry::Incompressible => 0 } + 64;
        }
        self.order.insert(self.tick, k);
        self.bytes += sz;
        while self.bytes > self.cap {
            let Some((_, key)) = self.order.pop_first() else { break };
            if let Some((old, _)) = self.map.remove(&key) {
                self.bytes -= match old { ZEntry::Frame(f) => f.len(), ZEntry::Incompressible => 0 } + 64;
            }
        }
    }
}

#[derive(Default)]
struct ZStats {
    pieces: AtomicU64,
    hits: AtomicU64,
    compressed: AtomicU64,
    raw_fallback: AtomicU64,
    busy_raw: AtomicU64,
    raw_bytes: AtomicU64,
    wire_bytes: AtomicU64,
}

struct Z {
    cache: std::sync::Mutex<ZCache>,
    permit: std::sync::Mutex<()>,
    stats: ZStats,
}

fn wants_sfz(h: &HeaderMap) -> bool {
    h.get("x-sfs-accept").and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == "sfz1"))
}

/// Builds the frame stream for `buf` = bytes [a, a+len) of the file identified by (path, version).
fn sfz_encode(z: &Z, path: &std::path::Path, version: &str, size: u64, a: u64, buf: &[u8]) -> Vec<u8> {
    use sfs_codec::{encode, Algo};
    let mut out = Vec::with_capacity(buf.len() / 2 + 64);
    let mut pos = 0usize;
    while pos < buf.len() {
        let abs = a + pos as u64;
        let end = (((abs / ZCHUNK) + 1) * ZCHUNK - a).min(buf.len() as u64) as usize;
        let piece = &buf[pos..end];
        // a full chunk, or the file's last chunk read up to EOF (small files are common in games)
        let whole = abs % ZCHUNK == 0 && (piece.len() as u64 == ZCHUNK || abs + piece.len() as u64 == size);
        let key = (path.to_path_buf(), version.to_string(), abs / ZCHUNK);
        z.stats.pieces.fetch_add(1, Relaxed);
        z.stats.raw_bytes.fetch_add(piece.len() as u64, Relaxed);
        let before = out.len();
        let cached = if whole { z.cache.lock().unwrap().get(&key) } else { None };
        match cached {
            Some(Some(frame)) => {
                z.stats.hits.fetch_add(1, Relaxed);
                out.extend_from_slice(&frame);
            }
            Some(None) => {
                z.stats.hits.fetch_add(1, Relaxed);
                out.extend(encode(Algo::Raw, piece, 0.0).0);
            }
            None if piece.len() < ZMIN_PIECE => out.extend(encode(Algo::Raw, piece, 0.0).0),
            None => match z.permit.try_lock() {
                Ok(_g) => {
                    let (frame, used) = encode(Algo::Zstd(1), piece, 0.05);
                    if used == Algo::Raw {
                        z.stats.raw_fallback.fetch_add(1, Relaxed);
                        if whole {
                            z.cache.lock().unwrap().put(key, ZEntry::Incompressible);
                        }
                    } else {
                        z.stats.compressed.fetch_add(1, Relaxed);
                        if whole {
                            z.cache.lock().unwrap().put(key, ZEntry::Frame(Arc::new(frame.clone())));
                        }
                    }
                    out.extend(frame);
                }
                Err(_) => {
                    z.stats.busy_raw.fetch_add(1, Relaxed);
                    out.extend(encode(Algo::Raw, piece, 0.0).0);
                }
            },
        }
        z.stats.wire_bytes.fetch_add((out.len() - before) as u64, Relaxed);
        pos = end;
    }
    out
}

/// Serve exactly the requested range (never the whole file unless asked without Range).
/// With `x-sfs-accept: sfz1` and a Range, the body is an sfz1 frame stream (`x-sfs-enc: sfz1`);
/// Content-Range always describes the raw bytes.
async fn serve_range(path: PathBuf, headers: HeaderMap, z: Option<Arc<Z>>) -> Response {
    let sfz = z.filter(|_| wants_sfz(&headers) && headers.contains_key(header::RANGE));
    // Client pins the version it opened; a changed file is refused, never served mixed.
    let want_ver = headers.get("x-sfs-version").and_then(|v| v.to_str().ok()).map(str::to_string);
    let t_all = std::time::Instant::now();
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, Option<(u64, u64)>, Vec<u8>, u64, bool, u64)> {
        let f = std::fs::File::open(&path)?;
        let md = f.metadata()?;
        let size = md.len();
        if want_ver.as_deref().is_some_and(|v| v != version_of(&md)) {
            return Err(std::io::Error::other(VERSION_MISMATCH));
        }
        let range = parse_range(&headers, size);
        let (a, b) = range.unwrap_or((0, size.saturating_sub(1)));
        let len = if size == 0 { 0 } else { (b - a + 1) as usize };
        if range.is_none() && len > (64 << 20) {
            return Err(std::io::Error::other("refusing full read of a large file without Range"));
        }
        let mut buf = vec![0u8; len];
        let t_read = std::time::Instant::now();
        let mut done = 0;
        while done < len {
            let n = pread(&f, &mut buf[done..], a + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        let read_us = t_read.elapsed().as_micros() as u64;
        // modified while we read it: the bytes may mix two versions
        if want_ver.is_some() && std::fs::metadata(&path).map(|m| version_of(&m)).ok() != Some(version_of(&md)) {
            return Err(std::io::Error::other(VERSION_MISMATCH));
        }
        let raw_len = buf.len() as u64;
        match (&sfz, range) {
            (Some(z), Some((a, _))) if raw_len > 0 => Ok((size, range, sfz_encode(z, &path, &version_of(&md), size, a, &buf), raw_len, true, read_us)),
            _ => Ok((size, range, buf, raw_len, false, read_us)),
        }
    })
    .await;
    match res {
        Ok(Ok((size, Some((a, _)), buf, raw_len, enc, read_us))) => {
            let end = a + raw_len - 1;
            let mut r = (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_RANGE, format!("bytes {a}-{end}/{size}")), (header::CONTENT_TYPE, if enc { "application/x-sfz1" } else { "application/octet-stream" }.into())], buf).into_response();
            if enc {
                r.headers_mut().insert("x-sfs-enc", header::HeaderValue::from_static("sfz1"));
            }
            // timing for the client's latency breakdown: disk read, and all server-side work
            // (blocking-pool wait + read + encode) before the body starts
            let h = r.headers_mut();
            h.insert("x-sfs-read-us", header::HeaderValue::from(read_us));
            h.insert("x-sfs-srv-us", header::HeaderValue::from(t_all.elapsed().as_micros() as u64));
            r
        }
        Ok(Ok((_, None, buf, _, _, _))) => (StatusCode::OK, buf).into_response(),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => err(StatusCode::NOT_FOUND, "not found"),
        Ok(Err(e)) if e.to_string() == VERSION_MISMATCH => err(StatusCode::PRECONDITION_FAILED, VERSION_MISMATCH),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

const VERSION_MISMATCH: &str = "file version changed";

async fn file(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap) -> Response {
    let Some(path) = safe_join(&app.files, &p) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    serve_range(path, headers, app.z.clone()).await
}

async fn zstats(State(app): State<Shared>) -> Response {
    let Some(z) = &app.z else { return Json(serde_json::json!({"enabled": false})).into_response() };
    let s = &z.stats;
    let (entries, bytes, cap) = {
        let c = z.cache.lock().unwrap();
        (c.map.len(), c.bytes, c.cap)
    };
    Json(serde_json::json!({
        "enabled": true, "pieces": s.pieces.load(Relaxed), "cache_hits": s.hits.load(Relaxed),
        "compressed": s.compressed.load(Relaxed), "raw_fallback": s.raw_fallback.load(Relaxed),
        "busy_raw": s.busy_raw.load(Relaxed), "raw_bytes": s.raw_bytes.load(Relaxed), "wire_bytes": s.wire_bytes.load(Relaxed),
        "cache_entries": entries, "cache_bytes": bytes, "cache_cap": cap,
    }))
    .into_response()
}

async fn obj_get(State(app): State<Shared>, Path((vol, id)): Path<(String, String)>, headers: HeaderMap) -> Response {
    if !safe_name(&vol) || !safe_name(&id) {
        return err(StatusCode::BAD_REQUEST, "bad name");
    }
    serve_range(app.objects.join(vol).join(id), headers, app.z.clone()).await
}

async fn obj_put(State(app): State<Shared>, Path((vol, name)): Path<(String, String)>, body: Bytes) -> Response {
    if !safe_name(&vol) || !safe_name(&name) {
        return err(StatusCode::BAD_REQUEST, "bad name");
    }
    let dir = app.objects.join(&vol);
    let seq = app.seq.fetch_add(1, Relaxed);
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<serde_json::Value> {
        use md5::{Digest, Md5};
        use std::io::Write;
        std::fs::create_dir_all(&dir)?;
        let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let id = format!("{nanos:x}{seq:x}-{name}");
        let tmp = dir.join(format!(".tmp-{id}"));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?; // durable before it becomes visible
        }
        std::fs::rename(&tmp, dir.join(&id))?;
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all(); // persist the rename (directory entry) on Linux
        }
        let md5 = format!("{:x}", Md5::digest(&body));
        Ok(serde_json::json!({"id": id, "size": body.len(), "md5": md5}))
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err(StatusCode::INSUFFICIENT_STORAGE, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn obj_delete(State(app): State<Shared>, Path((vol, id)): Path<(String, String)>) -> Response {
    if !safe_name(&vol) || !safe_name(&id) {
        return err(StatusCode::BAD_REQUEST, "bad name");
    }
    match tokio::fs::remove_file(app.objects.join(vol).join(id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn obj_list(State(app): State<Shared>, Path(vol): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    if !safe_name(&vol) {
        return err(StatusCode::BAD_REQUEST, "bad name");
    }
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let dir = app.objects.join(vol);
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<serde_json::Value>> {
        use md5::{Digest, Md5};
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(out) };
        for e in rd.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            if id.starts_with('.') {
                continue;
            }
            let Some((_, name)) = id.split_once('-') else { continue };
            if !name.starts_with(&prefix) {
                continue;
            }
            let size = e.metadata()?.len();
            let md5 = if size <= (64 << 20) { format!("{:x}", Md5::digest(std::fs::read(e.path())?)) } else { String::new() };
            out.push(serde_json::json!({"name": name, "id": id, "size": size, "md5": md5}));
        }
        Ok(out)
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, "list failed"),
    }
}

async fn space(State(app): State<Shared>) -> Response {
    let free = fs2::available_space(&app.objects).unwrap_or(0);
    let total = fs2::total_space(&app.objects).unwrap_or(0);
    Json(serde_json::json!({"free": free, "total": total})).into_response()
}

#[derive(Deserialize)]
struct _Unused;

#[tokio::main]
async fn main() {
    let files = PathBuf::from(arg("--files").expect("--files DIR"));
    let objects = PathBuf::from(arg("--objects").expect("--objects DIR"));
    std::fs::create_dir_all(&files).expect("files dir");
    std::fs::create_dir_all(&objects).expect("objects dir");
    let token = std::fs::read_to_string(arg("--token-file").expect("--token-file FILE")).expect("token file").trim().to_string();
    assert!(token.len() >= 32, "token too short");
    let listen = arg("--listen").unwrap_or_else(|| "127.0.0.1:8787".into());
    assert!(listen.starts_with("127.0.0.1:") || listen.starts_with("[::1]:") || std::env::args().any(|a| a == "--i-know-this-is-public"), "refusing to listen on a non-loopback address");
    // --zcache-mb 0 disables wire compression entirely (clients then always get raw bytes)
    let zmb: usize = arg("--zcache-mb").and_then(|v| v.parse().ok()).unwrap_or(256);
    let z = (zmb > 0).then(|| Arc::new(Z { cache: std::sync::Mutex::new(ZCache { cap: zmb << 20, ..Default::default() }), permit: std::sync::Mutex::new(()), stats: ZStats::default() }));
    let app = Arc::new(App { files, objects, token, seq: AtomicU64::new(0), z });
    let router = Router::new()
        .route("/v1/stat/", get(stat_root))
        .route("/v1/stat/{*p}", get(stat))
        .route("/v1/list/", get(list_root))
        .route("/v1/list/{*p}", get(list))
        .route("/v1/file/{*p}", get(file))
        .route("/v1/obj/{vol}/{id}", put(obj_put).get(obj_get).delete(obj_delete))
        .route("/v1/objs/{vol}", get(obj_list))
        .route("/v1/space", get(space))
        .route("/v1/zstats", get(zstats))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .layer(DefaultBodyLimit::max(128 << 20))
        .with_state(app);
    let l = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    println!("sfs-server escuchando en {listen}");
    axum::serve(l, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await.unwrap();
}
