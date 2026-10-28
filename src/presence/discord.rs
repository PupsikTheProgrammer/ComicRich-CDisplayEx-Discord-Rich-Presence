//! Minimal Discord IPC client over the local named pipe.
//! Frame: op (u32 LE), length (u32 LE), JSON payload.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};

use serde_json::{json, Value};

use super::Activity;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
const MAX_FRAME: usize = 64 * 1024;
/// "Watching" shows a screen icon; RPC has no "reading" type and "Playing" shows a gamepad.
const ACTIVITY_WATCHING: u8 = 3;
/// Show `details` instead of the application name in the member list status.
const STATUS_DISPLAY_DETAILS: u8 = 2;
/// Discord rejects activity strings outside 2..=128 characters.
const MAX_TEXT: usize = 128;

pub struct Client {
    pipe: File,
    nonce: u64,
}

impl Client {
    pub fn connect(app_id: &str) -> io::Result<Client> {
        for i in 0..10 {
            let Ok(pipe) = OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}"))
            else {
                continue;
            };
            let mut client = Client { pipe, nonce: 0 };
            client.send(OP_HANDSHAKE, &json!({ "v": 1, "client_id": app_id }))?;
            let (op, body) = client.recv()?;
            if op == OP_FRAME && body["evt"] == "READY" {
                return Ok(client);
            }
            return Err(io::Error::other(format!("handshake rejected: {body}")));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "Discord is not running"))
    }

    /// `None` clears the activity. `icon` is the fallback image URL.
    pub fn set_activity(&mut self, activity: Option<&Activity>, icon: Option<&str>) -> io::Result<()> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": activity.map(|a| activity_json(a, icon)) },
            "nonce": nonce,
        });
        self.send(OP_FRAME, &payload)?;
        loop {
            let (op, body) = self.recv()?;
            match op {
                OP_PING => self.send(OP_PONG, &body)?,
                OP_CLOSE => return Err(io::Error::other(format!("closed by Discord: {body}"))),
                OP_FRAME if body["nonce"] == nonce.as_str() => {
                    if body["evt"] == "ERROR" {
                        // Bad payload, not a broken connection: report and carry on.
                        log!("Discord rejected the activity: {}", body["data"]);
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn send(&mut self, op: u32, payload: &Value) -> io::Result<()> {
        let data = payload.to_string();
        let mut frame = Vec::with_capacity(8 + data.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(data.as_bytes());
        self.pipe.write_all(&frame)
    }

    fn recv(&mut self) -> io::Result<(u32, Value)> {
        let mut header = [0u8; 8];
        self.pipe.read_exact(&mut header)?;
        let op = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::other("oversized frame"));
        }
        let mut body = vec![0u8; len];
        self.pipe.read_exact(&mut body)?;
        Ok((op, serde_json::from_slice(&body)?))
    }
}

fn activity_json(a: &Activity, icon: Option<&str>) -> Value {
    let mut v = json!({
        "type": ACTIVITY_WATCHING,
        "status_display_type": STATUS_DISPLAY_DETAILS,
        "details": clamp(&a.details),
        "timestamps": { "start": a.start },
    });
    if let Some(image) = a.large_image.as_deref().or(icon) {
        v["assets"] = json!({ "large_image": image, "large_text": clamp(&a.large_text) });
        // The small badge shows the collection cover, or the app icon next to a cover.
        let small = a.small_image.as_deref().or(icon.filter(|_| a.large_image.is_some()));
        if let Some(small) = small {
            v["assets"]["small_image"] = small.into();
            v["assets"]["small_text"] = clamp(a.small_text.as_deref().unwrap_or("CDisplayEx")).into();
        }
    }
    if let Some(state) = &a.state {
        v["state"] = clamp(state).into();
    }
    v
}

fn clamp(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_TEXT).collect();
    while out.chars().count() < 2 {
        out.push(' ');
    }
    out
}
