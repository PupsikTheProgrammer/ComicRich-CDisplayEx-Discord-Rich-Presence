//! Metron (metron.cloud): free, open comic database. Needs a free account; allows
//! 20 requests per minute and 5000 per day per user, and reports what is left in
//! every response (followed by `Http`). A request takes it a few hundred
//! milliseconds with a token and a second or two with a password, so the cover
//! is reported as soon as the issue is found, and the series a name stands for
//! is remembered for the following issues.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cache::{self, Shared};
use super::online::{self, pick, query, Candidate, IssueMatch, Source};
use super::IssueRef;
use crate::http::{encode, Host, Http};
use crate::rate::Limit;

const BASE: &str = "https://metron.cloud/api";
/// One lookup is a burst of up to four requests; stay under the per-minute limit,
/// with room for other Metron clients of the same account.
const HOST: Host = Host { key: "metron", limit: Limit::new(18, Duration::from_secs(60)) };

pub struct Metron {
    base: String,
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

/// Remembers refused credentials across runs: Metron bans an address that keeps
/// sending bad logins, so they are not tried again until they change.
const REFUSED_KEY: &str = "metron refused credentials";

impl Metron {
    /// `auth` is the `Authorization` header value: a bearer token or basic auth.
    pub fn new(auth: String, cache: Shared) -> Self {
        Self::at(BASE, auth, cache)
    }

    fn at(base: &str, auth: String, cache: Shared) -> Self {
        let metron = Self { base: base.to_string(), auth, cache };
        if metron.refused() {
            log!("metron refused these credentials before; skipped until they change in comicrich.toml");
        }
        metron
    }

    /// A fingerprint of the credentials, so the password itself is not stored.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.auth.hash(&mut h);
        h.finish()
    }

    fn refused(&self) -> bool {
        cache::lock(&self.cache).lookup(REFUSED_KEY).and_then(Value::as_u64) == Some(self.fingerprint())
    }

    fn get(&self, http: &mut Http, path: &str) -> Option<Value> {
        if self.refused() {
            return None;
        }
        let v = http.json(HOST, &format!("{}{path}", self.base), Some(&self.auth));
        if http.rejected(HOST) {
            let mut cache = cache::lock(&self.cache);
            cache.set_lookup(REFUSED_KEY.into(), Value::from(self.fingerprint()));
            cache.save();
        }
        v
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::http::fake;
    use crate::meta::cache::Cache;

    const COVER: &str = "https://static.metron.cloud/media/issue/2024/06/09/runaways-v3-9.jpg";

    /// Metron's answers for "Runaways" vol. 3, in the shape of its API.
    fn metron() -> fake::Server {
        let series_list = r#"{"count":3,"results":[
            {"id":101,"series":"Runaways (2003)","year_began":2003,"volume":1,"issue_count":18},
            {"id":102,"series":"Runaways (2005)","year_began":2005,"volume":2,"issue_count":30},
            {"id":103,"series":"Runaways (2008)","year_began":2008,"volume":3,"issue_count":14}]}"#;
        let issue_list = format!(r#"{{"count":1,"results":[{{"id":9009,"number":"9","image":"{COVER}"}}]}}"#);
        let issue = format!(
            r#"{{"id":9009,"number":"9","name":["Rock Zombies: Conclusion"],"title":"","image":"{COVER}",
                "series":{{"id":103,"name":"Runaways","series_type":{{"name":"Ongoing Series"}}}},"reprints":[]}}"#
        );
        let series = r#"{"id":103,"name":"Runaways","series_type":{"name":"Limited Series"},"issue_count":14}"#;
        fake::serve(vec![
            ("/api/series/?", 200, series_list.into()),
            ("/api/series/103/", 200, series.into()),
            ("/api/issue/?series_id=103", 200, issue_list),
            ("/api/issue/9009/", 200, issue),
        ])
    }

    fn lookup(m: &mut Metron, http: &mut Http, server: &fake::Server) -> (Option<IssueMatch>, Option<usize>) {
        let mut cover_after = None;
        let found = m.issue(http, "Runaways", Some(2009), Some(3), "9", &mut |c| {
            assert_eq!(c, COVER);
            cover_after = Some(server.requests().len());
        });
        (found, cover_after)
    }

    #[test]
    fn cover_comes_early_and_series_is_remembered() {
        let server = metron();
        let cache = Arc::new(Mutex::new(Cache::default()));
        let mut m = Metron::at(&format!("{}/api", server.url), "Bearer secret-token".into(), cache.clone());

        let (found, cover_after) = lookup(&mut m, &mut Http::new(), &server);
        let found = found.expect("issue found");
        assert_eq!(cover_after, Some(2), "cover reported right after the issue list");
        assert_eq!((found.series.as_str(), found.year, found.volume), ("Runaways", Some(2008), Some(3)));
        assert_eq!(found.title.as_deref(), Some("Rock Zombies: Conclusion"));
        assert_eq!(found.cover.as_deref(), Some(COVER));
        assert_eq!(found.series_type.as_deref(), Some("Limited Series"));
        assert_eq!(found.issue_count, Some(14));
        assert_eq!(server.requests().len(), 4);
        assert!(server.authorizations().iter().all(|a| a.as_deref() == Some("Bearer secret-token")), "token not sent");

        // The next lookup in the series, in a later run: no series search or details.
        let mut m = Metron::at(&format!("{}/api", server.url), "Bearer secret-token".into(), cache);
        let (found, cover_after) = lookup(&mut m, &mut Http::new(), &server);
        assert_eq!(found.unwrap().issue_count, Some(14));
        assert_eq!(cover_after, Some(5), "cover after a single request");
        let later = &server.requests()[4..];
        assert!(later.iter().all(|p| p.starts_with("/api/issue/")), "series looked up again: {later:?}");
    }

    #[test]
    fn refused_credentials_are_remembered_across_runs() {
        let server = fake::serve(vec![("/", 401, String::new())]);
        let base = format!("{}/api", server.url);
        let cache = Arc::new(Mutex::new(Cache::default()));

        let mut m = Metron::at(&base, "Bearer wrong".into(), cache.clone());
        assert!(lookup(&mut m, &mut Http::new(), &server).0.is_none());
        assert_eq!(server.requests().len(), 1);

        // A later run with the same credentials sends nothing.
        let mut m = Metron::at(&base, "Bearer wrong".into(), cache.clone());
        assert!(lookup(&mut m, &mut Http::new(), &server).0.is_none());
        assert_eq!(server.requests().len(), 1);

        // Corrected credentials are tried.
        let mut m = Metron::at(&base, "Bearer right".into(), cache);
        lookup(&mut m, &mut Http::new(), &server);
        assert_eq!(server.requests().len(), 2);
    }
}
