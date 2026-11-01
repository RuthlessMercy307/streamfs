use crate::error::Result;
use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub path: String,
    /// Opaque provider id (Drive fileId, S3 key, ...). Stable across renames.
    pub id: String,
    pub size: u64,
    /// Opaque version token (md5/etag/revision). A change invalidates cached blocks.
    pub version: String,
    pub is_dir: bool,
}

/// Handle to an immutable remote object written by the volume layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjRef {
    pub id: String,
    pub size: u64,
    /// Content md5 as confirmed by the remote (hex).
    pub md5: String,
}

/// Object storage for write-back: whole immutable objects only (no ranged
/// rewrites, which Drive cannot do). Reads go through the `Backend` side, so the
/// read engine (cache, prefetch, hedging) serves volume data unchanged.
#[async_trait]
pub trait ObjectStore: Backend {
    /// Store `data` as a new object. Must verify the remote copy (md5) before returning Ok.
    async fn put_object(&self, name: &str, data: Bytes) -> Result<ObjRef>;
    async fn delete_object(&self, obj: &ObjRef) -> Result<()>;
    /// Usable remote space, if the provider reports it.
    async fn remote_free_bytes(&self) -> Result<Option<u64>>;
    /// Objects whose name starts with `prefix`, newest first by name. Used to find
    /// metadata snapshots; stores that cannot list return an empty list (no restore).
    async fn list_named(&self, _prefix: &str) -> Result<Vec<(String, ObjRef)>> {
        Ok(Vec::new())
    }
    fn object_meta(&self, obj: &ObjRef) -> FileMeta {
        FileMeta { path: format!("/obj/{}", obj.id), id: obj.id.clone(), size: obj.size, version: obj.md5.clone(), is_dir: false }
    }
}

/// Provider contract. The engine knows nothing about Drive/SFTP/S3.
///
/// `read_range` must return exactly `min(len, size - offset)` bytes of the
/// version named in `meta`, or an error. It must never return stale or
/// truncated data as if valid.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn stat(&self, path: &str) -> Result<FileMeta>;
    async fn list(&self, dir: &str) -> Result<Vec<FileMeta>>;
    async fn read_range(&self, meta: &FileMeta, offset: u64, len: u64) -> Result<Bytes>;
    /// Same contract as `read_range`. `urgent`: a reader is blocked on this request; backends
    /// with several connections may give it a lane that bulk traffic never uses.
    async fn read_range_class(&self, meta: &FileMeta, offset: u64, len: u64, _urgent: bool) -> Result<Bytes> {
        self.read_range(meta, offset, len).await
    }
}
