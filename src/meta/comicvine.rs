//! Comic Vine (comicvine.gamespot.com): fallback database. Needs a free API key;
//! about 200 requests per hour per resource.

use std::time::Duration;

use serde_json::Value;

use super::online::{pick, query, Candidate, IssueMatch, Source};
use super::IssueRef;
use crate::http::{encode, Host, Http};

const BASE: &str = "https://comicvine.gamespot.com/api";
const HOST: Host = Host { key: "comicvine", gap: Duration::from_millis(1100) };

pub struct ComicVine {
    key: String,
}

struct Volume {
    id: u64,
    count: Option<u32>,
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

    fn volume(&self, http: &mut Http, name: &str, year: Option<u32>) -> Option<Candidate<Volume>> {
        let v = self.get(
            http,
            "/search/",
            &format!("resources=volume&limit=20&field_list=id,name,start_year,count_of_issues&query={}", encode(&query(name))),
        )?;
        let candidates = v["results"]
            .as_array()?
            .iter()
            .filter_map(|r| {
                Some(Candidate {
                    name: r["name"].as_str()?.to_string(),
                    year: r["start_year"].as_str().and_then(|y| y.parse().ok()),
                    volume: None,
                    item: Volume { id: r["id"].as_u64()?, count: r["count_of_issues"].as_u64().map(|c| c as u32) },
                })
            })
            .collect();
        pick(candidates, name, year, None)
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

    fn issue(&mut self, http: &mut Http, series: &str, year: Option<u32>, _volume: Option<u32>, number: &str)
        -> Option<IssueMatch>
    {
        let found = self.volume(http, series, year)?;
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
        let volume = self.volume(http, &issue.series, issue.year)?;
        image(&self.find_issue(http, volume.item.id, &issue.number)?)
    }
}

fn image(issue: &Value) -> Option<String> {
    let img = &issue["image"];
    img["medium_url"].as_str().or(img["original_url"].as_str()).map(String::from)
}
