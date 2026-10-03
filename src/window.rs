//! Device window (`kcd-indicator --window`): status, media, quick actions and file sending for a paired
//! device. Runs as its own process so the tray stays small; GApplication uniqueness means a second launch
//! just raises the open window.

use crate::ipc::{self, Device, Media};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP_ID: &str = "io.github.kcd.Indicator";

const EVENTS: &[&str] = &[
    "device.added",
    "device.removed",
    "device.connected",
    "device.disconnected",
    "pair.accepted",
    "pair.rejected",
    "battery.update",
    "connectivity.update",
    "mpris.update",
    "sftp.mounted",
    "sftp.unmounted",
    "share.progress",
    "share.complete",
];

enum Msg {
    Devices(Result<Vec<Device>, String>),
    Progress { file: String, current: u64, total: u64 },
    Complete { file: String, success: bool, error: String },
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| match app.active_window() {
        Some(w) => w.present(),
        None => build(app),
    });
    // Our own flags (--window, --debug) are not GApplication options.
    app.run_with_args::<&str>(&[])
}

/// Watch kcd from a thread and forward what the window needs; reconnects like the tray does.
fn spawn_watcher(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        let mut backoff = 1;
        loop {
            // Don't wait for the first device event; render as soon as the window is up.
            if tx.send_blocking(Msg::Devices(ipc::devices())).is_err() {
                return;
            }
            let err = ipc::watch(EVENTS, |ev| {
                backoff = 1;
                let p = &ev.payload;
                let file = p["file"].as_str().unwrap_or_default().to_string();
                let msg = match ev.kind.as_str() {
                    "share.progress" => Msg::Progress {
                        file,
                        current: p["current"].as_u64().unwrap_or(0),
                        total: p["total"].as_u64().unwrap_or(0),
                    },
                    "share.complete" => Msg::Complete {
                        file,
                        success: p["success"].as_bool().unwrap_or(true),
                        error: p["error"].as_str().unwrap_or_default().to_string(),
                    },
                    _ => Msg::Devices(ipc::devices()),
                };
                let _ = tx.send_blocking(msg);
            })
            .unwrap_err();
            if tx.send_blocking(Msg::Devices(Err(err))).is_err() {
                return; // window closed
            }
            std::thread::sleep(Duration::from_secs(backoff));
            backoff = (backoff * 2).min(30);
        }
    });
}

#[derive(Default)]
struct State {
    devices: Vec<Device>,
    problem: Option<String>,
    selected: Option<String>,
    /// Ids shown in the device dropdown, to rebuild it only when the list changes.
    listed: Vec<String>,
    /// Ignore position/volume pushes from the phone while the user is dragging a slider.
    seek_hold: Option<Instant>,
    volume_hold: Option<Instant>,
    seek_gen: u32,
    volume_gen: u32,
    transfers: HashMap<String, (adw::ActionRow, gtk::ProgressBar)>,
}

impl State {
    fn device(&self) -> Option<&Device> {
        self.devices.iter().find(|d| Some(&d.id) == self.selected.as_ref())
    }
}

#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    title: adw::WindowTitle,
    picker: gtk::DropDown,
    battery: adw::ActionRow,
    battery_icon: gtk::Image,
    battery_bar: gtk::LevelBar,
    signal: adw::ActionRow,
    signal_icon: gtk::Image,
    files: adw::ActionRow,
    unmount: gtk::Button,
    media: adw::PreferencesGroup,
    art: gtk::Picture,
    art_placeholder: gtk::Image,
    track: gtk::Label,
    artist: gtk::Label,
    prev: gtk::Button,
    play: gtk::Button,
    next: gtk::Button,
    position: gtk::Scale,
    elapsed: gtk::Label,
    duration: gtk::Label,
    volume: gtk::Scale,
    actions: adw::PreferencesGroup,
    transfers: adw::PreferencesGroup,
    state: Rc<RefCell<State>>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Position right now: kcd stamps when `pos` was sampled, so it advances while playing without polling.
fn live_pos(m: &Media) -> i64 {
    let pos = if m.is_playing { m.pos + now_ms() - m.pos_anchor_ms } else { m.pos };
    pos.clamp(0, m.length.max(0))
}

fn fmt_time(ms: i64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn fmt_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / f64::from(1 << 30)),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / f64::from(1 << 20)),
        b => format!("{:.0} KB", b as f64 / 1024.0),
    }
}

fn row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk::Align::Center).build()
}

impl Ui {
    /// Run a kcd command off the main thread; failures become a toast, success optionally too.
    fn call(&self, cmd: &'static str, payload: Value, ok_msg: Option<&'static str>) {
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || ipc::call(cmd, payload)).await.unwrap_or_else(|_| Err("worker panicked".into()));
            match res {
                Err(e) => ui.toast(&e),
                Ok(_) => ok_msg.into_iter().for_each(|m| ui.toast(m)),
            }
        });
    }

    fn device_call(&self, cmd: &'static str, extra: Value, ok_msg: Option<&'static str>) {
        let Some(id) = self.state.borrow().selected.clone() else { return };
        let mut payload = json!({ "deviceId": id });
        if let (Value::Object(p), Value::Object(e)) = (&mut payload, extra) {
            p.extend(e);
        }
        self.call(cmd, payload, ok_msg);
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn send_files(&self, paths: Vec<String>) {
        let n = paths.len();
        for f in paths {
            // kcd reads `filePath`; its IPC docs say `file`.
            self.device_call("share", json!({ "filePath": f }), None);
        }
        if n > 0 {
            self.toast(&format!("Sending {n} file{}…", if n == 1 { "" } else { "s" }));
        }
    }

    fn render(&self) {
        let st = self.state.borrow();
        if let Some(p) = &st.problem {
            self.empty.set_icon_name(Some("network-offline-symbolic"));
            self.empty.set_title("kcd is not reachable");
            self.empty.set_description(Some(p));
            self.stack.set_visible_child_name("empty");
            self.title.set_subtitle("");
            return;
        }
        let Some(d) = st.device() else {
            self.empty.set_icon_name(Some("phone-symbolic"));
            self.empty.set_title("No paired devices");
            self.empty.set_description(Some("Open KDE Connect on your phone and pair it from the tray menu."));
            self.stack.set_visible_child_name("empty");
            self.title.set_subtitle("");
            return;
        };
        self.stack.set_visible_child_name("device");
        self.title.set_title(&d.name);
        self.title.set_subtitle(if d.connected { "Connected" } else { "Offline" });

        match &d.battery {
            Some(b) => {
                let step = (b.charge.clamp(0, 100) + 5) / 10 * 10;
                self.battery.set_subtitle(&format!("{}%{}", b.charge, if b.charging { " · charging" } else { "" }));
                self.battery_icon
                    .set_icon_name(Some(&format!("battery-level-{step}{}-symbolic", if b.charging { "-charging" } else { "" })));
                self.battery_bar.set_value(b.charge as f64);
                self.battery_bar.set_visible(true);
            }
            None => {
                self.battery.set_subtitle("No reading yet");
                self.battery_icon.set_icon_name(Some("battery-missing-symbolic"));
                self.battery_bar.set_visible(false);
            }
        }

        let sim = d.signal.as_ref().and_then(|s| s.signal_strengths.values().next());
        self.signal.set_visible(sim.is_some());
        if let Some(s) = sim {
            let bars = s.signal_strength.clamp(0, 4);
            let kind = if s.network_type.is_empty() || s.network_type == "Unknown" { String::new() } else { format!("{} · ", s.network_type) };
            self.signal.set_subtitle(&format!("{kind}{bars} of 4 bars"));
            let level = ["none", "weak", "ok", "good", "excellent"][bars as usize];
            self.signal_icon.set_icon_name(Some(&format!("network-cellular-signal-{level}-symbolic")));
        }

        let mount = d.sftp.as_ref().filter(|s| s.mounted).map(|s| s.mount_point.clone());
        self.files.set_subtitle(mount.as_deref().unwrap_or("Not mounted"));
        self.unmount.set_visible(mount.is_some());
        self.files.set_sensitive(d.connected);
        self.actions.set_sensitive(d.connected);

        let media = d.media.as_ref().filter(|m| d.connected && !m.title.is_empty());
        self.media.set_visible(media.is_some());
        if let Some(m) = media {
            self.track.set_label(&m.title);
            let by = [m.artist.as_str(), m.album.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            self.artist.set_label(&by);
            self.artist.set_visible(!by.is_empty());
            match m.album_art_url.strip_prefix("file://") {
                Some(path) => {
                    self.art.set_filename(Some(path));
                    self.art.set_visible(true);
                    self.art_placeholder.set_visible(false);
                }
                None => {
                    self.art.set_visible(false);
                    self.art_placeholder.set_visible(true);
                }
            }
            self.play.set_icon_name(if m.is_playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
            self.play.set_tooltip_text(Some(if m.is_playing { "Pause" } else { "Play" }));
            self.prev.set_sensitive(m.can_go_previous);
            self.next.set_sensitive(m.can_go_next);
            self.position.set_sensitive(m.can_seek && m.length > 0);
            self.position.set_range(0.0, m.length.max(1) as f64);
            self.duration.set_label(&fmt_time(m.length));
            if st.volume_hold.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                self.volume.set_value(m.volume as f64);
            }
        }
        drop(st);
        self.tick();
    }

    /// Advance the seek bar between pushes from the phone.
    fn tick(&self) {
        let st = self.state.borrow();
        let Some(m) = st.device().and_then(|d| d.media.as_ref()) else { return };
        if st.seek_hold.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        let pos = live_pos(m);
        self.position.set_value(pos as f64);
        self.elapsed.set_label(&fmt_time(pos));
    }

    fn sync_picker(&self) {
        let mut st = self.state.borrow_mut();
        let paired: Vec<(String, String, bool)> =
            st.devices.iter().filter(|d| d.state == "PAIRED").map(|d| (d.id.clone(), d.name.clone(), d.connected)).collect();
        if !paired.iter().any(|(id, ..)| Some(id) == st.selected.as_ref()) {
            // Keep the user's pick if still paired; otherwise prefer a connected device.
            st.selected = paired.iter().find(|(.., c)| *c).or(paired.first()).map(|(id, ..)| id.clone());
        }
        let ids: Vec<String> = paired.iter().map(|(id, ..)| id.clone()).collect();
        if ids != st.listed {
            let selected = ids.iter().position(|id| Some(id) == st.selected.as_ref()).unwrap_or(0);
            st.listed = ids;
            drop(st);
            let names: Vec<&str> = paired.iter().map(|(_, n, _)| n.as_str()).collect();
            self.picker.set_model(Some(&gtk::StringList::new(&names)));
            self.picker.set_selected(selected as u32);
            self.picker.set_visible(names.len() > 1);
        }
    }

    fn on_msg(&self, msg: Msg) {
        match msg {
            Msg::Devices(Ok(d)) => {
                let mut st = self.state.borrow_mut();
                st.devices = d;
                st.problem = None;
                drop(st);
                self.sync_picker();
            }
            Msg::Devices(Err(e)) => self.state.borrow_mut().problem = Some(e),
            Msg::Progress { file, current, total } => {
                let (row, bar) = self.transfer_row(&file);
                row.set_subtitle(&format!("{} of {}", fmt_bytes(current), fmt_bytes(total)));
                bar.set_fraction(if total > 0 { current as f64 / total as f64 } else { 0.0 });
                return;
            }
            Msg::Complete { file, success, error } => {
                let (row, bar) = self.transfer_row(&file);
                bar.set_visible(false);
                row.set_subtitle(if success { "Done" } else { &error });
                self.toast(&if success { format!("{file} transferred") } else { format!("{file} failed: {error}") });
                return;
            }
        }
        self.render();
    }

    fn transfer_row(&self, file: &str) -> (adw::ActionRow, gtk::ProgressBar) {
        let mut st = self.state.borrow_mut();
        st.transfers
            .entry(file.to_string())
            .or_insert_with(|| {
                let row = row(file, "document-send-symbolic");
                let bar = gtk::ProgressBar::builder().valign(gtk::Align::Center).width_request(120).build();
                row.add_suffix(&bar);
                self.transfers.add(&row);
                self.transfers.set_visible(true);
                (row, bar)
            })
            .clone()
    }
}

fn build(app: &adw::Application) {
    // Status
    let battery_icon = gtk::Image::from_icon_name("battery-missing-symbolic");
    let battery = adw::ActionRow::builder().title("Battery").build();
    battery.add_prefix(&battery_icon);
    let battery_bar = gtk::LevelBar::builder().min_value(0.0).max_value(100.0).valign(gtk::Align::Center).width_request(100).build();
    battery.add_suffix(&battery_bar);

    let signal_icon = gtk::Image::from_icon_name("network-cellular-signal-none-symbolic");
    let signal = adw::ActionRow::builder().title("Mobile signal").build();
    signal.add_prefix(&signal_icon);

    let files = row("Phone storage", "folder-remote-symbolic");
    let browse = icon_button("folder-open-symbolic", "Browse files");
    let unmount = icon_button("media-eject-symbolic", "Unmount");
    files.add_suffix(&unmount);
    files.add_suffix(&browse);

    let status = adw::PreferencesGroup::new();
    status.add(&battery);
    status.add(&signal);
    status.add(&files);

    // Media
    let art = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).width_request(88).height_request(88).build();
    let art_placeholder = gtk::Image::builder().icon_name("audio-x-generic-symbolic").pixel_size(40).width_request(88).height_request(88).build();
    let art_frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    art_frame.add_css_class("card");
    art_frame.set_overflow(gtk::Overflow::Hidden);
    art_frame.set_valign(gtk::Align::Center);
    // A Picture asks for the image's natural width; clamp it so wide art is cropped to a square.
    art_frame.append(&adw::Clamp::builder().maximum_size(88).tightening_threshold(88).child(&art).build());
    art_frame.append(&art_placeholder);

    let track = gtk::Label::builder().xalign(0.0).wrap(true).lines(2).ellipsize(gtk::pango::EllipsizeMode::End).build();
    track.add_css_class("heading");
    let artist = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    artist.add_css_class("dim-label");
    let prev = icon_button("media-skip-backward-symbolic", "Previous");
    let play = icon_button("media-playback-start-symbolic", "Play");
    let next = icon_button("media-skip-forward-symbolic", "Next");
    play.add_css_class("circular");
    play.add_css_class("suggested-action");
    for b in [&prev, &next] {
        b.add_css_class("flat");
        b.add_css_class("circular");
    }
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.append(&prev);
    controls.append(&play);
    controls.append(&next);

    let info = gtk::Box::new(gtk::Orientation::Vertical, 4);
    info.set_valign(gtk::Align::Center);
    info.set_hexpand(true);
    info.append(&track);
    info.append(&artist);
    info.append(&controls);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    top.append(&art_frame);
    top.append(&info);

    let position = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1000.0);
    position.set_hexpand(true);
    let elapsed = gtk::Label::new(Some("0:00"));
    let duration = gtk::Label::new(Some("0:00"));
    for l in [&elapsed, &duration] {
        l.add_css_class("numeric");
        l.add_css_class("caption");
        l.add_css_class("dim-label");
    }
    let seek_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    seek_row.append(&elapsed);
    seek_row.append(&position);
    seek_row.append(&duration);

    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume.set_hexpand(true);
    let volume_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    volume_row.append(&gtk::Image::from_icon_name("audio-volume-medium-symbolic"));
    volume_row.append(&volume);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("card");
    let inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
    inner.set_margin_top(14);
    inner.set_margin_bottom(14);
    inner.set_margin_start(14);
    inner.set_margin_end(14);
    inner.append(&top);
    inner.append(&seek_row);
    inner.append(&volume_row);
    card.append(&inner);

    let media = adw::PreferencesGroup::builder().title("Now playing").visible(false).build();
    media.add(&card);

    // Actions
    let send_row = row("Send files…", "document-send-symbolic");
    let clip_row = row("Send clipboard", "edit-paste-symbolic");
    let ring_row = row("Ring phone", "audio-volume-high-symbolic");
    let ping_row = row("Ping", "network-transmit-symbolic");
    let actions = adw::PreferencesGroup::builder().title("Actions").description("Or drop files anywhere on this window to send them.").build();
    for r in [&send_row, &clip_row, &ring_row, &ping_row] {
        r.set_activatable(true);
        r.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        actions.add(r);
    }

    let transfers = adw::PreferencesGroup::builder().title("Transfers").visible(false).build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(18);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    for g in [&status, &media, &actions, &transfers] {
        content.append(g);
    }
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(860).child(&content).build())
        .build();

    let empty = adw::StatusPage::builder().icon_name("phone-symbolic").title("Connecting to kcd…").build();
    let stack = gtk::Stack::new();
    stack.add_named(&scrolled, Some("device"));
    stack.add_named(&empty, Some("empty"));
    stack.set_visible_child_name("empty");

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let title = adw::WindowTitle::new("KDE Connect", "");
    let picker = gtk::DropDown::builder().visible(false).tooltip_text("Device").build();
    let menu = gio::Menu::new();
    menu.append(Some("Unpair…"), Some("win.unpair"));
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").tooltip_text("Device menu").menu_model(&menu).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&picker);
    header.pack_end(&menu_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("KDE Connect")
        .default_width(900)
        .default_height(720)
        .content(&toolbar)
        .build();

    let ui = Ui {
        window: window.clone(),
        toasts,
        stack,
        empty,
        title,
        picker: picker.clone(),
        battery,
        battery_icon,
        battery_bar,
        signal,
        signal_icon,
        files,
        unmount: unmount.clone(),
        media,
        art,
        art_placeholder,
        track,
        artist,
        prev: prev.clone(),
        play: play.clone(),
        next: next.clone(),
        position: position.clone(),
        elapsed,
        duration,
        volume: volume.clone(),
        actions,
        transfers,
        state: Rc::default(),
    };

    // Wiring
    let u = ui.clone();
    picker.connect_selected_notify(move |p| {
        let id = u.state.borrow().listed.get(p.selected() as usize).cloned();
        if id.is_some() {
            u.state.borrow_mut().selected = id;
            u.render();
        }
    });
    // kcd opens the file manager itself after mounting ([sftp] auto_open).
    let u = ui.clone();
    browse.connect_clicked(move |_| u.device_call("sftp_mount_local", json!({}), Some("Opening phone storage…")));
    let u = ui.clone();
    unmount.connect_clicked(move |_| u.device_call("sftp_unmount", json!({}), None));
    // The phone only accepts KDE Connect's PascalCase action names.
    for (button, action) in [(&prev, "Previous"), (&play, "PlayPause"), (&next, "Next")] {
        let u = ui.clone();
        button.connect_clicked(move |_| u.device_call("mpris_action", json!({ "action": action }), None));
    }

    // Sliders fire change-value only on user input. Debounce so a drag sends one request, and hold off
    // updates from the phone meanwhile so the knob doesn't jump back.
    let u = ui.clone();
    position.connect_change_value(move |_, _, target| {
        let mut st = u.state.borrow_mut();
        st.seek_hold = Some(Instant::now());
        st.seek_gen += 1;
        let generation = st.seek_gen;
        drop(st);
        u.elapsed.set_label(&fmt_time(target as i64));
        let u = u.clone();
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            let st = u.state.borrow();
            let Some(m) = st.device().and_then(|d| d.media.as_ref()) else { return };
            if st.seek_gen != generation {
                return;
            }
            // kcd's seek is a relative offset in ms.
            let offset = target as i64 - live_pos(m);
            drop(st);
            u.device_call("mpris_action", json!({ "seek": offset }), None);
        });
        glib::Propagation::Proceed
    });
    let u = ui.clone();
    volume.connect_change_value(move |_, _, value| {
        let mut st = u.state.borrow_mut();
        st.volume_hold = Some(Instant::now());
        st.volume_gen += 1;
        let generation = st.volume_gen;
        drop(st);
        let u = u.clone();
        glib::timeout_add_local_once(Duration::from_millis(200), move || {
            if u.state.borrow().volume_gen == generation {
                u.device_call("mpris_action", json!({ "volume": value.round() as i64 }), None);
            }
        });
        glib::Propagation::Proceed
    });

    let u = ui.clone();
    send_row.connect_activated(move |_| {
        let u = u.clone();
        glib::spawn_future_local(async move {
            let dialog = gtk::FileDialog::builder().title("Send files to phone").build();
            if let Ok(list) = dialog.open_multiple_future(Some(&u.window)).await {
                let paths = list.iter::<gio::File>().flatten().filter_map(|f| f.path()).map(|p| p.to_string_lossy().into_owned()).collect();
                u.send_files(paths);
            }
        });
    });
    let u = ui.clone();
    clip_row.connect_activated(move |_| u.device_call("clipboard_push", json!({}), Some("Clipboard sent")));
    let u = ui.clone();
    ring_row.connect_activated(move |_| u.device_call("findmyphone", json!({}), Some("Ringing your phone")));
    let u = ui.clone();
    ping_row.connect_activated(move |_| u.device_call("ping", json!({}), Some("Ping sent")));

    let unpair = gio::SimpleAction::new("unpair", None);
    window.add_action(&unpair);
    let u = ui.clone();
    unpair.connect_activate(move |_, _| {
        let Some(name) = u.state.borrow().device().map(|d| d.name.clone()) else { return };
        let dialog = adw::AlertDialog::new(Some(&format!("Unpair {name}?")), Some("You will need the phone at hand to pair again."));
        dialog.add_responses(&[("cancel", "Cancel"), ("unpair", "Unpair")]);
        dialog.set_response_appearance("unpair", adw::ResponseAppearance::Destructive);
        let u2 = u.clone();
        dialog.connect_response(Some("unpair"), move |_, _| u2.device_call("unpair", json!({}), None));
        dialog.present(Some(&u.window));
    });

    let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop_target.connect_drop(move |_, value, _, _| {
        let Ok(list) = value.get::<gdk::FileList>() else { return false };
        let connected = u.state.borrow().device().is_some_and(|d| d.connected);
        if !connected {
            u.toast("Phone is not connected");
            return false;
        }
        u.send_files(list.files().iter().filter_map(|f| f.path()).map(|p| p.to_string_lossy().into_owned()).collect());
        true
    });
    window.add_controller(drop_target);

    let u = ui.clone();
    glib::timeout_add_local(Duration::from_millis(500), move || {
        u.tick();
        glib::ControlFlow::Continue
    });

    let (tx, rx) = async_channel::unbounded();
    spawn_watcher(tx);
    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            u.on_msg(msg);
        }
    });

    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_live_position() {
        assert_eq!(fmt_time(0), "0:00");
        assert_eq!(fmt_time(252_000), "4:12");
        assert_eq!(fmt_bytes(512 * 1024), "512 KB");
        assert_eq!(fmt_bytes(3 << 20), "3.0 MB");
        let paused = Media { pos: 5000, pos_anchor_ms: 0, length: 10_000, ..Default::default() };
        assert_eq!(live_pos(&paused), 5000);
        let playing = Media { is_playing: true, pos: 5000, pos_anchor_ms: now_ms() - 2000, length: 10_000, ..Default::default() };
        assert!((6900..=7100).contains(&live_pos(&playing)));
        let overrun = Media { is_playing: true, pos: 9000, pos_anchor_ms: now_ms() - 5000, length: 10_000, ..Default::default() };
        assert_eq!(live_pos(&overrun), 10_000);
    }
}
