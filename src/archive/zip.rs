use std::io::{Read, Seek};

use super::{collect_nested, is_archive, normalize, visit_nested, Collector, Result, Sink, Want, MAX_DEPTH};

pub fn collect<R: Read + Seek>(reader: R, prefix: &str, depth: u32, c: &mut Collector) -> Result<()> {
    let mut archive = ::zip::ZipArchive::new(reader)?;
    let names: Vec<String> = archive.file_names().map(String::from).collect();
    for name in names {
        let entry = normalize(&name);
        if depth < MAX_DEPTH && is_archive(&entry) {
            let mut data = Vec::new();
            archive.by_name(&name)?.read_to_end(&mut data)?;
            collect_nested(data, &format!("{prefix}{entry}\\"), depth + 1, c)?;
        } else {
            c.entry(format!("{prefix}{entry}"));
        }
    }
    Ok(())
}

pub fn visit<R: Read + Seek>(reader: R, prefix: &str, depth: u32, want: Want, sink: Sink) -> Result<bool> {
    let mut archive = ::zip::ZipArchive::new(reader)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let entry = normalize(file.name());
        let path = format!("{prefix}{entry}");
        let nested = depth < MAX_DEPTH && is_archive(&entry);
        if !nested && !want(&path) {
            continue;
        }
        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data)?;
        drop(file);
        let keep_going = if nested {
            visit_nested(data, &format!("{path}\\"), depth + 1, want, sink)?
        } else {
            sink(&path, data)
        };
        if !keep_going {
            return Ok(false);
        }
    }
    Ok(true)
}
