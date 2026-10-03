//! Client for the kcd daemon's NDJSON Unix socket (docs/IPC_PROTOCOL.md upstream).
//! One request per connection, as the protocol recommends.

use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: String,
    pub connected: bool,
    pub battery: Option<Battery>,
    pub media: Option<Media>,
    pub sftp: Option<Sftp>,
    pub signal: Option<Signal>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Battery {
    pub charge: i64,
    pub charging: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// `file://` path once kcd has cached the art; empty while pending or absent.
    pub album_art_url: String,
    pub is_playing: bool,
    /// Track length and position in ms; `pos` was sampled at `pos_anchor_ms` (Unix ms).
    pub length: i64,
    pub pos: i64,
    pub pos_anchor_ms: i64,
    pub volume: i64,
    pub can_seek: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Signal {
    /// Per SIM subscription id.
    pub signal_strengths: std::collections::BTreeMap<String, SignalStrength>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SignalStrength {
    pub network_type: String,
    /// 0 (none) – 4 (full).
    pub signal_strength: i64,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Sftp {
    pub mounted: bool,
    pub mount_point: String,
}

#[derive(Deserialize, Debug)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, rename = "deviceId")]
    pub device_id: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Deserialize)]
struct Response {
    ok: bool,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    error: String,
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
        format!("/run/user/{uid}").into()
    });
    runtime.join("kcd/kcd.sock")
}

fn send(cmd: &str, payload: Value) -> Result<BufReader<UnixStream>, String> {
    let mut s = UnixStream::connect(socket_path()).map_err(|e| format!("kcd daemon not reachable: {e}"))?;
    let mut line = serde_json::to_vec(&json!({"cmd": cmd, "payload": payload})).unwrap();
    debug!("-> {}", String::from_utf8_lossy(&line));
    line.push(b'\n');
    s.write_all(&line).map_err(|e| e.to_string())?;
    Ok(BufReader::new(s))
}

fn read_response(r: &mut impl BufRead) -> Result<Value, String> {
    let mut line = String::new();
    if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("kcd daemon closed the connection".into());
    }
    debug!("<- {}", line.trim_end());
    let resp: Response = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if resp.ok { Ok(resp.data) } else { Err(resp.error) }
}

pub fn call(cmd: &str, payload: Value) -> Result<Value, String> {
    read_response(&mut send(cmd, payload)?)
}

pub fn devices() -> Result<Vec<Device>, String> {
    serde_json::from_value(call("devices", Value::Null)?).map_err(|e| e.to_string())
}

/// Blocks, calling `on_event` for each event until the stream breaks. `on_ready` runs once the
/// subscription is live, so requests whose replies arrive as events can't race it.
pub fn watch(events: &[&str], on_ready: impl FnOnce(), mut on_event: impl FnMut(Event)) -> Result<(), String> {
    let mut r = send("watch", json!({ "events": events }))?;
    read_response(&mut r)?; // ack line
    on_ready();
    for line in r.lines() {
        let line = line.map_err(|e| e.to_string())?;
        debug!("event: {line}");
        match serde_json::from_str(&line) {
            Ok(ev) => on_event(ev),
            Err(e) => log!("unparseable event ({e}): {line}"),
        }
    }
    Err("watch stream ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_summary() {
        let r = r#"{"ok":true,"data":[{"id":"x","name":"moto g32","type":"phone","state":"PAIRED","cert_fp":"","last_seen":"2026-10-02T20:02:15+05:30","connected":true,"battery":{"charge":54,"charging":false,"batteryAgeMs":1},"sftp":{"mounted":true,"mountPoint":"/m"},"media":{"title":"Song","artist":"A","isPlaying":true,"pos":1}}]}"#;
        let data = read_response(&mut r.as_bytes()).unwrap();
        let d: Vec<Device> = serde_json::from_value(data).unwrap();
        assert_eq!(d[0].battery.as_ref().unwrap().charge, 54);
        assert!(d[0].media.as_ref().unwrap().is_playing);
        assert_eq!(d[0].sftp.as_ref().unwrap().mount_point, "/m");
        assert_eq!(read_response(&mut &br#"{"ok":false,"error":"device not found"}"#[..]), Err("device not found".into()));
    }
}
