//! Turns watcher events and metadata updates into the presence that should be shown.
//!
//! Each double-clicked comic opens its own CDisplayEx window. The presence follows
//! the window the reader used last: the one that turned a page or came to the front.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Language;
use crate::meta::worker::Job;
use crate::meta::{filename, ComicMeta};
use crate::presence::{format, Activity, PresenceMsg};
use crate::resolver::{Resolved, Resolver};
use crate::watcher::{locks, WatchEvent};

/// CDisplayEx may update the title a few milliseconds before writing the registry,
/// so an unresolved title is retried for a while.
const RETRY_INTERVAL: Duration = Duration::from_millis(400);
const MAX_ATTEMPTS: u32 = 10;
const IDLE_TITLE: &str = "CDisplayEx";
/// After CDisplayEx writes its registry, as when leaving a comic, the windows are
/// checked this often and this long for another file being loaded (measured:
/// written ~100 ms before the new file, which is held for ~150 ms).
const SWITCH_POLL: Duration = Duration::from_millis(20);
const SWITCH_WATCH: Duration = Duration::from_millis(1500);

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
    due: Instant,
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
    /// Current title of each window.
    titles: HashMap<u32, String>,
    /// Until when windows are checked for a file switch.
    watch_until: Option<Instant>,
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
        titles: HashMap::new(),
        watch_until: None,
        metas: HashMap::new(),
        requested: HashSet::new(),
        started: HashMap::new(),
        showing: false,
    };
    loop {
        let input = match engine.next_wake() {
            Some(wait) => match inputs.recv_timeout(wait) {
                Ok(i) => Some(i),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match inputs.recv() {
                Ok(i) => Some(i),
                Err(_) => return,
            },
        };
        match input {
            Some(Input::Watch(WatchEvent::Title { pid, title })) => engine.on_title(pid, title, 1),
            Some(Input::Watch(WatchEvent::Focus(pid))) => engine.on_focus(pid),
            Some(Input::Watch(WatchEvent::Closed(pid))) => engine.on_closed(pid),
            Some(Input::Watch(WatchEvent::Registry)) => engine.on_registry(),
            Some(Input::Meta(path, meta)) => engine.on_meta(path, meta),
            None => engine.on_timer(),
        }
    }
}

impl Engine {
    /// How long until a retry or a switch check is due; `None` if nothing is.
    fn next_wake(&self) -> Option<Duration> {
        let retry = self.pending.as_ref().map(|p| p.due.saturating_duration_since(Instant::now()));
        let watch = self.watch_until.map(|_| SWITCH_POLL);
        retry.into_iter().chain(watch).min()
    }

    fn on_timer(&mut self) {
        if self.pending.as_ref().is_some_and(|p| p.due <= Instant::now()) {
            if let Some(p) = self.pending.take() {
                self.on_title(p.pid, p.title, p.attempt + 1);
            }
        }
        if self.watch_until.is_some_and(|t| t <= Instant::now()) {
            self.watch_until = None;
        }
        if self.watch_until.is_some() {
            self.check_switches();
        }
    }

    /// CDisplayEx wrote its registry, as it does on leaving a comic: watch the
    /// windows showing one for a switch to another file.
    fn on_registry(&mut self) {
        if !self.windows.is_empty() {
            self.watch_until = Some(Instant::now() + SWITCH_WATCH);
            self.check_switches();
        }
    }

    fn check_switches(&mut self) {
        let pids: Vec<u32> = self.windows.keys().copied().collect();
        for pid in pids {
            let title = self.titles.get(&pid).cloned().unwrap_or_default();
            if let Some(r) = self.resolver.switched(pid, &title) {
                if self.pending.as_ref().is_some_and(|p| p.pid == pid) {
                    self.pending = None;
                }
                self.touch(pid);
                let shown = self.show_comic(r);
                self.windows.insert(pid, shown);
                self.render();
            }
        }
    }

    fn on_title(&mut self, pid: u32, title: String, attempt: u32) {
        if self.pending.as_ref().is_some_and(|p| p.pid == pid) {
            self.pending = None;
        }
        self.titles.insert(pid, title.clone());
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
            None if attempt < MAX_ATTEMPTS => {
                self.pending = Some(Pending { pid, title, attempt, due: Instant::now() + RETRY_INTERVAL })
            }
            None => {
                log!("could not find the archive for title [{title}]");
                // A double-clicked file in a format that cannot be read (e.g. CB7)
                // is at least shown by its name.
                let shown = match locks::command_line_comic(pid) {
                    Some(path) => path.to_string_lossy().into_owned(),
                    None => title,
                };
                self.windows.insert(pid, Shown::Unknown(shown));
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
        self.titles.remove(&pid);
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

    /// `title` is a window title (the comic's name is its first component, for
    /// archives inside archives) or the full path of an unreadable file.
    fn unknown_activity(&mut self, title: &str) -> Activity {
        let first = if Path::new(title).is_absolute() { title } else { title.split(['\\', '/']).next().unwrap_or(title) };
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::*;
    use crate::archive;

    fn engine() -> (Engine, Receiver<PresenceMsg>, Receiver<Job>) {
        let (presence, presence_rx) = mpsc::channel();
        let (jobs, jobs_rx) = mpsc::channel();
        let engine = Engine {
            resolver: Resolver::default(),
            presence,
            jobs,
            language: Language::En,
            windows: HashMap::new(),
            recent: Vec::new(),
            pending: None,
            titles: HashMap::new(),
            watch_until: None,
            metas: HashMap::new(),
            requested: HashSet::new(),
            started: HashMap::new(),
            showing: false,
        };
        (engine, presence_rx, jobs_rx)
    }

    fn comic(name: &str, page: usize) -> Resolved {
        let path = archive::test_comic(name, 20);
        let listing = Arc::new(archive::list(&path).unwrap());
        Resolved { path, page, listing }
    }

    /// Simulates a window showing `r`, as `on_title` does once resolved.
    fn open(e: &mut Engine, pid: u32, r: Resolved) {
        e.touch(pid);
        let shown = e.show_comic(r);
        e.windows.insert(pid, shown);
        e.render();
    }

    fn last(rx: &Receiver<PresenceMsg>) -> Option<Activity> {
        match rx.try_iter().last().expect("a presence update") {
            PresenceMsg::Set(a) => Some(a),
            PresenceMsg::Clear => None,
        }
    }

    #[test]
    fn file_name_first_then_cover_from_metadata() {
        let (mut e, presence, jobs) = engine();
        let r = comic("Engine Cover 004.cbz", 2);
        open(&mut e, 1, r.clone());

        let a = last(&presence).unwrap();
        assert_eq!(a.details, "Engine Cover 004");
        assert_eq!(a.state.as_deref(), Some("Page 3 of 20"));
        assert_eq!(a.large_image, None);
        assert_eq!(jobs.try_iter().count(), 1);

        let meta = ComicMeta {
            kind: crate::meta::Kind::Issue,
            title: "Engine Cover".into(),
            number: Some("4".into()),
            cover: Some("https://covers/4.jpg".into()),
            ..Default::default()
        };
        e.on_meta(r.path.clone(), Arc::new(meta));
        let b = last(&presence).unwrap();
        assert_eq!(b.details, "Engine Cover #4");
        assert_eq!(b.large_image.as_deref(), Some("https://covers/4.jpg"));
        assert_eq!(b.start, a.start, "the reading timer keeps running");

        // Page turns do not ask for metadata again.
        open(&mut e, 1, Resolved { page: 3, ..r });
        assert_eq!(last(&presence).unwrap().state.as_deref(), Some("Engine Cover · Page 4 of 20"));
        assert_eq!(jobs.try_iter().count(), 0);
    }

    #[test]
    fn unidentified_comics_are_named_by_file() {
        let (mut e, _presence, _jobs) = engine();
        // An unreadable double-clicked file: its path.
        assert_eq!(e.unknown_activity(r"D:\Comics\Saga 012 (2013) (Digital).cb7").state.as_deref(), Some("Saga 012 (2013)"));
        // A window title: the archive inside the archive, not the page.
        assert_eq!(e.unknown_activity(r"Saga 012.cbr\page 003.jpg").state.as_deref(), Some("Saga 012"));
    }

    #[test]
    fn metadata_for_a_background_window_changes_nothing() {
        let (mut e, presence, _jobs) = engine();
        let a = comic("Engine Back 001.cbz", 0);
        open(&mut e, 1, a.clone());
        open(&mut e, 2, comic("Engine Front 001.cbz", 0));
        presence.try_iter().count();

        e.on_meta(a.path, Arc::new(ComicMeta { title: "Back".into(), ..Default::default() }));
        assert!(presence.try_recv().is_err());
    }

    #[test]
    fn follows_the_last_used_window_and_clears_when_all_close() {
        let (mut e, presence, _jobs) = engine();
        open(&mut e, 1, comic("Engine First 001.cbz", 0));
        open(&mut e, 2, comic("Engine Second 001.cbz", 0));
        assert_eq!(last(&presence).unwrap().details, "Engine Second 001");

        e.on_focus(1);
        assert_eq!(last(&presence).unwrap().details, "Engine First 001");

        e.on_closed(1);
        assert_eq!(last(&presence).unwrap().details, "Engine Second 001");

        e.on_closed(2);
        assert_eq!(last(&presence), None);
        // Nothing more to clear.
        e.render();
        assert!(presence.try_recv().is_err());
    }
}
