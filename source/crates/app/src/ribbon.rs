//! The Outlook-style ribbon: groups of icon+label controls, laid out either
//! Simplified (one row of 20 px icons, the label beside each; a "plain"
//! control shows its icon alone) or Classic (named groups in which a large
//! control is a 32 px icon over its label and small ones, a 16 px icon beside
//! the label, stack three to a column).
//!
//! The ribbon only presents widgets the caller has already built and wired.
//! [`RibbonGroup`]'s `button`/`toggle`/`menu`/`split` (large) and their
//! `small_*` twins swap a control's child for an icon+label box, so click
//! handlers, shortcuts and sensitivity stay exactly where they were;
//! switching layout re-orients that same box and moves the same widgets into
//! or out of their Classic columns, so nothing is rebuilt and no handler is
//! lost. [`RibbonIcon`] is the handle a
//! dynamic control (Pin, Flag, Read/Unread) uses to swap its glyph.
//!
//! [`RibbonOverflow`] makes the row adaptive: as it narrows, labels drop to
//! icon-only and then the controls that no longer fit move - still live - into
//! the trailing More popover. Layout is the first control to move, and More
//! itself is never overflowed.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// Which ribbon layout is in force (`ribbon-style` in GSettings).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RibbonMode {
    #[default]
    Simplified,
    Classic,
}

impl RibbonMode {
    /// In menu order.
    pub(crate) const ALL: [RibbonMode; 2] = [RibbonMode::Simplified, RibbonMode::Classic];

    /// The value stored in GSettings and carried by the `win.ribbon-style`
    /// stateful action.
    pub(crate) fn code(self) -> &'static str {
        match self {
            RibbonMode::Simplified => "simplified",
            RibbonMode::Classic => "classic",
        }
    }

    /// An unknown saved value reads as the default rather than failing.
    pub(crate) fn from_code(code: &str) -> RibbonMode {
        RibbonMode::ALL.into_iter().find(|m| m.code().eq_ignore_ascii_case(code.trim())).unwrap_or_default()
    }

    /// The English label (the menu and the Config row show it).
    pub(crate) fn label(self) -> &'static str {
        match self {
            RibbonMode::Simplified => "Simplified Ribbon",
            RibbonMode::Classic => "Classic Ribbon",
        }
    }

    /// Its position in [`RibbonMode::ALL`], i.e. in the layout menu and the
    /// Config → Appearance row.
    pub(crate) fn index(self) -> u32 {
        RibbonMode::ALL.iter().position(|m| *m == self).unwrap_or(0) as u32
    }
}

/// How a control is drawn in the Classic layout: large (the icon over its
/// label) or small (the icon beside its label, three to a column). The
/// Simplified row draws both alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RibbonSize {
    Large,
    Small,
}

/// The shape a dressed control takes, from the layout, its size and whether
/// it has moved into More.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Simplified,
    Large,
    Small,
    /// A row of the More popover: a menu entry, whichever layout is in force.
    Overflow,
}

/// The handle to a dressed control's icon. A control created with a live glyph
/// (Pin, Flag, Read/Unread) takes one of these and calls [`RibbonIcon::set_icon_name`]
/// instead of reaching for the button, which would tear the icon+label child
/// out from under the layout. It also carries the button so a live tooltip can
/// follow the glyph.
#[derive(Clone)]
pub(crate) struct RibbonIcon {
    image: gtk::Image,
    label: glib::WeakRef<gtk::Label>,
    button: glib::WeakRef<gtk::Widget>,
}

impl RibbonIcon {
    /// Swap the glyph, as the button's own `set_icon_name` used to.
    pub(crate) fn set_icon_name(&self, name: &str) {
        self.image.set_icon_name(Some(name));
    }

    /// Swap the control's label, for a command whose name names the action
    /// it will take (Mark as read / Mark as unread, UX-07). The accessible
    /// name follows so assistive technology hears the same wording.
    pub(crate) fn set_label(&self, text: &str) {
        if let Some(label) = self.label.upgrade() {
            label.set_label(text);
        }
        if let Some(button) = self.button.upgrade() {
            button.update_property(&[gtk::accessible::Property::Label(text)]);
        }
    }

    /// Update the owning button's tooltip (Pin/Unpin, Mark Read/Unread).
    pub(crate) fn set_tooltip_text(&self, text: Option<&str>) {
        if let Some(button) = self.button.upgrade() {
            button.set_tooltip_text(text);
        }
    }

    /// Enable or disable the owning button - used by Pin, whose capability
    /// depends on the selected account's mail transport (item 5.7.7).
    pub(crate) fn set_sensitive(&self, sensitive: bool) {
        if let Some(button) = self.button.upgrade() {
            button.set_sensitive(sensitive);
        }
    }
}

type RelayoutListener = Rc<dyn Fn()>;

/// The shared ribbon state: the mode in force and every live band, so one
/// switch re-lays-out them all.
#[derive(Clone)]
pub(crate) struct RibbonStyle {
    mode: Rc<Cell<RibbonMode>>,
    bands: Rc<RefCell<Vec<BandRegistration>>>,
    /// Callbacks run whenever the style changes, so the overflow controller
    /// can re-measure with the new control geometry.
    relayout_listeners: Rc<RefCell<Vec<RelayoutListener>>>,
}

struct BandRegistration {
    root: glib::WeakRef<gtk::Box>,
    /// The band's controls. Held strongly so the layout survives
    /// as long as the widgets do; the widgets themselves are only weakly
    /// referenced, so a closed composer's band is dropped on the next prune
    /// (which runs whenever a band is created or the mode changes).
    state: Rc<RefCell<BandState>>,
}

impl RibbonStyle {
    pub(crate) fn new(mode: RibbonMode) -> Self {
        RibbonStyle {
            mode: Rc::new(Cell::new(mode)),
            bands: Rc::new(RefCell::new(Vec::new())),
            relayout_listeners: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn mode(&self) -> RibbonMode {
        self.mode.get()
    }

    /// Register a callback for style changes. It is kept weakly by the
    /// overflow controller, so this never outlives the window that owns it.
    pub(crate) fn add_relayout_listener(&self, listener: Rc<dyn Fn()>) {
        self.relayout_listeners.borrow_mut().push(listener);
    }

    /// Switch every live band to `mode`. Bands whose root widget has gone
    /// (a composer that has closed) are dropped from the registry.
    pub(crate) fn set_mode(&self, mode: RibbonMode) {
        self.mode.set(mode);
        self.bands.borrow_mut().retain(|band| {
            let Some(root) = band.root.upgrade() else {
                return false;
            };
            apply_band(&root, &band.state.borrow(), mode);
            true
        });
        // The two layouts differ in every control's size, so the overflow fit
        // has to be redone for the new one.
        let listeners = self.relayout_listeners.borrow().clone();
        for listener in listeners {
            listener();
        }
    }
}

#[derive(Default)]
struct BandState {
    controls: Vec<ControlParts>,
    groups: Vec<BandGroup>,
    /// Labels are hidden by the overflow controller when the ribbon is too
    /// narrow for icon+label controls (Outlook drops labels before it moves
    /// controls into More).
    compact: Cell<bool>,
}

/// One group inside a band: a named group's box or a standalone row, plus the
/// divider that follows it and the indices of its controls in
/// [`BandState::controls`].
struct BandGroup {
    container: Option<glib::WeakRef<gtk::Box>>,
    row: glib::WeakRef<gtk::Box>,
    divider: Option<glib::WeakRef<gtk::Separator>>,
    /// The group's name under its controls, drawn in the Classic layout only.
    title: Option<glib::WeakRef<gtk::Label>>,
    controls: Vec<usize>,
}

struct ControlParts {
    content: glib::WeakRef<gtk::Box>,
    image: glib::WeakRef<gtk::Image>,
    label: glib::WeakRef<gtk::Label>,
    widget: glib::WeakRef<gtk::Widget>,
    size: RibbonSize,
    /// Its icon says what it is, so the Simplified row shows it without its
    /// label (Bold, Italic, the alignments).
    plain: bool,
    /// True while the control has been moved into the More popover.
    in_overflow: Cell<bool>,
    /// Whether the "close More after activating this control" handler has
    /// been attached (once, the first time it overflows).
    closes_more_on_click: Cell<bool>,
}

/// One band of controls (a module's/tab's ribbon row). Its [`RibbonGroup`]s
/// carry the divider between commands.
pub(crate) struct RibbonBand {
    style: RibbonStyle,
    root: gtk::Box,
    state: Rc<RefCell<BandState>>,
}

impl RibbonBand {
    /// A new, shrinkable band. `css_classes` are the extra style classes the
    /// row wants on top of `.ribbon` (e.g. `.toolbar` for the legacy look).
    pub(crate) fn new(style: &RibbonStyle, css_classes: &[&str]) -> Self {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
        root.add_css_class("ribbon");
        for class in css_classes {
            root.add_css_class(class);
        }
        let state = Rc::new(RefCell::new(BandState::default()));
        {
            // Drop bands whose widgets have gone before adding this one, so a
            // session that opens and closes many composers doesn't accumulate
            // dead registrations.
            let mut bands = style.bands.borrow_mut();
            bands.retain(|band| band.root.upgrade().is_some());
            bands.push(BandRegistration {
                root: root.downgrade(),
                state: state.clone(),
            });
        }
        let band = RibbonBand {
            style: style.clone(),
            root,
            state,
        };
        band.apply();
        band
    }

    /// The band's widget, to hand to a `gtk::Stack` or a box.
    pub(crate) fn root(&self) -> &gtk::Box {
        &self.root
    }

    /// Add a named group. `build` dresses that group's controls and may return
    /// handles (e.g. a dynamic icon) for the caller to keep.
    /// `name` isn't drawn; it is the group's accessible label.
    pub(crate) fn group<T>(&self, name: &str, build: impl FnOnce(&RibbonGroup) -> T) -> T {
        let group_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).build();
        group_box.add_css_class("ribbon-group");
        group_box.update_property(&[gtk::accessible::Property::Label(name)]);
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
        row.add_css_class("ribbon-group-row");
        group_box.append(&row);
        let title = gtk::Label::builder().label(name).css_classes(["ribbon-group-title", "dim-label"]).build();
        title.set_visible(self.style.mode() == RibbonMode::Classic);
        group_box.append(&title);
        let divider = gtk::Separator::new(gtk::Orientation::Vertical);
        divider.add_css_class("ribbon-divider");
        self.root.append(&group_box);
        self.root.append(&divider);
        let group_index = {
            let mut state = self.state.borrow_mut();
            state.groups.push(BandGroup {
                container: Some(group_box.downgrade()),
                row: row.downgrade(),
                divider: Some(divider.downgrade()),
                title: Some(title.downgrade()),
                controls: Vec::new(),
            });
            state.groups.len() - 1
        };
        let group = RibbonGroup {
            style: self.style.clone(),
            state: self.state.clone(),
            row,
            group_index,
            plain: Cell::new(false),
        };
        build(&group)
    }

    /// A bare run of controls with no divider (the trailing overflow button),
    /// dressed just like a named group's.
    pub(crate) fn standalone(&self) -> RibbonGroup {
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
        row.add_css_class("ribbon-group-row");
        self.root.append(&row);
        let group_index = {
            let mut state = self.state.borrow_mut();
            state.groups.push(BandGroup {
                container: None,
                row: row.downgrade(),
                divider: None,
                title: None,
                controls: Vec::new(),
            });
            state.groups.len() - 1
        };
        RibbonGroup {
            style: self.style.clone(),
            state: self.state.clone(),
            row,
            group_index,
            plain: Cell::new(false),
        }
    }

    fn apply(&self) {
        apply_band(&self.root, &self.state.borrow(), self.style.mode());
    }
}

/// A group of controls inside a [`RibbonBand`].
pub(crate) struct RibbonGroup {
    style: RibbonStyle,
    state: Rc<RefCell<BandState>>,
    row: gtk::Box,
    group_index: usize,
    /// Set by [`RibbonGroup::plain`]: the controls dressed after it drop their
    /// labels in the Simplified row.
    plain: Cell<bool>,
}

impl RibbonGroup {
    /// Dress a plain button with `icon` and `label`, as a large control.
    pub(crate) fn button(&self, button: &gtk::Button, icon: &str, label: &str) -> RibbonIcon {
        self.dress_button(button, icon, label, RibbonSize::Large)
    }

    /// [`RibbonGroup::button`], as a small control.
    pub(crate) fn small_button(&self, button: &gtk::Button, icon: &str, label: &str) -> RibbonIcon {
        self.dress_button(button, icon, label, RibbonSize::Small)
    }

    /// Dress a toggle button (a ribbon switch such as Folder pane).
    pub(crate) fn toggle(&self, button: &gtk::ToggleButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_button(button.upcast_ref(), icon, label, RibbonSize::Large)
    }

    /// [`RibbonGroup::toggle`], as a small control.
    pub(crate) fn small_toggle(&self, button: &gtk::ToggleButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_button(button.upcast_ref(), icon, label, RibbonSize::Small)
    }

    /// Dress a menu button (a `▾` dropdown); the arrow is shown beside the
    /// content so a labelled menu button keeps its affordance.
    pub(crate) fn menu(&self, button: &gtk::MenuButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_menu(button, icon, label, RibbonSize::Large)
    }

    /// [`RibbonGroup::menu`], as a small control.
    pub(crate) fn small_menu(&self, button: &gtk::MenuButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_menu(button, icon, label, RibbonSize::Small)
    }

    /// Dress a libadwaita split button (a main action plus its `▾` menu).
    pub(crate) fn split(&self, button: &adw::SplitButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_split(button, icon, label, RibbonSize::Large)
    }

    /// [`RibbonGroup::split`], as a small control.
    pub(crate) fn small_split(&self, button: &adw::SplitButton, icon: &str, label: &str) -> RibbonIcon {
        self.dress_split(button, icon, label, RibbonSize::Small)
    }

    /// Append a control that carries no icon+label of its own (a `DropDown`,
    /// a colour button, a spin button) to the group's row. It is a small
    /// control, so the Classic layout stacks it with its neighbours, and it
    /// still takes part in overflow, so it moves into More when the ribbon is
    /// too narrow.
    pub(crate) fn widget(&self, widget: &impl IsA<gtk::Widget>) {
        let widget = widget.clone().upcast::<gtk::Widget>();
        self.row.append(&widget);
        self.register_widget(widget, glib::WeakRef::new(), glib::WeakRef::new(), glib::WeakRef::new(), RibbonSize::Small);
    }

    /// Mark the controls dressed from here on as ones whose icon says what
    /// they are (Bold, Italic): the Simplified row shows them without labels.
    pub(crate) fn plain(&self) {
        self.plain.set(true);
    }

    fn dress_button(&self, button: &gtk::Button, icon: &str, label: &str, size: RibbonSize) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        self.register(button.clone().upcast(), content, image, text, size)
    }

    fn dress_menu(&self, button: &gtk::MenuButton, icon: &str, label: &str, size: RibbonSize) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        button.set_always_show_arrow(true);
        self.register(button.clone().upcast(), content, image, text, size)
    }

    fn dress_split(&self, button: &adw::SplitButton, icon: &str, label: &str, size: RibbonSize) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        self.register(button.clone().upcast(), content, image, text, size)
    }

    fn register(&self, widget: gtk::Widget, content: gtk::Box, image: gtk::Image, label: gtk::Label, size: RibbonSize) -> RibbonIcon {
        // `flat` is load-bearing, not cosmetic: setting a custom child strips
        // the `.image-button`/`.text-button` class GTK derives from the
        // button's icon/label, which is exactly what Adwaita's `.toolbar`
        // flattening rule keys off - so without it a plain button would paint
        // its full button background while the menu/split controls (whose
        // flattening selectors don't depend on those classes) stayed flat, and
        // the ribbon would be a mix. `flat` flattens every shape uniformly and
        // keeps the theme's own hover/checked feedback.
        widget.add_css_class("ribbon-control");
        widget.add_css_class("flat");
        // Keep the command's name available to assistive technology even when
        // compact mode hides the visual label.
        widget.update_property(&[gtk::accessible::Property::Label(label.text().as_str())]);
        self.row.append(&widget);
        self.register_widget(widget.clone(), content.downgrade(), image.downgrade(), label.downgrade(), size);
        RibbonIcon {
            image,
            label: label.downgrade(),
            button: widget.downgrade(),
        }
    }

    /// Record a control in the band's ordered group list, for overflow, then
    /// style it and give it its place for the layout in force. The styled
    /// parts are empty for a plain [`RibbonGroup::widget`].
    fn register_widget(&self, widget: gtk::Widget, content: glib::WeakRef<gtk::Box>, image: glib::WeakRef<gtk::Image>, label: glib::WeakRef<gtk::Label>, size: RibbonSize) {
        widget.add_css_class(match size {
            RibbonSize::Large => "ribbon-large",
            RibbonSize::Small => "ribbon-small",
        });
        let mode = self.style.mode();
        let mut state = self.state.borrow_mut();
        let index = state.controls.len();
        let control = ControlParts {
            content,
            image,
            label,
            widget: widget.downgrade(),
            size,
            plain: self.plain.get(),
            in_overflow: Cell::new(false),
            closes_more_on_click: Cell::new(false),
        };
        style_parts(&control, state.compact.get(), mode);
        state.controls.push(control);
        state.groups[self.group_index].controls.push(index);
        let state = &*state;
        if let Some(group) = state.groups.get(self.group_index) {
            place_group(state, group, mode);
        }
    }
}

/// Build the icon+label content box a control wears (its orientation is set by
/// [`style_control`], which runs for the current mode and on every switch).
fn build_content(icon: &str, label: &str) -> (gtk::Box, gtk::Image, gtk::Label) {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.add_css_class("ribbon-content");
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("ribbon-icon");
    let text = gtk::Label::builder().label(label).build();
    text.add_css_class("ribbon-label");
    content.append(&image);
    content.append(&text);
    (content, image, text)
}

fn style_control(content: &gtk::Box, image: &gtk::Image, label: &gtk::Label, look: Look) {
    let (orientation, spacing, icon, halign) = match look {
        Look::Simplified => (gtk::Orientation::Horizontal, 8, 20, gtk::Align::Fill),
        Look::Large => (gtk::Orientation::Vertical, 2, 32, gtk::Align::Center),
        // A column's buttons share its width; keep each icon at its leading
        // edge so the column's icons line up, as a menu's entries do.
        Look::Small | Look::Overflow => (gtk::Orientation::Horizontal, 6, 16, gtk::Align::Start),
    };
    content.set_orientation(orientation);
    content.set_spacing(spacing);
    content.set_halign(halign);
    image.set_pixel_size(icon);
    // A large button's label may take two lines ("New / Message"), centred
    // under its icon; everywhere else it stays on one.
    let large = look == Look::Large;
    label.set_wrap(large);
    label.set_max_width_chars(if large { 10 } else { -1 });
    label.set_justify(if large { gtk::Justification::Center } else { gtk::Justification::Left });
    label.set_halign(if large { gtk::Align::Center } else { gtk::Align::Start });
}

/// Style one control for `mode`: its shape, and whether its label shows.
fn style_parts(control: &ControlParts, compact: bool, mode: RibbonMode) {
    let in_overflow = control.in_overflow.get();
    let (Some(content), Some(image), Some(label)) = (control.content.upgrade(), control.image.upgrade(), control.label.upgrade()) else {
        return;
    };
    let look = match (in_overflow, mode, control.size) {
        (true, _, _) => Look::Overflow,
        (false, RibbonMode::Simplified, _) => Look::Simplified,
        (false, RibbonMode::Classic, RibbonSize::Large) => Look::Large,
        (false, RibbonMode::Classic, RibbonSize::Small) => Look::Small,
    };
    style_control(&content, &image, &label, look);
    // Compact mode drops labels, and a plain control has none in the
    // Simplified row; the More popover keeps them so its entries stay
    // identifiable.
    let hidden = compact || (control.plain && mode == RibbonMode::Simplified);
    label.set_visible(in_overflow || !hidden);
}

/// One place in a group's row: a control by itself, or a column of up to
/// three small ones (Classic only).
enum Slot {
    Alone(gtk::Widget),
    Column(Vec<gtk::Widget>),
}

/// How many small controls a Classic column holds.
const COLUMN_LEN: usize = 3;

/// Give `group`'s controls (those not in More) their places in its row for
/// `mode`: side by side in the Simplified row; in the Classic layout each
/// large control by itself and each run of small ones stacked in columns of
/// three. Nothing is moved when the row already has that shape, since the
/// overflow fit calls this on every width change.
fn place_group(state: &BandState, group: &BandGroup, mode: RibbonMode) {
    let Some(row) = group.row.upgrade() else {
        return;
    };
    let mut slots: Vec<Slot> = Vec::new();
    for control in group.controls.iter().filter_map(|index| state.controls.get(*index)) {
        if control.in_overflow.get() {
            continue;
        }
        let Some(widget) = control.widget.upgrade() else {
            continue;
        };
        if mode == RibbonMode::Classic && control.size == RibbonSize::Small {
            match slots.last_mut() {
                Some(Slot::Column(column)) if column.len() < COLUMN_LEN => column.push(widget),
                _ => slots.push(Slot::Column(vec![widget])),
            }
        } else {
            slots.push(Slot::Alone(widget));
        }
    }
    if row_matches(&row, &slots) {
        return;
    }
    for slot in &slots {
        let widgets = match slot {
            Slot::Alone(widget) => std::slice::from_ref(widget),
            Slot::Column(column) => column.as_slice(),
        };
        for widget in widgets {
            if widget.parent().is_some() {
                widget.unparent();
            }
        }
    }
    // What is left is the old columns.
    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
    for slot in slots {
        match slot {
            Slot::Alone(widget) => {
                widget.set_halign(gtk::Align::Fill);
                row.append(&widget);
            }
            Slot::Column(widgets) => {
                let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).build();
                column.add_css_class("ribbon-small-column");
                for widget in widgets {
                    widget.set_halign(gtk::Align::Fill);
                    column.append(&widget);
                }
                row.append(&column);
            }
        }
    }
}

/// Whether `row` already holds exactly `slots`, in order.
fn row_matches(row: &gtk::Box, slots: &[Slot]) -> bool {
    let mut child = row.first_child();
    for slot in slots {
        let Some(current) = child else {
            return false;
        };
        let fits = match slot {
            Slot::Alone(widget) => current == *widget,
            Slot::Column(widgets) => current.has_css_class("ribbon-small-column") && children(&current) == *widgets,
        };
        if !fits {
            return false;
        }
        child = current.next_sibling();
    }
    child.is_none()
}

fn children(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    std::iter::successors(widget.first_child(), |child| child.next_sibling()).collect()
}

fn apply_band(root: &gtk::Box, state: &BandState, mode: RibbonMode) {
    if mode == RibbonMode::Classic {
        root.add_css_class("ribbon-classic");
    } else {
        root.remove_css_class("ribbon-classic");
    }
    for group in &state.groups {
        place_group(state, group, mode);
        if let Some(title) = group.title.as_ref().and_then(|title| title.upgrade()) {
            title.set_visible(mode == RibbonMode::Classic);
        }
    }
    apply_control_styles(state, mode);
}

fn apply_control_styles(state: &BandState, mode: RibbonMode) {
    for control in &state.controls {
        style_parts(control, state.compact.get(), mode);
    }
}

/// The ribbon's adaptive overflow: keeps the trailing More button pinned,
/// drops labels first, then moves the controls that no longer fit into the
/// More popover.
///
/// It owns no commands of its own. Controls are moved - not duplicated - into
/// the popover and moved back to their original row when space returns, so
/// every click handler, split menu, toggle state and dynamic popover keeps
/// working exactly as it did on the ribbon.
#[derive(Clone)]
pub(crate) struct RibbonOverflow {
    inner: Rc<OverflowInner>,
}

#[derive(Clone)]
struct RegisteredBand {
    root: glib::WeakRef<gtk::Box>,
    state: Rc<RefCell<BandState>>,
}

struct OverflowInner {
    style: RibbonStyle,
    /// Weak: the controller is kept alive by the More popover's show handler,
    /// so holding the button strongly would create a reference cycle.
    more_button: glib::WeakRef<gtk::MenuButton>,
    /// The one-button band that pins More at the right edge.
    more_root: glib::WeakRef<gtk::Box>,
    overflow_box: gtk::Box,
    extras_box: gtk::Box,
    separator: gtk::Separator,
    bar: RefCell<glib::WeakRef<gtk::Box>>,
    stack: RefCell<glib::WeakRef<gtk::Stack>>,
    bands: RefCell<Vec<RegisteredBand>>,
    layout: RefCell<Option<RegisteredBand>>,
    mail_home: RefCell<glib::WeakRef<gtk::Box>>,
    /// Guards against the width notifications our own visibility changes
    /// cause while a fit is in progress.
    refreshing: Cell<bool>,
}

impl RibbonOverflow {
    /// `more_band` is the rigid band holding the More button; its width is
    /// always reserved. `overflow_box` is the vertical container inside the
    /// More popover that receives the moved controls.
    pub(crate) fn new(
        style: &RibbonStyle,
        more_button: &gtk::MenuButton,
        overflow_box: &gtk::Box,
        extras_box: &gtk::Box,
        separator: &gtk::Separator,
        more_band: &RibbonBand,
    ) -> Self {
        let inner = Rc::new(OverflowInner {
            style: style.clone(),
            more_button: more_button.downgrade(),
            more_root: more_band.root.downgrade(),
            overflow_box: overflow_box.clone(),
            extras_box: extras_box.clone(),
            separator: separator.clone(),
            bar: RefCell::new(glib::WeakRef::new()),
            stack: RefCell::new(glib::WeakRef::new()),
            bands: RefCell::new(Vec::new()),
            layout: RefCell::new(None),
            mail_home: RefCell::new(glib::WeakRef::new()),
            refreshing: Cell::new(false),
        });
        // A layout switch changes every control's natural width without
        // necessarily changing the row's allocation, so listen to the style
        // rather than waiting for a resize.
        let weak = Rc::downgrade(&inner);
        style.add_relayout_listener(Rc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.refresh();
            }
        }));
        RibbonOverflow { inner }
    }

    /// Register a normal ribbon page. Its controls are the overflow source
    /// whenever it is the stack's visible child.
    pub(crate) fn add_band(&self, band: &RibbonBand) {
        self.inner.bands.borrow_mut().push(RegisteredBand {
            root: band.root.downgrade(),
            state: band.state.clone(),
        });
    }

    /// Register the Layout band. It is the first thing to move into More when
    /// the ribbon runs out of room.
    pub(crate) fn set_layout_band(&self, band: &RibbonBand) {
        *self.inner.layout.borrow_mut() = Some(RegisteredBand {
            root: band.root.downgrade(),
            state: band.state.clone(),
        });
    }

    /// The Mail Home command band. Its More menu always has entries (the
    /// reading-pane extras), so More stays enabled while it is showing.
    pub(crate) fn set_mail_home_band(&self, band: &RibbonBand) {
        *self.inner.mail_home.borrow_mut() = band.root.downgrade();
    }

    /// Start watching `bar` (the whole ribbon row) and `stack` (the page
    /// switcher). Call after the band has been parented, so the first fit sees
    /// a real allocation.
    pub(crate) fn attach(&self, bar: &gtk::Box, stack: &gtk::Stack) {
        *self.inner.bar.borrow_mut() = bar.downgrade();
        *self.inner.stack.borrow_mut() = stack.downgrade();
        {
            let inner = Rc::downgrade(&self.inner);
            bar.connect_notify_local(Some("width"), move |_, _| {
                if let Some(inner) = inner.upgrade() {
                    inner.refresh();
                }
            });
        }
        {
            let inner = Rc::downgrade(&self.inner);
            bar.connect_map(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.refresh();
                }
            });
        }
        {
            let inner = Rc::downgrade(&self.inner);
            stack.connect_notify_local(Some("visible-child"), move |_, _| {
                if let Some(inner) = inner.upgrade() {
                    inner.refresh();
                }
            });
        }
        self.inner.refresh();
    }

    /// Recompute the separation/visibility of the popover's sections. The
    /// More popover's show handler calls this after rebuilding the Mail
    /// extras.
    pub(crate) fn sync_sections(&self) {
        self.inner.sync_sections();
    }
}

impl OverflowInner {
    fn refresh(&self) {
        if self.refreshing.replace(true) {
            return;
        }
        self.refit();
        self.refreshing.set(false);
    }

    fn refit(&self) {
        let Some(bar) = self.bar.borrow().upgrade() else {
            return;
        };
        let Some(stack) = self.stack.borrow().upgrade() else {
            return;
        };
        self.refit_to(bar.width(), &stack);
    }

    /// Fit the active page into `width` pixels. Separate from [`Self::refit`]
    /// so the geometry can be tested without showing a window.
    fn refit_to(&self, width: i32, stack: &gtk::Stack) {
        if width <= 0 {
            return;
        }
        // The pages live in a scrolled window with external scrollbars (so
        // the row's minimum width is zero). Keep it anchored at the leading
        // edge; after the fit there is no scroll range anyway.
        if let Some(scroller) = stack.parent().and_downcast::<gtk::ScrolledWindow>() {
            scroller.hadjustment().set_value(0.0);
        }
        // Always start from the complete ribbon, then put back only what
        // fits. This also restores controls left in More by a previous fit.
        self.restore_all();
        let Some(active) = self.active_band(stack) else {
            // A composer toolbar (or any non-ribbon page) is showing.
            if let Some(layout) = self.layout.borrow().as_ref().and_then(|l| l.root.upgrade()) {
                layout.set_visible(true);
            }
            self.sync_sections();
            return;
        };
        let Some(root) = active.root.upgrade() else {
            self.sync_sections();
            return;
        };

        let more_width = self.more_width();
        // A couple of pixels of slack keep rounding/the scroller's own
        // geometry from leaving the row one pixel too wide.
        let full_available = width - more_width - 2;
        let layout = self.layout.borrow().clone();
        let layout_visible = layout.as_ref().is_some_and(|l| l.root.upgrade().is_some());
        let layout_width = if layout_visible { self.layout_width() } else { 0 };
        let mut available = full_available - layout_width;

        // 1. Everything with labels.
        if root.measure(gtk::Orientation::Horizontal, -1).1 <= available {
            self.finish_fit(&active, layout.as_ref());
            return;
        }

        // 2. Icon-only, labels dropped.
        let mode = self.style.mode();
        set_compact(&active, true, mode);
        let mut compact_width = root.measure(gtk::Orientation::Horizontal, -1).1;
        if layout_visible {
            if let Some(layout) = &layout {
                set_compact(layout, true, mode);
            }
            // The Layout button's own label has just dropped, so its width
            // (and the page's available width) changed with it.
            available = full_available - self.layout_width();
            if compact_width <= available {
                self.finish_fit(&active, layout.as_ref());
                return;
            }
            // 3. Layout is the rightmost item, so it is the first to go.
            if let Some(layout) = &layout {
                self.move_band_to_overflow(layout);
                if let Some(root) = layout.root.upgrade() {
                    root.set_visible(false);
                }
            }
            available = full_available;
            compact_width = root.measure(gtk::Orientation::Horizontal, -1).1;
            if compact_width <= available {
                self.finish_fit(&active, layout.as_ref());
                return;
            }
        }

        // 4. Move the active page's controls into More, right to left.
        let control_count = active.state.borrow().controls.len();
        for index in (0..control_count).rev() {
            if root.measure(gtk::Orientation::Horizontal, -1).1 <= available {
                break;
            }
            self.move_control_to_overflow(&active, index);
            sync_group_visibility(&active.state.borrow());
        }
        self.finish_fit(&active, layout.as_ref());
    }

    fn finish_fit(&self, active: &RegisteredBand, layout: Option<&RegisteredBand>) {
        self.reorder_overflow(active, layout);
        self.sync_sections();
    }

    fn active_band(&self, stack: &gtk::Stack) -> Option<RegisteredBand> {
        let visible = stack.visible_child()?;
        self.bands
            .borrow()
            .iter()
            .find(|band| band.root.upgrade().is_some_and(|root| root.upcast::<gtk::Widget>() == visible))
            .cloned()
    }

    fn more_width(&self) -> i32 {
        self.more_root
            .upgrade()
            .map(|root| root.measure(gtk::Orientation::Horizontal, -1).1)
            .or_else(|| self.more_button.upgrade().map(|button| button.measure(gtk::Orientation::Horizontal, -1).1))
            .unwrap_or(0)
    }

    fn layout_width(&self) -> i32 {
        self.layout
            .borrow()
            .as_ref()
            .and_then(|layout| layout.root.upgrade())
            .map(|root| root.measure(gtk::Orientation::Horizontal, -1).1)
            .unwrap_or(0)
    }

    /// Put every control back on the ribbon and show all labels. The fit then
    /// hides/moves only what the current width cannot hold.
    fn restore_all(&self) {
        let mode = self.style.mode();
        for band in self.bands.borrow().iter() {
            restore_band(band, mode);
        }
        if let Some(layout) = self.layout.borrow().clone() {
            restore_band(&layout, mode);
            if let Some(root) = layout.root.upgrade() {
                root.set_visible(true);
            }
        }
    }

    fn move_band_to_overflow(&self, band: &RegisteredBand) {
        let state = band.state.borrow();
        for index in (0..state.controls.len()).rev() {
            self.move_control_to_overflow(band, index);
        }
        sync_group_visibility(&state);
    }

    fn move_control_to_overflow(&self, band: &RegisteredBand, index: usize) {
        let state = band.state.borrow();
        let Some(control) = state.controls.get(index) else {
            return;
        };
        if control.in_overflow.get() {
            return;
        }
        let Some(widget) = control.widget.upgrade() else {
            return;
        };
        if let Some(parent) = widget.parent() {
            widget.unparent();
            // A Classic column it was the last control of would otherwise
            // stay behind as an empty gap in the row.
            if parent.has_css_class("ribbon-small-column") && parent.first_child().is_none() {
                parent.unparent();
            }
        }
        control.in_overflow.set(true);
        self.overflow_box.append(&widget);
        widget.add_css_class("ribbon-overflow-control");
        widget.set_halign(gtk::Align::Fill);
        // The popover rows are full width; the overflow look keeps the
        // icon+label pair at the leading edge like a menu entry.
        style_parts(control, state.compact.get(), self.style.mode());
        // Acting on an overflowed command should dismiss More, like any menu
        // entry. Dropdowns (MenuButton) keep it open so their own menu can be
        // used; split buttons only dismiss on their main action.
        if !control.closes_more_on_click.replace(true) {
            let more_button = self.more_button.clone();
            if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                button.connect_clicked(move |_| {
                    if let Some(more_button) = more_button.upgrade() {
                        more_button.popdown();
                    }
                });
            } else if let Some(split) = widget.downcast_ref::<adw::SplitButton>() {
                split.connect_clicked(move |_| {
                    if let Some(more_button) = more_button.upgrade() {
                        more_button.popdown();
                    }
                });
            }
        }
    }

    /// Put the overflowed controls back in ribbon order, then the Layout
    /// control last. The overflow box is a flat list, so its order is the
    /// order the commands appear on the ribbon.
    fn reorder_overflow(&self, active: &RegisteredBand, layout: Option<&RegisteredBand>) {
        reorder_band(&self.overflow_box, &active.state.borrow());
        if let Some(layout) = layout {
            reorder_band(&self.overflow_box, &layout.state.borrow());
        }
    }

    fn sync_sections(&self) {
        let Some(more_button) = self.more_button.upgrade() else {
            return;
        };
        let mail_home_showing = self
            .mail_home
            .borrow()
            .upgrade()
            .zip(self.stack.borrow().upgrade())
            .is_some_and(|(mail_home, stack)| stack.visible_child().is_some_and(|visible| visible == mail_home.clone().upcast::<gtk::Widget>()));
        if !mail_home_showing {
            // The extras are Mail-Home context; don't leave them visible if
            // the module/tab changed while the popover was open.
            self.extras_box.set_visible(false);
        }
        let has_overflow = self.overflow_box.first_child().is_some();
        self.overflow_box.set_visible(has_overflow);
        let has_extras = self.extras_box.is_visible() && self.extras_box.first_child().is_some();
        self.separator.set_visible(has_overflow && has_extras);
        // The button stays pinned in every module; it is insensitive only
        // when its menu would be completely empty. If a resize empties an
        // open menu, close it rather than leaving an empty popover.
        more_button.set_sensitive(has_overflow || mail_home_showing);
        if !has_overflow && !has_extras && more_button.popover().is_some() {
            more_button.popdown();
        }
    }
}

/// Restore one band's controls to their original rows, in their original
/// order. A group already in place stays put; only one with overflowed or
/// moved controls is rebuilt (see [`place_group`]).
fn restore_band(band: &RegisteredBand, mode: RibbonMode) {
    let Some(root) = band.root.upgrade() else {
        return;
    };
    let state = band.state.borrow();
    state.compact.set(false);
    for control in &state.controls {
        if control.in_overflow.replace(false) {
            if let Some(widget) = control.widget.upgrade() {
                widget.remove_css_class("ribbon-overflow-control");
            }
        }
    }
    apply_band(&root, &state, mode);
    sync_group_visibility(&state);
}

fn sync_group_visibility(state: &BandState) {
    for group in &state.groups {
        let any_visible = group.controls.iter().any(|index| {
            state
                .controls
                .get(*index)
                .is_some_and(|control| !control.in_overflow.get() && control.widget.upgrade().is_some())
        });
        if let Some(container) = group.container.as_ref().and_then(|container| container.upgrade()) {
            container.set_visible(any_visible);
        }
        if let Some(divider) = group.divider.as_ref().and_then(|divider| divider.upgrade()) {
            divider.set_visible(any_visible);
        }
        if group.container.is_none() {
            if let Some(row) = group.row.upgrade() {
                row.set_visible(any_visible);
            }
        }
    }
}

fn set_compact(band: &RegisteredBand, compact: bool, mode: RibbonMode) {
    let state = band.state.borrow();
    if state.compact.replace(compact) != compact {
        apply_control_styles(&state, mode);
    }
}

fn reorder_band(overflow_box: &gtk::Box, state: &BandState) {
    // `GtkBox` has no "move to index" in GTK4; re-appending in ribbon order is
    // the simplest way to put the flat overflow list back in order. Only the
    // overflowed controls are touched.
    for control in &state.controls {
        if !control.in_overflow.get() {
            continue;
        }
        if let Some(widget) = control.widget.upgrade() {
            if widget.parent().is_some() {
                widget.unparent();
            }
            overflow_box.append(&widget);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_codes_round_trip_and_default() {
        for mode in RibbonMode::ALL {
            assert_eq!(RibbonMode::from_code(mode.code()), mode);
        }
        assert_eq!(RibbonMode::from_code(" CLASSIC "), RibbonMode::Classic);
        assert_eq!(RibbonMode::from_code("nonsense"), RibbonMode::Simplified);
        assert_eq!(RibbonMode::default(), RibbonMode::Simplified);
    }

    #[test]
    fn modes_have_distinct_labels() {
        assert_eq!(RibbonMode::Simplified.label(), "Simplified Ribbon");
        assert_eq!(RibbonMode::Classic.label(), "Classic Ribbon");
    }

    #[test]
    fn index_matches_the_all_order() {
        for (i, mode) in RibbonMode::ALL.into_iter().enumerate() {
            assert_eq!(mode.index(), i as u32);
        }
    }

    #[test]
    fn a_band_dresses_controls_and_follows_the_mode() {
        // Self-skipping: only the thread that initialised GTK may touch
        // widgets (see `gtk_test::gtk_ready`).
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let style = RibbonStyle::new(RibbonMode::Simplified);
        let band = RibbonBand::new(&style, &["toolbar"]);
        let button = gtk::Button::new();
        let toggle = gtk::ToggleButton::new();
        let menu = gtk::MenuButton::new();
        let split = adw::SplitButton::new();
        // Every control kind dresses without panicking (the menu, split and
        // toggle variants are the ones whose `set_child` is the risky path).
        let icon = band.group("Delete", |group| {
            let icon = group.button(&button, "user-trash-symbolic", "Delete");
            group.toggle(&toggle, "folder-symbolic", "Folder pane");
            group.menu(&menu, "tag-symbolic", "Categorize");
            group.split(&split, "mail-forward-symbolic", "Forward");
            group.widget(&gtk::DropDown::from_strings(&["a", "b"]));
            icon
        });
        // A dynamic icon swaps its glyph without disturbing the layout.
        icon.set_icon_name("mail-reply-sender-symbolic");

        // Every control shape is flat and styled through the same class, so
        // the ribbon can't come out as a mix of filled and flat buttons: a
        // `set_child` custom child strips the `.image-button` class Adwaita's
        // `.toolbar` flattening rule keys off, which is why the ribbon adds
        // `.flat` itself.
        for control in [
            button.clone().upcast::<gtk::Widget>(),
            toggle.clone().upcast(),
            menu.clone().upcast(),
            split.clone().upcast(),
        ] {
            assert!(control.has_css_class("ribbon-control"), "{} is a ribbon control", control.css_name());
            assert!(control.has_css_class("flat"), "{} is flat", control.css_name());
        }

        let content = button.child().and_downcast::<gtk::Box>().expect("button wears an icon+label child");
        let group_box = band.root().first_child().and_downcast::<gtk::Box>().expect("the band holds a group box");
        // The group's box holds its row and then its name.
        let title = group_box.last_child().and_downcast::<gtk::Label>().expect("the group's name");
        assert_eq!(title.label(), "Delete");
        let row_orientation = |button: &gtk::Button| button.child().and_downcast::<gtk::Box>().expect("content").orientation();

        // Simplified: one row, label beside the icon, no group names.
        assert_eq!(row_orientation(&button), gtk::Orientation::Horizontal);
        assert!(!title.is_visible());

        // Classic: a large control's icon over its label, and the name shows.
        style.set_mode(RibbonMode::Classic);
        assert_eq!(row_orientation(&button), gtk::Orientation::Vertical);
        assert!(title.is_visible());

        // And back.
        style.set_mode(RibbonMode::Simplified);
        assert_eq!(row_orientation(&button), gtk::Orientation::Horizontal);
        assert!(!title.is_visible());
        assert!(content.is_visible());
    }

    #[test]
    fn classic_stacks_small_controls_in_columns_of_three() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let style = RibbonStyle::new(RibbonMode::Simplified);
        let band = RibbonBand::new(&style, &[]);
        let large = gtk::Button::new();
        let small: Vec<gtk::Button> = (0..4).map(|_| gtk::Button::new()).collect();
        band.group("Basic Text", |group| {
            group.button(&large, "mail-attachment-symbolic", "Attach");
            group.plain();
            for button in &small {
                group.small_button(button, "format-text-bold-symbolic", "Bold");
            }
        });
        let group_box = band.root().first_child().and_downcast::<gtk::Box>().expect("group box");
        let row = group_box.first_child().and_downcast::<gtk::Box>().expect("group row");
        let row_widget = row.clone().upcast::<gtk::Widget>();
        let label_of = |button: &gtk::Button| {
            button
                .child()
                .and_downcast::<gtk::Box>()
                .and_then(|content| content.last_child())
                .and_downcast::<gtk::Label>()
                .expect("label")
        };

        // Simplified: everything side by side, and a plain control's label is gone.
        assert!(small.iter().all(|button| button.parent().as_ref() == Some(&row_widget)));
        assert!(label_of(&large).is_visible());
        assert!(!label_of(&small[0]).is_visible());

        // Classic: the large control alone, then the small ones three to a
        // column, each with its label beside its icon.
        style.set_mode(RibbonMode::Classic);
        assert_eq!(large.parent().as_ref(), Some(&row_widget));
        let first = small[0].parent().expect("a column");
        assert!(first.has_css_class("ribbon-small-column"));
        assert_eq!(small[1].parent().as_ref(), Some(&first));
        assert_eq!(small[2].parent().as_ref(), Some(&first));
        let second = small[3].parent().expect("a second column");
        assert_ne!(second, first);
        assert_eq!(second.parent().as_ref(), Some(&row_widget));
        assert_eq!(children(&row_widget).len(), 3);
        assert!(label_of(&small[0]).is_visible());

        // Placing again changes nothing.
        apply_band(band.root(), &band.state.borrow(), RibbonMode::Classic);
        assert_eq!(small[0].parent().as_ref(), Some(&first));

        // A column emptied by overflow leaves no gap, and comes back on restore.
        let more_band = RibbonBand::new(&style, &[]);
        let more_button = gtk::MenuButton::new();
        more_band.standalone().menu(&more_button, "view-more-symbolic", "More");
        let overflow_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let extras_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        let overflow = RibbonOverflow::new(&style, &more_button, &overflow_box, &extras_box, &separator, &more_band);
        overflow.add_band(&band);
        let registered = RegisteredBand {
            root: band.root().downgrade(),
            state: band.state.clone(),
        };
        overflow.inner.move_control_to_overflow(&registered, 4);
        assert_eq!(children(&row_widget).len(), 2);
        assert!(label_of(&small[3]).is_visible(), "a More entry keeps its label");
        overflow.inner.restore_all();
        assert_eq!(children(&row_widget).len(), 3);
        assert!(small[3].parent().is_some_and(|parent| parent.has_css_class("ribbon-small-column")));

        // Simplified again: back in one row.
        style.set_mode(RibbonMode::Simplified);
        assert!(small.iter().all(|button| button.parent().as_ref() == Some(&row_widget)));
        assert_eq!(children(&row_widget).len(), 5);
    }

    #[test]
    fn compact_mode_drops_labels_and_overflow_restores_them() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let style = RibbonStyle::new(RibbonMode::Simplified);
        let band = RibbonBand::new(&style, &[]);
        let button = gtk::Button::new();
        band.group("Delete", |group| group.button(&button, "user-trash-symbolic", "Delete"));
        let label = button
            .child()
            .and_downcast::<gtk::Box>()
            .and_then(|content| content.last_child())
            .and_downcast::<gtk::Label>()
            .expect("label");

        // The last-resort width mode hides every label on the page.
        band.state.borrow().compact.set(true);
        apply_control_styles(&band.state.borrow(), RibbonMode::Simplified);
        assert!(!label.is_visible());
        band.state.borrow().compact.set(false);
        apply_control_styles(&band.state.borrow(), RibbonMode::Simplified);
        assert!(label.is_visible());

        // A control can be moved into the More container and put back in its
        // original row without losing its tag or handlers.
        let more_band = RibbonBand::new(&style, &[]);
        let more_button = gtk::MenuButton::new();
        more_band.standalone().menu(&more_button, "view-more-symbolic", "More");
        let overflow_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let extras_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        let overflow = RibbonOverflow::new(&style, &more_button, &overflow_box, &extras_box, &separator, &more_band);
        overflow.add_band(&band);
        let registered = RegisteredBand {
            root: band.root().downgrade(),
            state: band.state.clone(),
        };
        overflow.inner.move_control_to_overflow(&registered, 0);
        assert!(button.parent().is_some_and(|parent| parent == overflow_box.clone().upcast::<gtk::Widget>()));
        assert!(button.has_css_class("ribbon-overflow-control"));

        overflow.inner.restore_all();
        let group_box = band.root().first_child().and_downcast::<gtk::Box>().expect("group box");
        let row = group_box.first_child().and_downcast::<gtk::Box>().expect("group row");
        assert!(button.parent().is_some_and(|parent| parent == row.clone().upcast::<gtk::Widget>()));
        assert!(!button.has_css_class("ribbon-overflow-control"));
    }

    #[test]
    fn a_narrow_ribbon_overflows_layout_and_commands_into_more() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let style = RibbonStyle::new(RibbonMode::Simplified);
        let band = RibbonBand::new(&style, &[]);
        let button = gtk::Button::new();
        band.group("Delete", |group| group.button(&button, "user-trash-symbolic", "Delete"));
        let label = button
            .child()
            .and_downcast::<gtk::Box>()
            .and_then(|content| content.last_child())
            .and_downcast::<gtk::Label>()
            .expect("label");

        let layout_band = RibbonBand::new(&style, &[]);
        let layout_button = gtk::MenuButton::new();
        layout_band.standalone().menu(&layout_button, "view-grid-symbolic", "Layout");

        let more_band = RibbonBand::new(&style, &[]);
        let more_button = gtk::MenuButton::new();
        more_band.standalone().menu(&more_button, "view-more-symbolic", "More");

        let overflow_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let extras_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        let stack = gtk::Stack::new();
        stack.add_named(band.root(), Some("band"));
        stack.set_visible_child_name("band");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.append(&stack);
        bar.append(layout_band.root());
        bar.append(more_band.root());

        let overflow = RibbonOverflow::new(&style, &more_button, &overflow_box, &extras_box, &separator, &more_band);
        overflow.add_band(&band);
        overflow.set_layout_band(&layout_band);
        overflow.attach(&bar, &stack);

        // A width smaller than even the More button: Layout and the command
        // both move into More, and the command keeps its label as a menu row.
        overflow.inner.refit_to(80, &stack);
        assert!(button.parent().is_some_and(|parent| parent == overflow_box.clone().upcast::<gtk::Widget>()));
        assert!(!layout_band.root().is_visible());
        assert!(label.is_visible());
        assert!(more_button.is_sensitive());

        // Plenty of room restores the ribbon, the Layout button and labels.
        overflow.inner.refit_to(10_000, &stack);
        let group_box = band.root().first_child().and_downcast::<gtk::Box>().expect("group box");
        let row = group_box.first_child().and_downcast::<gtk::Box>().expect("group row");
        assert!(button.parent().is_some_and(|parent| parent == row.clone().upcast::<gtk::Widget>()));
        assert!(layout_band.root().is_visible());
        assert!(label.is_visible());
        assert!(!more_button.is_sensitive(), "the empty More menu is disabled on a page without extras");
    }
}
