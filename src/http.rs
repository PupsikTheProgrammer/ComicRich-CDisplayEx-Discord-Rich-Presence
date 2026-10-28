//! Blocking HTTP with per-host rate limits, used only from background threads.

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::rate::{Limit, Window};

const USER_AGENT: &str = concat!("ComicRich/", env!("CARGO_PKG_VERSION"), " (CDisplayEx Discord presence)");
const TIMEOUT: Duration = Duration::from_secs(20);
/// A reachable server connects in well under a second; without this an
/// unreachable one holds every lookup for Windows' 21 s connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an unreachable host is skipped, so lookups go straight to the next source.
const DOWN_FOR: Duration = Duration::from_secs(300);
const MAX_RETRY_WAIT: u64 = 60;

pub struct Http {
    agent: ureq::Agent,
    windows: HashMap<&'static str, Window>,
    /// Unreachable hosts and when to try them again.
    down: HashMap<&'static str, Instant>,
}

/// A host and how many requests it accepts.
#[derive(Clone, Copy)]
pub struct Host {
    pub key: &'static str,
    pub limit: Limit,
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(TIMEOUT)
            .timeout_connect(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .build();
        Self { agent, windows: HashMap::new(), down: HashMap::new() }
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
        if self.down.get(host.key).is_some_and(|&until| Instant::now() < until) {
            return None;
        }
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
                Err(ureq::Error::Transport(t))
                    if matches!(t.kind(), ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io) =>
                {
                    log!("{} unreachable ({t}), skipping it for {DOWN_FOR:?}", host.key);
                    self.down.insert(host.key, Instant::now() + DOWN_FOR);
                    return None;
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
        let window = self.windows.entry(host.key).or_default();
        let wait = window.wait(host.limit, Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        window.record(Instant::now());
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

    #[test]
    fn unreachable_host_is_skipped() {
        // Nothing listens on port 1: the connection is refused at once.
        let host = Host { key: "nowhere", limit: Limit::NONE };
        let mut http = Http::new();
        assert!(http.json(host, "http://127.0.0.1:1/", None).is_none());
        assert!(http.down.contains_key("nowhere"));

        let started = Instant::now();
        assert!(http.json(host, "http://127.0.0.1:1/", None).is_none());
        assert!(started.elapsed() < Duration::from_millis(50), "second request was attempted");
        // Other hosts are unaffected.
        assert!(!http.down.contains_key("other"));
    }

    #[test]
    fn bursts_are_not_spaced_out() {
        let host = Host { key: "burst", limit: Limit::new(3, Duration::from_secs(60)) };
        let mut http = Http::new();
        let started = Instant::now();
        for _ in 0..3 {
            http.pace(host);
        }
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
