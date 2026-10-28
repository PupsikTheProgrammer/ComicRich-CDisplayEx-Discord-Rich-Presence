//! Turns watcher events and metadata updates into the presence that should be shown.
//!
//! Each double-clicked comic opens its own CDisplayEx window. The presence follows
//! the window the reader used last: the one that turned a page or came to the front.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Language;
use crate::meta::worker::Job;
use crate::meta::{filename, ComicMeta};
use crate::presence::{format, Activity, PresenceMsg};
use crate::resolver::{Resolved, Resolver};
use crate::watcher::WatchEvent;

/// CDisplayEx may update the title a few milliseconds before writing the registry,
/// so an unresolved title is retried for a while.
const RETRY_INTERVAL: Duration = Duration::from_millis(400);
const MAX_ATTEMPTS: u32 = 10;
const IDLE_TITLE: &str = "CDisplayEx";

pub enum Input {
    Watch(WatchEvent),
    Meta(PathBuf, Arc<ComicMeta>),
}

/// What one CDisplayEx window shows.
enum Shown {
    Nothing,
    Comic(Resolved),
    /// A page whose archive could not be identified.
    Unknown(String),
}

struct Pending {
    pid: u32,
    title: String,
    attempt: u32,
}

struct Engine {
    resolver: Resolver,
    presence: Sender<PresenceMsg>,
    jobs: Sender<Job>,
    language: Language,
    windows: HashMap<u32, Shown>,
    /// Window pids, least recently used first.
    recent: Vec<u32>,
    pending: Option<Pending>,
    metas: HashMap<PathBuf, Arc<ComicMeta>>,
    requested: HashSet<PathBuf>,
    /// When reading of each open comic started (Unix seconds).
    started: HashMap<PathBuf, u64>,
    showing: bool,
}

pub fn run(inputs: Receiver<Input>, presence: Sender<PresenceMsg>, jobs: Sender<Job>, language: Language) {
    let mut engine = Engine {
        resolver: Resolver::default(),
        presence,
        jobs,
        language,
        windows: HashMap::new(),
        recent: Vec::new(),
        pending: None,
        metas: HashMap::new(),
        requested: HashSet::new(),
        started: HashMap::new(),
        showing: false,
    };
    loop {
        let input = if engine.pending.is_some() {
            match inputs.recv_timeout(RETRY_INTERVAL) {
                Ok(i) => Some(i),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match inputs.recv() {
                Ok(i) => Some(i),
                Err(_) => return,
            }
        };
        match input {
            Some(Input::Watch(WatchEvent::Title { pid, title })) => engine.on_title(pid, title, 1),
            Some(Input::Watch(WatchEvent::Focus(pid))) => engine.on_focus(pid),
            Some(Input::Watch(WatchEvent::Closed(pid))) => engine.on_closed(pid),
            Some(Input::Meta(path, meta)) => engine.on_meta(path, meta),
            None => {
                if let Some(p) = engine.pending.take() {
                    engine.on_title(p.pid, p.title, p.attempt + 1);
                }
            }
        }
    }
}

impl Engine {
    fn on_title(&mut self, pid: u32, title: String, attempt: u32) {
        if self.pending.as_ref().is_some_and(|p| p.pid == pid) {
            self.pending = None;
        }
        self.touch(pid);
        if title.is_empty() || title.eq_ignore_ascii_case(IDLE_TITLE) {
            // A freshly started window is still loading its command-line comic.
            let shown = match self.resolver.preload(pid) {
                Some(r) => self.show_comic(r),
                None => {
                    self.resolver.reset(pid);
                    Shown::Nothing
                }
            };
            self.windows.insert(pid, shown);
            self.render();
            return;
        }
        match self.resolver.resolve(pid, &title, attempt) {
            Some(r) => {
                let shown = self.show_comic(r);
                self.windows.insert(pid, shown);
                self.render();
            }
            None if attempt < MAX_ATTEMPTS => self.pending = Some(Pending { pid, title, attempt }),
            None => {
                log!("could not find the archive for title [{title}]");
                self.windows.insert(pid, Shown::Unknown(title));
                self.render();
            }
        }
    }

    fn show_comic(&mut self, r: Resolved) -> Shown {
        log!("page {}/{} of {}", r.page + 1, r.listing.pages.len(), r.path.display());
        if self.requested.insert(r.path.clone()) {
            let _ = self.jobs.send(Job { path: r.path.clone(), listing: r.listing.clone(), refresh: false });
        }
        Shown::Comic(r)
    }

    fn on_focus(&mut self, pid: u32) {
        if self.windows.contains_key(&pid) && self.recent.last() != Some(&pid) {
            log!("switched to CDisplayEx window pid={pid}");
            self.touch(pid);
            self.render();
        }
    }

    fn on_closed(&mut self, pid: u32) {
        self.windows.remove(&pid);
        self.recent.retain(|&p| p != pid);
        self.resolver.forget(pid);
        if self.pending.as_ref().is_some_and(|p| p.pid == pid) {
            self.pending = None;
        }
        self.render();
    }

    fn on_meta(&mut self, path: PathBuf, meta: Arc<ComicMeta>) {
        self.metas.insert(path.clone(), meta);
        if matches!(self.active(), Some(Shown::Comic(r)) if r.path == path) {
            self.render();
        }
    }

    fn touch(&mut self, pid: u32) {
        self.recent.retain(|&p| p != pid);
        self.recent.push(pid);
    }

    /// The most recently used window that shows something.
    fn active(&self) -> Option<&Shown> {
        self.recent
            .iter()
            .rev()
            .filter_map(|pid| self.windows.get(pid))
            .find(|s| !matches!(s, Shown::Nothing))
    }

    fn render(&mut self) {
        let activity = match self.active() {
            Some(Shown::Comic(r)) => {
                let r = r.clone();
                Some(self.comic_activity(&r))
            }
            Some(Shown::Unknown(title)) => {
                let title = title.clone();
                Some(self.unknown_activity(&title))
            }
            _ => None,
        };
        // Reading timers only for comics still open somewhere.
        let open: HashSet<PathBuf> = self
            .windows
            .values()
            .filter_map(|s| match s {
                Shown::Comic(r) => Some(r.path.clone()),
                _ => None,
            })
            .collect();
        self.started.retain(|p, _| open.contains(p));

        match activity {
            Some(a) => {
                self.showing = true;
                let _ = self.presence.send(PresenceMsg::Set(a));
            }
            None if self.showing => {
                log!("nothing open, clearing presence");
                self.showing = false;
                let _ = self.presence.send(PresenceMsg::Clear);
            }
            None => {}
        }
    }

    fn start_for(&mut self, path: &Path) -> u64 {
        *self.started.entry(path.to_path_buf()).or_insert_with(unix_now)
    }

    fn comic_activity(&mut self, r: &Resolved) -> Activity {
        // Until the metadata thread answers, show what the file name says.
        let fallback;
        let meta = match self.metas.get(&r.path) {
            Some(m) => m.as_ref(),
            None => {
                fallback = ComicMeta { title: filename::clean(&r.path), ..Default::default() };
                &fallback
            }
        };
        let lines = format::lines(self.language, meta, r.page + 1, r.total());
        Activity {
            details: lines.details,
            state: lines.state,
            large_image: lines.large_image,
            large_text: lines.large_text,
            small_image: lines.small_image,
            small_text: lines.small_text,
            start: self.start_for(&r.path),
        }
    }

    fn unknown_activity(&mut self, title: &str) -> Activity {
        let first = title.split(['\\', '/']).next().unwrap_or(title);
        let name = filename::clean(first.as_ref());
        Activity {
            details: format::reading_line(self.language).to_string(),
            state: Some(name.clone()),
            large_image: None,
            large_text: name,
            small_image: None,
            small_text: None,
            start: self.start_for(Path::new(title)),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
