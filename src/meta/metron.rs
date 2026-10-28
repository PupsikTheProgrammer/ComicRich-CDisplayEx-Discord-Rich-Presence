//! Metron (metron.cloud): free, open comic database. Needs a free account;
//! allows about 30 requests per minute. Each request takes it a second or two,
//! so the cover is reported as soon as the issue is found, and the series a name
//! stands for is remembered for the following issues.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cache::{self, Shared};
use super::online::{self, pick, query, Candidate, IssueMatch, Source};
use super::IssueRef;
use crate::http::{basic_auth, encode, Host, Http};
use crate::rate::Limit;

const BASE: &str = "https://metron.cloud/api";
/// One lookup is a burst of up to four requests; stay under the per-minute limit.
const HOST: Host = Host { key: "metron", limit: Limit::new(25, Duration::from_secs(60)) };

pub struct Metron {
    auth: String,
    cache: Shared,
}

/// A database series as matched to a name, with its details.
#[derive(Serialize, Deserialize)]
struct Series {
    id: u64,
    name: String,
    year: Option<u32>,
    volume: Option<u32>,
    series_type: Option<String>,
    issue_count: Option<u32>,
}

impl Metron {
    pub fn new(user: &str, password: &str, cache: Shared) -> Self {
        Self { auth: basic_auth(user, password), cache }
    }

    fn get(&self, http: &mut Http, path: &str) -> Option<Value> {
        http.json(HOST, &format!("{BASE}{path}"), Some(&self.auth))
    }

    fn series(&self, http: &mut Http, name: &str, year: Option<u32>, volume: Option<u32>) -> Option<Candidate<u64>> {
        let v = self.get(http, &format!("/series/?name={}", encode(&query(name))))?;
        let candidates = results(&v)
            .iter()
            .filter_map(|r| {
                let (name, display_year) = online::split_year(r["series"].as_str().or(r["name"].as_str())?);
                Some(Candidate {
                    name,
                    year: u32_of(&r["year_began"]).or(display_year),
                    volume: u32_of(&r["volume"]),
                    item: r["id"].as_u64()?,
                })
            })
            .collect();
        pick(candidates, name, year, volume)
    }
}

impl Source for Metron {
    fn name(&self) -> &'static str {
        "metron"
    }

    fn issue(
        &mut self,
        http: &mut Http,
        series: &str,
        year: Option<u32>,
        volume: Option<u32>,
        number: &str,
        found_cover: &mut dyn FnMut(&str),
    ) -> Option<IssueMatch> {
        let key = format!("metron series|{}|{year:?}|{volume:?}", query(series).to_lowercase());
        let remembered = cache::lock(&self.cache)
            .lookup(&key)
            .and_then(|v| serde_json::from_value::<Series>(v.clone()).ok());
        let (mut found, remembered) = match remembered {
            Some(s) => (s, true),
            None => {
                let c = self.series(http, series, year, volume)?;
                let s = Series { id: c.item, name: c.name, year: c.year, volume: c.volume, series_type: None, issue_count: None };
                (s, false)
            }
        };

        let list = self.get(http, &format!("/issue/?series_id={}&number={}", found.id, encode(number)))?;
        let listed = results(&list).first()?;
        if let Some(cover) = text(&listed["image"]) {
            found_cover(&cover);
        }
        let issue = self.get(http, &format!("/issue/{}/", listed["id"].as_u64()?))?;
        if !remembered {
            if let Some(detail) = self.get(http, &format!("/series/{}/", found.id)) {
                found.name = text(&detail["name"]).unwrap_or(found.name);
                found.series_type = detail["series_type"]["name"].as_str().map(String::from);
                found.issue_count = u32_of(&detail["issue_count"]);
                if let Ok(v) = serde_json::to_value(&found) {
                    cache::lock(&self.cache).set_lookup(key, v);
                }
            }
        }

        let series_type = found.series_type.or(issue["series"]["series_type"]["name"].as_str().map(String::from));
        let title = text(&issue["title"]).or_else(|| issue["name"].as_array()?.iter().find_map(text));
        let reprints = issue["reprints"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| online::parse_issue_name(r["issue"].as_str()?)).collect())
            .unwrap_or_default();

        Some(IssueMatch {
            series: found.name,
            year: found.year,
            volume: found.volume,
            series_type,
            issue_count: found.issue_count,
            title,
            cover: text(&issue["image"]).or_else(|| text(&listed["image"])),
            reprints,
        })
    }

    fn cover(&mut self, http: &mut Http, issue: &IssueRef) -> Option<String> {
        let mut path = format!("/issue/?series_name={}&number={}", encode(&query(&issue.series)), encode(&issue.number));
        if let Some(year) = issue.year {
            path.push_str(&format!("&series_year_began={year}"));
        }
        let v = self.get(http, &path)?;
        let candidates = results(&v)
            .iter()
            .filter_map(|r| {
                let s = &r["series"];
                Some(Candidate {
                    name: text(&s["name"])?,
                    year: u32_of(&s["year_began"]),
                    volume: u32_of(&s["volume"]),
                    item: text(&r["image"])?,
                })
            })
            .collect();
        pick(candidates, &issue.series, issue.year, None).map(|c| c.item)
    }
}

fn results(v: &Value) -> &[Value] {
    v["results"].as_array().map_or(&[], Vec::as_slice)
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn u32_of(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}
