//! Full-fidelity log of application reads, for offline analysis and reproducible replay.
//!
//! One line per filesystem read request that reached the engine (not per tunnel request):
//!   `F <fid> <size> <version> <path>`  once per file, before its first read
//!   `R <t_us> <fid> <offset> <len> <lat_us> <missed_blocks>`
//! `t_us` is when the read started, relative to the log's creation; `lat_us` is how long the
//! caller waited; `missed_blocks` > 0 means it waited for remote data. ~40 bytes per read, so
//! a 1-hour session is a few tens of MB. Text on purpose: greppable, stable across versions.

use crate::backend::FileMeta;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::time::Instant;

pub struct ReadLog {
    t0: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    w: BufWriter<std::fs::File>,
    fids: HashMap<String, u32>,
    failed: bool,
}

impl ReadLog {
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(ReadLog { t0: Instant::now(), inner: Mutex::new(Inner { w: BufWriter::with_capacity(256 << 10, f), fids: HashMap::new(), failed: false }) })
    }

    /// Never fails the read: a log write error just stops logging.
    pub fn log(&self, meta: &FileMeta, started: Instant, offset: u64, len: u64, lat_us: u64, missed: u32) {
        let t_us = started.saturating_duration_since(self.t0).as_micros() as u64;
        let mut g = self.inner.lock();
        if g.failed {
            return;
        }
        let n = g.fids.len() as u32;
        let (fid, new) = match g.fids.get(&meta.path) {
            Some(&f) => (f, false),
            None => {
                g.fids.insert(meta.path.clone(), n);
                (n, true)
            }
        };
        let r = (|| {
            if new {
                // the version has no spaces (server-generated); the path is the rest of the line
                writeln!(g.w, "F {fid} {} {} {}", meta.size, meta.version.replace(' ', "_"), meta.path)?;
            }
            writeln!(g.w, "R {t_us} {fid} {offset} {len} {lat_us} {missed}")
        })();
        if r.is_err() {
            g.failed = true;
        }
    }

    pub fn flush(&self) {
        let mut g = self.inner.lock();
        if g.w.flush().is_err() {
            g.failed = true;
        }
    }
}

impl Drop for ReadLog {
    fn drop(&mut self) {
        let _ = self.inner.lock().w.flush();
    }
}
