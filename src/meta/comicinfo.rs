//! ComicRack `ComicInfo.xml` and the "Collects ..." line of its summary.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use super::IssueRef;
use crate::archive;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ComicInfo {
    pub title: String,
    pub series: String,
    pub number: String,
    pub count: String,
    pub volume: String,
    pub format: String,
    pub year: String,
    pub summary: String,
    pub pages: Pages,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Pages {
    #[serde(rename = "Page")]
    pub page: Vec<Page>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Page {
    #[serde(rename = "@Image")]
    pub image: String,
    #[serde(rename = "@Bookmark")]
    pub bookmark: String,
}

pub fn read(path: &Path, entry: &str) -> Option<ComicInfo> {
    let mut data = None;
    archive::visit(path, &|name| name.eq_ignore_ascii_case(entry), &mut |_, bytes| {
        data = Some(bytes);
        false
    })
    .map_err(|e| log!("cannot read ComicInfo.xml: {e}"))
    .ok()?;
    parse(&String::from_utf8_lossy(&data?))
}

pub fn parse(xml: &str) -> Option<ComicInfo> {
    quick_xml::de::from_str(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| log!("invalid ComicInfo.xml: {e}"))
        .ok()
}

static COLLECTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bcollect(?:s|ing)\b\s*:?\s*([^\n]+)").unwrap());
static ENTRY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<series>[^#;]*?)\s*(?:\((?P<year>\d{4})\))?\s*(?P<nums>#\s*\d+(?:\s*(?:-|–|,|\band\b|&)\s*#?\s*\d+)*)")
        .unwrap()
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*[-–]\s*#?\s*(\d+)|(\d+)").unwrap());
static LEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:\s|,|&|\band\b|\bplus\b|\bmaterial\s+from\b|\bstories\s+from\b|\bfrom\b)+").unwrap());

const MAX_ISSUES: usize = 200;

/// `Collects Marvel Spotlight (1971) #3-4; Werewolf By Night (1972) #1, #3 And #15; ...`
pub fn parse_collects(summary: &str) -> Vec<IssueRef> {
    let Some(text) = COLLECTS.captures(summary).map(|c| c[1].to_string()) else { return Vec::new() };
    let mut refs = Vec::new();
    for chunk in text.split(';') {
        for c in ENTRY.captures_iter(chunk) {
            let series = LEADING.replace(&c["series"], "");
            let series = series.trim().trim_matches(|ch: char| ",.:".contains(ch)).trim();
            if series.is_empty() {
                continue;
            }
            let year = c.name("year").and_then(|y| y.as_str().parse().ok());
            for n in NUMBER.captures_iter(&c["nums"]) {
                let (first, last) = match (n.get(1), n.get(2), n.get(3)) {
                    (Some(a), Some(b), _) => (a.as_str().parse().unwrap_or(0u32), b.as_str().parse().unwrap_or(0u32)),
                    (_, _, Some(x)) => {
                        let x = x.as_str().parse().unwrap_or(0);
                        (x, x)
                    }
                    _ => continue,
                };
                for number in first..=last.min(first + MAX_ISSUES as u32) {
                    refs.push(IssueRef { series: series.to_string(), year, number: number.to_string() });
                }
            }
        }
    }
    refs.truncate(MAX_ISSUES);
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_darkhold_summary() {
        let s = "Collects Marvel Spotlight (1971) #3-4; Werewolf By Night (1972) #1, #3 And #15; Tomb Of Dracula (1972) #18-19; Marvel Chillers #1-2; Avengers (1963) #185-187; Doctor Strange (1974) #59-62, #67 and #81; Thor (1966) #332-333 and material From Dracula Lives #6; Amazing Spider-Man Annual #22; X-Men Annual (1970) #12 and Doctor Strange, Sorcerer Supreme #9-13 and #15.\n\nLearn the hidden history...";
        let refs = parse_collects(s);
        let labels: Vec<String> = refs.iter().map(|r| r.label()).collect();
        assert_eq!(refs.len(), 29);
        assert_eq!(labels[0], "Marvel Spotlight #3");
        assert_eq!(labels[3], "Werewolf By Night #3");
        assert_eq!(refs[3].year, Some(1972));
        assert_eq!(labels[20], "Dracula Lives #6");
        assert_eq!(labels[22], "X-Men Annual #12");
        assert_eq!(labels[23], "Doctor Strange, Sorcerer Supreme #9");
        assert_eq!(labels[28], "Doctor Strange, Sorcerer Supreme #15");
    }

    #[test]
    fn parses_comic_info() {
        let xml = r#"<?xml version="1.0"?><ComicInfo xmlns:xsi="x"><Series>Ghost Rider</Series><Number /><Year>2023</Year>
            <Pages><Page Image="0" Type="FrontCover" /><Page Image="25" Bookmark="Ghost Rider #2" /></Pages></ComicInfo>"#;
        let info = parse(xml).unwrap();
        assert_eq!(info.series, "Ghost Rider");
        assert_eq!(info.number, "");
        assert_eq!(info.pages.page[1].bookmark, "Ghost Rider #2");
    }
}
