//! Maps a CDisplayEx window title (current image path) to the open comic file and page.
//!
//! CDisplayEx exposes no file path while reading, so candidates come from its registry
//! history and are confirmed by checking that the archive actually contains the image.
//! Page names like `001.jpg` exist in many archives, so a switch to another file is
//! settled by stronger evidence first: the file CDisplayEx holds open while loading,
//! then a registry entry created by a first-time open. A file opened from the menu
//! may not change the title at all; `switched` finds it while it is being loaded,
//! after CDisplayEx records the comic it left in the registry.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::archive::{self, Listing};
use crate::watcher::locks;
use crate::watcher::registry::{self, ComicEntry, Snapshot};

/// Attempts (see `engine`) to wait for stronger evidence before guessing among
/// several archives that all contain a generically named page.
const GUESS_AFTER_ATTEMPT: u32 = 3;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: PathBuf,
    /// Zero-based, as CDisplayEx counts.
    pub page: usize,
    pub listing: Arc<Listing>,
}

impl Resolved {
    /// Page count, if known.
    pub fn total(&self) -> Option<usize> {
        self.listing.total_known.then_some(self.listing.pages.len())
    }
}

struct Cached {
    size: u64,
    modified: Option<SystemTime>,
    /// `None` when the archive could not be read; kept so it is not retried every page turn.
    listing: Option<Arc<Listing>>,
}

#[derive(Default)]
pub struct Resolver {
    cache: HashMap<PathBuf, Cached>,
    /// Comic shown in each CDisplayEx process.
    current: HashMap<u32, (PathBuf, Arc<Listing>)>,
    /// `Comics` ids already present; a new one marks a first-time open.
    known_ids: Option<HashSet<u32>>,
    /// Processes whose command-line comic was already shown; later files in the
    /// same window come from its menu.
    used_command_line: HashSet<u32>,
}

impl Resolver {
    /// The window shows no comic: forget its file and remember which registry
    /// entries exist, so the next first-time open stands out.
    pub fn reset(&mut self, pid: u32) {
        self.current.remove(&pid);
        self.known_ids = Some(registry::read().comics.iter().map(|c| c.id).collect());
    }

    /// The comic a window was started with, before CDisplayEx has finished loading
    /// it (seconds for a large archive). The page is the one CDisplayEx saved last
    /// time, where it will reopen.
    pub fn preload(&mut self, pid: u32) -> Option<Resolved> {
        if self.used_command_line.contains(&pid) {
            return None;
        }
        let path = locks::command_line_comic(pid)?;
        let listing = self.listing(&path)?;
        let saved = saved_page(&path, &listing);
        self.used_command_line.insert(pid);
        log!("resolved: command line, while CDisplayEx loads");
        Some(self.select(pid, path, listing, saved.unwrap_or(0)))
    }

    /// Another comic the window is loading, if any: one opened from the menu may
    /// keep the title unchanged (its first page named like the current one) or
    /// name a page the current comic has too, so it is caught by the file
    /// CDisplayEx holds open while loading. `title` is the window's current title.
    pub fn switched(&mut self, pid: u32, title: &str) -> Option<Resolved> {
        let current = self.current.get(&pid).map(|(p, _)| p.clone());
        let is_current = |p: &Path| current.as_ref().is_some_and(|c| p.as_os_str().eq_ignore_ascii_case(c.as_os_str()));
        let path = locks::open_comics(pid).into_iter().find(|p| !is_current(p))?;
        let listing = self.listing(&path)?;
        // The title may still be the old comic's; a new one follows if the page differs.
        let page = listing.find(title).or_else(|| saved_page(&path, &listing)).unwrap_or(0);
        log!("resolved: window switched to another file");
        Some(self.select(pid, path, listing, page))
    }

    /// The window was closed.
    pub fn forget(&mut self, pid: u32) {
        self.current.remove(&pid);
        self.used_command_line.remove(&pid);
    }

    /// `attempt` counts retries of the same title (1 = first try).
    pub fn resolve(&mut self, pid: u32, title: &str, attempt: u32) -> Option<Resolved> {
        // Fast path: a page turn within the comic already open in this window.
        if let Some((path, listing)) = self.current.get(&pid) {
            if let Some(page) = listing.find(title) {
                return Some(Resolved { path: path.clone(), page, listing: listing.clone() });
            }
        }

        // 1. A window started by double-clicking a comic shows it first.
        if !self.used_command_line.contains(&pid) {
            if let Some(path) = locks::command_line_comic(pid) {
                if let Some(r) = self.try_select(pid, &path, title, "command line") {
                    self.used_command_line.insert(pid);
                    return Some(r);
                }
            }
        }
        // 2. The archive this window is loading right now. Checked before anything
        //    slow, since the file is released a few dozen milliseconds after the title.
        let started = Instant::now();
        let open = locks::open_comics(pid);
        log!("open in CDisplayEx: {open:?} (checked in {:?})", started.elapsed());
        for path in open {
            if let Some(r) = self.try_select(pid, &path, title, "held open by CDisplayEx") {
                return Some(r);
            }
        }

        let snap = registry::read();
        let candidates = candidates(&snap, title);
        let known = self.known_ids.replace(snap.comics.iter().map(|c| c.id).collect());
        // 3. A registry entry that did not exist before: a first-time open.
        if let Some(known) = &known {
            for entry in candidates.iter().filter(|c| !known.contains(&c.id)) {
                if let Some(r) = self.try_select(pid, &entry.path, title, "new registry entry") {
                    return Some(r);
                }
            }
        }

        let mut matches = Vec::new();
        for entry in &candidates {
            let Some(listing) = self.listing(&entry.path) else { continue };
            if let Some(page) = listing.find(title) {
                matches.push((entry, listing, page));
            }
        }
        // 4. Only one archive has this page.
        if matches.len() == 1 || (is_distinctive(title) && !matches.is_empty()) {
            let (entry, listing, page) = matches.swap_remove(0);
            log!("resolved by page name");
            return Some(self.select(pid, entry.path.clone(), listing, page));
        }
        // 5. Ambiguous: give the registry a moment, then pick the archive whose saved
        //    page matches, since CDisplayEx reopens a comic where it was left.
        if matches.is_empty() || attempt < GUESS_AFTER_ATTEMPT {
            return None;
        }
        let pick = matches.iter().position(|(e, _, p)| e.last_page == Some(*p as u32)).unwrap_or(0);
        let (entry, listing, page) = matches.swap_remove(pick);
        log!("guessed among {} archives containing [{title}]", matches.len() + 1);
        Some(self.select(pid, entry.path.clone(), listing, page))
    }

    fn try_select(&mut self, pid: u32, path: &Path, title: &str, how: &str) -> Option<Resolved> {
        let listing = self.listing(path)?;
        let page = listing.find(title)?;
        log!("resolved: {how}");
        Some(self.select(pid, path.to_path_buf(), listing, page))
    }

    fn select(&mut self, pid: u32, path: PathBuf, listing: Arc<Listing>, page: usize) -> Resolved {
        self.current.insert(pid, (path.clone(), listing.clone()));
        Resolved { path, page, listing }
    }

    fn listing(&mut self, path: &Path) -> Option<Arc<Listing>> {
        let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        let (size, modified) = (meta.len(), meta.modified().ok());
        if let Some(c) = self.cache.get(path).filter(|c| c.size == size && c.modified == modified) {
            return c.listing.clone();
        }

        let started = Instant::now();
        let listing = match archive::list(path) {
            Ok(l) => {
                log!("listed {} pages in {:?}: {}", l.pages.len(), started.elapsed(), path.display());
                Some(Arc::new(l))
            }
            Err(e) => {
                log!("cannot read {}: {e}", path.display());
                None
            }
        };
        self.cache.insert(path.to_path_buf(), Cached { size, modified, listing: listing.clone() });
        listing
    }
}

/// The page CDisplayEx saved for `path`, where it reopens the comic.
fn saved_page(path: &Path, listing: &Listing) -> Option<usize> {
    registry::read()
        .comics
        .into_iter()
        .find(|c| c.path.as_os_str().eq_ignore_ascii_case(path.as_os_str()))
        .and_then(|c| c.last_page)
        .map(|p| p as usize)
        .filter(|&p| p < listing.pages.len())
}

/// Most likely files first: name resembles the title, newest registry entry
/// (a first-time open), inside the last opened folder, then by history order.
fn candidates(snap: &Snapshot, title: &str) -> Vec<ComicEntry> {
    let key = title_key(title);
    let newest = snap.comics.iter().map(|c| c.id).max();
    let mut list = snap.comics.clone();
    list.sort_by_key(|c| {
        let similar = !key.is_empty() && alnum_lower(&c.path.to_string_lossy()).contains(&key);
        let in_folder = snap.last_open_folder.as_deref().is_some_and(|f| c.path.parent() == Some(trim_sep(f)));
        let history = snap.history.iter().position(|&id| id == c.id).unwrap_or(usize::MAX);
        (Reverse(similar), Reverse(Some(c.id) == newest), Reverse(in_folder), history)
    });
    list
}

fn trim_sep(p: &Path) -> &Path {
    p.to_str().map(|s| Path::new(s.trim_end_matches(['\\', '/']))).unwrap_or(p)
}

fn alnum_lower(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// First letters of the title's leading path component, used to match file names.
fn title_key(title: &str) -> String {
    let first = title.split(['\\', '/']).next().unwrap_or(title);
    let key: String = alnum_lower(first).chars().take(8).collect();
    if key.chars().filter(|c| c.is_alphabetic()).count() >= 4 { key } else { String::new() }
}

fn is_distinctive(title: &str) -> bool {
    let name = title.rsplit(['\\', '/']).next().unwrap_or(title);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.chars().filter(|c| c.is_alphabetic()).count() >= 4
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, path: &str) -> ComicEntry {
        ComicEntry { id, path: PathBuf::from(path), last_page: None }
    }

    #[test]
    fn similar_names_come_first() {
        let snap = Snapshot {
            comics: vec![entry(5, r"D:\C\Runaways 01.cbr"), entry(3, r"D:\C\Ghost Rider v01 (2023).cbz")],
            history: vec![5, 3],
            last_open_folder: None,
        };
        let order = candidates(&snap, "Ghost Rider v01 00003.jpg");
        assert_eq!(order[0].id, 3);
    }

    #[test]
    fn newest_entry_wins_without_similarity() {
        let snap = Snapshot {
            comics: vec![entry(5, r"D:\C\a.cbr"), entry(9, r"D:\C\b.cbr")],
            history: vec![5],
            last_open_folder: None,
        };
        assert_eq!(candidates(&snap, "001.jpg")[0].id, 9);
    }

    #[test]
    fn distinctive_titles() {
        assert!(is_distinctive(r"Inner.cbr\Dishonored 001-000.jpg"));
        assert!(!is_distinctive(r"Vol 1\001.jpg"));
    }
}
