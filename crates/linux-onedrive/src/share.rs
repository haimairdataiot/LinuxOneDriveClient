//! The Share dialog, modelled on the official client's sharing panel
//! (research doc 05 §A2/§C2), in three pages:
//!
//! * **Share**: add names or email addresses, choose Can edit / Can view,
//!   add a message, Send; below it the "Copy link" row with its settings.
//! * **Link settings**: who the link works for (Anyone, People in your
//!   organization, People with existing access, Specific people), the
//!   permission, an expiry date and a password.
//! * **Manage access**: every link and person with access, with copy,
//!   change and remove actions, and **Stop sharing**.

use crate::client;
use crate::util;
use adw::prelude::*;
use gtk::glib;
use od_core::dbus::{
    AccessEntry, InviteOutcome, LinkOptions, LinkResult, PathInfo, Person, SharingInfo, StopSharingOutcome,
};
use std::cell::RefCell;
use std::rc::Rc;
use webkit::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Share,
    LinkSettings,
    ManageAccess,
}

struct State {
    account: String,
    path: String,
    info: SharingInfo,
    link: LinkOptions,
    recipients: Vec<String>,
}

#[derive(Clone)]
struct Dialog {
    dialog: adw::Dialog,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    state: Rc<RefCell<State>>,
    link_row: adw::ActionRow,
    chips: gtk::FlowBox,
    access_list: gtk::Box,
}

async fn account_for(path: &str) -> zbus::Result<String> {
    let json = client::manager().await?.paths_info(&[path]).await?;
    let infos: Vec<PathInfo> = serde_json::from_str(&json).unwrap_or_default();
    infos
        .into_iter()
        .next()
        .map(|i| i.account)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| zbus::Error::Failure("This item isn't in a OneDrive folder.".into()))
}

/// Human description of a link: "Anyone with the link can edit".
pub fn describe(audience: &str, role: &str, org: &str) -> String {
    let who = match audience {
        "organization" => format!("People in {}", if org.is_empty() { "your organization" } else { org }),
        "existing" => "People with existing access".to_string(),
        "users" => "Specific people".to_string(),
        _ => "Anyone with the link".to_string(),
    };
    let what = match role {
        "edit" => "can edit",
        "review" => "can review",
        "blocksDownload" => "can view but not download",
        "owner" => "owner",
        _ => "can view",
    };
    format!("{who} {what}")
}

fn date_text(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%-d %B %Y").to_string())
        .unwrap_or_default()
}

fn expiry_text(ts: i64) -> String {
    format!("Expires {}", date_text(ts))
}

/// Open the dialog for a file or folder inside a OneDrive folder.
pub fn open(parent: Option<gtk::Widget>, path: &str, page: Page) {
    let path = path.to_string();
    let p = path.clone();
    client::call(
        async move {
            let account = account_for(&p).await?;
            let info: SharingInfo =
                serde_json::from_str(&client::account(&account).await?.get_sharing_info(&p).await?).unwrap_or_default();
            Ok::<_, zbus::Error>((account, info))
        },
        move |r| match r {
            Err(e) => util::error_dialog(
                parent.as_ref().unwrap_or(&gtk::Label::new(None).upcast()),
                "Can't share",
                &util::friendly(&e),
            ),
            Ok((_, info)) if !info.can_share => {
                let d = adw::AlertDialog::new(Some("Can't share this item"), Some(&info.reason));
                d.add_response("ok", "OK");
                d.present(parent.as_ref());
            }
            Ok((account, info)) => build(parent, account, path, info, page),
        },
    );
}

/// "Copy link" without opening the dialog (file-manager action).
pub fn copy_link(widget: &impl IsA<gtk::Widget>, path: &str, on_done: impl FnOnce(Result<String, String>) + 'static) {
    let p = path.to_string();
    let w = widget.as_ref().clone();
    client::call(
        async move {
            let account = account_for(&p).await?;
            client::account(&account).await?.copy_link(&p).await
        },
        move |r: zbus::Result<String>| match r {
            Ok(url) => {
                util::copy_to_clipboard(&w, &url);
                on_done(Ok(url));
            }
            Err(e) => on_done(Err(util::friendly(&e))),
        },
    );
}

fn page_widget(
    title: &str,
    tag: &str,
    child: &impl IsA<gtk::Widget>,
    header_end: Option<&gtk::Widget>,
) -> adw::NavigationPage {
    let tv = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    if let Some(w) = header_end {
        hb.pack_end(w);
    }
    tv.add_top_bar(&hb);
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
    let clamp = adw::Clamp::builder()
        .maximum_size(520)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .child(child)
        .build();
    scroller.set_child(Some(&clamp));
    tv.set_content(Some(&scroller));
    adw::NavigationPage::builder().title(title).tag(tag).child(&tv).build()
}

fn build(parent: Option<gtk::Widget>, account: String, path: String, info: SharingInfo, page: Page) {
    let business = info.drive_type != "personal";
    let link =
        LinkOptions { audience: info.default_audience.clone(), role: info.default_role.clone(), ..Default::default() };
    let name = info.name.clone();
    let state = Rc::new(RefCell::new(State { account, path, info, link, recipients: Vec::new() }));
    let dialog = adw::Dialog::builder()
        .title(format!("Share \u{201c}{name}\u{201d}"))
        .content_width(480)
        .content_height(640)
        .build();
    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));
    dialog.set_child(Some(&toasts));

    let link_row = adw::ActionRow::builder().use_markup(false).build();
    let chips = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(3)
        .row_spacing(4)
        .column_spacing(4)
        .margin_top(6)
        .visible(false)
        .build();
    let access_list = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let d = Dialog { dialog: dialog.clone(), nav: nav.clone(), toasts, state, link_row, chips, access_list };

    nav.add(&d.share_page(business));
    nav.add(&d.link_settings_page(business));
    nav.add(&d.manage_page());
    d.update_link_row();
    match page {
        Page::Share => nav.replace_with_tags(&["share"]),
        Page::LinkSettings => nav.replace_with_tags(&["share", "link"]),
        Page::ManageAccess => {
            nav.replace_with_tags(&["share", "manage"]);
            d.load_access();
        }
    }
    dialog.present(parent.as_ref());
}

impl Dialog {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn account(&self) -> String {
        self.state.borrow().account.clone()
    }

    fn path(&self) -> String {
        self.state.borrow().path.clone()
    }

    // ------------------------------------------------------------ page 1: Share

    fn share_page(&self, business: bool) -> adw::NavigationPage {
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 18);

        // Recipients
        let people = adw::PreferencesGroup::builder().title("Share with people").build();
        let entry = adw::EntryRow::builder()
            .title("Add a name, group or email")
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let suggestions = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .visible(false)
            .build();
        people.add(&entry);
        let options = adw::PreferencesGroup::new();
        let role = adw::ComboRow::builder()
            .title("Permission")
            .model(&gtk::StringList::new(&["Can edit", "Can view"]))
            .build();
        options.add(&role);
        let notify = adw::SwitchRow::builder().title("Notify people by email").active(true).build();
        options.add(&notify);
        let message = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .height_request(72)
            .build();
        let message_frame = gtk::Frame::builder().child(&message).css_classes(["card"]).build();
        let message_label =
            gtk::Label::builder().label("Message (optional)").xalign(0.0).css_classes(["dim-label", "caption"]).build();
        let send = gtk::Button::builder()
            .label("Send")
            .halign(gtk::Align::End)
            .css_classes(["suggested-action", "pill"])
            .sensitive(false)
            .build();

        let people_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        people_box.append(&people);
        people_box.append(&suggestions);
        people_box.append(&self.chips);
        options.set_margin_top(6);
        people_box.append(&options);
        people_box.append(&message_label);
        people_box.append(&message_frame);
        people_box.append(&send);
        if !self.state.borrow().info.people_suggestions {
            let enable = gtk::Button::builder()
                .label("Suggest people from Microsoft 365")
                .css_classes(["flat", "caption"])
                .halign(gtk::Align::Start)
                .build();
            let me = self.clone();
            enable.connect_clicked(move |b| me.enable_suggestions(b));
            people_box.append(&enable);
        }
        vbox.append(&people_box);

        // Copy link
        let link_group = adw::PreferencesGroup::builder().title("Copy link").build();
        self.link_row.add_prefix(&gtk::Image::from_icon_name("insert-link-symbolic"));
        let settings = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Link settings")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let copy =
            gtk::Button::builder().label("Copy").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        self.link_row.add_suffix(&settings);
        self.link_row.add_suffix(&copy);
        link_group.add(&self.link_row);
        vbox.append(&link_group);
        let note = gtk::Label::builder()
            .label(if business {
                "Links follow your organization's sharing policy."
            } else {
                "Anyone who gets a link like this can open it, even without a Microsoft account."
            })
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        vbox.append(&note);

        // Wiring
        let me = self.clone();
        settings.connect_clicked(move |_| me.nav.push_by_tag("link"));
        let me = self.clone();
        copy.connect_clicked(move |b| me.copy_current_link(b));

        let me = self.clone();
        let send2 = send.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let sugg = suggestions.clone();
        entry.connect_changed(move |e| {
            let text = e.text().to_string();
            if text.ends_with(',') || text.ends_with(';') || text.ends_with(' ') {
                me.add_recipient(text.trim_end_matches([',', ';', ' ']), e, &send2);
                return;
            }
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let (me2, sugg2, e2, send3, pending2) =
                (me.clone(), sugg.clone(), e.clone(), send2.clone(), pending.clone());
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                pending2.borrow_mut().take();
                me2.suggest(&text, &sugg2, &e2, &send3);
            });
            *pending.borrow_mut() = Some(id);
        });
        let me = self.clone();
        let send2 = send.clone();
        entry.connect_entry_activated(move |e| {
            let t = e.text().to_string();
            me.add_recipient(&t, e, &send2);
        });

        let me = self.clone();
        send.connect_clicked(move |b| {
            let recipients = me.state.borrow().recipients.clone();
            let buf = message.buffer();
            let msg = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
            let opts = serde_json::json!({
                "recipients": recipients,
                "role": if role.selected() == 0 { "edit" } else { "view" },
                "message": msg,
                "notify": notify.is_active(),
            })
            .to_string();
            let (acct, path) = (me.account(), me.path());
            b.set_sensitive(false);
            let (me2, b2, buf2) = (me.clone(), b.clone(), buf.clone());
            client::call(
                async move { client::account(&acct).await?.invite(&path, &opts).await },
                move |r: zbus::Result<String>| match r
                    .map(|j| serde_json::from_str::<InviteOutcome>(&j).unwrap_or_default())
                {
                    Ok(out) => {
                        if out.failed.is_empty() {
                            me2.toast(&format!(
                                "Shared with {} {}",
                                out.granted,
                                if out.granted == 1 { "person" } else { "people" }
                            ));
                        } else {
                            let who: Vec<String> =
                                out.failed.iter().map(|f| format!("{}: {}", f.email, f.message)).collect();
                            util::error_dialog(&me2.dialog, "Some people couldn't be added", &who.join("\n"));
                        }
                        me2.state.borrow_mut().recipients.clear();
                        me2.render_chips(&b2);
                        buf2.set_text("");
                    }
                    Err(e) => {
                        b2.set_sensitive(true);
                        util::error_dialog(&me2.dialog, "Couldn't share", &util::friendly(&e));
                    }
                },
            );
        });

        let manage = gtk::Button::builder().icon_name("system-users-symbolic").tooltip_text("Manage access").build();
        let me = self.clone();
        manage.connect_clicked(move |_| {
            me.nav.push_by_tag("manage");
            me.load_access();
        });
        page_widget(
            &format!("Share \u{201c}{}\u{201d}", self.state.borrow().info.name),
            "share",
            &vbox,
            Some(manage.upcast_ref()),
        )
    }

    fn add_recipient(&self, text: &str, entry: &adw::EntryRow, send: &gtk::Button) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        if !t.contains('@') || t.contains(' ') {
            self.toast("Enter an email address");
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            if !st.recipients.iter().any(|r| r.eq_ignore_ascii_case(t)) {
                st.recipients.push(t.to_string());
            }
        }
        entry.set_text("");
        self.render_chips(send);
    }

    fn render_chips(&self, send: &gtk::Button) {
        while let Some(c) = self.chips.first_child() {
            self.chips.remove(&c);
        }
        let list = self.state.borrow().recipients.clone();
        for r in &list {
            let b = gtk::Button::builder().css_classes(["pill", "small"]).halign(gtk::Align::Start).build();
            let hb = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            hb.append(&gtk::Label::new(Some(r)));
            hb.append(&gtk::Image::from_icon_name("window-close-symbolic"));
            b.set_child(Some(&hb));
            b.set_tooltip_text(Some("Remove"));
            let (me, r2, send2) = (self.clone(), r.clone(), send.clone());
            b.connect_clicked(move |_| {
                me.state.borrow_mut().recipients.retain(|x| x != &r2);
                me.render_chips(&send2);
            });
            self.chips.append(&b);
        }
        self.chips.set_visible(!list.is_empty());
        send.set_sensitive(!list.is_empty());
    }

    fn suggest(&self, text: &str, list: &gtk::ListBox, entry: &adw::EntryRow, send: &gtk::Button) {
        while let Some(c) = list.first_child() {
            list.remove(&c);
        }
        if text.trim().is_empty() {
            list.set_visible(false);
            return;
        }
        let (acct, q) = (self.account(), text.to_string());
        let (me, list2, entry2, send2) = (self.clone(), list.clone(), entry.clone(), send.clone());
        client::call(
            async move { client::account(&acct).await?.search_people(&q).await },
            move |r: zbus::Result<String>| {
                let people: Vec<Person> = r.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
                while let Some(c) = list2.first_child() {
                    list2.remove(&c);
                }
                for p in people.iter().take(5) {
                    let row = adw::ActionRow::builder()
                        .title(if p.name.is_empty() { &p.email } else { &p.name })
                        .subtitle(if p.name.is_empty() { "" } else { &p.email })
                        .use_markup(false)
                        .activatable(true)
                        .build();
                    row.add_prefix(&adw::Avatar::new(
                        28,
                        Some(if p.name.is_empty() { &p.email } else { &p.name }),
                        true,
                    ));
                    let (me3, email, e3, s3, l3) =
                        (me.clone(), p.email.clone(), entry2.clone(), send2.clone(), list2.clone());
                    row.connect_activated(move |_| {
                        me3.add_recipient(&email, &e3, &s3);
                        l3.set_visible(false);
                    });
                    list2.append(&row);
                }
                list2.set_visible(!people.is_empty());
            },
        );
    }

    fn enable_suggestions(&self, button: &gtk::Button) {
        if !util::web_view_available() {
            self.toast("People suggestions need the built-in sign-in page, which can't start on this system");
            return;
        }
        let acct = self.account();
        let (me, b) = (self.clone(), button.clone());
        client::call(
            async move { client::account(&acct).await?.begin_people_consent().await },
            move |r: zbus::Result<(String, String)>| match r {
                Ok((url, redirect)) => {
                    let (me2, b2) = (me.clone(), b.clone());
                    auth_window(&me.dialog, &url, &redirect, move |redirect_url| {
                        let (me2, b2) = (me2.clone(), b2.clone());
                        client::call(
                            async move { client::manager().await?.complete_sign_in(&redirect_url).await },
                            move |r: zbus::Result<zbus::zvariant::OwnedObjectPath>| match r {
                                Ok(_) => {
                                    me2.state.borrow_mut().info.people_suggestions = true;
                                    b2.set_visible(false);
                                    me2.toast("People suggestions are on");
                                }
                                Err(e) => me2.toast(&util::friendly(&e)),
                            },
                        );
                    });
                }
                Err(e) => me.toast(&util::friendly(&e)),
            },
        );
    }

    fn update_link_row(&self) {
        let st = self.state.borrow();
        self.link_row.set_title(&describe(&st.link.audience, &st.link.role, &st.info.organization));
        let mut extra = Vec::new();
        if st.link.expires > 0 {
            extra.push(expiry_text(st.link.expires));
        }
        if !st.link.password.is_empty() {
            extra.push("Password".into());
        }
        if st.link.audience == "users" {
            extra.push(st.link.recipients.join(", "));
        }
        self.link_row.set_subtitle(&extra.join(" · "));
    }

    fn copy_current_link(&self, button: &gtk::Button) {
        self.copy_link_with(button, false);
    }

    fn copy_link_with(&self, button: &gtk::Button, replace: bool) {
        let (acct, path) = (self.account(), self.path());
        let mut link = self.state.borrow().link.clone();
        link.replace = replace;
        let opts = serde_json::to_string(&link).unwrap_or_default();
        button.set_sensitive(false);
        let (me, b) = (self.clone(), button.clone());
        client::call(
            async move { client::account(&acct).await?.create_link(&path, &opts).await },
            move |r: zbus::Result<String>| {
                b.set_sensitive(true);
                match r.map(|j| serde_json::from_str::<LinkResult>(&j).unwrap_or_default()) {
                    Ok(res) if res.existing => me.ask_replace(&b, res),
                    Ok(res) if !res.url.is_empty() => {
                        util::copy_to_clipboard(&b, &res.url);
                        me.toast(if res.settings_applied {
                            "Link copied"
                        } else {
                            "Link copied (some settings couldn't be applied)"
                        });
                        if !res.message.is_empty() {
                            me.toast(&res.message);
                        }
                    }
                    Ok(_) => me.toast("No link was returned"),
                    Err(e) => util::error_dialog(&me.dialog, "Couldn't create the link", &util::friendly(&e)),
                }
            },
        );
    }

    /// OneDrive keeps one link of each kind; one with other settings exists.
    fn ask_replace(&self, button: &gtk::Button, existing: LinkResult) {
        let st = self.state.borrow();
        let mut has = Vec::new();
        has.push(if existing.expires > 0 {
            format!("an expiration date of {}", date_text(existing.expires))
        } else {
            "no expiration date".into()
        });
        has.push(if existing.has_password { "a password".into() } else { "no password".into() });
        let body = format!(
            "\u{201c}{}\u{201d} already has a link like this, with {}. Replacing it creates a new link with your \
             settings, and people who have the current link will lose access.",
            st.info.name,
            has.join(" and ")
        );
        drop(st);
        let alert = adw::AlertDialog::new(Some("Replace the existing link?"), Some(&body));
        alert.add_responses(&[("cancel", "Cancel"), ("existing", "Copy Existing Link"), ("replace", "Replace Link")]);
        alert.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("existing"));
        alert.set_close_response("cancel");
        let (me, b) = (self.clone(), button.clone());
        alert.connect_response(None, move |_, r| match r {
            "existing" => {
                util::copy_to_clipboard(&b, &existing.url);
                me.toast("Link copied");
            }
            "replace" => me.copy_link_with(&b, true),
            _ => {}
        });
        alert.present(Some(button));
    }

    // ------------------------------------------------------------ page 2: Link settings

    fn link_settings_page(&self, business: bool) -> adw::NavigationPage {
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let org = self.state.borrow().info.organization.clone();
        let who = adw::PreferencesGroup::builder().title("Who would you like this link to work for?").build();
        let mut options: Vec<(&str, String, &str)> =
            vec![("anyone", "Anyone".into(), "Anyone with the link, inside or outside your organization")];
        if business {
            options.push((
                "organization",
                format!("People in {}", if org.is_empty() { "your organization" } else { &org }),
                "Only people in your organization who have the link",
            ));
            options.push(("existing", "People with existing access".into(), "Doesn't change who has access"));
        }
        options.push(("users", "Specific people".into(), "Only the people you choose"));
        let current = self.state.borrow().link.clone();
        let mut group_btn: Option<gtk::CheckButton> = None;
        let radios: Rc<RefCell<Vec<(String, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));
        for (key, title, sub) in &options {
            let check = gtk::CheckButton::new();
            if let Some(g) = &group_btn {
                check.set_group(Some(g));
            } else {
                group_btn = Some(check.clone());
            }
            check.set_active(current.audience == *key);
            let row = adw::ActionRow::builder().title(title).subtitle(*sub).activatable_widget(&check).build();
            row.add_prefix(&check);
            who.add(&row);
            radios.borrow_mut().push((key.to_string(), check));
        }
        let people = adw::EntryRow::builder()
            .title("Email addresses, separated by commas")
            .text(current.recipients.join(", "))
            .build();
        people.set_visible(current.audience == "users");
        who.add(&people);
        for (key, check) in radios.borrow().iter() {
            let (k, p) = (key.clone(), people.clone());
            check.connect_toggled(move |c| {
                if c.is_active() {
                    p.set_visible(k == "users");
                }
            });
        }
        vbox.append(&who);

        let other = adw::PreferencesGroup::builder().title("Other settings").build();
        let mut roles: Vec<(&str, &str)> = vec![("edit", "Can edit"), ("view", "Can view")];
        if business {
            roles.push(("review", "Can review"));
            roles.push(("blocksDownload", "Can view, can't download"));
        }
        let role = adw::ComboRow::builder()
            .title("Permission")
            .model(&gtk::StringList::new(&roles.iter().map(|r| r.1).collect::<Vec<_>>()))
            .selected(roles.iter().position(|r| r.0 == current.role).unwrap_or(0) as u32)
            .build();
        other.add(&role);
        let expiry = adw::ExpanderRow::builder()
            .title("Set expiration date")
            .show_enable_switch(true)
            .enable_expansion(current.expires > 0)
            .build();
        let days = adw::SpinRow::with_range(1.0, 730.0, 1.0);
        days.set_title("Days until the link stops working");
        days.set_value(if current.expires > 0 {
            ((current.expires - chrono::Utc::now().timestamp()) / 86_400).max(1) as f64
        } else {
            7.0
        });
        expiry.add_row(&days);
        other.add(&expiry);
        let password = adw::ExpanderRow::builder()
            .title("Set password")
            .show_enable_switch(true)
            .enable_expansion(!current.password.is_empty())
            .build();
        let pw = adw::PasswordEntryRow::builder().title("Password").text(current.password.clone()).build();
        password.add_row(&pw);
        other.add(&password);
        if !business {
            other.set_description(Some("Expiration dates and passwords need a Microsoft 365 subscription."));
        }
        vbox.append(&other);

        let apply = gtk::Button::builder()
            .label("Apply")
            .halign(gtk::Align::End)
            .css_classes(["suggested-action", "pill"])
            .build();
        vbox.append(&apply);
        let me = self.clone();
        apply.connect_clicked(move |_| {
            let audience = radios
                .borrow()
                .iter()
                .find(|(_, c)| c.is_active())
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| "anyone".into());
            let recipients: Vec<String> =
                people.text().split([',', ';']).map(|s| s.trim().to_string()).filter(|s| s.contains('@')).collect();
            if audience == "users" && recipients.is_empty() {
                me.toast("Add at least one email address");
                return;
            }
            {
                let mut st = me.state.borrow_mut();
                st.link.audience = audience;
                st.link.role = roles.get(role.selected() as usize).map(|r| r.0).unwrap_or("view").to_string();
                st.link.expires = if expiry.enables_expansion() {
                    chrono::Utc::now().timestamp() + days.value() as i64 * 86_400
                } else {
                    0
                };
                st.link.password = if password.enables_expansion() { pw.text().to_string() } else { String::new() };
                st.link.recipients = recipients;
            }
            me.update_link_row();
            me.nav.pop();
        });
        page_widget("Link Settings", "link", &vbox, None)
    }

    // ------------------------------------------------------------ page 3: Manage access

    fn manage_page(&self) -> adw::NavigationPage {
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 18);
        vbox.append(&self.access_list);
        let stop = gtk::Button::builder()
            .label("Stop Sharing")
            .halign(gtk::Align::Center)
            .css_classes(["destructive-action", "pill"])
            .build();
        vbox.append(&stop);
        let me = self.clone();
        stop.connect_clicked(move |b| {
            let name = me.state.borrow().info.name.clone();
            let alert = adw::AlertDialog::new(
                Some(&format!("Stop sharing \u{201c}{name}\u{201d}?")),
                Some("Everyone except you will lose access, and links to it will stop working."),
            );
            alert.add_responses(&[("cancel", "Cancel"), ("stop", "Stop Sharing")]);
            alert.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
            alert.set_default_response(Some("cancel"));
            let me2 = me.clone();
            alert.connect_response(None, move |_, r| {
                if r != "stop" {
                    return;
                }
                let (acct, path, me3) = (me2.account(), me2.path(), me2.clone());
                client::call(
                    async move { client::account(&acct).await?.stop_sharing(&path).await },
                    move |r: zbus::Result<String>| match r
                        .map(|j| serde_json::from_str::<StopSharingOutcome>(&j).unwrap_or_default())
                    {
                        Ok(out) if out.failed.is_empty() => {
                            me3.toast("Stopped sharing");
                            me3.load_access();
                        }
                        Ok(out) => {
                            util::error_dialog(&me3.dialog, "Some access couldn't be removed", &out.failed.join("\n"));
                            me3.load_access();
                        }
                        Err(e) => util::error_dialog(&me3.dialog, "Couldn't stop sharing", &util::friendly(&e)),
                    },
                );
            });
            alert.present(Some(b));
        });
        page_widget("Manage Access", "manage", &vbox, None)
    }

    fn load_access(&self) {
        while let Some(c) = self.access_list.first_child() {
            self.access_list.remove(&c);
        }
        let spinner = gtk::Spinner::builder().spinning(true).height_request(32).margin_top(24).build();
        self.access_list.append(&spinner);
        let (acct, path, me) = (self.account(), self.path(), self.clone());
        client::call(
            async move { client::account(&acct).await?.list_access(&path).await },
            move |r: zbus::Result<String>| {
                while let Some(c) = me.access_list.first_child() {
                    me.access_list.remove(&c);
                }
                match r.map(|j| serde_json::from_str::<Vec<AccessEntry>>(&j).unwrap_or_default()) {
                    Ok(list) => me.render_access(list),
                    Err(e) => me.access_list.append(
                        &adw::StatusPage::builder()
                            .icon_name("dialog-error-symbolic")
                            .title("Can't show who has access")
                            .description(util::friendly(&e))
                            .build(),
                    ),
                }
            },
        );
    }

    fn render_access(&self, list: Vec<AccessEntry>) {
        let org = self.state.borrow().info.organization.clone();
        let links = adw::PreferencesGroup::builder().title("Links giving access").build();
        let people = adw::PreferencesGroup::builder().title("People with access").build();
        let (mut n_links, mut n_people) = (0, 0);
        for e in list {
            let mut sub: Vec<String> = Vec::new();
            if e.expires > 0 {
                sub.push(expiry_text(e.expires));
            }
            if e.has_password {
                sub.push("Password".into());
            }
            if !e.inherited_from.is_empty() {
                sub.push(format!("Inherited from {}", e.inherited_from));
            }
            match e.kind.as_str() {
                "link" => {
                    n_links += 1;
                    if e.audience == "users" {
                        sub.insert(
                            0,
                            e.people
                                .iter()
                                .map(|p| if p.name.is_empty() { p.email.clone() } else { p.name.clone() })
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                    let row = adw::ActionRow::builder()
                        .title(describe(&e.audience, &e.role, &org))
                        .subtitle(sub.join(" · "))
                        .use_markup(false)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("insert-link-symbolic"));
                    if !e.url.is_empty() {
                        let copy = gtk::Button::builder()
                            .icon_name("edit-copy-symbolic")
                            .tooltip_text("Copy link")
                            .valign(gtk::Align::Center)
                            .css_classes(["flat"])
                            .build();
                        let (url, me) = (e.url.clone(), self.clone());
                        copy.connect_clicked(move |b| {
                            util::copy_to_clipboard(b, &url);
                            me.toast("Link copied");
                        });
                        row.add_suffix(&copy);
                    }
                    if e.removable {
                        row.add_suffix(&self.remove_button(
                            &e.id,
                            "Remove link",
                            "Anyone using this link will lose access.",
                        ));
                    }
                    links.add(&row);
                }
                "person" | "owner" => {
                    n_people += 1;
                    let p = e.people.first().cloned().unwrap_or_default();
                    let display = if p.name.is_empty() { p.email.clone() } else { p.name.clone() };
                    if !p.email.is_empty() && p.email != display {
                        sub.insert(0, p.email.clone());
                    }
                    let row =
                        adw::ActionRow::builder().title(&display).subtitle(sub.join(" · ")).use_markup(false).build();
                    row.add_prefix(&adw::Avatar::new(32, Some(&display), true));
                    if e.kind == "owner" {
                        row.add_suffix(&gtk::Label::builder().label("Owner").css_classes(["dim-label"]).build());
                    } else {
                        if e.role_editable {
                            let dd = gtk::DropDown::from_strings(&["Can edit", "Can view"]);
                            dd.set_selected(if e.role == "edit" { 0 } else { 1 });
                            dd.set_valign(gtk::Align::Center);
                            let (id, me) = (e.id.clone(), self.clone());
                            dd.connect_selected_notify(move |d| {
                                let role = if d.selected() == 0 { "edit" } else { "view" };
                                let (acct, path, id, me2) = (me.account(), me.path(), id.clone(), me.clone());
                                client::call(
                                    async move { client::account(&acct).await?.set_access_role(&path, &id, role).await },
                                    move |r: zbus::Result<()>| match r {
                                        Ok(()) => me2.toast("Permission changed"),
                                        Err(e) => me2.toast(&util::friendly(&e)),
                                    },
                                );
                            });
                            row.add_suffix(&dd);
                        } else {
                            row.add_suffix(
                                &gtk::Label::builder()
                                    .label(if e.role == "edit" { "Can edit" } else { "Can view" })
                                    .css_classes(["dim-label"])
                                    .build(),
                            );
                        }
                        if e.removable {
                            row.add_suffix(&self.remove_button(
                                &e.id,
                                "Remove access",
                                &format!("{display} will no longer have access."),
                            ));
                        }
                    }
                    people.add(&row);
                }
                _ => {}
            }
        }
        if n_links == 0 {
            links.add(
                &adw::ActionRow::builder()
                    .title("No links")
                    .subtitle("Create one from \u{201c}Copy link\u{201d}.")
                    .build(),
            );
        }
        if n_people == 0 {
            people.add(&adw::ActionRow::builder().title("Only you").build());
        }
        self.access_list.append(&links);
        self.access_list.append(&people);
    }

    fn remove_button(&self, id: &str, heading: &str, body: &str) -> gtk::Button {
        let b = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(heading)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let (id, me, heading, body) = (id.to_string(), self.clone(), heading.to_string(), body.to_string());
        b.connect_clicked(move |btn| {
            let alert = adw::AlertDialog::new(Some(&format!("{heading}?")), Some(&body));
            alert.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
            alert.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            let (id, me2) = (id.clone(), me.clone());
            alert.connect_response(None, move |_, r| {
                if r != "remove" {
                    return;
                }
                let (acct, path, id, me3) = (me2.account(), me2.path(), id.clone(), me2.clone());
                client::call(
                    async move { client::account(&acct).await?.remove_access(&path, &id).await },
                    move |r: zbus::Result<()>| match r {
                        Ok(()) => {
                            me3.toast("Access removed");
                            me3.load_access();
                        }
                        Err(e) => util::error_dialog(&me3.dialog, "Couldn't remove access", &util::friendly(&e)),
                    },
                );
            });
            alert.present(Some(btn));
        });
        b
    }
}

/// A small window with Microsoft's sign-in page (used for granting extra
/// permissions); calls `on_redirect` with the final redirect URL.
pub fn auth_window(parent: &impl IsA<gtk::Widget>, url: &str, redirect: &str, on_redirect: impl Fn(String) + 'static) {
    let window =
        adw::Window::builder().title("Allow access").default_width(480).default_height(640).modal(true).build();
    if let Some(root) = parent.as_ref().root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&root));
    }
    let data = od_core::paths::data_dir().join("webkit");
    let cache = od_core::paths::cache_dir().join("webkit");
    let session = webkit::NetworkSession::new(data.to_str(), cache.to_str());
    let view = webkit::WebView::builder().network_session(&session).vexpand(true).build();
    let redirect = redirect.to_string();
    let on_redirect = Rc::new(on_redirect);
    let w = window.clone();
    view.connect_decide_policy(move |_, decision, kind| {
        if kind != webkit::PolicyDecisionType::NavigationAction {
            return false;
        }
        let uri = decision
            .downcast_ref::<webkit::NavigationPolicyDecision>()
            .and_then(|d| d.navigation_action())
            .and_then(|a| a.request())
            .and_then(|r| r.uri())
            .map(|u| u.to_string())
            .unwrap_or_default();
        if !redirect.is_empty() && uri.starts_with(&redirect) {
            decision.ignore();
            w.close();
            on_redirect(uri);
            return true;
        }
        false
    });
    view.load_uri(url);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(&view));
    window.set_content(Some(&tv));
    window.present();
}
