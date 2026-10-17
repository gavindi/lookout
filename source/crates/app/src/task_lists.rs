//! Task-list create / rename / delete dialogs and routing (OUTLOOK_PARITY.md
//! item 4.8 / T-01). The Tasks navigation pane's "+ New list" and each source
//! row's ⋯ menu call into here; the routing picks the owning backend - a local
//! list in `settings.json`, a CalDAV collection (MKCALENDAR / PROPPATCH /
//! DELETE), a Google Tasks list, or a Microsoft To Do list - and sends it the
//! matching command through the existing actor channels. The session re-lists
//! on success, which is what repaints the pane.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::CalendarId;

use crate::app_config::LocalTaskList;
use crate::window::{CalendarUiState, UiState};

/// Where a new task list is created - the choices the create dialog offers.
#[derive(Clone)]
pub(crate) enum TaskListTarget {
    /// The on-device store (`settings.json`).
    Local,
    /// A CalDAV account (`AccountId`): MKCALENDAR under its calendar-home.
    Calendar(lookout_core::AccountId),
    /// A Google Tasks account, keyed by its email.
    Google(String),
    /// A Microsoft 365 To Do account, keyed by its `AccountId`.
    Graph(lookout_core::AccountId),
}

/// The create targets available right now, as `(label, target)`: "On this
/// device" always, then each connected backend account.
fn targets(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(String, TaskListTarget)> {
    let st = calendar_state.borrow();
    let mut out = vec![("On this device".to_string(), TaskListTarget::Local)];
    // Every CalDAV account is offered; a server without MKCALENDAR answers with
    // an error the UI toasts (degrade softly, §10).
    let mut accounts: Vec<(&lookout_core::AccountId, &crate::window::CalendarAccountHandle)> = st.accounts.iter().collect();
    accounts.sort_by_key(|(_, handle)| handle.display_name.to_lowercase());
    for (account_id, handle) in accounts {
        out.push((format!("{} (calendar)", handle.display_name), TaskListTarget::Calendar(account_id.clone())));
    }
    let mut google: Vec<_> = st.google_tasks.values().collect();
    google.sort_by_key(|h| h.email.to_lowercase());
    for handle in google {
        out.push((format!("{} (Google Tasks)", handle.email), TaskListTarget::Google(handle.email.clone())));
    }
    let mut graph: Vec<(&lookout_core::AccountId, &crate::window::GraphTodoHandle)> = st.graph_todo.iter().collect();
    graph.sort_by_key(|(_, handle)| handle.display_name.to_lowercase());
    for (account_id, handle) in graph {
        out.push((format!("{} (Microsoft To Do)", handle.display_name), TaskListTarget::Graph(account_id.clone())));
    }
    out
}

/// The name a list currently shows, for the rename dialog's prefill.
pub(crate) fn list_name(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> String {
    if let Some(list_id) = crate::window::local_task_list_id(calendar_id) {
        return calendar_state
            .borrow()
            .local_task_lists
            .iter()
            .find(|l| l.id == list_id)
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "Tasks".to_string());
    }
    if let Some(list_id) = crate::google_tasks::google_task_list_id(calendar_id) {
        if let Some(list) = calendar_state.borrow().google_tasks.values().find_map(|h| h.task_lists.iter().find(|l| l.id == list_id)) {
            return list.title.clone();
        }
    }
    if let Some(list_id) = lookout_graph::graph_todo_list_id(calendar_id) {
        if let Some(list) = calendar_state.borrow().graph_todo.values().find_map(|h| h.task_lists.iter().find(|l| l.id == list_id)) {
            return list.display_name.clone();
        }
    }
    crate::window::task_list_names(calendar_state)
        .get(calendar_id)
        .cloned()
        .unwrap_or_else(|| "List".to_string())
}

/// Shows the "New list" dialog: a name plus a target picker.
pub(crate) fn create_task_list_dialog(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
) {
    let choices = targets(calendar_state);
    let dialog = adw::Window::builder().transient_for(window).modal(true).title("New list").default_width(420).build();

    let name_entry = adw::EntryRow::builder().title("Name").build();
    let labels: Vec<&str> = choices.iter().map(|(label, _)| label.as_str()).collect();
    let model = gtk::StringList::new(&labels);
    let target_row = adw::ComboRow::builder().title("Create in").model(&model).build();
    let group = adw::PreferencesGroup::new();
    group.add(&name_entry);
    group.add(&target_row);

    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["error"]).visible(false).build();
    crate::privacy::mark_ui(&error_label);

    let cancel = gtk::Button::with_label("Cancel");
    let create = gtk::Button::with_label("Create");
    create.add_css_class("suggested-action");
    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&create);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&group);
    content.append(&error_label);
    content.append(&buttons);
    dialog.set_content(Some(&content));

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let name_entry = name_entry.clone();
        let choices = choices.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        let error_label = error_label.clone();
        create.connect_clicked(move |_| {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                error_label.set_label("Give the list a name.");
                error_label.set_visible(true);
                return;
            }
            let Some((_, target)) = choices.get(target_row.selected() as usize) else {
                error_label.set_label("Pick where to create the list.");
                error_label.set_visible(true);
                return;
            };
            create_list(&state, &calendar_state, &tasks_view, &toast_overlay, target.clone(), &name);
            dialog.close();
        });
    }
    dialog.present();
}

/// Shows the rename dialog for the list `calendar_id` names.
pub(crate) fn rename_task_list_dialog(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    calendar_id: CalendarId,
) {
    let dialog = adw::Window::builder().transient_for(window).modal(true).title("Rename list").default_width(420).build();
    let name_entry = adw::EntryRow::builder().title("Name").build();
    name_entry.set_text(&list_name(calendar_state, &calendar_id));
    let group = adw::PreferencesGroup::new();
    group.add(&name_entry);

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Rename");
    save.add_css_class("suggested-action");
    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&save);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&group);
    content.append(&buttons);
    dialog.set_content(Some(&content));

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let name_entry = name_entry.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        let calendar_id = calendar_id.clone();
        save.connect_clicked(move |_| {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            rename_list(&state, &calendar_state, &tasks_view, &toast_overlay, &calendar_id, &name);
            dialog.close();
        });
    }
    dialog.present();
}

/// Shows a confirmation before deleting the list `calendar_id` names (a
/// destructive, non-undoable action - §6.1's permanent-delete exception).
pub(crate) fn delete_task_list_confirm(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    calendar_id: CalendarId,
) {
    let name = list_name(calendar_state, &calendar_id);
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Delete list?")
        .body(format!("\"{name}\" and every task in it will be deleted. This can't be undone."))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        dialog.connect_response(None, move |_dialog, response| {
            if response == "delete" {
                delete_list(&state, &calendar_state, &tasks_view, &toast_overlay, &calendar_id);
            }
        });
    }
    dialog.present();
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn create_list(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    target: TaskListTarget,
    name: &str,
) {
    match target {
        TaskListTarget::Local => {
            add_local_list(state, calendar_state, name);
            crate::window::refresh_tasks_view(calendar_state, tasks_view);
        }
        TaskListTarget::Calendar(account_id) => {
            let tx = calendar_state.borrow().accounts.get(&account_id).map(|h| h.cmd_tx.clone());
            match tx {
                Some(tx) => {
                    let _ = tx.send_blocking(lookout_core::calendar_protocol::CalendarCommand::CreateCalendar { display_name: name.to_string() });
                }
                None => toast(toast_overlay, "That calendar account is no longer connected."),
            }
        }
        TaskListTarget::Google(email) => {
            let tx = calendar_state.borrow().google_tasks.get(&email).map(|h| h.cmd_tx.clone());
            match tx {
                Some(tx) => {
                    let _ = tx.send_blocking(crate::google_tasks::GoogleTasksCommand::CreateList { title: name.to_string() });
                }
                None => toast(toast_overlay, "That Google account is no longer connected."),
            }
        }
        TaskListTarget::Graph(account_id) => {
            let tx = calendar_state.borrow().graph_todo.get(&account_id).map(|h| h.cmd_tx.clone());
            match tx {
                Some(tx) => {
                    let _ = tx.send_blocking(lookout_graph::TodoCommand::CreateList { display_name: name.to_string() });
                }
                None => toast(toast_overlay, "That Microsoft To Do account is no longer connected."),
            }
        }
    }
}

fn rename_list(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    calendar_id: &CalendarId,
    name: &str,
) {
    if let Some(list_id) = crate::window::local_task_list_id(calendar_id) {
        rename_local_list(state, calendar_state, list_id, name);
        crate::window::refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = crate::google_tasks::google_task_list_id(calendar_id) {
        let tx = crate::window::google_tasks_handle_for_calendar(&calendar_state.borrow(), calendar_id).map(|h| h.cmd_tx.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send_blocking(crate::google_tasks::GoogleTasksCommand::RenameList {
                    list_id: list_id.to_string(),
                    title: name.to_string(),
                });
            }
            None => toast(toast_overlay, "That Google list is no longer available."),
        }
        return;
    }
    if let Some(list_id) = lookout_graph::graph_todo_list_id(calendar_id) {
        let tx = crate::window::graph_todo_handle_for_calendar(&calendar_state.borrow(), calendar_id).map(|h| h.cmd_tx.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send_blocking(lookout_graph::TodoCommand::UpdateList {
                    list_id: list_id.to_string(),
                    display_name: name.to_string(),
                });
            }
            None => toast(toast_overlay, "That To Do list is no longer available."),
        }
        return;
    }
    let tx = crate::window::task_handle_for_id(calendar_state, calendar_id);
    match tx {
        Some(tx) => {
            let _ = tx.send_blocking(lookout_core::calendar_protocol::CalendarCommand::UpdateCalendar {
                calendar_id: calendar_id.clone(),
                display_name: name.to_string(),
            });
        }
        None => toast(toast_overlay, "That calendar is no longer connected."),
    }
}

fn delete_list(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    calendar_id: &CalendarId,
) {
    if let Some(list_id) = crate::window::local_task_list_id(calendar_id) {
        delete_local_list(state, calendar_state, list_id);
        crate::window::refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = crate::google_tasks::google_task_list_id(calendar_id) {
        let tx = crate::window::google_tasks_handle_for_calendar(&calendar_state.borrow(), calendar_id).map(|h| h.cmd_tx.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send_blocking(crate::google_tasks::GoogleTasksCommand::DeleteList { list_id: list_id.to_string() });
            }
            None => toast(toast_overlay, "That Google list is no longer available."),
        }
        return;
    }
    if let Some(list_id) = lookout_graph::graph_todo_list_id(calendar_id) {
        let tx = crate::window::graph_todo_handle_for_calendar(&calendar_state.borrow(), calendar_id).map(|h| h.cmd_tx.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send_blocking(lookout_graph::TodoCommand::DeleteList { list_id: list_id.to_string() });
            }
            None => toast(toast_overlay, "That To Do list is no longer available."),
        }
        return;
    }
    let tx = crate::window::task_handle_for_id(calendar_state, calendar_id);
    match tx {
        Some(tx) => {
            let _ = tx.send_blocking(lookout_core::calendar_protocol::CalendarCommand::DeleteCalendar { calendar_id: calendar_id.clone() });
        }
        None => toast(toast_overlay, "That calendar is no longer connected."),
    }
}

// --- Local list mutation (settings.json + the in-memory mirror) ---

fn add_local_list(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, name: &str) {
    let config = state.borrow().app_config.clone();
    config.borrow_mut().upsert_local_task_list(LocalTaskList {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
    });
    crate::app_config::save(&config.borrow());
    calendar_state.borrow_mut().local_task_lists = config.borrow().local_task_lists();
}

fn rename_local_list(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, list_id: &str, name: &str) {
    let config = state.borrow().app_config.clone();
    config.borrow_mut().upsert_local_task_list(LocalTaskList {
        id: list_id.to_string(),
        name: name.to_string(),
    });
    crate::app_config::save(&config.borrow());
    calendar_state.borrow_mut().local_task_lists = config.borrow().local_task_lists();
}

fn delete_local_list(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, list_id: &str) {
    let config = state.borrow().app_config.clone();
    let removed_calendar = crate::window::local_task_list_calendar_id(list_id);
    config.borrow_mut().remove_local_task_list(list_id);
    crate::app_config::save(&config.borrow());
    {
        let mut st = calendar_state.borrow_mut();
        st.local_task_lists = config.borrow().local_task_lists();
        // Drop the list's tasks from the in-memory mirror and the device store
        // (the whole list is gone - the confirm dialog warned).
        let doomed: Vec<String> = st.local_tasks.iter().filter(|t| t.calendar_id == removed_calendar).map(|t| t.uid.0.clone()).collect();
        if let Some(db) = &st.local_tasks_db {
            for uid in &doomed {
                let _ = db.borrow().delete_local_task(uid);
            }
        }
        st.local_tasks.retain(|t| t.calendar_id != removed_calendar);
    }
}

fn toast(toast_overlay: &adw::ToastOverlay, message: &str) {
    toast_overlay.add_toast(adw::Toast::new(message));
}
