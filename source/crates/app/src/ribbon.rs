//! The Outlook-style ribbon: groups of icon+label controls, laid out either
//! Simplified (one row, the label beside the icon) or Classic (the icon over
//! the label, with larger icons).
//!
//! The ribbon only presents widgets the caller has already built and wired.
//! [`RibbonGroup`]'s `button`/`toggle`/`menu`/`split` swap a control's child
//! for an icon+label box, so click handlers, shortcuts and sensitivity stay
//! exactly where they were; switching layout re-orients that same box, so
//! nothing is rebuilt and no handler is lost. [`RibbonIcon`] is the handle a
//! dynamic control (Pin, Flag, Read/Unread) uses to swap its glyph.

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

/// The handle to a dressed control's icon. A control created with a live glyph
/// (Pin, Flag, Read/Unread) takes one of these and calls [`RibbonIcon::set_icon_name`]
/// instead of reaching for the button, which would tear the icon+label child
/// out from under the layout. It also carries the button so a live tooltip can
/// follow the glyph.
#[derive(Clone)]
pub(crate) struct RibbonIcon {
    image: gtk::Image,
    button: glib::WeakRef<gtk::Widget>,
}

impl RibbonIcon {
    /// Swap the glyph, as the button's own `set_icon_name` used to.
    pub(crate) fn set_icon_name(&self, name: &str) {
        self.image.set_icon_name(Some(name));
    }

    /// Update the owning button's tooltip (Pin/Unpin, Mark Read/Unread).
    pub(crate) fn set_tooltip_text(&self, text: Option<&str>) {
        if let Some(button) = self.button.upgrade() {
            button.set_tooltip_text(text);
        }
    }
}

/// The shared ribbon state: the mode in force and every live band, so one
/// switch re-lays-out them all.
#[derive(Clone)]
pub(crate) struct RibbonStyle {
    mode: Rc<Cell<RibbonMode>>,
    bands: Rc<RefCell<Vec<BandRegistration>>>,
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
        }
    }

    pub(crate) fn mode(&self) -> RibbonMode {
        self.mode.get()
    }

    /// Switch every live band to `mode`. Bands whose root widget has gone
    /// (a composer that has closed) are dropped from the registry.
    pub(crate) fn set_mode(&self, mode: RibbonMode) {
        self.mode.set(mode);
        let mut bands = self.bands.borrow_mut();
        bands.retain(|band| {
            let Some(root) = band.root.upgrade() else {
                return false;
            };
            apply_band(&root, &band.state.borrow(), mode);
            true
        });
    }
}

#[derive(Default)]
struct BandState {
    controls: Vec<ControlParts>,
}

struct ControlParts {
    content: glib::WeakRef<gtk::Box>,
    image: glib::WeakRef<gtk::Image>,
    label: glib::WeakRef<gtk::Label>,
}

/// One band of controls (a module's/tab's ribbon row). Its [`RibbonGroup`]s
/// carry the divider between commands.
pub(crate) struct RibbonBand {
    style: RibbonStyle,
    root: gtk::Box,
    state: Rc<RefCell<BandState>>,
}

impl RibbonBand {
    /// A new band. `css_classes` are the extra style classes the row wants on
    /// top of `.ribbon` (e.g. `.toolbar` for the legacy look).
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
        let divider = gtk::Separator::new(gtk::Orientation::Vertical);
        divider.add_css_class("ribbon-divider");
        self.root.append(&group_box);
        self.root.append(&divider);
        let group = RibbonGroup {
            style: self.style.clone(),
            state: self.state.clone(),
            row,
        };
        build(&group)
    }

    /// A bare run of controls with no divider (the trailing
    /// overflow button), dressed just like a named group's.
    pub(crate) fn standalone(&self) -> RibbonGroup {
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
        row.add_css_class("ribbon-group-row");
        self.root.append(&row);
        RibbonGroup {
            style: self.style.clone(),
            state: self.state.clone(),
            row,
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
}

impl RibbonGroup {
    /// Dress a plain button with `icon` and `label`.
    pub(crate) fn button(&self, button: &gtk::Button, icon: &str, label: &str) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        self.register(button.clone().upcast(), content, image, text)
    }

    /// Dress a toggle button (a ribbon switch such as Folder pane).
    pub(crate) fn toggle(&self, button: &gtk::ToggleButton, icon: &str, label: &str) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        self.register(button.clone().upcast(), content, image, text)
    }

    /// Dress a menu button (a `▾` dropdown); the arrow is shown beside the
    /// content so a labelled menu button keeps its affordance.
    pub(crate) fn menu(&self, button: &gtk::MenuButton, icon: &str, label: &str) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        button.set_always_show_arrow(true);
        self.register(button.clone().upcast(), content, image, text)
    }

    /// Dress a libadwaita split button (a main action plus its `▾` menu).
    pub(crate) fn split(&self, button: &adw::SplitButton, icon: &str, label: &str) -> RibbonIcon {
        let (content, image, text) = build_content(icon, label);
        button.set_child(Some(&content));
        self.register(button.clone().upcast(), content, image, text)
    }

    /// Append a control that carries no icon+label of its own (a `DropDown`,
    /// a colour button, a spin button) to the group's row.
    pub(crate) fn widget(&self, widget: &impl IsA<gtk::Widget>) {
        self.row.append(widget);
    }

    fn register(&self, widget: gtk::Widget, content: gtk::Box, image: gtk::Image, label: gtk::Label) -> RibbonIcon {
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
        style_control(&content, &image, &label, self.style.mode());
        self.row.append(&widget);
        self.state.borrow_mut().controls.push(ControlParts {
            content: content.downgrade(),
            image: image.downgrade(),
            label: label.downgrade(),
        });
        RibbonIcon {
            image,
            button: widget.downgrade(),
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

fn style_control(content: &gtk::Box, image: &gtk::Image, label: &gtk::Label, mode: RibbonMode) {
    match mode {
        RibbonMode::Simplified => {
            content.set_orientation(gtk::Orientation::Horizontal);
            content.set_spacing(6);
            image.set_pixel_size(16);
            label.set_halign(gtk::Align::Start);
            content.remove_css_class("ribbon-classic-content");
        }
        RibbonMode::Classic => {
            content.set_orientation(gtk::Orientation::Vertical);
            content.set_spacing(3);
            image.set_pixel_size(22);
            label.set_halign(gtk::Align::Center);
            content.add_css_class("ribbon-classic-content");
        }
    }
}

fn apply_band(root: &gtk::Box, state: &BandState, mode: RibbonMode) {
    if mode == RibbonMode::Classic {
        root.add_css_class("ribbon-classic");
    } else {
        root.remove_css_class("ribbon-classic");
    }
    for control in &state.controls {
        if let (Some(content), Some(image), Some(label)) = (control.content.upgrade(), control.image.upgrade(), control.label.upgrade()) {
            style_control(&content, &image, &label, mode);
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
        // A group draws no caption in either mode: its box holds only the row.
        let only_row = |group_box: &gtk::Box| group_box.first_child().is_some() && group_box.first_child() == group_box.last_child();
        let row_orientation = |button: &gtk::Button| button.child().and_downcast::<gtk::Box>().expect("content").orientation();

        // Simplified: one row, label beside the icon.
        assert_eq!(row_orientation(&button), gtk::Orientation::Horizontal);
        assert!(only_row(&group_box));

        // Classic: icon over label, still no caption.
        style.set_mode(RibbonMode::Classic);
        assert_eq!(row_orientation(&button), gtk::Orientation::Vertical);
        assert!(only_row(&group_box));

        // And back.
        style.set_mode(RibbonMode::Simplified);
        assert_eq!(row_orientation(&button), gtk::Orientation::Horizontal);
        assert!(only_row(&group_box));
        assert!(content.is_visible());
    }
}
