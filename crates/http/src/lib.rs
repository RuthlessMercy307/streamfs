//! Backend + ObjectStore for `sfs-server` (reached through an SSH tunnel to 127.0.0.1).
//! Read-only game trees use `/v1/file`, write-back volume packs use `/v1/obj`.

use async_trait::async_trait;
use bytes::Bytes;
use sfs_core::{Backend, BackendKind, Error, FileMeta, ObjRef, ObjectStore, Result};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

#[derive(Default)]
pub struct HttpMetrics {
    pub requests: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub errors: AtomicU64,
    /// bytes actually received for reads (compressed frames when sfz1 was used)
    pub wire_in: AtomicU64,
    pub decode_us: AtomicU64,
    /// Connection failures that marked a tunnel down (reconnects in progress).
    pub tunnel_down: AtomicU64,
    /// Reads refused because the file changed on the server since it was opened.
    pub version_mismatch: AtomicU64,
    pub urgent_lane: AtomicU64,
    /// Range reads split by size: [0] = up to 512 KiB, [1] = larger. Time from sending the
    /// request to the response headers (RTT + server work), time receiving the body, and the
    /// server's own read time when it reports it (x-sfs-read-us).
    pub ranges: [AtomicU64; 2],
    pub ttfb_us: [AtomicU64; 2],
    pub body_us: [AtomicU64; 2],
    pub server_us: [AtomicU64; 2],
    /// all server-side work before the body (blocking-pool wait + read + encode), x-sfs-srv-us
    pub server_total_us: [AtomicU64; 2],
    pub server_reported: [AtomicU64; 2],
}

impl HttpMetrics {
    pub fn json(&self) -> serde_json::Value {
        let ms = |a: &[AtomicU64; 2]| [a[0].load(Relaxed) / 1000, a[1].load(Relaxed) / 1000];
        let n = |a: &[AtomicU64; 2]| [a[0].load(Relaxed), a[1].load(Relaxed)];
        serde_json::json!({
            "requests": self.requests.load(Relaxed), "errors": self.errors.load(Relaxed),
            "bytes_in": self.bytes_in.load(Relaxed), "wire_in": self.wire_in.load(Relaxed),
            "tunnel_down": self.tunnel_down.load(Relaxed), "version_mismatch": self.version_mismatch.load(Relaxed),
            "urgent_lane": self.urgent_lane.load(Relaxed), "decode_ms": self.decode_us.load(Relaxed) / 1000,
            // [<=512 KiB, larger]
            "ranges": n(&self.ranges), "ttfb_ms_sum": ms(&self.ttfb_us), "body_ms_sum": ms(&self.body_us),
            "server_read_ms_sum": ms(&self.server_us), "server_total_ms_sum": ms(&self.server_total_us),
            "server_reported": n(&self.server_reported),
        })
    }
}

pub struct HttpStore {
    /// One URL per SSH tunnel (each its own TCP connection). Requests rotate across them; a
    /// tunnel that fails to connect is skipped for a few seconds while its supervisor reconnects.
    bases: Vec<url::Url>,
    rr: AtomicU64,
    down_until_ms: Vec<AtomicU64>,
    t0: std::time::Instant,
    token: String,
    vol: String,
    http: reqwest::Client,
    compress: bool,
    /// server-side subtree served as "/" (e.g. "/steam/KingdomComeDeliverance"); "" = whole tree
    root: String,
    pub metrics: HttpMetrics,
}

/// Frames are decoded on one blocking worker at a time (never on the async runtime threads).
static DECODER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
/// Largest raw piece a server frame may declare (server chunks are 256 KiB; 1 MiB is the hard bound).
const MAX_FRAME_RAW: usize = 1 << 20;
/// How long a tunnel that refused a connection is skipped.
const DOWN_MS: u64 = 5000;

fn transient(m: impl Into<String>) -> Error {
    Error::backend(BackendKind::Transient, m)
}
fn fatal(m: impl Into<String>) -> Error {
    Error::backend(BackendKind::Fatal, m)
}

impl HttpStore {
    /// `base`: e.g. http://127.0.0.1:8787 (the local end of the SSH tunnel), or several
    /// comma-separated URLs, one per tunnel. `vol`: object namespace.
    pub fn new(base: &str, token: &str, vol: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(32)
            .build()
            .map_err(|e| fatal(e.to_string()))?;
        let bases: Vec<url::Url> = base.split(',').map(|b| url::Url::parse(b.trim())).collect::<std::result::Result<_, _>>().map_err(|e| fatal(e.to_string()))?;
        if bases.is_empty() {
            return Err(fatal("no server URL"));
        }
        let down_until_ms = bases.iter().map(|_| AtomicU64::new(0)).collect();
        Ok(HttpStore { bases, rr: AtomicU64::new(0), down_until_ms, t0: std::time::Instant::now(), token: token.into(), vol: vol.into(), http, compress: false, root: String::new(), metrics: HttpMetrics::default() })
    }

    /// Serve only the server subtree `root` (paths seen by the engine stay relative to it).
    pub fn with_root(mut self, root: &str) -> Self {
        let r = norm(root);
        self.root = if r == "/" { String::new() } else { r };
        self
    }

    fn remote(&self, p: &str) -> String {
        format!("{}{}", self.root, norm(p))
    }

    /// Ask the server for sfz1 wire compression on range reads. Data returned is always the
    /// exact raw bytes; an older server that ignores the header simply answers raw.
    pub fn with_compression(mut self, on: bool) -> Self {
        self.compress = on;
        self
    }

    /// Segments are percent-encoded individually. A trailing "" in `segs` is kept only
    /// when `path` is empty (e.g. `/v1/list/` for the root), never producing `//`.
    /// Next tunnel in rotation, skipping ones marked down (all down -> plain rotation).
    fn pick(&self) -> usize {
        self.pick_from(0)
    }

    fn up(&self, i: usize) -> bool {
        self.down_until_ms[i].load(Relaxed) <= self.t0.elapsed().as_millis() as u64
    }

    fn pick_from(&self, first: usize) -> usize {
        let n = self.bases.len();
        let m = n - first;
        let start = self.rr.fetch_add(1, Relaxed) as usize;
        (0..m).map(|k| first + (start + k) % m).find(|&i| self.up(i)).unwrap_or_else(|| (0..n).map(|k| (start + k) % n).find(|&i| self.up(i)).unwrap_or(start % n))
    }

    /// Lane for a range read. With 3+ tunnels the first one is reserved for urgent reads, so a
    /// blocked reader's bytes never queue behind bulk data inside the same TCP connection.
    fn lane(&self, urgent: bool) -> usize {
        if self.bases.len() < 3 {
            return self.pick();
        }
        if urgent && self.up(0) {
            self.metrics.urgent_lane.fetch_add(1, Relaxed);
            return 0;
        }
        self.pick_from(1)
    }

    pub fn tunnels(&self) -> usize {
        self.bases.len()
    }

    fn url(&self, segs: &[&str], path: &str) -> url::Url {
        self.url_on(self.pick(), segs, path)
    }

    fn url_on(&self, lane: usize, segs: &[&str], path: &str) -> url::Url {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut u = self.bases[lane].clone();
        {
            let mut p = u.path_segments_mut().expect("base url");
            p.clear();
            p.extend(segs.iter().filter(|s| !s.is_empty()));
            if parts.is_empty() && segs.last() == Some(&"") {
                p.push("");
            }
            p.extend(parts);
        }
        u
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        self.metrics.requests.fetch_add(1, Relaxed);
        let req = rb.header("x-sfs-token", &self.token).build().map_err(|e| fatal(e.without_url().to_string()))?;
        let origin = req.url().origin();
        let resp = self.http.execute(req).await.map_err(|e| {
            self.metrics.errors.fetch_add(1, Relaxed);
            if e.is_connect() {
                // tunnel down (supervisor reconnecting): route around it for a while
                if let Some(i) = self.bases.iter().position(|b| b.origin() == origin) {
                    self.down_until_ms[i].store(self.t0.elapsed().as_millis() as u64 + DOWN_MS, Relaxed);
                    self.metrics.tunnel_down.fetch_add(1, Relaxed);
                }
            }
            transient(format!("network: {}", e.without_url()))
        })?;
        let st = resp.status().as_u16();
        if resp.status().is_success() {
            return Ok(resp);
        }
        self.metrics.errors.fetch_add(1, Relaxed);
        let body = resp.text().await.unwrap_or_default();
        Err(match st {
            404 => Error::backend(BackendKind::NotFound, body),
            412 => {
                self.metrics.version_mismatch.fetch_add(1, Relaxed);
                Error::backend(BackendKind::VersionChanged, body)
            }
            401 | 403 => fatal(format!("HTTP {st}: {body}")),
            507 => Error::backend(BackendKind::Quota, format!("server storage full: {body}")),
            _ => transient(format!("HTTP {st}: {body}")),
        })
    }

    async fn get_range(&self, u: url::Url, off: u64, len: u64, version: Option<&str>) -> Result<Bytes> {
        let mut rb = self.http.get(u).header(reqwest::header::RANGE, format!("bytes={}-{}", off, off + len - 1));
        if let Some(v) = version {
            // the server refuses (412) instead of serving bytes of a newer file version
            rb = rb.header("x-sfs-version", v);
        }
        if self.compress {
            rb = rb.header("x-sfs-accept", "sfz1");
        }
        let t_send = std::time::Instant::now();
        let resp = self.send(rb).await?;
        if resp.status().as_u16() != 206 {
            return Err(fatal("server ignored Range"));
        }
        let k = usize::from(len > 512 << 10);
        let m = &self.metrics;
        m.ranges[k].fetch_add(1, Relaxed);
        m.ttfb_us[k].fetch_add(t_send.elapsed().as_micros() as u64, Relaxed);
        if let Some(us) = resp.headers().get("x-sfs-read-us").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
            m.server_us[k].fetch_add(us, Relaxed);
            m.server_reported[k].fetch_add(1, Relaxed);
            if let Some(t) = resp.headers().get("x-sfs-srv-us").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
                m.server_total_us[k].fetch_add(t, Relaxed);
            }
        }
        let t_body = std::time::Instant::now();
        let body_done = || m.body_us[k].fetch_add(t_body.elapsed().as_micros() as u64, Relaxed);
        let expect = format!("bytes {}-{}/", off, off + len - 1);
        if !resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with(&expect)) {
            return Err(transient("Content-Range does not match the request (file changed?)"));
        }
        if resp.headers().get("x-sfs-enc").is_some_and(|v| v == "sfz1") {
            let wire = resp.bytes().await.map_err(|e| transient(format!("body: {}", e.without_url())))?;
            body_done();
            self.metrics.wire_in.fetch_add(wire.len() as u64, Relaxed);
            let permit = DECODER.acquire().await.expect("decoder semaphore");
            let t = std::time::Instant::now();
            let raw = tokio::task::spawn_blocking(move || sfs_codec::decode_stream(&wire, MAX_FRAME_RAW, len as usize))
                .await
                .map_err(|e| fatal(e.to_string()))?
                // a corrupt or truncated stream is retried like a network error; it never reaches the cache
                .map_err(|e| {
                    self.metrics.errors.fetch_add(1, Relaxed);
                    transient(format!("sfz1 decode: {e}"))
                })?;
            drop(permit);
            self.metrics.decode_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
            self.metrics.bytes_in.fetch_add(len, Relaxed);
            return Ok(Bytes::from(raw));
        }
        if let Some(cl) = resp.content_length() {
            if cl != len {
                return Err(fatal(format!("Content-Length {cl} != {len}")));
            }
        }
        let b = resp.bytes().await.map_err(|e| transient(format!("body: {}", e.without_url())))?;
        body_done();
        if b.len() as u64 != len {
            return Err(transient(format!("short body {} of {len}", b.len())));
        }
        self.metrics.bytes_in.fetch_add(len, Relaxed);
        self.metrics.wire_in.fetch_add(len, Relaxed);
        Ok(b)
    }
}

#[derive(serde::Deserialize)]
struct StatResp {
    size: u64,
    version: String,
    is_dir: bool,
}

#[derive(serde::Deserialize)]
struct ListEntry {
    name: String,
    size: u64,
    version: String,
    is_dir: bool,
}

fn norm(p: &str) -> String {
    format!("/{}", p.replace('\\', "/").trim_matches('/'))
}

#[async_trait]
impl Backend for HttpStore {
    fn name(&self) -> &str {
        "http"
    }

    async fn space(&self) -> Result<Option<(u64, u64)>> {
        let v: serde_json::Value = self.send(self.http.get(self.url(&["v1", "space"], ""))).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        Ok(v["total"].as_u64().zip(v["free"].as_u64()).filter(|&(t, _)| t > 0))
    }

    async fn stat(&self, path: &str) -> Result<FileMeta> {
        let p = norm(path);
        let s: StatResp = self.send(self.http.get(self.url(&["v1", "stat", ""], &self.remote(&p)))).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        Ok(FileMeta { path: p.clone(), id: p, size: s.size, version: s.version, is_dir: s.is_dir })
    }

    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>> {
        let d = norm(dir);
        let v: Vec<ListEntry> = self.send(self.http.get(self.url(&["v1", "list", ""], &self.remote(&d)))).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        Ok(v.into_iter()
            .map(|e| {
                let p = format!("{}/{}", d.trim_end_matches('/'), e.name);
                FileMeta { path: p.clone(), id: p, size: e.size, version: e.version, is_dir: e.is_dir }
            })
            .collect())
    }

    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes> {
        self.read_range_class(meta, offset, len, false).await
    }

    async fn read_range_class(&self, meta: &FileMeta, offset: u64, len: u64, urgent: bool) -> Result<Bytes> {
        if offset >= meta.size {
            return Ok(Bytes::new());
        }
        let len = len.min(meta.size - offset);
        let lane = self.lane(urgent);
        match meta.path.strip_prefix("/obj/") {
            // objects are immutable: their version is a content hash, not checked per read
            Some(id) => self.get_range(self.url_on(lane, &["v1", "obj", &self.vol], id), offset, len, None).await,
            None => self.get_range(self.url_on(lane, &["v1", "file"], &self.remote(&meta.path)), offset, len, Some(&meta.version)).await,
        }
    }
}

#[async_trait]
impl ObjectStore for HttpStore {
    async fn put_object(&self, name: &str, data: Bytes) -> Result<ObjRef> {
        use md5::{Digest, Md5};
        let local = format!("{:x}", Md5::digest(&data));
        let n = data.len() as u64;
        let r: ObjRef = self.send(self.http.put(self.url(&["v1", "obj", &self.vol], name)).body(data)).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        if r.md5 != local || r.size != n {
            return Err(fatal(format!("upload of {name} not verified: md5 {} vs local {local}", r.md5)));
        }
        self.metrics.bytes_out.fetch_add(n, Relaxed);
        Ok(r)
    }

    async fn delete_object(&self, obj: &ObjRef) -> Result<()> {
        self.send(self.http.delete(self.url(&["v1", "obj", &self.vol], &obj.id))).await.map(|_| ())
    }

    async fn remote_free_bytes(&self) -> Result<Option<u64>> {
        let v: serde_json::Value = self.send(self.http.get(self.url(&["v1", "space"], ""))).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        Ok(v["free"].as_u64())
    }

    async fn list_named(&self, prefix: &str) -> Result<Vec<(String, ObjRef)>> {
        let mut u = self.url(&["v1", "objs", &self.vol], "");
        u.query_pairs_mut().append_pair("prefix", prefix);
        let v: Vec<serde_json::Value> = self.send(self.http.get(u)).await?.json().await.map_err(|e| fatal(e.to_string()))?;
        let mut out: Vec<(String, ObjRef)> = v
            .into_iter()
            .filter_map(|e| {
                Some((e["name"].as_str()?.to_string(), ObjRef { id: e["id"].as_str()?.to_string(), size: e["size"].as_u64()?, md5: e["md5"].as_str().unwrap_or("").to_string() }))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }

    fn object_meta(&self, obj: &ObjRef) -> FileMeta {
        FileMeta { path: format!("/obj/{}", obj.id), id: format!("obj:{}", obj.id), size: obj.size, version: obj.md5.clone(), is_dir: false }
    }
}
