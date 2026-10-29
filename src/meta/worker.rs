//! Background metadata threads: cached or file metadata first, then online
//! lookup, then locating issues inside collections. Online lookups have a thread
//! of their own, so a slow database never holds up a comic that is already
//! cached. Both run in Windows background mode (lowest CPU and I/O priority) and
//! publish each improvement as soon as it is known.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};

use super::cache::{self, Cache, Shared};
use super::comicvine::ComicVine;
use super::metron::Metron;
use super::online::Source;
use super::imagehash::{self, PageHashes};
use super::{segments, ComicMeta, Kind};
use crate::archive::{self, Listing};
use crate::config::Config;
use crate::http::{Host, Http};
use crate::rate::Limit;

/// Collections reprinting more issues than this are not cover-matched.
const MAX_COVER_LOOKUPS: usize = 80;
const COVER_HOST: Host = Host { key: "covers", limit: Limit::every(Duration::from_millis(250)) };
const MAX_COVER_BYTES: u64 = 8 << 20;

pub struct Job {
    pub path: PathBuf,
    pub listing: Arc<Listing>,
    /// Ignore cached metadata for this file (diagnostics).
    pub refresh: bool,
}

/// Lowest CPU and I/O priority for the calling thread.
pub fn background_mode() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN as i32) };
}

type Publish = Arc<dyn Fn(PathBuf, Arc<ComicMeta>) + Send + Sync>;

/// Serves jobs until `jobs` is closed and every started lookup has finished.
pub fn run(jobs: Receiver<Job>, config: &Config, publish: impl Fn(PathBuf, Arc<ComicMeta>) + Send + Sync + 'static) {
    let cache: Shared = Arc::new(Mutex::new(Cache::load()));
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    if config.online {
        if let Some(auth) = config.metron.authorization() {
            sources.push(Box::new(Metron::new(auth, cache.clone())));
        }
        if !config.comicvine.api_key.is_empty() {
            sources.push(Box::new(ComicVine::new(&config.comicvine.api_key)));
        }
    }
    log!("online sources: {:?}", sources.iter().map(|s| s.name()).collect::<Vec<_>>());
    serve(jobs, sources, cache, Arc::new(publish));
}

fn serve(jobs: Receiver<Job>, sources: Vec<Box<dyn Source>>, cache: Shared, publish: Publish) {
    background_mode();
    let (online_tx, online_rx) = mpsc::channel();
    let online = (!sources.is_empty()).then(|| {
        let (cache, publish) = (cache.clone(), publish.clone());
        thread::spawn(move || look_up(online_rx, sources, cache, publish))
    });

    while let Ok(job) = jobs.recv() {
        let key = cache::key(&job.path);
        let cached = key.as_deref().filter(|_| !job.refresh).and_then(|k| cache::lock(&cache).comic(k).cloned());
        let fresh = cached.is_none();
        let meta = cached.unwrap_or_else(|| super::local(&job.path, &job.listing));
        log!("metadata ({}): {meta:?}", meta.source);
        publish(job.path.clone(), Arc::new(meta.clone()));

        if online.is_some() && !meta.online {
            let _ = online_tx.send((job, meta));
        } else if let Some(key) = key {
            let mut cache = cache::lock(&cache);
            if fresh {
                cache.set_comic(key, meta);
            } else {
                cache.touch(&key);
            }
            cache.save();
        }
    }
    drop(online_tx);
    if let Some(online) = online {
        let _ = online.join();
    }
}

/// The online thread: looks comics up and stores the results.
fn look_up(lookups: Receiver<(Job, ComicMeta)>, mut sources: Vec<Box<dyn Source>>, cache: Shared, publish: Publish) {
    background_mode();
    let mut http = Http::new();
    while let Ok(mut lookup) = lookups.recv() {
        // Only the latest comic matters if several were opened in a row.
        while let Ok(next) = lookups.try_recv() {
            lookup = next;
        }
        let (job, mut meta) = lookup;
        let started = Instant::now();
        let before = meta.clone();
        let mut found_cover = |cover: &str| {
            log!("cover found in {:?}: {cover}", started.elapsed());
            let early = ComicMeta { cover: Some(cover.to_string()), ..before.clone() };
            publish(job.path.clone(), Arc::new(early));
        };
        enrich(&mut meta, job.listing.pages.len(), &mut sources, &mut http, &mut found_cover);
        meta.online = true;
        log!("online lookup done in {:?}: {meta:?}", started.elapsed());
        publish(job.path.clone(), Arc::new(meta.clone()));

        if meta.kind == Kind::Collection && !meta.has_exact_segments() && meta.contents.len() >= 2 {
            let started = Instant::now();
            locate_issues(&mut meta, &job, &mut sources, &mut http, &cache);
            log!("issue detection done in {:?}", started.elapsed());
            publish(job.path.clone(), Arc::new(meta.clone()));
        }
        if let Some(key) = cache::key(&job.path) {
            let mut cache = cache::lock(&cache);
            cache.set_comic(key, meta);
            cache.save();
        }
    }
}

fn enrich(
    meta: &mut ComicMeta,
    total: usize,
    sources: &mut [Box<dyn Source>],
    http: &mut Http,
    found_cover: &mut dyn FnMut(&str),
) {
    let number = meta.number.clone().unwrap_or_else(|| "1".into());
    for source in sources.iter_mut() {
        let Some(m) = source.issue(http, &meta.title, meta.year, meta.volume, &number, found_cover) else { continue };
        log!("{} matched {} ({:?}) #{number}", source.name(), m.series, m.year);
        let db_kind = m.series_type.as_deref().and_then(Kind::from_format);
        // A database "series" for a collection is the collection itself; keep the local verdict.
        if meta.kind != Kind::Collection || db_kind == Some(Kind::Collection) {
            meta.kind = db_kind.unwrap_or(meta.kind);
        }
        meta.title = m.series;
        meta.year = meta.year.or(m.year);
        meta.volume = meta.volume.or(m.volume);
        if meta.kind == Kind::Limited {
            meta.count = meta.count.or(m.issue_count);
        }
        meta.issue_title = meta.issue_title.take().or(m.title);
        meta.cover = m.cover.or(meta.cover.take());
        meta.source = source.name().into();
        if meta.kind == Kind::Collection && meta.contents.is_empty() && !m.reprints.is_empty() {
            meta.contents = m.reprints;
            if !meta.has_exact_segments() {
                meta.segments = segments::estimate(total, &meta.contents, &[]);
            }
        }
        return;
    }
    log!("no online match for {:?} #{number}", meta.title);
}

/// Finds each reprinted issue's cover among the pages.
fn locate_issues(
    meta: &mut ComicMeta,
    job: &Job,
    sources: &mut [Box<dyn Source>],
    http: &mut Http,
    cache: &Shared,
) {
    let total = job.listing.pages.len();
    let mut urls: Vec<Option<String>> = Vec::new();
    let mut hashes: Vec<Option<u64>> = Vec::new();
    for issue in meta.contents.iter().take(MAX_COVER_LOOKUPS) {
        let url = sources.iter_mut().find_map(|s| s.cover(http, issue));
        let hash = url.as_ref().and_then(|u| {
            let cached = cache::lock(cache).cover(u);
            cached.or_else(|| {
                let h = imagehash::dhash(&http.bytes(COVER_HOST, u, MAX_COVER_BYTES)?)?;
                cache::lock(cache).set_cover(u.clone(), h);
                Some(h)
            })
        });
        log!("cover for {}: {}", issue.label(), url.as_deref().unwrap_or("not found"));
        urls.push(url);
        hashes.push(hash);
    }
    urls.resize(meta.contents.len(), None);

    let anchors = if hashes.iter().any(Option::is_some) {
        let pages = hash_pages(job);
        segments::match_covers(&pages, &hashes)
    } else {
        Vec::new()
    };
    meta.segments = segments::estimate_with_covers(total, &meta.contents, &anchors, &urls);
    for s in &meta.segments {
        log!("  page {:>4}: {}{}", s.start + 1, if s.exact { "" } else { "≈ " }, s.label);
    }
}

/// Hashes of every page, read in a single pass over the archive.
pub fn hash_pages(job: &Job) -> Vec<Option<PageHashes>> {
    let index: HashMap<&str, usize> = job.listing.lower().iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let mut hashes = vec![None; job.listing.pages.len()];
    let started = Instant::now();
    let mut decoding = Duration::ZERO;
    let result = archive::visit(
        &job.path,
        &|name| index.contains_key(name.to_lowercase().as_str()),
        &mut |name, bytes| {
            if let Some(&i) = index.get(name.to_lowercase().as_str()) {
                let t = Instant::now();
                hashes[i] = imagehash::page_hashes(&bytes);
                decoding += t.elapsed();
            }
            true
        },
    );
    if let Err(e) = result {
        log!("cannot read pages for cover matching: {e}");
    }
    log!(
        "hashed {}/{} pages in {:?} ({:?} decoding)",
        hashes.iter().filter(|h| h.is_some()).count(),
        hashes.len(),
        started.elapsed(),
        decoding
    );
    hashes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::online::IssueMatch;
    use crate::meta::IssueRef;

    /// A database that answers after `delay`, with the cover halfway through.
    struct Slow {
        delay: Duration,
    }

    impl Source for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }

        fn issue(
            &mut self,
            _http: &mut Http,
            _series: &str,
            _year: Option<u32>,
            _volume: Option<u32>,
            _number: &str,
            found_cover: &mut dyn FnMut(&str),
        ) -> Option<IssueMatch> {
            thread::sleep(self.delay / 2);
            found_cover("https://covers/db.jpg");
            thread::sleep(self.delay / 2);
            Some(IssueMatch { series: "From Database".into(), cover: Some("https://covers/db.jpg".into()), ..Default::default() })
        }

        fn cover(&mut self, _http: &mut Http, _issue: &IssueRef) -> Option<String> {
            None
        }
    }

    struct Published {
        file: String,
        meta: Arc<ComicMeta>,
        at: Instant,
    }

    /// Runs the worker over `paths` until all lookups are done.
    fn serve_all(paths: &[PathBuf], delay: Duration, cache: Cache) -> Vec<Published> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let publish: Publish = Arc::new(move |path: PathBuf, meta| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            sink.lock().unwrap().push(Published { file, meta, at: Instant::now() });
        });
        let (tx, rx) = mpsc::channel();
        for path in paths {
            let listing = Arc::new(archive::list(path).unwrap());
            tx.send(Job { path: path.clone(), listing, refresh: false }).unwrap();
        }
        drop(tx);
        serve(rx, vec![Box::new(Slow { delay })], Arc::new(Mutex::new(cache)), publish);
        Arc::try_unwrap(out).ok().unwrap().into_inner().unwrap()
    }

    #[test]
    fn cover_is_published_before_the_details() {
        let path = archive::test_comic("Early Cover 007.cbz", 3);
        let started = Instant::now();
        let events = serve_all(&[path], Duration::from_millis(600), Cache::default());

        let covers: Vec<_> = events.iter().map(|e| e.meta.cover.as_deref()).collect();
        assert_eq!(covers, [None, Some("https://covers/db.jpg"), Some("https://covers/db.jpg")]);
        let (file, early, done) = (&events[0], &events[1], &events[2]);
        assert!(file.at - started < Duration::from_millis(200), "file metadata took {:?}", file.at - started);
        // The early update keeps the file's title until the details arrive.
        assert_eq!(early.meta.title, file.meta.title);
        assert_eq!(done.meta.title, "From Database");
        assert!(done.at - early.at >= Duration::from_millis(250), "cover was not early");
    }

    #[test]
    fn cached_comic_does_not_wait_for_another_lookup() {
        let slow = archive::test_comic("Not Cached 001.cbz", 3);
        let cached = archive::test_comic("Cached 002.cbz", 3);
        let mut cache = Cache::default();
        let meta = ComicMeta { title: "Cached".into(), cover: Some("https://covers/cached.jpg".into()), online: true, ..Default::default() };
        cache.set_comic(cache::key(&cached).unwrap(), meta);

        let started = Instant::now();
        let events = serve_all(&[slow, cached], Duration::from_secs(1), cache);

        let cached: Vec<_> = events.iter().filter(|e| e.file == "Cached 002.cbz").collect();
        assert_eq!(cached.len(), 1, "a cached comic is not looked up again");
        assert_eq!(cached[0].meta.cover.as_deref(), Some("https://covers/cached.jpg"));
        assert!(cached[0].at - started < Duration::from_millis(300), "cached comic waited {:?}", cached[0].at - started);
        let slow_done = events.iter().filter(|e| e.file == "Not Cached 001.cbz").last().unwrap();
        assert_eq!(slow_done.meta.title, "From Database");
    }
}
