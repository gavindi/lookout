//! The message list's sort application and list style (OUTLOOK_PARITY.md
//! M-07 / UX-27).
//!
//! One callback is the single way the sort key and direction change - the
//! list header's menu, the View ribbon's Arrange By / Reverse Sort, and the
//! single-line column headers all call it - so the four widgets can never
//! disagree about what the list is sorted by. It persists to GSettings,
//! updates the `win.sort-key` action and the direction toggle, refreshes the
//! column carets, and re-sorts the list.
//!
//! The module also owns the decision of which row layout to show: Outlook's
//! single-line columnar list when the reading pane is at the bottom (or off)
//! or the list pane is wide enough, and the existing three-line row otherwise.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::message_list::{MessageListModel, SortKey};
use crate::window::columns::{refresh_header, ColumnHeader};
use crate::window::search::resort_message_list;
use crate::window::state::UiState;
use crate::window::support::{sort_direction_icon, sort_direction_tooltip};

/// The one way the sort key/direction change: `(key, descending, persist)`.
pub(crate) type SortApplier = Rc<dyn Fn(SortKey, bool, bool)>;

/// Where the applier finds the column header once it exists (weak: the header
/// owns the applier, so a strong reference here would cycle).
pub(crate) type HeaderCaret = Rc<RefCell<Weak<ColumnHeader>>>;

/// The width at which the automatic layout switches to Outlook's single-line
/// columns. Deliberately derived from the column model
/// (`message_columns::preferred_single_line_width`) rather than a second
/// hand-maintained number: the threshold is always "the default columns fit".
pub(crate) fn automatic_single_line_width() -> i32 {
    crate::message_columns::preferred_single_line_width()
}

/// Which row layout the list uses (View ▸ Messages ▸ Message layout).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListStyle {
    /// Outlook's rule: single-line when the reading pane is bottom/off or the
    /// list is wide enough, three-line otherwise.
    Auto,
    ThreeLine,
    SingleLine,
}

impl ListStyle {
    pub(crate) const ALL: [ListStyle; 3] = [ListStyle::Auto, ListStyle::ThreeLine, ListStyle::SingleLine];

    /// Resolves the stored `mail-list-style` value; an unknown value lands on
    /// `Auto`, like every other stored-choice helper in `settings.rs`.
    pub(crate) fn from_setting(value: &str) -> Self {
        match value {
            "three-line" => ListStyle::ThreeLine,
            "single-line" => ListStyle::SingleLine,
            _ => ListStyle::Auto,
        }
    }

    /// The value written to `mail-list-style` and used by the action state.
    pub(crate) fn code(self) -> &'static str {
        match self {
            ListStyle::Auto => "auto",
            ListStyle::ThreeLine => "three-line",
            ListStyle::SingleLine => "single-line",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ListStyle::Auto => "Automatic",
            ListStyle::ThreeLine => "Three-line",
            ListStyle::SingleLine => "Single-line",
        }
    }
}

/// The message list's current width, read from the split it lives in. In the
/// horizontal (reading pane right) layout the paned's `position` is the start
/// child's width, i.e. the list pane; in the vertical layout the full paned
/// width is used, though the automatic rule already forces single-line there.
/// `GtkPaned:position` is a real notifying property, unlike `GtkWidget:width`.
pub(crate) fn list_pane_width(paned: &gtk::Paned) -> i32 {
    if paned.orientation() == gtk::Orientation::Horizontal {
        // With the end child hidden (Reading pane Off), the start child takes
        // the whole allocation, so the remembered position is not its width.
        let end_visible = paned.end_child().is_some_and(|child| child.is_visible());
        if end_visible {
            paned.position().max(0)
        } else {
            paned.width().max(0)
        }
    } else {
        paned.width().max(0)
    }
}

/// Whether `style` puts the list in the single-line columnar layout.
/// `reading_pane_position` is the stored "right" / "bottom" / "off" choice;
/// "off" is the reading pane being hidden. A forced style ignores both.
pub(crate) fn single_line_for(style: ListStyle, reading_pane_position: &str, width: i32) -> bool {
    match style {
        ListStyle::SingleLine => true,
        ListStyle::ThreeLine => false,
        ListStyle::Auto => match reading_pane_position {
            "off" | "bottom" => true,
            _ => width >= automatic_single_line_width(),
        },
    }
}

/// Builds the shared sort applier. The returned weak-header cell is filled
/// with the header once it is built (the header is created after the sort
/// menu it has to refresh).
pub(crate) fn sort_applier(
    state: &Rc<RefCell<UiState>>,
    message_list: &MessageListModel,
    key_action: &gio::SimpleAction,
    key_button: &gtk::MenuButton,
    direction_button: &gtk::ToggleButton,
) -> (SortApplier, HeaderCaret) {
    let header: HeaderCaret = Rc::new(RefCell::new(Weak::new()));
    // Weak handles for the two widgets whose own handlers call back into this
    // closure: a strong capture of either would close a reference cycle
    // (widget -> handler -> applier -> widget).
    let action_weak: glib::WeakRef<gio::SimpleAction> = glib::WeakRef::new();
    action_weak.set(Some(key_action));
    let direction_weak: glib::WeakRef<gtk::ToggleButton> = glib::WeakRef::new();
    direction_weak.set(Some(direction_button));

    let state = state.clone();
    let message_list = message_list.clone();
    let key_button = key_button.clone();
    let header_for_apply = header.clone();
    let apply: SortApplier = Rc::new(move |key, descending, persist| {
        {
            let mut st = state.borrow_mut();
            st.sort_key = key;
            st.sort_descending = descending;
        }
        if persist {
            let store = state.borrow().settings.clone();
            store.set_string(crate::settings::SORT_KEY, key.action_state());
            store.set_bool(crate::settings::SORT_DESCENDING, descending);
        }
        if let Some(action) = action_weak.upgrade() {
            action.set_state(&key.action_state().to_variant());
        }
        key_button.set_label(key.label());
        if let Some(button) = direction_weak.upgrade() {
            button.set_active(descending);
            button.set_icon_name(sort_direction_icon(descending));
            button.set_tooltip_text(Some(sort_direction_tooltip(key, descending)));
        }
        refresh_header(&header_for_apply.borrow(), key, descending);
        resort_message_list(&state, &message_list);
    });

    (apply, header)
}

/// Installs the applier on the direction toggle. The state check inside the
/// handler is what stops the applier's own `set_active` from looping.
pub(crate) fn connect_direction_toggle(direction_button: &gtk::ToggleButton, state: Rc<RefCell<UiState>>, apply: SortApplier) {
    direction_button.connect_toggled(move |button| {
        let descending = button.is_active();
        if state.borrow().sort_descending == descending {
            return;
        }
        let key = state.borrow().sort_key;
        apply(key, descending, true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bottom_or_hidden_reading_pane_always_gets_the_columnar_list() {
        assert!(single_line_for(ListStyle::Auto, "bottom", 0));
        assert!(single_line_for(ListStyle::Auto, "off", 0));
    }

    #[test]
    fn a_right_reading_pane_gets_it_only_when_the_list_is_wide() {
        let threshold = automatic_single_line_width();
        assert_eq!(threshold, crate::message_columns::row_width(&crate::message_columns::DEFAULT_COLUMNS));
        assert!(!single_line_for(ListStyle::Auto, "right", 0));
        assert!(!single_line_for(ListStyle::Auto, "right", threshold - 1));
        assert!(single_line_for(ListStyle::Auto, "right", threshold));
        assert!(single_line_for(ListStyle::Auto, "right", 1600));
    }

    #[test]
    fn an_unknown_position_behaves_like_right() {
        assert!(!single_line_for(ListStyle::Auto, "sideways", 400));
        assert!(single_line_for(ListStyle::Auto, "sideways", 900));
    }

    #[test]
    fn a_forced_style_ignores_the_pane_and_width() {
        assert!(single_line_for(ListStyle::SingleLine, "right", 0));
        assert!(!single_line_for(ListStyle::ThreeLine, "bottom", 1600));
        assert_eq!(ListStyle::from_setting("single-line"), ListStyle::SingleLine);
        assert_eq!(ListStyle::from_setting("three-line"), ListStyle::ThreeLine);
        assert_eq!(ListStyle::from_setting("auto"), ListStyle::Auto);
        assert_eq!(ListStyle::from_setting("nonsense"), ListStyle::Auto);
        for style in [ListStyle::Auto, ListStyle::ThreeLine, ListStyle::SingleLine] {
            assert_eq!(ListStyle::from_setting(style.code()), style);
            assert!(!style.label().is_empty());
        }
    }

    #[test]
    fn a_header_click_starts_in_outlooks_natural_direction() {
        // "Most first" for the ranked fields.
        assert!(SortKey::Date.natural_descending());
        assert!(SortKey::Size.natural_descending());
        assert!(SortKey::Importance.natural_descending());
        assert!(SortKey::Flag.natural_descending());
        assert!(SortKey::Attachments.natural_descending());
        // A-Z for the text fields (ascending).
        assert!(!SortKey::Sender.natural_descending());
        assert!(!SortKey::Subject.natural_descending());
        assert!(!SortKey::Category.natural_descending());
        assert!(!SortKey::To.natural_descending());
    }
}
