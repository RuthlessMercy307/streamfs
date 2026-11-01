use async_trait::async_trait;
use sfs_core::{Backend, BackendKind, Config, Engine, Error};
use sfs_gdrive::auth::{StaticToken, TokenProvider};
use sfs_gdrive::DriveBackend;
use sfs_sim::expected;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SEED: u64 = 77;
const SIZE: u64 = 5 << 30; // 5 GB "game file", never materialised
const MB: u64 = 1 << 20;

/// Serves ranged media like Drive does (206 + Content-Range), with optional fault injection.
struct MediaResponder {
    calls: Arc<AtomicU64>,
    fail_every: u64,
    ignore_range: bool,
}

impl Respond for MediaResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Relaxed) + 1;
        if self.fail_every > 0 && n % self.fail_every == 0 {
            return if n % (self.fail_every * 2) == 0 {
                ResponseTemplate::new(429).insert_header("Retry-After", "0")
            } else {
                ResponseTemplate::new(403).set_body_string(
                    r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}],"code":403}}"#,
                )
            };
        }
        let range = req.headers.get("range").map(|v| v.to_str().unwrap().to_string());
        if self.ignore_range || range.is_none() {
            return ResponseTemplate::new(200).set_body_bytes(vec![0u8; 3_000_000]);
        }
        let r = range.unwrap();
        let (a, b) = r.trim_start_matches("bytes=").split_once('-').unwrap();
        let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
        let b = b.min(SIZE - 1);
        ResponseTemplate::new(206)
            .insert_header("Content-Range", format!("bytes {a}-{b}/{SIZE}").as_str())
            .set_body_bytes(expected(SEED, a, (b - a + 1) as usize))
    }
}

async fn server_with(responder: MediaResponder) -> (MockServer, Arc<AtomicU64>) {
    let server = MockServer::start().await;
    let calls = responder.calls.clone();
    // metadata for fileId "FID"
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/FID"))
        .and(wiremock::matchers::query_param("fields", "id,name,mimeType,size,md5Checksum,version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"FID","name":"game.pak","mimeType":"application/octet-stream",
            "size": SIZE.to_string(),"md5Checksum":"abc","version":"3"})))
        .mount(&server)
        .await;
    // folder lookup by name
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files":[{"id":"FID","name":"game.pak","mimeType":"application/octet-stream",
            "size": SIZE.to_string(),"md5Checksum":"abc","version":"3"}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/drive/v3/files/FID$"))
        .and(wiremock::matchers::query_param("alt", "media"))
        .respond_with(responder)
        .mount(&server)
        .await;
    (server, calls)
}

fn responder(fail_every: u64, ignore_range: bool) -> MediaResponder {
    MediaResponder { calls: Arc::new(AtomicU64::new(0)), fail_every, ignore_range }
}

fn drive(server: &MockServer) -> DriveBackend {
    DriveBackend::with_api_base(Arc::new(StaticToken("t".into())), "root", &server.uri())
}

#[tokio::test]
async fn ranged_read_returns_exact_bytes_via_206() {
    let (server, calls) = server_with(responder(0, false)).await;
    let be = drive(&server);
    let meta = be.stat("/game.pak").await.unwrap();
    assert_eq!(meta.size, SIZE);
    let got = be.read_range(&meta, 3 * MB + 17, 300_000).await.unwrap();
    assert_eq!(&got[..], &expected(SEED, 3 * MB + 17, 300_000)[..]);
    assert_eq!(calls.load(Relaxed), 1);
}

#[tokio::test]
async fn server_ignoring_range_is_refused_not_downloaded() {
    let (server, _) = server_with(responder(0, true)).await;
    let be = drive(&server);
    let meta = be.stat("/game.pak").await.unwrap();
    let e = be.read_range(&meta, 10 * MB, 1 * MB).await.unwrap_err();
    assert!(matches!(e, Error::Backend { kind: BackendKind::Fatal, .. }), "{e:?}");
}

#[test]
fn error_taxonomy_matches_drive_reasons() {
    use sfs_gdrive::map_status;
    let k = |s, body: &str| match map_status(s, None, body) {
        Error::Backend { kind, .. } => kind,
        _ => panic!(),
    };
    assert_eq!(k(403, r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#), BackendKind::RateLimited);
    assert_eq!(k(403, r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#), BackendKind::RateLimited);
    assert_eq!(k(403, r#"{"error":{"errors":[{"reason":"downloadQuotaExceeded"}]}}"#), BackendKind::Quota);
    assert_eq!(k(403, r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#), BackendKind::Fatal);
    assert_eq!(k(429, ""), BackendKind::RateLimited);
    assert_eq!(k(404, ""), BackendKind::NotFound);
    assert_eq!(k(503, ""), BackendKind::Transient);
    let e = map_status(429, Some(Duration::from_secs(2)), "");
    assert!(matches!(e, Error::Backend { retry_after: Some(d), .. } if d == Duration::from_secs(2)));
}

struct FlakyToken {
    invalidated: AtomicU64,
}
#[async_trait]
impl TokenProvider for FlakyToken {
    async fn access_token(&self) -> sfs_core::Result<String> {
        Ok(if self.invalidated.load(Relaxed) == 0 { "stale".into() } else { "fresh".into() })
    }
    async fn invalidate(&self) {
        self.invalidated.fetch_add(1, Relaxed);
    }
}

#[tokio::test]
async fn expired_token_401_triggers_one_refresh_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/FID"))
        .and(wiremock::matchers::header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/FID"))
        .and(wiremock::matchers::header("authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"FID","mimeType":"application/octet-stream","size":"10","md5Checksum":"m","version":"1"})))
        .mount(&server)
        .await;
    let tok = Arc::new(FlakyToken { invalidated: AtomicU64::new(0) });
    let be = DriveBackend::with_api_base(tok.clone(), "FID", &server.uri());
    let meta = be.stat("/").await.unwrap();
    assert_eq!(meta.size, 10);
    assert_eq!(tok.invalidated.load(Relaxed), 1);
    assert_eq!(be.metrics.http_401.load(Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_over_mock_drive_survives_403_429_and_stays_correct() {
    let (server, calls) = server_with(responder(4, false)).await;
    let be: Arc<dyn Backend> = Arc::new(drive(&server));
    let dir = std::env::temp_dir().join(format!("sfs-gd-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = Config::new(&dir);
    cfg.cache_bytes = 256 * MB;
    cfg.base_backoff = Duration::from_millis(5);
    let eng = Engine::new(be, cfg).unwrap();
    let h = eng.open("/game.pak").await.unwrap();
    let mut s = 99u64;
    for _ in 0..30 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let off = s % (SIZE - 4 * MB);
        let got = h.read(off, (3 * MB) as usize).await.unwrap();
        assert_eq!(&got[..], &expected(SEED, off, (3 * MB) as usize)[..]);
    }
    let st = eng.stats();
    assert!(st.retries > 0 && st.rate_limited > 0, "{st:?}");
    // 30 reads of 3 MB out of a 5 GB file: nowhere near a full download.
    assert!(st.backend_bytes < 200 * MB, "{}", st.backend_bytes);
    assert!(calls.load(Relaxed) > 30);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn path_resolution_and_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files": []})))
        .mount(&server)
        .await;
    let be = drive(&server);
    let e = be.stat("/nope/x.bin").await.unwrap_err();
    assert!(matches!(e, Error::Backend { kind: BackendKind::NotFound, .. }), "{e:?}");
}
