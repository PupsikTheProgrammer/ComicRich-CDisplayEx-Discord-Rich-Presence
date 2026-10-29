//! PDF page count without a PDF library. The cross-reference data at the end of
//! the file leads to the page tree root and its `/Count` in a few small reads,
//! cross-reference and object streams (PDF 1.5) included. A file that cannot be
//! followed that way is scanned whole: the largest `/Count`, or failing that the
//! number of page objects, streamed in chunks to keep memory flat. A full scan of
//! a 900 MB scan-based PDF takes seconds; following the structure, milliseconds.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

use flate2::read::ZlibDecoder;
use regex::bytes::Regex;

use super::Result;

/// Placeholder size when the count is hidden in compressed object streams.
pub const UNKNOWN_PAGES: usize = 5000;
const CHUNK: usize = 1 << 20;
const OVERLAP: usize = 32;
/// `startxref` is within the last kilobyte or so.
const TAIL: usize = 2048;
/// Objects and cross-reference sections are read in pieces of this size, growing as needed.
const PIECE: usize = 64 * 1024;
const MAX_READ: usize = 32 << 20;
/// Revisions followed through `/Prev`.
const MAX_SECTIONS: usize = 64;
const MAX_PAGES: u64 = 100_000;

static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/Count\s*(\d{1,5})").unwrap());
static PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/Type\s*/Page[^s]").unwrap());
static STARTXREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"startxref\s+(\d+)").unwrap());
static OBJ_HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)\s+(\d+)\s+obj").unwrap());

pub fn page_count(path: &Path) -> Result<Option<usize>> {
    if let Some(count) = Structure::open(path).and_then(|mut s| s.page_count()) {
        return Ok(Some(count));
    }
    scan(path)
}

/// Where an object is stored.
#[derive(Debug, Clone, Copy)]
enum Loc {
    At(u64),
    /// Inside the object stream with this object number.
    InStream(u32),
}

struct Structure {
    file: File,
    size: u64,
    xref: HashMap<u32, Loc>,
}

impl Structure {
    fn open(path: &Path) -> Option<Structure> {
        let file = File::open(path).ok()?;
        let size = file.metadata().ok()?.len();
        Some(Structure { file, size, xref: HashMap::new() })
    }

    fn page_count(&mut self) -> Option<usize> {
        let tail = self.read(self.size.saturating_sub(TAIL as u64), TAIL)?;
        let start = STARTXREF.captures_iter(&tail).last().and_then(|c| number(&c[1]))?;

        // Newest revision first; an object keeps the location it had there.
        let mut root = None;
        let mut queue = vec![start];
        let mut seen = HashSet::new();
        while let Some(offset) = queue.pop() {
            if !seen.insert(offset) || seen.len() > MAX_SECTIONS {
                continue;
            }
            let trailer = self.section(offset)?;
            root = root.or(reference(&trailer, "/Root"));
            queue.extend(int(&trailer, "/Prev"));
            // A hybrid file's stream section completes the table it belongs to.
            queue.extend(int(&trailer, "/XRefStm"));
        }

        let catalog = self.object(root?)?.dict;
        let pages = self.object(reference(&catalog, "/Pages")?)?.dict;
        let count = match int(&pages, "/Count") {
            Some(n) => n,
            None => leading_int(&self.object(reference(&pages, "/Count")?)?.dict)?,
        };
        (count > 0 && count < MAX_PAGES).then_some(count as usize)
    }

    /// Reads the cross-reference section at `offset` into `xref`; returns its trailer.
    fn section(&mut self, offset: u64) -> Option<Vec<u8>> {
        let head = self.read(offset, 16)?;
        if head.trim_ascii_start().starts_with(b"xref") {
            self.table(offset)
        } else {
            self.xref_stream(offset)
        }
    }

    /// A classic `xref` table: subsections of 20-byte entries, then `trailer`.
    fn table(&mut self, offset: u64) -> Option<Vec<u8>> {
        let mut len = PIECE;
        let buf = loop {
            let buf = self.read(offset, len)?;
            if find(&buf, b"trailer").is_some() || buf.len() < len {
                break buf;
            }
            len = (len * 4).min(MAX_READ);
            if len == MAX_READ {
                return None;
            }
        };
        let trailer_at = find(&buf, b"trailer")?;
        let mut tokens = buf[..trailer_at].split(u8::is_ascii_whitespace).filter(|t| !t.is_empty());
        if tokens.next()? != b"xref" {
            return None;
        }
        while let Some(first) = tokens.next() {
            let (first, count) = (number(first)?, number(tokens.next()?)?);
            for i in 0..count {
                let (pos, _gen, kind) = (tokens.next()?, tokens.next()?, tokens.next()?);
                if kind == b"n" {
                    self.xref.entry((first + i) as u32).or_insert(Loc::At(number(pos)?));
                }
            }
        }
        let trailer = &buf[trailer_at..];
        Some(trailer[..find(trailer, b"startxref").unwrap_or(trailer.len())].to_vec())
    }

    /// A cross-reference stream: binary entries of `/W` widths; its dictionary is the trailer.
    fn xref_stream(&mut self, offset: u64) -> Option<Vec<u8>> {
        let obj = self.object_at(offset)?;
        let data = decode(&obj.dict, obj.stream.as_deref()?)?;
        let w = array(&obj.dict, "/W")?;
        if w.len() != 3 || w.iter().any(|&n| n > 8) {
            return None;
        }
        let row = (w[0] + w[1] + w[2]) as usize;
        let index = array(&obj.dict, "/Index").unwrap_or_else(|| vec![0, int(&obj.dict, "/Size").unwrap_or(0)]);
        let mut rows = data.chunks_exact(row.max(1));
        for pair in index.chunks_exact(2) {
            for i in 0..pair[1] {
                let entry = rows.next()?;
                let (a, rest) = entry.split_at(w[0] as usize);
                let b = &rest[..w[1] as usize];
                // A missing type field means type 1.
                let kind = if a.is_empty() { 1 } else { be(a) };
                let loc = match kind {
                    1 => Loc::At(be(b)),
                    2 => Loc::InStream(be(b) as u32),
                    _ => continue,
                };
                self.xref.entry((pair[0] + i) as u32).or_insert(loc);
            }
        }
        Some(obj.dict)
    }

    fn object(&mut self, num: u32) -> Option<Object> {
        match *self.xref.get(&num)? {
            Loc::At(offset) => self.object_at(offset),
            Loc::InStream(stream) => {
                let Loc::At(offset) = *self.xref.get(&stream)? else { return None };
                let obj = self.object_at(offset)?;
                let data = decode(&obj.dict, obj.stream.as_deref()?)?;
                let first = int(&obj.dict, "/First")? as usize;
                let header: Vec<u64> = data.get(..first)?.split(u8::is_ascii_whitespace).filter_map(number).collect();
                let pairs: Vec<&[u64]> = header.chunks_exact(2).collect();
                let i = pairs.iter().position(|p| p[0] == num as u64)?;
                let start = first + pairs[i][1] as usize;
                let end = pairs.get(i + 1).map_or(data.len(), |p| first + p[1] as usize);
                Some(Object { dict: data.get(start..end)?.to_vec(), stream: None })
            }
        }
    }

    /// The object whose `N G obj` header is at `offset`.
    fn object_at(&mut self, offset: u64) -> Option<Object> {
        let mut len = PIECE;
        loop {
            let buf = self.read(offset, len)?;
            if let Some(obj) = parse_object(&buf) {
                return Some(obj);
            }
            if buf.len() < len || len >= MAX_READ {
                return None;
            }
            len = (len * 4).min(MAX_READ);
        }
    }

    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if offset >= self.size {
            return None;
        }
        let len = len.min((self.size - offset) as usize);
        let mut buf = vec![0u8; len];
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

struct Object {
    /// Everything before the stream (the dictionary), or the whole object.
    dict: Vec<u8>,
    /// Raw stream bytes, if the object has a stream.
    stream: Option<Vec<u8>>,
}

/// `None` while `buf` does not yet hold the whole object.
fn parse_object(buf: &[u8]) -> Option<Object> {
    let start = OBJ_HEADER.find(buf)?.end();
    let body = &buf[start..];
    let endobj = find(body, b"endobj");
    match find(body, b"stream") {
        Some(s) if endobj.is_none_or(|e| s < e) => {
            let dict = &body[..s];
            let mut data_start = s + b"stream".len();
            if body.get(data_start) == Some(&b'\r') {
                data_start += 1;
            }
            if body.get(data_start) == Some(&b'\n') {
                data_start += 1;
            }
            let data_end = match int(dict, "/Length") {
                Some(len) => data_start + len as usize,
                None => data_start + find(&body[data_start..], b"endstream")?,
            };
            let data = body.get(data_start..data_end)?;
            Some(Object { dict: dict.to_vec(), stream: Some(data.to_vec()) })
        }
        _ => Some(Object { dict: body[..endobj?].to_vec(), stream: None }),
    }
}

/// Stream contents: FlateDecode and PNG predictors, as cross-reference and
/// object streams use; other filters are not needed for these.
fn decode(dict: &[u8], raw: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    if find(dict, b"/Filter").is_some() {
        if find(dict, b"/FlateDecode").is_none() {
            return None;
        }
        ZlibDecoder::new(raw).read_to_end(&mut data).ok()?;
    } else {
        data = raw.to_vec();
    }
    match int(dict, "/Predictor") {
        Some(p) if p >= 10 => unpredict(&data, int(dict, "/Columns").unwrap_or(1) as usize),
        Some(p) if p > 1 => None,
        _ => Some(data),
    }
}

/// Reverses PNG row filters (one byte per pixel, as in cross-reference streams).
fn unpredict(data: &[u8], columns: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; columns];
    for row in data.chunks_exact(columns + 1) {
        let (filter, bytes) = (row[0], &row[1..]);
        let mut cur = vec![0u8; columns];
        for i in 0..columns {
            let left = if i > 0 { cur[i - 1] } else { 0 };
            let up = prev[i];
            let up_left = if i > 0 { prev[i - 1] } else { 0 };
            let predicted = match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((left as u16 + up as u16) / 2) as u8,
                4 => paeth(left, up, up_left),
                _ => return None,
            };
            cur[i] = bytes[i].wrapping_add(predicted);
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    Some(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// A direct integer value; `None` when missing or an indirect reference.
fn int(dict: &[u8], key: &str) -> Option<u64> {
    let re = Regex::new(&format!(r"{key}\s+(\d+)(\s+\d+\s+R)?")).ok()?;
    let c = re.captures(dict)?;
    if c.get(2).is_some() {
        return None;
    }
    number(&c[1])
}

/// The object number of an indirect reference `key N G R`.
fn reference(dict: &[u8], key: &str) -> Option<u32> {
    let re = Regex::new(&format!(r"{key}\s+(\d+)\s+\d+\s+R")).ok()?;
    number(&re.captures(dict)?[1]).map(|n| n as u32)
}

fn array(dict: &[u8], key: &str) -> Option<Vec<u64>> {
    let re = Regex::new(&format!(r"{key}\s*\[([\d\s]*)\]")).ok()?;
    let c = re.captures(dict)?;
    Some(c[1].split(u8::is_ascii_whitespace).filter_map(number).collect())
}

fn leading_int(body: &[u8]) -> Option<u64> {
    body.split(u8::is_ascii_whitespace).find(|t| !t.is_empty()).and_then(number)
}

fn number(s: &[u8]) -> Option<u64> {
    std::str::from_utf8(s).ok()?.trim().parse().ok()
}

/// Big-endian unsigned integer.
fn be(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |n, &b| n << 8 | b as u64)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whole-file scan, for files whose structure could not be followed.
fn scan(path: &Path) -> Result<Option<usize>> {
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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("comicrich-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Appends `N 0 obj ... endobj` and returns its offset.
    fn obj(pdf: &mut Vec<u8>, num: u32, body: &[u8]) -> usize {
        let at = pdf.len();
        pdf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
        at
    }

    /// Appends a cross-reference table with its trailer; returns its offset.
    fn table(pdf: &mut Vec<u8>, entries: &[(u32, usize)], trailer: &str) -> usize {
        let at = pdf.len();
        pdf.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        for &(num, offset) in entries {
            pdf.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(format!("trailer\n<< {trailer} >>\nstartxref\n{at}\n%%EOF\n").as_bytes());
        at
    }

    #[test]
    fn follows_the_table_to_the_newest_revision() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let catalog = obj(&mut pdf, 1, b"<< /Type /Catalog /Pages 2 0 R >>");
        let pages = obj(&mut pdf, 2, b"<< /Type /Pages /Kids [] /Count 12 >>");
        let first_xref = table(&mut pdf, &[(1, catalog), (2, pages)], "/Size 3 /Root 1 0 R");
        // An incremental update drops pages: the old /Count is larger but stale.
        let pages = obj(&mut pdf, 2, b"<< /Type /Pages /Kids [] /Count 9 >>");
        table(&mut pdf, &[(2, pages)], &format!("/Size 3 /Root 1 0 R /Prev {first_xref}"));

        let path = write("revisions.pdf", &pdf);
        assert_eq!(Structure::open(&path).unwrap().page_count(), Some(9));
        assert_eq!(scan(&path).unwrap(), Some(12), "a scan alone would be wrong here");
        assert_eq!(page_count(&path).unwrap(), Some(9));
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn follows_compressed_cross_reference_and_object_streams() {
        let mut pdf = b"%PDF-1.5\n".to_vec();
        // Objects 1 and 2 live inside object stream 3.
        let o1 = b"<< /Type /Catalog /Pages 2 0 R >>";
        let o2 = b"<< /Type /Pages /Kids [] /Count 243 >>";
        let header = format!("1 0 2 {} ", o1.len() + 1);
        let mut objects = header.clone().into_bytes();
        objects.extend_from_slice(o1);
        objects.push(b' ');
        objects.extend_from_slice(o2);
        let packed = zlib(&objects);
        let mut body = format!("<< /Type /ObjStm /N 2 /First {} /Filter /FlateDecode /Length {} >>\nstream\n", header.len(), packed.len()).into_bytes();
        body.extend_from_slice(&packed);
        body.extend_from_slice(b"\nendstream");
        let objstm = obj(&mut pdf, 3, &body);

        // Cross-reference stream 4: /W [1 4 2], PNG "Up" rows.
        let xref_at = pdf.len();
        let entries: [(u8, u32, u16); 5] = [(0, 0, 65535), (2, 3, 0), (2, 3, 1), (1, objstm as u32, 0), (1, xref_at as u32, 0)];
        let mut rows = Vec::new();
        let mut prev = [0u8; 7];
        for (t, f2, f3) in entries {
            let mut cur = [0u8; 7];
            cur[0] = t;
            cur[1..5].copy_from_slice(&f2.to_be_bytes());
            cur[5..7].copy_from_slice(&f3.to_be_bytes());
            rows.push(2);
            rows.extend(cur.iter().zip(prev).map(|(c, p)| c.wrapping_sub(p)));
            prev = cur;
        }
        let packed = zlib(&rows);
        let mut body = format!(
            "<< /Type /XRef /Size 5 /Root 1 0 R /W [1 4 2] /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 7 >> /Length {} >>\nstream\n",
            packed.len()
        )
        .into_bytes();
        body.extend_from_slice(&packed);
        body.extend_from_slice(b"\nendstream");
        obj(&mut pdf, 4, &body);
        pdf.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());

        let path = write("streams.pdf", &pdf);
        assert_eq!(Structure::open(&path).unwrap().page_count(), Some(243));
        // Everything is compressed: a scan finds nothing to count.
        assert_eq!(scan(&path).unwrap(), None);
    }

    #[test]
    fn broken_structure_falls_back_to_a_scan() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Pages /Count 31 >>\nendobj\nstartxref\n99999\n%%EOF\n";
        let path = write("broken.pdf", pdf);
        assert_eq!(Structure::open(&path).unwrap().page_count(), None);
        assert_eq!(page_count(&path).unwrap(), Some(31));
    }
}
