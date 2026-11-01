//! Google Drive backend (Drive API v3, read-only).
//!
//! Partial reads use `files/{id}?alt=media` with an HTTP `Range` header
//! (documented for binary files; Google-native Docs/Sheets cannot be ranged and
//! are rejected). A response that ignores Range is NEVER read in full: the
//! declared Content-Length is checked before the body is consumed.

pub mod auth;

use async_trait::async_trait;
use auth::TokenProvider;
use bytes::Bytes;
use parking_lot::Mutex;
use serde::Deserialize;
use sfs_core::{Backend, BackendKind, Error, FileMeta, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,version";

/// Everything we send to Google, so quota use is measured, not guessed.
#[derive(Default)]
pub struct Metrics {
    pub metadata_requests: AtomicU64,
    pub media_requests: AtomicU64,
    pub media_bytes: AtomicU64,
    pub http_403: AtomicU64,
    pub http_429: AtomicU64,
    pub http_5xx: AtomicU64,
    pub http_401: AtomicU64,
    pub upload_requests: AtomicU64,
    pub upload_bytes: AtomicU64,
}

pub struct DriveBackend {
    http: reqwest::Client,
    tokens: Arc<dyn TokenProvider>,
    api: String,
    root_id: String,
    /// (parent id, name) -> child id. Names are not unique in Drive; ambiguity is an error.
    children: Mutex<HashMap<(String, String), String>>,
    pub metrics: Metrics,
}

#[derive(Deserialize)]
struct DriveFile {
    id: String,
    name: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    size: Option<String>,
    #[serde(rename = "md5Checksum")]
    md5: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next: Option<String>,
}

fn transient(msg: impl Into<String>) -> Error {
    Error::backend(BackendKind::Transient, msg)
}
fn fatal(msg: impl Into<String>) -> Error {
    Error::backend(BackendKind::Fatal, msg)
}

fn map_transport(e: reqwest::Error) -> Error {
    transient(format!("network: {}", e.without_url()))
}

/// Translate an HTTP error into the engine's error taxonomy.
pub fn map_status(status: u16, retry_after: Option<Duration>, body: &str) -> Error {
    let reasons: Vec<String> = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .map(|v| {
            let mut r: Vec<String> = v["error"]["errors"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["reason"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if let Some(s) = v["error"]["status"].as_str() {
                r.push(s.to_string());
            }
            r
        })
        .unwrap_or_default();
    let has = |names: &[&str]| reasons.iter().any(|r| names.iter().any(|n| r.eq_ignore_ascii_case(n)));
    let msg = format!("HTTP {status} {}", reasons.join(","));
    let mk = |kind| Error::Backend { kind, msg: msg.clone(), retry_after };
    match status {
        429 => mk(BackendKind::RateLimited),
        403 if has(&["userRateLimitExceeded", "rateLimitExceeded", "sharingRateLimitExceeded", "RESOURCE_EXHAUSTED"]) => {
            mk(BackendKind::RateLimited)
        }
        403 if has(&["downloadQuotaExceeded", "dailyLimitExceeded", "quotaExceeded", "dailyLimitExceededUnreg"]) => {
            mk(BackendKind::Quota)
        }
        404 => mk(BackendKind::NotFound),
        408 | 500..=599 => mk(BackendKind::Transient),
        _ => mk(BackendKind::Fatal),
    }
}

impl DriveBackend {
    pub fn new(tokens: Arc<dyn TokenProvider>, root_id: &str) -> Self {
        Self::with_api_base(tokens, root_id, "https://www.googleapis.com")
    }

    pub fn with_api_base(tokens: Arc<dyn TokenProvider>, root_id: &str, api: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .build()
            .expect("http client");
        DriveBackend {
            http,
            tokens,
            api: api.trim_end_matches('/').to_string(),
            root_id: root_id.to_string(),
            children: Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
        }
    }

    async fn get(&self, url: &str, query: &[(&str, String)], range: Option<String>, media: bool) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let token = self.tokens.access_token().await?;
            let mut rb = self.http.get(url).bearer_auth(token).query(query);
            if let Some(r) = &range {
                rb = rb.header(reqwest::header::RANGE, r);
            }
            if media {
                self.metrics.media_requests.fetch_add(1, Relaxed);
            } else {
                self.metrics.metadata_requests.fetch_add(1, Relaxed);
            }
            let resp = rb.send().await.map_err(map_transport)?;
            let st = resp.status().as_u16();
            match st {
                401 => {
                    self.metrics.http_401.fetch_add(1, Relaxed);
                    if attempt == 0 {
                        self.tokens.invalidate().await;
                        continue;
                    }
                }
                403 => {
                    self.metrics.http_403.fetch_add(1, Relaxed);
                }
                429 => {
                    self.metrics.http_429.fetch_add(1, Relaxed);
                }
                500..=599 => {
                    self.metrics.http_5xx.fetch_add(1, Relaxed);
                }
                _ => {}
            }
            return Ok(resp);
        }
        Err(fatal("authorization failed twice (401)"))
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.text().await.unwrap_or_default();
        Err(map_status(status, retry_after, &body.chars().take(2000).collect::<String>()))
    }

    async fn file_by_id(&self, id: &str) -> Result<DriveFile> {
        let url = format!("{}/drive/v3/files/{id}", self.api);
        let q = [("fields", FILE_FIELDS.to_string()), ("supportsAllDrives", "true".into())];
        let resp = Self::check(self.get(&url, &q, None, false).await?).await?;
        resp.json().await.map_err(|e| fatal(format!("bad file json: {e}")))
    }

    /// Stat a file by Drive id (1 metadata request); path is just "/<name>".
    pub async fn stat_id(&self, id: &str) -> Result<FileMeta> {
        let f = self.file_by_id(id).await?;
        let name = f.name.clone().unwrap_or_else(|| id.to_string());
        Self::to_meta(&format!("/{name}"), &f)
    }

    async fn list_children(&self, parent: &str, name: Option<&str>) -> Result<Vec<DriveFile>> {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
        let mut q = format!("'{}' in parents and trashed=false", esc(parent));
        if let Some(n) = name {
            q.push_str(&format!(" and name='{}'", esc(n)));
        }
        let url = format!("{}/drive/v3/files", self.api);
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut query = vec![
                ("q", q.clone()),
                ("fields", format!("nextPageToken,files({FILE_FIELDS})")),
                ("pageSize", "1000".into()),
                ("supportsAllDrives", "true".into()),
                ("includeItemsFromAllDrives", "true".into()),
            ];
            if let Some(p) = &page {
                query.push(("pageToken", p.clone()));
            }
            let resp = Self::check(self.get(&url, &query, None, false).await?).await?;
            let l: FileList = resp.json().await.map_err(|e| fatal(format!("bad list json: {e}")))?;
            out.extend(l.files);
            match l.next {
                Some(p) => page = Some(p),
                None => return Ok(out),
            }
        }
    }

    /// Resolve "/a/b/c" to the Drive file for c (one metadata request per uncached component).
    async fn resolve(&self, path: &str) -> Result<DriveFile> {
        let mut cur = self.root_id.clone();
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return self.file_by_id(&cur).await;
        }
        let mut last = None;
        for (i, c) in comps.iter().enumerate() {
            let cached = self.children.lock().get(&(cur.clone(), c.to_string())).cloned();
            if let (Some(id), false) = (&cached, i + 1 == comps.len()) {
                cur = id.clone();
                continue;
            }
            let found = self.list_children(&cur, Some(c)).await?;
            let f = match found.len() {
                0 => return Err(Error::backend(BackendKind::NotFound, format!("{path}: '{c}' not found"))),
                1 => found.into_iter().next().unwrap(),
                n => return Err(fatal(format!("{path}: {n} entries named '{c}' in the same folder; rename one"))),
            };
            self.children.lock().insert((cur.clone(), c.to_string()), f.id.clone());
            cur = f.id.clone();
            last = Some(f);
        }
        Ok(last.expect("non-empty path"))
    }

    fn to_meta(path: &str, f: &DriveFile) -> Result<FileMeta> {
        let is_dir = f.mime_type.as_deref() == Some(FOLDER_MIME);
        if !is_dir && f.mime_type.as_deref().map_or(false, |m| m.starts_with("application/vnd.google-apps.")) {
            return Err(fatal(format!("{path}: Google-native document, not a downloadable binary file")));
        }
        let size = match (&f.size, is_dir) {
            (Some(s), _) => s.parse::<u64>().map_err(|_| fatal("bad size"))?,
            (None, true) => 0,
            (None, false) => return Err(fatal(format!("{path}: Drive reports no size"))),
        };
        Ok(FileMeta {
            path: path.to_string(),
            id: f.id.clone(),
            size,
            version: format!("{}-{}", f.md5.clone().unwrap_or_default(), f.version.clone().unwrap_or_default()),
            is_dir,
        })
    }
}

#[async_trait]
impl Backend for DriveBackend {
    fn name(&self) -> &str {
        "gdrive"
    }

    async fn stat(&self, path: &str) -> Result<FileMeta> {
        let f = self.resolve(path).await?;
        // The cached id may predate an edit: always re-read metadata for the final component.
        let f = if path.split('/').any(|c| !c.is_empty()) { self.file_by_id(&f.id).await? } else { f };
        Self::to_meta(path, &f)
    }

    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        let d = self.resolve(dir).await?;
        let base = dir.trim_end_matches('/');
        let mut out = Vec::new();
        for f in self.list_children(&d.id, None).await? {
            let name = f.name.clone().unwrap_or_default();
            self.children.lock().insert((d.id.clone(), name.clone()), f.id.clone());
            if let Ok(m) = Self::to_meta(&format!("{base}/{name}"), &f) {
                out.push(m); // Google-native docs are skipped, not listed as playable
            }
        }
        Ok(out)
    }

    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        if offset >= meta.size {
            return Ok(Bytes::new());
        }
        let len = len.min(meta.size - offset);
        let url = format!("{}/drive/v3/files/{}", self.api, meta.id);
        let q = [("alt", "media".to_string()), ("supportsAllDrives", "true".into())];
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let resp = Self::check(self.get(&url, &q, Some(range), true).await?).await?;

        let status = resp.status().as_u16();
        let clen = resp.content_length();
        match status {
            206 => {
                if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                    let want = format!("bytes {}-", offset);
                    if !cr.starts_with(&want) {
                        return Err(fatal(format!("Content-Range mismatch: asked {offset}, got '{cr}'")));
                    }
                }
            }
            200 => {
                // Range ignored => the body is the whole file. Refuse unless it is exactly what we asked.
                if offset != 0 || clen.map_or(true, |c| c != len) {
                    return Err(fatal("server ignored the Range header; refusing to download the whole file"));
                }
            }
            s => return Err(fatal(format!("unexpected HTTP {s} for ranged read"))),
        }
        if clen.map_or(false, |c| c != len) {
            return Err(fatal(format!("Content-Length {clen:?} != requested {len}")));
        }
        let body = resp.bytes().await.map_err(map_transport)?;
        if body.len() as u64 != len {
            // Connection died mid-body (or the server lied): retryable, never returned as valid data.
            return Err(transient(format!("short body: {} of {len}", body.len())));
        }
        self.metrics.media_bytes.fetch_add(len, Relaxed);
        Ok(body)
    }
}

/// Hard cap on bytes pulled through a backend. Used by probes/benchmarks so a
/// bug or a long run cannot burn through Drive's transfer quota.
/// Result of a verified upload.
#[derive(Debug, Clone)]
pub struct Uploaded {
    pub id: String,
    pub md5: String,
    pub size: u64,
}

/// Uploads need the `drive.file` scope: the app may create files and modify
/// only the files it created. Drive cannot rewrite a byte range of an existing
/// file, so every upload creates a whole, immutable object.
impl DriveBackend {
    const UPLOAD_CHUNK: usize = 16 << 20; // multiple of 256 KiB; one PUT per 16 MiB pack

    async fn send_authed(&self, mk: impl Fn(&str) -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let token = self.tokens.access_token().await?;
            self.metrics.upload_requests.fetch_add(1, Relaxed);
            let resp = mk(&token).send().await.map_err(map_transport)?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                self.tokens.invalidate().await;
                continue;
            }
            return Ok(resp);
        }
        Err(fatal("authorization failed twice (401)"))
    }

    /// Exactly one child named `name` (None if absent; error if ambiguous).
    pub async fn find_child(&self, parent: &str, name: &str) -> Result<Option<FileMeta>> {
        let found = self.list_children(parent, Some(name)).await?;
        match found.len() {
            0 => Ok(None),
            1 => Self::to_meta(&format!("/{name}"), &found[0]).map(Some),
            n => Err(fatal(format!("{n} entries named '{name}' in the same folder"))),
        }
    }

    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<String> {
        let url = format!("{}/drive/v3/files?fields=id&supportsAllDrives=true", self.api);
        let body = serde_json::json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]});
        let resp = Self::check(self.send_authed(|t| self.http.post(&url).bearer_auth(t).json(&body)).await?).await?;
        let v: serde_json::Value = resp.json().await.map_err(|e| fatal(format!("bad folder json: {e}")))?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| fatal("folder id missing"))
    }

    /// Resolve or create each component of `path` under `root_id`; returns the last folder id.
    pub async fn ensure_folder_path(&self, path: &str) -> Result<String> {
        let mut cur = self.root_id.clone();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = match self.find_child(&cur, c).await? {
                Some(m) if m.is_dir => m.id,
                Some(_) => return Err(fatal(format!("'{c}' exists and is not a folder"))),
                None => self.create_folder(&cur, c).await?,
            };
        }
        Ok(cur)
    }

    /// Resumable upload of `data` as a new file; resumes after network errors and
    /// verifies Drive's md5 against the local one before reporting success.
    pub async fn upload_bytes(&self, parent: &str, name: &str, data: &[u8]) -> Result<Uploaded> {
        use md5::{Digest, Md5};
        let local_md5 = format!("{:x}", Md5::digest(data));
        let size = data.len() as u64;
        let init = format!("{}/upload/drive/v3/files?uploadType=resumable&fields=id,md5Checksum,size&supportsAllDrives=true", self.api);
        let meta = serde_json::json!({"name": name, "parents": [parent]});
        let resp = Self::check(
            self.send_authed(|t| {
                self.http
                    .post(&init)
                    .bearer_auth(t)
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", size.to_string())
                    .json(&meta)
            })
            .await?,
        )
        .await?;
        let session = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| fatal("resumable upload: no session URI"))?;

        let mut off = 0u64;
        let mut failures = 0u32;
        loop {
            let (body, range) = if size == 0 {
                (Vec::new(), "bytes */0".to_string())
            } else {
                let end = (off + Self::UPLOAD_CHUNK as u64).min(size);
                (data[off as usize..end as usize].to_vec(), format!("bytes {}-{}/{}", off, end - 1, size))
            };
            let sent = body.len() as u64;
            let r = self
                .send_authed(|t| self.http.put(&session).bearer_auth(t).header(reqwest::header::CONTENT_RANGE, &range).body(body.clone()))
                .await;
            let resp = match r {
                Ok(resp) => resp,
                Err(_) if failures < 6 => {
                    // Network error: ask the session how much it has, then continue from there.
                    failures += 1;
                    tokio::time::sleep(Duration::from_millis(500 << failures)).await;
                    match self.query_upload_offset(&session, size).await? {
                        Some(o) => {
                            off = o;
                            continue;
                        }
                        None => break, // already complete: fall through to verification via find
                    }
                }
                Err(e) => return Err(e),
            };
            match resp.status().as_u16() {
                200 | 201 => {
                    let v: serde_json::Value = resp.json().await.map_err(|e| fatal(format!("bad upload json: {e}")))?;
                    let md5 = v["md5Checksum"].as_str().unwrap_or_default().to_string();
                    if md5 != local_md5 {
                        return Err(fatal(format!("upload of {name}: md5 mismatch (local {local_md5}, Drive {md5})")));
                    }
                    self.metrics.upload_bytes.fetch_add(size, Relaxed);
                    return Ok(Uploaded { id: v["id"].as_str().unwrap_or_default().to_string(), md5, size });
                }
                308 => {
                    off = Self::range_end(&resp).unwrap_or(off + sent);
                }
                s if (s == 429 || s >= 500) && failures < 6 => {
                    failures += 1;
                    tokio::time::sleep(Duration::from_millis(500 << failures)).await;
                    if let Some(o) = self.query_upload_offset(&session, size).await? {
                        off = o;
                    }
                }
                _ => return Err(Self::check(resp).await.err().unwrap_or_else(|| fatal("upload failed"))),
            }
        }
        // The session reported completion while we were recovering: confirm by lookup.
        match self.find_child(parent, name).await? {
            Some(m) if m.size == size && m.version.starts_with(&local_md5) => Ok(Uploaded { id: m.id, md5: local_md5, size }),
            _ => Err(fatal(format!("upload of {name} could not be confirmed"))),
        }
    }

    /// `Some(next_offset)` if the session is still open, `None` if it already finished.
    async fn query_upload_offset(&self, session: &str, size: u64) -> Result<Option<u64>> {
        let resp = self
            .send_authed(|t| self.http.put(session).bearer_auth(t).header(reqwest::header::CONTENT_RANGE, format!("bytes */{size}")).body(Vec::new()))
            .await?;
        match resp.status().as_u16() {
            308 => Ok(Some(Self::range_end(&resp).unwrap_or(0))),
            200 | 201 => Ok(None),
            _ => Err(Self::check(resp).await.err().unwrap_or_else(|| fatal("upload status query failed"))),
        }
    }

    /// Next byte to send, from a 308 response's `Range: bytes=0-N` header.
    fn range_end(resp: &reqwest::Response) -> Option<u64> {
        let r = resp.headers().get(reqwest::header::RANGE)?.to_str().ok()?;
        r.rsplit('-').next()?.parse::<u64>().ok().map(|n| n + 1)
    }

    /// Delete a file this app created (drive.file). Used for garbage collection of packs.
    pub async fn delete_file(&self, id: &str) -> Result<()> {
        let url = format!("{}/drive/v3/files/{id}?supportsAllDrives=true", self.api);
        let resp = self.send_authed(|t| self.http.delete(&url).bearer_auth(t)).await?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        Self::check(resp).await.map(|_| ())
    }
}

pub struct BudgetBackend {
    inner: Arc<dyn Backend>,
    budget: u64,
    used: AtomicU64,
}

impl BudgetBackend {
    pub fn new(inner: Arc<dyn Backend>, budget_bytes: u64) -> Self {
        BudgetBackend { inner, budget: budget_bytes, used: 0.into() }
    }
    pub fn used(&self) -> u64 {
        self.used.load(Relaxed)
    }
}

#[async_trait]
impl Backend for BudgetBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }
    async fn stat(&self, path: &str) -> Result<FileMeta> {
        self.inner.stat(path).await
    }
    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        self.inner.list(dir).await
    }
    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        let want = len.min(meta.size.saturating_sub(offset));
        if self.used.fetch_add(want, Relaxed) + want > self.budget {
            self.used.fetch_sub(want, Relaxed);
            return Err(fatal(format!("transfer budget of {} bytes exhausted; stopping on purpose", self.budget)));
        }
        self.inner.read_range(meta, offset, len).await
    }
}

impl DriveBackend {
    /// Free bytes in the account's Drive quota (None if unlimited / not reported).
    pub async fn storage_free(&self) -> Result<Option<u64>> {
        let url = format!("{}/drive/v3/about", self.api);
        let q = [("fields", "storageQuota(limit,usage)".to_string())];
        let resp = Self::check(self.get(&url, &q, None, false).await?).await?;
        let v: serde_json::Value = resp.json().await.map_err(|e| fatal(format!("bad about json: {e}")))?;
        let num = |k: &str| v["storageQuota"][k].as_str().and_then(|s| s.parse::<u64>().ok());
        Ok(match (num("limit"), num("usage")) {
            (Some(l), Some(u)) => Some(l.saturating_sub(u)),
            _ => None,
        })
    }
}

/// Object store for the write-back volume: each pack is a new, immutable Drive file
/// in an app-created folder (drive.file scope). Writes are paced so sustained
/// creation stays under Google's ~3 write requests/s per account.
pub struct DriveObjectStore {
    drive: Arc<DriveBackend>,
    folder: String,
    min_gap: Duration,
    last: tokio::sync::Mutex<std::time::Instant>,
}

impl DriveObjectStore {
    pub fn new(drive: Arc<DriveBackend>, folder_id: &str, min_gap_between_objects: Duration) -> Self {
        DriveObjectStore {
            drive,
            folder: folder_id.to_string(),
            min_gap: min_gap_between_objects,
            last: tokio::sync::Mutex::new(std::time::Instant::now() - Duration::from_secs(60)),
        }
    }
    pub fn drive(&self) -> &Arc<DriveBackend> {
        &self.drive
    }
}

#[async_trait]
impl Backend for DriveObjectStore {
    fn name(&self) -> &str {
        "gdrive-obj"
    }
    async fn stat(&self, path: &str) -> Result<FileMeta> {
        self.drive.stat(path).await
    }
    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        self.drive.list(dir).await
    }
    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        self.drive.read_range(meta, offset, len).await
    }
}

#[async_trait]
impl sfs_core::ObjectStore for DriveObjectStore {
    async fn put_object(&self, name: &str, data: Bytes) -> Result<sfs_core::ObjRef> {
        {
            let mut last = self.last.lock().await;
            let next = *last + self.min_gap;
            let now = std::time::Instant::now();
            if next > now {
                tokio::time::sleep(next - now).await;
            }
            *last = std::time::Instant::now();
        }
        let u = self.drive.upload_bytes(&self.folder, name, &data).await?;
        Ok(sfs_core::ObjRef { id: u.id, size: u.size, md5: u.md5 })
    }
    async fn delete_object(&self, obj: &sfs_core::ObjRef) -> Result<()> {
        self.drive.delete_file(&obj.id).await
    }
    async fn remote_free_bytes(&self) -> Result<Option<u64>> {
        self.drive.storage_free().await
    }
    async fn list_named(&self, prefix: &str) -> Result<Vec<(String, sfs_core::ObjRef)>> {
        let mut out: Vec<(String, sfs_core::ObjRef)> = self
            .drive
            .list_children(&self.folder, None)
            .await?
            .into_iter()
            .filter_map(|f| {
                let name = f.name.clone()?;
                if !name.starts_with(prefix) {
                    return None;
                }
                let size = f.size.as_deref()?.parse().ok()?;
                Some((name, sfs_core::ObjRef { id: f.id, size, md5: f.md5.unwrap_or_default() }))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }
}
