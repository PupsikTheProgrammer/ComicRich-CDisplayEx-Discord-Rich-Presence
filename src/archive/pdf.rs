//! PDF page count without a PDF library: the page tree root carries the largest
//! `/Count`; failing that, page objects are counted. Streams the file in chunks to
//! keep memory flat on large scans.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use regex::bytes::Regex;

use super::Result;

/// Placeholder size when the count is hidden in compressed object streams.
pub const UNKNOWN_PAGES: usize = 5000;
const CHUNK: usize = 1 << 20;
const OVERLAP: usize = 32;

static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/Count\s*(\d{1,5})").unwrap());
static PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/Type\s*/Page[^s]").unwrap());

pub fn page_count(path: &Path) -> Result<Option<usize>> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK + OVERLAP];
    let mut carry = 0;
    let (mut max_count, mut pages) = (0usize, 0usize);
    loop {
        let n = file.read(&mut buf[carry..])?;
        let filled = carry + n;
        let at_end = n == 0;
        // Matches starting in the tail are counted with the next chunk instead.
        let limit = if at_end { filled } else { filled.saturating_sub(OVERLAP) };
        let window = &buf[..filled];
        for c in COUNT.captures_iter(window).filter(|c| c.get(0).unwrap().start() < limit) {
            if let Some(v) = std::str::from_utf8(&c[1]).ok().and_then(|s| s.parse().ok()) {
                max_count = max_count.max(v);
            }
        }
        pages += PAGE.find_iter(window).filter(|m| m.start() < limit).count();
        if at_end {
            break;
        }
        carry = filled - limit;
        buf.copy_within(limit..filled, 0);
    }
    let count = if max_count > 0 { max_count } else { pages };
    Ok((count > 0).then_some(count))
}
