//! Message-list actions and menus: selection verbs, context menus, categories UI, mailbox switching and compose entry points.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::calendar_protocol::CalendarCommand;
use lookout_core::{AccountId, BodyPart, CalendarId, CalendarTask, EmailBody, EmailSummary, MailboxId, MailboxRole, SystemFlagBit, TaskPriority, TaskStatus, TaskUid, Uid};
use lookout_mail::session::AccountCommand;
use webkit::prelude::*;

use crate::google_tasks::{self, GoogleTasksCommand};
use crate::last_view::{self, LastSelection};
use crate::message_list::{MessageListModel, SelectionKind, ThreadId};
use crate::worker::Worker;

use super::*;

/// Builds a follow-up `CalendarTask` seed for a mail message plus the
/// calendar it defaults into - the title is the email's subject (falling
/// back to "(no subject)"); since `CalendarTask` has no url/link field, the
/// Notes field carries a "From: <sender> — <date>" line and a hidden marker
/// (`task_email_marker`) as the only way back to the source email, letting
/// `email_has_task`/`task_for_email` later recognize this task as belonging
/// to it - skipped when the message carries no Message-ID (rare, but then
/// there's nothing stable to match against). The calendar defaults to
/// `preferred_account`'s own task-capable calendar or Google Tasks list when
/// it has one, else the same fallback order `show_new_task_editor` uses.
/// Shared by `show_create_task_for_email` (toolbar, opens an editor around
/// this seed) and the message row's hover-quickmenu Follow-up toggle (saves
/// this seed directly, no editor).
pub(crate) fn build_task_seed_for_email(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    summary: &EmailSummary,
    preferred_account: Option<AccountId>,
    preferred_email: Option<String>,
) -> (CalendarId, CalendarTask) {
    let default_calendar = default_task_calendar_preferring(calendar_state, preferred_account.as_ref(), preferred_email.as_deref());

    let title = summary.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(no subject)").to_string();
    let sender_label = summary
        .from
        .first()
        .map(|addr| addr.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&addr.address).to_string())
        .unwrap_or_else(|| "unknown sender".to_string());
    let mut description = format!(
        "From: {sender_label} — {}",
        summary.date.with_timezone(&chrono::Local).format("%a, %b %-d, %Y at %-I:%M %p")
    );
    if let Some(message_id) = &summary.message_id {
        description.push_str("\n\n");
        description.push_str(&task_email_marker(message_id));
    }

    let seed = CalendarTask {
        uid: TaskUid(uuid::Uuid::new_v4().to_string()),
        calendar_id: default_calendar.clone(),
        summary: Some(title),
        description: Some(description),
        due: None,
        start: None,
        created: Some(chrono::Utc::now()),
        completed: None,
        status: TaskStatus::NeedsAction,
        priority: TaskPriority(0),
        percent_complete: None,
        categories: Vec::new(),
        href: None,
        etag: None,
    };
    (default_calendar, seed)
}

/// Opens the task editor's create form seeded from a mail message - the mail
/// toolbar's "Add as Task" button.
pub(crate) fn show_create_task_for_email(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    summary: &EmailSummary,
    preferred_account: Option<AccountId>,
    preferred_email: Option<String>,
) {
    let mut calendars = pickable_task_calendars(calendar_state);
    if calendars.is_empty() {
        calendars.push(("Local · Tasks".to_string(), local_tasks_calendar_id()));
    }
    let (default_calendar, seed) = build_task_seed_for_email(calendar_state, summary, preferred_account, preferred_email);
    let categories = state.borrow().app_config.borrow().categories.clone();

    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            prefill: Some(&seed),
            categories: &categories,
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |_calendar_id, _uid, _href, _etag| {},
    );
}

/// Opens the editor for an existing task.
pub(crate) fn open_task_editor_for(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    task: &CalendarTask,
) {
    // The picker lists every task-capable source. The task's own store is
    // appended unconditionally too: the save handler stamps the form's
    // selected calendar onto the task, so an edit whose source fell out of
    // the filtered list must still resolve back to it, not silently re-home
    // the task into the first picker entry.
    let mut calendars = pickable_task_calendars(calendar_state);
    if !calendars.iter().any(|(_, id)| *id == task.calendar_id) {
        if let Some(list_id) = local_task_list_id(&task.calendar_id) {
            let name = calendar_state
                .borrow()
                .local_task_lists
                .iter()
                .find(|l| l.id == list_id)
                .map(|l| l.name.clone())
                .unwrap_or_else(|| "Tasks".to_string());
            calendars.push((format!("Local · {name}"), task.calendar_id.clone()));
        } else if let Some((handle, info)) = calendar_state
            .borrow()
            .accounts
            .values()
            .find_map(|h| h.calendars.iter().find(|c| c.id == task.calendar_id).map(|c| (h, c)))
        {
            calendars.push((format!("{} · {}", handle.display_name, info.display_name), task.calendar_id.clone()));
        } else if let Some(list) = calendar_state.borrow().google_tasks.values().find_map(|h| {
            h.task_lists
                .iter()
                .find(|l| google_tasks::google_task_calendar_id(&l.id) == task.calendar_id)
                .map(|l| (h, l))
        }) {
            calendars.push((format!("{} · {}", list.0.email, list.1.title), task.calendar_id.clone()));
        } else if let Some(list) = calendar_state.borrow().graph_todo.values().find_map(|h| {
            h.task_lists
                .iter()
                .find(|l| lookout_graph::graph_todo_calendar_id(&l.id) == task.calendar_id)
                .map(|l| (h, l))
        }) {
            calendars.push((format!("{} · {}", list.0.display_name, list.1.display_name), task.calendar_id.clone()));
        }
    }
    let calendar_state = calendar_state.clone();
    let calendar_state_for_delete = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    let tasks_view_for_delete = tasks_view.clone();
    let categories = state.borrow().app_config.borrow().categories.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar: task.calendar_id.clone(),
            existing: Some(task),
            prefill: None,
            categories: &categories,
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |calendar_id, uid, href, etag| route_task_delete(&calendar_state_for_delete, &tasks_view_for_delete, calendar_id, uid, href, etag),
    );
}

/// Routes a saved (created or edited) task to its store - one of: the local
/// on-device store, the owning Google Tasks session, or the owning CalDAV
/// account's session (the `route_calendar_save` counterpart).
pub(crate) fn route_task_save(calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>, calendar_id: CalendarId, task: CalendarTask) {
    if is_local_task_calendar(&calendar_id) {
        let mut st = calendar_state.borrow_mut();
        match st.local_tasks.iter_mut().find(|t| t.uid == task.uid) {
            Some(existing) => *existing = task.clone(),
            None => st.local_tasks.push(task.clone()),
        }
        if let Some(db) = &st.local_tasks_db {
            let _ = db.borrow().save_local_task(&task);
        }
        drop(st);
        refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = google_tasks::google_task_list_id(&calendar_id) {
        let (handle, is_new) = {
            let st = calendar_state.borrow();
            let Some(handle) = google_tasks_handle_for_calendar(&st, &calendar_id) else {
                tracing::warn!("tried to save a task into an unknown Google task list {calendar_id}");
                return;
            };
            (handle.cmd_tx.clone(), !handle.last_tasks.iter().any(|t| t.uid == task.uid))
        };
        let cmd = if is_new {
            GoogleTasksCommand::CreateTask {
                list_id: list_id.to_string(),
                task: Box::new(task),
            }
        } else {
            GoogleTasksCommand::UpdateTask {
                list_id: list_id.to_string(),
                task: Box::new(task),
            }
        };
        let _ = handle.send_blocking(cmd);
        return;
    }
    if let Some(list_id) = lookout_graph::graph_todo_list_id(&calendar_id).map(str::to_string) {
        let command = {
            let st = calendar_state.borrow();
            let Some(handle) = graph_todo_handle_for_calendar(&st, &calendar_id) else {
                tracing::warn!("tried to save a task into an unknown Microsoft To Do list {calendar_id}");
                return;
            };
            let is_new = !handle.last_tasks.iter().any(|t| t.uid == task.uid);
            let command = if is_new {
                lookout_graph::TodoCommand::CreateTask { list_id, task: Box::new(task) }
            } else {
                lookout_graph::TodoCommand::UpdateTask { list_id, task: Box::new(task) }
            };
            (handle.cmd_tx.clone(), command)
        };
        let _ = command.0.send_blocking(command.1);
        return;
    }
    let Some(handle) = task_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to save a task into an unknown calendar {calendar_id}");
        return;
    };
    if task.href.is_some() {
        let _ = handle.send_blocking(CalendarCommand::UpdateTask { task: Box::new(task) });
    } else {
        let _ = handle.send_blocking(CalendarCommand::CreateTask { task: Box::new(task) });
    }
}

/// Routes a task deletion to its store. `uid` is the store's own key for
/// local/Google tasks; `href`/`etag` identify the CalDAV resource.
pub(crate) fn route_task_delete(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    calendar_id: CalendarId,
    uid: TaskUid,
    href: Option<String>,
    etag: Option<String>,
) {
    if is_local_task_calendar(&calendar_id) {
        let mut st = calendar_state.borrow_mut();
        if let Some(db) = &st.local_tasks_db {
            let _ = db.borrow().delete_local_task(&uid.0);
        }
        st.local_tasks.retain(|t| t.uid != uid);
        drop(st);
        refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = google_tasks::google_task_list_id(&calendar_id) {
        let st = calendar_state.borrow();
        let Some(handle) = google_tasks_handle_for_calendar(&st, &calendar_id) else {
            tracing::warn!("tried to delete a task from an unknown Google task list {calendar_id}");
            return;
        };
        // A Google task's uid is its API task id.
        let _ = handle.cmd_tx.send_blocking(GoogleTasksCommand::DeleteTask {
            list_id: list_id.to_string(),
            task_id: uid.0,
        });
        return;
    }
    if let Some(list_id) = lookout_graph::graph_todo_list_id(&calendar_id).map(str::to_string) {
        let st = calendar_state.borrow();
        let Some(handle) = graph_todo_handle_for_calendar(&st, &calendar_id) else {
            tracing::warn!("tried to delete a task from an unknown Microsoft To Do list {calendar_id}");
            return;
        };
        let _ = handle.cmd_tx.send_blocking(lookout_graph::TodoCommand::DeleteTask { list_id, task_id: uid.0, etag });
        return;
    }
    let Some(handle) = task_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to delete a task from an unknown calendar {calendar_id}");
        return;
    };
    let Some(href) = href else {
        tracing::warn!("tried to delete a task without a server href");
        return;
    };
    let _ = handle.send_blocking(CalendarCommand::DeleteTask { calendar_id, href, etag });
}

/// Adds (`add`) or removes the category `keyword` on `summaries`: one
/// `StoreKeywordsMany` per mailbox to its owning account's session, which
/// `STORE`s the atom, patches the cache, and re-emits (repainting the rows'
/// tag dots). Undo restores each message's previous state - only the
/// messages the change actually touched are reversed.
pub(crate) fn send_keyword_store(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary], keyword: &str, add: bool) {
    let changed: Vec<EmailSummary> = summaries.iter().filter(|s| s.keywords.iter().any(|k| k == keyword) != add).cloned().collect();
    let (to_add, to_remove) = if add {
        (vec![keyword.to_string()], Vec::new())
    } else {
        (Vec::new(), vec![keyword.to_string()])
    };
    let mut inverse = Vec::new();
    for (cmd_tx, mailbox, uids) in command_targets_for(state, &changed) {
        inverse.push(AccountCommand::StoreKeywordsMany {
            mailbox: mailbox.clone(),
            uids: uids.clone(),
            add: to_remove.clone(),
            remove: to_add.clone(),
        });
        let _ = cmd_tx.send_blocking(AccountCommand::StoreKeywordsMany {
            mailbox,
            uids,
            add: to_add.clone(),
            remove: to_remove.clone(),
        });
    }
    // Undo: the reverse keyword STORE (item 0.4).
    record_undo(state, if add { "Categorised" } else { "Category removed" }, inverse);
}

/// Applies the tag `key` to every message in the dragged payload - the drop
/// side of the tag menu rows' drop targets. Groups the messages per
/// `(account, mailbox)` and issues one `StoreKeywordsMany` per group, the
/// same way a folder drop groups `MoveMessagesTo`. `false` when the payload
/// isn't a message drag (or empty), letting GTK try other drop targets.
pub(crate) fn handle_keyword_drag_drop(state: &Rc<RefCell<UiState>>, key: &str, value: &glib::Value) -> bool {
    let Ok(bytes) = value.get::<glib::Bytes>() else {
        return false;
    };
    let payload: Vec<(String, u32)> = match serde_json::from_slice(bytes.as_ref()) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("failed to parse message drag payload: {e}");
            return false;
        }
    };
    if payload.is_empty() {
        return false;
    }
    let keyword = lookout_core::tag_keyword(key);
    let mut groups: HashMap<AccountId, (MailboxId, Vec<Uid>)> = HashMap::new();
    for (mailbox, uid) in payload {
        let Some(account_id) = mailbox_account_id(&MailboxId(mailbox.clone())) else { continue };
        let entry = groups.entry(account_id).or_insert_with(|| (MailboxId(mailbox), Vec::new()));
        entry.1.push(Uid(uid));
    }
    let mut sent = false;
    for (account_id, (source, uids)) in groups {
        let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else { continue };
        // Undo: remove the same keyword again (item 0.4).
        record_undo(
            state,
            "Categorised",
            vec![AccountCommand::StoreKeywordsMany {
                mailbox: source.clone(),
                uids: uids.clone(),
                add: Vec::new(),
                remove: vec![keyword.clone()],
            }],
        );
        let _ = cmd_tx.send_blocking(AccountCommand::StoreKeywordsMany {
            mailbox: source,
            uids,
            add: vec![keyword.clone()],
            remove: Vec::new(),
        });
        sent = true;
    }
    sent
}

/// What the message context menu (item 1.17) needs from the window, shared
/// by every row's right-click and the list's Shift+F10 / Menu key.
#[derive(Clone)]
pub(crate) struct MessageMenuContext {
    pub(crate) state: Rc<RefCell<UiState>>,
    pub(crate) worker: Rc<Worker>,
    pub(crate) reading_stack: gtk::Stack,
    pub(crate) message_list: MessageListModel,
    pub(crate) tags: Rc<RefCell<crate::categories::CategorySet>>,
}

/// Where the message context menu was opened.
pub(crate) enum MessageMenuTarget {
    /// A message row (`position` in the list, when known). Boxed, as
    /// `MessageItem::Message` is, since `EmailSummary` is large.
    Row { summary: Box<EmailSummary>, position: Option<u32> },
    /// A conversation header: the menu acts on every message in it.
    Thread(ThreadId),
    /// One message on its own, whatever is selected - a conversation card's
    /// "More actions" (item 1.7).
    Message(Box<EmailSummary>),
    /// The current selection (the keyboard's Shift+F10 / Menu key).
    Selection,
}

/// The messages the menu acts on for `target`, plus the one the
/// single-message verbs (Reply, Forward, View ▸, Open in new window, Print)
/// use. A right-click on a row inside the selection acts on the selection;
/// on any other row it selects that row alone first, as Outlook does. A
/// conversation header acts on the whole conversation, its newest message
/// being the single one.
pub(crate) fn message_menu_targets(message_list: &MessageListModel, target: MessageMenuTarget) -> (Vec<EmailSummary>, Option<EmailSummary>) {
    let selection_targets = |selected: Vec<EmailSummary>| {
        let primary = (selected.len() == 1).then(|| selected[0].clone());
        (selected, primary)
    };
    match target {
        MessageMenuTarget::Row { summary, position } => {
            let summary = *summary;
            let selected = message_list.selected_summaries();
            if selected.iter().any(|s| s.mailbox == summary.mailbox && s.uid == summary.uid) {
                return selection_targets(selected);
            }
            if let Some(position) = position {
                message_list.selection.select_item(position, true);
            }
            (vec![summary.clone()], Some(summary))
        }
        MessageMenuTarget::Thread(id) => {
            let messages = message_list.thread_messages(&id);
            let newest = messages.iter().max_by_key(|s| s.date).cloned();
            (messages, newest)
        }
        MessageMenuTarget::Message(summary) => (vec![(*summary).clone()], Some(*summary)),
        // A selected conversation header acts on the whole conversation,
        // with its newest message for the single-message verbs (item 1.7).
        MessageMenuTarget::Selection => match message_list.selection_kind() {
            SelectionKind::Thread(_, messages) => {
                let newest = crate::message_list::newest_message(&messages);
                (messages, newest)
            }
            _ => selection_targets(message_list.selected_summaries()),
        },
    }
}

/// Opens the message context menu (item 1.17) on `anchor`, pointing at
/// `point` (in `anchor`'s coordinates) or at the whole widget. Returns
/// whether there was anything to act on.
pub(crate) fn open_message_menu(context: &MessageMenuContext, anchor: &gtk::Widget, point: Option<(f64, f64)>, target: MessageMenuTarget) -> bool {
    use crate::message_menu::{FlagEntry, MenuChoices, SnoozeEntry, TargetFacts};

    let (targets, primary) = message_menu_targets(&context.message_list, target);
    if targets.is_empty() {
        return false;
    }
    let state = &context.state;
    let account = mailbox_account_id(&targets[0].mailbox);
    let (menu_state, recent_folders, sender_blocked, focused_inbox, thread_ignored) = {
        let st = state.borrow();
        let facts: Vec<TargetFacts> = targets
            .iter()
            .map(|summary| TargetFacts {
                account: mailbox_account_id(&summary.mailbox).map(|id| id.0).unwrap_or_default(),
                role: mailbox_role(&st, &summary.mailbox),
                unread: summary.is_unread(),
                pinned: summary.is_pinned(),
                draft: summary_is_draft(&st, summary),
                snoozed: lookout_core::snooze_until_from_keywords(&summary.keywords).is_some(),
            })
            .collect();
        let folders = account.as_ref().and_then(|id| st.accounts.get(id)).map(|handle| handle.folders.clone()).unwrap_or_default();
        let has_archive = folders.iter().any(|folder| folder.role == MailboxRole::Archive);
        let menu_state = crate::message_menu::message_menu_state(&facts, primary.is_some(), has_archive);
        // Whether the target sender is already blocked, so Block ▸ reads
        // "Unblock sender" (item 2.9 / M-17).
        let sender_blocked = primary.as_ref().is_some_and(|summary| {
            account.as_ref().is_some_and(|account_id| {
                st.blocked_senders.iter().any(|(blocked_account, entry)| {
                    blocked_account == account_id && summary.from.first().is_some_and(|sender| lookout_core::sender_matches_trust_entry(&sender.address, entry))
                })
            })
        });
        // The account's recent folders that still exist, less the one every
        // target is already in.
        let current = targets.iter().all(|s| s.mailbox == targets[0].mailbox).then(|| targets[0].mailbox.clone());
        let recent: Vec<(MailboxId, String)> = account
            .as_ref()
            .and_then(|id| st.recent_folders.get(id))
            .into_iter()
            .flatten()
            .filter(|id| Some(*id) != current.as_ref())
            .filter_map(|id| {
                folders
                    .iter()
                    .find(|folder| folder.id == *id)
                    .map(|folder| (id.clone(), lookout_core::display_name(&folder.name)))
            })
            .collect();
        // Whether the account provides Focused Inbox (M-03 / item 5.4): a
        // Microsoft 365 account with Graph access.
        let focused_inbox = account
            .as_ref()
            .and_then(|id| st.accounts.get(id))
            .and_then(|handle| handle.graph_mail.clone())
            .is_some_and(|client| client.consented());
        // Whether the target conversation is already ignored (item 2.8 /
        // M-20), so the menu reads "Stop ignoring".
        let thread_ignored = primary.as_ref().is_some_and(|summary| {
            account
                .as_ref()
                .is_some_and(|account_id| st.ignored_threads.contains(&(account_id.clone(), summary.thread_key.0.clone())))
        });
        (menu_state, recent, sender_blocked, focused_inbox, thread_ignored)
    };
    let today = crate::calendar_tz::display_zone().today();
    let prefs = state.borrow().settings.calendar_preferences();
    let now = chrono::Local::now();
    let choices = MenuChoices {
        recent_folders,
        flags: crate::follow_up::flag_presets(today, prefs.week_start, &prefs.work_days)
            .into_iter()
            .map(|preset| FlagEntry {
                name: preset.name.to_string(),
                due: preset.due,
            })
            .collect(),
        snoozes: crate::snooze::snooze_presets(&now)
            .into_iter()
            .map(|preset| SnoozeEntry {
                label: format!("{} ({})", preset.name, crate::datetime_picker::format_short_time(&preset.at, &now)),
                at: preset.at.with_timezone(&chrono::Utc),
            })
            .collect(),
        report_phishing: targets.len() == 1 && !state.borrow().settings.get_string(crate::settings::MAIL_REPORT_PHISHING_ADDRESS).trim().is_empty(),
        sender_blocked,
        focused_inbox,
        thread_ignored,
    };

    let description = crate::message_menu::message_menu_description(&menu_state, &choices);
    let dispatch: crate::context_menu::Dispatch = {
        let context = context.clone();
        let anchor = anchor.downgrade();
        let targets = targets.clone();
        let primary = primary.clone();
        Rc::new(move |name, param| {
            let Some(action) = crate::message_menu::message_action_for(name, param.as_ref()) else {
                return;
            };
            let Some(anchor) = anchor.upgrade() else { return };
            run_message_menu_action(&context, &anchor, &targets, primary.as_ref(), action);
        })
    };
    let menu = crate::context_menu::ContextMenu::new(&description, &crate::message_menu::action_specs(), dispatch);
    for (name, enabled) in crate::message_menu::enabled_actions(&menu_state) {
        menu.set_enabled(name, enabled);
    }
    let categorize = build_tag_menu(&context.tags, state, targets, &context.message_list);
    menu.add_child(&categorize, crate::message_menu::CATEGORIZE_SLOT);

    let popover = menu.popover.clone();
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    popover.set_parent(anchor);
    menu.insert_action_group(anchor, crate::message_menu::NAMESPACE);
    if let Some((x, y)) = point {
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    }
    // Built per opening, so it's dropped once closed - after the activated
    // item has run, hence the idle.
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
    true
}

/// Runs one message context-menu action on `targets` (`primary` for the
/// single-message verbs) through the same helpers the ribbon uses, so every
/// verb behaves - and undoes - the same wherever it's invoked.
pub(crate) fn run_message_menu_action(
    context: &MessageMenuContext,
    anchor: &gtk::Widget,
    targets: &[EmailSummary],
    primary: Option<&EmailSummary>,
    action: crate::message_menu::MessageMenuAction,
) {
    use crate::message_menu::MessageMenuAction as A;
    let state = &context.state;
    let message_list = &context.message_list;
    let app_window = anchor.root().and_downcast::<adw::ApplicationWindow>();
    match action {
        A::Reply | A::ReplyAll | A::Forward => {
            let Some(summary) = primary.cloned() else { return };
            let mode = match action {
                A::Reply => Some(crate::compose::ReplyMode::Reply),
                A::ReplyAll => Some(crate::compose::ReplyMode::ReplyAll),
                _ => None,
            };
            reply_to_message(state, &context.worker, &context.reading_stack, summary, mode);
        }
        A::ReplyWithMeeting => {
            // Open a new event prefilled with the conversation's participants
            // and subject (item 3.19 / C-20). Late-bound like the More menu's
            // "Create task from message…", since the menu is built before
            // `calendar_state` exists.
            let hook = state.borrow().reply_with_meeting.clone();
            if let Some(hook) = hook {
                hook(targets);
            }
        }
        A::Delete => move_messages_to_role(state, message_list, targets, MailboxRole::Trash),
        A::Archive => move_messages_to_role(state, message_list, targets, MailboxRole::Archive),
        A::ReportJunk => set_junk_selection(state, message_list, targets, true),
        A::NotJunk => set_junk_selection(state, message_list, targets, false),
        A::ReportPhishing => report_phishing_selection(state, targets),
        A::CreateRule => {
            // Prefill a rule from the single target and open the editor
            // (item 2.5 / M-14): Rules ▸ Create rule in the message menu.
            if let Some(summary) = primary.cloned() {
                let rule = crate::rules_view::rule_from_message(&summary);
                crate::rules_view::show_rule_editor(anchor, state.clone(), rule, || {});
            }
        }
        A::BlockSender | A::BlockDomain => {
            let Some(summary) = primary.cloned() else { return };
            let domain = matches!(action, A::BlockDomain);
            block_sender_selection(state, message_list, &summary, targets, domain);
        }
        A::UnblockSender => {
            let Some(summary) = primary.cloned() else { return };
            unblock_sender_selection(state, &summary);
        }
        A::Ignore => {
            let Some(summary) = primary.cloned() else { return };
            ignore_conversation(state, message_list, targets, &summary);
        }
        A::StopIgnoring => {
            let Some(summary) = primary.cloned() else { return };
            stop_ignoring_conversation(state, &summary);
        }
        A::CleanUpConversation => {
            let Some(summary) = primary.cloned() else { return };
            clean_up_conversation(state, message_list, targets, &summary);
        }
        A::MoveTo(folder) => move_messages_to_folder(state, message_list, targets, &folder),
        A::CopyTo(folder) => copy_messages_to_folder(state, targets, &folder),
        A::MoveOther | A::CopyOther => {
            let Some(account_id) = targets.first().and_then(|s| mailbox_account_id(&s.mailbox)) else {
                return;
            };
            let folders = state.borrow().accounts.get(&account_id).map(|handle| handle.folders.clone()).unwrap_or_default();
            let current = targets.iter().all(|s| s.mailbox == targets[0].mailbox).then(|| targets[0].mailbox.clone());
            // Folders that can't hold messages (`\Noselect`) aren't offered.
            let rows = crate::folder_menu::folder_picker_rows(folders, &account_id, |folder| {
                Some(&folder.id) == current.as_ref() || folder.flags.iter().any(|flag| flag.eq_ignore_ascii_case("Noselect"))
            });
            let moving = matches!(action, A::MoveOther);
            let noun = if targets.len() == 1 { "item" } else { "items" };
            let heading = format!("{} {} {noun}", if moving { "Move" } else { "Copy" }, targets.len());
            let state = state.clone();
            let message_list = message_list.clone();
            let targets = targets.to_vec();
            crate::folder_menu::present_folder_picker(
                anchor,
                &heading,
                if moving { "Choose the folder to move to." } else { "Choose the folder to copy to." },
                if moving { "Move" } else { "Copy" },
                rows,
                move |folder: MailboxId| {
                    if moving {
                        move_messages_to_folder(&state, &message_list, &targets, &folder);
                    } else {
                        copy_messages_to_folder(&state, &targets, &folder);
                    }
                },
            );
        }
        A::SetRead(mark_read) => set_messages_read(state, message_list, targets, mark_read),
        A::Pin(pinning) => set_messages_pinned(state, message_list, targets, pinning),
        A::Flag(flag) => set_follow_up(state, message_list, targets.to_vec(), flag),
        A::FlagCustom => {
            let state = state.clone();
            let message_list = message_list.clone();
            let targets = targets.to_vec();
            crate::datetime_picker::present_date_dialog(
                Some(anchor),
                "Flag for follow-up",
                "Due date",
                "Flag",
                crate::calendar_tz::display_zone().today(),
                Rc::new(move |due| set_follow_up(&state, &message_list, targets.clone(), crate::follow_up::flagged(Some(due)))),
            );
        }
        A::Snooze(until) => snooze_messages(state, targets, until),
        A::SnoozeCustom => {
            let state = state.clone();
            let targets = targets.to_vec();
            crate::datetime_picker::present_time_dialog(Some(anchor), &snooze_time_dialog(), Rc::new(move |until| snooze_messages(&state, &targets, until)));
        }
        A::Unsnooze => {
            let snoozed: Vec<EmailSummary> = targets
                .iter()
                .filter(|s| lookout_core::snooze_until_from_keywords(&s.keywords).is_some())
                .cloned()
                .collect();
            for (cmd_tx, mailbox, uids) in command_targets_for(state, &snoozed) {
                let _ = cmd_tx.send_blocking(AccountCommand::WakeSnoozed { mailbox, uids });
            }
        }
        A::SetImportance(importance) => set_importance(state, message_list, targets, importance),
        A::MoveFocused(focused) => set_focused(state, message_list, targets, focused_classification(focused), lookout_mail::session::FocusedScope::Message),
        A::AlwaysMoveFocused(focused) => set_focused(state, message_list, targets, focused_classification(focused), lookout_mail::session::FocusedScope::Sender),
        A::StopMovingFocused => {
            let Some(summary) = primary.cloned() else { return };
            stop_moving_focused(state, &summary);
        }
        A::ViewDetails | A::ViewSource => {
            let (Some(summary), Some(parent)) = (primary, app_window) else { return };
            let page = if matches!(action, A::ViewSource) { DetailsPage::Source } else { DetailsPage::Details };
            show_message_details_dialog(state, &parent, summary, page);
        }
        A::OpenWindow => {
            let (Some(summary), Some(parent)) = (primary, app_window) else { return };
            crate::message_window::open_message_window(&parent, state, &context.worker, &context.reading_stack, summary);
        }
        A::Print => {
            let (Some(summary), Some(parent)) = (primary, anchor.root().and_downcast::<gtk::Window>()) else {
                return;
            };
            print_message(state, &parent, summary);
        }
    }
}

/// The Focused/Other classification for a menu choice (`true` = Focused).
fn focused_classification(focused: bool) -> lookout_core::InferenceClassification {
    if focused {
        lookout_core::InferenceClassification::Focused
    } else {
        lookout_core::InferenceClassification::Other
    }
}

/// The Undo-toast wording for an importance change.
fn importance_label(importance: lookout_core::Importance) -> String {
    match importance {
        lookout_core::Importance::High => "Marked high importance".to_string(),
        lookout_core::Importance::Low => "Marked low importance".to_string(),
        lookout_core::Importance::Normal => "Marked normal importance".to_string(),
    }
}

/// The Undo-toast wording for a Focused/Other change.
fn focused_label(classification: lookout_core::InferenceClassification, sender_scope: bool) -> String {
    let bucket = if classification == lookout_core::InferenceClassification::Focused {
        "Focused"
    } else {
        "Other"
    };
    if sender_scope {
        format!("Always moved to {bucket}")
    } else {
        format!("Moved to {bucket}")
    }
}

/// Groups `summaries`' `(mailbox, uids)` pairs by mailbox, for one command per
/// mailbox.
fn groups_by_mailbox(summaries: &[EmailSummary]) -> Vec<(MailboxId, Vec<Uid>)> {
    let mut groups: Vec<(MailboxId, Vec<Uid>)> = Vec::new();
    for summary in summaries {
        match groups.iter_mut().find(|(mailbox, _)| *mailbox == summary.mailbox) {
            Some((_, uids)) => uids.push(summary.uid),
            None => groups.push((summary.mailbox.clone(), vec![summary.uid])),
        }
    }
    groups
}

/// Sets the targets' importance (M-12): shows it at once, sends one
/// `SetImportance` per mailbox, and records an undo that restores each
/// message's previous importance (grouped by that previous value).
pub(crate) fn set_importance(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, targets: &[EmailSummary], importance: lookout_core::Importance) {
    if targets.is_empty() {
        return;
    }
    let mut previous: Vec<(MailboxId, lookout_core::Importance, Vec<Uid>)> = Vec::new();
    for summary in targets {
        let before = summary.importance;
        match previous.iter_mut().find(|(mailbox, importance, _)| *mailbox == summary.mailbox && *importance == before) {
            Some((_, _, uids)) => uids.push(summary.uid),
            None => previous.push((summary.mailbox.clone(), before, vec![summary.uid])),
        }
    }
    for (mailbox, uids) in groups_by_mailbox(targets) {
        optimistic_set_importance(state, message_list, &mailbox, &uids, importance);
        let Some(account_id) = mailbox_account_id(&mailbox) else { continue };
        let Some(cmd_tx) = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone()) else {
            continue;
        };
        let _ = cmd_tx.send_blocking(AccountCommand::SetImportance { mailbox, uids, importance });
    }
    let inverse = previous
        .into_iter()
        .map(|(mailbox, importance, uids)| AccountCommand::SetImportance { mailbox, uids, importance })
        .collect();
    record_undo(state, &importance_label(importance), inverse);
}

/// Shows `importance` on `mailbox`/`uids` in the list (and the unified view's
/// snapshot) before the session confirms it.
fn optimistic_set_importance(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], importance: lookout_core::Importance) {
    let mut changed = false;
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut message| {
            if message.mailbox == *mailbox && uids.contains(&message.uid) {
                message.importance = importance;
                changed = true;
            }
            message
        })
        .collect();
    if changed {
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
    }
    if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(mailbox) {
        for message in snapshot.iter_mut().filter(|message| uids.contains(&message.uid)) {
            message.importance = importance;
        }
    }
}

/// Reclassifies the targets' Focused/Other bucket (M-03): shows it at once,
/// sends one `SetFocused` per mailbox (`scope` deciding whether a sender
/// override is also recorded), and records an undo that restores each
/// message's previous classification. A sender-scope change's override is not
/// undone (the messages are).
pub(crate) fn set_focused(
    state: &Rc<RefCell<UiState>>,
    message_list: &MessageListModel,
    targets: &[EmailSummary],
    classification: lookout_core::InferenceClassification,
    scope: lookout_mail::session::FocusedScope,
) {
    if targets.is_empty() {
        return;
    }
    let mut previous: Vec<(MailboxId, lookout_core::InferenceClassification, Vec<Uid>)> = Vec::new();
    for summary in targets {
        let before = summary.inference_classification;
        match previous
            .iter_mut()
            .find(|(mailbox, classification, _)| *mailbox == summary.mailbox && *classification == before)
        {
            Some((_, _, uids)) => uids.push(summary.uid),
            None => previous.push((summary.mailbox.clone(), before, vec![summary.uid])),
        }
    }
    for (mailbox, uids) in groups_by_mailbox(targets) {
        optimistic_set_focused(state, message_list, &mailbox, &uids, classification);
        let Some(account_id) = mailbox_account_id(&mailbox) else { continue };
        let Some(cmd_tx) = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone()) else {
            continue;
        };
        let _ = cmd_tx.send_blocking(AccountCommand::SetFocused {
            mailbox,
            uids,
            classification,
            scope,
        });
    }
    let inverse = previous
        .into_iter()
        .map(|(mailbox, classification, uids)| AccountCommand::SetFocused {
            mailbox,
            uids,
            classification,
            scope: lookout_mail::session::FocusedScope::Message,
        })
        .collect();
    record_undo(state, &focused_label(classification, scope == lookout_mail::session::FocusedScope::Sender), inverse);
}

/// Shows `classification` on `mailbox`/`uids` in the list (and the unified
/// view's snapshot) before the session confirms it.
fn optimistic_set_focused(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], classification: lookout_core::InferenceClassification) {
    let mut changed = false;
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut message| {
            if message.mailbox == *mailbox && uids.contains(&message.uid) {
                message.inference_classification = classification;
                changed = true;
            }
            message
        })
        .collect();
    if changed {
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
    }
    if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(mailbox) {
        for message in snapshot.iter_mut().filter(|message| uids.contains(&message.uid)) {
            message.inference_classification = classification;
        }
    }
}

/// "Stop moving from this sender" (M-03): delete the sender's Focused/Other
/// override on Graph and drop the local row. Uses the id from the last
/// override sync when it's known, else a live lookup.
pub(crate) fn stop_moving_focused(state: &Rc<RefCell<UiState>>, summary: &EmailSummary) {
    let Some(account_id) = mailbox_account_id(&summary.mailbox) else {
        return;
    };
    let (client, cmd_tx, read_cache) = {
        let st = state.borrow();
        let Some(handle) = st.accounts.get(&account_id) else { return };
        let Some(client) = handle.graph_mail.clone().filter(|client| client.consented()) else {
            return;
        };
        (client, handle.cmd_tx.clone(), handle.address_cache.clone())
    };
    let Some(sender) = summary.from.first().cloned() else {
        return;
    };
    let local_id = read_cache.as_ref().and_then(|cache| cache.inference_overrides().ok()).and_then(|rows| {
        rows.into_iter()
            .find(|row| row.sender.eq_ignore_ascii_case(&sender.address))
            .and_then(|row| row.override_id)
    });
    // Drop the local row whatever Graph says.
    let _ = cmd_tx.send_blocking(AccountCommand::ClearInferenceOverride { sender: sender.address.clone() });
    glib::spawn_future_local(async move {
        let ids: Vec<String> = match local_id {
            Some(id) => vec![id],
            None => match client.list_overrides().await {
                Ok(overrides) => overrides
                    .into_iter()
                    .filter(|override_entry| override_entry.sender.address.eq_ignore_ascii_case(&sender.address))
                    .map(|override_entry| override_entry.id)
                    .collect(),
                Err(e) => {
                    tracing::warn!("couldn't read the Focused/Other overrides: {e}");
                    return;
                }
            },
        };
        for id in ids {
            if let Err(e) = client.delete_override(&id).await {
                tracing::warn!("couldn't delete the Focused/Other override for {}: {e}", sender.address);
            }
        }
    });
}

/// The Snooze menus' "Choose a date…" dialog (the ribbon's and the message
/// context menu's).
pub(crate) fn snooze_time_dialog() -> crate::datetime_picker::TimeDialog {
    crate::datetime_picker::TimeDialog {
        heading: "Snooze",
        field_label: "Snooze until",
        accept: "Snooze",
        note: None,
    }
}

/// Builds the "More" menu popover's contents: "Open in new window" opens the
/// selected message in its own window (item 1.9), "View message details…"
/// shows the details/source dialog (item 1.10), "Save as .eml…" exports the
/// selected message's whole raw RFC 5322 bytes to a file, and "Print…" sends
/// the visible body through WebKit's print pipeline. Rebuilt on every popover
/// `show` (like the categorize menu) so each item's sensitivity tracks
/// whether a message is selected when the menu opens.
pub(crate) fn build_more_menu(
    has_selection: bool,
    message_list: &MessageListModel,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    popover: &gtk::Popover,
) -> gtk::Box {
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    // "Reply with meeting…" (item 3.19 / C-20): a new event prefilled with
    // the conversation's participants and subject.
    let reply_meeting = gtk::Button::builder().label("Reply with meeting…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    reply_meeting.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        reply_meeting.connect_clicked(move |_| {
            popover.popdown();
            let messages = message_list.selected_summaries();
            if messages.is_empty() {
                return;
            }
            let hook = state.borrow().reply_with_meeting.clone();
            if let Some(hook) = hook {
                hook(&messages);
            }
        });
    }
    boxed.append(&reply_meeting);
    // "Forward as attachment" (item 1.11 / C-13): the whole message goes out
    // as a `message/rfc822` part, nothing quoted inline (UX-32's reading-pane
    // ⋯ verb).
    let forward_attachment = gtk::Button::builder()
        .label("Forward as attachment")
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .build();
    forward_attachment.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        forward_attachment.connect_clicked(move |_| {
            popover.popdown();
            let Some(summary) = message_list.selected_summary() else { return };
            forward_as_attachment(&state, &summary);
        });
    }
    boxed.append(&forward_attachment);
    let open_window = gtk::Button::builder().label("Open in new window").css_classes(["flat"]).halign(gtk::Align::Start).build();
    open_window.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let popover = popover.clone();
        open_window.connect_clicked(move |_| {
            popover.popdown();
            let Some(parent) = popover.root().and_downcast::<adw::ApplicationWindow>() else {
                return;
            };
            let Some(summary) = message_list.selected_summary() else { return };
            crate::message_window::open_message_window(&parent, &state, &worker, &reading_stack, &summary);
        });
    }
    boxed.append(&open_window);
    let details_button = gtk::Button::builder()
        .label("View message details…")
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .build();
    details_button.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        details_button.connect_clicked(move |_| {
            popover.popdown();
            let Some(parent) = popover.root().and_downcast::<adw::ApplicationWindow>() else {
                return;
            };
            let Some(summary) = message_list.selected_summary() else { return };
            show_message_details_dialog(&state, &parent, &summary, DetailsPage::Details);
        });
    }
    boxed.append(&details_button);
    // "Create task from message…" (the Follow-up button's old job, before
    // item 1.16 made Flag a message-level follow-up flag).
    let create_task = gtk::Button::builder()
        .label("Create task from message…")
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .build();
    create_task.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        create_task.connect_clicked(move |_| {
            popover.popdown();
            let Some(summary) = message_list.selected_summary() else { return };
            let hook = state.borrow().create_task_from_message.clone();
            if let Some(hook) = hook {
                hook(&summary);
            }
        });
    }
    boxed.append(&create_task);
    let save_eml = gtk::Button::builder().label("Save as .eml…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    save_eml.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        save_eml.connect_clicked(move |_| {
            popover.popdown();
            start_raw_message_export(&message_list, &state);
        });
    }
    boxed.append(&save_eml);
    let print_button = gtk::Button::builder().label("Print…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    // Print is only meaningful while a message is actually showing in the
    // reading pane; the selection alone isn't enough (it may be a selection
    // the pane hasn't rendered yet, or nothing rendered at all).
    print_button.set_sensitive(has_selection && matches!(reading_stack.visible_child_name().as_deref(), Some("message") | Some("conversation")));
    {
        let popover = popover.clone();
        let reading_stack = reading_stack.clone();
        let state = state.clone();
        print_button.connect_clicked(move |_| {
            popover.popdown();
            let Some(parent) = popover.root().and_downcast::<gtk::Window>() else { return };
            print_reading_pane(&state, &reading_stack, &parent);
        });
    }
    boxed.append(&print_button);
    boxed
}

/// Prints what the reading pane shows: the message on the "message" page,
/// or - on the conversation page (item 1.7) - the selected card's message,
/// else the newest, as Outlook prints the selected item. Nothing otherwise.
pub(crate) fn print_reading_pane(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, parent: &gtk::Window) {
    match reading_stack.visible_child_name().as_deref() {
        Some("message") => print_visible_message(reading_stack, parent),
        Some("conversation") => {
            let summary = {
                let st = state.borrow();
                st.rendered_conversation.as_ref().and_then(|pane| {
                    let messages: Vec<EmailSummary> = pane.cards.iter().map(|card| card.member.summary.clone()).collect();
                    pane.focus
                        .as_ref()
                        .and_then(|(mailbox, uid)| messages.iter().find(|m| &m.mailbox == mailbox && m.uid == *uid).cloned())
                        .or_else(|| crate::message_list::newest_message(&messages))
                })
            };
            if let Some(summary) = summary {
                print_message(state, parent, &summary);
            }
        }
        _ => {}
    }
}

/// Prints the currently-visible message body. HTML bodies print directly
/// through the reading pane's WebView; plain-text bodies (rendered in the
/// `GtkTextView` fallback) are wrapped in a minimal HTML page and printed
/// through an offscreen WebView, so every message goes through WebKit's
/// print pipeline.
pub(crate) fn print_visible_message(reading_stack: &gtk::Stack, parent: &gtk::Window) {
    let Some(content_stack) = find_named_child(reading_stack, "body").and_then(|child| child.downcast::<gtk::Stack>().ok()) else {
        return;
    };
    match content_stack.visible_child_name().as_deref() {
        Some("html") => {
            if let Some(web_view) = content_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
                let _ = webkit::PrintOperation::new(&web_view).run_dialog(Some(parent));
            }
        }
        Some("text") => {
            let text = content_stack
                .child_by_name("text")
                .and_downcast::<gtk::ScrolledWindow>()
                .and_then(|scroller| scroller.child())
                .and_downcast::<gtk::TextView>()
                .map(|text_view| text_view.buffer().text(&text_view.buffer().start_iter(), &text_view.buffer().end_iter(), false))
                .unwrap_or_default();
            print_html_once(&plain_text_print_html(&text), parent);
        }
        _ => {}
    }
}

/// A plain-text body as a minimal printable HTML page.
pub(crate) fn plain_text_print_html(text: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <style>body {{ font-family: sans-serif; white-space: pre-wrap; margin: 1em; }}</style>\
         </head><body>{}</body></html>",
        glib::markup_escape_text(text)
    )
}

/// Prints an HTML string through a throwaway offscreen WebView once its load
/// finishes; the WebKit print dialog is modal, so the page only needs to be
/// parsed far enough to lay out - it never appears on screen.
pub(crate) fn print_html_once<T: IsA<gtk::Window>>(html: &str, parent: &T) {
    let parent = parent.clone();
    let print_settings = webkit::Settings::new();
    print_settings.set_hardware_acceleration_policy(webkit::HardwareAccelerationPolicy::Never);
    let web_view = webkit::WebView::builder().settings(&print_settings).build();
    web_view.connect_load_changed(move |web_view, event| {
        if event == webkit::LoadEvent::Finished {
            let _ = webkit::PrintOperation::new(web_view).run_dialog(Some(&parent));
        }
    });
    web_view.load_html(html, None);
}

/// Persists the live category set into `settings.json` (item 2.1). The live
/// `CategorySet` is the working copy the whole UI reads; this mirrors it into
/// `AppConfig::categories` and writes the file, so a change survives restart.
pub(crate) fn save_categories(state: &Rc<RefCell<UiState>>, set: &crate::categories::CategorySet) {
    let app_config = state.borrow().app_config.clone();
    app_config.borrow_mut().categories = set.categories.clone();
    crate::app_config::save(&app_config.borrow());
}

/// Builds the category menu shown by both the toolbar Categorize popover and
/// the message context menu's Categorize ▸ (item 2.1 / M-09): one toggle row
/// per defined category (a colour dot plus a check button - checked when every
/// target carries that category's keyword, mixed when only some do), then a
/// "Manage categories…" row. `targets` are the messages the toggles act on,
/// all at once; none (nothing selected) renders the toggles disabled. The
/// caller owns showing it - for the toolbar it's the popover child (rebuilt on
/// every `show`), for the context menu a custom slot.
pub(crate) fn build_tag_menu(
    tags: &Rc<RefCell<crate::categories::CategorySet>>,
    state: &Rc<RefCell<UiState>>,
    targets: Vec<EmailSummary>,
    message_list: &MessageListModel,
) -> gtk::Box {
    let set = tags.borrow();
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    if set.categories.is_empty() {
        let empty = gtk::Label::builder()
            .label("No categories defined yet")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .margin_top(2)
            .margin_bottom(2)
            .build();
        crate::privacy::mark_ui(&empty);
        boxed.append(&empty);
    } else {
        for tag in &set.categories {
            let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
            let dot = preset_dot_area(tag.color(), 10);
            let toggle = gtk::CheckButton::builder().label(&tag.name).build();
            // A category name is content: blurred by privacy mode.
            crate::privacy::mark_content(&toggle);
            let keyword = tag.keyword();
            let (active, inconsistent) = tag_check_state(&targets, &keyword);
            toggle.set_active(active);
            toggle.set_inconsistent(inconsistent);
            toggle.set_sensitive(!targets.is_empty());
            row.append(&dot);
            row.append(&toggle);

            let targets = targets.clone();
            let state = state.clone();
            let state_for_toggle = state.clone();
            toggle.connect_toggled(move |t| {
                // A mixed check applies the category to every target on its
                // first click, as Outlook does.
                t.set_inconsistent(false);
                send_keyword_store(&state_for_toggle, &targets, &keyword, t.is_active());
            });
            // Dropping messages onto the category row applies that category to
            // all of them - the batch counterpart of the toggles (one
            // `StoreKeywordsMany` per source mailbox).
            {
                let drop_state = state.clone();
                let drop_key = tag.key.clone();
                let row_for_enter = row.clone();
                let row_for_leave = row.clone();
                let drop_target = gtk::DropTarget::builder()
                    .formats(&gtk::gdk::ContentFormats::for_type(glib::Bytes::static_type()))
                    .actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE)
                    .build();
                drop_target.connect_drop(move |_target, value, _x, _y| handle_keyword_drag_drop(&drop_state, &drop_key, value));
                drop_target.connect_enter(move |_target, _x, _y| {
                    row_for_enter.add_css_class("lookout-drop-target");
                    gtk::gdk::DragAction::COPY
                });
                drop_target.connect_leave(move |_target| {
                    row_for_leave.remove_css_class("lookout-drop-target");
                });
                row.add_controller(drop_target);
            }
            boxed.append(&row);
        }
    }

    let manage = gtk::Button::builder().label("Manage categories…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    {
        let tags = tags.clone();
        let state = state.clone();
        let message_list = message_list.clone();
        let manage_button = manage.clone();
        manage.connect_clicked(move |_| {
            show_manage_categories_dialog(manage_button.upcast_ref::<gtk::Widget>(), state.clone(), tags.clone(), message_list.clone());
        });
    }
    boxed.append(&manage);
    boxed
}

/// A Categorize check's `(active, inconsistent)` state for `targets`:
/// checked when every target carries `keyword`, mixed when only some do.
pub(crate) fn tag_check_state(targets: &[EmailSummary], keyword: &str) -> (bool, bool) {
    let carrying = targets.iter().filter(|s| s.keywords.iter().any(|k| k == keyword)).count();
    (carrying > 0 && carrying == targets.len(), carrying > 0 && carrying < targets.len())
}

/// The classic category shortcut's function-key number for a pressed key:
/// `F2`–`F12` map to `2`–`12` (rendered as `Ctrl+F2`…`Ctrl+F12`, item 2.1).
/// Any other key is `None`.
pub(crate) fn category_function_key(key: gtk::gdk::Key) -> Option<u8> {
    use gtk::gdk::Key;
    let n = match key {
        Key::F2 => 2,
        Key::F3 => 3,
        Key::F4 => 4,
        Key::F5 => 5,
        Key::F6 => 6,
        Key::F7 => 7,
        Key::F8 => 8,
        Key::F9 => 9,
        Key::F10 => 10,
        Key::F11 => 11,
        Key::F12 => 12,
        _ => return None,
    };
    Some(n)
}

/// The category-management editor (item 2.1 / M-09, CAL-07, T-08): lists the
/// shared master list - a coloured preset swatch, an editable name, an
/// optional classic shortcut (Ctrl+F2…F12) and a delete button per row - with
/// an "add" row for new ones. Mutates the live `CategorySet`, persists it into
/// `settings.json` (`save_categories`), pushes the change to any connected
/// Microsoft 365 account (`push_categories_to_graph`), and forces the message
/// list to re-render - a recolour/rename doesn't change any message's keywords,
/// so the list's no-op check would otherwise skip the rebuild.
///
/// Deleting a category is non-destructive: only the definition is removed. The
/// `$Lookout-tag-*` keywords already stored on messages stay on the server and
/// simply stop displaying.
pub(crate) fn show_manage_categories_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, tags: Rc<RefCell<crate::categories::CategorySet>>, message_list: MessageListModel) {
    let window = anchor.root().and_downcast::<gtk::Window>();
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Categories").default_width(500).default_height(560);
        if let Some(win) = window {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder().child(&list).vexpand(true).build();

    // `rebuild` re-renders the category list; its per-row handlers call it
    // again after an edit. An `Rc<RefCell<Box<dyn Fn()>>>` so a handler
    // created while it runs can still reach it. (The handlers clone the `Rc`,
    // which forms a reference cycle that lives until the dialog's widgets
    // drop - a bounded, one-off cost for a modal dialog.)
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    {
        let list = list.clone();
        let tags_rc = tags.clone();
        let state_rc = state.clone();
        let message_list_rc = message_list.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let set = tags_rc.borrow();
            if set.categories.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No categories yet - add one below.")
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                crate::privacy::mark_ui(&empty);
                list.append(&empty);
            }
            for tag in &set.categories {
                let row = gtk::ListBoxRow::new();
                let row_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(10)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(10)
                    .margin_end(10)
                    .build();

                let key = tag.key.clone();
                let preset_picker = build_preset_picker(tag.preset, {
                    let tags = tags_rc.clone();
                    let state = state_rc.clone();
                    let message_list = message_list_rc.clone();
                    let rebuild = rebuild_handle.clone();
                    let key = key.clone();
                    Rc::new(move |preset: u8| {
                        if let Some(tag) = tags.borrow_mut().categories.iter_mut().find(|t| t.key == key) {
                            tag.preset = preset;
                        }
                        save_categories(&state, &tags.borrow());
                        push_categories_to_graph(&state);
                        message_list.refresh();
                        (rebuild.borrow())();
                    })
                });

                let name = gtk::Entry::builder().text(&tag.name).hexpand(true).build();

                let shortcut_picker = build_shortcut_picker(tag.shortcut, {
                    let tags = tags_rc.clone();
                    let state = state_rc.clone();
                    let message_list = message_list_rc.clone();
                    let rebuild = rebuild_handle.clone();
                    let key = key.clone();
                    Rc::new(move |shortcut: Option<u8>| {
                        let mut set = tags.borrow_mut();
                        // A key can belong to only one category: clear it from
                        // any other holder before assigning it.
                        if let Some(n) = shortcut {
                            for t in set.categories.iter_mut() {
                                if t.shortcut == Some(n) && t.key != key {
                                    t.shortcut = None;
                                }
                            }
                        }
                        if let Some(tag) = set.categories.iter_mut().find(|t| t.key == key) {
                            tag.shortcut = shortcut;
                        }
                        drop(set);
                        save_categories(&state, &tags.borrow());
                        message_list.refresh();
                        (rebuild.borrow())();
                    })
                });

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.set_tooltip_text(Some("Delete category"));
                delete.add_css_class("flat");

                row_box.append(&preset_picker);
                row_box.append(&name);
                row_box.append(&shortcut_picker);
                row_box.append(&delete);
                row.set_child(Some(&row_box));
                list.append(&row);

                {
                    let tags = tags_rc.clone();
                    let state = state_rc.clone();
                    let message_list = message_list_rc.clone();
                    let key = key.clone();
                    name.connect_changed(move |entry| {
                        let mut set = tags.borrow_mut();
                        if let Some(tag) = set.categories.iter_mut().find(|t| t.key == key) {
                            tag.name = entry.text().trim().to_string();
                        }
                        drop(set);
                        save_categories(&state, &tags.borrow());
                        message_list.refresh();
                    });
                }

                {
                    let tags = tags_rc.clone();
                    let state = state_rc.clone();
                    let message_list = message_list_rc.clone();
                    let rebuild = rebuild_handle.clone();
                    let key = key.clone();
                    delete.connect_clicked(move |_| {
                        tags.borrow_mut().categories.retain(|t| t.key != key);
                        save_categories(&state, &tags.borrow());
                        push_categories_to_graph(&state);
                        message_list.refresh();
                        (rebuild.borrow())();
                    });
                }
            }
        });
    }
    (rebuild.borrow())();

    // --- Add-a-category row: name entry + preset picker + Add button ---
    let add_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();
    let new_name = gtk::Entry::builder().placeholder_text("New category name").hexpand(true).build();
    let new_preset = Rc::new(RefCell::new(crate::categories::default_preset(tags.borrow().categories.len())));
    let new_preset_picker = build_preset_picker(*new_preset.borrow(), {
        let new_preset = new_preset.clone();
        Rc::new(move |preset: u8| *new_preset.borrow_mut() = preset)
    });
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_row.append(&new_name);
    add_row.append(&new_preset_picker);
    add_row.append(&add_button);

    let error_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .css_classes(["error"])
        .visible(false)
        .margin_start(10)
        .margin_end(10)
        .build();
    crate::privacy::mark_ui(&error_label);
    {
        let tags = tags.clone();
        let state = state.clone();
        let new_name = new_name.clone();
        let new_preset = new_preset.clone();
        let error_label = error_label.clone();
        let message_list = message_list.clone();
        let rebuild = rebuild.clone();
        add_button.connect_clicked(move |_| {
            let key = lookout_core::sanitize_tag_key(new_name.text().trim());
            let name = new_name.text().trim().to_string();
            if name.is_empty() || key.is_empty() {
                error_label.set_label("Enter a category name.");
                error_label.set_visible(true);
                return;
            }
            if tags.borrow().contains_key(&key) || tags.borrow().contains_name(&name) {
                error_label.set_label("A category with this name already exists.");
                error_label.set_visible(true);
                return;
            }
            let preset = *new_preset.borrow();
            tags.borrow_mut().categories.push(crate::categories::CategoryDef::new(key, name, preset));
            save_categories(&state, &tags.borrow());
            push_categories_to_graph(&state);
            error_label.set_visible(false);
            new_name.set_text("");
            *new_preset.borrow_mut() = crate::categories::default_preset(tags.borrow().categories.len());
            message_list.refresh();
            (rebuild.borrow())();
        });
    }

    let close_button = gtk::Button::with_label("Close");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    content.append(&scroller);
    content.append(&add_row);
    content.append(&error_label);
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    close_button.set_halign(gtk::Align::End);
    close_button.set_margin_end(12);
    content.append(&close_button);
    dialog.set_child(Some(&content));
    dialog.present();

    // Opening the editor is itself a definition view change that can alter row
    // colours if the file changed under us; a refresh is cheap and correct.
    message_list.refresh();
}

/// A filled colour dot for a preset, painted directly (no CSS classes needed).
/// Shared with the event/task editors' category pickers.
pub(crate) fn preset_dot_area(rgb: &str, size: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().width_request(size).height_request(size).build();
    let rgba = hex_to_rgba(rgb);
    area.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgb(rgba.red() as f64, rgba.green() as f64, rgba.blue() as f64);
        let radius = (w.min(h) as f64) / 2.0 - 1.0;
        if radius > 0.0 {
            cr.arc(w as f64 / 2.0, h as f64 / 2.0, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    });
    area
}

/// A `MenuButton` showing the current category colour, whose popover is a 5×5
/// grid of Outlook's 25 presets (UX-85). Picking one calls `on_pick` with the
/// preset index and closes the popover.
pub(crate) fn build_preset_picker(current: u8, on_pick: Rc<dyn Fn(u8)>) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder().tooltip_text("Category colour").css_classes(["flat"]).build();
    let preset = lookout_core::preset_by_index(current).unwrap_or(&lookout_core::OUTLOOK_CATEGORY_PRESETS[0]);
    button.set_child(Some(&preset_dot_area(preset.rgb, 18)));

    let grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(4)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    for (i, p) in lookout_core::OUTLOOK_CATEGORY_PRESETS.iter().enumerate() {
        let swatch = gtk::Button::builder().css_classes(["flat", "circular"]).tooltip_text(p.name).build();
        swatch.set_child(Some(&preset_dot_area(p.rgb, 16)));
        let on_pick = on_pick.clone();
        let button_for_close = button.clone();
        swatch.connect_clicked(move |_| {
            on_pick(i as u8);
            if let Some(pop) = button_for_close.popover() {
                pop.popdown();
            }
        });
        grid.attach(&swatch, (i % 5) as i32, (i / 5) as i32, 1, 1);
    }
    let popover = gtk::Popover::builder().child(&grid).build();
    button.set_popover(Some(&popover));
    button
}

/// A `MenuButton` for a category's classic shortcut (Ctrl+F2…F12). Its popover
/// lists "None" plus every function key; picking one calls `on_pick`.
pub(crate) fn build_shortcut_picker(current: Option<u8>, on_pick: Rc<dyn Fn(Option<u8>)>) -> gtk::MenuButton {
    let label = current.map(|n| format!("Ctrl+F{n}")).unwrap_or_else(|| "Shortcut".into());
    let button = gtk::MenuButton::builder()
        .label(label)
        .tooltip_text("Classic category shortcut")
        .css_classes(["flat"])
        .build();
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    let mut options: Vec<(String, Option<u8>)> = vec![("None".to_string(), None)];
    options.extend(crate::categories::SHORTCUT_KEYS.map(|n| (format!("Ctrl+F{n}"), Some(n))));
    for (text, value) in options {
        let b = gtk::Button::builder().label(&text).css_classes(["flat"]).halign(gtk::Align::Fill).build();
        let on_pick = on_pick.clone();
        let button_for_close = button.clone();
        b.connect_clicked(move |_| {
            on_pick(value);
            if let Some(pop) = button_for_close.popover() {
                pop.popdown();
            }
        });
        list.append(&b);
    }
    let popover = gtk::Popover::builder().child(&list).build();
    button.set_popover(Some(&popover));
    button
}

/// `#rrggbb` -> `gdk::RGBA`. Any malformed input degrades to a mid-grey
/// rather than failing, matching the "colours are cosmetic" spirit of
/// `calendar_colors::resolve_color`.
pub(crate) fn hex_to_rgba(color: &str) -> gtk::gdk::RGBA {
    let body = color.strip_prefix('#').unwrap_or(color);
    let byte = |i: usize| u8::from_str_radix(&body[i..i + 2], 16).unwrap_or(128);
    let (r, g, b) = match body.len() {
        6 if body.bytes().all(|c| c.is_ascii_hexdigit()) => (byte(0), byte(2), byte(4)),
        _ => (128, 128, 128),
    };
    gtk::gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// Switches the message list to a single mailbox and asks its owning account
/// to sync it. Shared by the folder-selection handler and the account
/// switcher's fallback path.
pub(crate) fn select_mailbox(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    account_id: AccountId,
    mailbox_id: MailboxId,
) {
    {
        let mut st = state.borrow_mut();
        st.mail_view = MailView::Single;
        st.current_account = Some(account_id.clone());
        st.current_mailbox = Some(mailbox_id.clone());
        st.restore_pending = false;
        last_view::save(
            &st.settings,
            &LastSelection {
                unified: false,
                mailbox: Some(mailbox_id.0.clone()),
            },
        );
    }
    // Clear immediately rather than leaving the previously-selected folder's
    // rows on screen - the actual cached content for the new folder paints
    // in a moment later, once the off-thread read (below) answers. Mirrors
    // `exit_search`'s single-view repaint.
    let (key, descending) = current_sort(state);
    message_list.repopulate(Vec::new(), key, descending);
    request_mailbox_sync(state, &account_id, &mailbox_id);
    refresh_message_loading_state(state, message_list, message_list_stack);

    if let Some(cache) = state.borrow().accounts.get(&account_id).and_then(|h| h.address_cache.clone()) {
        // Captured just before the read is dispatched so the reply can tell
        // whether a live sync landed while it was in flight (see
        // `UiState::mailbox_sync_epoch`).
        let epoch = state.borrow().mailbox_sync_epoch.get(&mailbox_id).copied().unwrap_or(0);
        let reply_rx = spawn_cache_read(worker, cache, {
            let mailbox_id = mailbox_id.clone();
            move |cache| {
                let mut messages = cache.load_messages(&mailbox_id).ok()?;
                let snoozed = cache.active_snoozed_uids(&mailbox_id, chrono::Utc::now()).unwrap_or_default();
                messages.retain(|m| !snoozed.contains(&m.uid));
                Some(messages)
            }
        });
        let state = state.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        glib::spawn_future_local(async move {
            let Ok(Some(cached)) = reply_rx.recv().await else { return };
            // Discard a reply for a mailbox the user has since switched away
            // from - applying it now would repaint the wrong folder. Also
            // drop any pending `open-message` selection: it was queued for
            // whatever navigation triggered *that* switch, and leaving it
            // stashed could misfire on some later, unrelated load.
            if state.borrow().current_mailbox.as_ref() != Some(&mailbox_id) {
                state.borrow_mut().pending_message_selection = None;
                return;
            }
            // If a live sync for this mailbox landed since this read was
            // dispatched, its `MessagesUpdated` already painted the freshest
            // set. A window read from disk *before* that sync - empty for a
            // folder the background prefetch has only indexed as `partial`
            // search rows - must not repaint over it.
            if state.borrow().mailbox_sync_epoch.get(&mailbox_id).copied().unwrap_or(0) != epoch {
                state.borrow_mut().pending_message_selection = None;
                return;
            }
            let (key, descending) = current_sort(&state);
            message_list.repopulate(cached, key, descending);
            // The list just painted from cache - clear the spinner now rather
            // than leaving it up until the separate live IMAP sync's
            // `MessagesUpdated` arrives. Without this, a cache hit still shows
            // "loading" for the full round trip to the account session and
            // back, even though the correct rows are already on screen
            // underneath it.
            refresh_message_loading_state(&state, &message_list, &message_list_stack);
            // Consume a pending `app.open-message` selection (an AI Chat
            // link click) now that this mailbox's data has actually landed.
            if let Some((pending_mailbox, pending_uid)) = state.borrow_mut().pending_message_selection.take() {
                if pending_mailbox == mailbox_id {
                    message_list.select_uid(&pending_mailbox, pending_uid);
                }
            }
        });
    }
}

/// Requests a mailbox sync from its account, deduplicating against
/// in-flight requests: a `SyncMailbox` that's already outstanding (sent but
/// not yet answered by a `MessagesUpdated`) suppresses a duplicate. Without
/// this the startup burst - the session's cache replay plus the app's
/// on-demand syncs for the same inbox - queues several identical syncs.
/// Returns whether a request was actually sent. Pending entries are cleared
/// by the `MessagesUpdated` that answers them and by a `FoldersUpdated`
/// (every reconnect re-lists folders), so a request that dies with a dropped
/// connection can never permanently suppress a later one.
pub(crate) fn request_mailbox_sync(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox_id: &MailboxId) -> bool {
    if state.borrow().syncing.contains(mailbox_id) {
        return false;
    }
    set_mailbox_syncing(state, mailbox_id, true);
    let cmd_tx = state.borrow().accounts.get(account_id).map(|handle| handle.cmd_tx.clone());
    if let Some(cmd_tx) = cmd_tx {
        let _ = cmd_tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id.clone()));
    }
    true
}

/// Marks `mailbox_id` syncing or not in `UiState::syncing`, then repaints its
/// folder-pane spinner via `refresh_folder_spinner`. Nothing else prompts a
/// rebind for a syncing-only change: `rebuild_folder_tree`'s signature guard
/// deliberately excludes `syncing`, since folding it in would trigger a full
/// model swap (and the flicker that comes with it) on every sync start/stop,
/// which happens far too often for that. So this is the only path that keeps
/// the sidebar indicator live for a real sync - every mutation of `syncing`
/// should go through it rather than touching the set directly.
pub(crate) fn set_mailbox_syncing(state: &Rc<RefCell<UiState>>, mailbox_id: &MailboxId, syncing: bool) {
    {
        let mut st = state.borrow_mut();
        if syncing {
            st.syncing.insert(mailbox_id.clone());
        } else {
            st.syncing.remove(mailbox_id);
        }
    }
    refresh_folder_spinner(state, mailbox_id);
}

/// `set_mailbox_syncing`'s counterpart for the background body prefetch (see
/// `AccountEvent::PrefetchStarted`/`PrefetchFinished`): marks `mailbox_id` in
/// `UiState::prefetching` rather than `syncing`, so a mailbox mid-prefetch
/// can never suppress `request_mailbox_sync`'s dedup check for a real sync of
/// the same mailbox, then repaints its spinner the same way.
pub(crate) fn set_mailbox_prefetching(state: &Rc<RefCell<UiState>>, mailbox_id: &MailboxId, prefetching: bool) {
    {
        let mut st = state.borrow_mut();
        if prefetching {
            st.prefetching.insert(mailbox_id.clone());
        } else {
            st.prefetching.remove(mailbox_id);
        }
    }
    refresh_folder_spinner(state, mailbox_id);
}

/// Shared repaint step for `set_mailbox_syncing`/`set_mailbox_prefetching`:
/// if `mailbox_id` has any folder-pane row(s) currently bound (in view) -
/// a starred mailbox can have two at once, its own tree row and its
/// Favorites pinned row - shows their spinner exactly while the mailbox is
/// syncing and/or prefetching.
pub(crate) fn refresh_folder_spinner(state: &Rc<RefCell<UiState>>, mailbox_id: &MailboxId) {
    let (spinners, active) = {
        let mut st = state.borrow_mut();
        let active = st.syncing.contains(mailbox_id) || st.prefetching.contains(mailbox_id);
        let spinners = match st.folder_row_spinners.get_mut(mailbox_id) {
            Some(weaks) => {
                let live: Vec<gtk::Spinner> = weaks.iter().filter_map(|weak| weak.upgrade()).collect();
                // Prune dead entries now so the vec doesn't grow unbounded as
                // rows are recycled away over the pane's lifetime.
                weaks.retain(|weak| weak.upgrade().is_some());
                live
            }
            None => Vec::new(),
        };
        (spinners, active)
    };
    for spinner in spinners {
        // The map can hold a stale entry for a row GTK has since recycled to
        // show a different mailbox as the pane scrolls - only apply the
        // toggle if this spinner is still stamped for `mailbox_id` (stamped
        // on every `bind`).
        let stamped = unsafe { spinner.data::<Option<MailboxId>>("lookout-sync-mailbox") };
        let still_current = stamped.is_some_and(|ptr| unsafe { ptr.as_ref() }.as_ref() == Some(mailbox_id));
        if still_current {
            spinner.set_visible(active);
            spinner.set_spinning(active);
        }
    }
}

/// Clears every outstanding sync and prefetch entry belonging to
/// `account_id` - a reconnect (fresh `FoldersUpdated`) or account removal
/// invalidates them all at once, since whatever they were waiting on can now
/// never land (a dropped connection also silently kills the prefetch pass
/// mid-mailbox, with no `PrefetchFinished` ever coming for it) - and turns
/// off any of their folder-pane spinners that are currently bound, the same
/// "don't leave the spinner running for a request that'll never land" case
/// `refresh_message_loading_state` already guards against for the message
/// list.
pub(crate) fn clear_account_syncing(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let cleared: Vec<MailboxId> = {
        let mut st = state.borrow_mut();
        let belongs = |mailbox: &MailboxId| mailbox_account_id(mailbox).as_ref() == Some(account_id);
        let before: HashSet<MailboxId> = st.syncing.iter().chain(st.prefetching.iter()).cloned().collect();
        st.syncing.retain(|mailbox| !belongs(mailbox));
        st.prefetching.retain(|mailbox| !belongs(mailbox));
        before.into_iter().filter(|mailbox| belongs(mailbox)).collect()
    };
    for mailbox_id in cleared {
        refresh_folder_spinner(state, &mailbox_id);
    }
}

/// Shows the "Fetching message list." spinner in place of the message list
/// exactly while the view on screen is empty *and* its sync is still
/// outstanding (tracked in `UiState::syncing`) - never for a folder that's
/// genuinely empty, since nothing ever clears `syncing` for a mailbox that
/// isn't waiting on anything, and a permanently-spinning "fetching" message
/// would be worse than the plain empty list it replaces. Call after anything
/// that can change either input: a `repopulate`, or a `syncing`/
/// `FoldersUpdated` change.
pub(crate) fn refresh_message_loading_state(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, message_list_stack: &gtk::Stack) {
    let empty = message_list.selection.n_items() == 0;
    let waiting = if empty {
        let (mail_view, current_mailbox, syncing) = {
            let st = state.borrow();
            (st.mail_view, st.current_mailbox.clone(), st.syncing.clone())
        };
        match mail_view {
            MailView::Single => current_mailbox.is_some_and(|m| syncing.contains(&m)),
            MailView::UnifiedInbox => account_inboxes(state).into_iter().any(|(_, inbox)| syncing.contains(&inbox)),
            MailView::Search => false,
        }
    } else {
        false
    };
    message_list_stack.set_visible_child_name(if !empty {
        "list"
    } else if waiting {
        "loading"
    } else {
        "empty"
    });
}

/// Every connected account's Inbox - the mailbox set the unified view is
/// built from.
pub(crate) fn account_inboxes(state: &Rc<RefCell<UiState>>) -> Vec<(AccountId, MailboxId)> {
    state
        .borrow()
        .accounts
        .iter()
        .filter_map(|(id, handle)| handle.folders.iter().find(|m| matches!(m.role, MailboxRole::Inbox)).map(|m| (id.clone(), m.id.clone())))
        .collect()
}

/// Re-syncs whatever the message list is currently showing: the open mailbox,
/// or every account's Inbox in the unified view - then, for the same
/// account(s), bumps their standard background refresh along too
/// (`request_account_refresh`) rather than waiting for it to fall due on its
/// own IDLE cadence. Both dedupe against a request already outstanding
/// (`request_mailbox_sync` against `syncing`, `request_account_refresh`
/// against `refreshing`), so pressing Sync repeatedly is a no-op rather than
/// queuing repeat round trips, and each account's background command channel
/// being FIFO means the highlighted folder's own sync always lands first,
/// with the account-wide relist following behind it.
pub(crate) fn resync_current_view(state: &Rc<RefCell<UiState>>) {
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        for (account_id, inbox_id) in account_inboxes(state) {
            request_mailbox_sync(state, &account_id, &inbox_id);
            request_account_refresh(state, &account_id);
        }
        return;
    }
    let current = {
        let st = state.borrow();
        st.current_account.clone().zip(st.current_mailbox.clone())
    };
    if let Some((account_id, mailbox_id)) = current {
        request_mailbox_sync(state, &account_id, &mailbox_id);
        request_account_refresh(state, &account_id);
    }
}

/// Sends `AccountCommand::Refresh` - a folder-list relist plus a resync of
/// whatever mailbox the session currently considers open, outside of IDLE's
/// own cadence - deduplicating against one already outstanding for
/// `account_id` via `UiState::refreshing`, cleared when the account's
/// answering `FoldersUpdated` lands (`Refresh`'s own first step). Without the
/// dedupe, clicking the message list's Sync button again before the previous
/// refresh's relist has landed would queue a second one behind it for no
/// benefit.
pub(crate) fn request_account_refresh(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    if state.borrow().refreshing.contains(account_id) {
        return;
    }
    state.borrow_mut().refreshing.insert(account_id.clone());
    let cmd_tx = state.borrow().accounts.get(account_id).map(|handle| handle.cmd_tx.clone());
    if let Some(cmd_tx) = cmd_tx {
        let _ = cmd_tx.send_blocking(AccountCommand::Refresh);
    }
    sync_mail_properties_from_graph(state, account_id);
    sync_and_push_rules_from_graph(state, account_id);
}

/// Snoozes the selected messages until `until` (item 1.15) and offers Undo.
pub(crate) fn snooze_selection(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, until: chrono::DateTime<chrono::Utc>) {
    snooze_messages(state, &message_list.selected_summaries(), until);
}

/// Snoozes `summaries` until `until` (item 1.15) and offers Undo. The
/// session decides whether the snooze lives on the server (the "Scheduled"
/// folder) or stays local; `UnsnoozeMessages` reverses either.
pub(crate) fn snooze_messages(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary], until: chrono::DateTime<chrono::Utc>) {
    let targets = command_targets_for(state, summaries);
    if targets.is_empty() {
        return;
    }
    let mut inverse = Vec::new();
    for (cmd_tx, mailbox, uids) in targets {
        inverse.push(AccountCommand::UnsnoozeMessages {
            mailbox: mailbox.clone(),
            uids: uids.clone(),
        });
        let _ = cmd_tx.send_blocking(AccountCommand::SnoozeMessages { mailbox, uids, until });
    }
    let when = crate::datetime_picker::format_short_time(&until.with_timezone(&chrono::Local), &chrono::Local::now());
    record_undo(state, &format!("Snoozed until {when}"), inverse);
}

/// Moves `summaries` to their accounts' `role` folder (Delete, Archive) - the
/// ribbon buttons' and the message context menu's shared path. Rows go at
/// once, one `MoveMessages` per source mailbox, and one undo entry covers the
/// whole action, even across accounts: its inverse moves each group back from
/// the role folder it landed in (item 0.4).
pub(crate) fn move_messages_to_role(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], role: MailboxRole) {
    let mut inverse = Vec::new();
    let mut count = 0usize;
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        optimistic_remove_messages(state, message_list, &mailbox, &uids);
        if let Some(destination) = mailbox_account_id(&mailbox).and_then(|account_id| role_mailbox_id(state, &account_id, role)) {
            inverse.push(AccountCommand::RevertMove {
                moved_from: mailbox.clone(),
                moved_to: destination,
                uids: uids.clone(),
            });
        }
        count += uids.len();
        let _ = cmd_tx.send_blocking(AccountCommand::MoveMessages { mailbox, uids, role });
    }
    record_undo(state, &undo_move_description(role, count), inverse);
}

/// Reports `summaries` as junk (`junk == true`) or marks them not junk (item
/// 1.24 / M-17): sets the RFC 5788 `$Junk` / `$NotJunk` keyword so server
/// spam filters learn from the decision, and moves the messages to the Junk
/// folder - or back to the Inbox for "not junk". The `SetJunk` and
/// `MoveMessages` commands race in order through the same session, so the
/// keyword is on the message before it moves. One undo entry covers the whole
/// action; its `RevertMove` clears the junk keyword as it moves the messages
/// back.
pub(crate) fn set_junk_selection(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], junk: bool) {
    let role = if junk { MailboxRole::Junk } else { MailboxRole::Inbox };
    let mut inverse = Vec::new();
    let mut count = 0usize;
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        optimistic_remove_messages(state, message_list, &mailbox, &uids);
        if let Some(destination) = mailbox_account_id(&mailbox).and_then(|account_id| role_mailbox_id(state, &account_id, role)) {
            inverse.push(AccountCommand::RevertMove {
                moved_from: mailbox.clone(),
                moved_to: destination,
                uids: uids.clone(),
            });
        }
        count += uids.len();
        let _ = cmd_tx.send_blocking(AccountCommand::SetJunk {
            mailbox: mailbox.clone(),
            uids: uids.clone(),
            junk,
        });
        let _ = cmd_tx.send_blocking(AccountCommand::MoveMessages { mailbox, uids, role });
    }
    let description = if junk {
        undo_move_description(MailboxRole::Junk, count)
    } else {
        let noun = if count == 1 { "item" } else { "items" };
        format!("{count} {noun} marked as not junk")
    };
    record_undo(state, &description, inverse);
}

/// Blocks the sender of `summary` (item 2.9 / M-17): adds the exact address -
/// or, for `domain`, every address on its domain - to the receiving account's
/// blocked list, drops any conflicting safe-sender entry (a sender is never
/// both safe and blocked), then reports the selected `targets` as junk. Future
/// arrivals from the sender are moved to Junk by
/// `rules::enforce_blocked_senders`.
pub(crate) fn block_sender_selection(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summary: &EmailSummary, targets: &[EmailSummary], domain: bool) {
    let Some(account) = mailbox_account_id(&summary.mailbox) else { return };
    let Some(sender) = summary.from.first() else { return };
    let entry = if domain {
        sender
            .address
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .and_then(|domain| lookout_core::normalize_trust_entry(&format!("@{domain}")))
    } else {
        lookout_core::normalize_trust_entry(&sender.address)
    };
    let Some(entry) = entry else {
        if let Some(overlay) = &state.borrow().toast_overlay {
            overlay.add_toast(adw::Toast::new("That sender can't be blocked."));
        }
        return;
    };
    {
        let mut st = state.borrow_mut();
        st.blocked_senders.insert((account.clone(), entry.clone()));
        // Remove any safe-sender entry for the same address/domain so the two
        // lists stay mutually exclusive.
        let conflicting: Vec<String> = st
            .trusted_senders
            .keys()
            .filter(|(trusted_account, trusted_entry)| trusted_account == &account && lookout_core::sender_matches_trust_entry(&sender.address, trusted_entry))
            .map(|(_, trusted_entry)| trusted_entry.clone())
            .collect();
        for trusted_entry in conflicting {
            st.trusted_senders.remove(&(account.clone(), trusted_entry.clone()));
            if let Some(db) = &st.ui_db {
                let _ = db.remove_trusted_sender(&account, &trusted_entry);
            }
        }
        if let Some(db) = &st.ui_db {
            let _ = db.set_blocked_sender(&account, &entry);
        }
    }
    set_junk_selection(state, message_list, targets, true);
    if let Some(overlay) = &state.borrow().toast_overlay {
        let label = if domain { "Sender's domain blocked" } else { "Sender blocked" };
        overlay.add_toast(adw::Toast::new(label));
    }
}

/// Removes the sender of `summary` - exact or matching `@domain` entry - from
/// the account's blocked list (item 2.9 / M-17).
pub(crate) fn unblock_sender_selection(state: &Rc<RefCell<UiState>>, summary: &EmailSummary) {
    let Some(account) = mailbox_account_id(&summary.mailbox) else { return };
    let Some(sender) = summary.from.first() else { return };
    let mut removed = false;
    {
        let mut st = state.borrow_mut();
        let matches: Vec<String> = st
            .blocked_senders
            .iter()
            .filter(|(blocked_account, entry)| blocked_account == &account && lookout_core::sender_matches_trust_entry(&sender.address, entry))
            .map(|(_, entry)| entry.clone())
            .collect();
        for entry in matches {
            st.blocked_senders.remove(&(account.clone(), entry.clone()));
            if let Some(db) = &st.ui_db {
                let _ = db.remove_blocked_sender(&account, &entry);
            }
            removed = true;
        }
    }
    if removed {
        if let Some(overlay) = &state.borrow().toast_overlay {
            overlay.add_toast(adw::Toast::new("Sender unblocked"));
        }
    }
}

/// Whether `text` is entirely quoted/attribution - no unquoted content of its
/// own (item 2.8 / M-20). Used by Clean up conversation to spot a message that
/// only repeats a later one; deliberately conservative, recognising `>`
/// quote lines, "On … wrote:" attributions and Outlook's
/// `-----Original Message-----` marker.
pub(crate) fn wholly_quoted(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let quoted = trimmed.starts_with('>') || trimmed.starts_with("-----Original Message-----") || (trimmed.starts_with("On ") && trimmed.ends_with("wrote:"));
        if !quoted {
            return false;
        }
    }
    true
}

/// Ignores the conversation of `primary` (item 2.8 / M-20 / M-16): remembers
/// its thread key so future arrivals move to Deleted, and moves the thread's
/// currently-visible messages (`targets` that share the key) there now.
pub(crate) fn ignore_conversation(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, targets: &[EmailSummary], primary: &EmailSummary) {
    let Some(account) = mailbox_account_id(&primary.mailbox) else { return };
    let thread = primary.thread_key.0.clone();
    let label = primary.subject.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "(No subject)".to_string());
    crate::rules::ignore_thread(state, &account, &thread, &label);
    let to_move: Vec<EmailSummary> = targets.iter().filter(|summary| summary.thread_key == primary.thread_key).cloned().collect();
    if !to_move.is_empty() {
        move_messages_to_role(state, message_list, &to_move, MailboxRole::Trash);
    }
    if let Some(overlay) = &state.borrow().toast_overlay {
        overlay.add_toast(adw::Toast::new("Conversation ignored"));
    }
}

/// Stops ignoring the conversation of `primary` (item 2.8 / M-20).
pub(crate) fn stop_ignoring_conversation(state: &Rc<RefCell<UiState>>, primary: &EmailSummary) {
    let Some(account) = mailbox_account_id(&primary.mailbox) else { return };
    crate::rules::unignore_thread(state, &account, &primary.thread_key.0);
    if let Some(overlay) = &state.borrow().toast_overlay {
        overlay.add_toast(adw::Toast::new("Conversation no longer ignored"));
    }
}

/// Cleans up a conversation (item 2.8 / M-20): moves its older messages whose
/// cached preview is entirely quoted (so they add nothing a later message
/// doesn't already carry) to Deleted. Conservative and undoable - only
/// messages with a known, wholly-quoted preview are touched, and the move is a
/// normal Undo-able delete.
pub(crate) fn clean_up_conversation(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, targets: &[EmailSummary], primary: &EmailSummary) {
    let thread_id = (primary.mailbox.clone(), primary.thread_key.clone());
    let mut messages = message_list.thread_messages(&thread_id);
    if messages.is_empty() {
        messages = targets.to_vec();
    }
    let newest = messages.iter().map(|message| message.date).max();
    let redundant: Vec<EmailSummary> = messages
        .into_iter()
        .filter(|message| Some(message.date) != newest)
        .filter(|message| message.preview.as_deref().is_some_and(wholly_quoted))
        .collect();
    let Some(overlay) = state.borrow().toast_overlay.clone() else { return };
    if redundant.is_empty() {
        overlay.add_toast(adw::Toast::new("Nothing to clean up in this conversation"));
        return;
    }
    let count = redundant.len();
    move_messages_to_role(state, message_list, &redundant, MailboxRole::Trash);
    overlay.add_toast(adw::Toast::new(&format!("Cleaned up {count} message(s)")));
}

/// Starts a Report phishing run (item 6.7 / S-09) for the single target: the
/// message's raw bytes are fetched, then (in the `RawMessageFetched` handler)
/// forwarded as a `.eml` attachment to the configured address and the message
/// moved to Junk. A no-op when no reporting address is set or there is no
/// target.
pub(crate) fn report_phishing_selection(state: &Rc<RefCell<UiState>>, targets: &[EmailSummary]) {
    let Some(summary) = targets.first() else { return };
    if state.borrow().settings.get_string(crate::settings::MAIL_REPORT_PHISHING_ADDRESS).trim().is_empty() {
        return;
    }
    state.borrow_mut().pending_report_phishing = Some((summary.mailbox.clone(), summary.uid));
    if !send_fetch_raw_message(state, &summary.mailbox, summary.uid) {
        state.borrow_mut().pending_report_phishing = None;
    }
}

/// Builds the forwarding message a Report phishing run sends: the original
/// message as a `message/rfc822` attachment to the configured address, from
/// the target's own account. `None` when the address is unset or the account
/// handle is gone.
pub(crate) fn report_phishing_message(state: &Rc<RefCell<UiState>>, summary: &EmailSummary, raw: &[u8]) -> Option<lookout_mail::ComposedMessage> {
    let address = state.borrow().settings.get_string(crate::settings::MAIL_REPORT_PHISHING_ADDRESS).trim().to_string();
    if address.is_empty() {
        return None;
    }
    let account_id = mailbox_account_id(&summary.mailbox)?;
    let (from, display_name) = {
        let st = state.borrow();
        let handle = st.accounts.get(&account_id)?;
        (handle.email.clone(), handle.display_name.clone())
    };
    let subject = summary.subject.clone().unwrap_or_default();
    let report_subject = if subject.trim().is_empty() {
        "Phishing report".to_string()
    } else {
        format!("Phishing report: {}", subject.trim())
    };
    Some(lookout_mail::ComposedMessage {
        from,
        display_name: (!display_name.trim().is_empty()).then_some(display_name),
        to: vec![address],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject: report_subject,
        importance: lookout_core::Importance::Normal,
        sensitivity: lookout_core::MessageSensitivity::Normal,
        text_body: "Forwarding a message reported as phishing. The original is attached.".to_string(),
        html_body: None,
        attachments: vec![lookout_mail::Attachment {
            filename: phishing_attachment_name(&subject),
            content_type: "message/rfc822".to_string(),
            bytes: raw.to_vec(),
        }],
        inline_images: vec![],
        calendar_part: None,
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to: None,
        references: vec![],
        message_id: None,
    })
}

/// A safe `.eml` filename derived from a message's subject.
pub(crate) fn phishing_attachment_name(subject: &str) -> String {
    let mut name = String::new();
    for ch in subject.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            name.push(ch);
        } else if ch == ' ' || ch == '.' {
            name.push('_');
        }
        if name.len() >= 60 {
            break;
        }
    }
    let name = name.trim_matches('_');
    if name.is_empty() {
        "message.eml".to_string()
    } else {
        format!("{name}.eml")
    }
}

/// Moves `summaries` into the folder `target` (the message context menu's
/// Move ▸, item 1.17): rows go at once, one `MoveMessagesTo` per source
/// mailbox (the ones already there are skipped), one undo entry, and
/// `target` becomes the account's most recent folder.
pub(crate) fn move_messages_to_folder(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], target: &MailboxId) {
    let mut inverse = Vec::new();
    let mut count = 0usize;
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        if mailbox == *target || mailbox_account_id(&mailbox) != mailbox_account_id(target) {
            continue;
        }
        optimistic_remove_messages(state, message_list, &mailbox, &uids);
        inverse.push(AccountCommand::RevertMove {
            moved_from: mailbox.clone(),
            moved_to: target.clone(),
            uids: uids.clone(),
        });
        count += uids.len();
        let _ = cmd_tx.send_blocking(AccountCommand::MoveMessagesTo {
            mailbox,
            uids,
            target: target.clone(),
        });
    }
    if count > 0 {
        record_recent_folder(state, target);
    }
    record_undo(state, &undo_move_description(MailboxRole::Custom, count), inverse);
}

/// Copies `summaries` into the folder `target` (Copy ▸, item 1.17): one
/// `CopyMessagesTo` per source mailbox, and Undo removes the copies again
/// (`RevertCopy`).
pub(crate) fn copy_messages_to_folder(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary], target: &MailboxId) {
    let mut inverse = Vec::new();
    let mut count = 0usize;
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        if mailbox == *target || mailbox_account_id(&mailbox) != mailbox_account_id(target) {
            continue;
        }
        inverse.push(AccountCommand::RevertCopy {
            copied_from: mailbox.clone(),
            copied_to: target.clone(),
            uids: uids.clone(),
        });
        count += uids.len();
        let _ = cmd_tx.send_blocking(AccountCommand::CopyMessagesTo {
            mailbox,
            uids,
            target: target.clone(),
        });
    }
    if count > 0 {
        record_recent_folder(state, target);
    }
    let noun = if count == 1 { "item" } else { "items" };
    record_undo(state, &format!("{count} {noun} copied"), inverse);
}

/// Makes `folder` its account's most recently used move/copy target and
/// persists the list (Move ▸ / Copy ▸, item 1.17).
pub(crate) fn record_recent_folder(state: &Rc<RefCell<UiState>>, folder: &MailboxId) {
    let Some(account_id) = mailbox_account_id(folder) else { return };
    let mut st = state.borrow_mut();
    let recent = st.recent_folders.entry(account_id.clone()).or_default();
    crate::message_menu::push_recent_folder(recent, folder.clone());
    let recent = recent.clone();
    if let Some(db) = &st.ui_db {
        if let Err(e) = db.set_recent_folders(&account_id, &recent) {
            tracing::warn!("recent folders won't persist: {e}");
        }
    }
}

/// Pins (`pinning`) or unpins `summaries`: shows it at once, one
/// `StoreFlagsMany` per mailbox, and one undo entry - the opposite
/// `\Flagged` change on every affected message (item 0.4).
pub(crate) fn set_messages_pinned(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], pinning: bool) {
    let (add, remove) = if pinning {
        (vec![SystemFlagBit::Flagged], Vec::new())
    } else {
        (Vec::new(), vec![SystemFlagBit::Flagged])
    };
    let mut inverse = Vec::new();
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        optimistic_toggle_pinned(state, message_list, &mailbox, &uids, pinning);
        inverse.push(AccountCommand::StoreFlagsMany {
            mailbox: mailbox.clone(),
            uids: uids.clone(),
            add: remove.clone(),
            remove: add.clone(),
        });
        let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
            mailbox,
            uids,
            add: add.clone(),
            remove: remove.clone(),
        });
    }
    record_undo(state, if pinning { "Pinned" } else { "Unpinned" }, inverse);
}

/// Marks `summaries` read (`mark_read`) or unread: shows it at once, one
/// `StoreFlagsMany` per mailbox, and one undo entry - the opposite `\Seen`
/// change on every affected message (item 0.4).
pub(crate) fn set_messages_read(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], mark_read: bool) {
    let (add, remove) = if mark_read {
        (vec![SystemFlagBit::Seen], Vec::new())
    } else {
        (Vec::new(), vec![SystemFlagBit::Seen])
    };
    let mut inverse = Vec::new();
    for (cmd_tx, mailbox, uids) in command_targets_for(state, summaries) {
        optimistic_toggle_read(state, message_list, &mailbox, &uids, mark_read);
        inverse.push(AccountCommand::StoreFlagsMany {
            mailbox: mailbox.clone(),
            uids: uids.clone(),
            add: remove.clone(),
            remove: add.clone(),
        });
        let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
            mailbox,
            uids,
            add: add.clone(),
            remove: remove.clone(),
        });
    }
    record_undo(state, if mark_read { "Marked as read" } else { "Marked as unread" }, inverse);
}

/// Runs `run` with `summary`'s body: at once when it's cached, else once
/// `FetchBody` brings it (`UiState::body_waiter`, run by the `BodyFetched`
/// handler). A newer request replaces a waiting one, and a body that never
/// arrives simply drops it. A silent no-op when the account is offline.
pub(crate) fn with_message_body(state: &Rc<RefCell<UiState>>, summary: &EmailSummary, run: impl FnOnce(EmailBody) + 'static) {
    let cached = state.borrow_mut().cached_body(&summary.mailbox, summary.uid);
    if let Some(body) = cached {
        run(body);
        return;
    }
    let key = (summary.mailbox.clone(), summary.uid);
    let (cmd_tx, in_flight) = {
        let st = state.borrow();
        let cmd_tx = mailbox_account_id(&summary.mailbox)
            .and_then(|account_id| st.accounts.get(&account_id))
            .map(|handle| handle.interactive_cmd_tx.clone());
        (cmd_tx, st.pending_body_request.as_ref() == Some(&key))
    };
    let Some(cmd_tx) = cmd_tx else { return };
    state.borrow_mut().body_waiter = Some(BodyWaiter {
        mailbox: key.0.clone(),
        uid: key.1,
        run: Box::new(run),
    });
    if !in_flight {
        let _ = cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox: key.0, uid: key.1 });
    }
}

/// Opens a reply (`Some(mode)`) to, or a forward (`None`) of, `summary` in
/// the reading pane - the message context menu's Reply / Reply all /
/// Forward, which (unlike the ribbon's) may act on a message whose body
/// hasn't loaded yet, so the composer opens once it has.
pub(crate) fn reply_to_message(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, summary: EmailSummary, mode: Option<crate::compose::ReplyMode>) {
    let Some((from_email, cmd_tx)) = reply_send_context(state, &summary) else { return };
    let state_for_body = state.clone();
    let worker = worker.clone();
    let reading_stack = reading_stack.clone();
    with_message_body(state, &summary.clone(), move |body| match mode {
        Some(mode) => {
            let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, mode);
            let title = match mode {
                crate::compose::ReplyMode::Reply => "Reply",
                crate::compose::ReplyMode::ReplyAll => "Reply All",
            };
            let rich_text_default = state_for_body.borrow().rich_text_default;
            show_composer_in_reading_pane(
                &state_for_body,
                &worker,
                &reading_stack,
                title,
                cmd_tx,
                prefill,
                rich_text_default,
                mailbox_account_id(&summary.mailbox),
                Some(crate::config_view::SignatureDefault::Reply),
            );
        }
        None => forward_message(&state_for_body, &worker, &reading_stack, &summary, &body, false),
    });
}

/// Opens a forward composer for `summary` (item 1.11 / C-13), Outlook's
/// default Forward: the original is quoted inline and its attachments (and
/// inline `cid:` images) are fetched and re-attached. The fetch runs first
/// (`ForwardBatch`); if the message has nothing to attach, the composer opens
/// straight away.
pub(crate) fn forward_message(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, summary: &EmailSummary, body: &EmailBody, pop_out_on_open: bool) {
    let account_id = mailbox_account_id(&summary.mailbox);
    let cmd_tx = account_id.as_ref().and_then(|id| state.borrow().accounts.get(id).map(|h| h.cmd_tx.clone()));
    let Some(cmd_tx) = cmd_tx else { return };

    let prefill = crate::compose::build_forward_prefill(summary, body);
    // The parts to re-attach: file attachments (the strip's filter) and the
    // inline `cid:` images the forwarded HTML references.
    let parts: Vec<BodyPart> = body.parts.iter().filter(|p| p.is_attachment || p.cid.is_some()).cloned().collect();
    if parts.is_empty() {
        let rich_text_default = state.borrow().rich_text_default;
        show_composer_in_reading_pane_full(
            state,
            worker,
            reading_stack,
            "Forward",
            cmd_tx,
            prefill,
            rich_text_default,
            account_id,
            Some(crate::config_view::SignatureDefault::Reply),
            None,
            pop_out_on_open,
        );
        return;
    }
    // One fetch at a time, shared with "Save all" / "Download as zip".
    {
        let st = state.borrow();
        if st.pending_attachment_batch.is_some() || st.pending_attachment.is_some() {
            if let Some(toast_overlay) = &st.toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Another attachment download is still running"));
            }
            return;
        }
    }
    start_attachment_batch(
        state,
        &summary.mailbox,
        summary.uid,
        parts,
        AttachmentBatchAction::Forward(Box::new(ForwardBatch {
            prefill,
            cmd_tx,
            title: "Forward".to_string(),
            account_id,
            signature_kind: Some(crate::config_view::SignatureDefault::Reply),
            pop_out_on_open,
        })),
    );
}

/// Opens a "Forward as attachment" composer for `summary` (item 1.11 / C-13):
/// nothing is quoted; the session is asked for the whole raw message, which
/// is attached as a `message/rfc822` part once it arrives.
pub(crate) fn forward_as_attachment(state: &Rc<RefCell<UiState>>, summary: &EmailSummary) {
    let account_id = mailbox_account_id(&summary.mailbox);
    let cmd_tx = account_id.as_ref().and_then(|id| state.borrow().accounts.get(id).map(|h| h.cmd_tx.clone()));
    let Some(cmd_tx) = cmd_tx else { return };
    start_forward_as_attachment(state, summary.clone(), cmd_tx, account_id);
}

/// Starts a "Forward as attachment" fetch (item 1.11 / C-13): records the
/// request, asks the session for the whole raw message, and arms the same
/// 60-second backstop as the other raw fetches. The composer opens once
/// `AccountEvent::RawMessageFetched` lands.
pub(crate) fn start_forward_as_attachment(state: &Rc<RefCell<UiState>>, summary: EmailSummary, cmd_tx: async_channel::Sender<AccountCommand>, account_id: Option<AccountId>) {
    let Some(account) = mailbox_account_id(&summary.mailbox) else { return };
    let fetch_tx = state.borrow().accounts.get(&account).map(|h| h.interactive_cmd_tx.clone());
    let Some(fetch_tx) = fetch_tx else { return };
    // One forward-as-attachment fetch at a time.
    if state.borrow().pending_forward_raw.is_some() {
        return;
    }
    state.borrow_mut().pending_forward_raw = Some(PendingForwardRaw {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        summary: summary.clone(),
        cmd_tx,
        account_id,
    });
    let _ = fetch_tx.send_blocking(AccountCommand::FetchRawMessage {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
    });
    // Backstop: clear the pending state after a generous grace period rather
    // than blocking future forwards forever (a session dying mid-fetch loses
    // the command to the reconnect). Only fires if this exact request is still
    // outstanding.
    let timeout_state = state.clone();
    let timeout_mailbox = summary.mailbox.clone();
    let timeout_uid = summary.uid;
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let active = timeout_state
            .borrow()
            .pending_forward_raw
            .as_ref()
            .is_some_and(|p| p.mailbox == timeout_mailbox && p.uid == timeout_uid);
        if active {
            timeout_state.borrow_mut().pending_forward_raw = None;
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Forward timed out"));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Prints `summary` whether or not the reading pane shows it (the message
/// context menu's Print): its body - fetched first if need be - goes through
/// an offscreen WebView under the same remote-content policy the reading
/// pane applies to that sender.
pub(crate) fn print_message(state: &Rc<RefCell<UiState>>, parent: &gtk::Window, summary: &EmailSummary) {
    let state_for_body = state.clone();
    let parent = parent.clone();
    let trust_sender = mailbox_account_id(&summary.mailbox).zip(summary.from.first().map(|a| a.address.trim().to_lowercase()));
    with_message_body(state, summary, move |body| {
        let html = match &body.html_body {
            Some(html) => {
                let (images_allowed, all_allowed) = remote_content_policy(&state_for_body, trust_sender.as_ref(), false);
                wrap_message_with_csp(html, images_allowed, all_allowed)
            }
            None => plain_text_print_html(body.text_body.as_deref().unwrap_or_default()),
        };
        print_html_once(&html, &parent);
    });
}

/// Records a reversible action and shows its "… / Undo" toast. A no-op when
/// `inverse` is empty (nothing to reverse).
pub(crate) fn record_undo(state: &Rc<RefCell<UiState>>, description: &str, inverse: Vec<AccountCommand>) {
    if inverse.is_empty() {
        return;
    }
    let id = {
        let mut st = state.borrow_mut();
        let id = st.next_undo_id;
        st.next_undo_id += 1;
        st.undo_stack.push(UndoEntry {
            id,
            description: description.to_string(),
            inverse,
        });
        while st.undo_stack.len() > MAX_UNDO_ENTRIES {
            st.undo_stack.remove(0);
        }
        id
    };
    show_undo_toast(state, id, description);
}

/// Pops and applies the most recent undo entry (Ctrl+Z). Returns whether
/// anything was undone.
pub(crate) fn undo_last(state: &Rc<RefCell<UiState>>) -> bool {
    let entry = state.borrow_mut().undo_stack.pop();
    match entry {
        Some(entry) => {
            apply_undo(state, entry);
            true
        }
        None => false,
    }
}

/// Removes and returns the entry with `id`, if it's still on the stack. Shared
/// by a toast's Undo button and its `dismissed` signal.
pub(crate) fn take_undo(state: &Rc<RefCell<UiState>>, id: u64) -> Option<UndoEntry> {
    let mut st = state.borrow_mut();
    let position = st.undo_stack.iter().position(|entry| entry.id == id)?;
    Some(st.undo_stack.remove(position))
}

/// Groups `summaries` by the `(account, mailbox)` they live in, for the
/// batch Delete/Archive/Report/Snooze/Pin/Mark-read actions - the ribbon's
/// (on the selection) and the message context menu's (item 1.17, on its
/// targets, which may be a conversation header's messages). The account is
/// derived from each message's own `MailboxId` rather than the view's
/// `current_account`, so the unified "All Inboxes" list routes each message
/// to the right account regardless of which mailboxes are mixed in.
///
/// Returns one entry per distinct mailbox touched - so a batch action sends
/// exactly one plural `AccountCommand` per mailbox, not one per message -
/// with each group's uids in `summaries` order. A message whose account has
/// since disconnected is silently dropped from its group rather than failing
/// the whole batch. No messages (or every account disconnected) yields an
/// empty `Vec`, which every caller treats as a no-op loop.
pub(crate) fn command_targets_for(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary]) -> Vec<(async_channel::Sender<AccountCommand>, MailboxId, Vec<Uid>)> {
    let st = state.borrow();
    let mut groups: HashMap<(AccountId, MailboxId), (async_channel::Sender<AccountCommand>, Vec<Uid>)> = HashMap::new();
    for summary in summaries {
        let Some(account_id) = mailbox_account_id(&summary.mailbox) else { continue };
        let Some(handle) = st.accounts.get(&account_id) else { continue };
        groups
            .entry((account_id, summary.mailbox.clone()))
            .or_insert_with(|| (handle.cmd_tx.clone(), Vec::new()))
            .1
            .push(summary.uid);
    }
    groups.into_iter().map(|((_, mailbox), (cmd_tx, uids))| (cmd_tx, mailbox, uids)).collect()
}

/// Resolves the currently-selected message plus its already-fetched body,
/// for the Reply/Reply-All/Forward button handlers. Returns `None` if
/// nothing is selected, a section header is selected, its account has
/// disconnected, or the selected message's body isn't in the in-memory cache
/// (its body hasn't arrived yet, or it's outside the cache's bounded recency
/// window) - the calling handler is then a silent no-op, same convention as
/// `command_targets_for`. The
/// "From" identity is the message's owning account, so replies composed from
/// the unified view go out from the right address.
pub(crate) fn selected_message_reply_context(
    message_list: &MessageListModel,
    state: &Rc<RefCell<UiState>>,
) -> Option<(EmailSummary, EmailBody, String, async_channel::Sender<AccountCommand>)> {
    let summary = message_list.selected_summary()?;

    let account_id = mailbox_account_id(&summary.mailbox)?;
    let mut st = state.borrow_mut();
    let body = st.body_cache.get(&summary.mailbox, &summary.uid)?;
    let handle = st.accounts.get(&account_id)?;
    Some((summary, body, handle.email.clone(), handle.cmd_tx.clone()))
}

/// The account context a reply/forward from a pop-out message window (item
/// 1.9) needs: the sending identity's address and the account's command
/// channel. The body is resolved separately by the caller (the window's own
/// parsed body, falling back to the body cache) so this stays independent of
/// the LRU. `None` when the message's account isn't connected - the caller
/// is then a silent no-op, same convention as `selected_message_reply_context`.
pub(crate) fn reply_send_context(state: &Rc<RefCell<UiState>>, summary: &EmailSummary) -> Option<(String, async_channel::Sender<AccountCommand>)> {
    let account_id = mailbox_account_id(&summary.mailbox)?;
    let st = state.borrow();
    let handle = st.accounts.get(&account_id)?;
    Some((handle.email.clone(), handle.cmd_tx.clone()))
}

/// Opens a new-message composer in the reading pane for the best-guess
/// account: the selected message's account, else the open mailbox's, else
/// any connected account - exactly the toolbar compose button's logic,
/// shared with the tray icon's "Compose…" menu item.
pub(crate) fn compose_new_message(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, message_list: &MessageListModel) {
    let st = state.borrow();
    // A section header is selected exactly like "nothing selected" here:
    // fall through to the open mailbox's account.
    let account_id = message_list
        .selected_summary()
        .and_then(|summary| mailbox_account_id(&summary.mailbox))
        .or_else(|| st.current_account.clone())
        .or_else(|| st.accounts.keys().next().cloned());
    let Some(handle) = account_id.clone().and_then(|id| st.accounts.get(&id)) else { return };
    let cmd_tx = handle.cmd_tx.clone();
    let rich_text_default = state.borrow().rich_text_default;
    drop(st);
    show_composer_in_reading_pane(
        state,
        worker,
        reading_stack,
        "New Message",
        cmd_tx,
        crate::compose::ComposePrefill::default(),
        rich_text_default,
        account_id,
        Some(crate::config_view::SignatureDefault::New),
    );
}

/// How a Sweep selects which of the sender's messages to act on (item 2.7 /
/// M-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepMode {
    /// Every message from the sender.
    All,
    /// Every message except the most recent.
    KeepLatest,
    /// Messages older than this many days.
    Older(i64),
}

/// The messages a [`SweepMode`] acts on, from `messages` (item 2.7 / M-16).
/// Pure, so the selection rule is unit-tested.
pub(crate) fn sweep_selection(mut messages: Vec<EmailSummary>, mode: SweepMode) -> Vec<EmailSummary> {
    match mode {
        SweepMode::All => messages,
        SweepMode::KeepLatest => {
            if let Some(newest) = messages.iter().enumerate().max_by_key(|(_, message)| message.date).map(|(index, _)| index) {
                messages.remove(newest);
            }
            messages
        }
        SweepMode::Older(days) => {
            let cutoff = chrono::Utc::now() - chrono::Duration::days(days.max(0));
            messages.retain(|message| message.date < cutoff);
            messages
        }
    }
}

/// The maximum messages one sweep gathers from the local cache (a guard against
/// an unbounded sender).
const SWEEP_LIMIT: usize = 5000;

/// Opens the Sweep dialog for `primary` (item 2.7 / M-16): bulk delete or move
/// of every message from the same sender in the open folder, optionally keeping
/// the latest, optionally older than N days, and optionally a standing rule for
/// future mail.
pub(crate) fn show_sweep_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, worker: Rc<Worker>, message_list: MessageListModel, primary: EmailSummary) {
    let Some(sender) = primary.from.first().cloned() else {
        if let Some(overlay) = &state.borrow().toast_overlay {
            overlay.add_toast(adw::Toast::new("This message has no sender to sweep"));
        }
        return;
    };
    let sender_label = sender.display_label().to_string();
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Sweep").default_width(460);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let all = gtk::CheckButton::with_label(&format!("All messages from {sender_label}"));
    // The sender's name/address is content: blurred by privacy mode.
    crate::privacy::mark(&all);
    all.set_active(true);
    let keep_latest = gtk::CheckButton::with_label("All except the latest");
    keep_latest.set_group(Some(&all));
    let older = gtk::CheckButton::with_label("Older than");
    older.set_group(Some(&all));
    let older_spin = gtk::SpinButton::with_range(1.0, 3650.0, 1.0);
    older_spin.set_value(10.0);
    let older_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    older_row.append(&older);
    older_row.append(&older_spin);
    let days_label = gtk::Label::new(Some("days"));
    crate::privacy::mark_ui(&days_label);
    older_row.append(&days_label);

    let target_dropdown = gtk::DropDown::builder().build();
    target_dropdown.set_model(Some(&gtk::StringList::new(&["Deleted Items", "Archive"])));
    target_dropdown.set_selected(0);
    let target_row = adw::ActionRow::builder().title("Move to").subtitle("Where the swept messages go").build();
    target_row.add_suffix(&target_dropdown);

    let standing = adw::SwitchRow::builder()
        .title("Also apply to future messages")
        .subtitle("Creates a rule for this sender")
        .build();

    let options = adw::PreferencesGroup::builder().title("Sweep").build();
    options.add(&all);
    options.add(&keep_latest);
    options.add(&older_row);
    options.add(&target_row);
    options.add(&standing);

    let cancel = gtk::Button::with_label("Cancel");
    let sweep = gtk::Button::with_label("Sweep");
    sweep.add_css_class("destructive-action");
    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&sweep);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&options);
    root.append(&buttons);
    dialog.set_child(Some(&root));
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }

    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let dialog = dialog.clone();
        let account = mailbox_account_id(&primary.mailbox);
        let mailbox = primary.mailbox.clone();
        let address = sender.address.clone();
        let older = older.clone();
        let older_spin = older_spin.clone();
        let keep_latest = keep_latest.clone();
        let target_dropdown = target_dropdown.clone();
        let standing = standing.clone();
        sweep.connect_clicked(move |_| {
            let mode = if older.is_active() {
                SweepMode::Older(older_spin.value() as i64)
            } else if keep_latest.is_active() {
                SweepMode::KeepLatest
            } else {
                SweepMode::All
            };
            let role = if target_dropdown.selected() == 1 { MailboxRole::Archive } else { MailboxRole::Trash };
            let standing = standing.is_active();
            dialog.close();
            run_sweep(&state, &worker, &message_list, account.clone(), mailbox.clone(), address.clone(), mode, role, standing);
        });
    }
    dialog.present();
}

/// Runs one sweep (item 2.7 / M-16): gathers the sender's messages from the
/// account's local cache scoped to the open folder, applies the mode, moves the
/// survivors to `role`, and (optionally) records a standing rule.
#[allow(clippy::too_many_arguments)]
fn run_sweep(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    message_list: &MessageListModel,
    account: Option<AccountId>,
    mailbox: MailboxId,
    address: String,
    mode: SweepMode,
    role: MailboxRole,
    standing: bool,
) {
    if standing {
        add_sweep_rule(state, &address, role);
    }
    let Some(account) = account else { return };
    let cache = state.borrow().accounts.get(&account).and_then(|handle| handle.address_cache.clone());
    let Some(cache) = cache else {
        if let Some(overlay) = &state.borrow().toast_overlay {
            overlay.add_toast(adw::Toast::new("Sweep needs the local message cache - try again in a moment"));
        }
        return;
    };
    let query = lookout_mail::parse_search_query(&format!("from:{address}"));
    let scope = [mailbox];
    let reply = crate::window::spawn_cache_read(worker, cache, move |cache| cache.search_query(&query, Some(&scope), SWEEP_LIMIT).unwrap_or_default());
    let state = state.clone();
    let message_list = message_list.clone();
    glib::spawn_future_local(async move {
        let Ok(messages) = reply.recv().await else { return };
        let selected = sweep_selection(messages, mode);
        let Some(overlay) = state.borrow().toast_overlay.clone() else { return };
        if selected.is_empty() {
            overlay.add_toast(adw::Toast::new("Nothing to sweep"));
            return;
        }
        let count = selected.len();
        move_messages_to_role(&state, &message_list, &selected, role);
        overlay.add_toast(adw::Toast::new(&format!("Swept {count} message(s)")));
    });
}

/// Adds a standing client rule for a sweep (item 2.7 / M-16): mail from
/// `address` moves to `role` on arrival.
fn add_sweep_rule(state: &Rc<RefCell<UiState>>, address: &str, role: MailboxRole) {
    let mut rule = lookout_core::Rule::new("", format!("Sweep {address}"));
    rule.backend = lookout_core::RuleBackend::Client;
    rule.conditions = vec![lookout_core::RuleCondition::From(address.to_string())];
    rule.actions = vec![lookout_core::RuleAction::Move(lookout_core::RuleTarget::Role(role))];
    let mut rules = state.borrow().app_config.borrow().rules.clone();
    rules.push(rule);
    crate::rules_view::save_rules(state, rules);
}

#[cfg(test)]
mod tests {
    use super::{sweep_selection, wholly_quoted, SweepMode};
    use chrono::Utc;
    use lookout_core::EmailSummary;

    fn msg(uid: u32, days_ago: i64) -> EmailSummary {
        EmailSummary {
            inference_classification: Default::default(),
            uid: lookout_core::Uid(uid),
            mailbox: lookout_core::MailboxId("acct:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date: Utc::now() - chrono::Duration::days(days_ago),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: Default::default(),
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    #[test]
    fn sweep_modes_select_the_right_messages() {
        let messages = vec![msg(1, 30), msg(2, 10), msg(3, 0)];
        assert_eq!(sweep_selection(messages.clone(), SweepMode::All).len(), 3);
        let kept = sweep_selection(messages.clone(), SweepMode::KeepLatest);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|m| m.uid.0 != 3), "the newest is kept");
        let older = sweep_selection(messages, SweepMode::Older(15));
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].uid.0, 1);
    }

    #[test]
    fn wholly_quoted_recognises_fully_quoted_and_rejects_real_content() {
        assert!(wholly_quoted(""));
        assert!(wholly_quoted("> quoted line\n> another\n"));
        assert!(wholly_quoted("On Tue, Ada wrote:\n> hello\n"));
        assert!(wholly_quoted("-----Original Message-----\n> old\n"));
        // Any unquoted line means the message carries its own content.
        assert!(!wholly_quoted("Here is my reply\n> quoted\n"));
        assert!(!wholly_quoted("plain text with no quote marker"));
        assert!(!wholly_quoted("On the whole, agreed"));
    }
}
