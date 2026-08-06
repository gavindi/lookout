use adw::prelude::*;
use lookout_core::EmailSummary;

pub const AVATAR_COLOR_CLASSES: [&str; 6] = ["avatar-color-0", "avatar-color-1", "avatar-color-2", "avatar-color-3", "avatar-color-4", "avatar-color-5"];

/// Reading-pane header: a full-bleed subject strip (`subject_bar`), the
/// sender/action block (`widget`) - avatar, `Name<address>`, a flat labelled
/// Reply/Reply All/Forward cluster (duplicating the top command toolbar's, by
/// design - see the plan this shipped under) plus disabled placeholders for
/// theme/contact/overflow, and a `To:`+date row - and a bottom action bar
/// (`action_bar`) with its own Reply/Forward repeated below the body. The
/// caller packs all three separately into the reading pane's stack page.
/// Content is set via `update()` from whatever `EmailSummary` is selected -
/// deliberately not from the (slower, async) `EmailBody` fetch, so the header
/// appears the instant a message is selected rather than waiting on the body
/// to load.
#[derive(Clone)]
pub struct MessageHeader {
    pub subject_bar: gtk::Box,
    pub widget: gtk::Box,
    pub action_bar: gtk::Box,
    subject_label: gtk::Label,
    avatar_label: gtk::Label,
    sender_name_label: gtk::Label,
    sender_email_label: gtk::Label,
    to_label: gtk::Label,
    date_label: gtk::Label,
    pub reply_button: gtk::Button,
    pub reply_all_button: gtk::Button,
    pub forward_button: gtk::Button,
    pub bottom_reply_button: gtk::Button,
    pub bottom_forward_button: gtk::Button,
}

/// A flat button with an icon + text label, via `adw::ButtonContent` - used
/// for the Reply/Reply All/Forward cluster so they read as labelled actions
/// rather than bare icon chrome.
fn action_button(icon: &str, label: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder().icon_name(icon).label(label).build();
    gtk::Button::builder().child(&content).css_classes(["flat"]).build()
}

/// A disabled placeholder button for a control that isn't wired up yet - same
/// convention as the top command toolbar's `more_button`. The tooltip names
/// what it will do so the disabled state reads as "not yet" rather than
/// "broken".
fn placeholder_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().icon_name(icon).css_classes(["flat"]).build();
    button.set_tooltip_text(Some(tooltip));
    button.set_sensitive(false);
    button
}

pub fn build() -> MessageHeader {
    let subject_label = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["message-header-subject"]).build();
    let subject_bar = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["message-subject-bar"]).build();
    subject_bar.set_widget_name("subject-bar");
    subject_bar.append(&subject_label);

    let avatar_label = gtk::Label::builder()
        .css_classes(["avatar-circle"])
        .width_request(40)
        .height_request(40)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();

    let sender_name_label = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let sender_email_label = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(["message-header-meta"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let sender_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    sender_box.append(&sender_name_label);
    sender_box.append(&sender_email_label);

    let reply_button = action_button("mail-reply-sender-symbolic", "Reply");
    reply_button.set_tooltip_text(Some("Reply"));
    let reply_all_button = action_button("mail-reply-all-symbolic", "Reply all");
    reply_all_button.set_tooltip_text(Some("Reply All"));
    let forward_button = action_button("mail-forward-symbolic", "Forward");
    forward_button.set_tooltip_text(Some("Forward"));

    let theme_button = placeholder_button(
        crate::window::themed_icon_name(&["weather-clear-symbolic", "display-brightness-symbolic", "night-light-symbolic"]),
        "Switch message theme (coming soon)",
    );
    let contact_button = placeholder_button(
        crate::window::themed_icon_name(&["avatar-default-symbolic", "contact-new-symbolic", "system-users-symbolic"]),
        "View contact (coming soon)",
    );
    let header_more_button = placeholder_button("view-more-symbolic", "More (coming soon)");

    let sender_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    sender_row.append(&avatar_label);
    sender_row.append(&sender_box);
    sender_row.append(&theme_button);
    sender_row.append(&reply_button);
    sender_row.append(&reply_all_button);
    sender_row.append(&forward_button);
    sender_row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    sender_row.append(&contact_button);
    sender_row.append(&header_more_button);

    let to_label = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["message-header-meta"]).build();
    let date_label = gtk::Label::builder().css_classes(["message-header-meta"]).valign(gtk::Align::Center).build();
    let meta_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    meta_row.append(&to_label);
    meta_row.append(&date_label);

    let widget = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(12)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    widget.append(&sender_row);
    widget.append(&meta_row);

    let bottom_reply_button = action_button("mail-reply-sender-symbolic", "Reply");
    bottom_reply_button.set_tooltip_text(Some("Reply"));
    let bottom_forward_button = action_button("mail-forward-symbolic", "Forward");
    bottom_forward_button.set_tooltip_text(Some("Forward"));

    let action_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Start)
        .css_classes(["message-action-bar"])
        .build();
    action_bar.append(&bottom_reply_button);
    action_bar.append(&bottom_forward_button);

    MessageHeader {
        subject_bar,
        widget,
        action_bar,
        subject_label,
        avatar_label,
        sender_name_label,
        sender_email_label,
        to_label,
        date_label,
        reply_button,
        reply_all_button,
        forward_button,
        bottom_reply_button,
        bottom_forward_button,
    }
}

impl MessageHeader {
    pub fn update(&self, summary: &EmailSummary) {
        self.subject_label.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));

        match summary.from.first() {
            Some(sender) => {
                self.sender_name_label.set_label(sender.display_label());
                let has_distinct_name = sender.name.as_deref().is_some_and(|n| !n.trim().is_empty());
                self.sender_email_label.set_label(&format!("<{}>", sender.address));
                self.sender_email_label.set_visible(has_distinct_name);

                self.avatar_label.set_label(&initials(sender.name.as_deref(), &sender.address));
                for class in AVATAR_COLOR_CLASSES {
                    self.avatar_label.remove_css_class(class);
                }
                self.avatar_label.add_css_class(avatar_color_class(&sender.address));
            }
            None => {
                self.sender_name_label.set_label("Unknown sender");
                self.sender_email_label.set_visible(false);
                self.avatar_label.set_label("?");
            }
        }

        let to = summary.to.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
        self.to_label.set_label(&format!("To: {to}"));

        self.date_label
            .set_label(&summary.date.with_timezone(&chrono::Local).format("%a %d/%m/%Y %I:%M %p").to_string());
    }
}

/// First+last initials for a multi-word name ("Microsoft Security" -> "MS"),
/// else the first two characters of whatever single word/address is given.
pub fn initials(name: Option<&str>, address: &str) -> String {
    let source = name.filter(|n| !n.trim().is_empty()).unwrap_or(address);
    let mut words = source.split_whitespace();
    match (words.next(), words.next()) {
        (Some(first), Some(last)) => {
            let a = first.chars().next().unwrap_or('?');
            let b = last.chars().next().unwrap_or('?');
            format!("{a}{b}").to_uppercase()
        }
        (Some(only), None) => only.chars().take(2).collect::<String>().to_uppercase(),
        _ => "?".to_string(),
    }
}

/// Picks a deterministic avatar background color class from `seed` (the
/// sender's address), so the same sender always gets the same color - a
/// fixed palette of CSS classes registered once (see `install_paned_css`)
/// rather than per-instance inline styling.
pub fn avatar_color_class(seed: &str) -> &'static str {
    let hash = seed.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    AVATAR_COLOR_CLASSES[(hash as usize) % AVATAR_COLOR_CLASSES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_uses_first_and_last_word_of_a_multi_word_name() {
        assert_eq!(initials(Some("Microsoft Security"), "msft@example.com"), "MS");
        assert_eq!(initials(Some("Ada Lovelace"), "ada@example.com"), "AL");
    }

    #[test]
    fn initials_falls_back_to_first_two_chars_without_a_multi_word_name() {
        assert_eq!(initials(None, "ada@example.com"), "AD");
        assert_eq!(initials(Some(""), "ada@example.com"), "AD");
        assert_eq!(initials(Some("Ada"), "ada@example.com"), "AD");
    }

    #[test]
    fn avatar_color_class_is_deterministic_per_seed() {
        let a = avatar_color_class("ada@example.com");
        let b = avatar_color_class("ada@example.com");
        assert_eq!(a, b);
        assert!(AVATAR_COLOR_CLASSES.contains(&a));
    }
}
