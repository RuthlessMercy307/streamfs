//! OAuth 2.0 for an installed (Desktop) app: loopback redirect + PKCE (S256).
//!
//! * Scope is read-only (`drive.readonly`): Drive has no folder-scoped scope, and
//!   `drive.file` cannot read files the app did not create.
//! * The refresh token lives in Windows Credential Manager (via `keyring`),
//!   never in a config file, log or the repository. Access tokens stay in memory.
//! * The user signs in and consents in their own browser; this code never sees
//!   their Google password.

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{distributions::Alphanumeric, Rng};
use sfs_core::{BackendKind, Error, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/drive.readonly";
/// Create/modify only files this app created (non-sensitive scope).
pub const SCOPE_FILE: &str = "https://www.googleapis.com/auth/drive.file";
const KEYRING_SERVICE: &str = "streamfs-gdrive";

fn fatal(m: impl Into<String>) -> Error {
    Error::backend(BackendKind::Fatal, m)
}

#[derive(Clone)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_uri: String,
    pub token_uri: String,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ClientConfig {
    /// Parse the `client_secret_*.json` Google lets you download for an OAuth client.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).map_err(|e| fatal(format!("cannot read {}: {e}", path.display())))?;
        let v: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| fatal(format!("bad client json: {e}")))?;
        let o = v
            .get("installed")
            .ok_or_else(|| fatal("client json has no \"installed\" key: create the OAuth client as type 'Desktop app'"))?;
        let s = |k: &str| o.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Ok(ClientConfig {
            client_id: s("client_id").ok_or_else(|| fatal("client_id missing"))?,
            client_secret: s("client_secret"),
            auth_uri: s("auth_uri").unwrap_or_else(|| "https://accounts.google.com/o/oauth2/v2/auth".into()),
            token_uri: s("token_uri").unwrap_or_else(|| "https://oauth2.googleapis.com/token".into()),
        })
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("streamfs").join("client_secret.json")
    }
}

#[async_trait]
pub trait TokenProvider: Send + Sync + 'static {
    async fn access_token(&self) -> Result<String>;
    /// Drop the cached access token (server said 401).
    async fn invalidate(&self);
}

/// Fixed token, for tests.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenProvider for StaticToken {
    async fn access_token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
    async fn invalidate(&self) {}
}

pub struct RefreshStore {
    account: String,
}

impl RefreshStore {
    pub fn new(account: &str) -> Self {
        RefreshStore { account: account.to_string() }
    }
    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &self.account).map_err(|e| fatal(format!("credential store: {e}")))
    }
    pub fn get(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(fatal(format!("credential store: {e}"))),
        }
    }
    pub fn set(&self, token: &str) -> Result<()> {
        self.entry()?.set_password(token).map_err(|e| fatal(format!("credential store: {e}")))
    }
    pub fn clear(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(fatal(format!("credential store: {e}"))),
        }
    }
}

struct Cached {
    access: Option<String>,
    expires: Instant,
}

pub struct OAuthTokens {
    client: ClientConfig,
    http: reqwest::Client,
    store: RefreshStore,
    cached: tokio::sync::Mutex<Cached>,
}

impl OAuthTokens {
    pub fn new(client: ClientConfig, store: RefreshStore) -> Self {
        OAuthTokens {
            client,
            http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap(),
            store,
            cached: tokio::sync::Mutex::new(Cached { access: None, expires: Instant::now() }),
        }
    }
}

#[derive(serde::Deserialize)]
struct TokenResp {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: Option<String>,
}

async fn token_request(http: &reqwest::Client, client: &ClientConfig, mut form: Vec<(&str, String)>) -> Result<TokenResp> {
    form.push(("client_id", client.client_id.clone()));
    if let Some(s) = &client.client_secret {
        form.push(("client_secret", s.clone()));
    }
    let resp = http
        .post(&client.token_uri)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::backend(BackendKind::Transient, format!("token endpoint: {}", e.without_url())))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if body.contains("invalid_grant") {
            return Err(fatal(
                "Google rechazo el refresh token (invalid_grant): caduco o fue revocado. \
                 Si el proyecto OAuth esta en modo 'Testing' caduca a los 7 dias. Ejecuta: sfs-auth login",
            ));
        }
        let kind = if status.is_server_error() { BackendKind::Transient } else { BackendKind::Fatal };
        return Err(Error::backend(kind, format!("token endpoint HTTP {status}")));
    }
    serde_json::from_str(&body).map_err(|e| fatal(format!("token response: {e}")))
}

#[async_trait]
impl TokenProvider for OAuthTokens {
    async fn access_token(&self) -> Result<String> {
        let mut c = self.cached.lock().await;
        if let Some(t) = &c.access {
            if Instant::now() + Duration::from_secs(60) < c.expires {
                return Ok(t.clone());
            }
        }
        let refresh = self
            .store
            .get()?
            .ok_or_else(|| fatal("no hay sesion de Google guardada. Ejecuta: sfs-auth login"))?;
        let r = token_request(
            &self.http,
            &self.client,
            vec![("grant_type", "refresh_token".into()), ("refresh_token", refresh)],
        )
        .await?;
        c.expires = Instant::now() + Duration::from_secs(r.expires_in);
        c.access = Some(r.access_token.clone());
        Ok(r.access_token)
    }

    async fn invalidate(&self) {
        self.cached.lock().await.access = None;
    }
}

fn rand_string(n: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(n).map(char::from).collect()
}

fn open_browser(url: &str) {
    // rundll32 avoids cmd.exe quoting problems with '&' in the URL.
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

/// Run the browser sign-in and persist the refresh token. The user performs
/// the sign-in and consent in their browser; we only receive the auth code on 127.0.0.1.
pub async fn interactive_login(client: &ClientConfig, store: &RefreshStore) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = rand_string(96);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = rand_string(24);

    let mut url = url::Url::parse(&client.auth_uri).map_err(|e| fatal(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &format!("{SCOPE_READONLY} {SCOPE_FILE}"))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    println!("Abriendo el navegador. Permisos pedidos: leer tus archivos de Drive (solo lectura) y crear/modificar");
    println!("unicamente los archivos que cree esta app (drive.file). No puede borrar ni cambiar tus otros archivos.");
    println!("Si no se abre, copia esta URL en tu navegador:\n\n{url}\n");
    open_browser(url.as_str());

    let code = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/").to_string();
            let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok();
            let q: std::collections::HashMap<String, String> =
                parsed.map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default();
            let body = "<html><body style=\"font-family:sans-serif\"><h3>Listo. Puedes cerrar esta pestana y volver a la terminal.</h3></body></html>";
            let _ = sock
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                )
                .await;
            if let Some(err) = q.get("error") {
                return Err::<String, Error>(fatal(format!("autorizacion rechazada: {err}")));
            }
            if let Some(code) = q.get("code") {
                if q.get("state") != Some(&state) {
                    return Err(fatal("state invalido (posible redireccion ajena); abortado"));
                }
                return Ok(code.clone());
            }
            // favicon or other noise: keep waiting
        }
    })
    .await
    .map_err(|_| fatal("tiempo agotado esperando la autorizacion (5 min)"))??;

    let http = reqwest::Client::new();
    let r = token_request(
        &http,
        client,
        vec![
            ("grant_type", "authorization_code".into()),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    let refresh = r.refresh_token.ok_or_else(|| fatal("Google no devolvio refresh_token"))?;
    // Refuse anything broader than asked (e.g. full `drive`) and report a partial grant.
    let granted: Vec<&str> = r.scope.as_deref().unwrap_or("").split(' ').collect();
    if granted.iter().any(|s| *s == "https://www.googleapis.com/auth/drive") {
        return Err(fatal("Google concedio el permiso completo 'drive', mas amplio de lo pedido; abortado"));
    }
    if !granted.contains(&SCOPE_READONLY) {
        return Err(fatal(format!("falta el permiso de lectura: concedido {:?}", r.scope)));
    }
    if !granted.contains(&SCOPE_FILE) {
        println!("Aviso: no se concedio drive.file; la lectura funciona pero no se podra subir nada.");
    }
    store.set(&refresh)?;
    Ok(())
}
