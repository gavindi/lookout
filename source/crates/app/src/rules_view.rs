//! The Rules manager and rule editor (Config → Mail → Rules, item 2.5 /
//! M-14).
//!
//! The manager lists the user's rules in order (an enable switch, the
//! condition/action summary, the backend each rule runs on, reorder up/down,
//! edit and delete), and the editor builds one rule: a name, its conditions,
//! its exceptions, its actions, and where it runs. Both write through to
//! `settings.json` (`AppConfig::rules`) via [`save_rules`], the same pattern
//! the categories manager uses.
//!
//! The editor is deliberately compact: a rule's target folder is named in a
//! text field - a well-known role ("Archive", "Junk Email", …) or a folder
//! name - and parsed by [`parse_target`]; categories are a comma-separated
//! list of names from the shared master list (item 2.1).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{EmailAddress, FollowUpFlag, Importance, MailboxRole, Rule, RuleAction, RuleBackend, RuleCondition, RuleTarget, SizeComparison};

use crate::util::{clear_box, labeled_row};
use crate::window::UiState;

/// Persists a rule list into `settings.json` (`AppConfig::rules`).
pub(crate) fn save_rules(state: &Rc<RefCell<UiState>>, rules: Vec<Rule>) {
    let app_config = state.borrow().app_config.clone();
    app_config.borrow_mut().rules = rules;
    crate::app_config::save(&app_config.borrow());
    // A changed rule set may alter what the server should run (item 2.5).
    crate::window::push_sieve_script(state);
}

/// The label for a rule's backend, as the manager shows it ("runs on").
fn backend_label(backend: RuleBackend) -> &'static str {
    match backend {
        RuleBackend::Client => "This device",
        RuleBackend::Sieve => "Server (Sieve)",
        RuleBackend::Graph => "Microsoft 365",
    }
}

/// A one-line human summary of a rule's conditions and actions.
fn rule_summary(rule: &Rule) -> String {
    let conditions = if rule.conditions.is_empty() {
        "all messages".to_string()
    } else {
        rule.conditions.iter().map(RuleCondition::label).collect::<Vec<_>>().join(" and ")
    };
    let excepts = if rule.exceptions.is_empty() {
        String::new()
    } else {
        format!(" (except {})", rule.exceptions.iter().map(RuleCondition::label).collect::<Vec<_>>().join(" or "))
    };
    let actions = if rule.actions.is_empty() {
        "no actions".to_string()
    } else {
        rule.actions.iter().map(RuleAction::label).collect::<Vec<_>>().join(", ")
    };
    format!("When {conditions}{excepts}: {actions}")
}

/// A new rule id: a UUID string, stable across edits.
fn new_rule_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The well-known role a folder name refers to, or `None` for a real folder.
fn role_from_label(name: &str) -> Option<MailboxRole> {
    match name.trim().to_ascii_lowercase().as_str() {
        "inbox" => Some(MailboxRole::Inbox),
        "sent" | "sent items" => Some(MailboxRole::Sent),
        "drafts" => Some(MailboxRole::Drafts),
        "deleted items" | "trash" | "deleted" => Some(MailboxRole::Trash),
        "junk email" | "junk" | "spam" => Some(MailboxRole::Junk),
        "archive" => Some(MailboxRole::Archive),
        _ => None,
    }
}

/// The role name for a target's text field.
fn role_label(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "Inbox",
        MailboxRole::Sent => "Sent Items",
        MailboxRole::Drafts => "Drafts",
        MailboxRole::Trash => "Deleted Items",
        MailboxRole::Junk => "Junk Email",
        MailboxRole::Archive => "Archive",
        MailboxRole::Custom => "Folder",
    }
}

/// Parses a target's text field: a well-known role name, else a folder name.
pub(crate) fn parse_target(text: &str) -> RuleTarget {
    match role_from_label(text) {
        Some(role) => RuleTarget::Role(role),
        None => RuleTarget::Folder(text.trim().to_string()),
    }
}

/// The text a target renders as.
fn target_text(target: &RuleTarget) -> String {
    match target {
        RuleTarget::Role(role) => role_label(*role).to_string(),
        RuleTarget::Folder(name) => name.clone(),
    }
}

/// Parses a comma- or semicolon-separated address list.
fn parse_addresses(text: &str) -> Vec<EmailAddress> {
    text.split([',', ';']).map(str::trim).filter(|entry| !entry.is_empty()).map(EmailAddress::new).collect()
}

/// The default condition for each kind label, in dropdown order.
fn condition_kinds() -> Vec<(&'static str, RuleCondition)> {
    vec![
        ("From contains", RuleCondition::From(String::new())),
        ("To contains", RuleCondition::To(String::new())),
        ("Cc contains", RuleCondition::Cc(String::new())),
        ("Subject contains", RuleCondition::Subject(String::new())),
        ("Body contains", RuleCondition::Body(String::new())),
        ("Mailing list contains", RuleCondition::ListId(String::new())),
        (
            "Header contains",
            RuleCondition::Header {
                name: String::new(),
                value: String::new(),
            },
        ),
        ("Has an attachment", RuleCondition::HasAttachment(true)),
        ("Importance is", RuleCondition::Importance(Importance::High)),
        (
            "Size is",
            RuleCondition::Size {
                comparison: SizeComparison::GreaterThan,
                bytes: 1024 * 1024,
            },
        ),
        ("Sent only to me", RuleCondition::SentOnlyToMe),
    ]
}

/// The dropdown index for a condition's kind.
fn condition_kind_index(condition: &RuleCondition) -> u32 {
    condition_kinds()
        .iter()
        .position(|(_, sample)| std::mem::discriminant(sample) == std::mem::discriminant(condition))
        .unwrap_or(0) as u32
}

/// The default action for each kind label, in dropdown order.
pub(crate) fn action_kinds() -> Vec<(&'static str, RuleAction)> {
    vec![
        ("Move to", RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))),
        ("Copy to", RuleAction::Copy(RuleTarget::Role(MailboxRole::Archive))),
        ("Delete", RuleAction::Delete),
        (
            "Flag for follow-up",
            RuleAction::Flag(FollowUpFlag {
                status: lookout_core::FlagStatus::Flagged,
                ..FollowUpFlag::default()
            }),
        ),
        ("Categorize as", RuleAction::Categorize(Vec::new())),
        ("Mark as read", RuleAction::MarkRead(true)),
        ("Forward to", RuleAction::Forward(Vec::new())),
        ("Redirect to", RuleAction::Redirect(Vec::new())),
        ("Stop processing more rules", RuleAction::StopProcessing),
    ]
}

/// The dropdown index for an action's kind.
fn action_kind_index(action: &RuleAction) -> u32 {
    action_kinds()
        .iter()
        .position(|(_, sample)| std::mem::discriminant(sample) == std::mem::discriminant(action))
        .unwrap_or(0) as u32
}

/// Presents the modal Rules manager, anchored to `anchor`'s window.
pub(crate) fn show_manage_rules_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Rules").default_width(640).default_height(560);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("Rules run on incoming mail while Lookout is open. Where a rule can run on the server - a Sieve server, or Microsoft 365 - it keeps working when Lookout is closed.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let groups: Rc<RefCell<Vec<adw::PreferencesGroup>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let state = state.clone();
        let page = page.clone();
        let rebuild_handle = rebuild.clone();
        let groups = groups.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            for group in groups.borrow().iter() {
                page.remove(group);
            }
            groups.borrow_mut().clear();

            let rules: Vec<Rule> = state.borrow().app_config.borrow().rules.clone();
            let group = adw::PreferencesGroup::builder().title("Rules").build();
            if rules.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No rules yet. Create one here, or from a message's Rules ▸ Create rule.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                group.add(&empty);
            }
            for (index, rule) in rules.iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&rule.name))
                    .subtitle(glib::markup_escape_text(&format!("{} · {}", backend_label(rule.backend), rule_summary(rule))))
                    .build();
                row.set_activatable(false);
                let suffix = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).valign(gtk::Align::Center).build();

                let enabled = gtk::Switch::new();
                enabled.set_active(rule.enabled);
                enabled.set_valign(gtk::Align::Center);
                enabled.set_tooltip_text(Some("Enable this rule"));
                enabled.connect_state_set({
                    let state = state.clone();
                    let id = rule.id.clone();
                    move |_, active| {
                        let mut rules = state.borrow().app_config.borrow().rules.clone();
                        if let Some(rule) = rules.iter_mut().find(|r| r.id == id) {
                            rule.enabled = active;
                        }
                        save_rules(&state, rules);
                        glib::Propagation::Proceed
                    }
                });
                suffix.append(&enabled);

                for (icon, tooltip, enabled_now, delta) in [
                    ("go-up-symbolic", "Move up", index > 0, -1i32),
                    ("go-down-symbolic", "Move down", index + 1 < rules.len(), 1),
                ] {
                    let button = gtk::Button::from_icon_name(icon);
                    button.add_css_class("flat");
                    button.set_sensitive(enabled_now);
                    button.set_tooltip_text(Some(tooltip));
                    button.connect_clicked({
                        let state = state.clone();
                        let rebuild_handle = rebuild_handle.clone();
                        move |_| {
                            let target = index as i32 + delta;
                            if target >= 0 {
                                let app_config = state.borrow().app_config.clone();
                                app_config.borrow_mut().move_rule(index, target as usize);
                                crate::app_config::save(&app_config.borrow());
                                crate::window::push_sieve_script(&state);
                            }
                            (rebuild_handle.borrow())();
                        }
                    });
                    suffix.append(&button);
                }

                let edit = gtk::Button::from_icon_name("document-edit-symbolic");
                edit.add_css_class("flat");
                edit.set_tooltip_text(Some("Edit this rule"));
                edit.connect_clicked({
                    let state = state.clone();
                    let rule = rule.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move |button| {
                        let (state, rule) = (state.clone(), rule.clone());
                        let rebuild_handle = rebuild_handle.clone();
                        show_rule_editor(button.upcast_ref::<gtk::Widget>(), state, rule, move || (rebuild_handle.borrow())());
                    }
                });
                suffix.append(&edit);

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.add_css_class("flat");
                delete.set_tooltip_text(Some("Delete this rule"));
                delete.connect_clicked({
                    let state = state.clone();
                    let id = rule.id.clone();
                    let backend = rule.backend;
                    let account = rule.account.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move |_| {
                        // Remove the Outlook copy too, when this rule lives there.
                        if backend == RuleBackend::Graph {
                            if let Some(account) = &account {
                                crate::window::delete_graph_rule(&state, account, id.clone());
                            }
                        }
                        let app_config = state.borrow().app_config.clone();
                        app_config.borrow_mut().remove_rule(&id);
                        crate::app_config::save(&app_config.borrow());
                        crate::window::push_sieve_script(&state);
                        (rebuild_handle.borrow())();
                    }
                });
                suffix.append(&delete);
                row.add_suffix(&suffix);
                group.add(&row);
            }

            let new_row = adw::ActionRow::builder().title("New rule…").build();
            new_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            new_row.set_activatable(true);
            new_row.connect_activated({
                let state = state.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |row| {
                    let (state, rebuild_handle) = (state.clone(), rebuild_handle.clone());
                    let rule = Rule::new(new_rule_id(), "New rule");
                    show_rule_editor(row.upcast_ref::<gtk::Widget>(), state, rule, move || (rebuild_handle.borrow())());
                }
            });
            group.add(&new_row);
            page.add(&group);
            groups.borrow_mut().push(group);
        });
    }
    (rebuild.borrow())();

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&hint);
    root.append(&gtk::ScrolledWindow::builder().vexpand(true).child(&page).build());
    let close_button = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).margin_top(12).build();
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    root.append(&close_button);
    dialog.set_child(Some(&root));
    dialog.present();
}

/// Presents the rule editor for `rule`. `on_saved` runs after the rule is
/// written, so the manager can repaint.
pub(crate) fn show_rule_editor(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, rule: Rule, on_saved: impl Fn() + 'static) {
    let rule = Rc::new(RefCell::new(rule));
    let on_saved = Rc::new(on_saved);

    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Rule").default_width(620).default_height(640);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let name_entry = gtk::Entry::builder().text(rule.borrow().name.clone()).placeholder_text("Rule name").hexpand(true).build();
    name_entry.connect_changed({
        let rule = rule.clone();
        move |entry| rule.borrow_mut().name = entry.text().to_string()
    });
    root.append(&labeled_row("Name", &name_entry));

    // Connected accounts, for the server-side backends - a Sieve or Graph rule
    // lives on one server, so it needs an account.
    let accounts: Vec<(lookout_core::AccountId, String)> = {
        let st = state.borrow();
        let mut accounts: Vec<(lookout_core::AccountId, String)> = st
            .accounts
            .iter()
            .map(|(account_id, handle)| {
                let label = if !handle.display_name.trim().is_empty() {
                    handle.display_name.clone()
                } else if !handle.email.trim().is_empty() {
                    handle.email.clone()
                } else {
                    account_id.0.clone()
                };
                (account_id.clone(), label)
            })
            .collect();
        accounts.sort_by_key(|(_, label)| label.to_lowercase());
        accounts
    };

    let backend = gtk::DropDown::from_strings(&["This device", "Server (Sieve)", "Microsoft 365"]);
    backend.set_selected(match rule.borrow().backend {
        RuleBackend::Client => 0,
        RuleBackend::Sieve => 1,
        RuleBackend::Graph => 2,
    });
    let account_row = if accounts.is_empty() {
        None
    } else {
        let labels: Vec<&str> = accounts.iter().map(|(_, label)| label.as_str()).collect();
        let account = gtk::DropDown::from_strings(&labels);
        if let Some(index) = accounts.iter().position(|(id, _)| rule.borrow().account.as_ref() == Some(id)) {
            account.set_selected(index as u32);
        }
        account.connect_selected_notify({
            let rule = rule.clone();
            let accounts = accounts.clone();
            move |dropdown| {
                if let Some((account_id, _)) = accounts.get(dropdown.selected() as usize) {
                    rule.borrow_mut().account = Some(account_id.clone());
                }
            }
        });
        let row = labeled_row("Account", &account);
        row.set_visible(rule.borrow().backend != RuleBackend::Client);
        Some(row)
    };
    backend.connect_selected_notify({
        let rule = rule.clone();
        let accounts = accounts.clone();
        let account_row = account_row.clone();
        move |dropdown| {
            let selected = match dropdown.selected() {
                1 => RuleBackend::Sieve,
                2 => RuleBackend::Graph,
                _ => RuleBackend::Client,
            };
            {
                let mut rule = rule.borrow_mut();
                rule.backend = selected;
                // A server-side rule (Sieve or Graph) must name its account;
                // the first connected one is the default. The client backend
                // clears the scope.
                match selected {
                    RuleBackend::Sieve | RuleBackend::Graph => {
                        if rule.account.is_none() {
                            rule.account = accounts.first().map(|(account_id, _)| account_id.clone());
                        }
                    }
                    RuleBackend::Client => rule.account = None,
                }
            }
            if let Some(row) = &account_row {
                row.set_visible(selected != RuleBackend::Client);
            }
        }
    });
    root.append(&labeled_row("Runs on", &backend));
    if let Some(row) = &account_row {
        root.append(row);
    }

    // The three sections are rebuilt whenever a row is added, removed or its
    // kind changes; the name entry and backend dropdown above persist.
    let sections = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    root.append(&sections);

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let rule = rule.clone();
        let sections = sections.clone();
        let rebuild_handle = rebuild.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            clear_box(&sections);

            let conditions = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
            for index in 0..rule.borrow().conditions.len() {
                conditions.append(&condition_row(rule.clone(), index, false, rebuild_handle.clone()));
            }
            conditions.append(&add_button("Add condition", {
                let rule = rule.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |_| {
                    rule.borrow_mut().conditions.push(RuleCondition::From(String::new()));
                    (rebuild_handle.borrow())();
                }
            }));
            sections.append(&section("Apply to messages where", &conditions));

            let exceptions = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
            for index in 0..rule.borrow().exceptions.len() {
                exceptions.append(&condition_row(rule.clone(), index, true, rebuild_handle.clone()));
            }
            exceptions.append(&add_button("Add exception", {
                let rule = rule.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |_| {
                    rule.borrow_mut().exceptions.push(RuleCondition::From(String::new()));
                    (rebuild_handle.borrow())();
                }
            }));
            sections.append(&section("Except", &exceptions));

            let actions = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
            for index in 0..rule.borrow().actions.len() {
                actions.append(&action_row(rule.clone(), index, rebuild_handle.clone()));
            }
            actions.append(&add_button("Add action", {
                let rule = rule.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |_| {
                    rule.borrow_mut().actions.push(RuleAction::MarkRead(true));
                    (rebuild_handle.borrow())();
                }
            }));
            sections.append(&section("Do", &actions));
        });
    }
    (rebuild.borrow())();

    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    buttons.append(&cancel);
    buttons.append(&save);
    root.append(&buttons);

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let state = state.clone();
        let rule = rule.clone();
        let on_saved = on_saved.clone();
        let dialog = dialog.clone();
        save.connect_clicked(move |_| {
            let mut saved = rule.borrow().clone();
            if saved.name.trim().is_empty() {
                saved.name = "Untitled rule".to_string();
            }
            // A server-side rule must name a connected account; pick the first
            // when none was chosen.
            if matches!(saved.backend, RuleBackend::Sieve | RuleBackend::Graph) && saved.account.is_none() {
                saved.account = accounts.first().map(|(account_id, _)| account_id.clone());
            }
            let app_config = state.borrow().app_config.clone();
            app_config.borrow_mut().upsert_rule(saved);
            crate::app_config::save(&app_config.borrow());
            crate::window::push_sieve_script(&state);
            on_saved();
            dialog.close();
        });
    }

    dialog.set_child(Some(&gtk::ScrolledWindow::builder().vexpand(true).child(&root).build()));
    dialog.present();
}

/// A titled section around `content`.
fn section(title: &str, content: &gtk::Box) -> gtk::Box {
    let outer = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    let label = gtk::Label::builder().label(title).halign(gtk::Align::Start).css_classes(["heading"]).build();
    outer.append(&label);
    outer.append(content);
    outer
}

/// An "Add …" button.
fn add_button(label: &str, on_click: impl Fn(&gtk::Button) + 'static) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Start);
    button.add_css_class("dim-label");
    button.connect_clicked(move |button| on_click(button));
    button
}

/// Writes `new` into the condition/exception at `index`.
type ConditionSetter = Rc<dyn Fn(RuleCondition)>;

fn condition_setter(rule: Rc<RefCell<Rule>>, index: usize, exception: bool) -> ConditionSetter {
    Rc::new(move |new| {
        let mut rule = rule.borrow_mut();
        let list = if exception { &mut rule.exceptions } else { &mut rule.conditions };
        if index < list.len() {
            list[index] = new;
        }
    })
}

/// One condition (or exception) row: a kind dropdown, a kind-specific value
/// editor, and a remove button.
fn condition_row(rule: Rc<RefCell<Rule>>, index: usize, exception: bool, rebuild: Rc<RefCell<Box<dyn Fn()>>>) -> gtk::Box {
    let current = {
        let rule = rule.borrow();
        let list = if exception { &rule.exceptions } else { &rule.conditions };
        list.get(index).cloned().unwrap_or_else(|| RuleCondition::From(String::new()))
    };
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let kinds = condition_kinds();
    let labels: Vec<&str> = kinds.iter().map(|(label, _)| *label).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_selected(condition_kind_index(&current));
    dropdown.set_valign(gtk::Align::Center);
    dropdown.connect_selected_notify({
        let rule = rule.clone();
        let rebuild = rebuild.clone();
        move |dropdown| {
            let kinds = condition_kinds();
            let replacement = kinds
                .get(dropdown.selected() as usize)
                .map(|(_, sample)| sample.clone())
                .unwrap_or_else(|| RuleCondition::From(String::new()));
            {
                let mut rule = rule.borrow_mut();
                let list = if exception { &mut rule.exceptions } else { &mut rule.conditions };
                if index < list.len() {
                    list[index] = replacement;
                }
            }
            (rebuild.borrow())();
        }
    });
    row.append(&dropdown);
    append_condition_value(&row, condition_setter(rule.clone(), index, exception), current);
    let remove = gtk::Button::from_icon_name("list-remove-symbolic");
    remove.add_css_class("flat");
    remove.set_valign(gtk::Align::Center);
    remove.connect_clicked({
        let rule = rule.clone();
        let rebuild = rebuild.clone();
        move |_| {
            {
                let mut rule = rule.borrow_mut();
                let list = if exception { &mut rule.exceptions } else { &mut rule.conditions };
                if index < list.len() {
                    list.remove(index);
                }
            }
            (rebuild.borrow())();
        }
    });
    row.append(&remove);
    row
}

/// Appends the value editor for a condition's kind.
fn append_condition_value(row: &gtk::Box, set: ConditionSetter, condition: RuleCondition) {
    match condition {
        RuleCondition::From(_) | RuleCondition::To(_) | RuleCondition::Cc(_) | RuleCondition::Subject(_) | RuleCondition::Body(_) | RuleCondition::ListId(_) => {
            let (text, rebuild) = match &condition {
                RuleCondition::From(t) => (t.clone(), 0u8),
                RuleCondition::To(t) => (t.clone(), 1),
                RuleCondition::Cc(t) => (t.clone(), 2),
                RuleCondition::Subject(t) => (t.clone(), 3),
                RuleCondition::Body(t) => (t.clone(), 4),
                _ => (String::new(), 5),
            };
            let entry = gtk::Entry::builder().text(text).hexpand(true).build();
            entry.connect_changed(move |entry| {
                let value = entry.text().to_string();
                set(match rebuild {
                    0 => RuleCondition::From(value),
                    1 => RuleCondition::To(value),
                    2 => RuleCondition::Cc(value),
                    3 => RuleCondition::Subject(value),
                    4 => RuleCondition::Body(value),
                    _ => RuleCondition::ListId(value),
                });
            });
            row.append(&entry);
        }
        RuleCondition::Header { name, value } => {
            let name_entry = gtk::Entry::builder().text(name).placeholder_text("Header name").width_chars(16).build();
            let value_entry = gtk::Entry::builder().text(value).placeholder_text("contains").hexpand(true).build();
            {
                let set = set.clone();
                let value_entry_handle = value_entry.clone();
                name_entry.connect_changed(move |entry| {
                    set(RuleCondition::Header {
                        name: entry.text().to_string(),
                        value: value_entry_handle.text().to_string(),
                    });
                });
            }
            {
                let name_entry_handle = name_entry.clone();
                value_entry.connect_changed(move |entry| {
                    set(RuleCondition::Header {
                        name: name_entry_handle.text().to_string(),
                        value: entry.text().to_string(),
                    })
                });
            }
            row.append(&name_entry);
            row.append(&value_entry);
        }
        RuleCondition::HasAttachment(has) => {
            let dropdown = gtk::DropDown::from_strings(&["Has an attachment", "Has no attachment"]);
            dropdown.set_selected(if has { 0 } else { 1 });
            dropdown.connect_selected_notify(move |dropdown| set(RuleCondition::HasAttachment(dropdown.selected() == 0)));
            row.append(&dropdown);
        }
        RuleCondition::Importance(importance) => {
            let dropdown = gtk::DropDown::from_strings(&["High", "Normal", "Low"]);
            dropdown.set_selected(match importance {
                Importance::High => 0,
                Importance::Normal => 1,
                Importance::Low => 2,
            });
            dropdown.connect_selected_notify(move |dropdown| {
                set(RuleCondition::Importance(match dropdown.selected() {
                    0 => Importance::High,
                    2 => Importance::Low,
                    _ => Importance::Normal,
                }))
            });
            row.append(&dropdown);
        }
        RuleCondition::Size { comparison, bytes } => {
            let dropdown = gtk::DropDown::from_strings(&["larger than", "at least", "smaller than", "at most", "exactly"]);
            dropdown.set_selected(match comparison {
                SizeComparison::GreaterThan => 0,
                SizeComparison::GreaterOrEqual => 1,
                SizeComparison::LessThan => 2,
                SizeComparison::LessOrEqual => 3,
                SizeComparison::Equal => 4,
            });
            let megabytes = (bytes as f64 / (1024.0 * 1024.0)).max(0.1);
            let entry = gtk::Entry::builder().text(format!("{megabytes:.1}")).width_chars(8).build();
            {
                let set = set.clone();
                let entry_handle = entry.clone();
                dropdown.connect_selected_notify(move |dropdown| {
                    set(RuleCondition::Size {
                        comparison: comparison_from_index(dropdown.selected()),
                        bytes: parse_megabytes(&entry_handle.text()),
                    });
                });
            }
            {
                let dropdown_handle = dropdown.clone();
                entry.connect_changed(move |entry| {
                    let comparison = comparison_from_index(dropdown_handle.selected());
                    set(RuleCondition::Size {
                        comparison,
                        bytes: parse_megabytes(&entry.text()),
                    });
                });
            }
            row.append(&dropdown);
            row.append(&entry);
            row.append(&gtk::Label::builder().label("MB").build());
        }
        RuleCondition::SentOnlyToMe => {}
    }
}

fn comparison_from_index(index: u32) -> SizeComparison {
    match index {
        0 => SizeComparison::GreaterThan,
        1 => SizeComparison::GreaterOrEqual,
        2 => SizeComparison::LessThan,
        3 => SizeComparison::LessOrEqual,
        _ => SizeComparison::Equal,
    }
}

/// Writes `new` into the action at `index`.
pub(crate) type ActionSetter = Rc<dyn Fn(RuleAction)>;
/// Reads the action at `index` (the editor's working copy).
pub(crate) type ActionGetter = Rc<dyn Fn() -> Option<RuleAction>>;
/// Removes the action at `index`.
pub(crate) type ActionRemover = Rc<dyn Fn()>;

fn action_setter(rule: Rc<RefCell<Rule>>, index: usize) -> ActionSetter {
    Rc::new(move |new| {
        let mut rule = rule.borrow_mut();
        if index < rule.actions.len() {
            rule.actions[index] = new;
        }
    })
}

/// One action row (kind dropdown, kind-specific value editor, remove button).
/// Shared by the rule editor and the Quick Step editor (item 2.6), so the two
/// offer exactly the same actions with identical value editors. The three
/// closures read, write and remove the row's action from the caller's working
/// copy, which is either a `Rule`'s action list or a `QuickStep`'s.
pub(crate) fn action_row_with(get: ActionGetter, set: ActionSetter, remove: ActionRemover, rebuild: Rc<RefCell<Box<dyn Fn()>>>) -> gtk::Box {
    let current = get().unwrap_or(RuleAction::MarkRead(true));
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let kinds = action_kinds();
    let labels: Vec<&str> = kinds.iter().map(|(label, _)| *label).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_selected(action_kind_index(&current));
    dropdown.set_valign(gtk::Align::Center);
    dropdown.connect_selected_notify({
        let set = set.clone();
        let rebuild = rebuild.clone();
        move |dropdown| {
            let kinds = action_kinds();
            let replacement = kinds
                .get(dropdown.selected() as usize)
                .map(|(_, sample)| sample.clone())
                .unwrap_or(RuleAction::MarkRead(true));
            set(replacement);
            (rebuild.borrow())();
        }
    });
    row.append(&dropdown);
    append_action_value(&row, set, current);
    let remove_button = gtk::Button::from_icon_name("list-remove-symbolic");
    remove_button.add_css_class("flat");
    remove_button.set_valign(gtk::Align::Center);
    remove_button.connect_clicked({
        let rebuild = rebuild.clone();
        move |_| {
            remove();
            (rebuild.borrow())();
        }
    });
    row.append(&remove_button);
    row
}

/// One action row for a rule being edited: binds [`action_row_with`] to the
/// rule's own action list.
fn action_row(rule: Rc<RefCell<Rule>>, index: usize, rebuild: Rc<RefCell<Box<dyn Fn()>>>) -> gtk::Box {
    let get: ActionGetter = {
        let rule = rule.clone();
        Rc::new(move || rule.borrow().actions.get(index).cloned())
    };
    let set = action_setter(rule.clone(), index);
    let remove: ActionRemover = {
        let rule = rule.clone();
        Rc::new(move || {
            let mut rule = rule.borrow_mut();
            if index < rule.actions.len() {
                rule.actions.remove(index);
            }
        })
    };
    action_row_with(get, set, remove, rebuild)
}

/// Appends the value editor for an action's kind.
fn append_action_value(row: &gtk::Box, set: ActionSetter, action: RuleAction) {
    match action {
        RuleAction::Move(target) => {
            let entry = target_entry(&target);
            entry.connect_changed(move |entry| set(RuleAction::Move(parse_target(&entry.text()))));
            row.append(&entry);
        }
        RuleAction::Copy(target) => {
            let entry = target_entry(&target);
            entry.connect_changed(move |entry| set(RuleAction::Copy(parse_target(&entry.text()))));
            row.append(&entry);
        }
        RuleAction::Delete | RuleAction::StopProcessing => {}
        RuleAction::Flag(flag) => {
            let due = flag.due.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            let entry = gtk::Entry::builder().text(due).placeholder_text("due date YYYY-MM-DD (optional)").hexpand(true).build();
            entry.connect_changed(move |entry| {
                let due = chrono::NaiveDate::parse_from_str(entry.text().trim(), "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|naive| chrono::DateTime::from_naive_utc_and_offset(naive, chrono::Utc));
                set(RuleAction::Flag(FollowUpFlag {
                    status: lookout_core::FlagStatus::Flagged,
                    due,
                    ..FollowUpFlag::default()
                }));
            });
            row.append(&entry);
        }
        RuleAction::Categorize(names) => {
            let entry = gtk::Entry::builder().text(names.join(", ")).placeholder_text("Category, Another").hexpand(true).build();
            entry.connect_changed(move |entry| {
                let names = entry.text().split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_string).collect();
                set(RuleAction::Categorize(names));
            });
            row.append(&entry);
        }
        RuleAction::MarkRead(read) => {
            let dropdown = gtk::DropDown::from_strings(&["Mark as read", "Mark as unread"]);
            dropdown.set_selected(if read { 0 } else { 1 });
            dropdown.connect_selected_notify(move |dropdown| set(RuleAction::MarkRead(dropdown.selected() == 0)));
            row.append(&dropdown);
        }
        RuleAction::Forward(to) => {
            let entry = gtk::Entry::builder().text(address_text(&to)).placeholder_text("name@example.com").hexpand(true).build();
            entry.connect_changed(move |entry| set(RuleAction::Forward(parse_addresses(&entry.text()))));
            row.append(&entry);
        }
        RuleAction::Redirect(to) => {
            let entry = gtk::Entry::builder().text(address_text(&to)).placeholder_text("name@example.com").hexpand(true).build();
            entry.connect_changed(move |entry| set(RuleAction::Redirect(parse_addresses(&entry.text()))));
            row.append(&entry);
        }
    }
}

/// A target entry, pre-filled with the target's role or folder name.
fn target_entry(target: &RuleTarget) -> gtk::Entry {
    gtk::Entry::builder()
        .text(target_text(target))
        .placeholder_text("Archive, or a folder name")
        .hexpand(true)
        .build()
}

/// The comma-joined addresses an address action shows.
fn address_text(addresses: &[EmailAddress]) -> String {
    addresses.iter().map(|address| address.address.clone()).collect::<Vec<_>>().join(", ")
}

/// Parses a megabytes text field into bytes, falling back to 1 MB.
fn parse_megabytes(text: &str) -> u64 {
    let value: f64 = text.trim().parse().unwrap_or(1.0);
    (value.max(0.0) * 1024.0 * 1024.0) as u64
}

/// A rule prefilled from a message (the message context menu's Rules ▸ Create
/// rule): the sender becomes a From condition, a non-empty subject a Subject
/// condition, and a List-Id a mailing-list condition.
pub(crate) fn rule_from_message(summary: &lookout_core::EmailSummary) -> Rule {
    let mut rule = Rule::new(new_rule_id(), "New rule");
    if let Some(from) = summary.from.first() {
        rule.conditions.push(RuleCondition::From(from.address.clone()));
    }
    if let Some(subject) = summary.subject.as_deref().filter(|subject| !subject.trim().is_empty()) {
        rule.conditions.push(RuleCondition::Subject(subject.to_string()));
    }
    if let Some(list) = summary.list_id.as_deref().filter(|list| !list.trim().is_empty()) {
        rule.conditions.push(RuleCondition::ListId(list.to_string()));
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip_through_text() {
        assert_eq!(parse_target("Archive"), RuleTarget::Role(MailboxRole::Archive));
        assert_eq!(parse_target("junk email"), RuleTarget::Role(MailboxRole::Junk));
        assert_eq!(parse_target("Projects"), RuleTarget::Folder("Projects".into()));
        assert_eq!(target_text(&RuleTarget::Role(MailboxRole::Trash)), "Deleted Items");
        assert_eq!(target_text(&RuleTarget::Folder("Projects".into())), "Projects");
    }

    #[test]
    fn address_lists_parse_on_commas_and_semicolons() {
        let addresses = parse_addresses("a@example.com, b@example.com; c@example.com");
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[1].address, "b@example.com");
        assert!(parse_addresses("  ").is_empty());
    }

    #[test]
    fn summary_mentions_conditions_and_actions() {
        let mut rule = Rule::new("r", "Newsletters");
        rule.conditions = vec![RuleCondition::From("news@example.com".into())];
        rule.actions = vec![RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))];
        let summary = rule_summary(&rule);
        assert!(summary.contains("From contains"));
        assert!(summary.contains("Move to Archive"));
    }

    #[test]
    fn prefill_takes_sender_subject_and_list() {
        use lookout_core::{EmailAddress, EmailSummary, MailboxId, MessageSensitivity, ThreadKey, Uid};
        let summary = EmailSummary {
            inference_classification: Default::default(),
            uid: Uid(1),
            mailbox: MailboxId("acct:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey("t".into()),
            subject: Some("Weekly digest".into()),
            from: vec![EmailAddress::new("news@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            date: "2026-10-14T00:00:00Z".parse().unwrap(),
            flags: std::collections::BTreeSet::new(),
            keywords: std::collections::BTreeSet::new(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: Importance::Normal,
            sensitivity: MessageSensitivity::Normal,
            list_id: Some("list.example.com".into()),
            structure: None,
        };
        let rule = rule_from_message(&summary);
        assert!(rule.conditions.iter().any(|c| matches!(c, RuleCondition::From(f) if f == "news@example.com")));
        assert!(rule.conditions.iter().any(|c| matches!(c, RuleCondition::Subject(s) if s == "Weekly digest")));
        assert!(rule.conditions.iter().any(|c| matches!(c, RuleCondition::ListId(l) if l == "list.example.com")));
    }
}
