//! Blocking HTTP with per-host rate limits, used only from background threads.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::rate::{Limit, Window};

const USER_AGENT: &str = concat!("ComicRich/", env!("CARGO_PKG_VERSION"), " (CDisplayEx Discord presence)");
const TIMEOUT: Duration = Duration::from_secs(20);
/// A reachable server connects in well under a second; without this an
/// unreachable one holds every lookup for Windows' 21 s connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an unreachable host is skipped, so lookups go straight to the next source.
const DOWN_FOR: Duration = Duration::from_secs(300);
/// The longest a lookup waits for a rate limit; beyond that the host is skipped.
const MAX_WAIT: Duration = Duration::from_secs(60);

pub struct Http {
    agent: ureq::Agent,
    windows: HashMap<&'static str, Window>,
    /// Unreachable or rate-limited hosts and when to try them again.
    down: HashMap<&'static str, Instant>,
    /// Hosts whose rate limit is spent, and when the next request may go.
    paused: HashMap<&'static str, Instant>,
    /// Hosts that refused the credentials. Repeated bad logins get an address
    /// banned (Metron jails it), so they are not asked again.
    rejected: HashSet<&'static str>,
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
        Self { agent, windows: HashMap::new(), down: HashMap::new(), paused: HashMap::new(), rejected: HashSet::new() }
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

    /// Whether `host` refused the credentials.
    pub fn rejected(&self, host: Host) -> bool {
        self.rejected.contains(host.key)
    }

    fn call(&mut self, host: Host, url: &str, auth: Option<&str>) -> Option<ureq::Response> {
        if self.rejected.contains(host.key) || self.down.get(host.key).is_some_and(|&until| Instant::now() < until) {
            return None;
        }
        for attempt in 0..2 {
            self.pace(host);
            let mut request = self.agent.get(url);
            if let Some(auth) = auth {
                request = request.set("Authorization", auth);
            }
            match request.call() {
                Ok(r) => {
                    self.follow_limits(host, &r);
                    return Some(r);
                }
                Err(ureq::Error::Status(429, r)) => {
                    let wait = Duration::from_secs(r.header("Retry-After").and_then(|s| s.parse().ok()).unwrap_or(30));
                    log!("rate limited by {}, asked to wait {wait:?}", host.key);
                    if attempt > 0 || !self.pause(host, wait) {
                        return None;
                    }
                }
                Err(ureq::Error::Status(code @ (401 | 403), _)) => {
                    log!("{} refused the credentials (HTTP {code}), not asking it again; check comicrich.toml", host.key);
                    self.rejected.insert(host.key);
                    return None;
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

    /// Follows rate limit headers (Metron's `X-RateLimit-{Burst,Sustained}-*`):
    /// once a budget is spent, the next request waits for it to reset.
    fn follow_limits(&mut self, host: Host, r: &ureq::Response) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let reset = ["Burst", "Sustained"]
            .iter()
            .filter_map(|kind| {
                let remaining: u64 = r.header(&format!("X-RateLimit-{kind}-Remaining"))?.parse().ok()?;
                let reset: u64 = r.header(&format!("X-RateLimit-{kind}-Reset"))?.parse().ok()?;
                (remaining == 0).then_some(reset)
            })
            .max();
        if let Some(reset) = reset {
            let wait = Duration::from_secs(reset.saturating_sub(now));
            log!("{} rate limit used up, next request in {wait:?}", host.key);
            self.pause(host, wait);
        }
    }

    /// Holds requests to `host` for `wait`; a long wait skips the host instead,
    /// so lookups go to the next source. Returns whether requests will wait.
    fn pause(&mut self, host: Host, wait: Duration) -> bool {
        if wait <= MAX_WAIT {
            self.paused.insert(host.key, Instant::now() + wait);
            true
        } else {
            log!("{} not asked again for {wait:?}", host.key);
            self.down.insert(host.key, Instant::now() + wait);
            false
        }
    }

    fn pace(&mut self, host: Host) {
        if let Some(until) = self.paused.remove(host.key) {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
        }
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
pub mod fake {
    //! A local HTTP server standing in for a comic database in tests.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct Server {
        /// `http://127.0.0.1:<port>`
        pub url: String,
        /// Path (with query) and `Authorization` header of each request.
        requests: Arc<Mutex<Vec<(String, Option<String>)>>>,
    }

    impl Server {
        /// Paths (with queries) requested so far, in order.
        pub fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
        }

        /// The `Authorization` header of each request, in order.
        pub fn authorizations(&self) -> Vec<Option<String>> {
            self.requests.lock().unwrap().iter().map(|(_, a)| a.clone()).collect()
        }
    }

    /// Answers each request from the first route whose path prefix matches it,
    /// with that status and JSON body; anything else gets 404.
    pub fn serve(routes: Vec<(&'static str, u16, String)>) -> Server {
        serve_with_headers(routes.into_iter().map(|(p, s, b)| (p, s, String::new(), b)).collect())
    }

    /// As `serve`, with extra response header lines (`Name: value\r\n` each).
    pub fn serve_with_headers(routes: Vec<(&'static str, u16, String, String)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let auth = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                    .map(|(_, v)| v.trim().to_string());
                log.lock().unwrap().push((path.clone(), auth));
                let (status, headers, body) = routes
                    .iter()
                    .find(|(prefix, ..)| path.starts_with(prefix))
                    .map_or((404, String::new(), String::new()), |(_, s, h, b)| (*s, h.clone(), b.clone()));
                let reply = format!(
                    "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Server { url, requests }
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
    fn refused_credentials_are_not_sent_again() {
        let server = fake::serve(vec![("/", 401, String::new())]);
        let host = Host { key: "strict", limit: Limit::NONE };
        let mut http = Http::new();
        for _ in 0..3 {
            assert!(http.json(host, &format!("{}/api/", server.url), Some("Basic bad")).is_none());
        }
        assert!(http.rejected(host));
        assert_eq!(server.requests().len(), 1, "credentials were sent again after a 401");
    }

    fn unix_in(secs: u64) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + secs
    }

    #[test]
    fn spent_burst_budget_waits_for_its_reset() {
        let headers = format!("X-RateLimit-Burst-Remaining: 0\r\nX-RateLimit-Burst-Reset: {}\r\n", unix_in(2));
        let server = fake::serve_with_headers(vec![("/", 200, headers, "{}".into())]);
        let host = Host { key: "budget", limit: Limit::NONE };
        let mut http = Http::new();
        let started = Instant::now();
        assert!(http.json(host, &server.url, None).is_some());
        assert!(http.json(host, &server.url, None).is_some());
        assert!(started.elapsed() >= Duration::from_millis(900), "did not wait for the reset");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn spent_daily_budget_skips_the_host() {
        let headers = format!("X-RateLimit-Sustained-Remaining: 0\r\nX-RateLimit-Sustained-Reset: {}\r\n", unix_in(3600));
        let server = fake::serve_with_headers(vec![("/", 200, headers, "{}".into())]);
        let host = Host { key: "daily", limit: Limit::NONE };
        let mut http = Http::new();
        assert!(http.json(host, &server.url, None).is_some());
        let started = Instant::now();
        assert!(http.json(host, &server.url, None).is_none());
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn long_retry_after_skips_the_host() {
        let server = fake::serve_with_headers(vec![("/", 429, "Retry-After: 600\r\n".into(), String::new())]);
        let host = Host { key: "throttled", limit: Limit::NONE };
        let mut http = Http::new();
        let started = Instant::now();
        assert!(http.json(host, &server.url, None).is_none());
        assert!(http.json(host, &server.url, None).is_none());
        assert!(started.elapsed() < Duration::from_secs(1), "waited for a long Retry-After");
        assert_eq!(server.requests().len(), 1);
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
