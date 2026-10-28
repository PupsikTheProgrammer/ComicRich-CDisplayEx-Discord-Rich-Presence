use std::path::Path;

use super::{collect_nested, is_archive, normalize, visit_nested, Collector, Result, Sink, Want, MAX_DEPTH};

pub fn collect(path: &Path, prefix: &str, depth: u32, c: &mut Collector) -> Result<()> {
    let mut nested = Vec::new();
    for header in unrar::Archive::new(path).open_for_listing()? {
        let header = header?;
        if !header.is_file() {
            continue;
        }
        let entry = normalize(&header.filename.to_string_lossy());
        if depth < MAX_DEPTH && is_archive(&entry) {
            nested.push(entry);
        } else {
            c.entry(format!("{prefix}{entry}"));
        }
    }
    if nested.is_empty() {
        return Ok(());
    }

    // Only nested archives need decompression; everything else is skipped.
    let mut archive = unrar::Archive::new(path).open_for_processing()?;
    while let Some(header) = archive.read_header()? {
        let entry = normalize(&header.entry().filename.to_string_lossy());
        archive = if nested.contains(&entry) {
            let (data, rest) = header.read()?;
            collect_nested(data, &format!("{prefix}{entry}\\"), depth + 1, c)?;
            rest
        } else {
            header.skip()?
        };
    }
    Ok(())
}

pub fn visit(path: &Path, prefix: &str, depth: u32, want: Want, sink: Sink) -> Result<bool> {
    let mut archive = unrar::Archive::new(path).open_for_processing()?;
    while let Some(header) = archive.read_header()? {
        let entry = normalize(&header.entry().filename.to_string_lossy());
        let path = format!("{prefix}{entry}");
        let nested = depth < MAX_DEPTH && is_archive(&entry);
        if !header.entry().is_file() || (!nested && !want(&path)) {
            archive = header.skip()?;
            continue;
        }
        let (data, rest) = header.read()?;
        let keep_going = if nested {
            visit_nested(data, &format!("{path}\\"), depth + 1, want, sink)?
        } else {
            sink(&path, data)
        };
        if !keep_going {
            return Ok(false);
        }
        archive = rest;
    }
    Ok(true)
}
