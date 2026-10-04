//! Recent phone notifications. kcd keeps no history and its `notification` events carry no id (so they
//! can't be dismissed or matched to `notification.canceled`), so the always-running tray records them to a
//! small file in the runtime dir and the window lists them, with inline reply where the app supports it.

use super::{Ui, fmt_when};
use adw::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

const KEEP: usize = 100;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Notification {
    pub device_id: String,
    pub app: String,
    pub title: String,
    pub text: String,
    /// Non-empty when the app accepts inline replies (`notify_reply`).
    pub reply_id: String,
    pub time_ms: i64,
}

impl Notification {
    pub fn from_event(device_id: &str, p: &Value) -> Self {
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        Notification {
            device_id: device_id.into(),
            app: s("appName"),
            title: s("title"),
            text: s("text"),
            reply_id: s("requestReplyId"),
            time_ms: super::now_ms(),
        }
    }
}

/// `$XDG_RUNTIME_DIR/kcd-indicator/notifications.json`: tmpfs, private to the user, gone at logout.
fn store_path() -> PathBuf {
    crate::ipc::socket_path().parent().and_then(|kcd| kcd.parent()).unwrap_or(std::path::Path::new("/tmp")).join("kcd-indicator/notifications.json")
}

pub fn load() -> Vec<Notification> {
    std::fs::read(store_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(list: &[Notification]) -> std::io::Result<()> {
    let path = store_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(list)?)?;
    std::fs::rename(tmp, path) // atomic, so the window never reads half a file
}

/// The phone forgets reply handles when the connection ends and each works once: clear the device's reply
/// ids (all of them, or just `reply_id`). Returns whether anything changed.
fn forget_replies(list: &mut [Notification], device_id: &str, reply_id: Option<&str>) -> bool {
    let mut changed = false;
    for n in list.iter_mut().filter(|n| n.device_id == device_id && !n.reply_id.is_empty()) {
        if reply_id.is_none_or(|r| r == n.reply_id) {
            n.reply_id.clear();
            changed = true;
        }
    }
    changed
}

/// Called by the tray when a device connects or disconnects.
pub fn forget_device_replies(device_id: &str) -> std::io::Result<()> {
    let mut list = load();
    if forget_replies(&mut list, device_id, None) { save(&list) } else { Ok(()) }
}

/// Called by the tray for every `notification` event; newest first, capped at `KEEP`.
pub fn record(n: Notification) -> std::io::Result<()> {
    let mut list = load();
    list.insert(0, n);
    list.truncate(KEEP);
    save(&list)
}

#[derive(Clone)]
pub(super) struct Page {
    pub(super) root: gtk::Stack,
    clear: gtk::Button,
    group: adw::PreferencesGroup,
    rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    list: Rc<RefCell<Vec<Notification>>>,
}

impl Page {
    pub(super) fn new() -> Self {
        let clear = gtk::Button::builder().label("Clear").valign(gtk::Align::Center).css_classes(["flat"]).build();
        let group = adw::PreferencesGroup::builder()
            .title("Recent notifications")
            .description("Recorded by the tray while it runs. Reply where the app allows it.")
            .header_suffix(&clear)
            .build();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(18).margin_bottom(24).margin_start(12).margin_end(12).build();
        content.append(&group);
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&adw::Clamp::builder().maximum_size(860).child(&content).build())
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("preferences-system-notifications-symbolic")
            .title("No notifications yet")
            .description("Notifications from your phone appear here while the tray is running.")
            .build();
        let root = gtk::Stack::new();
        root.add_named(&scrolled, Some("list"));
        root.add_named(&empty, Some("empty"));

        Page { root, clear, group, rows: Rc::default(), list: Rc::new(RefCell::new(load())) }
    }

    pub(super) fn connect(&self, ui: &Ui) {
        let (p, ui) = (self.clone(), ui.clone());
        self.clear.connect_clicked(move |_| {
            p.list.borrow_mut().clear();
            if let Err(e) = save(&[]) {
                log!("cannot clear notifications: {e}");
            }
            p.render(&ui);
        });
    }

    pub(super) fn push(&self, n: Notification, ui: &Ui) {
        // The tray records it too; this keeps an open window live without re-reading the file.
        let mut list = self.list.borrow_mut();
        list.insert(0, n);
        list.truncate(KEEP);
        drop(list);
        self.render(ui);
    }

    /// Drop stale reply ids in the window and the shared store.
    pub(super) fn forget_replies(&self, device_id: &str, reply_id: Option<&str>, ui: &Ui) {
        if !forget_replies(&mut self.list.borrow_mut(), device_id, reply_id) {
            return;
        }
        let mut stored = load();
        if forget_replies(&mut stored, device_id, reply_id) && let Err(e) = save(&stored) {
            log!("cannot update notifications: {e}");
        }
        self.render(ui);
    }

    pub(super) fn render(&self, ui: &Ui) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        let device = ui.state.borrow().selected.clone().unwrap_or_default();
        let list = self.list.borrow();
        let mine: Vec<&Notification> = list.iter().filter(|n| n.device_id == device).collect();
        self.root.set_visible_child_name(if mine.is_empty() { "empty" } else { "list" });
        for n in mine {
            let title = match (n.app.is_empty(), n.title.is_empty()) {
                (false, false) => format!("{} · {}", n.app, n.title),
                (true, _) => n.title.clone(),
                (_, true) => n.app.clone(),
            };
            let row = super::plain_row(&title, &n.text);
            row.set_subtitle_lines(4);
            row.add_prefix(&gtk::Image::from_icon_name("preferences-system-notifications-symbolic"));
            let when = gtk::Label::new(Some(&fmt_when(n.time_ms)));
            when.add_css_class("dim-label");
            when.add_css_class("caption");
            row.add_suffix(&when);
            if !n.reply_id.is_empty() {
                let reply = gtk::Button::builder().icon_name("mail-reply-sender-symbolic").tooltip_text("Reply").valign(gtk::Align::Center).css_classes(["flat"]).build();
                let (ui2, page, device, reply_id, to) = (ui.clone(), self.clone(), n.device_id.clone(), n.reply_id.clone(), title.clone());
                reply.connect_clicked(move |_| {
                    let (page, device, used) = (page.clone(), device.clone(), reply_id.clone());
                    ask_reply(&ui2, &to, reply_id.clone(), move |ui| page.forget_replies(&device, Some(&used), ui));
                });
                row.add_suffix(&reply);
            }
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
        }
    }
}

/// `sent` runs once the reply is handed to kcd; the phone accepts each reply id only once.
fn ask_reply(ui: &Ui, to: &str, reply_id: String, sent: impl Fn(&Ui) + 'static) {
    let entry = gtk::Entry::builder().placeholder_text("Reply").activates_default(true).build();
    let dialog = adw::AlertDialog::new(Some("Reply"), Some(to));
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    let ui2 = ui.clone();
    dialog.connect_response(Some("send"), move |_, _| {
        let text = entry.text().to_string();
        if !text.trim().is_empty() {
            ui2.device_call("notify_reply", json!({ "replyId": reply_id, "message": text }), Some("Reply sent"));
            sent(&ui2);
        }
    });
    dialog.present(Some(&ui.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_payload() {
        let n = Notification::from_event("d", &json!({"appName": "WhatsApp", "title": "Ada", "text": "hi", "requestReplyId": "r1"}));
        assert_eq!((n.app.as_str(), n.title.as_str(), n.text.as_str(), n.reply_id.as_str()), ("WhatsApp", "Ada", "hi", "r1"));
        assert!(Notification::from_event("d", &json!({"title": "x"})).reply_id.is_empty());
    }

    #[test]
    fn forgets_reply_ids() {
        let n = |dev: &str, r: &str| Notification { device_id: dev.into(), reply_id: r.into(), ..Default::default() };
        let mut list = vec![n("a", "r1"), n("a", "r2"), n("b", "r3")];
        assert!(forget_replies(&mut list, "a", Some("r1")));
        assert_eq!(list.iter().map(|n| n.reply_id.as_str()).collect::<Vec<_>>(), ["", "r2", "r3"]);
        assert!(forget_replies(&mut list, "a", None));
        assert_eq!(list.iter().map(|n| n.reply_id.as_str()).collect::<Vec<_>>(), ["", "", "r3"]);
        assert!(!forget_replies(&mut list, "a", None));
    }
}
