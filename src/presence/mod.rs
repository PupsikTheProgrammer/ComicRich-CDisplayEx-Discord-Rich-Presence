//! Owns the Discord connection: connects at startup, reconnects, and respects
//! rate limits.

mod discord;
pub mod format;

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::http::{Host, Http};
use crate::rate::{Limit, Window};

/// Discord allows about 5 activity updates per 20 seconds; short bursts are fine.
const RATE: Limit = Limit::new(5, Duration::from_secs(20));
/// Changes arriving this close together (file name, then cached cover a few
/// milliseconds later) go out as one update.
const DEBOUNCE: Duration = Duration::from_millis(200);
const RECONNECT_DELAY: Duration = Duration::from_secs(15);
const IDLE_WAIT: Duration = Duration::from_secs(3600);
const DISCORD_API: Host = Host { key: "discord", limit: Limit::NONE };

/// Application icon URL, fetched in the background at startup.
static ICON: OnceLock<Option<String>> = OnceLock::new();

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
    let icon_app = app_id.clone();
    std::thread::spawn(move || ICON.get_or_init(|| app_icon(&icon_app)));

    let mut client: Option<discord::Client> = None;
    let mut desired: Option<Activity> = None;
    // What Discord shows now, with the icon it was sent with.
    let mut sent: Option<(Option<Activity>, Option<String>)> = None;
    let mut sends = Window::default();
    let mut changed_at = Instant::now();
    let mut next_connect = Instant::now();

    loop {
        // Connect right away, while CDisplayEx is still loading the comic; the
        // connection then stays open, since a handshake can take Discord seconds.
        if client.is_none() && Instant::now() >= next_connect {
            match discord::Client::connect(&app_id) {
                Ok(c) => {
                    log!("connected to Discord");
                    client = Some(c);
                }
                Err(e) => {
                    log!("Discord unavailable: {e}");
                    next_connect = Instant::now() + RECONNECT_DELAY;
                }
            }
        }

        // An icon arriving after the first update is sent along on its own.
        let icon = ICON.get().cloned().flatten().filter(|_| desired.is_some());
        let target = (desired.clone(), icon.clone());
        let dirty = sent.as_ref() != Some(&target) && !(sent.is_none() && desired.is_none());

        let ready_in = sends.wait(RATE, Instant::now()).max(DEBOUNCE.saturating_sub(changed_at.elapsed()));

        let wait = match &client {
            Some(_) if dirty && ready_in.is_zero() => {
                let c = client.as_mut().unwrap();
                sends.record(Instant::now());
                match c.set_activity(desired.as_ref(), icon.as_deref()) {
                    Ok(()) => {
                        log!("presence updated: {desired:?}");
                        sent = Some(target);
                    }
                    Err(e) => {
                        log!("Discord connection lost: {e}");
                        client = None;
                        sent = None;
                        next_connect = Instant::now() + Duration::from_secs(2);
                    }
                }
                continue;
            }
            Some(_) if dirty => ready_in,
            None => next_connect.saturating_duration_since(Instant::now()),
            // Wake up for the icon if it is still being fetched.
            Some(_) if ICON.get().is_none() => DEBOUNCE,
            Some(_) => IDLE_WAIT,
        };

        match rx.recv_timeout(wait) {
            Ok(msg) => {
                desired = apply(msg);
                while let Ok(msg) = rx.try_recv() {
                    desired = apply(msg);
                }
                changed_at = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
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
