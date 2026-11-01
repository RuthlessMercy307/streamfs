//! Local copy-on-write layer.
//!
//! The remote tree is an immutable base. Anything created or modified through
//! the drive lives under `root` (mirroring virtual paths) and always wins over
//! the remote base. Deleting or renaming away a path that exists remotely
//! records a "whiteout" so the remote entry stays hidden. The overlay is
//! separate from the block cache and is never evicted.

use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const RESERVED_PREFIX: &str = ".sfs-";
const WHITEOUTS: &str = ".sfs-whiteouts.json";

pub struct Overlay {
    root: PathBuf,
    whiteouts: Mutex<HashSet<String>>,
}

fn key(vpath: &str) -> String {
    vpath.to_lowercase() // Windows names are case-insensitive
}

impl Overlay {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let wh: HashSet<String> = match std::fs::read(root.join(WHITEOUTS)) {
            Ok(b) => serde_json::from_slice::<Vec<String>>(&b)
                .map_err(std::io::Error::other)?
                .into_iter()
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(e),
        };
        Ok(Overlay {
            root: root.to_path_buf(),
            whiteouts: Mutex::new(wh),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Local path backing virtual path `vpath` ("/a/b").
    pub fn local(&self, vpath: &str) -> PathBuf {
        let rel = vpath.trim_start_matches('/');
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel.replace('/', "\\"))
        }
    }

    pub fn is_whiteout(&self, vpath: &str) -> bool {
        self.whiteouts.lock().contains(&key(vpath))
    }

    pub fn add_whiteout(&self, vpath: &str) -> std::io::Result<()> {
        let mut g = self.whiteouts.lock();
        if g.insert(key(vpath)) {
            self.persist(&g)?;
        }
        Ok(())
    }

    pub fn remove_whiteout(&self, vpath: &str) -> std::io::Result<()> {
        let mut g = self.whiteouts.lock();
        if g.remove(&key(vpath)) {
            self.persist(&g)?;
        }
        Ok(())
    }

    /// Atomic + durable: a crash leaves either the old or the new list.
    fn persist(&self, set: &HashSet<String>) -> std::io::Result<()> {
        let mut v: Vec<&String> = set.iter().collect();
        v.sort();
        let tmp = self.root.join(".sfs-whiteouts.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec(&v).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, self.root.join(WHITEOUTS))
    }
}
