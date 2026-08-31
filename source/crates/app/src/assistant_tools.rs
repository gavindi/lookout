//! The Lookout tab's AI Chat assistant: OpenAI-style function calling over
//! the app's own data. The tools the agent may invoke read nothing but the
//! local caches and in-memory snapshots the rest of the dashboard already
//! uses - each connected mail account's SQLite cache (envelopes + the FTS5
//! index), the CardDAV contact snapshots the People screen shows, and the
//! merged task set - so the assistant answers from the same offline, synced
//! data the user sees, with no live IMAP/CalDAV traffic and nothing leaving
//! the machine beyond the tool calls the user's prompt provokes.
//!
//! [`chat_with_tools`] runs the whole conversation: it POSTs the prompt with
//! the tool definitions, and whenever the model answers with `tool_calls`
//! instead of text it executes each call against a [`ToolContext`] captured
//! on the GTK thread, feeds the results back as `tool` messages, and repeats
//! until the model answers in plain text (or hits the turn cap - a runaway
//! agent is stopped, not served forever). A failing tool is reported to the
//! model as a JSON `{"error": ...}` payload rather than aborting the
//! conversation, so it can recover by asking for narrower parameters.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, CalendarId, CalendarTask, EmailAddress, EmailSummary, EventOccurrence, TaskStatus};
use lookout_mail::Cache;

use crate::contacts_view::ContactsAccountSnapshot;

/// The fixed system message every assistant conversation starts with: the
/// agent's persona and its ground rules - answer from the tools' data, own
/// up when a tool finds nothing, and format the reply as readable markdown
/// (the chat view renders it as HTML: headings, bold, lists, code, tables,
/// and links - `#`/`##` headings each become their own visual card). Graphics
/// are allowed: markdown images (`![alt](url)`) or inline SVG inside a
/// ```html fenced block. The tool descriptions carry the rest of the
/// instructions.
pub const SYSTEM_PROMPT: &str = "You are Lookout's local assistant, helping the user work with their own email, contacts, tasks, and calendar. \
Use the provided tools to look up their real data whenever the question involves specific messages, people, tasks, or events. \
Answer plainly from what the tools return; if a tool fails or finds nothing, say so. \
Never claim access to anything the tools don't cover. \
Format your answer with markdown: headings, **bold**, lists, `code`, links, and tables where they help. \
Each `#` or `##` heading starts a new visual card in the chat view, so use them to separate distinct topics in a multi-part answer, and reserve `###`/`####` for sub-headings within a single topic; a short, single-topic answer doesn't need a heading at all. \
When you mention a specific email or calendar event a tool returned, link to it using that item's own `link` field verbatim as the markdown link's URL, e.g. `[Team sync](<link value>)` - never invent, alter, or guess a link; only use a `link` value a tool actually returned. \
For graphics (charts, diagrams, icons), use markdown images or inline SVG inside a ```html fenced block - never scripts.";

/// The most tool-call rounds a single conversation may take before we give
/// up. Enough for the model to look around (recent mail, then a search,
/// then tasks) while capping runaway loops.
const MAX_TOOL_TURNS: usize = 8;
/// The default row cap for a tool result.
const DEFAULT_LIMIT: usize = 10;
/// The hard cap on any tool's `limit` argument - results stay small enough
/// to fit a model's context window without the caller truncating.
const MAX_LIMIT: usize = 50;

/// Everything the assistant's tools can read, captured on the GTK thread at
/// ask time. All three halves are cheap clones of data the dashboard keeps
/// anyway: `Arc<Cache>` handles to the mail caches (the same WAL readers the
/// composer autocomplete and the dashboard's feeds use), the per-account
/// contact snapshots, and the merged task set. Tools never mutate any of it.
#[derive(Clone)]
pub struct ToolContext {
    /// Read-side handles on every connected mail account's cache.
    pub caches: Vec<Arc<Cache>>,
    /// One contact snapshot per connected contacts account, with its
    /// account id for labelling.
    pub contacts: Vec<(AccountId, ContactsAccountSnapshot)>,
    /// The merged task set from every source (CalDAV, Google Tasks, local).
    pub tasks: Vec<CalendarTask>,
    /// Every synced calendar occurrence across accounts, webcal
    /// subscriptions, and birthdays - the `upcoming_events` tool's source,
    /// gathered the same way the dashboard's own "Upcoming events" card
    /// does.
    pub occurrences: Vec<EventOccurrence>,
    /// The calendars the user has checked visible in "My calendars" -
    /// `upcoming_events` filters to these, same as the dashboard card, so
    /// the assistant never surfaces an event from a calendar the user has
    /// hidden.
    pub checked_calendars: HashSet<CalendarId>,
}

/// The OpenAI-style `tools` payload describing every function the agent may
/// call. Kept as data (not code) so the request body stays declarative and
/// the executor can stay a plain `match` on the name.
fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "recent_emails",
                "description": "The most recent cached email messages across all connected accounts, newest first. Each entry has the subject, sender, recipients, date, a preview, and whether it's unread. Only covers messages already synced locally.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "description": "How many messages to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_emails",
                "description": "Full-text search over the locally cached emails: subject, sender, recipients, and body text. Returns matching messages newest first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The words or phrase to search for"},
                        "limit": {"type": "integer", "description": "How many messages to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "top_contacts",
                "description": "The people you've corresponded with most across all connected mail accounts, from lifetime email history, most-contacted first with each person's count.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "description": "How many people to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_contacts",
                "description": "The contacts from the address books of all connected contacts accounts: names and email addresses. An optional query narrows it to people whose name or email contains the text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Optional text to match against names and email addresses"},
                        "limit": {"type": "integer", "description": "How many contacts to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_tasks",
                "description": "The tasks from every connected task source (CalDAV, Google Tasks, and on-device). By default only outstanding (not completed) tasks are returned, soonest-due first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["outstanding", "all"], "description": "\"outstanding\" (the default) hides completed tasks; \"all\" includes them"},
                        "limit": {"type": "integer", "description": "How many tasks to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "upcoming_events",
                "description": "Upcoming calendar events from every synced, currently-visible calendar (accounts, webcal subscriptions, birthdays), soonest first. Only covers events already synced locally within the app's sync horizon.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "days": {"type": "integer", "description": "How many days ahead to look (default 14, at most 90)", "minimum": 1, "maximum": 90},
                        "limit": {"type": "integer", "description": "How many events to return (default 10, at most 50)", "minimum": 1, "maximum": 50}
                    },
                    "additionalProperties": false
                }
            }
        }
    ])
}

/// Asks the configured agent a question with the full tool surface, running
/// the tool-call loop to completion. Returns the assistant's final plain-text
/// reply. The one `system` message is fixed at call time (the UI passes a
/// small persona note); the agent's own tool descriptions carry the rest of
/// the instructions, so no per-tool prompt text is needed here.
pub async fn chat_with_tools(base_url: &str, token: &str, agent: &str, prompt: &str, system: &str, ctx: &ToolContext) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err("Type something to ask the assistant".to_string());
    }
    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": prompt}),
    ];
    let tools = tool_definitions();
    for _turn in 0..MAX_TOOL_TURNS {
        let body = serde_json::json!({
            "model": agent,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "stream": false,
        });
        let reply = crate::assistant::chat_completions(base_url, token, agent, &body).await?;
        let message = reply
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .cloned()
            .ok_or_else(|| "The server's chat reply had no message".to_string())?;
        let tool_calls = message.get("tool_calls").and_then(|calls| calls.as_array()).cloned().unwrap_or_default();
        if tool_calls.is_empty() {
            return message
                .get("content")
                .and_then(|content| content.as_str())
                .map(str::to_string)
                .ok_or_else(|| "The server's chat reply had no message content".to_string());
        }
        // The assistant message (with its tool calls) joins the transcript
        // as-is, then one `tool` reply per call reports the executed result.
        messages.push(message);
        for call in &tool_calls {
            let id = call.get("id").and_then(|id| id.as_str()).unwrap_or_default().to_string();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(|name| name.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(|arguments| arguments.as_str())
                .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
                .unwrap_or(serde_json::Value::Null);
            let content = match execute_tool(ctx, &name, &arguments).await {
                Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
                // A tool failure is data for the model, not a conversation
                // failure - it can pick narrower parameters and try again.
                Err(e) => serde_json::json!({"error": e}).to_string(),
            };
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": content,
            }));
        }
    }
    Err(format!("The assistant made too many tool calls (limit {MAX_TOOL_TURNS}) - please ask a narrower question."))
}

/// Executes one tool call against the context. Cache-backed tools read on
/// the tokio blocking pool - the same discipline as the dashboard's
/// `spawn_cache_read` - so a slow SQLite scan never stalls the worker
/// threads' other futures.
async fn execute_tool(ctx: &ToolContext, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    match name {
        "recent_emails" => recent_emails(ctx, args).await,
        "search_emails" => search_emails(ctx, args).await,
        "top_contacts" => top_contacts(ctx, args).await,
        "list_contacts" => list_contacts(ctx, args),
        "list_tasks" => list_tasks(ctx, args),
        "upcoming_events" => upcoming_events(ctx, args),
        other => Err(format!("Unknown tool {other:?}")),
    }
}

/// The `limit` argument's value: the caller's number clamped to
/// `1..=MAX_LIMIT`, or `DEFAULT_LIMIT` when absent/malformed.
fn arg_limit(args: &serde_json::Value) -> usize {
    args.get("limit")
        .and_then(|limit| limit.as_u64())
        .map(|limit| (limit as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT)
}

/// A `Name <address>` label for a person, or the bare address when unnamed.
fn address_label(address: &EmailAddress) -> String {
    match &address.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", address.address),
        _ => address.address.clone(),
    }
}

/// The newest cached messages across every mail account, merged and sorted
/// newest-first, capped at `limit`.
async fn recent_emails(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let limit = arg_limit(args);
    let mut messages: Vec<EmailSummary> = Vec::new();
    for cache in &ctx.caches {
        let cache = cache.clone();
        let found = tokio::task::spawn_blocking(move || cache.recent_messages(limit))
            .await
            .map_err(|e| format!("Cache read failed: {e}"))?;
        match found {
            Ok(found) => messages.extend(found),
            Err(e) => tracing::warn!("assistant recent_emails cache read failed: {e}"),
        }
    }
    messages.sort_by_key(|message| std::cmp::Reverse(message.date));
    messages.truncate(limit);
    Ok(serialize_emails(&messages))
}

/// Full-text search across every mail account's cache, merged newest-first.
async fn search_emails(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let query = args.get("query").and_then(|query| query.as_str()).unwrap_or("").trim();
    if query.is_empty() {
        return Err("search_emails needs a non-empty \"query\" argument".to_string());
    }
    let limit = arg_limit(args);
    let mut messages: Vec<EmailSummary> = Vec::new();
    for cache in &ctx.caches {
        let cache = cache.clone();
        let query = query.to_string();
        let found = tokio::task::spawn_blocking(move || cache.search(&query, limit))
            .await
            .map_err(|e| format!("Cache read failed: {e}"))?;
        match found {
            Ok(found) => messages.extend(found),
            Err(e) => tracing::warn!("assistant search_emails cache read failed: {e}"),
        }
    }
    messages.sort_by_key(|message| std::cmp::Reverse(message.date));
    messages.truncate(limit);
    Ok(serialize_emails(&messages))
}

/// The most-corresponded-with people across every mail cache, from the
/// cumulative `addresses` table the composer autocomplete ranks by.
async fn top_contacts(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let limit = arg_limit(args);
    // address -> (best-known display name, lifetime appearances), summed
    // across accounts so the same person in two accounts counts once.
    let mut by_address: HashMap<String, (String, i64)> = HashMap::new();
    for cache in &ctx.caches {
        let cache = cache.clone();
        let found = tokio::task::spawn_blocking(move || cache.top_addresses(MAX_LIMIT * 4))
            .await
            .map_err(|e| format!("Cache read failed: {e}"))?;
        let Ok(entries) = found else {
            tracing::warn!("assistant top_contacts cache read failed: {:?}", found);
            continue;
        };
        for (address, count) in entries {
            let entry = by_address.entry(address.address).or_insert_with(|| (String::new(), 0));
            entry.1 += count;
            if entry.0.is_empty() {
                entry.0 = address.name.unwrap_or_default();
            }
        }
    }
    let mut ranked: Vec<(String, String, i64)> = by_address.into_iter().map(|(address, (name, count))| (name, address, count)).collect();
    // Count descending, ties broken by address so the output is stable (the
    // hash map's iteration order is not).
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    ranked.truncate(limit);
    let entries: Vec<serde_json::Value> = ranked
        .into_iter()
        .map(|(name, address, count)| serde_json::json!({"name": name, "address": address, "count": count}))
        .collect();
    Ok(serde_json::Value::Array(entries))
}

/// The address-book contacts across every contacts account: name plus all
/// email addresses, optionally narrowed by a name/email substring match.
fn list_contacts(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let limit = arg_limit(args);
    let query = args.get("query").and_then(|query| query.as_str()).unwrap_or("").trim().to_lowercase();
    let mut entries: Vec<serde_json::Value> = Vec::new();
    for (account_id, snapshot) in &ctx.contacts {
        for record in &snapshot.contacts {
            let name = record.card.full_name.clone().unwrap_or_default();
            let emails: Vec<String> = record.card.email_addresses().iter().map(address_label).collect();
            if !query.is_empty() {
                let haystack = format!("{} {}", name.to_lowercase(), emails.join(" ").to_lowercase());
                if !haystack.contains(&query) {
                    continue;
                }
            }
            entries.push(serde_json::json!({
                "account": account_id.0,
                "name": name,
                "emails": emails,
            }));
            if entries.len() >= limit {
                break;
            }
        }
        if entries.len() >= limit {
            break;
        }
    }
    Ok(serde_json::Value::Array(entries))
}

/// The task set, optionally including completed ones, soonest-due first.
fn list_tasks(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let limit = arg_limit(args);
    let include_completed = args.get("status").and_then(|status| status.as_str()) == Some("all");
    let mut tasks: Vec<&CalendarTask> = ctx.tasks.iter().filter(|task| include_completed || task.status != TaskStatus::Completed).collect();
    // Tasks without a due date sort last.
    tasks.sort_by_key(|task| task.due.unwrap_or(DateTime::<Utc>::MAX_UTC));
    tasks.truncate(limit);
    let entries: Vec<serde_json::Value> = tasks
        .iter()
        .map(|task| {
            serde_json::json!({
                "summary": task.summary.clone().unwrap_or_default(),
                "due": task.due.map(|due| due.to_rfc3339()),
                "status": task.status,
                "priority": task.priority.0,
                "categories": task.categories,
                "calendar": task.calendar_id.0,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(entries))
}

/// A tool result entry for one message: the fields the agent can answer
/// from, compact enough to stay within context budget.
fn serialize_emails(messages: &[EmailSummary]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "subject": message.subject.clone().unwrap_or_default(),
                "from": message.from.first().map(address_label).unwrap_or_default(),
                "to": message.to.iter().map(address_label).collect::<Vec<_>>(),
                "date": message.date.to_rfc3339(),
                "unread": message.is_unread(),
                "preview": message.preview.clone().unwrap_or_default(),
                "mailbox": message.mailbox.0,
                "uid": message.uid.0,
                "link": crate::chat_links::open_message_link(&message.mailbox, message.uid),
            })
        })
        .collect();
    serde_json::Value::Array(entries)
}

/// The default horizon `upcoming_events` looks ahead when the caller
/// doesn't specify `days`, and the hard cap on that argument.
const DEFAULT_EVENT_HORIZON_DAYS: i64 = 14;
const MAX_EVENT_HORIZON_DAYS: i64 = 90;

/// The next occurrences across every checked calendar, soonest first,
/// within `days` of now. Reuses `lookout_view::upcoming_occurrences` - the
/// same filter/sort the dashboard's own "Upcoming events" card runs -
/// rather than re-deriving it.
fn upcoming_events(ctx: &ToolContext, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let limit = arg_limit(args);
    let days = args
        .get("days")
        .and_then(|days| days.as_i64())
        .unwrap_or(DEFAULT_EVENT_HORIZON_DAYS)
        .clamp(1, MAX_EVENT_HORIZON_DAYS);
    let now = chrono::Local::now();
    let horizon = chrono::Duration::days(days);
    let occurrences = crate::lookout_view::upcoming_occurrences(ctx.occurrences.iter(), now, horizon, &ctx.checked_calendars, limit);
    Ok(serialize_events(&occurrences))
}

/// A tool result entry for one calendar occurrence: the fields the agent
/// can answer from, plus a precomputed link back to the event.
fn serialize_events(occurrences: &[&EventOccurrence]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = occurrences
        .iter()
        .map(|occ| {
            serde_json::json!({
                "summary": occ.summary.clone().unwrap_or_default(),
                "start": occ.start.to_rfc3339(),
                "end": occ.end.to_rfc3339(),
                "all_day": occ.all_day,
                "location": occ.location.clone(),
                "link": crate::chat_links::open_event_link(occ),
            })
        })
        .collect();
    serde_json::Value::Array(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{EmailField, MailboxId, SystemFlagBit, TaskPriority, Uid, UidValidity, VCard};
    use lookout_dav::ContactRecord;

    fn task(summary: &str, due: Option<DateTime<Utc>>, completed: bool) -> CalendarTask {
        CalendarTask {
            uid: lookout_core::TaskUid(format!("uid-{summary}")),
            calendar_id: lookout_core::CalendarId("cal".to_string()),
            summary: Some(summary.to_string()),
            description: None,
            due,
            start: None,
            completed: if completed { Some(Utc::now()) } else { None },
            status: if completed { TaskStatus::Completed } else { TaskStatus::NeedsAction },
            priority: TaskPriority(0),
            percent_complete: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    fn context() -> ToolContext {
        ToolContext {
            caches: Vec::new(),
            contacts: Vec::new(),
            tasks: Vec::new(),
            occurrences: Vec::new(),
            checked_calendars: HashSet::new(),
        }
    }

    #[test]
    fn tool_definitions_cover_all_six_tools() {
        let definitions = tool_definitions();
        let names: Vec<&str> = definitions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
            .collect();
        assert_eq!(
            names,
            vec!["recent_emails", "search_emails", "top_contacts", "list_contacts", "list_tasks", "upcoming_events"]
        );
        // The search tool's query is required; the row caps stay bounded.
        let search = &definitions[1]["function"]["parameters"];
        assert_eq!(search["required"][0], "query");
        for tool in definitions.as_array().unwrap() {
            let max = tool["function"]["parameters"]["properties"]["limit"]["maximum"].as_u64().unwrap();
            assert!(max <= MAX_LIMIT as u64, "limits must stay within {MAX_LIMIT}");
        }
    }

    #[test]
    fn arg_limit_defaults_clamps_and_tolerates_garbage() {
        assert_eq!(arg_limit(&serde_json::json!({})), DEFAULT_LIMIT);
        assert_eq!(arg_limit(&serde_json::json!({"limit": 3})), 3);
        assert_eq!(arg_limit(&serde_json::json!({"limit": 0})), 1);
        assert_eq!(arg_limit(&serde_json::json!({"limit": 5000})), MAX_LIMIT);
        assert_eq!(arg_limit(&serde_json::json!({"limit": "lots"})), DEFAULT_LIMIT);
    }

    #[test]
    fn list_tasks_filters_completed_sorts_by_due_and_limits() {
        let mut ctx = context();
        ctx.tasks = vec![
            task("later", Some(Utc::now() + chrono::Duration::days(5)), false),
            task("done", Some(Utc::now() - chrono::Duration::days(1)), true),
            task("soon", Some(Utc::now() + chrono::Duration::hours(1)), false),
            task("undated", None, false),
        ];

        let outstanding = list_tasks(&ctx, &serde_json::json!({})).unwrap();
        let summaries: Vec<&str> = outstanding.as_array().unwrap().iter().filter_map(|entry| entry["summary"].as_str()).collect();
        assert_eq!(summaries, vec!["soon", "later", "undated"], "completed hidden, soonest-due first, undated last");

        let all = list_tasks(&ctx, &serde_json::json!({"status": "all", "limit": 2})).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2, "the limit is honored");
        assert_eq!(all[0]["summary"], "done", "completed tasks join when asked");
    }

    fn occurrence(uid: &str, start: DateTime<Utc>) -> EventOccurrence {
        EventOccurrence {
            uid: lookout_core::EventUid(uid.to_string()),
            calendar_id: lookout_core::CalendarId("cal-1".to_string()),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start,
            end: start + chrono::Duration::hours(1),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            transparency: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn upcoming_events_filters_to_checked_calendars_sorts_and_carries_a_link() {
        let mut ctx = context();
        let soon = occurrence("soon", Utc::now() + chrono::Duration::hours(1));
        let later = occurrence("later", Utc::now() + chrono::Duration::days(2));
        let mut hidden = occurrence("hidden", Utc::now() + chrono::Duration::hours(2));
        hidden.calendar_id = lookout_core::CalendarId("cal-hidden".to_string());
        ctx.occurrences = vec![later.clone(), hidden, soon.clone()];
        ctx.checked_calendars = std::iter::once(soon.calendar_id.clone()).collect();

        let result = upcoming_events(&ctx, &serde_json::json!({})).unwrap();
        let entries = result.as_array().unwrap();
        assert_eq!(entries.len(), 2, "the unchecked calendar's event is excluded");
        assert_eq!(entries[0]["summary"], "soon", "soonest first");
        assert_eq!(entries[1]["summary"], "later");
        assert!(
            entries[0]["link"].as_str().unwrap().starts_with("lookout-action:open-event?data="),
            "an event carries a ready-to-use deep link"
        );
    }

    #[tokio::test]
    async fn list_tasks_rejects_unknown_tools() {
        let ctx = context();
        let result = execute_tool(&ctx, "drop_database", &serde_json::Value::Null).await;
        assert!(result.is_err(), "unknown tools must be refused, not silently ignored");
    }

    #[test]
    fn list_contacts_matches_by_name_and_email() {
        use crate::contacts_view::test_snapshot;

        let mut ctx = context();
        let mut card = VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some("c1".to_string()),
            full_name: Some("Ada Lovelace".to_string()),
            name: None,
            organization: None,
            title: None,
            emails: vec![EmailField {
                types: vec!["work".to_string()],
                address: "ada@example.org".to_string(),
            }],
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
            categories: Vec::new(),
            other: Vec::new(),
        };
        let snapshot = test_snapshot(
            "Test",
            vec![ContactRecord {
                href: "ada.vcf".to_string(),
                etag: None,
                card: card.clone(),
            }],
        );
        ctx.contacts.push((AccountId("/test/books".to_string()), snapshot));
        card.full_name = Some("Grace Hopper".to_string());
        card.uid = Some("c2".to_string());
        card.emails = vec![EmailField {
            types: vec!["work".to_string()],
            address: "grace@example.org".to_string(),
        }];
        let snapshot = test_snapshot(
            "Test",
            vec![ContactRecord {
                href: "grace.vcf".to_string(),
                etag: None,
                card,
            }],
        );
        ctx.contacts.push((AccountId("/test/books".to_string()), snapshot));

        let all = list_contacts(&ctx, &serde_json::json!({})).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2, "no query returns everyone");
        assert_eq!(all[0]["name"], "Ada Lovelace");
        assert_eq!(all[0]["emails"][0], "Ada Lovelace <ada@example.org>");

        let by_name = list_contacts(&ctx, &serde_json::json!({"query": "hopper"})).unwrap();
        assert_eq!(by_name.as_array().unwrap().len(), 1);
        assert_eq!(by_name[0]["name"], "Grace Hopper");

        let by_email = list_contacts(&ctx, &serde_json::json!({"query": "ADA@"})).unwrap();
        assert_eq!(by_email.as_array().unwrap().len(), 1, "email matching is case-insensitive");
    }

    /// Serves consecutive `POST /chat/completions` requests with the given
    /// status/body script, returning the conversation result and every
    /// request's raw body (so tests can assert on the transcript the loop
    /// builds).
    async fn serve_script(script: Vec<(&'static str, &'static str)>) -> (Result<String, String>, Vec<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in script {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            }
            requests
        });

        let (_, cache) = temp_cache_with_messages();
        let ctx = ToolContext { caches: vec![cache], ..context() };
        let result = chat_with_tools(&format!("http://127.0.0.1:{port}"), "sk-test", "gpt-4o", "What's new in my inbox?", SYSTEM_PROMPT, &ctx).await;
        let requests = server.await.unwrap();
        (result, requests)
    }

    /// The whole point of the module: the model calls a tool, the loop
    /// executes it against the local data, and the reply lands after the
    /// results were fed back.
    #[tokio::test]
    async fn chat_with_tools_executes_tool_calls_and_returns_the_final_answer() {
        let (result, requests) = serve_script(vec![
            (
                "200 OK",
                r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"recent_emails","arguments":"{}"}}]}}]}"#,
            ),
            ("200 OK", r#"{"choices":[{"message":{"role":"assistant","content":"You have 2 recent messages."}}]}"#),
        ])
        .await;

        assert_eq!(result.unwrap(), "You have 2 recent messages.");
        assert_eq!(requests.len(), 2, "one tool round, then the final answer");
        // The second request carries the assistant message's tool call and
        // the executed result (real cache data, serialized as JSON) back.
        assert!(requests[1].contains("\"role\":\"tool\""), "a tool message must follow the call");
        assert!(requests[1].contains("\"tool_call_id\":\"call_1\""));
        assert!(requests[1].contains("Truffle weekly roundup"), "the executed cache result must ride along");
        assert!(
            requests[1].contains("\"role\":\"assistant\""),
            "the assistant's tool-call message must stay in the transcript"
        );
    }

    /// A reply whose message has no `content` and no `tool_calls` is a
    /// protocol violation, not an empty answer.
    #[tokio::test]
    async fn chat_with_tools_reports_a_reply_without_content() {
        let (result, _) = serve_script(vec![("200 OK", r#"{"choices":[{"message":{"role":"assistant"}}]}"#)]).await;
        assert!(result.unwrap_err().contains("no message content"));
    }

    #[tokio::test]
    async fn chat_with_tools_rejects_a_blank_prompt_before_any_network_io() {
        let ctx = context();
        let error = chat_with_tools("http://127.0.0.1:1", "sk-test", "gpt-4o", "  ", SYSTEM_PROMPT, &ctx).await;
        assert!(error.unwrap_err().contains("Type something"));
    }

    /// Builds a real cache on disk (unique account id, like the mail crate's
    /// own cache tests) so the cache-backed executors are exercised
    /// end-to-end rather than against mocks.
    fn temp_cache_with_messages() -> (AccountId, Arc<Cache>) {
        let account_id = AccountId(format!("/test/assistant_tools_{}", uuid::Uuid::new_v4()));
        let cache = Arc::new(Cache::open(&account_id).unwrap());
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let older = EmailSummary {
            uid: Uid(1),
            mailbox: mailbox_id.clone(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: Some("Truffle weekly roundup".to_string()),
            from: vec![EmailAddress {
                name: Some("Truffle Security".to_string()),
                address: "no-reply@truffle.example".to_string(),
            }],
            to: vec![EmailAddress {
                name: None,
                address: "me@example.org".to_string(),
            }],
            cc: Vec::new(),
            date: Utc::now() - chrono::Duration::days(2),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: Some("Your scan finished, nothing found".to_string()),
            structure: None,
        };
        let mut newer = older.clone();
        newer.uid = Uid(2);
        newer.subject = Some("Lunch on Friday?".to_string());
        newer.from = vec![EmailAddress {
            name: Some("Ada Lovelace".to_string()),
            address: "ada@example.org".to_string(),
        }];
        newer.date = Utc::now();
        newer.flags.insert(SystemFlagBit::Seen);
        newer.preview = Some("Catching up over tacos".to_string());
        cache.replace_messages(&mailbox_id, UidValidity(1), &[older, newer]).unwrap();
        // The session's sync calls `record_addresses` separately from the
        // envelope write - replicate that so `top_addresses` has data.
        cache.load_messages(&mailbox_id).and_then(|messages| cache.record_addresses(&messages)).unwrap();
        (account_id, cache)
    }

    #[tokio::test]
    async fn recent_emails_returns_newest_first_with_labels() {
        let (account_id, cache) = temp_cache_with_messages();
        let ctx = ToolContext { caches: vec![cache], ..context() };

        let result = recent_emails(&ctx, &serde_json::json!({})).await.unwrap();
        let entries = result.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["subject"], "Lunch on Friday?", "newest first");
        assert_eq!(entries[0]["unread"], false, "the seen flag rides along");
        assert_eq!(entries[1]["unread"], true);
        assert_eq!(entries[0]["from"], "Ada Lovelace <ada@example.org>");
        assert_eq!(entries[0]["mailbox"], format!("{}:INBOX", account_id.0));
        assert!(
            entries[0]["link"].as_str().unwrap().starts_with("lookout-action:open-message?data="),
            "a message carries a ready-to-use deep link"
        );

        let limited = recent_emails(&ctx, &serde_json::json!({"limit": 1})).await.unwrap();
        assert_eq!(limited.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_emails_finds_body_and_subject_terms() {
        let (_, cache) = temp_cache_with_messages();
        let ctx = ToolContext { caches: vec![cache], ..context() };

        let by_subject = search_emails(&ctx, &serde_json::json!({"query": "truffle"})).await.unwrap();
        assert_eq!(by_subject.as_array().unwrap().len(), 1);
        assert_eq!(by_subject[0]["subject"], "Truffle weekly roundup");

        let by_body = search_emails(&ctx, &serde_json::json!({"query": "scan"})).await.unwrap();
        assert_eq!(by_body.as_array().unwrap().len(), 1);

        let empty = search_emails(&ctx, &serde_json::json!({"query": "  "})).await;
        assert!(empty.is_err(), "a blank query is an error, not a full dump");
    }

    #[tokio::test]
    async fn top_contacts_sums_across_caches_and_ranks() {
        let (_, cache) = temp_cache_with_messages();
        // A second cache with the same correspondent, to prove summing.
        let second_id = AccountId(format!("/test/assistant_tools_second_{}", uuid::Uuid::new_v4()));
        let second = Arc::new(Cache::open(&second_id).unwrap());
        let second_mailbox = MailboxId::new(&second_id, "INBOX");
        let message = EmailSummary {
            uid: Uid(1),
            mailbox: second_mailbox.clone(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: Some("Re: lunch".to_string()),
            from: vec![EmailAddress {
                name: Some("Ada Lovelace".to_string()),
                address: "ada@example.org".to_string(),
            }],
            to: Vec::new(),
            cc: Vec::new(),
            date: Utc::now(),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            structure: None,
        };
        second.replace_messages(&second_mailbox, UidValidity(1), std::slice::from_ref(&message)).unwrap();
        second.record_addresses(std::slice::from_ref(&message)).unwrap();

        let ctx = ToolContext {
            caches: vec![cache, second],
            ..context()
        };
        let result = top_contacts(&ctx, &serde_json::json!({})).await.unwrap();
        let entries = result.as_array().unwrap();
        let ada = entries.iter().find(|entry| entry["address"] == "ada@example.org").unwrap();
        assert_eq!(ada["count"], 2, "the same address in two caches counts once");
        assert_eq!(ada["name"], "Ada Lovelace");
        assert!(
            entries[0]["count"].as_i64().unwrap() >= entries[1]["count"].as_i64().unwrap(),
            "ranked by count, descending"
        );
        // The count-2 ties (ada, me) sort deterministically by address.
        let first: Vec<String> = entries.iter().take(2).filter_map(|entry| entry["address"].as_str().map(str::to_string)).collect();
        assert_eq!(first, vec!["ada@example.org".to_string(), "me@example.org".to_string()]);
    }
}
