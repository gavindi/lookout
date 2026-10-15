//! The Microsoft Graph To Do transport: the HTTP operations the Tasks view's
//! To Do sync loop drives, layered on [`GraphClient`] (OUTLOOK_PARITY.md
//! §5.5/§5.6, item 5.3 / A-05, T-12).
//!
//! Every method returns `lookout_core` types through the pure mapping in the
//! parent module, so the sync loop never touches Graph JSON. `@odata.nextLink`
//! paging is handled by [`GraphClient::get_all`]; writes are guarded by the
//! task's `@odata.etag` (`If-Match`) so a concurrent change fails loudly rather
//! than being clobbered.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::text::percent_encode;
use lookout_core::CalendarTask;

use crate::client::GraphClient;
use crate::error::Result;

use super::{checklist_item_from_graph, linked_resource_from_graph, todo_list_from_graph, todo_task_from_graph, ChecklistStep, LinkedResource, TodoList};

/// Most To Do lists read for one account.
const MAX_LISTS: usize = 500;
/// Most tasks read for one list (a guard against an unbounded list).
const MAX_TASKS: usize = 5000;

/// The `$select` fields the task read needs - exactly what the mapper consumes,
/// so Graph doesn't return full task bodies or navigation properties.
const TASK_SELECT: &str = "$select=id,title,body,importance,status,dueDateTime,startDateTime,completedDateTime,createdDateTime,categories";

/// The Microsoft To Do HTTP operations, over a shared [`GraphClient`].
pub struct TodoClient {
    graph: GraphClient,
}

impl TodoClient {
    pub fn new(graph: GraphClient) -> Self {
        TodoClient { graph }
    }

    /// The account's To Do lists (`/me/todo/lists`). Graph returns the built-in
    /// default list plus the user's own; the well-known flagged-email list is
    /// *not* returned here.
    pub async fn list_lists(&self) -> Result<Vec<TodoList>> {
        let items = self.graph.get_all("me/todo/lists?$top=100&$select=id,displayName,wellknownListName", MAX_LISTS).await?;
        Ok(items.iter().filter_map(todo_list_from_graph).collect())
    }

    /// Every task in `list_id`, mapped into that list's synthetic calendar.
    pub async fn list_tasks(&self, list_id: &str) -> Result<Vec<CalendarTask>> {
        let path = format!("me/todo/lists/{}/tasks?$top=100&{TASK_SELECT}", encode_segment(list_id));
        let items = self.graph.get_all(&path, MAX_TASKS).await?;
        Ok(items.iter().filter_map(|value| todo_task_from_graph(value, list_id)).collect())
    }

    /// Creates `task` in `list_id`, returning Graph's copy (with its id/etag)
    /// when the response carried one.
    pub async fn create_task(&self, list_id: &str, task: &CalendarTask) -> Result<Option<CalendarTask>> {
        let path = format!("me/todo/lists/{}/tasks", encode_segment(list_id));
        let created = self.graph.post(&path, &super::task_to_todo_body(task)).await?;
        Ok(todo_task_from_graph(&created, list_id))
    }

    /// Updates `task` in place, guarded by its etag (`If-Match`) so a concurrent
    /// change fails loudly (HTTP 412) rather than being clobbered.
    pub async fn update_task(&self, list_id: &str, task: &CalendarTask) -> Result<Option<CalendarTask>> {
        let path = format!("me/todo/lists/{}/tasks/{}", encode_segment(list_id), encode_segment(&task.uid.0));
        let mut headers: Vec<(&str, &str)> = vec![("Prefer", "return=representation")];
        if let Some(etag) = task.etag.as_deref() {
            headers.push(("If-Match", etag));
        }
        let updated = self.graph.patch_with_headers(&path, &super::task_to_todo_body(task), &headers).await?;
        Ok(todo_task_from_graph(&updated, list_id).or_else(|| Some(task.clone())))
    }

    /// Deletes the task `task_id` in `list_id`, guarded by its etag (`If-Match`)
    /// when known.
    pub async fn delete_task(&self, list_id: &str, task_id: &str, etag: Option<&str>) -> Result<()> {
        let path = format!("me/todo/lists/{}/tasks/{}", encode_segment(list_id), encode_segment(task_id));
        let headers: Vec<(&str, &str)> = etag.map(|etag| vec![("If-Match", etag)]).unwrap_or_default();
        self.graph.delete_with_headers(&path, &headers).await?;
        Ok(())
    }

    /// One task's checklist items (steps). Not part of the shared task model yet
    /// (T-07 / item 4.11), so the session doesn't call this; the transport is
    /// ready for when it does.
    pub async fn checklist_items(&self, list_id: &str, task_id: &str) -> Result<Vec<ChecklistStep>> {
        let path = format!("me/todo/lists/{}/tasks/{}/checklistItems", encode_segment(list_id), encode_segment(task_id));
        let items = self.graph.get_all(&path, MAX_TASKS).await?;
        Ok(items.iter().filter_map(checklist_item_from_graph).collect())
    }

    /// One task's linked resources - for a flagged-email task, the message it
    /// points at. Mapped for completeness (M-10 already syncs flags directly).
    pub async fn linked_resources(&self, list_id: &str, task_id: &str) -> Result<Vec<LinkedResource>> {
        let path = format!("me/todo/lists/{}/tasks/{}/linkedResources", encode_segment(list_id), encode_segment(task_id));
        let items = self.graph.get_all(&path, MAX_TASKS).await?;
        Ok(items.iter().filter_map(linked_resource_from_graph).collect())
    }
}

/// Percent-encodes a path segment (a Graph list/task id), leaving RFC 3986
/// unreserved characters. Graph ids are base64-ish and may contain `+`, `/` or
/// `=`, which must not leak into the URL path.
fn encode_segment(segment: &str) -> String {
    percent_encode(segment)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::error::GraphError;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String> {
            Ok("stub".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> TodoClient {
        TodoClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    fn task_json() -> Value {
        json!({
            "id": "task-1",
            "@odata.etag": "etag-1",
            "title": "Buy milk",
            "body": { "content": "Two litres", "contentType": "text" },
            "importance": "high",
            "status": "notStarted",
            "categories": ["Errands"]
        })
    }

    #[tokio::test]
    async fn list_lists_maps_every_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "l1", "displayName": "Tasks", "wellknownListName": "defaultList" }]
            })))
            .mount(&server)
            .await;

        let lists = client(&server).list_lists().await.unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].display_name, "Tasks");
    }

    #[tokio::test]
    async fn list_tasks_maps_into_the_synthetic_calendar() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists/l1/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [task_json()] })))
            .mount(&server)
            .await;

        let tasks = client(&server).list_tasks("l1").await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].calendar_id.0, "todo:l1");
        assert_eq!(tasks[0].summary.as_deref(), Some("Buy milk"));
    }

    #[tokio::test]
    async fn create_task_posts_the_body_and_reads_the_copy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/todo/lists/l1/tasks"))
            .and(body_partial_json(json!({ "title": "Buy milk" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(task_json()))
            .mount(&server)
            .await;

        let task = todo_task_from_graph(&task_json(), "l1").unwrap();
        let created = client(&server).create_task("l1", &task).await.unwrap().unwrap();
        assert_eq!(created.uid.0, "task-1");
        assert_eq!(created.etag.as_deref(), Some("etag-1"));
    }

    #[tokio::test]
    async fn update_task_sends_if_match_and_prefer() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/todo/lists/l1/tasks/task-1"))
            .and(header("If-Match", "etag-1"))
            .and(header("Prefer", "return=representation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "task-1", "@odata.etag": "etag-2", "title": "Buy milk" })))
            .mount(&server)
            .await;

        let task = todo_task_from_graph(&task_json(), "l1").unwrap();
        let updated = client(&server).update_task("l1", &task).await.unwrap().unwrap();
        assert_eq!(updated.etag.as_deref(), Some("etag-2"));
    }

    #[tokio::test]
    async fn delete_task_sends_if_match() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/todo/lists/l1/tasks/task-1"))
            .and(header("If-Match", "etag-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_task("l1", "task-1", Some("etag-1")).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_maps_to_a_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/todo/lists/l1/tasks/task-1"))
            .respond_with(ResponseTemplate::new(412).set_body_string("precondition failed"))
            .mount(&server)
            .await;

        let task = todo_task_from_graph(&task_json(), "l1").unwrap();
        match client(&server).update_task("l1", &task).await.unwrap_err() {
            GraphError::Status { status, .. } => assert_eq!(status, 412),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn checklist_items_and_linked_resources_read_their_navigation_properties() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists/l1/tasks/task-1/checklistItems"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "s1", "displayName": "Step one", "isChecked": false }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/todo/lists/l1/tasks/task-1/linkedResources"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "r1", "webUrl": "https://outlook.office.com/mail/id/abc", "externalId": "msg-1" }]
            })))
            .mount(&server)
            .await;

        let steps = client(&server).checklist_items("l1", "task-1").await.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].display_name, "Step one");

        let links = client(&server).linked_resources("l1", "task-1").await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].external_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn encode_segment_escapes_path_specials() {
        assert_eq!(encode_segment("l1"), "l1");
        assert_eq!(encode_segment("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
