//! Blocking HTTP with per-host pacing, used only from the background metadata thread.

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::Value;

const USER_AGENT: &str = concat!("ComicRich/", env!("CARGO_PKG_VERSION"), " (CDisplayEx Discord presence)");
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRY_WAIT: u64 = 60;

pub struct Http {
    agent: ureq::Agent,
    last: HashMap<&'static str, Instant>,
}

/// A host and the minimum delay between requests to it.
#[derive(Clone, Copy)]
pub struct Host {
    pub key: &'static str,
    pub gap: Duration,
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).user_agent(USER_AGENT).build();
        Self { agent, last: HashMap::new() }
    }

    pub fn json(&mut self, host: Host, url: &str, auth: Option<&str>) -> Option<Value> {
        let response = self.call(host, url, auth)?;
        response.into_json().map_err(|e| log!("bad JSON from {}: {e}", redact(url))).ok()
    }

    pub fn bytes(&mut self, host: Host, url: &str, limit: u64) -> Option<Vec<u8>> {
        let response = self.call(host, url, None)?;
        let mut data = Vec::new();
        response.into_reader().take(limit).read_to_end(&mut data).ok()?;
        Some(data)
    }

    fn call(&mut self, host: Host, url: &str, auth: Option<&str>) -> Option<ureq::Response> {
        for attempt in 0..2 {
            self.pace(host);
            let mut request = self.agent.get(url);
            if let Some(auth) = auth {
                request = request.set("Authorization", auth);
            }
            match request.call() {
                Ok(r) => return Some(r),
                Err(ureq::Error::Status(429, r)) if attempt == 0 => {
                    let wait = r.header("Retry-After").and_then(|s| s.parse().ok()).unwrap_or(30);
                    log!("rate limited by {}, waiting {wait}s", host.key);
                    std::thread::sleep(Duration::from_secs(wait.min(MAX_RETRY_WAIT)));
                }
                Err(e) => {
                    log!("request failed {}: {e}", redact(url));
                    return None;
                }
            }
        }
        None
    }

    fn pace(&mut self, host: Host) {
        if let Some(last) = self.last.get(host.key) {
            if let Some(wait) = host.gap.checked_sub(last.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        self.last.insert(host.key, Instant::now());
    }
}

/// Percent-encodes a query parameter value.
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn basic_auth(user: &str, password: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{password}");
    let mut out = String::from("Basic ");
    for chunk in input.as_bytes().chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Hides API keys in logged URLs.
fn redact(url: &str) -> String {
    match url.find("api_key=") {
        Some(i) => {
            let end = url[i..].find('&').map_or(url.len(), |e| i + e);
            format!("{}api_key=***{}", &url[..i], &url[end..])
        }
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_basic_auth() {
        assert_eq!(basic_auth("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(basic_auth("a", "b"), "Basic YTpi");
    }

    #[test]
    fn encodes_and_redacts() {
        assert_eq!(encode("Doctor Strange/Avengers"), "Doctor%20Strange%2FAvengers");
        assert_eq!(redact("https://x/?api_key=SECRET&format=json"), "https://x/?api_key=***&format=json");
    }
}
