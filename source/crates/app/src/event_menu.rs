//! The calendar event context menu (OUTLOOK_PARITY.md item 3.12 / CAL-31):
//! what a right-click on an event offers - in new Outlook's order - built on
//! the shared [`crate::context_menu`] builder.
//!
//! This module is the pure half: which entries the menu shows for the event
//! it acts on ([`EventMenuState`]), the [`MenuDescription`] for that state,
//! and the mapping from an activated item back to an [`EventMenuAction`].
//! `window.rs` owns the effects - every action runs through the same helpers
//! the editor uses, so a duplicate or a show-as change behaves (and prompts
//! for invitations) the same wherever it's invoked.
//!
//! Read-only feeds (webcal subscriptions, the synthetic birthdays calendar)
//! have no write-back path, so their events offer only Open and Save as .ics.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use gtk::glib;
use gtk::prelude::*;
use lookout_core::interop::BusyStatus;

use crate::context_menu::{ActionSpec, MenuDescription};

/// The action namespace the menu's group is inserted under.
pub const NAMESPACE: &str = "event";

/// The custom-widget slot holding the Categorize checklist.
pub const CATEGORIZE_SLOT: &str = "categorize";

/// The Show as ▸ entries, in Outlook's ("new Outlook") order: the OOF state
/// is labelled "Away" there. Kept in lockstep with the editor's own picker
/// (`event_editor::SHOW_AS_CHOICES`).
pub const SHOW_AS_CHOICES: [(BusyStatus, &str); 5] = [
    (BusyStatus::Free, "Free"),
    (BusyStatus::WorkingElsewhere, "Working elsewhere"),
    (BusyStatus::Tentative, "Tentative"),
    (BusyStatus::Busy, "Busy"),
    (BusyStatus::Oof, "Away"),
];

/// What a menu item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventMenuAction {
    /// Open the full event editor.
    Open,
    Delete,
    /// Clone the event into its own calendar under a fresh UID.
    Duplicate,
    /// Clone the event into another calendar the user picks.
    CopyToCalendar,
    /// Write the event (or its series) to a `.ics` file.
    SaveAsIcs,
    /// Set the event's show-as state.
    ShowAs(BusyStatus),
    /// Toggle the event's RFC 5545 `CLASS` between `Private` and `Public`.
    Private,
}

/// Which entries the menu offers for the event it acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMenuState {
    /// A read-only feed (webcal, birthdays): no write path at all.
    pub read_only: bool,
    /// At least one other writable calendar exists to copy into.
    pub has_writable_target: bool,
    /// The event is a recurring series (or an instance of one); the caller
    /// prompts for the edit scope on the inline edits.
    pub is_recurring: bool,
    /// The event's current show-as state, so Show as ▸ checks the right item.
    pub show_as: BusyStatus,
    /// The event is currently `Private`, so the Private item shows checked.
    pub private: bool,
}

/// Every action the menu's group carries.
pub fn action_specs() -> Vec<ActionSpec> {
    vec![
        ActionSpec::simple("open"),
        ActionSpec::simple("delete"),
        ActionSpec::simple("duplicate"),
        ActionSpec::simple("copy-calendar"),
        ActionSpec::simple("save-ics"),
        ActionSpec::stateful("show-as", glib::VariantTy::STRING, &BusyStatus::Busy.as_cdo().to_variant()),
        ActionSpec::stateful("private", glib::VariantTy::BOOLEAN, &false.to_variant()),
    ]
}

/// The bare names of the actions the menu greys out by state, with whether
/// each is enabled in `state`.
pub fn enabled_actions(state: &EventMenuState) -> Vec<(&'static str, bool)> {
    vec![
        ("open", true),
        ("delete", !state.read_only),
        ("duplicate", !state.read_only),
        ("copy-calendar", !state.read_only && state.has_writable_target),
        ("save-ics", true),
        ("show-as", !state.read_only),
        ("private", !state.read_only),
    ]
}

fn ns(name: &str) -> String {
    format!("{NAMESPACE}.{name}")
}

/// The Show as ▸ submenu: the five states as radio items.
fn show_as_submenu() -> MenuDescription {
    let mut menu = MenuDescription::new().section();
    for (status, label) in SHOW_AS_CHOICES {
        menu = menu.action_with_target(label, ns("show-as"), status.as_cdo().to_variant());
    }
    menu
}

/// The menu for `state`, in Outlook's order (CAL-31): Open · Delete,
/// Duplicate, Copy to calendar…, Save as .ics… · Categorize ▸, Show as ▸,
/// Private. Read-only feeds keep only Open and Save as .ics.
pub fn event_menu_description(state: &EventMenuState) -> MenuDescription {
    let mut menu = MenuDescription::new().section().action("Open", ns("open"));
    if !state.read_only {
        menu = menu
            .section()
            .action("Delete", ns("delete"))
            .action("Duplicate", ns("duplicate"))
            .action("Copy to calendar…", ns("copy-calendar"));
    }
    menu = menu.section().action("Save as .ics…", ns("save-ics"));
    if !state.read_only {
        menu = menu
            .section()
            .submenu("Categorize", MenuDescription::new().section().custom(CATEGORIZE_SLOT))
            .submenu("Show as", show_as_submenu())
            // A single stateful item whose target is `true`: checked while the
            // event is Private, unchecked otherwise (the caller sets the state
            // from the event before popping the menu, and toggles on activate).
            .action_with_target("Private", ns("private"), true.to_variant());
    }
    menu
}

/// Maps an activated action's bare name and parameter back to what it asks
/// for. `None` for an unknown name or a malformed parameter.
pub fn event_action_for(name: &str, param: Option<&glib::Variant>) -> Option<EventMenuAction> {
    Some(match name {
        "open" => EventMenuAction::Open,
        "delete" => EventMenuAction::Delete,
        "duplicate" => EventMenuAction::Duplicate,
        "copy-calendar" => EventMenuAction::CopyToCalendar,
        "save-ics" => EventMenuAction::SaveAsIcs,
        "show-as" => EventMenuAction::ShowAs(BusyStatus::from_cdo(&param?.get::<String>()?)?),
        // The parameter is the check item's target (`true`); the desired new
        // state is the opposite of the event's current one, so `window.rs`
        // toggles from the occurrence rather than trusting the value.
        "private" => EventMenuAction::Private,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_menu::MenuItem;

    fn state() -> EventMenuState {
        EventMenuState {
            read_only: false,
            has_writable_target: true,
            is_recurring: false,
            show_as: BusyStatus::Busy,
            private: false,
        }
    }

    /// The labels of a description, section by section, with submenus as
    /// "Label ▸" and custom slots as "[id]".
    fn labels(menu: &MenuDescription) -> Vec<Vec<String>> {
        menu.sections_for_test()
            .iter()
            .map(|section| {
                section
                    .iter()
                    .map(|item| match item {
                        MenuItem::Action(action) => action.label.clone(),
                        MenuItem::Submenu { label, .. } => format!("{label} ▸"),
                        MenuItem::Custom(id) => format!("[{id}]"),
                    })
                    .collect()
            })
            .collect()
    }

    fn submenu<'a>(menu: &'a MenuDescription, name: &str) -> &'a MenuDescription {
        menu.sections_for_test()
            .iter()
            .flatten()
            .find_map(|item| match item {
                MenuItem::Submenu { label, menu } if label == name => Some(menu),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {name} submenu"))
    }

    #[test]
    fn the_menu_follows_outlooks_order() {
        let menu = event_menu_description(&state());
        assert_eq!(
            labels(&menu),
            vec![
                vec!["Open"],
                vec!["Delete", "Duplicate", "Copy to calendar…"],
                vec!["Save as .ics…"],
                vec!["Categorize ▸", "Show as ▸", "Private"],
            ]
        );
        assert_eq!(labels(submenu(&menu, "Categorize")), vec![vec![format!("[{CATEGORIZE_SLOT}]")]]);
        assert_eq!(labels(submenu(&menu, "Show as")), vec![vec!["Free", "Working elsewhere", "Tentative", "Busy", "Away"]]);
    }

    #[test]
    fn read_only_feeds_keep_only_open_and_save() {
        let read_only = EventMenuState { read_only: true, ..state() };
        let menu = event_menu_description(&read_only);
        assert_eq!(labels(&menu), vec![vec!["Open"], vec!["Save as .ics…"]]);
        let enabled: std::collections::HashMap<_, _> = enabled_actions(&read_only).into_iter().collect();
        assert!(enabled["open"] && enabled["save-ics"]);
        assert!(!enabled["delete"] && !enabled["duplicate"] && !enabled["copy-calendar"]);
        assert!(!enabled["show-as"] && !enabled["private"]);
    }

    #[test]
    fn copy_needs_a_writable_target() {
        let no_target = EventMenuState {
            has_writable_target: false,
            ..state()
        };
        let enabled: std::collections::HashMap<_, _> = enabled_actions(&no_target).into_iter().collect();
        assert!(!enabled["copy-calendar"], "no second calendar to copy into");
        assert!(enabled["duplicate"], "duplicate stays in the event's own calendar");
    }

    #[test]
    fn enabled_names_are_all_declared() {
        let enabled: std::collections::HashMap<_, _> = enabled_actions(&state()).into_iter().collect();
        assert!(enabled.values().all(|v| *v));
        let declared: Vec<&str> = action_specs().iter().map(|spec| spec.name).collect();
        for (name, _) in enabled_actions(&state()) {
            assert!(declared.contains(&name), "{name} isn't declared");
        }
    }

    #[test]
    fn action_names_map_and_bad_parameters_are_refused() {
        for (name, expected) in [
            ("open", EventMenuAction::Open),
            ("delete", EventMenuAction::Delete),
            ("duplicate", EventMenuAction::Duplicate),
            ("copy-calendar", EventMenuAction::CopyToCalendar),
            ("save-ics", EventMenuAction::SaveAsIcs),
            ("private", EventMenuAction::Private),
        ] {
            assert_eq!(event_action_for(name, None), Some(expected), "{name}");
        }
        assert_eq!(
            event_action_for("show-as", Some(&BusyStatus::Oof.as_cdo().to_variant())),
            Some(EventMenuAction::ShowAs(BusyStatus::Oof))
        );
        assert_eq!(event_action_for("show-as", Some(&"nonsense".to_variant())), None);
        assert_eq!(event_action_for("show-as", None), None);
        assert_eq!(event_action_for("ignore", None), None);
    }

    /// The description builds into a real popover whose Categorize submenu has
    /// the custom slot, and every declared action resolves in its group.
    #[test]
    fn the_menu_builds_with_its_categorize_slot() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let dispatch: crate::context_menu::Dispatch = std::rc::Rc::new(|_, _| {});
        let menu = crate::context_menu::ContextMenu::new(&event_menu_description(&state()), &action_specs(), dispatch);
        assert!(menu.popover.add_child(&gtk::Label::new(Some("tags")), CATEGORIZE_SLOT), "the Categorize slot exists");
        for spec in action_specs() {
            assert!(menu.actions.lookup_action(spec.name).is_some(), "{} is in the group", spec.name);
        }
    }
}
