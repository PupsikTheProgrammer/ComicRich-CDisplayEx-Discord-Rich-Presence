//! Metron (metron.cloud): free, open comic database. Needs a free account;
//! allows about 30 requests per minute.

use std::time::Duration;

use serde_json::Value;

use super::online::{self, pick, query, Candidate, IssueMatch, Source};
use super::IssueRef;
use crate::http::{basic_auth, encode, Host, Http};

const BASE: &str = "https://metron.cloud/api";
const HOST: Host = Host { key: "metron", gap: Duration::from_millis(2100) };

pub struct Metron {
    auth: String,
}

impl Metron {
    pub fn new(user: &str, password: &str) -> Self {
        Self { auth: basic_auth(user, password) }
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

    fn issue(&mut self, http: &mut Http, series: &str, year: Option<u32>, volume: Option<u32>, number: &str)
        -> Option<IssueMatch>
    {
        let found = self.series(http, series, year, volume)?;
        let list = self.get(http, &format!("/issue/?series_id={}&number={}", found.item, encode(number)))?;
        let issue_id = results(&list).first()?["id"].as_u64()?;
        let issue = self.get(http, &format!("/issue/{issue_id}/"))?;
        let detail = self.get(http, &format!("/series/{}/", found.item));

        let series_type = detail
            .as_ref()
            .and_then(|d| d["series_type"]["name"].as_str())
            .or(issue["series"]["series_type"]["name"].as_str())
            .map(String::from);
        let title = text(&issue["title"]).or_else(|| issue["name"].as_array()?.iter().find_map(text));
        let reprints = issue["reprints"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| online::parse_issue_name(r["issue"].as_str()?)).collect())
            .unwrap_or_default();

        Some(IssueMatch {
            series: detail.as_ref().and_then(|d| text(&d["name"])).unwrap_or(found.name),
            year: found.year,
            volume: found.volume,
            series_type,
            issue_count: detail.as_ref().and_then(|d| u32_of(&d["issue_count"])),
            title,
            cover: text(&issue["image"]),
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
