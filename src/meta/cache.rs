//! Remembers metadata per file (path + size + modification time) and cover hashes
//! per URL, so each comic is looked up online only once. Only text is stored,
//! about 2 KB per comic. Stored in `%LOCALAPPDATA%\ComicRich\cache.json`.
//!
//! Pruned on start: comics whose file is gone or changed, comics not opened for
//! a year, cover hashes no remaining comic refers to, and the least recently
//! opened comics beyond a size cap.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ComicMeta;

/// Bump when cached metadata would be interpreted differently.
const VERSION: u32 = 1;
const MAX_AGE_SECS: u64 = 365 * 24 * 3600;
/// About 5000 comics; far beyond what a year of reading leaves behind.
const MAX_BYTES: usize = 10 << 20;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    version: u32,
    comics: HashMap<String, ComicMeta>,
    covers: HashMap<String, u64>,
    /// When each comic was last opened (Unix seconds).
    used: HashMap<String, u64>,
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl Cache {
    pub fn load() -> Self {
        let file = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("ComicRich").join("cache.json"));
        let mut cache: Cache = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .filter(|c: &Cache| c.version == VERSION)
            .unwrap_or_default();
        cache.version = VERSION;
        cache.file = file;
        if cache.prune(now()) {
            cache.save();
        }
        cache
    }

    pub fn save(&self) {
        let Some(file) = &self.file else { return };
        let result = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(file, serde_json::to_vec(self).unwrap_or_default()));
        if let Err(e) = result {
            log!("cannot save cache {}: {e}", file.display());
        }
    }

    pub fn comic(&self, key: &str) -> Option<&ComicMeta> {
        self.comics.get(key)
    }

    pub fn set_comic(&mut self, key: String, meta: ComicMeta) {
        self.used.insert(key.clone(), now());
        self.comics.insert(key, meta);
    }

    /// Records that a cached comic was opened again.
    pub fn touch(&mut self, key: &str) {
        self.used.insert(key.to_string(), now());
    }

    pub fn cover(&self, url: &str) -> Option<u64> {
        self.covers.get(url).copied()
    }

    pub fn set_cover(&mut self, url: String, hash: u64) {
        self.covers.insert(url, hash);
    }

    /// Returns true if anything was removed.
    fn prune(&mut self, now: u64) -> bool {
        let before = (self.comics.len(), self.covers.len());

        for k in self.comics.keys() {
            self.used.entry(k.clone()).or_insert(now);
        }
        let used = &self.used;
        self.comics.retain(|k, _| {
            let fresh = used.get(k).is_some_and(|&t| now.saturating_sub(t) < MAX_AGE_SECS);
            // The key embeds size and modification time: a changed file gets a new key.
            fresh && path_of(k).is_some_and(|p| key(&p).as_deref() == Some(k.as_str()))
        });

        // Least recently opened first out, until under the size cap.
        let mut by_age: Vec<(u64, String)> =
            self.comics.keys().map(|k| (self.used.get(k).copied().unwrap_or(0), k.clone())).collect();
        by_age.sort();
        let mut size: usize = self.comics.iter().map(|(k, m)| k.len() + approx_size(m)).sum();
        for (_, k) in by_age {
            if size <= MAX_BYTES {
                break;
            }
            if let Some(m) = self.comics.remove(&k) {
                size -= k.len() + approx_size(&m);
            }
        }

        let comics = &self.comics;
        self.used.retain(|k, _| comics.contains_key(k));
        let referenced: HashSet<&str> = self
            .comics
            .values()
            .flat_map(|m| m.segments.iter().filter_map(|s| s.cover.as_deref()).chain(m.cover.as_deref()))
            .collect();
        self.covers.retain(|url, _| referenced.contains(url.as_str()));

        let removed = (before.0 - self.comics.len(), before.1 - self.covers.len());
        if removed != (0, 0) {
            log!("cache: removed {} comics and {} cover hashes", removed.0, removed.1);
        }
        removed != (0, 0)
    }
}

pub fn key(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}|{}|{modified}", path.to_string_lossy().to_lowercase(), meta.len()))
}

fn path_of(key: &str) -> Option<PathBuf> {
    let mut parts = key.rsplitn(3, '|');
    let (_, _, path) = (parts.next()?, parts.next()?, parts.next()?);
    Some(PathBuf::from(path))
}

fn approx_size(m: &ComicMeta) -> usize {
    serde_json::to_vec(m).map_or(0, |v| v.len())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::Segment;

    #[test]
    fn prunes_missing_stale_and_orphaned() {
        let own = std::env::current_exe().unwrap();
        let live = key(&own).unwrap();
        let gone = r"c:\no\such\file.cbz|100|1".to_string();
        let stale = format!("{}|1|1", own.to_string_lossy().to_lowercase()); // size/time changed
        let seg = |url: &str| Segment { start: 0, label: "x".into(), cover: Some(url.into()), exact: true };

        let mut c = Cache::default();
        let meta = ComicMeta { segments: vec![seg("kept")], ..Default::default() };
        c.comics.insert(live.clone(), meta);
        c.comics.insert(gone.clone(), ComicMeta::default());
        c.comics.insert(stale.clone(), ComicMeta::default());
        c.covers.insert("kept".into(), 1);
        c.covers.insert("orphan".into(), 2);

        assert!(c.prune(1_000_000_000));
        assert_eq!(c.comics.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(c.covers.keys().collect::<Vec<_>>(), ["kept"]);

        // Not opened for over a year.
        assert!(c.prune(1_000_000_000 + MAX_AGE_SECS + 1));
        assert!(c.comics.is_empty());
    }
}
