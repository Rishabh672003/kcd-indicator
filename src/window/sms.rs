//! Messages page. kcd stores no SMS: the phone answers `sms_request_conversations` with the latest message
//! of every thread and `sms_request_conversation` with a page of one thread, all as `sms.incoming` events.
//! We assemble threads from those events, de-duplicating by (date, type) since kcd's `u_id` is always 0.

use super::{Ui, fmt_when, now_ms};
use adw::prelude::*;
use gtk::glib;
use serde::Deserialize;
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

/// Messages fetched per thread page.
const PAGE: i64 = 50;

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Sms {
    pub body: String,
    /// The other party for both directions (kcd keeps only the first address).
    pub sender: String,
    pub date: i64,
    /// 1 = received, 2 = sent.
    #[serde(rename = "type")]
    pub kind: i64,
    pub thread_id: i64,
    pub attachments: Option<Vec<serde_json::Value>>,
}

#[derive(Default)]
struct Thread {
    address: String,
    messages: BTreeMap<(i64, i64), Sms>,
}

#[derive(Default)]
struct State {
    threads: HashMap<i64, Thread>,
    /// Last 10 digits of a number → contact name.
    contacts: HashMap<String, String>,
    selected: Option<i64>,
    /// List row per thread; the row's widget name is the thread id.
    rows: HashMap<i64, (adw::ActionRow, gtk::Label)>,
    /// Threads changed since the last render.
    dirty: HashSet<i64>,
    render_queued: bool,
}

/// Compare numbers by their last 10 digits so "+91 98…" and "098…" match.
fn number_key(n: &str) -> String {
    let digits: String = n.chars().filter(char::is_ascii_digit).collect();
    digits[digits.len().saturating_sub(10)..].to_string()
}

impl State {
    fn last_date(&self, id: i64) -> i64 {
        self.threads.get(&id).and_then(|t| t.messages.keys().last()).map_or(0, |(date, _)| *date)
    }

    fn name(&self, address: &str) -> String {
        self.contacts.get(&number_key(address)).cloned().unwrap_or_else(|| address.to_string())
    }
}

#[derive(Clone)]
pub struct Page {
    pub root: gtk::Paned,
    list: gtk::ListBox,
    stack: gtk::Stack,
    who: adw::WindowTitle,
    convo: gtk::Box,
    scroll: gtk::ScrolledWindow,
    older: gtk::Button,
    entry: gtk::Entry,
    send: gtk::Button,
    new_message: gtk::Button,
    state: Rc<RefCell<State>>,
}

impl Page {
    pub fn new() -> Self {
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["navigation-sidebar"]).build();
        let new_message = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("New message").build())
            .css_classes(["flat"])
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.append(&new_message);
        sidebar.append(&gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&list).build());
        sidebar.set_size_request(300, -1);

        let who = adw::WindowTitle::new("", "");
        let older = gtk::Button::builder().label("Load older").css_classes(["flat"]).build();
        let top = gtk::CenterBox::builder().margin_top(6).margin_bottom(6).margin_start(12).margin_end(12).build();
        top.set_center_widget(Some(&who));
        top.set_end_widget(Some(&older));

        let convo = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(12).margin_bottom(12).margin_start(16).margin_end(16).build();
        let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&convo).build();

        let entry = gtk::Entry::builder().placeholder_text("Text message").hexpand(true).build();
        let send = gtk::Button::builder().icon_name("mail-send-symbolic").tooltip_text("Send").css_classes(["suggested-action", "circular"]).build();
        let compose = gtk::Box::builder().spacing(8).margin_top(8).margin_bottom(10).margin_start(12).margin_end(12).build();
        compose.append(&entry);
        compose.append(&send);

        let thread_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        thread_view.append(&top);
        thread_view.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        thread_view.append(&scroll);
        thread_view.append(&compose);

        let pick = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("Messages")
            .description("Pick a conversation, or start a new one.")
            .build();
        let loading = adw::StatusPage::builder().title("Loading conversations…").description("Asking your phone for its messages.").build();
        loading.set_child(Some(&gtk::Spinner::builder().spinning(true).halign(gtk::Align::Center).width_request(32).height_request(32).build()));
        let stack = gtk::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&pick, Some("pick"));
        stack.add_named(&thread_view, Some("thread"));

        let root = gtk::Paned::builder().orientation(gtk::Orientation::Horizontal).start_child(&sidebar).end_child(&stack).position(320).shrink_start_child(false).build();
        Page { root, list, stack, who, convo, scroll, older, entry, send, new_message, state: Rc::default() }
    }

    pub fn connect(&self, ui: &Ui) {
        let p = self.clone();
        self.list.set_sort_func(move |a, b| {
            let st = p.state.borrow();
            let date = |r: &gtk::ListBoxRow| r.widget_name().parse().map_or(0, |id| st.last_date(id));
            date(b).cmp(&date(a)).into()
        });
        let (p, u) = (self.clone(), ui.clone());
        self.list.connect_row_activated(move |_, row| {
            if let Ok(id) = row.widget_name().parse() {
                p.state.borrow_mut().selected = Some(id);
                p.render_thread();
                p.entry.grab_focus(); // so typing goes to the reply box, not the list
                p.request_thread(&u, id, now_ms() + 60_000);
            }
        });
        let (p, u) = (self.clone(), ui.clone());
        self.older.connect_clicked(move |_| {
            let st = p.state.borrow();
            let Some(id) = st.selected else { return };
            let oldest = st.threads.get(&id).and_then(|t| t.messages.keys().next()).map_or(now_ms(), |(date, _)| *date);
            drop(st);
            p.request_thread(&u, id, oldest);
        });
        let (p, u) = (self.clone(), ui.clone());
        let send = move || {
            let text = p.entry.text().to_string();
            let st = p.state.borrow();
            let Some(to) = st.selected.and_then(|id| st.threads.get(&id)).map(|t| t.address.clone()) else { return };
            drop(st);
            if !text.trim().is_empty() {
                // The phone echoes the sent message back as an `sms.incoming` of type 2.
                u.device_call("send_sms", json!({ "phoneNumber": to, "message": text }), Some("Message sent"));
                p.entry.set_text("");
            }
        };
        let s = send.clone();
        self.entry.connect_activate(move |_| s());
        self.send.connect_clicked(move |_| send());
        let u = ui.clone();
        self.new_message.connect_clicked(move |_| ask_new_message(&u));
    }

    /// The phone answers with up to `PAGE` messages older than `before` (ms).
    fn request_thread(&self, ui: &Ui, thread: i64, before: i64) {
        // kcd sends 0 for omitted range fields, which asks for nothing; always pass both.
        ui.device_call("sms_request_conversation", json!({ "threadID": thread, "rangeStartTimestamp": before, "numberToRequest": PAGE }), None);
    }

    /// Ask for the latest message of every thread. Call once the event subscription is live.
    pub fn request_list(&self, ui: &Ui) {
        if self.state.borrow().threads.is_empty() {
            self.stack.set_visible_child_name("loading");
        }
        ui.device_call("sms_request_conversations", json!({}), None);
    }

    /// Forget everything (device switched).
    pub fn reset(&self) {
        let mut st = self.state.borrow_mut();
        st.threads.clear();
        st.selected = None;
        st.rows.clear();
        st.dirty.clear();
        drop(st);
        self.list.remove_all();
    }

    pub fn set_contacts(&self, contacts: &serde_json::Value) {
        let mut map = HashMap::new();
        for c in contacts.as_array().into_iter().flatten() {
            let name = c["name"].as_str().unwrap_or_default();
            for phone in c["phones"].as_array().into_iter().flatten().filter_map(|p| p.as_str()) {
                map.insert(number_key(phone), name.to_string());
            }
        }
        let mut st = self.state.borrow_mut();
        st.contacts = map;
        let all: Vec<i64> = st.threads.keys().copied().collect();
        st.dirty.extend(all);
        drop(st);
        self.queue_render();
    }

    pub fn add(&self, sms: Sms) {
        let mut st = self.state.borrow_mut();
        let thread = st.threads.entry(sms.thread_id).or_default();
        if thread.address.is_empty() {
            thread.address = sms.sender.clone();
        }
        let id = sms.thread_id;
        thread.messages.insert((sms.date, sms.kind), sms);
        st.dirty.insert(id);
        drop(st);
        self.queue_render();
    }

    /// Conversations arrive as hundreds of events in a burst; coalesce them into one redraw.
    fn queue_render(&self) {
        let mut st = self.state.borrow_mut();
        if st.render_queued {
            return;
        }
        st.render_queued = true;
        drop(st);
        let p = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            let mut st = p.state.borrow_mut();
            st.render_queued = false;
            let dirty = std::mem::take(&mut st.dirty);
            let thread_changed = st.selected.is_some_and(|id| dirty.contains(&id));
            drop(st);
            p.render_list(&dirty);
            if thread_changed {
                p.render_thread();
            }
        });
    }

    /// Update the rows of changed threads; the list's sort func keeps them newest first.
    fn render_list(&self, dirty: &HashSet<i64>) {
        let mut st = self.state.borrow_mut();
        let mut added = Vec::new();
        for id in dirty {
            let Some(t) = st.threads.get(id) else { continue };
            let Some(last) = t.messages.values().last() else { continue };
            let preview = if last.kind == 2 { format!("You: {}", one_line(&last.body)) } else { one_line(&last.body) };
            let (title, when) = (st.name(&t.address), fmt_when(last.date));
            match st.rows.get(id) {
                Some((row, label)) => {
                    row.set_title(&title);
                    row.set_subtitle(&preview);
                    label.set_label(&when);
                }
                None => {
                    let row = super::plain_row(&title, &preview);
                    row.set_subtitle_lines(1);
                    row.set_activatable(true);
                    row.set_widget_name(&id.to_string());
                    let label = gtk::Label::new(Some(&when));
                    label.add_css_class("dim-label");
                    label.add_css_class("caption");
                    row.add_suffix(&label);
                    st.rows.insert(*id, (row.clone(), label));
                    added.push(row);
                }
            }
        }
        let selected = st.selected.and_then(|id| st.rows.get(&id)).map(|(row, _)| row.clone());
        let show_pick = st.selected.is_none() && !st.threads.is_empty();
        drop(st); // the sort func borrows the state
        for row in &added {
            self.list.append(row);
        }
        self.list.invalidate_sort();
        if let Some(row) = selected {
            self.list.select_row(Some(&row));
        }
        if show_pick && self.stack.visible_child_name().as_deref() == Some("loading") {
            self.stack.set_visible_child_name("pick");
        }
    }

    fn render_thread(&self) {
        let st = self.state.borrow();
        let Some(t) = st.selected.and_then(|id| st.threads.get(&id)) else { return };
        self.stack.set_visible_child_name("thread");
        let name = st.name(&t.address);
        self.who.set_title(&name);
        self.who.set_subtitle(if name == t.address { "" } else { &t.address });
        while let Some(child) = self.convo.first_child() {
            self.convo.remove(&child);
        }
        for m in t.messages.values() {
            let sent = m.kind == 2;
            let mut text = m.body.clone();
            if m.attachments.as_ref().is_some_and(|a| !a.is_empty()) {
                text = format!("[attachment] {text}");
            }
            let bubble = gtk::Label::builder().label(text.trim()).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).max_width_chars(48).xalign(0.0).selectable(true).build();
            bubble.add_css_class("sms-bubble");
            if sent {
                bubble.add_css_class("sms-sent");
            }
            let time = gtk::Label::new(Some(&fmt_when(m.date)));
            time.add_css_class("dim-label");
            time.add_css_class("caption");
            let item = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).halign(if sent { gtk::Align::End } else { gtk::Align::Start }).build();
            time.set_halign(item.halign());
            item.append(&bubble);
            item.append(&time);
            self.convo.append(&item);
        }
        // Stick to the newest message once layout has run.
        let adj = self.scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ask_new_message(ui: &Ui) {
    let to = gtk::Entry::builder().placeholder_text("Phone number").input_purpose(gtk::InputPurpose::Phone).build();
    let text = gtk::Entry::builder().placeholder_text("Message").activates_default(true).build();
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
    fields.append(&to);
    fields.append(&text);
    let dialog = adw::AlertDialog::new(Some("New message"), None);
    dialog.set_extra_child(Some(&fields));
    dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    let u = ui.clone();
    dialog.connect_response(Some("send"), move |_, _| {
        let (number, message) = (to.text().trim().to_string(), text.text().to_string());
        if number.is_empty() || message.trim().is_empty() {
            u.toast("Enter a number and a message");
            return;
        }
        u.device_call("send_sms", json!({ "phoneNumber": number, "message": message }), Some("Message sent"));
    });
    dialog.present(Some(&ui.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_numbers_and_parses_events() {
        assert_eq!(number_key("+91 98765-43210"), "9876543210");
        assert_eq!(number_key("09876543210"), "9876543210");
        assert_eq!(number_key("123"), "123");
        let sms: Sms = serde_json::from_value(json!({"body": "hi", "sender": "+1", "date": 5, "type": 2, "thread_id": 7, "read": true, "u_id": 0})).unwrap();
        assert_eq!((sms.kind, sms.thread_id, sms.date), (2, 7, 5));
        assert_eq!(one_line("a\n  b\tc"), "a b c");
    }
}
