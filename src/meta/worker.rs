//! Background metadata thread: file metadata first, then online lookup, then
//! locating issues inside collections. Runs in Windows background mode (lowest
//! CPU and I/O priority) and publishes each improvement as soon as it is known.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};

use super::cache::{self, Cache};
use super::comicvine::ComicVine;
use super::metron::Metron;
use super::online::Source;
use super::imagehash::{self, PageHashes};
use super::{segments, ComicMeta, Kind};
use crate::archive::{self, Listing};
use crate::config::Config;
use crate::http::{Host, Http};

/// Collections reprinting more issues than this are not cover-matched.
const MAX_COVER_LOOKUPS: usize = 80;
const COVER_HOST: Host = Host { key: "covers", gap: Duration::from_millis(250) };
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

pub fn run(jobs: Receiver<Job>, config: &Config, publish: impl Fn(PathBuf, Arc<ComicMeta>)) {
    background_mode();

    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    if config.online {
        if !config.metron.username.is_empty() && !config.metron.password.is_empty() {
            sources.push(Box::new(Metron::new(&config.metron.username, &config.metron.password)));
        }
        if !config.comicvine.api_key.is_empty() {
            sources.push(Box::new(ComicVine::new(&config.comicvine.api_key)));
        }
    }
    log!("online sources: {:?}", sources.iter().map(|s| s.name()).collect::<Vec<_>>());

    let mut cache = Cache::load();
    let mut http = Http::new();

    while let Ok(mut job) = jobs.recv() {
        // Only the latest comic matters if several were opened in a row.
        while let Ok(next) = jobs.try_recv() {
            job = next;
        }
        let key = cache::key(&job.path);
        let cached = key.as_deref().filter(|_| !job.refresh).and_then(|k| cache.comic(k)).cloned();
        let fresh = cached.is_none();
        let mut meta = cached.unwrap_or_else(|| super::local(&job.path, &job.listing));
        log!("metadata ({}): {meta:?}", meta.source);
        publish(job.path.clone(), Arc::new(meta.clone()));

        let mut changed = fresh;
        if !sources.is_empty() && !meta.online {
            let started = Instant::now();
            enrich(&mut meta, job.listing.pages.len(), &mut sources, &mut http);
            meta.online = true;
            changed = true;
            log!("online lookup done in {:?}: {meta:?}", started.elapsed());
            publish(job.path.clone(), Arc::new(meta.clone()));

            if meta.kind == Kind::Collection && !meta.has_exact_segments() && meta.contents.len() >= 2 {
                let started = Instant::now();
                locate_issues(&mut meta, &job, &mut sources, &mut http, &mut cache);
                log!("issue detection done in {:?}", started.elapsed());
                publish(job.path.clone(), Arc::new(meta.clone()));
            }
        }
        if let Some(key) = key {
            if changed {
                cache.set_comic(key, meta);
            } else {
                cache.touch(&key);
            }
            cache.save();
        }
    }
}

fn enrich(meta: &mut ComicMeta, total: usize, sources: &mut [Box<dyn Source>], http: &mut Http) {
    let number = meta.number.clone().unwrap_or_else(|| "1".into());
    for source in sources.iter_mut() {
        let Some(m) = source.issue(http, &meta.title, meta.year, meta.volume, &number) else { continue };
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
    cache: &mut Cache,
) {
    let total = job.listing.pages.len();
    let mut urls: Vec<Option<String>> = Vec::new();
    let mut hashes: Vec<Option<u64>> = Vec::new();
    for issue in meta.contents.iter().take(MAX_COVER_LOOKUPS) {
        let url = sources.iter_mut().find_map(|s| s.cover(http, issue));
        let hash = url.as_ref().and_then(|u| {
            cache.cover(u).or_else(|| {
                let h = imagehash::dhash(&http.bytes(COVER_HOST, u, MAX_COVER_BYTES)?)?;
                cache.set_cover(u.clone(), h);
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
