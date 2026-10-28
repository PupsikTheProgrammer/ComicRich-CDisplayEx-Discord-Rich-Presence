//! Owns the Discord connection: connects lazily, reconnects, and respects rate limits.

mod discord;
pub mod format;

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::http::{Host, Http};

/// Discord allows about 5 activity updates per 20 seconds.
const MIN_GAP: Duration = Duration::from_secs(4);
const RECONNECT_DELAY: Duration = Duration::from_secs(15);
const IDLE_WAIT: Duration = Duration::from_secs(3600);
const DISCORD_API: Host = Host { key: "discord", gap: Duration::ZERO };

#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub details: String,
    pub state: Option<String>,
    /// Cover URL; the application icon is used when absent.
    pub large_image: Option<String>,
    pub large_text: String,
    pub small_image: Option<String>,
    pub small_text: Option<String>,
    /// Unix seconds when reading started.
    pub start: u64,
}

pub enum PresenceMsg {
    Set(Activity),
    Clear,
}

pub fn run(app_id: String, rx: Receiver<PresenceMsg>) {
    let mut client: Option<discord::Client> = None;
    let mut icon: Option<Option<String>> = None;
    let mut desired: Option<Activity> = None;
    let mut sent: Option<Activity> = None;
    let mut last_send: Option<Instant> = None;
    let mut next_connect = Instant::now();

    loop {
        let now = Instant::now();
        let dirty = desired != sent;
        let wait = if !dirty {
            IDLE_WAIT
        } else if client.is_none() && now < next_connect {
            next_connect - now
        } else {
            last_send.map_or(Duration::ZERO, |t| MIN_GAP.saturating_sub(t.elapsed()))
        };

        match rx.recv_timeout(wait) {
            Ok(msg) => {
                desired = apply(msg);
                // Coalesce bursts of page turns into one update.
                while let Ok(msg) = rx.try_recv() {
                    desired = apply(msg);
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if desired == sent || last_send.is_some_and(|t| t.elapsed() < MIN_GAP) {
            continue;
        }

        // The connection stays open between comics: a fresh handshake can take
        // Discord several seconds to answer.
        if client.is_none() {
            if desired.is_none() {
                sent = None;
                continue;
            }
            if Instant::now() < next_connect {
                continue;
            }
            match discord::Client::connect(&app_id) {
                Ok(c) => {
                    log!("connected to Discord");
                    client = Some(c);
                }
                Err(e) => {
                    log!("Discord unavailable: {e}");
                    next_connect = Instant::now() + RECONNECT_DELAY;
                    continue;
                }
            }
        }
        let icon = icon.get_or_insert_with(|| app_icon(&app_id));

        let Some(c) = client.as_mut() else { continue };
        last_send = Some(Instant::now());
        match c.set_activity(desired.as_ref(), icon.as_deref()) {
            Ok(()) => {
                log!("presence updated: {desired:?}");
                sent = desired.clone();
            }
            Err(e) => {
                log!("Discord connection lost: {e}");
                client = None;
                sent = None;
                next_connect = Instant::now() + Duration::from_secs(2);
            }
        }
    }
}

fn apply(msg: PresenceMsg) -> Option<Activity> {
    match msg {
        PresenceMsg::Set(a) => Some(a),
        PresenceMsg::Clear => None,
    }
}

/// Discord does not fall back to the app icon by itself, so its CDN URL is
/// looked up from the public application info.
fn app_icon(app_id: &str) -> Option<String> {
    let info = Http::new().json(DISCORD_API, &format!("https://discord.com/api/v10/applications/{app_id}/rpc"), None)?;
    let hash = info["icon"].as_str()?;
    let url = format!("https://cdn.discordapp.com/app-icons/{app_id}/{hash}.png?size=512");
    log!("application icon: {url}");
    Some(url)
}
