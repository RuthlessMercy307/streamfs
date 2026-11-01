//! Learned startup prefetch, independent of the application.
//!
//! Every mount records which regions are read during its first minutes (`Engine::start_trace`,
//! cache hits included: it is behaviour, not network traffic). The last few sessions are kept.
//! On the next mount the regions worth fetching early are the ones that recur: with two or
//! more sessions recorded, a 1 MiB region must have been read in at least two of them (one-off
//! reads of a single session, e.g. a menu clicked once, are not repeated); with a single
//! session everything it read is used. Regions are replayed in order of first use, as
//! speculative traffic, so real reads always go first and unused guesses are evicted first.

use crate::engine::{ProfEntry, Profile};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const GRAIN: u64 = 1 << 20;
pub const MAX_SESSIONS: usize = 4;

#[derive(Serialize, Deserialize, Default)]
pub struct History {
    /// Oldest first.
    pub sessions: Vec<Profile>,
}

impl History {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Atomic replace, so a crash mid-write never loses the previous history.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Start a new session slot (drops the oldest beyond MAX_SESSIONS). Returns its index.
    pub fn begin(&mut self) -> usize {
        self.sessions.push(Profile::default());
        while self.sessions.len() > MAX_SESSIONS {
            self.sessions.remove(0);
        }
        self.sessions.len() - 1
    }

    /// One recorded session reduced to 1 MiB-grain regions (small to store, same plan).
    pub fn compact(p: &Profile) -> Profile {
        History { sessions: vec![p.clone()] }.plan(u64::MAX)
    }

    /// Regions to prefetch, in order of first use, at most `budget` bytes.
    pub fn plan(&self, budget: u64) -> Profile {
        let done: Vec<&Profile> = self.sessions.iter().filter(|p| !p.entries.is_empty()).collect();
        if done.is_empty() {
            return Profile::default();
        }
        let need = if done.len() >= 2 { 2 } else { 1 };
        // (path, grain) -> (sessions that read it, earliest t_ms, latest entry seen)
        let mut seen: HashMap<(String, u64), (u32, u64, ProfEntry)> = HashMap::new();
        for p in &done {
            let mut mine: HashMap<(String, u64), ()> = HashMap::new();
            for e in &p.entries {
                if e.len == 0 {
                    continue;
                }
                for g in e.offset / GRAIN..=(e.offset + e.len - 1) / GRAIN {
                    let k = (e.path.clone(), g);
                    let first_in_session = mine.insert(k.clone(), ()).is_none();
                    let v = seen.entry(k).or_insert((0, e.t_ms, e.clone()));
                    if first_in_session {
                        v.0 += 1;
                    }
                    v.1 = v.1.min(e.t_ms);
                    v.2 = e.clone(); // later sessions overwrite: newest version wins
                }
            }
        }
        let mut keep: Vec<((String, u64), (u32, u64, ProfEntry))> = seen.into_iter().filter(|(_, v)| v.0 >= need).collect();
        keep.sort_by(|a, b| a.1 .1.cmp(&b.1 .1).then(a.0 .0.cmp(&b.0 .0)).then(a.0 .1.cmp(&b.0 .1)));
        let mut out: Vec<ProfEntry> = Vec::new();
        let mut spent = 0u64;
        for ((path, g), (_, t, e)) in keep {
            if spent >= budget {
                break;
            }
            let off = g * GRAIN;
            if off >= e.size {
                continue;
            }
            let len = GRAIN.min(e.size - off);
            match out.last_mut() {
                Some(l) if l.path == path && l.offset + l.len == off => l.len += len,
                _ => out.push(ProfEntry { path, id: e.id, size: e.size, version: e.version, offset: off, len, t_ms: t }),
            }
            spent += len;
        }
        Profile { entries: out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(path: &str, off: u64, len: u64, t: u64) -> ProfEntry {
        ProfEntry { path: path.into(), id: path.into(), size: 100 << 20, version: "v".into(), offset: off, len, t_ms: t }
    }

    #[test]
    fn single_session_is_replayed_in_order() {
        let mut h = History::default();
        let i = h.begin();
        h.sessions[i].entries = vec![ent("/b", 5 << 20, 10, 50), ent("/a", 0, 3 << 20, 10)];
        let p = h.plan(u64::MAX);
        assert_eq!(p.entries[0].path, "/a");
        assert_eq!((p.entries[0].offset, p.entries[0].len), (0, 3 << 20));
        assert_eq!(p.entries[1].path, "/b");
    }

    #[test]
    fn with_two_sessions_only_recurring_regions_count() {
        let mut h = History::default();
        let a = h.begin();
        h.sessions[a].entries = vec![ent("/a", 0, 1 << 20, 0), ent("/once", 0, 1 << 20, 5)];
        let b = h.begin();
        h.sessions[b].entries = vec![ent("/a", 0, 1 << 20, 3), ent("/other", 0, 1 << 20, 1)];
        let p = h.plan(u64::MAX);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].path, "/a");
        assert_eq!(p.entries[0].t_ms, 0);
    }

    #[test]
    fn budget_and_session_cap() {
        let mut h = History::default();
        for _ in 0..6 {
            let i = h.begin();
            h.sessions[i].entries = vec![ent("/a", 0, 10 << 20, 0)];
        }
        assert_eq!(h.sessions.len(), MAX_SESSIONS);
        assert_eq!(h.plan(3 << 20).total_bytes(), 3 << 20);
    }
}
