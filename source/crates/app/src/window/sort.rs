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

/// Below this list-pane width, the single-line columns cannot all fit and the
/// three-line row reads better - but a bottom/off reading pane always has the
/// full window width, so it always gets the single-line layout.
pub(crate) const SINGLE_LINE_MIN_WIDTH: i32 = 740;

/// Which row layout the list uses. `reading_pane_position` is the stored
/// "right" / "bottom" / "off" choice; "off" is the reading pane being hidden.
pub(crate) fn is_single_line(reading_pane_position: &str, width: i32) -> bool {
    match reading_pane_position {
        "off" | "bottom" => true,
        _ => width >= SINGLE_LINE_MIN_WIDTH,
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
        assert!(is_single_line("bottom", 0));
        assert!(is_single_line("off", 0));
    }

    #[test]
    fn a_right_reading_pane_gets_it_only_when_the_list_is_wide() {
        assert!(!is_single_line("right", 0));
        assert!(!is_single_line("right", SINGLE_LINE_MIN_WIDTH - 1));
        assert!(is_single_line("right", SINGLE_LINE_MIN_WIDTH));
        assert!(is_single_line("right", 1600));
    }

    #[test]
    fn an_unknown_position_behaves_like_right() {
        assert!(!is_single_line("sideways", 400));
        assert!(is_single_line("sideways", 900));
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
