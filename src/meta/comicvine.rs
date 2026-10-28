//! Comic Vine (comicvine.gamespot.com): fallback database. Needs a free API key;
//! about 200 requests per hour per resource.

use std::time::Duration;

use serde_json::Value;

use super::online::{pick, query, similarity, Candidate, IssueMatch, Source};
use super::IssueRef;
use crate::http::{encode, Host, Http};
use crate::rate::Limit;

const BASE: &str = "https://comicvine.gamespot.com/api";
/// Comic Vine also blocks clients that send more than about one request a second.
const HOST: Host = Host { key: "comicvine", limit: Limit::every(Duration::from_millis(1100)) };

pub struct ComicVine {
    key: String,
}

/// Shorter runs under a series' name are mostly collections and specials, not volumes.
const MIN_VOLUME_ISSUES: u32 = 3;

struct Volume {
    id: u64,
    count: Option<u32>,
}

/// A volume search result.
struct Found {
    name: String,
    year: Option<u32>,
    publisher: Option<String>,
    volume: Volume,
}

impl ComicVine {
    pub fn new(key: &str) -> Self {
        Self { key: key.to_string() }
    }

    fn get(&self, http: &mut Http, path: &str, params: &str) -> Option<Value> {
        let url = format!("{BASE}{path}?api_key={}&format=json&{params}", encode(&self.key));
        let v = http.json(HOST, &url, None)?;
        (v["status_code"].as_u64() == Some(1)).then_some(v)
    }

    fn volume(&self, http: &mut Http, name: &str, year: Option<u32>, volume: Option<u32>, number: &str)
        -> Option<Candidate<Volume>>
    {
        let v = self.get(
            http,
            "/search/",
            &format!(
                "resources=volume&limit=20&field_list=id,name,start_year,count_of_issues,publisher&query={}",
                encode(&query(name))
            ),
        )?;
        let found = v["results"]
            .as_array()?
            .iter()
            .filter_map(|r| {
                Some(Found {
                    name: r["name"].as_str()?.to_string(),
                    year: r["start_year"].as_str().and_then(|y| y.parse().ok()),
                    publisher: r["publisher"]["name"].as_str().map(String::from),
                    volume: Volume { id: r["id"].as_u64()?, count: r["count_of_issues"].as_u64().map(|c| c as u32) },
                })
            })
            .collect();
        pick(candidates(found, name, number), name, year, volume)
    }

    fn find_issue(&self, http: &mut Http, volume: u64, number: &str) -> Option<Value> {
        let v = self.get(
            http,
            "/issues/",
            &format!("field_list=id,name,issue_number,image&filter=volume:{volume},issue_number:{}", encode(number)),
        )?;
        v["results"].as_array()?.first().cloned()
    }
}

impl Source for ComicVine {
    fn name(&self) -> &'static str {
        "comicvine"
    }

    // The issue request is the last one; its cover comes with the rest.
    fn issue(
        &mut self,
        http: &mut Http,
        series: &str,
        year: Option<u32>,
        volume: Option<u32>,
        number: &str,
        _found_cover: &mut dyn FnMut(&str),
    ) -> Option<IssueMatch> {
        let found = self.volume(http, series, year, volume, number)?;
        let issue = self.find_issue(http, found.item.id, number)?;
        Some(IssueMatch {
            series: found.name,
            year: found.year,
            issue_count: found.item.count,
            title: issue["name"].as_str().filter(|s| !s.trim().is_empty()).map(String::from),
            cover: image(&issue),
            ..Default::default()
        })
    }

    fn cover(&mut self, http: &mut Http, issue: &IssueRef) -> Option<String> {
        let volume = self.volume(http, &issue.series, issue.year, None, &issue.number)?;
        image(&self.find_issue(http, volume.item.id, &issue.number)?)
    }
}

fn image(issue: &Value) -> Option<String> {
    let img = &issue["image"];
    img["medium_url"].as_str().or(img["original_url"].as_str()).map(String::from)
}

/// Search results as candidates. Comic Vine has no volume numbers, so the
/// volumes of a series are numbered by start year: those with exactly its name,
/// from its main publisher (not foreign editions), long enough to be a run.
/// Volumes too short to hold issue `number` are left out.
fn candidates(found: Vec<Found>, name: &str, number: &str) -> Vec<Candidate<Volume>> {
    let exact = |f: &Found| similarity(&f.name, name) == 1.0;
    let mut publishers: Vec<&str> = found.iter().filter(|f| exact(f)).filter_map(|f| f.publisher.as_deref()).collect();
    publishers.sort();
    let main = publishers
        .chunk_by(|a, b| a == b)
        .max_by_key(|run| run.len())
        .map(|run| run[0].to_string());
    let mut runs: Vec<(Option<u32>, u64)> = found
        .iter()
        .filter(|f| exact(f) && f.publisher == main && f.volume.count.unwrap_or(0) >= MIN_VOLUME_ISSUES)
        .map(|f| (f.year, f.volume.id))
        .collect();
    runs.sort_by_key(|&(year, id)| (year.unwrap_or(u32::MAX), id));

    let wanted: Option<u32> = number.trim_start_matches('0').parse().ok();
    let holds = |f: &Found| match (wanted, f.volume.count) {
        (Some(n), Some(c)) => c >= n,
        _ => true,
    };
    let any_holds = found.iter().any(holds);
    found
        .into_iter()
        .filter(|f| !any_holds || holds(f))
        .map(|f| Candidate {
            volume: runs.iter().position(|&(_, id)| id == f.volume.id).map(|i| i as u32 + 1),
            name: f.name,
            year: f.year,
            item: f.volume,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Comic Vine's answer to "Runaways", as of 2026-10.
    fn runaways() -> Vec<Found> {
        let f = |year, count, id, name: &str, publisher: &str| Found {
            name: name.into(),
            year: Some(year),
            publisher: Some(publisher.into()),
            volume: Volume { id, count: Some(count) },
        };
        vec![
            f(2008, 14, 22775, "Runaways", "Marvel"),
            f(2005, 2, 18055, "Runaways", "Marvel"),
            f(2025, 5, 165025, "Runaways", "Marvel"),
            f(2004, 1, 18060, "Runaways", "Marvel"),
            f(2017, 38, 104320, "Runaways", "Marvel"),
            f(2005, 30, 11872, "Runaways", "Marvel"),
            f(2003, 18, 17988, "Runaways", "Marvel"),
            f(2007, 16, 42887, "Runaways", "Panini España"),
            f(2005, 12, 42886, "Runaways", "Panini España"),
            f(2015, 4, 82705, "Runaways", "Marvel"),
            f(2009, 1, 28015, "Runaways: Pride and Joy", "Marvel"),
            f(2014, 4, 76194, "Runaways: The Complete Collection", "Marvel"),
        ]
    }

    fn chosen(volume: Option<u32>, number: &str) -> u64 {
        pick(candidates(runaways(), "Runaways", number), "Runaways", None, volume).unwrap().item.id
    }

    #[test]
    fn volume_number_picks_the_right_run() {
        assert_eq!(chosen(Some(1), "1"), 17988);
        assert_eq!(chosen(Some(2), "1"), 11872);
        assert_eq!(chosen(Some(3), "01"), 22775);
        assert_eq!(chosen(Some(5), "1"), 104320);
    }

    #[test]
    fn issue_number_rules_out_short_runs() {
        assert_eq!(chosen(None, "1"), 17988);
        // The 2003 run has 18 issues; #22 must be from the 2005 one.
        assert_eq!(chosen(None, "22"), 11872);
        // Volumes keep their numbers when shorter ones are ruled out.
        assert_eq!(chosen(Some(5), "30"), 104320);
        assert_eq!(chosen(Some(2), "25"), 11872);
    }
}
