//! The composer's To/Cc/Bcc fields: each recipient is a discrete removable
//! chip rather than a run of comma-separated text, with completions offered
//! from the account's address book as you type.
//!
//! Built from stock widgets held in a plain struct rather than a `glib`
//! subclass, matching `MessageListModel` (see the note in `message_list.rs`):
//! the app crate has no GObject subclassing anywhere, and this needs none.
//! Chips are recreated from the canonical address list on every change, the
//! same repopulate-from-truth approach the message list uses.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use lookout_core::EmailAddress;

/// How many completions the popover offers at once. Enough to be useful,
/// short enough that the list never covers the body editor below it.
const MAX_SUGGESTIONS: usize = 8;

/// Splits a recipient field's text into individual address tokens.
///
/// Commas separate recipients, but not every comma is a separator: RFC 5322
/// display names are routinely `"Lovelace, Ada" <ada@example.com>`, and
/// splitting those naively produces two broken recipients. Commas inside
/// double quotes and inside angle brackets are therefore treated as part of
/// the token. Empty tokens are dropped, so trailing separators and repeated
/// commas are harmless.
pub fn parse_address_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angles = false;

    for ch in text.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '<' if !in_quotes => {
                in_angles = true;
                current.push(ch);
            }
            '>' if !in_quotes => {
                in_angles = false;
                current.push(ch);
            }
            ',' | ';' if !in_quotes && !in_angles => {
                let token = current.trim();
                if !token.is_empty() {
                    tokens.push(token.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let token = current.trim();
    if !token.is_empty() {
        tokens.push(token.to_string());
    }
    tokens
}

/// The bare addr-spec inside a token: `Ada <ada@example.com>` yields
/// `ada@example.com`, a bare address yields itself. Used for both validation
/// and what actually goes into the message's recipient list.
pub fn address_of(token: &str) -> &str {
    match (token.rfind('<'), token.rfind('>')) {
        (Some(open), Some(close)) if close > open + 1 => token[open + 1..close].trim(),
        _ => token.trim(),
    }
}

/// Whether a token carries something usable as an address. Deliberately
/// shallow (exactly one `@`, a non-empty local part, a dotted domain),
/// because this only drives a visual warning on the chip, never a refusal
/// to accept what was typed. The server is the real authority on whether an
/// address exists, and over-strict client validation that rejects a valid
/// but unusual address is worse than a chip that looks fine and bounces.
pub fn is_plausible_address(token: &str) -> bool {
    let address = address_of(token);
    let mut parts = address.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !domain.contains(' ')
}

/// Produces completions for a partially typed recipient. Supplied by the
/// window, which owns the per-account cache handle the addresses come from -
/// this module never touches storage itself.
pub type SuggestionSource = Rc<dyn Fn(&str) -> Vec<EmailAddress>>;

/// A recipient field. Cheap to clone; clones share the same chips and entry.
#[derive(Clone)]
pub struct RecipientEntry {
    root: gtk::Box,
    /// The title row: the field's caption label plus, on the To row, the
    /// Cc/Bcc reveal buttons appended via `add_header_suffix`.
    header: gtk::Box,
    /// Wraps chips onto further lines as they accumulate, with the text entry
    /// as the final child so typing always continues after the last chip.
    flow: gtk::FlowBox,
    entry: gtk::Text,
    /// The canonical contents - full tokens (`Ada <ada@example.com>` or a
    /// bare address), in the order the user entered them. The chip widgets
    /// are rebuilt from this, never read back out of.
    tokens: Rc<RefCell<Vec<String>>>,
    popover: gtk::Popover,
    suggestion_list: gtk::ListBox,
    suggestions: Rc<RefCell<Vec<EmailAddress>>>,
    source: Rc<RefCell<Option<SuggestionSource>>>,
}

impl RecipientEntry {
    pub fn new(title: &str) -> Self {
        let title_label = gtk::Label::builder().label(title).xalign(0.0).hexpand(true).css_classes(["caption", "dim-label"]).build();
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        header.append(&title_label);

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .row_spacing(4)
            .column_spacing(4)
            .min_children_per_line(1)
            .max_children_per_line(64)
            .homogeneous(false)
            .build();

        let entry = gtk::Text::builder().hexpand(true).width_request(120).build();
        flow.append(&entry);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["recipient-field"])
            .build();
        root.append(&header);
        root.append(&flow);

        let suggestion_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&suggestion_list)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .max_content_height(220)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let popover = gtk::Popover::builder()
            .child(&scroller)
            .position(gtk::PositionType::Bottom)
            // Autohide would take the keyboard focus off the entry the moment
            // the popover appeared, so the next keystroke would go nowhere.
            // Visibility is managed by hand instead.
            .autohide(false)
            .has_arrow(false)
            .build();
        popover.set_parent(&entry);

        let this = RecipientEntry {
            root,
            header,
            flow,
            entry,
            tokens: Rc::new(RefCell::new(Vec::new())),
            popover,
            suggestion_list,
            suggestions: Rc::new(RefCell::new(Vec::new())),
            source: Rc::new(RefCell::new(None)),
        };
        this.connect_handlers();
        this
    }

    /// The widget to pack into the composer.
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Appends a widget to the field's title row, right-aligned by the title
    /// label's `hexpand`. The composer uses this to put the Cc/Bcc reveal
    /// buttons on the To row.
    pub fn add_header_suffix(&self, widget: &impl IsA<gtk::Widget>) {
        self.header.append(widget);
    }

    /// Replaces the field's contents from a comma-separated string - how
    /// `ComposePrefill` carries Reply/Reply-All/Forward recipients.
    pub fn set_from_text(&self, text: &str) {
        *self.tokens.borrow_mut() = parse_address_tokens(text);
        self.rebuild_chips();
    }

    /// Every recipient, including anything still being typed but not yet
    /// committed to a chip - a Send click must not silently drop a
    /// half-entered address just because the user didn't press Enter.
    pub fn addresses(&self) -> Vec<String> {
        let mut out: Vec<String> = self.tokens.borrow().clone();
        let pending = self.entry.text().trim().to_string();
        if !pending.is_empty() {
            out.extend(parse_address_tokens(&pending));
        }
        out
    }

    /// The field as one comma-separated string, for the draft snapshot's
    /// change detection.
    pub fn text_value(&self) -> String {
        self.addresses().join(", ")
    }

    pub fn set_suggestion_source(&self, source: SuggestionSource) {
        *self.source.borrow_mut() = Some(source);
    }

    /// Commits whatever is typed, then reports the tokens. Called by Send so
    /// the field ends up visually consistent with what was actually sent.
    pub fn commit_pending(&self) {
        self.commit_entry_text();
    }

    fn connect_handlers(&self) {
        // Enter commits either the highlighted completion or the typed text.
        {
            let this = self.clone();
            self.entry.connect_activate(move |_| {
                if !this.take_selected_suggestion() {
                    this.commit_entry_text();
                }
            });
        }

        // Typing a separator commits the token before it, so pasting
        // "a@x.com, b@y.com" lands as two chips without any keypress.
        {
            let this = self.clone();
            self.entry.connect_changed(move |entry| {
                let text = entry.text().to_string();
                if text.contains(',') || text.contains(';') {
                    this.commit_entry_text();
                    return;
                }
                this.refresh_suggestions(&text);
            });
        }

        {
            let this = self.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, _| match key {
                gtk::gdk::Key::BackSpace if this.entry.text().is_empty() => {
                    this.tokens.borrow_mut().pop();
                    this.rebuild_chips();
                    glib::Propagation::Stop
                }
                // Tab would otherwise move focus with a token half-entered.
                gtk::gdk::Key::Tab if !this.entry.text().trim().is_empty() => {
                    this.commit_entry_text();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Down => {
                    this.move_suggestion_selection(1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Up => {
                    this.move_suggestion_selection(-1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Escape if this.popover.is_visible() => {
                    this.popover.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            });
            self.entry.add_controller(keys);
        }

        // Clicking a completion is equivalent to highlighting it and pressing
        // Enter.
        {
            let this = self.clone();
            self.suggestion_list.connect_row_activated(move |_, row| {
                this.accept_suggestion(row.index());
            });
        }

        // A field that loses focus mid-token keeps the text (it still counts
        // via `addresses()`), but the completion list must not linger over
        // whatever the user moved on to.
        {
            let popover = self.popover.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_leave(move |_| popover.popdown());
            self.entry.add_controller(focus);
        }
    }

    /// Turns the entry's current text into chips.
    fn commit_entry_text(&self) {
        let text = self.entry.text().to_string();
        let tokens = parse_address_tokens(&text);
        if tokens.is_empty() {
            // Nothing usable (empty, or just separators) - clear the leftovers
            // so a stray comma doesn't sit in the field forever.
            if !text.trim().is_empty() {
                self.entry.set_text("");
            }
            return;
        }
        self.tokens.borrow_mut().extend(tokens);
        self.entry.set_text("");
        self.popover.popdown();
        self.rebuild_chips();
    }

    /// Rebuilds every chip from `tokens`. Wholesale rather than incremental:
    /// the list is a handful of items, and rebuilding keeps the widgets and
    /// the canonical list impossible to desynchronise.
    fn rebuild_chips(&self) {
        // Drop every child but the one wrapping the text entry - it's the
        // only `gtk::Text` in the flow, which is what identifies it.
        let mut child = self.flow.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            let holds_entry = widget.downcast_ref::<gtk::FlowBoxChild>().and_then(|c| c.child()).is_some_and(|c| c.is::<gtk::Text>());
            if !holds_entry {
                self.flow.remove(&widget);
            }
            child = next;
        }
        // Everything except the entry is gone; re-insert chips ahead of it.
        for (index, token) in self.tokens.borrow().iter().enumerate() {
            let chip = self.build_chip(index, token);
            self.flow.insert(&chip, index as i32);
        }
    }

    fn build_chip(&self, index: usize, token: &str) -> gtk::Box {
        let label = gtk::Label::builder()
            .label(chip_label(token))
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(28)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "recipient-chip-remove"])
            .tooltip_text("Remove recipient")
            .build();
        {
            let this = self.clone();
            remove.connect_clicked(move |_| {
                // Read the index at click time from the position the chip was
                // built at: a rebuild recreates every chip, so a handler can
                // never outlive its own row.
                let mut tokens = this.tokens.borrow_mut();
                if index < tokens.len() {
                    tokens.remove(index);
                }
                drop(tokens);
                this.rebuild_chips();
            });
        }

        let chip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
        chip.add_css_class("recipient-chip");
        if !is_plausible_address(token) {
            // Flagged, not rejected - see `is_plausible_address`.
            chip.add_css_class("recipient-chip-invalid");
            chip.set_tooltip_text(Some("This doesn't look like an email address"));
        } else {
            chip.set_tooltip_text(Some(address_of(token)));
        }
        chip.append(&label);
        chip.append(&remove);
        chip
    }

    fn refresh_suggestions(&self, prefix: &str) {
        let prefix = prefix.trim();
        let source = self.source.borrow().clone();
        let Some(source) = source else {
            return;
        };
        // An empty field offering the whole address book would pop a list
        // over the composer the instant it opened.
        let matches = if prefix.is_empty() { Vec::new() } else { source(prefix) };

        // Anything already chipped is not a useful suggestion.
        let existing: Vec<String> = self.tokens.borrow().iter().map(|t| address_of(t).to_lowercase()).collect();
        let matches: Vec<EmailAddress> = matches
            .into_iter()
            .filter(|m| !existing.contains(&m.address.to_lowercase()))
            .take(MAX_SUGGESTIONS)
            .collect();

        while let Some(row) = self.suggestion_list.first_child() {
            self.suggestion_list.remove(&row);
        }
        if matches.is_empty() {
            self.popover.popdown();
            *self.suggestions.borrow_mut() = Vec::new();
            return;
        }
        for candidate in &matches {
            let row = gtk::Label::builder()
                .label(suggestion_label(candidate))
                .xalign(0.0)
                .margin_top(4)
                .margin_bottom(4)
                .margin_start(8)
                .margin_end(8)
                .build();
            self.suggestion_list.append(&row);
        }
        *self.suggestions.borrow_mut() = matches;
        self.suggestion_list.unselect_all();
        self.popover.popup();
    }

    fn move_suggestion_selection(&self, delta: i32) {
        if !self.popover.is_visible() {
            return;
        }
        let count = self.suggestions.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.suggestion_list.selected_row().map(|r| r.index()).unwrap_or(-1);
        let next = (current + delta).rem_euclid(count);
        if let Some(row) = self.suggestion_list.row_at_index(next) {
            self.suggestion_list.select_row(Some(&row));
        }
    }

    /// Commits the highlighted completion, if there is one. Returns whether
    /// it did, so Enter can fall through to committing the typed text.
    fn take_selected_suggestion(&self) -> bool {
        if !self.popover.is_visible() {
            return false;
        }
        let Some(index) = self.suggestion_list.selected_row().map(|r| r.index()) else {
            return false;
        };
        self.accept_suggestion(index)
    }

    fn accept_suggestion(&self, index: i32) -> bool {
        let Some(candidate) = self.suggestions.borrow().get(index.max(0) as usize).cloned() else {
            return false;
        };
        self.tokens.borrow_mut().push(token_for(&candidate));
        self.entry.set_text("");
        self.popover.popdown();
        self.rebuild_chips();
        true
    }
}

/// What a chip shows: the display name when there is one, since a column of
/// full `Name <address>` tokens is unreadable at chip width.
fn chip_label(token: &str) -> &str {
    let address = address_of(token);
    if address == token.trim() {
        return address;
    }
    let name = token[..token.rfind('<').unwrap_or(token.len())].trim().trim_matches('"').trim();
    if name.is_empty() {
        address
    } else {
        name
    }
}

fn suggestion_label(candidate: &EmailAddress) -> String {
    match &candidate.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", candidate.address),
        _ => candidate.address.clone(),
    }
}

fn token_for(candidate: &EmailAddress) -> String {
    match &candidate.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", candidate.address),
        _ => candidate.address.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_separators_outside_quotes_and_angles() {
        assert_eq!(parse_address_tokens("a@x.com, b@y.com"), vec!["a@x.com", "b@y.com"]);
        // A display name containing a comma is one recipient, not two - the
        // "Surname, Given" form is what makes naive splitting wrong.
        assert_eq!(
            parse_address_tokens("\"Lovelace, Ada\" <ada@x.com>, bob@y.com"),
            vec!["\"Lovelace, Ada\" <ada@x.com>", "bob@y.com"]
        );
        assert_eq!(parse_address_tokens("a@x.com;b@y.com"), vec!["a@x.com", "b@y.com"]);
        // Empty tokens are dropped, so trailing and doubled separators are
        // harmless rather than producing blank chips.
        assert_eq!(parse_address_tokens("a@x.com,,  , b@y.com,"), vec!["a@x.com", "b@y.com"]);
        assert!(parse_address_tokens("   ").is_empty());
    }

    #[test]
    fn extracts_the_addr_spec_from_a_token() {
        assert_eq!(address_of("ada@example.com"), "ada@example.com");
        assert_eq!(address_of("Ada Lovelace <ada@example.com>"), "ada@example.com");
        assert_eq!(address_of("  spaced@example.com  "), "spaced@example.com");
        // Malformed brackets fall back to the whole token rather than
        // producing an empty address.
        assert_eq!(address_of("<>"), "<>");
    }

    #[test]
    fn flags_implausible_addresses_only() {
        assert!(is_plausible_address("ada@example.com"));
        assert!(is_plausible_address("Ada Lovelace <ada@example.co.uk>"));
        assert!(!is_plausible_address("ada"));
        assert!(!is_plausible_address("ada@localhost"));
        assert!(!is_plausible_address("a@b@example.com"));
        assert!(!is_plausible_address("@example.com"));
        assert!(!is_plausible_address("ada@example.com extra@example.com"));
    }

    #[test]
    fn chip_shows_the_name_when_there_is_one() {
        assert_eq!(chip_label("Ada Lovelace <ada@example.com>"), "Ada Lovelace");
        assert_eq!(chip_label("ada@example.com"), "ada@example.com");
        assert_eq!(chip_label("<ada@example.com>"), "ada@example.com");
    }
}
