//! The single-line message list's column header (OUTLOOK_PARITY.md M-07 /
//! UX-27).
//!
//! One flat button per column, in the configured order, with a caret on the
//! column the list is sorted by. Clicking a column sorts by it (Outlook's
//! natural direction first - newest, largest, high importance, flagged, with
//! attachments; A-Z for text - and the opposite direction on a second click).
//! Right-clicking opens Sort ascending / Sort descending, a check list of
//! columns, and Reset columns. The chosen set is stored in
//! `mail-list-columns`.
//!
//! The header is only visible in the single-line list style (the three-line
//! row has no columns to name). The widths here are the same constants the
//! rows use, so the labels sit exactly over their cells; both sides are bare
//! boxes rather than a `GtkColumnView`, because section headers have to span
//! the full width and a column view cannot do that.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk::glib;
use gtk::prelude::*;

use crate::context_menu::{ActionSpec, ContextMenu, Dispatch, MenuDescription};
use crate::message_columns::{self, MessageColumn, ROW_LEADING_WIDTH};
use crate::message_list::SortKey;
use crate::settings::SettingsStore;
use crate::window::support::themed_icon_name;

/// The list's column configuration, shared by the header and every row.
///
/// `configured` is what the user chose (Outlook's default until they change
/// it); `visible` is that set fitted to the list's current width. Rows read
/// `visible` at bind time, so a width or configuration change rebuilds the
/// header and refreshes the list.
#[derive(Clone)]
pub(crate) struct ColumnState {
    configured: Rc<RefCell<Vec<MessageColumn>>>,
    visible: Rc<RefCell<Vec<MessageColumn>>>,
    /// The width last fitted against, so a configuration change can re-fit
    /// without waiting for a resize.
    width: Rc<Cell<i32>>,
}

impl ColumnState {
    pub(crate) fn from_settings(settings: &SettingsStore) -> Self {
        let configured = message_columns::parse_columns(&settings.get_strv(crate::settings::MAIL_LIST_COLUMNS));
        ColumnState {
            visible: Rc::new(RefCell::new(configured.clone())),
            configured: Rc::new(RefCell::new(configured)),
            width: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn visible(&self) -> Vec<MessageColumn> {
        self.visible.borrow().clone()
    }

    pub(crate) fn is_visible(&self, column: MessageColumn) -> bool {
        self.visible.borrow().contains(&column)
    }

    /// Whether the user has the column chosen, regardless of whether the
    /// current width can fit it. The column chooser's checks follow this, so a
    /// column that is merely shed for space still reads as on.
    pub(crate) fn is_configured(&self, column: MessageColumn) -> bool {
        self.configured.borrow().contains(&column)
    }

    /// Fits the configured columns to `width`, returning whether the visible
    /// set changed. Called on every list-width notification; only a change in
    /// the band the columns fit into does any work.
    pub(crate) fn fit_width(&self, width: i32) -> bool {
        self.width.set(width);
        let fitted = message_columns::columns_for_width(&self.configured.borrow(), width);
        let mut visible = self.visible.borrow_mut();
        if *visible == fitted {
            return false;
        }
        *visible = fitted;
        true
    }

    /// Adds or removes one column, persists the new order, and re-fits to the
    /// current width. Returns whether the visible set changed.
    pub(crate) fn set_column_visible(&self, column: MessageColumn, visible: bool, settings: &SettingsStore) -> bool {
        let mut configured = self.configured.borrow_mut();
        let was_present = configured.contains(&column);
        if visible && !was_present {
            configured.push(column);
        } else if !visible && was_present {
            configured.retain(|candidate| *candidate != column);
        } else {
            return false;
        }
        // Keep the chooser's order stable: MessageColumn::ALL is the display
        // order, and Outlook's default is that order minus To.
        let all = MessageColumn::ALL;
        configured.sort_by_key(|candidate| all.iter().position(|value| value == candidate).unwrap_or(usize::MAX));
        settings.set_strv(crate::settings::MAIL_LIST_COLUMNS, message_columns::columns_to_ids(&configured));
        drop(configured);
        self.refit()
    }

    /// Restores Outlook's default columns and persists them.
    pub(crate) fn reset(&self, settings: &SettingsStore) -> bool {
        let mut configured = self.configured.borrow_mut();
        *configured = message_columns::DEFAULT_COLUMNS.to_vec();
        settings.set_strv(crate::settings::MAIL_LIST_COLUMNS, message_columns::columns_to_ids(&configured));
        drop(configured);
        self.refit()
    }

    fn refit(&self) -> bool {
        let fitted = message_columns::columns_for_width(&self.configured.borrow(), self.width.get());
        let mut visible = self.visible.borrow_mut();
        if *visible == fitted {
            return false;
        }
        *visible = fitted;
        true
    }
}

/// The widget pieces of the header, cloned into the button and menu handlers.
struct HeaderParts {
    cells: gtk::Box,
    buttons: Rc<RefCell<Vec<ColumnButton>>>,
    state: ColumnState,
    settings: Rc<SettingsStore>,
    sort: Rc<Cell<(SortKey, bool)>>,
    set_sort: Rc<dyn Fn(SortKey, bool, bool)>,
    on_columns_changed: Rc<dyn Fn()>,
}

struct ColumnButton {
    column: MessageColumn,
    button: gtk::Button,
    caret: gtk::Image,
}

/// Builds the header. The returned `Rc` must be kept alive for as long as the
/// window is open (the list-style controller holds it). Every handler below
/// captures a `Weak` back to it, so the widget tree and the header cannot
/// leak each other.
pub(crate) fn build_column_header(
    state: ColumnState,
    settings: Rc<SettingsStore>,
    set_sort: Rc<dyn Fn(SortKey, bool, bool)>,
    on_columns_changed: Rc<dyn Fn()>,
) -> Rc<ColumnHeader> {
    let root_build = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .css_classes(["message-column-header"])
        .build();
    // The header's fixed column widths must not become the message pane's
    // minimum: the scrollbar-less viewport clips instead of propagating them,
    // so Automatic can shrink the pane below the columns and fall back to the
    // three-line rows. The inner box stays visible; the viewport is toggled.
    let viewport = gtk::ScrolledWindow::builder()
        .child(&root_build)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(false)
        .propagate_natural_height(true)
        .has_frame(false)
        .hexpand(true)
        .visible(false)
        .build();
    viewport.add_css_class("message-column-header-viewport");
    Rc::new(ColumnHeader {
        parts: HeaderParts {
            cells: gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(crate::message_columns::COLUMN_SPACING)
                .hexpand(true)
                .margin_end(crate::message_columns::ROW_TRAILING_MARGIN)
                .build(),
            buttons: Rc::new(RefCell::new(Vec::new())),
            state,
            settings,
            sort: Rc::new(Cell::new((SortKey::Date, true))),
            set_sort,
            on_columns_changed,
        },
        root_build,
        viewport,
    })
}

/// The header row widget.
pub(crate) struct ColumnHeader {
    parts: HeaderParts,
    root_build: gtk::Box,
    viewport: gtk::ScrolledWindow,
}

impl ColumnHeader {
    pub(crate) fn root(&self) -> gtk::ScrolledWindow {
        self.viewport.clone()
    }

    /// Whether the header is on screen.
    pub(crate) fn set_visible(&self, visible: bool) {
        self.viewport.set_visible(visible);
    }

    /// Builds the column buttons once the root is in the widget tree. Called
    /// by `window.rs` after construction (and again after a column change).
    pub(crate) fn init(self: &Rc<Self>) {
        let root = &self.root_build;
        let gutter = gtk::Box::builder().width_request(ROW_LEADING_WIDTH).build();
        root.append(&gutter);
        root.append(&self.parts.cells);
        self.rebuild();
    }

    /// Replaces the buttons with the current visible columns, restoring the
    /// caret from the remembered sort.
    pub(crate) fn rebuild(self: &Rc<Self>) {
        let cells = &self.parts.cells;
        while let Some(child) = cells.first_child() {
            cells.remove(&child);
        }
        self.parts.buttons.borrow_mut().clear();

        for column in self.parts.state.visible() {
            let caret = gtk::Image::builder().icon_name("pan-down-symbolic").pixel_size(6).visible(false).build();
            let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(3).hexpand(column.expands()).build();
            // The icon-only columns (importance, attachment, flag) use the
            // same glyphs as the rows; a word would not fit their width.
            let icon_name = match column {
                MessageColumn::Importance => Some("emblem-important-symbolic"),
                MessageColumn::Attachment => Some("mail-attachment-symbolic"),
                MessageColumn::Flag => Some(themed_icon_name(&["mail-flag-symbolic", "flag-filled-symbolic", "mail-mark-important-symbolic"])),
                _ => None,
            };
            if let Some(icon_name) = icon_name {
                let icon = gtk::Image::builder().icon_name(icon_name).pixel_size(12).build();
                content.append(&icon);
            } else {
                // Cap the label's natural width to its cell, so a button with
                // a long word cannot widen the column and shift the ones after
                // it away from the row cells.
                let max_chars = if column.expands() { 40 } else { ((column.width() - 10) / 7).max(4) };
                let label = gtk::Label::builder()
                    .label(column.label())
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(max_chars)
                    .hexpand(column.expands())
                    .build();
                if matches!(column, MessageColumn::Received | MessageColumn::Size) {
                    label.set_xalign(1.0);
                } else {
                    label.set_xalign(0.0);
                }
                content.append(&label);
            }
            content.append(&caret);
            let button = gtk::Button::builder()
                .child(&content)
                .css_classes(["flat", "message-column-header-button"])
                .tooltip_text(format!("Sort by {}", column.label()))
                .hexpand(column.expands())
                .halign(if column.expands() { gtk::Align::Fill } else { gtk::Align::Start })
                .width_request(if column.expands() { 0 } else { column.width() })
                .focusable(true)
                .build();
            button.update_property(&[gtk::accessible::Property::Label(column.label())]);

            {
                let weak = Rc::downgrade(self);
                button.connect_clicked(move |_| {
                    if let Some(header) = weak.upgrade() {
                        header.click_column(column);
                    }
                });
            }
            {
                let weak = Rc::downgrade(self);
                let button_for_menu = button.clone();
                let gesture = gtk::GestureClick::new();
                gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
                gesture.connect_pressed(move |gesture, _, x, y| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(header) = weak.upgrade() {
                        header.open_menu(&button_for_menu, column, x, y);
                    }
                });
                button.add_controller(gesture);
            }

            cells.append(&button);
            self.parts.buttons.borrow_mut().push(ColumnButton { column, button, caret });
        }
        self.refresh(self.parts.sort.get().0, self.parts.sort.get().1);
    }

    /// Updates the carets to show `key` in `descending` order.
    pub(crate) fn refresh(&self, key: SortKey, descending: bool) {
        self.parts.sort.set((key, descending));
        for entry in self.parts.buttons.borrow().iter() {
            let active = entry.column.sort_key() == key;
            entry.caret.set_visible(active);
            if active {
                entry.caret.set_icon_name(Some(if descending { "pan-down-symbolic" } else { "pan-up-symbolic" }));
                entry.button.set_tooltip_text(Some(&format!(
                    "Sorted by {} ({}); click to reverse",
                    entry.column.label(),
                    if descending {
                        entry.column.sort_key().descending_label()
                    } else {
                        entry.column.sort_key().ascending_label()
                    }
                )));
            } else {
                entry.button.set_tooltip_text(Some(&format!("Sort by {}", entry.column.label())));
            }
        }
    }

    /// Re-fits the visible columns to a new list width; a change rebuilds the
    /// header and asks the window to refresh the rows.
    pub(crate) fn set_width(self: &Rc<Self>, width: i32) {
        if self.parts.state.fit_width(width) {
            self.rebuild();
            (self.parts.on_columns_changed)();
        }
    }

    fn click_column(&self, column: MessageColumn) {
        let (current, descending) = self.parts.sort.get();
        let key = column.sort_key();
        let descending = if current == key { !descending } else { key.natural_descending() };
        (self.parts.set_sort)(key, descending, true);
    }

    fn set_column_visible(self: &Rc<Self>, column: MessageColumn, visible: bool) {
        if self.parts.state.set_column_visible(column, visible, &self.parts.settings) {
            self.rebuild();
            (self.parts.on_columns_changed)();
        }
    }

    fn reset_columns(self: &Rc<Self>) {
        if self.parts.state.reset(&self.parts.settings) {
            self.rebuild();
            (self.parts.on_columns_changed)();
        }
    }

    /// The header's right-click menu: sort this column either way, choose the
    /// columns, reset. Built fresh per opening (the message-menu idiom), so
    /// the check marks and the popover's lifetime stay simple.
    fn open_menu(self: &Rc<Self>, anchor: &gtk::Button, column: MessageColumn, x: f64, y: f64) {
        let mut columns = MenuDescription::new().section();
        for candidate in MessageColumn::ALL {
            columns = columns.custom(format!("col-{}", candidate.id()));
        }
        let description = MenuDescription::new()
            .section()
            .action("Sort ascending", "cols.ascending")
            .action("Sort descending", "cols.descending")
            .submenu("Columns", columns)
            .section()
            .action("Reset columns", "cols.reset");

        let weak = Rc::downgrade(self);
        let target = column;
        let dispatch: Dispatch = Rc::new(move |name, _| {
            let Some(header) = weak.upgrade() else { return };
            match name {
                "ascending" => (header.parts.set_sort)(target.sort_key(), false, true),
                "descending" => (header.parts.set_sort)(target.sort_key(), true, true),
                "reset" => header.reset_columns(),
                _ => {}
            }
        });
        let specs = [ActionSpec::simple("ascending"), ActionSpec::simple("descending"), ActionSpec::simple("reset")];
        let menu = ContextMenu::new(&description, &specs, dispatch);
        for candidate in MessageColumn::ALL {
            let check = gtk::CheckButton::with_label(candidate.label());
            check.set_active(self.parts.state.is_configured(candidate));
            {
                let weak = Rc::downgrade(self);
                check.connect_toggled(move |check| {
                    if let Some(header) = weak.upgrade() {
                        header.set_column_visible(candidate, check.is_active());
                    }
                });
            }
            menu.add_child(&check, &format!("col-{}", candidate.id()));
        }

        let popover = menu.popover.clone();
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        // Parented to the header root, not the clicked button: toggling a
        // column rebuilds the buttons, and a popover whose parent is destroyed
        // would close mid-choice.
        popover.set_parent(&self.root_build);
        // The group lives on the popover, not the header, so reopening the
        // menu never stacks another "cols" group on the same widget.
        popover.insert_action_group("cols", Some(&menu.actions));
        let (x, y) = anchor
            .compute_point(&self.root_build, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|point| (point.x() as f64, point.y() as f64))
            .unwrap_or((x, y));
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }
}

/// A row/header cell box for `column`: the fixed width, the expanding subject,
/// and centred content. The row fills it with the column's widgets.
pub(crate) fn column_cell(column: MessageColumn) -> gtk::Box {
    let cell = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(3)
        .valign(gtk::Align::Center)
        .hexpand(column.expands())
        .build();
    cell.add_css_class("message-single-cell");
    if !column.expands() {
        cell.set_width_request(column.width());
    }
    cell
}

/// Applies the state's visible set to one row's cells (message or thread).
pub(crate) fn apply_cell_visibility(cells: &[(MessageColumn, gtk::Widget)], state: &ColumnState) {
    for (column, cell) in cells {
        cell.set_visible(state.is_visible(*column));
    }
}

/// The sort path's caret refresh. A free function so `window.rs` can call it
/// with the weak header it already holds.
pub(crate) fn refresh_header(header: &Weak<ColumnHeader>, key: SortKey, descending: bool) {
    if let Some(header) = header.upgrade() {
        header.refresh(key, descending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_columns::DEFAULT_COLUMNS;

    fn state() -> ColumnState {
        ColumnState {
            configured: Rc::new(RefCell::new(DEFAULT_COLUMNS.to_vec())),
            visible: Rc::new(RefCell::new(DEFAULT_COLUMNS.to_vec())),
            width: Rc::new(Cell::new(0)),
        }
    }

    fn ids(state: &ColumnState) -> Vec<&'static str> {
        state.visible().iter().map(|column| column.id()).collect()
    }

    #[test]
    fn the_visible_set_narrows_and_widens_with_the_pane() {
        let state = state();
        // The default set already fits a wide pane, so the first fit there is
        // a no-op; the fits that matter are the narrowing ones.
        assert!(!state.fit_width(1200));
        assert!(ids(&state).contains(&"categories"));
        assert!(!state.fit_width(1200), "a width inside the same band is a no-op");

        assert!(state.fit_width(420));
        let narrow = ids(&state);
        assert!(narrow.contains(&"from"));
        assert!(narrow.contains(&"subject"));
        assert!(narrow.contains(&"received"));
        assert!(!narrow.contains(&"categories"), "categories are shed before the core");

        assert!(state.fit_width(1200));
        assert!(ids(&state).contains(&"categories"), "growing the pane brings the column back");
    }
}
