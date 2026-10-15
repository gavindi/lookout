//! The Microsoft Graph To Do session actor (OUTLOOK_PARITY.md §5.5/§5.6, item
//! 5.3 / A-05, T-12).
//!
//! Microsoft To Do is a separate backend from the Graph calendar (item 5.1):
//! Graph calendars never carry VTODOs, so this actor owns `/me/todo/lists` and
//! `/me/todo/lists/{id}/tasks` and speaks a small command/event pair the app's
//! Tasks view drives. It owns the connect/poll/backoff lifecycle and the write
//! path; the HTTP lives in [`TodoClient`].
//!
//! A missing or revoked Graph consent can't fix itself by retrying, so the
//! actor surfaces it once and stops - Config's "Sync with Outlook" reconnects
//! the account, the same contract as the calendar and contacts backends.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::time::Duration;

use lookout_core::{AccountId, CalendarTask};

use crate::error::GraphError;
use crate::todo::client::TodoClient;
use crate::todo::{TodoList, WELL_KNOWN_FLAGGED_EMAIL};

/// How often to re-poll while idle, mirroring the calendar actor.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A write (or an explicit refresh) the app asks the session to perform. The
/// task's own `calendar_id` carries the list; `list_id` is the resolved Graph
/// list id (the synthetic id's suffix).
pub enum TodoCommand {
    /// Full resync outside the poll cadence (an F5, or the Tasks tab opening).
    Refresh,
    CreateTask {
        list_id: String,
        task: Box<CalendarTask>,
    },
    UpdateTask {
        list_id: String,
        task: Box<CalendarTask>,
    },
    DeleteTask {
        list_id: String,
        task_id: String,
        etag: Option<String>,
    },
}

/// Whole-snapshot events the session pushes to the app.
pub enum TodoEvent {
    /// The account's To Do lists, from `/me/todo/lists`.
    ListsUpdated(Vec<TodoList>),
    /// Every list's full task snapshot.
    TasksUpdated(Vec<CalendarTask>),
    /// A problem worth surfacing: a connect failure, revoked consent, a failed
    /// write, ...
    Error(String),
}

/// Runs one Microsoft 365 account's To Do lifecycle on the calling task (spawn
/// onto the shared worker, as `run_calendar_session` is). Reconnects with
/// backoff on any transient error; returns when the account's consent is gone
/// or the command channel closes.
pub async fn run_todo_session(account_id: AccountId, client: TodoClient, commands: async_channel::Receiver<TodoCommand>, events: async_channel::Sender<TodoEvent>) {
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        match connect_and_run(&account_id, &client, &commands, &events).await {
            Ok(ShutdownReason::Requested) => return,
            Err(e) => {
                if fatal(&e) {
                    // Non-retryable: surface once and stop. Config's
                    // "Sync with Outlook" reconnects the account, and a fresh
                    // grant is what covers a scope added after the last
                    // consent (Tasks.ReadWrite here).
                    let message = if matches!(e, GraphError::Status { status: 403, .. }) {
                        format!("{e} - re-run \"Sync with Outlook\" in Config to grant Microsoft To Do access")
                    } else {
                        e.to_string()
                    };
                    let _ = events.send(TodoEvent::Error(message)).await;
                    return;
                }
                tracing::warn!("graph to-do session error, will reconnect: {e}");
                let _ = events.send(TodoEvent::Error(e.to_string())).await;
            }
        }

        // Wait out the backoff, cut short by a command or a channel close
        // (which means the app dropped this account's handle).
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cmd = commands.recv() => match cmd {
                Ok(_) => {
                    let _ = events.send(TodoEvent::Error("not connected - the task change was not saved".to_string())).await;
                }
                Err(_) => return,
            }
        }
        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
    }
}

enum ShutdownReason {
    Requested,
}

/// Whether an error can never fix itself by retrying: a missing/revoked Graph
/// consent, or a `403` (the account consented before `Tasks.ReadWrite` was
/// added, so the grant lacks the scope until it is re-run).
fn fatal(error: &GraphError) -> bool {
    matches!(error, GraphError::ConsentRequired(_) | GraphError::Status { status: 403, .. })
}

async fn connect_and_run(
    account_id: &AccountId,
    client: &TodoClient,
    commands: &async_channel::Receiver<TodoCommand>,
    events: &async_channel::Sender<TodoEvent>,
) -> Result<ShutdownReason, GraphError> {
    sync(client, events).await?;

    loop {
        enum Wake {
            Poll,
            Command(TodoCommand),
            ChannelClosed,
        }
        let wake = tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => Wake::Poll,
            c = commands.recv() => match c {
                Ok(cmd) => Wake::Command(cmd),
                Err(_) => Wake::ChannelClosed,
            },
        };

        match wake {
            Wake::Poll => sync(client, events).await?,
            Wake::Command(cmd) => {
                handle_command(account_id, client, cmd, events).await?;
                // Drain anything else already queued before sleeping again.
                for cmd in std::iter::from_fn(|| commands.try_recv().ok()) {
                    handle_command(account_id, client, cmd, events).await?;
                }
            }
            Wake::ChannelClosed => return Ok(ShutdownReason::Requested),
        }
    }
}

/// Applies one write command (emitting a non-fatal `Error` on failure), then
/// resyncs so the app's snapshot reflects the server. A resync failure
/// propagates (the outer loop reconnects).
async fn handle_command(account_id: &AccountId, client: &TodoClient, command: TodoCommand, events: &async_channel::Sender<TodoEvent>) -> Result<(), GraphError> {
    match command {
        TodoCommand::Refresh => {}
        TodoCommand::CreateTask { list_id, task } => {
            if let Err(e) = client.create_task(&list_id, &task).await {
                tracing::warn!("graph to-do create failed for {account_id}: {e}");
                let _ = events.send(TodoEvent::Error(format!("couldn't save the task \"{}\": {e}", task.uid.0))).await;
                return Ok(());
            }
        }
        TodoCommand::UpdateTask { list_id, task } => {
            if let Err(e) = client.update_task(&list_id, &task).await {
                tracing::warn!("graph to-do update failed for {account_id}: {e}");
                let _ = events.send(TodoEvent::Error(format!("couldn't save the task \"{}\": {e}", task.uid.0))).await;
                return Ok(());
            }
        }
        TodoCommand::DeleteTask { list_id, task_id, etag } => {
            if let Err(e) = client.delete_task(&list_id, &task_id, etag.as_deref()).await {
                tracing::warn!("graph to-do delete failed for {account_id}: {e}");
                let _ = events.send(TodoEvent::Error(format!("couldn't delete the task: {e}"))).await;
                return Ok(());
            }
        }
    }
    sync(client, events).await
}

/// One full resync: the lists, then every list's tasks, emitted as events. A
/// single list's failure is logged and skipped; consent/unauthorized errors
/// propagate so the outer loop reconnects.
async fn sync(client: &TodoClient, events: &async_channel::Sender<TodoEvent>) -> Result<(), GraphError> {
    let lists = client.list_lists().await?;
    let _ = events.send(TodoEvent::ListsUpdated(lists.clone())).await;

    let mut tasks = Vec::new();
    for list in &lists {
        // The flagged-email list is Outlook's own view of flagged messages;
        // M-10 already syncs those flags directly (§4.2), so reading it here
        // would duplicate the Tasks view's "Flagged email" section.
        if list.well_known.as_deref() == Some("flaggedEmails") || list.id == WELL_KNOWN_FLAGGED_EMAIL {
            continue;
        }
        match client.list_tasks(&list.id).await {
            Ok(list_tasks) => tasks.extend(list_tasks),
            Err(e @ GraphError::Unauthorized) => return Err(e),
            Err(e @ GraphError::ConsentRequired(_)) => return Err(e),
            Err(e) => tracing::warn!("failed to fetch To Do list {:?}: {e}", list.display_name),
        }
    }
    let _ = events.send(TodoEvent::TasksUpdated(tasks)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::client::GraphClient;
    use crate::error::Result;

    struct StubTokens {
        consent: bool,
    }

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String> {
            if self.consent {
                Ok("stub".to_string())
            } else {
                Err(crate::error::GraphError::ConsentRequired("Tasks.ReadWrite".to_string()))
            }
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer, consent: bool) -> TodoClient {
        TodoClient::new(GraphClient::with_base_url(Arc::new(StubTokens { consent }), format!("{}/v1.0", server.uri())))
    }

    #[test]
    fn fatal_covers_consent_and_forbidden_but_not_transient_errors() {
        assert!(fatal(&crate::error::GraphError::ConsentRequired("Tasks.ReadWrite".to_string())));
        assert!(fatal(&crate::error::GraphError::Status {
            status: 403,
            message: "forbidden".to_string(),
            request_id: None,
        }));
        assert!(!fatal(&crate::error::GraphError::Unauthorized));
        assert!(!fatal(&crate::error::GraphError::Status {
            status: 500,
            message: "oops".to_string(),
            request_id: None,
        }));
    }

    async fn mount_lists(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "l1", "displayName": "Tasks", "wellknownListName": "defaultList" }]
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn connect_emits_lists_and_tasks_then_returns_on_channel_close() {
        let server = MockServer::start().await;
        mount_lists(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists/l1/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "t1", "title": "Buy milk" }]
            })))
            .mount(&server)
            .await;

        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_todo_session(AccountId("a1".into()), client(&server, true), cmd_rx, evt_tx));

        // Close the command channel so the actor returns instead of polling.
        drop(cmd_tx);

        let mut lists = None;
        let mut tasks = None;
        while let Ok(event) = evt_rx.recv().await {
            match event {
                TodoEvent::ListsUpdated(value) => lists = Some(value),
                TodoEvent::TasksUpdated(value) => tasks = Some(value),
                TodoEvent::Error(message) => panic!("unexpected error: {message}"),
            }
        }
        tokio::time::timeout(Duration::from_secs(5), handle).await.expect("the actor returns").unwrap();

        assert_eq!(lists.unwrap().len(), 1);
        assert_eq!(tasks.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_consent_failure_surfaces_once_and_stops() {
        let server = MockServer::start().await;
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_todo_session(AccountId("a1".into()), client(&server, false), cmd_rx, evt_tx));
        drop(cmd_tx);

        let event = evt_rx.recv().await.expect("an error event");
        assert!(matches!(event, TodoEvent::Error(_)));
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the actor stops on consent loss")
            .unwrap();
    }

    #[tokio::test]
    async fn a_write_command_posts_then_resyncs() {
        let server = MockServer::start().await;
        mount_lists(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists/l1/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/todo/lists/l1/tasks"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "t-new", "title": "New" })))
            .mount(&server)
            .await;

        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_todo_session(AccountId("a1".into()), client(&server, true), cmd_rx, evt_tx));

        let task = CalendarTask {
            uid: lookout_core::TaskUid("t-new".to_string()),
            calendar_id: crate::todo::graph_todo_calendar_id("l1"),
            summary: Some("New".to_string()),
            description: None,
            due: None,
            start: None,
            created: None,
            completed: None,
            status: lookout_core::TaskStatus::NeedsAction,
            priority: lookout_core::TaskPriority(0),
            percent_complete: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        };
        cmd_tx
            .send(TodoCommand::CreateTask {
                list_id: "l1".to_string(),
                task: Box::new(task),
            })
            .await
            .unwrap();
        drop(cmd_tx);

        let mut saw_task_snapshot = false;
        while let Ok(event) = evt_rx.recv().await {
            if matches!(event, TodoEvent::TasksUpdated(_)) {
                saw_task_snapshot = true;
            }
        }
        tokio::time::timeout(Duration::from_secs(5), handle).await.expect("the actor returns").unwrap();
        assert!(saw_task_snapshot);
    }
}
