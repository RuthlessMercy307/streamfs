use sfs_core::Backend;
use sfs_gdrive::auth::{interactive_login, ClientConfig, OAuthTokens, RefreshStore, TokenProvider};
use std::path::PathBuf;

const USAGE: &str = "uso: sfs-auth <login|status|biggest|logout> [--client ruta\\client_secret.json] [--account nombre]";

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let client_path = arg(&args, "--client").map(PathBuf::from).unwrap_or_else(ClientConfig::default_path);
    let account = arg(&args, "--account").unwrap_or_else(|| "default".into());
    let store = RefreshStore::new(&account);

    let result: Result<(), sfs_core::Error> = async {
        match cmd {
            "login" => {
                let client = ClientConfig::from_file(&client_path)?;
                interactive_login(&client, &store).await?;
                println!("Sesion guardada en el Administrador de credenciales de Windows (cuenta {account}).");
            }
            "status" => {
                let client = ClientConfig::from_file(&client_path)?;
                let t = OAuthTokens::new(client, RefreshStore::new(&account));
                let tok = t.access_token().await?;
                let r = reqwest::Client::new()
                    .get("https://www.googleapis.com/drive/v3/about?fields=user(emailAddress),storageQuota(limit,usage)")
                    .bearer_auth(tok)
                    .send()
                    .await
                    .map_err(|e| sfs_core::Error::Io(e.without_url().to_string()))?;
                println!("HTTP {}\n{}", r.status(), r.text().await.unwrap_or_default());
            }
            "biggest" => {
                // Metadata-only listing (1 request) to pick a test file; downloads nothing.
                let client = ClientConfig::from_file(&client_path)?;
                let t = OAuthTokens::new(client, RefreshStore::new(&account));
                let tok = t.access_token().await?;
                let r = reqwest::Client::new()
                    .get("https://www.googleapis.com/drive/v3/files")
                    .query(&[
                        ("orderBy", "quotaBytesUsed desc"),
                        ("pageSize", args.get(1).map(String::as_str).unwrap_or("15")),
                        ("q", "mimeType != 'application/vnd.google-apps.folder' and trashed=false"),
                        ("fields", "files(id,name,size,mimeType,md5Checksum)"),
                    ])
                    .bearer_auth(tok)
                    .send()
                    .await
                    .map_err(|e| sfs_core::Error::Io(e.without_url().to_string()))?;
                let v: serde_json::Value = r.json().await.map_err(|e| sfs_core::Error::Io(e.to_string()))?;
                for f in v["files"].as_array().into_iter().flatten() {
                    let gb = f["size"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e9;
                    println!("{:8.2} GB  {}  id={}  md5={}", gb, f["name"].as_str().unwrap_or("?"), f["id"].as_str().unwrap_or("?"), f["md5Checksum"].as_str().unwrap_or("-"));
                }
            }
            "where" => {
                // Metadata-only: parent folder ids of a file (1 request).
                let id = args.get(1).cloned().unwrap_or_default();
                let client = ClientConfig::from_file(&client_path)?;
                let t = OAuthTokens::new(client, RefreshStore::new(&account));
                let tok = t.access_token().await?;
                let r = reqwest::Client::new()
                    .get(format!("https://www.googleapis.com/drive/v3/files/{id}?fields=name,parents"))
                    .bearer_auth(tok)
                    .send()
                    .await
                    .map_err(|e| sfs_core::Error::Io(e.without_url().to_string()))?;
                println!("{}", r.text().await.unwrap_or_default());
            }
            "cat" => {
                // Download a (small) Drive file by id to stdout or --out FILE. Read-only.
                let id = args.get(1).cloned().unwrap_or_default();
                let client = ClientConfig::from_file(&client_path)?;
                let tokens = std::sync::Arc::new(OAuthTokens::new(client, RefreshStore::new(&account)));
                let drive = sfs_gdrive::DriveBackend::new(tokens, "root");
                let m = drive.stat_id(&id).await?;
                let data = drive.read_range(&m, 0, m.size).await?;
                match arg(&args, "--out") {
                    Some(p) => std::fs::write(p, &data).map_err(sfs_core::Error::from)?,
                    None => print!("{}", String::from_utf8_lossy(&data)),
                }
            }
            "ls" => {
                // Read-only listing of a Drive folder: name, size, id.
                let client = ClientConfig::from_file(&client_path)?;
                let tokens = std::sync::Arc::new(OAuthTokens::new(client, RefreshStore::new(&account)));
                let drive = sfs_gdrive::DriveBackend::new(tokens, "root");
                let mut items = drive.list(args.get(1).map(String::as_str).unwrap_or("")).await?;
                items.sort_by(|a, b| a.path.cmp(&b.path));
                for m in &items {
                    println!("{:>12}  {}  {}", m.size, m.path.rsplit('/').next().unwrap_or(""), m.id);
                }
                println!("{} entradas", items.len());
            }
            "purge-test-volume" => {
                // Deletes the objects of a TEST volume folder. Restricted to streamfs-volumes/*;
                // with drive.file the app can only delete files it created itself.
                let path = args.get(1).cloned().unwrap_or_default();
                if !path.starts_with("streamfs-volumes/") || path.contains("..") {
                    println!("solo se permiten carpetas bajo streamfs-volumes/");
                    return Ok(());
                }
                let client = ClientConfig::from_file(&client_path)?;
                let tokens = std::sync::Arc::new(OAuthTokens::new(client, RefreshStore::new(&account)));
                let drive = sfs_gdrive::DriveBackend::new(tokens, "root");
                let items = drive.list(&path).await?;
                let mut n = 0;
                for m in items.iter().filter(|m| !m.is_dir) {
                    drive.delete_file(&m.id).await?;
                    n += 1;
                }
                println!("{path}: {n} objetos borrados");
            }
            "folder-id" => {
                // Id of an existing folder path under "Mi unidad" (does not create anything).
                let client = ClientConfig::from_file(&client_path)?;
                let tokens = std::sync::Arc::new(OAuthTokens::new(client, RefreshStore::new(&account)));
                let drive = sfs_gdrive::DriveBackend::new(tokens, "root");
                let m = drive.stat(args.get(1).map(String::as_str).unwrap_or("/")).await?;
                println!("{}", m.id);
            }
            "upload" => {
                // sfs-auth upload <carpeta local> <ruta en Drive, p.ej. streamfs-tests/Risk of Rain>
                let local = std::path::PathBuf::from(args.get(1).cloned().unwrap_or_default());
                let remote = args.get(2).cloned().unwrap_or_default();
                if !local.is_dir() || remote.is_empty() {
                    println!("uso: sfs-auth upload <carpeta local> <ruta en Drive>");
                    return Ok(());
                }
                let client = ClientConfig::from_file(&client_path)?;
                let tokens = std::sync::Arc::new(OAuthTokens::new(client, RefreshStore::new(&account)));
                let drive = sfs_gdrive::DriveBackend::new(tokens, "root");
                let t0 = std::time::Instant::now();
                let (mut up, mut skip, mut bytes) = (0, 0, 0u64);
                let mut manifest = Vec::new();
                let mut stack = vec![(local.clone(), remote.trim_matches('/').to_string())];
                while let Some((dir, rpath)) = stack.pop() {
                    let folder = drive.ensure_folder_path(&rpath).await?;
                    let mut entries: Vec<_> = std::fs::read_dir(&dir).map_err(sfs_core::Error::from)?.flatten().collect();
                    entries.sort_by_key(|e| e.file_name());
                    for e in entries {
                        let name = e.file_name().to_string_lossy().to_string();
                        let p = e.path();
                        if p.is_dir() {
                            stack.push((p, format!("{rpath}/{name}")));
                            continue;
                        }
                        let data = std::fs::read(&p).map_err(sfs_core::Error::from)?;
                        let md5 = {
                            use md5::{Digest, Md5};
                            format!("{:x}", Md5::digest(&data))
                        };
                        match drive.find_child(&folder, &name).await? {
                            Some(m) if m.version.starts_with(&md5) && m.size == data.len() as u64 => {
                                skip += 1;
                                println!("  igual   {rpath}/{name}");
                                manifest.push(serde_json::json!({"path": format!("{rpath}/{name}"), "id": m.id, "md5": md5, "size": m.size}));
                            }
                            Some(_) => {
                                return Err(sfs_core::Error::backend(
                                    sfs_core::BackendKind::Fatal,
                                    format!("{rpath}/{name} ya existe en Drive con otro contenido: no se sobrescribe"),
                                ));
                            }
                            None => {
                                let u = drive.upload_bytes(&folder, &name, &data).await?;
                                up += 1;
                                bytes += u.size;
                                println!("  subido  {rpath}/{name}  {} bytes  md5 {} verificado", u.size, u.md5);
                                manifest.push(serde_json::json!({"path": format!("{rpath}/{name}"), "id": u.id, "md5": u.md5, "size": u.size}));
                            }
                        }
                    }
                }
                let secs = t0.elapsed().as_secs_f64();
                println!(
                    "subidos {up}, ya presentes {skip}, {:.1} MB en {:.1} s ({:.2} MB/s), peticiones de subida {}",
                    bytes as f64 / 1e6, secs, bytes as f64 / 1e6 / secs.max(0.001),
                    drive.metrics.upload_requests.load(std::sync::atomic::Ordering::Relaxed)
                );
                std::fs::write("upload-manifest.json", serde_json::to_string_pretty(&manifest).unwrap()).map_err(sfs_core::Error::from)?;
            }
            "logout" => {
                store.clear()?;
                println!("Sesion local borrada. (Para revocar tambien en Google: myaccount.google.com/permissions)");
            }
            _ => println!("{USAGE}"),
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
