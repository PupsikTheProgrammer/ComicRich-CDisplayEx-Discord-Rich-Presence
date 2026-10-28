//! Common shape of the comic databases and fuzzy matching of their results.

use std::sync::LazyLock;

use regex::Regex;

use super::IssueRef;
use crate::http::Http;

/// Minimum name similarity to accept a database series.
pub const MIN_SIMILARITY: f32 = 0.6;

#[derive(Debug, Default, Clone)]
pub struct IssueMatch {
    pub series: String,
    pub year: Option<u32>,
    pub volume: Option<u32>,
    pub series_type: Option<String>,
    pub issue_count: Option<u32>,
    pub title: Option<String>,
    pub cover: Option<String>,
    /// Issues a collected edition reprints.
    pub reprints: Vec<IssueRef>,
}

pub trait Source {
    fn name(&self) -> &'static str;
    fn issue(&mut self, http: &mut Http, series: &str, year: Option<u32>, volume: Option<u32>, number: &str)
        -> Option<IssueMatch>;
    fn cover(&mut self, http: &mut Http, issue: &IssueRef) -> Option<String>;
}

/// A database series candidate.
pub struct Candidate<T> {
    pub name: String,
    pub year: Option<u32>,
    pub volume: Option<u32>,
    pub item: T,
}

/// Best candidate by name, then year and volume.
pub fn pick<T>(candidates: Vec<Candidate<T>>, name: &str, year: Option<u32>, volume: Option<u32>) -> Option<Candidate<T>> {
    let mut scored: Vec<(f32, Candidate<T>)> = candidates
        .into_iter()
        .filter_map(|c| {
            let sim = similarity(&c.name, name);
            if sim < MIN_SIMILARITY {
                return None;
            }
            let mut score = sim;
            if let (Some(want), Some(got)) = (year, c.year) {
                score += match want.abs_diff(got) {
                    0 => 0.3,
                    1 => 0.15,
                    d if d > 3 => -0.3,
                    _ => 0.0,
                };
            }
            if let (Some(want), Some(got)) = (volume, c.volume) {
                score += if want == got { 0.25 } else { -0.1 };
            }
            Some((score, c))
        })
        .collect();
    // Without a year, prefer the earliest run: "Runaways #1" means the 2003 series.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.year.cmp(&b.1.year)));
    scored.into_iter().next().map(|(_, c)| c)
}

/// Token overlap of two titles, 0..=1, ignoring punctuation and articles.
pub fn similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    if ta == tb {
        return 1.0;
    }
    let common = ta.iter().filter(|t| tb.contains(t)).count();
    common as f32 / ta.len().max(tb.len()) as f32
}

fn tokens(s: &str) -> Vec<String> {
    let s = s.to_lowercase().replace('&', " and ");
    let mut t: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && *w != "the" && *w != "a" && *w != "of")
        .map(String::from)
        .collect();
    t.sort();
    t.dedup();
    t
}

/// Words for a database name search, punctuation removed.
pub fn query(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

static DISPLAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<series>.+?)\s*(?:\((?P<year>\d{4})\))?\s*#\s*(?P<number>\S+)\s*$").unwrap());

/// Parses `Marvel Spotlight (1971) #3`.
pub fn parse_issue_name(s: &str) -> Option<IssueRef> {
    let c = DISPLAY.captures(s.trim())?;
    Some(IssueRef {
        series: c["series"].to_string(),
        year: c.name("year").and_then(|y| y.as_str().parse().ok()),
        number: c["number"].to_string(),
    })
}

/// Strips a trailing `(2003)` from a display name.
pub fn split_year(s: &str) -> (String, Option<u32>) {
    let s = s.trim();
    if let Some(open) = s.rfind(" (") {
        let inner = s[open + 2..].trim_end_matches(')');
        if let Ok(y) = inner.parse::<u32>() {
            return (s[..open].to_string(), Some(y));
        }
    }
    (s.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_titles() {
        assert_eq!(similarity("Avengers/Doctor Strange: Rise of the Darkhold", "Avengers-Doctor Strange - Rise of the Darkhold"), 1.0);
        assert!(similarity("Runaways", "Runaways") > similarity("Runaways", "New Runaways"));
        assert!(similarity("Spider-Man", "Batman") < MIN_SIMILARITY);
    }

    #[test]
    fn picks_by_year_and_volume() {
        let c = |name: &str, year, volume| Candidate { name: name.into(), year: Some(year), volume: Some(volume), item: year };
        let list = || vec![c("Runaways", 2015, 5), c("Runaways", 2003, 1), c("Runaways", 2005, 2)];
        assert_eq!(pick(list(), "Runaways", None, Some(2), ).unwrap().item, 2005);
        assert_eq!(pick(list(), "Runaways", None, None).unwrap().item, 2003);
        assert_eq!(pick(list(), "Runaways", Some(2015), None).unwrap().item, 2015);
    }

    #[test]
    fn parses_names() {
        let r = parse_issue_name("Marvel Spotlight (1971) #3").unwrap();
        assert_eq!((r.series.as_str(), r.year, r.number.as_str()), ("Marvel Spotlight", Some(1971), "3"));
        assert_eq!(split_year("Runaways (2003)"), ("Runaways".to_string(), Some(2003)));
    }
}
