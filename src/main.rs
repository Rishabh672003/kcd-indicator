//! System tray indicator for kcd (headless KDE Connect daemon), modelled on kdeconnect-indicator.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static DEBUG: AtomicBool = AtomicBool::new(false);
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Timestamped stderr log line, always printed.
macro_rules! log {
    ($($a:tt)*) => { eprintln!("[{:>9.3}] {}", crate::START.elapsed().as_secs_f64(), format_args!($($a)*)) };
}

/// Like `log!`, but only with `--debug` / `KCD_INDICATOR_DEBUG=1`.
macro_rules! debug {
    ($($a:tt)*) => { if crate::DEBUG.load(std::sync::atomic::Ordering::Relaxed) { log!($($a)*) } };
}

mod icons;
mod ipc;
mod window;

use ipc::Device;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

type Action = Box<dyn Fn(&mut Indicator) + Send>;

/// Events that change what the menu shows; each triggers a full `devices` refresh.
const WATCHED: &[&str] = &[
    "device.added",
    "device.removed",
    "device.connected",
    "device.disconnected",
    "pair.requested",
    "pair.accepted",
    "pair.rejected",
    "battery.update",
    "mpris.update",
    "sftp.mounted",
    "sftp.unmounted",
    "notification",
];

#[derive(Default)]
struct Indicator {
    /// Why the device list can't be shown (daemon unreachable, bad reply, …); `None` when healthy.
    problem: Option<String>,
    devices: Vec<Device>,
    /// Verification keys of incoming pair requests, by device id.
    pair_keys: HashMap<String, String>,
}

fn notify(title: &str, body: &str) {
    debug!("notify: {title}: {body}");
    if let Err(e) = Command::new("notify-send").args(["-a", "kcd-indicator", "-i", "phone", title, body]).spawn() {
        log!("notify-send failed: {e}");
    }
}

/// Run blocking work off the tray loop; report failures as a notification.
fn background(f: impl FnOnce() -> Result<(), String> + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(e) = f() {
            log!("action failed: {e}");
            notify("kcd", &e);
        }
    });
}

fn device_cmd(cmd: &'static str, id: &str, extra: Value) -> Action {
    let id = id.to_string();
    Box::new(move |_| {
        let mut payload = json!({ "deviceId": id });
        if let (Value::Object(p), Value::Object(e)) = (&mut payload, extra.clone()) {
            p.extend(e);
        }
        background(move || ipc::call(cmd, payload).map(drop));
    })
}

// ksni treats `_` as a mnemonic marker; double it to show it literally.
fn item(label: &str, icon: &str, activate: Action) -> MenuItem<Indicator> {
    StandardItem { label: label.replace('_', "__"), icon_name: icon.into(), activate, ..Default::default() }.into()
}

fn info(label: &str) -> MenuItem<Indicator> {
    StandardItem { label: label.replace('_', "__"), enabled: false, ..Default::default() }.into()
}

fn pair(id: String, name: String) -> Action {
    Box::new(move |_| {
        let (id, name) = (id.clone(), name.clone());
        background(move || {
            let r = ipc::call("pair", json!({ "deviceId": id }))?;
            let key = r["verificationKey"].as_str().unwrap_or("");
            notify(&format!("Pairing with {name}"), &format!("Accept on the phone. Verification key: {key}"));
            Ok(())
        })
    })
}

fn send_files(id: String) -> Action {
    Box::new(move |_| {
        let id = id.clone();
        background(move || {
            let out = Command::new("zenity")
                .args(["--file-selection", "--multiple", "--separator=\n", "--title=Send files to device"])
                .output()
                .map_err(|e| format!("zenity not available: {e}"))?;
            for f in String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()) {
                ipc::call("share", json!({ "deviceId": id, "filePath": f }))?;
            }
            Ok(())
        })
    })
}

fn device_menu(d: &Device, pair_key: Option<&String>) -> MenuItem<Indicator> {
    let id = d.id.as_str();
    let none = Value::Null;
    let mut items = Vec::new();
    let mut status = String::new();

    match d.state.as_str() {
        "PAIRED" if d.connected => {
            if let Some(b) = &d.battery {
                status = format!(" ({}%{})", b.charge, if b.charging { ", charging" } else { "" });
                items.push(info(&format!("Battery: {}%{}", b.charge, if b.charging { " (charging)" } else { "" })));
                items.push(MenuItem::Separator);
            }
            // The daemon opens the file manager itself after mounting ([sftp] auto_open).
            items.push(item("Browse device", "folder-remote", device_cmd("sftp_mount_local", id, none.clone())));
            if d.sftp.as_ref().is_some_and(|s| s.mounted) {
                items.push(item("Unmount", "media-eject", device_cmd("sftp_unmount", id, none.clone())));
            }
            items.push(item("Send clipboard", "edit-paste", device_cmd("clipboard_push", id, none.clone())));
            items.push(item("Send file…", "document-send", send_files(d.id.clone())));
            items.push(item("Ring device", "audio-volume-high", device_cmd("findmyphone", id, none.clone())));
            items.push(item("Ping", "network-transmit", device_cmd("ping", id, none.clone())));
            if let Some(m) = d.media.as_ref().filter(|m| !m.title.is_empty()) {
                let now = if m.artist.is_empty() { m.title.clone() } else { format!("{} — {}", m.title, m.artist) };
                // kcd forwards `action` verbatim; the phone only accepts KDE Connect's PascalCase names.
                let media = vec![
                    info(&now),
                    item(
                        if m.is_playing { "Pause" } else { "Play" },
                        if m.is_playing { "media-playback-pause" } else { "media-playback-start" },
                        device_cmd("mpris_action", id, json!({ "action": "PlayPause" })),
                    ),
                    item("Previous", "media-skip-backward", device_cmd("mpris_action", id, json!({ "action": "Previous" }))),
                    item("Next", "media-skip-forward", device_cmd("mpris_action", id, json!({ "action": "Next" }))),
                ];
                items.push(SubMenu { label: "Media".into(), icon_name: "multimedia-player".into(), submenu: media, ..Default::default() }.into());
            }
            items.push(MenuItem::Separator);
            items.push(item("Unpair", "list-remove", device_cmd("unpair", id, none)));
        }
        "PAIRED" => {
            status = " (offline)".into();
            items.push(info("Not connected"));
            items.push(item("Unpair", "list-remove", device_cmd("unpair", id, none)));
        }
        "PAIR_REQUESTED_BY_PEER" => {
            status = " (pair request)".into();
            items.push(info(&format!("Verification key: {}", pair_key.map_or("?", |k| k.as_str()))));
            // kcd's `pair` ignores the documented accept/reject flags: it accepts a peer's request, and
            // rejecting goes through `unpair`.
            items.push(item("Accept pairing", "dialog-ok", device_cmd("pair", id, none.clone())));
            items.push(item("Reject", "dialog-cancel", device_cmd("unpair", id, none)));
        }
        "PAIR_REQUESTED" => {
            status = " (pairing…)".into();
            items.push(info("Waiting for the phone to accept"));
            items.push(item("Cancel", "dialog-cancel", device_cmd("unpair", id, none)));
        }
        _ => {
            status = " (unpaired)".into();
            items.push(item("Pair", "list-add", pair(d.id.clone(), d.name.clone())));
        }
    }

    let icon = match d.kind.as_str() {
        "tablet" => "tablet",
        "laptop" => "computer-laptop",
        "desktop" => "computer",
        _ => "phone",
    };
    SubMenu { label: format!("{}{status}", d.name).replace('_', "__"), icon_name: icon.into(), submenu: items, ..Default::default() }
        .into()
}

impl ksni::Tray for Indicator {
    fn id(&self) -> String {
        "kcd-indicator".into()
    }

    fn title(&self) -> String {
        "KDE Connect (kcd)".into()
    }

    /// Left click opens the device window in its own process, keeping GTK out of the tray.
    fn activate(&mut self, _x: i32, _y: i32) {
        background(|| {
            let exe = self_exe().map_err(|e| e.to_string())?;
            let mut cmd = Command::new(exe);
            cmd.arg("--window");
            if DEBUG.load(Ordering::Relaxed) {
                cmd.arg("--debug");
            }
            // Wait so the exited window is reaped instead of lingering as a zombie.
            cmd.spawn().and_then(|mut child| child.wait()).map(drop).map_err(|e| format!("cannot open window: {e}"))
        });
    }

    fn icon_name(&self) -> String {
        icons::name(&self.devices)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if let Some(p) = &self.problem {
            p.clone()
        } else {
            let lines: Vec<String> = self
                .devices
                .iter()
                .filter(|d| d.state == "PAIRED")
                // 🗲 (U+1F5F2) renders as a monochrome text glyph; ⚡ (U+26A1) becomes a colour emoji.
                .map(|d| match (&d.battery, d.connected) {
                    (_, false) => format!("{}: offline", d.name),
                    (Some(b), true) => format!("{}: {}%{}", d.name, b.charge, if b.charging { " 🗲" } else { "" }),
                    (None, true) => format!("{}: connected", d.name),
                })
                .collect();
            if lines.is_empty() { "No paired devices".into() } else { lines.join("\n") }
        };
        ksni::ToolTip { title: "KDE Connect (kcd)".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = Vec::new();
        if let Some(p) = &self.problem {
            menu.push(info(p));
            menu.push(item(
                "Start daemon",
                "system-run",
                Box::new(|_| {
                    background(|| {
                        Command::new("systemctl").args(["--user", "start", "kcd.service"]).status().map(drop).map_err(|e| e.to_string())
                    })
                }),
            ));
        } else {
            if self.devices.is_empty() {
                menu.push(info("No devices found"));
            }
            menu.extend(self.devices.iter().map(|d| device_menu(d, self.pair_keys.get(&d.id))));
        }
        menu.push(MenuItem::Separator);
        menu.push(item("Quit", "application-exit", Box::new(|_| std::process::exit(0))));
        menu
    }
}

/// Our own binary for spawning the window. After an upgrade replaces the file, Linux reports the running
/// image as "<path> (deleted)"; the new binary sits at the original path, so launch that.
fn self_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(original) => original.into(),
        None => exe,
    })
}

fn refresh(tray: &ksni::blocking::Handle<Indicator>) {
    let devices = ipc::devices();
    tray.update(|t| match devices {
        Ok(d) => {
            debug!(
                "state: {}",
                d.iter().map(|d| format!("{} [{}{}]", d.name, d.state, if d.connected { ", connected" } else { "" })).collect::<Vec<_>>().join("; ")
            );
            t.problem = None;
            t.devices = d;
        }
        Err(e) => {
            log!("devices refresh failed: {e}");
            t.problem = Some(format!("kcd error: {e}"));
        }
    });
}

fn on_event(tray: &ksni::blocking::Handle<Indicator>, ev: ipc::Event) {
    match ev.kind.as_str() {
        "pair.requested" => {
            let key = ev.payload["verificationKey"].as_str().unwrap_or("").to_string();
            let name = ev.payload["name"].as_str().unwrap_or("A device");
            notify(&format!("{name} wants to pair"), &format!("Verification key: {key}\nAccept from the tray menu."));
            tray.update(|t| t.pair_keys.insert(ev.device_id.clone(), key));
        }
        // Recorded for the window's notification list; kcd itself keeps no history.
        "notification" => {
            let n = window::notifications::Notification::from_event(&ev.device_id, &ev.payload);
            if let Err(e) = window::notifications::record(n) {
                log!("cannot record notification: {e}");
            }
            return;
        }
        // The phone drops its reply handles when the connection ends.
        "device.connected" | "device.disconnected" => {
            if let Err(e) = window::notifications::forget_device_replies(&ev.device_id) {
                log!("cannot update notifications: {e}");
            }
        }
        "pair.accepted" | "pair.rejected" => {
            tray.update(|t| t.pair_keys.remove(&ev.device_id));
        }
        // The daemon also pushes some unrequested events (e.g. cached connectivity.update on subscribe).
        k if k != "state.snapshot" && !WATCHED.contains(&k) => return,
        _ => {}
    }
    refresh(tray);
}

const USAGE: &str = "usage: kcd-indicator [--debug] [--window [PAGE]]\n  --debug, -d      log IPC traffic and events to stderr (or set KCD_INDICATOR_DEBUG=1)\n  --window [PAGE]  open the device window (left-clicking the tray icon does this);\n                   PAGE is device, messages or notifications";

fn main() {
    LazyLock::force(&START);
    let env_debug = std::env::var("KCD_INDICATOR_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0");
    let mut open_window = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--debug" | "-d" => DEBUG.store(true, Ordering::Relaxed),
            "--window" => open_window = Some(args.next_if(|a| !a.starts_with('-'))),
            "--help" | "-h" => return println!("{USAGE}"),
            _ => {
                eprintln!("unknown argument: {arg}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    if env_debug {
        DEBUG.store(true, Ordering::Relaxed);
    }
    if let Some(page) = open_window {
        std::process::exit(match window::run(page) {
            gtk::glib::ExitCode::SUCCESS => 0,
            _ => 1,
        });
    }
    debug!("kcd-indicator {} starting, socket {}", env!("CARGO_PKG_VERSION"), ipc::socket_path().display());

    if let Err(e) = icons::install() {
        log!("cannot install tray icons: {e}");
    }

    // Fetch state before registering: some hosts (wayle) subscribe to NewIcon only after their initial
    // read, so a change in the first milliseconds would be missed and the icon would stay stale.
    let mut indicator = Indicator::default();
    match ipc::devices() {
        Ok(d) => indicator.devices = d,
        Err(e) => indicator.problem = Some(e),
    }
    let tray = indicator.spawn().unwrap_or_else(|e| {
        eprintln!("kcd-indicator: cannot register tray icon: {e}");
        std::process::exit(1)
    });

    // Live updates over `watch`; reconnect with exponential backoff (1s → 30s) when the daemon goes away.
    let mut backoff = 1;
    loop {
        debug!("watch: connecting");
        let err = ipc::watch(WATCHED, || {}, |ev| {
            backoff = 1;
            on_event(&tray, ev);
        })
        .unwrap_err();
        log!("watch: {err}; retrying in {backoff}s");
        tray.update(|t| {
            t.problem = Some(err);
            t.devices.clear();
        });
        std::thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(30);
    }
}
