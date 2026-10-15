//! A reusable context-menu builder (OUTLOOK_PARITY.md item 0.5), shared by the
//! folder, message, event and contact/task menus.
//!
//! It generalises the pattern `folder_menu.rs` established: a [`gio::Menu`]
//! built from a declarative [`MenuDescription`], backed by a
//! [`gio::SimpleActionGroup`] whose every action calls one [`Dispatch`]
//! closure, presented through a [`gtk::PopoverMenu`] (usually hung off a
//! [`gtk::MenuButton`]). A [`ContextMenu`] also carries the per-item refresh
//! (`set_enabled`, `set_state`) and opaque widget slots (`add_child`) that
//! larger menus need, so a caller never has to reach into raw `gio` action
//! plumbing and every menu in the app is built the same way.
//!
//! The action enum and the effect of each action live with the caller (e.g.
//! [`crate::folder_menu::FolderMenuAction`]): this module only knows names and
//! parameter values, which keeps it usable for messages, events, contacts and
//! tasks alike.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

/// The callback every built action routes through: the action's bare name (as
/// declared in its [`ActionSpec`]) and its activation parameter (the menu
/// item's target value), if any.
pub type Dispatch = Rc<dyn Fn(&str, Option<glib::Variant>)>;

/// How a disabled item is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisabledWhen {
    /// Grey the item out (the default).
    #[default]
    Grey,
    /// Hide it entirely (`hidden-when = action-disabled`), for the two halves
    /// of an on/off toggle that should read as a single item.
    Hidden,
}

/// One actionable entry in a menu.
#[derive(Debug, Clone)]
pub struct MenuAction {
    pub label: String,
    /// The namespaced action name, e.g. `"folder.rename"`.
    pub action: String,
    /// The action's target value, for parameterised or stateful (radio)
    /// actions - `None` for a plain action.
    pub target: Option<glib::Variant>,
    /// A themed icon name shown before the label, if any.
    pub icon: Option<String>,
    pub disabled_when: DisabledWhen,
}

impl MenuAction {
    pub fn new(label: impl Into<String>, action: impl Into<String>) -> Self {
        MenuAction {
            label: label.into(),
            action: action.into(),
            target: None,
            icon: None,
            disabled_when: DisabledWhen::Grey,
        }
    }

    pub fn target(mut self, target: impl Into<glib::Variant>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// A themed icon shown before the label. Used by the message and event
    /// menus (items 1.17/3.12); the folder menu keeps plain labels.
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Hide (rather than grey) the item while its action is disabled.
    pub fn hidden_when_disabled(mut self) -> Self {
        self.disabled_when = DisabledWhen::Hidden;
        self
    }
}

/// One entry in a menu.
#[derive(Debug, Clone)]
pub enum MenuItem {
    Action(MenuAction),
    Submenu {
        label: String,
        menu: MenuDescription,
    },
    /// A placeholder for a widget registered with [`ContextMenu::add_child`],
    /// matched by the `custom` id.
    Custom(String),
}

/// A menu (or submenu): an ordered list of sections, each a list of items.
/// Each section renders as a separated group, matching `gio::Menu`'s own
/// section model (so "empty" sections are simply omitted).
#[derive(Debug, Clone, Default)]
pub struct MenuDescription {
    sections: Vec<Vec<MenuItem>>,
}

impl MenuDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new section. Returns `self` so a description reads as a chain.
    pub fn section(mut self) -> Self {
        self.sections.push(Vec::new());
        self
    }

    pub fn item(mut self, item: MenuItem) -> Self {
        if self.sections.is_empty() {
            self.sections.push(Vec::new());
        }
        self.sections.last_mut().expect("a section was just ensured").push(item);
        self
    }

    pub fn action(self, label: impl Into<String>, action: impl Into<String>) -> Self {
        self.item(MenuItem::Action(MenuAction::new(label, action)))
    }

    /// An action item carrying a themed icon before its label.
    pub fn icon_action(self, label: impl Into<String>, action: impl Into<String>, icon: impl Into<String>) -> Self {
        self.item(MenuItem::Action(MenuAction::new(label, action).icon(icon)))
    }

    /// [`MenuDescription::icon_action`] for a parameterised or stateful item.
    pub fn icon_action_with_target(self, label: impl Into<String>, action: impl Into<String>, target: impl Into<glib::Variant>, icon: impl Into<String>) -> Self {
        self.item(MenuItem::Action(MenuAction::new(label, action).target(target).icon(icon)))
    }

    /// An action item carrying a target value (a parameterised or radio item).
    pub fn action_with_target(self, label: impl Into<String>, action: impl Into<String>, target: impl Into<glib::Variant>) -> Self {
        self.item(MenuItem::Action(MenuAction::new(label, action).target(target)))
    }

    /// An action item hidden (not greyed) while disabled - one half of a
    /// toggle pair.
    pub fn hidden_action(self, label: impl Into<String>, action: impl Into<String>) -> Self {
        self.item(MenuItem::Action(MenuAction::new(label, action).hidden_when_disabled()))
    }

    pub fn submenu(self, label: impl Into<String>, menu: MenuDescription) -> Self {
        self.item(MenuItem::Submenu { label: label.into(), menu })
    }

    pub fn custom(self, id: impl Into<String>) -> Self {
        self.item(MenuItem::Custom(id.into()))
    }

    /// The description's sections, for tests that assert the built shape
    /// without reaching into `GMenuModel` internals.
    #[cfg(test)]
    pub(crate) fn sections_for_test(&self) -> &[Vec<MenuItem>] {
        &self.sections
    }
}

/// One action a menu's [`gio::SimpleActionGroup`] should carry.
#[derive(Debug, Clone)]
pub struct ActionSpec {
    /// The bare name within the menu's namespace, e.g. `"color"`.
    pub name: &'static str,
    /// `Some(…)` for a parameterised or stateful action, `None` otherwise.
    pub parameter_type: Option<&'static glib::VariantTy>,
    /// Initial state for a stateful (radio) action; `None` for a plain one.
    pub state: Option<glib::Variant>,
}

impl ActionSpec {
    /// A plain, parameterless action.
    pub fn simple(name: &'static str) -> Self {
        ActionSpec {
            name,
            parameter_type: None,
            state: None,
        }
    }

    /// An action that takes a parameter but holds no state.
    pub fn parameterised(name: &'static str, parameter_type: &'static glib::VariantTy) -> Self {
        ActionSpec {
            name,
            parameter_type: Some(parameter_type),
            state: None,
        }
    }

    /// A stateful action, rendered as a radio group when referenced with a
    /// target value.
    pub fn stateful(name: &'static str, parameter_type: &'static glib::VariantTy, initial: &glib::Variant) -> Self {
        ActionSpec {
            name,
            parameter_type: Some(parameter_type),
            state: Some(initial.clone()),
        }
    }
}

/// Builds a [`gio::SimpleActionGroup`] whose every action calls
/// `dispatch(name, parameter)`.
pub fn build_action_group(specs: &[ActionSpec], dispatch: Dispatch) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    for spec in specs {
        let action = match &spec.state {
            Some(initial) => gio::SimpleAction::new_stateful(spec.name, spec.parameter_type, initial),
            None => gio::SimpleAction::new(spec.name, spec.parameter_type),
        };
        let dispatch = dispatch.clone();
        let name = spec.name;
        action.connect_activate(move |_, param| dispatch(name, param.cloned()));
        group.add_action(&action);
    }
    group
}

/// Builds a [`gio::Menu`] from a description.
pub fn build_menu(description: &MenuDescription) -> gio::Menu {
    let menu = gio::Menu::new();
    for section in &description.sections {
        let group = gio::Menu::new();
        for item in section {
            match item {
                MenuItem::Action(action) => group.append_item(&action_item(action)),
                MenuItem::Submenu { label, menu: submenu } => group.append_submenu(Some(label), &build_menu(submenu)),
                MenuItem::Custom(id) => group.append_item(&custom_item(id)),
            }
        }
        menu.append_section(None, &group);
    }
    menu
}

fn action_item(action: &MenuAction) -> gio::MenuItem {
    let item = gio::MenuItem::new(None, None);
    item.set_label(Some(&action.label));
    item.set_action_and_target_value(Some(&action.action), action.target.as_ref());
    if let Some(icon) = &action.icon {
        item.set_icon(&gio::ThemedIcon::new(icon));
    }
    if action.disabled_when == DisabledWhen::Hidden {
        item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    }
    item
}

fn custom_item(id: &str) -> gio::MenuItem {
    let item = gio::MenuItem::new(None, None);
    item.set_attribute_value("custom", Some(&id.to_variant()));
    item
}

/// A built menu: the popover, its action group, and the small refresh surface
/// menus need. Cheap to clone (both fields are `GObject`s).
#[derive(Clone)]
pub struct ContextMenu {
    pub popover: gtk::PopoverMenu,
    pub actions: gio::SimpleActionGroup,
}

impl ContextMenu {
    /// Builds the popover and action group from a description and its action
    /// specs. The group is not inserted anywhere yet - `attach_button` (or the
    /// caller, for a manually-popped menu) does that with the namespace.
    pub fn new(description: &MenuDescription, specs: &[ActionSpec], dispatch: Dispatch) -> Self {
        let popover = gtk::PopoverMenu::from_model(Some(&build_menu(description)));
        let actions = build_action_group(specs, dispatch);
        ContextMenu { popover, actions }
    }

    /// Attaches the popover to a `MenuButton` and inserts the action group
    /// under `namespace`, so items resolve `namespace.*` through the button.
    pub fn attach_button(&self, button: &gtk::MenuButton, namespace: &str) {
        button.set_popover(Some(&self.popover));
        button.insert_action_group(namespace, Some(&self.actions));
    }

    /// Inserts the action group on an arbitrary widget - the manual-popover
    /// path (a message row's right-click, item 1.17).
    pub fn insert_action_group(&self, widget: &impl IsA<gtk::Widget>, namespace: &str) {
        widget.insert_action_group(namespace, Some(&self.actions));
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_enabled(enabled);
        }
    }

    pub fn set_state(&self, name: &str, value: &glib::Variant) {
        if let Some(action) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_state(value);
        }
    }

    /// Registers a widget for a [`MenuItem::Custom`] id.
    pub fn add_child(&self, widget: &impl IsA<gtk::Widget>, id: &str) {
        self.popover.add_child(widget, id);
    }

    pub fn popdown(&self) {
        self.popover.popdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure `gio` construction (no `gtk::init`), so no `gtk_ready` gate.
    #[test]
    fn action_group_carries_every_declared_spec() {
        let specs = [
            ActionSpec::simple("create"),
            ActionSpec::parameterised("color", glib::VariantTy::STRING),
            ActionSpec::stateful("count", glib::VariantTy::STRING, &"unread".to_variant()),
        ];
        let dispatch: Dispatch = Rc::new(|_, _| {});
        let group = build_action_group(&specs, dispatch);

        let simple = group.lookup_action("create").and_downcast::<gio::SimpleAction>().expect("create is a SimpleAction");
        assert_eq!(simple.parameter_type(), None);
        assert!(simple.state().is_none(), "a plain action has no state");
        assert!(simple.is_enabled(), "actions start enabled");

        let color = group.lookup_action("color").and_downcast::<gio::SimpleAction>().expect("color is a SimpleAction");
        assert!(color.parameter_type().is_some(), "color takes a parameter");

        let count = group.lookup_action("count").and_downcast::<gio::SimpleAction>().expect("count is a SimpleAction");
        assert_eq!(count.state().and_then(|s| s.get::<String>()), Some("unread".to_string()));
    }

    #[test]
    fn dispatch_receives_the_action_name_and_parameter() {
        type Seen = Rc<std::cell::RefCell<Vec<(String, Option<String>)>>>;
        let specs = [ActionSpec::simple("delete"), ActionSpec::parameterised("color", glib::VariantTy::STRING)];
        let seen: Seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let dispatch: Dispatch = {
            let seen = seen.clone();
            Rc::new(move |name, param| seen.borrow_mut().push((name.to_string(), param.and_then(|p| p.get::<String>()))))
        };
        let group = build_action_group(&specs, dispatch);

        group.lookup_action("delete").and_downcast::<gio::SimpleAction>().unwrap().activate(None);
        group
            .lookup_action("color")
            .and_downcast::<gio::SimpleAction>()
            .unwrap()
            .activate(Some(&"#ff0000".to_variant()));

        assert_eq!(
            seen.borrow().as_slice(),
            &[("delete".to_string(), None), ("color".to_string(), Some("#ff0000".to_string()))]
        );
    }

    /// The description chains sections and items in order, and the toggle
    /// helper marks its item hidden-when-disabled.
    #[test]
    fn menu_description_chains_sections_and_items() {
        let description = MenuDescription::new()
            .section()
            .action("Rename", "folder.rename")
            .action_with_target("No color", "folder.color", "".to_variant())
            .hidden_action("Add to Favorites", "folder.favorite-add")
            .submenu("Change color", MenuDescription::new().section().custom("colors"))
            .section()
            .action("Delete", "folder.delete");

        let sections = description.sections_for_test();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].len(), 4);
        match &sections[0][2] {
            MenuItem::Action(action) => {
                assert_eq!(action.label, "Add to Favorites");
                assert_eq!(action.disabled_when, DisabledWhen::Hidden);
            }
            other => panic!("expected an action, got {other:?}"),
        }
        match &sections[0][3] {
            MenuItem::Submenu { label, menu } => {
                assert_eq!(label, "Change color");
                assert_eq!(menu.sections_for_test().len(), 1);
            }
            other => panic!("expected a submenu, got {other:?}"),
        }
        assert_eq!(sections[1].len(), 1);
    }
}
