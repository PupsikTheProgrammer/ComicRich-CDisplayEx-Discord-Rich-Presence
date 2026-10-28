//! Reads comic archives without extracting them to disk.
//!
//! The format is detected by magic bytes, not extension: plenty of `.cbr` files are
//! really ZIPs and vice versa. Archives nested inside archives (e.g. `.cbr.cbr`
//! wrappers) are expanded the way CDisplayEx shows them: `Inner.cbr\page.jpg`.

mod natsort;
mod pdf;
mod rar;
mod zip;

use std::error::Error;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
/// Selects entries (by normalized path) whose contents should be read.
pub type Want<'a> = &'a dyn Fn(&str) -> bool;
/// Receives an entry's path and bytes; returns false to stop visiting.
pub type Sink<'a> = &'a mut dyn FnMut(&str, Vec<u8>) -> bool;

const MAX_DEPTH: u32 = 2;
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "jxl", "avif"];
const ARCHIVE_EXTENSIONS: &[&str] = &["cbz", "zip", "cbr", "rar"];
const COMIC_INFO: &str = "comicinfo.xml";

/// Pages of one comic in reading order.
#[derive(Debug)]
pub struct Listing {
    /// Entry paths with `\` separators, naturally sorted.
    pub pages: Vec<String>,
    lower: Vec<String>,
    /// Path of the shallowest `ComicInfo.xml`, if any.
    pub comic_info: Option<String>,
    /// False for PDFs whose page count could not be determined.
    pub total_known: bool,
}

impl Listing {
    fn new(mut pages: Vec<String>, comic_info: Option<String>, total_known: bool) -> Self {
        pages.sort_by(|a, b| natsort::compare(a, b));
        let lower = pages.iter().map(|p| p.to_lowercase()).collect();
        Self { pages, lower, comic_info, total_known }
    }

    /// Index of the page CDisplayEx shows for the given window title.
    pub fn find(&self, title: &str) -> Option<usize> {
        let title = normalize(title).to_lowercase();
        if let Some(i) = self.lower.iter().position(|p| *p == title) {
            return Some(i);
        }
        // Fall back to the bare file name when it is unique within the archive.
        let name = file_name(&title);
        let mut matches = self.lower.iter().enumerate().filter(|(_, p)| file_name(p) == name);
        match (matches.next(), matches.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    }

    /// Lowercased page paths, aligned with `pages`.
    pub fn lower(&self) -> &[String] {
        &self.lower
    }
}

#[derive(Default)]
struct Collector {
    pages: Vec<String>,
    comic_info: Option<String>,
}

impl Collector {
    fn entry(&mut self, path: String) {
        if is_image(&path) {
            self.pages.push(path);
        } else if file_name(&path).eq_ignore_ascii_case(COMIC_INFO) {
            let depth = |p: &str| p.matches('\\').count();
            if self.comic_info.as_deref().map_or(true, |c| depth(&path) < depth(c)) {
                self.comic_info = Some(path);
            }
        }
    }
}

pub fn list(path: &Path) -> Result<Listing> {
    let mut c = Collector::default();
    match sniff_file(path)? {
        Kind::Zip => zip::collect(File::open(path)?, "", 0, &mut c)?,
        Kind::Rar => rar::collect(path, "", 0, &mut c)?,
        Kind::Pdf => {
            let count = pdf::page_count(path)?;
            // CDisplayEx renders PDF pages to `pdf<index>.png`, zero-based.
            let pages = (0..count.unwrap_or(pdf::UNKNOWN_PAGES)).map(|i| format!("pdf{i}.png")).collect();
            return Ok(Listing::new(pages, None, count.is_some()));
        }
        Kind::Unknown => return Err("unsupported archive format".into()),
    }
    Ok(Listing::new(c.pages, c.comic_info, true))
}

/// Streams the bytes of selected entries, descending into nested archives.
pub fn visit(path: &Path, want: Want, sink: Sink) -> Result<()> {
    match sniff_file(path)? {
        Kind::Zip => zip::visit(File::open(path)?, "", 0, want, sink)?,
        Kind::Rar => rar::visit(path, "", 0, want, sink)?,
        Kind::Pdf | Kind::Unknown => true,
    };
    Ok(())
}

/// Lists an archive that was itself read out of another archive.
fn collect_nested(data: Vec<u8>, prefix: &str, depth: u32, c: &mut Collector) -> Result<()> {
    match sniff(&data) {
        Kind::Zip => zip::collect(Cursor::new(data), prefix, depth, c),
        Kind::Rar => with_temp_file(data, |tmp| rar::collect(tmp, prefix, depth, c)),
        Kind::Pdf | Kind::Unknown => Ok(()),
    }
}

fn visit_nested(data: Vec<u8>, prefix: &str, depth: u32, want: Want, sink: Sink) -> Result<bool> {
    match sniff(&data) {
        Kind::Zip => zip::visit(Cursor::new(data), prefix, depth, want, sink),
        Kind::Rar => with_temp_file(data, |tmp| rar::visit(tmp, prefix, depth, want, sink)),
        Kind::Pdf | Kind::Unknown => Ok(true),
    }
}

/// unrar only reads from files, so nested RARs go through a short-lived temp file.
fn with_temp_file<T>(data: Vec<u8>, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "comicrich-{}-{}.rar",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, &data)?;
    drop(data);
    let result = f(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

enum Kind {
    Zip,
    Rar,
    Pdf,
    Unknown,
}

fn sniff_file(path: &Path) -> Result<Kind> {
    let mut magic = [0u8; 8];
    let n = File::open(path)?.read(&mut magic)?;
    Ok(sniff(&magic[..n]))
}

fn sniff(magic: &[u8]) -> Kind {
    if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
        Kind::Zip
    } else if magic.starts_with(b"Rar!\x1a\x07") {
        Kind::Rar
    } else if magic.starts_with(b"%PDF") {
        Kind::Pdf
    } else {
        Kind::Unknown
    }
}

fn normalize(name: &str) -> String {
    name.replace('/', "\\").trim_start_matches('\\').to_string()
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn has_extension(name: &str, extensions: &[&str]) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| extensions.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn is_image(name: &str) -> bool {
    has_extension(name, IMAGE_EXTENSIONS) && !name.to_lowercase().starts_with("__macosx\\")
}

fn is_archive(name: &str) -> bool {
    has_extension(name, ARCHIVE_EXTENSIONS)
}

/// Writes a CBZ with `pages` blank pages to a fresh temporary folder, for tests
/// that need a real comic file.
#[cfg(test)]
pub fn test_comic(name: &str, pages: usize) -> std::path::PathBuf {
    use std::io::Write;
    use ::zip::write::{SimpleFileOptions, ZipWriter};

    let dir = std::env::temp_dir().join(format!("comicrich-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    let options = SimpleFileOptions::default().compression_method(::zip::CompressionMethod::Stored);
    for i in 1..=pages {
        zip.start_file(format!("{i:03}.jpg"), options).unwrap();
        zip.write_all(b"not really a jpeg").unwrap();
    }
    zip.finish().unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_a_real_cbz() {
        let path = test_comic("Listing Test 001.cbz", 12);
        let l = list(&path).unwrap();
        assert_eq!(l.pages.len(), 12);
        assert_eq!(l.find("010.jpg"), Some(9));
    }

    fn listing(names: &[&str]) -> Listing {
        Listing::new(names.iter().map(|s| s.to_string()).collect(), None, true)
    }

    #[test]
    fn finds_exact_and_by_file_name() {
        let l = listing(&["Inner.cbr\\p 002.jpg", "Inner.cbr\\p 001.jpg", "Inner.cbr\\p 010.jpg"]);
        assert_eq!(l.find("Inner.cbr\\p 001.jpg"), Some(0));
        assert_eq!(l.find("inner.cbr/P 010.JPG"), Some(2));
        assert_eq!(l.find("p 002.jpg"), Some(1));
        assert_eq!(l.find("missing.jpg"), None);
    }

    #[test]
    fn ambiguous_file_name_is_not_matched() {
        let l = listing(&["a\\001.jpg", "b\\001.jpg"]);
        assert_eq!(l.find("001.jpg"), None);
        assert_eq!(l.find("b\\001.jpg"), Some(1));
    }

    #[test]
    fn classifies_entries() {
        assert!(is_image("x\\Page 01.JPG"));
        assert!(!is_image("ComicInfo.xml"));
        assert!(!is_image("__MACOSX\\._01.jpg"));
        assert!(is_archive("Inner (Digital).cbr"));
    }

    #[test]
    fn prefers_shallow_comic_info() {
        let mut c = Collector::default();
        c.entry("Inner.cbr\\ComicInfo.xml".into());
        c.entry("ComicInfo.xml".into());
        assert_eq!(c.comic_info.as_deref(), Some("ComicInfo.xml"));
    }
}
