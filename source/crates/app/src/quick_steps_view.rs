//! The Quick Steps manager and editor (Config → Mail → Quick Steps…, item 2.6
//! / M-15).
//!
//! The manager lists the steps in gallery order - which is also the
//! `Ctrl+Shift+1`…`9` slot order - with reorder up/down, edit and delete, and
//! a "New quick step…" row; the editor names a step and builds its action
//! chain. Both write through to `settings.json` (`AppConfig::quick_steps`),
//! the same pattern the rules manager uses, and the editor reuses the rules
//! editor's action widgets ([`crate::rules_view::action_row_with`]) so a step
//! offers exactly the actions a rule does.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{QuickStep, RuleAction};

use crate::rules_view::{action_row_with, ActionGetter, ActionRemover, ActionSetter};
use crate::util::{clear_box, labeled_row};
use crate::window::UiState;

/// A new step id: a UUID string, stable across edits.
fn new_step_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Presents the modal Quick Steps manager, anchored to `anchor`'s window.
pub(crate) fn show_manage_quick_steps_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Quick steps").default_width(640).default_height(560);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("Quick steps run on the messages you select, in any folder. They appear in the Mail ribbon's Quick steps menu, and the first nine are bound to Ctrl+Shift+1 to Ctrl+Shift+9 in this order. Stored on this device.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&hint);

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

            let steps: Vec<QuickStep> = state.borrow().app_config.borrow().quick_steps.clone();
            let group = adw::PreferencesGroup::builder().title("Quick steps").build();
            if steps.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No quick steps yet. Create one below; it appears in the ribbon's Quick steps menu.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                crate::privacy::mark_ui(&empty);
                group.add(&empty);
            }
            for (index, step) in steps.iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&step.name))
                    .subtitle(glib::markup_escape_text(&step.summary()))
                    .build();
                crate::privacy::mark_content(&row);
                row.set_activatable(false);
                let suffix = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).valign(gtk::Align::Center).build();

                for (icon, tooltip, enabled_now, delta) in [
                    ("go-up-symbolic", "Move up", index > 0, -1i32),
                    ("go-down-symbolic", "Move down", index + 1 < steps.len(), 1),
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
                                app_config.borrow_mut().move_quick_step(index, target as usize);
                                crate::app_config::save(&app_config.borrow());
                            }
                            (rebuild_handle.borrow())();
                        }
                    });
                    suffix.append(&button);
                }

                let edit = gtk::Button::from_icon_name("document-edit-symbolic");
                edit.add_css_class("flat");
                edit.set_tooltip_text(Some("Edit this quick step"));
                edit.connect_clicked({
                    let state = state.clone();
                    let step = step.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move |button| {
                        let (state, step) = (state.clone(), step.clone());
                        let rebuild_handle = rebuild_handle.clone();
                        show_quick_step_editor(button.upcast_ref::<gtk::Widget>(), state, step, move || (rebuild_handle.borrow())());
                    }
                });
                suffix.append(&edit);

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.add_css_class("flat");
                delete.set_tooltip_text(Some("Delete this quick step"));
                delete.connect_clicked({
                    let state = state.clone();
                    let id = step.id.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move |_| {
                        let app_config = state.borrow().app_config.clone();
                        app_config.borrow_mut().remove_quick_step(&id);
                        crate::app_config::save(&app_config.borrow());
                        (rebuild_handle.borrow())();
                    }
                });
                suffix.append(&delete);
                row.add_suffix(&suffix);
                group.add(&row);
            }

            let new_row = adw::ActionRow::builder().title("New quick step…").build();
            new_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            new_row.set_activatable(true);
            new_row.connect_activated({
                let state = state.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |row| {
                    let (state, rebuild_handle) = (state.clone(), rebuild_handle.clone());
                    let step = QuickStep::new(new_step_id(), "New quick step");
                    show_quick_step_editor(row.upcast_ref::<gtk::Widget>(), state, step, move || (rebuild_handle.borrow())());
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

/// Presents the Quick Step editor for `step`. `on_saved` runs after the step is
/// written, so the manager can repaint.
pub(crate) fn show_quick_step_editor(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, step: QuickStep, on_saved: impl Fn() + 'static) {
    let step = Rc::new(RefCell::new(step));
    let on_saved = Rc::new(on_saved);

    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Quick Step").default_width(620).default_height(560);
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

    let name_entry = gtk::Entry::builder()
        .text(step.borrow().name.clone())
        .placeholder_text("Quick Step name")
        .hexpand(true)
        .build();
    name_entry.connect_changed({
        let step = step.clone();
        move |entry| step.borrow_mut().name = entry.text().to_string()
    });
    root.append(&labeled_row("Name", &name_entry));

    // The action chain is edited on its own working copy; it is written back to
    // the step on Save.
    let actions: Rc<RefCell<Vec<RuleAction>>> = Rc::new(RefCell::new(step.borrow().actions.clone()));
    let sections = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    root.append(&sections);

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let actions = actions.clone();
        let sections = sections.clone();
        let rebuild_handle = rebuild.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            clear_box(&sections);
            let rows = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
            for index in 0..actions.borrow().len() {
                let get: ActionGetter = {
                    let actions = actions.clone();
                    Rc::new(move || actions.borrow().get(index).cloned())
                };
                let set: ActionSetter = {
                    let actions = actions.clone();
                    Rc::new(move |new| {
                        if let Some(slot) = actions.borrow_mut().get_mut(index) {
                            *slot = new;
                        }
                    })
                };
                let remove: ActionRemover = {
                    let actions = actions.clone();
                    Rc::new(move || {
                        let mut actions = actions.borrow_mut();
                        if index < actions.len() {
                            actions.remove(index);
                        }
                    })
                };
                rows.append(&action_row_with(get, set, remove, rebuild_handle.clone()));
            }
            rows.append(&add_button("Add action", {
                let actions = actions.clone();
                let rebuild_handle = rebuild_handle.clone();
                move |_| {
                    actions.borrow_mut().push(RuleAction::MarkRead(true));
                    (rebuild_handle.borrow())();
                }
            }));
            sections.append(&section("Do", &rows));
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
        let step = step.clone();
        let actions = actions.clone();
        let on_saved = on_saved.clone();
        let dialog = dialog.clone();
        save.connect_clicked(move |_| {
            let mut saved = step.borrow().clone();
            if saved.name.trim().is_empty() {
                saved.name = "Untitled quick step".to_string();
            }
            saved.actions = actions.borrow().clone();
            let app_config = state.borrow().app_config.clone();
            app_config.borrow_mut().upsert_quick_step(saved);
            crate::app_config::save(&app_config.borrow());
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
    crate::privacy::mark_ui(&label);
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

/// Builds the Mail ribbon's **Quick steps** popover contents (UX-09/UX-21):
/// one row per step in gallery order (disabled while nothing is selected),
/// then "Create new quick step…" and "Manage quick steps…". Rebuilt on every
/// `show`, so the enabled state tracks the live selection.
pub(crate) fn build_quick_steps_menu(state: &Rc<RefCell<UiState>>, message_list: &crate::message_list::MessageListModel, popover: &gtk::Popover) -> gtk::Box {
    let steps: Vec<QuickStep> = state.borrow().app_config.borrow().quick_steps.clone();
    let has_selection = !message_list.selected_summaries().is_empty();
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    if steps.is_empty() {
        let empty = gtk::Label::builder()
            .label("No quick steps yet")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .margin_top(2)
            .margin_bottom(2)
            .build();
        crate::privacy::mark_ui(&empty);
        boxed.append(&empty);
    } else {
        for (slot, step) in steps.iter().enumerate() {
            let label = if slot < 9 {
                format!("{}   (Ctrl+Shift+{})", step.name, slot + 1)
            } else {
                step.name.clone()
            };
            let button = gtk::Button::builder()
                .label(label)
                .css_classes(["flat"])
                .halign(gtk::Align::Start)
                .tooltip_text(step.summary())
                .build();
            crate::privacy::mark_content(&button);
            button.set_sensitive(has_selection && step.runnable());
            button.connect_clicked({
                let state = state.clone();
                let message_list = message_list.clone();
                let popover = popover.clone();
                let step = step.clone();
                move |_| {
                    popover.popdown();
                    let summaries = message_list.selected_summaries();
                    crate::quick_steps::apply_quick_step(&state, &message_list, &summaries, &step);
                }
            });
            boxed.append(&button);
        }
    }

    boxed.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let create = gtk::Button::builder()
        .label("Create new quick step…")
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .build();
    create.connect_clicked({
        let state = state.clone();
        let popover = popover.clone();
        move |button| {
            popover.popdown();
            let step = QuickStep::new(new_step_id(), "New quick step");
            show_quick_step_editor(button.upcast_ref::<gtk::Widget>(), state.clone(), step, || {});
        }
    });
    boxed.append(&create);
    let manage = gtk::Button::builder().label("Manage quick steps…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    manage.connect_clicked({
        let state = state.clone();
        let popover = popover.clone();
        move |button| {
            popover.popdown();
            show_manage_quick_steps_dialog(button.upcast_ref::<gtk::Widget>(), state.clone());
        }
    });
    boxed.append(&manage);
    boxed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_step_ids_are_unique() {
        assert_ne!(new_step_id(), new_step_id());
    }
}
