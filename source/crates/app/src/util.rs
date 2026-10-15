//! Small UI and text helpers shared across the app's views.
//!
//! Every helper here previously existed as a copy-pasted private function in
//! two or more view modules: HTML escaping for the WebView-rendered bodies
//! (compose, conversation, dashboard), and the recurring libadwaita widget
//! shapes (labelled rows, card sections, flat icon buttons, account labels).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use adw::prelude::*;

use lookout_core::AccountId;

/// Escapes text for HTML: the five entities that matter for element text and
/// double-quoted attributes alike.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a value for a double-quoted attribute: quotes and ampersands are
/// what matter, and angle brackets go too, so even a reader of the markup
/// can't mistake a body's `</iframe>` for structure.
pub fn escape_attribute(value: &str) -> String {
    escape_html(value)
}

/// Removes every child from a container (the common "repopulate this box"
/// prelude).
pub fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// A flat button with a themed icon and a label, as the reading pane's
/// header actions and the profile card use.
pub fn action_button(icon: &str, label: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder().icon_name(icon).label(label).build();
    gtk::Button::builder().child(&content).css_classes(["flat"]).build()
}

/// A caption-plus-widget form row (the settings-style dialog fields).
pub fn labeled_row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let label = gtk::Label::builder().label(label).width_chars(9).halign(gtk::Align::Start).build();
    widget.set_hexpand(true);
    row.append(&label);
    row.append(widget);
    row
}

/// Wraps content in a `.card`-styled box - libadwaita's built-in style class
/// for a rounded-corner, softly-shaded panel. No per-section header bar: the
/// enclosing `Gtk.Paned` (or dialog) provides the gap between cards, and the
/// small margin here is what makes each card's rounded corners visible
/// against the window background instead of touching the divider/edges.
pub fn card_section(content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    card.append(content);
    card
}

/// The best available human name for an account: its display name, else its
/// email, else the raw account id.
pub fn account_label(account: &AccountId, display_name: &str, email: &str) -> String {
    if !display_name.trim().is_empty() {
        display_name.to_string()
    } else if !email.trim().is_empty() {
        email.to_string()
    } else {
        account.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_covers_text_and_attribute_metacharacters() {
        assert_eq!(escape_html("a<b>&\"c\"'d'"), "a&lt;b&gt;&amp;&quot;c&quot;&#39;d&#39;");
        assert_eq!(escape_attribute("</iframe>"), "&lt;/iframe&gt;");
    }

    #[test]
    fn account_label_prefers_name_then_email_then_id() {
        let account = AccountId("acct-1".to_string());
        assert_eq!(account_label(&account, "Ada", "ada@example.com"), "Ada");
        assert_eq!(account_label(&account, "  ", "ada@example.com"), "ada@example.com");
        assert_eq!(account_label(&account, "", ""), "acct-1");
    }
}
