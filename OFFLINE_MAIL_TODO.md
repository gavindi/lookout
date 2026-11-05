# Work offline and the Outbox (M-40)

This plan implements the last Mail gap in `OUTLOOK_PARITY.md` §5.1 (**M-40**): a
user-controlled **Work Offline** mode and an **Outbox** folder in the folder pane.

Lookout already has a persistent local outbox, schedule send, undo send and cache-first
reads. What it lacks is a switch that puts the mail sessions into an offline state (so a
launch or a toggle stops every IMAP/Graph round trip) and a folder that lists the queue.
It also cannot currently queue a send while the account is disconnected: the session only
handles `SendMessage` inside a live connection, so the command waits for reconnect instead
of landing in the outbox.

**Status:** complete. Parts A–G are implemented (the shared signal and shell, Mail,
Calendar/webcal, People, Tasks, the Outbox folder, and the docs close-out).

**Progress:** A1–A5, B1–B5, C1–C5, P1–P2, T1–T4, F1–F6 and G1–G2 are complete. `cargo fmt --all`,
`cargo check --workspace --all-targets` and
`cargo clippy --workspace --all-targets -- -D warnings -A clippy::result_large_err`
are clean. Tests written for the completed parts have been added but **not run** (see the
Tests section). Nothing is committed and no version was bumped.

**Effort:** `L` — roughly one to two weeks, because the approved scope pauses every
network module, not just Mail.

**Data-format change:** none in the mail cache. The work adds GSettings keys and, for
Graph To Do, one new read-only task cache so To Do lists survive an offline launch.
The outbox table and every existing `PRAGMA user_version` stay untouched.

---

## Resolved decisions

| Decision | Outcome |
|---|---|
| Scope of the toggle | **Full scope.** Mail, Calendar (CalDAV + Graph), webcal, People (CardDAV + Graph) and Tasks (CalDAV VTODO, Google Tasks, Graph To Do) all stop network activity. |
| Shape of the Outbox row | **One synthetic top-level Outbox row**, in the same style as All Inboxes, grouping all accounts' items in one list. |
| Visibility | **Always visible**, as Outlook's Outbox is; the badge shows the count and hides at zero. |

---

## What "Work Offline" means

### In scope

- One app-wide toggle, persisted across restarts, that stops every configured account's
  network traffic without quitting Lookout.
- Mail: cached folders, messages and bodies remain readable; sends, cancels, reschedules
  and retries operate on the local outbox and are delivered when the account goes online.
- Calendar: cached months and tasks remain visible; writes are refused with a clear
  message because there is no offline mutation queue.
- People: the cached contact snapshot remains visible; edits, imports and merges are
  refused with a clear message.
- Tasks: cached CalDAV and Google Tasks lists remain visible, plus a new Graph To Do cache
  so Microsoft tasks also survive an offline launch; writes are refused.
- Webcal: the cached month remains visible; subscribing/unsubscribing still updates the
  local list, but no feed is fetched until online.
- Reminders and the EDS mirror keep working from local data (they never needed the
  network).
- A visible indicator in every module with a one-click **Go online** action.

### Out of scope

- Offline write queues for Calendar, People and Tasks. Outlook syncs those later; Lookout
  does not have a mutation log, and building one is a separate item. The editors say
  "You're working offline — reconnect to save" and stay open.
- A local draft store. Compose autosave to IMAP Drafts cannot run offline; the composer
  keeps its text in memory and reports that it was not saved. Sends still queue.
- The Copilot-style assistant's own HTTP endpoint, weather (not implemented) and any other
  explicit user-initiated online action outside the sync modules.
- Per-account offline state. The switch is app-wide, like Outlook's.

---

## State before this work (for reference)

| Module | Session / loop | Cache shown at startup | Offline write behaviour today |
|---|---|---|---|
| Mail (IMAP) | `lookout_mail::session::run_account_session` | Folders, Inbox messages, outbox, flagged mail replay before login | Sends only queue inside a live connection; a disconnected send waits in `carried_command` |
| Mail (Graph) | `lookout_graph::run_graph_mail_session` | Same replay via `replay_cache` | Same; the transport is a session-local choice |
| Calendar (CalDAV) | `lookout_dav::session::run_calendar_session` | Current month replays from `CalendarCache` before connecting; tasks do **not** replay | Writes hit the network and fail; the editor closes on save before the answer lands |
| Calendar (Graph) | `lookout_graph::calendar::session::run_calendar_session` | **No replay.** Months are written to the same `CalendarCache` by the app for search only | Writes fail; no offline display |
| Webcal | `lookout_dav::subscription::run_subscription_session` | Current month's cached feeds replay before fetching | List changes apply locally; fetches fail |
| People (CardDAV/Graph) | App-side `sync_contacts_account` poll loop | Full cached snapshot replay before the first poll | Writes await a reply and fail; editors close optimistically |
| Tasks (CalDAV) | Same calendar session as Calendar | `CalendarCache` stores tasks, but the session does not replay them | Writes fail |
| Tasks (Google) | `app::google_tasks::run_google_tasks_session` | Lists and tasks replay from `TasksCache` | Writes fail |
| Tasks (Graph To Do) | `lookout_graph::todo::run_todo_session` | **No cache at all** | Writes fail |

Shared facts:

- Every actor already has a reconnect/backoff or poll loop, and every one is driven by an
  `async_channel` command receiver plus an event sender.
- `ConnectionState` exists in two forms: `lookout_mail::session::ConnectionState` and
  `lookout_core::calendar_protocol::ConnectionState`.
- `tokio::sync::watch` is already used in the mail session for the selected mailbox, so
  the signal proposed below is an existing pattern, not a new dependency.
- `Gio.NetworkMonitor` only sends `Reconnect`; it does not currently have a manual-offline
  concept.

---

## Design

### One shared offline signal

Create **one** `tokio::sync::watch` channel in `build_window`:

```text
let (offline_tx, offline_rx) = tokio::sync::watch::channel(settings.get_bool(WORK_OFFLINE));
```

- Keep `offline_tx` in `UiState` for the app's lifetime and call `send(bool)` from the
  toggle.
- Every actor receives a cloned `tokio::sync::watch::Receiver<bool>` at spawn time.
- New accounts/actors created while offline use `offline_tx.subscribe()`, whose initial
  value is already `true` — no startup race.
- Each actor checks the receiver before connecting/polling and selects on
  `offline_rx.changed()` while waiting, so a toggle interrupts an idle wait immediately.
- A watch receiver's `changed()` returns `Err` only if the sender is dropped. The app
  holds the sender forever; tests keep `_offline_tx` alive for the same reason. Document
  that an `Err` means "signal gone" and must not spin a `select!`.

A per-actor `SetOffline` command is deliberately **not** added: one signal reaches all
actors, including app-side loops, without adding a variant to four unrelated command
enums or broadcasting from the app.

Add `ConnectionState::Offline` to both mail and calendar protocol enums so the UI can
distinguish a user-requested pause from an error or a dropped connection.

### Actor changes at a glance

| Actor | Signature change | Offline behaviour |
|---|---|---|
| `run_account_session` | extra `offline_rx` | Skip login; wait; process outbox commands; on online reset backoff and connect |
| `run_graph_mail_session` | extra `offline_rx` | Skip initial/poll sync and drain; replay cache; process outbox commands |
| `run_calendar_session` (DAV) | extra `offline_rx` | Replay month + tasks; skip connect/poll; refuse writes |
| `run_calendar_session` (Graph) | extra `offline_rx` | Skip connect/poll; refuse writes |
| `run_subscription_session` | extra `offline_rx` | Skip `sync_all`; re-emit cached feeds for a requested month |
| `sync_contacts_account` | extra `offline_rx` | Skip polls; reply to writes with an offline error |
| `run_google_tasks_session` | extra `offline_rx` | Skip token refresh and sync; reply to writes with an offline error |
| `run_todo_session` | extra `offline_rx` | Skip connect/poll; reply to writes; fast-paint the new cache |

### UI

- **Send / Receive ribbon tab** (Mail only, as in Outlook): a **Preferences** group with a
  **Work Offline** toggle. Icon `network-offline-symbolic` while on,
  `network-transmit-receive-symbolic` while off.
- **A title-bar indicator** so the state is visible in Calendar, People and Tasks too: a
  small button next to the existing pane toggles, icon `network-offline-symbolic`, tooltip
  "Working offline — mail, calendar, contacts and tasks won't sync. Click to go online."
  Clicking it goes online (or opens a tiny popover with **Go online** / **Stay offline** if
  an accidental click is a concern). Hide it while online.
- **Mail banner**: an `adw::Banner` above the message list, "Working offline — messages
  won't be sent or received.", button **Go online**. It sits beside the existing outbox
  and automatic-replies banners.
- **Editor guards**: while offline, the event, contact and task editors keep the dialog
  open and show "You're working offline — reconnect to save" instead of closing and
  letting the async failure arrive as a toast. The contact import/merge flows toast the
  same message before starting.
- **Refresh actions**: Send/Receive All, the Mail sync button, Calendar's month sync,
  People's Refresh and Tasks' Refresh either do nothing or toast "You're working offline".
  They must not leave a spinner that can never clear.

---

## Part A — Signal, preference and shell

### [x] A1 — Persist the preference (`S`)

- Add `WORK_OFFLINE` (`work-offline`) to `source/crates/app/src/settings.rs`, seeded
  `false` in the in-memory defaults.
- Add the `b` key to
  `source/data/gschema/io.github.gavindi.Lookout.gschema.xml`, default `false`, with a
  description naming all modules.
- Read it once in `build_window` to seed the watch channel and `UiState::work_offline`.

### [x] A2 — Create and store the signal (`S`)

- In `source/crates/app/src/window.rs` (near where the cache and settings are created):
  create `(offline_tx, offline_rx)`.
- Add `pub(crate) offline_tx: tokio::sync::watch::Sender<bool>` and
  `pub(crate) work_offline: bool` to `UiState` in `window/state.rs`, plus
  `pub(crate) offline_indicator: Option<gtk::Button>` for A4.
- Add a helper `offline_receiver(state) -> watch::Receiver<bool>` that returns
  `state.borrow().offline_tx.subscribe()`. Every connect site uses it, so a new account
  inherits the current value.

### [x] A3 — App wiring (`M`)

- Add `set_work_offline(state, offline)` in a small `window/offline.rs` (or beside the
  outbox helpers):
  - write the GSettings value;
  - update `UiState::work_offline`;
  - `offline_tx.send(offline)`;
  - reveal/hide the Mail banner and the title-bar indicator (A4);
  - clear stale `refreshing` / `syncing` markers so a later reconnect is not deduped;
  - toast "Working offline" / "You're back online";
  - on going online, seed nothing explicitly: each actor reconnects and syncs itself.
- Guards in `window/mail_actions.rs`:
  - `request_mailbox_sync` returns `false` without marking the mailbox `syncing`;
  - `request_account_refresh` returns without sending;
  - `send_receive_all` / `resync_current_view` toast and return.
- Guard the `Gio.NetworkMonitor` handler: do not send `Reconnect` while offline.
- Guard Calendar's `SyncMonth`/`FetchMonth` requests and the Tasks/People Refresh
  actions at their entry points (the actors also ignore them, but avoiding the dead
  request keeps spinners honest).
- If a body fetch was pending when the toggle flipped, show the offline page
  (`show_body_fetch_error`) rather than leaving the reading pane spinning.

### [x] A4 — Toggle, indicator and banner (`S`)

- `source/crates/app/src/window.rs`, Send / Receive band: add a **Preferences** group with
  a toggle via `RibbonGroup::toggle` (the shared ribbon already supports toggles), wired
  to `set_work_offline`.
- Title bar: add the offline indicator button next to the pane toggles, hidden while
  online. It reflects the same state as the ribbon toggle.
- Message box: add the `adw::Banner` with **Go online**, revealed while offline. Keep the
  text markup-escaped and reusable for future status banners.

### [x] A5 — Editor and import guards (`M`)

- `event_editor.rs`: take an `is_offline: Rc<dyn Fn() -> bool>` (or a shared
  `Rc<Cell<bool>>`) and, in the Save handler, show the error label and keep the dialog
  open when offline.
- `task_editor.rs`: the same for Save and Delete.
- `contacts_editor.rs`, `contact_merge_view.rs`, `contacts_view.rs` import/export: the
  same before dispatching a write; the People list stays usable.
- `task_lists.rs` create/rename/delete: refuse with a toast while offline.
- Calendar list lifecycle (create/rename/delete calendar) and colour write-back: refuse or
  keep local, matching the existing degrade-soft rules.

---

## Part B — Mail

### [x] B1 — Pass the signal into both mail sessions (`M`)

- `run_account_session(config, credentials, commands, interactive_commands, events, offline_rx)`.
- `run_graph_mail_session(config, client, commands, interactive_commands, events, offline_rx)`.
- Update the app call sites in `window/connect.rs` (`connect_account` and
  `connect_other_account`) to pass `offline_receiver(&state)`.
- Update the tests and examples:
  - `source/crates/mail/tests/imap_integration.rs` (two sessions);
  - `source/crates/mail/examples/smoke_test.rs` and `send_test.rs`;
  - `source/crates/graph/tests/graph_mail_session.rs` and
    `graph_mail_integration.rs`.
  Each test creates `let (_offline_tx, offline_rx) = tokio::sync::watch::channel(false);`
  and keeps `_offline_tx` alive.

### [x] B2 — IMAP offline loop and command pump (`M`)

- `ConnectionState::Offline` in `source/crates/mail/src/session.rs`.
- Outer loop in `run_account_session`:
  - before each connect, if `*offline_rx.borrow()`, emit `Offline` and enter
    `wait_offline`;
  - `connect_and_run` receives a cloned receiver;
  - when `wait_offline` sees the signal go false, reset the backoff to its initial value
    and continue the existing loop.
- `wait_offline` selects on the two command channels and `offline_rx.changed()`:
  - `Shutdown` → exit;
  - `SendMessage` → the cache-only queue path (B3);
  - `CancelOutbox` / `RescheduleOutbox` → cache-only;
  - `FetchAttachment` → `PartFetchFailed` "You're working offline.";
  - `FetchRawMessage` → `RawMessageFetchFailed` with the same wording;
  - `SaveDraft` → the new `DraftSaveFailed` (B5);
  - `SetPrefetchPolicy` / `ConfigureLocalFocus` / `Reconnect` → apply locally, no network;
  - everything else → debug log and drop (the reconnect sync supersedes it).
- `connect_and_run`:
  - add a `ShutdownReason::Offline` variant;
  - check `*offline_rx.borrow()` before the outbox drain and before each round of work;
    on true, log out (best-effort), abort the IDLE supervisor, return `Offline`;
  - add `offline_rx.changed()` to the IDLE `select!` (and any other long wait) so a
    toggle does not wait out `IDLE_SLICE`.
- Do not regress the undo-send ordering: commands are already handled before the drain;
  the offline check must also run before the drain so a toggle during the undo-send
  window prevents the send.

### [x] B3 — Extract the cache-only outbox handlers (`S`)

- Factor the bodies of `SendMessage`, `CancelOutbox` and `RescheduleOutbox` out of the big
  `connect_and_run` match into helpers usable without a session:
  - queue: serialize `OutboxMessage::from_composed`, `Cache::enqueue_outbox`, emit
    `OutboxQueued` + `OutboxContents`, update the next-due time;
  - cancel: `Cache::cancel_outbox`, emit `OutboxCancelled` (payload when decodable) or
    `OutboxCancelFailed`, emit `OutboxContents`;
  - reschedule: `Cache::reschedule_outbox`, update the next-due time, emit
    `OutboxContents`.
- The connected path calls the same helpers, so the behaviour cannot drift.

### [x] B4 — Graph mail offline mode (`M`)

In `source/crates/graph/src/mail/session.rs`:

- `replay_cache` still runs and emits folders, the open folder, flagged mail and the
  outbox before any network call.
- If offline: skip `initial_sync`, `sync_folders`, `sync_mailbox`, polling and
  `service_due`'s drain; keep the command loop alive and process the same cache-only
  outbox commands as B3.
- Add the watch branch to the main `select!`; going online sets `next_poll = Utc::now()`
  so the next iteration catches up.
- `ConnectionState::Offline` on entering, `Connecting`/`Idle` on leaving.

### [x] B5 — Composer status while offline (`S`)

- Add `AccountEvent::DraftSaveFailed { message_id, message }` and handle it in the Graph
  session too (or emit it only from the offline pump and document the asymmetry).
- `source/crates/app/src/compose.rs`: set the status label to "Not saved — working
  offline" and keep the text in the editor.
- Send still queues through B2; the outbox payload is independent of the Drafts copy.

---

## Part C — Calendar and webcal

### [x] C1 — CalDAV offline mode (`M`)

`source/crates/dav/src/session.rs`:

- Add `offline_rx` to `run_calendar_session`; the existing cached-month replay runs before
  the offline check and therefore still paints.
- Before `connect_and_run`, if offline: emit `ConnectionState::Offline` and wait on
  `offline_rx.changed()` / commands. On online, reset backoff and connect.
- Add a `ShutdownReason::Offline`; add the watch branch to the session's `select!`.
- Offline command answers:
  - `CreateEvent`/`UpdateEvent` → `EventSaveFailed` with "You're working offline — the
    event was not saved" (the app already rolls back optimistic drags on this event);
  - `DeleteEvent` → `Error` with the same wording;
  - `CreateTask`/`UpdateTask`/`DeleteTask` → `Error` "You're working offline — the task
    was not saved/deleted";
  - `CreateCalendar`/`UpdateCalendar`/`DeleteCalendar` → `Error`;
  - `FetchFreeBusy` → `FreeBusyFailed` "You're working offline";
  - `FetchEventAttachment` → `EventAttachmentFetchFailed`;
  - `Refresh`/`SyncMonth`/`FetchMonth`/`SyncTasks` → update the remembered month (where
    applicable) and ignore; the app also seeds cache (C3);
  - `SetCalendarColor`/`SetSharedCalendars` → keep local/no-op, matching the existing
    degrade-soft behaviour.
- Keep the offline wait alive across repeated commands (do not return on the first write
  failure).

### [x] C2 — Graph calendar offline mode (`M`)

`source/crates/graph/src/calendar/session.rs`:

- Add `offline_rx` to `run_calendar_session` and `connect_and_run`.
- If offline: emit `ConnectionState::Offline` and wait; do not list calendars or poll.
- Add the watch branch to the existing `select!`; treat the transition the same as an
  error-with-backoff except that there is no error and no backoff increment while offline.
- Answer writes with the same offline errors as C1; a missing Graph consent still follows
  the existing non-retryable path.

### [x] C3 — Show cached calendar months offline (`M`)

The Graph calendar session has no cache replay, and the CalDAV session only replays the
current month at startup. Add an app-side seed so both backends show the last synced month
and so navigating months offline still works:

- Add `seed_calendar_month_from_cache(state, month)` in `window/calendar_glue.rs`
  (or `window/connect.rs`) using `CalendarCacheSlot::open(...).load_month(month)` on the
  worker.
- Populate `CalendarAccountHandle.last_occurrences` when the loaded month is the
  displayed month, and always insert into `occurrences_by_month` for the dashboard.
- Feed the seed into the reminder engine so offline reminders for cached events still
  fire.
- Call it:
  - once per calendar account after discovery;
  - when the displayed month changes while offline;
  - when the toggle goes offline.
- Do not write the seed back to the cache (it came from there) and do not mark it as a
  live sync (`last_synced_month` only when the month genuinely matches the cached data).
- For Graph, this is the only path that paints a month offline; for CalDAV it is a
  harmless second source that covers month navigation.

### [x] C4 — Answer offline scheduling and rooms (`S`)

- The Scheduling Assistant's `FetchFreeBusy` fails as in C1; the tab already shows
  attendees as unknown, so add the "Working offline" prefix to that state.
- `Find a room` (Graph `places`) is an explicit user action: while offline, disable the
  action or toast "You're working offline".
- The event editor's **Teams meeting** toggle on a Graph calendar is a server-side action;
  while offline, keep the toggle but explain that the join link will be created on save
  after reconnecting (or disable Save entirely per A5).

### [x] C5 — Webcal subscriptions (`S`)

`source/crates/dav/src/subscription.rs`:

- Add `offline_rx` to `run_subscription_session`.
- Keep the cached-feed first paint.
- While offline: skip `sync_all`; on `SyncMonth`/`FetchMonth`, update `current_month`,
  build the cached feed list for that month and emit it (no network); on `Reload`, update
  the local subscription list and emit cached feeds for the current month.
- On `Refresh`, do nothing offline.
- On going online, run `sync_all` for the current month and continue the poll loop.
- `SubscriptionCommand::Shutdown` still exits.

---

## Part D — People

### [x] P1 — Pause the contacts poll loop (`M`)

`source/crates/app/src/contacts_view.rs`:

- Add `offline_rx` to `sync_contacts_account`.
- Keep the startup cached-snapshot replay.
- At the top of the loop, if offline:
  - select on `offline_rx.changed()` and `cmd_rx.recv()`;
  - for `Create`/`Update`/`Delete`, reply on the command's `reply` channel with
    "You're working offline — the contact was not saved/deleted";
  - for `Refresh`, ignore;
  - on going online, fall through to the normal resync (set `resync = true`).
- Never launch a CardDAV/Graph request while offline.

### [x] P2 — Offline display and editors (`S`)

- The People rail and list already render the cached `ContactsAccountSnapshot`, so no
  display change is needed once the loop stops polling.
- Guard the editors/import/merge as in A5; the directory (GAL/LDAP) lookups degrade to no
  hits, which is already the contract (§10).
- Profile-card "Files"/"Emails" tabs use local caches and keep working.

---

## Part E — Tasks

### [x] T1 — Replay CalDAV tasks at startup (`S`) — done with C1

- `source/crates/dav/src/session.rs`: after the cached-month replay, load
  `CalendarCache::load_tasks()` and emit `CalendarSessionEvent::TasksUpdated` before
  connecting. The cache is already written by `sync_tasks`.
- This gives the Tasks view CalDAV VTODOs offline with no new storage.

### [x] T2 — Google Tasks offline mode (`S`)

`source/crates/app/src/google_tasks.rs`:

- The cache fast-paint already runs before token acquisition; keep it.
- Add `offline_rx` to `run_google_tasks_session`.
- Before `oauth.try_access_token()`, if offline, enter the offline wait instead: no token
  refresh, no sync.
- Offline commands: `Refresh` ignored; every write (`CreateTask`, `UpdateTask`,
  `DeleteTask`, `CreateList`, `RenameList`, `DeleteList`) emits
  `GoogleTasksEvent::Error("You're working offline — …")`.
- On going online, acquire a token, sync, and resume the poll loop.

### [x] T3 — Graph To Do cache and offline mode (`M`)

`source/crates/graph/src/todo/`:

- Add a small per-account JSON cache (lists + tasks) using `lookout_core::fsutil`'s atomic
  writer, in the same style as Google Tasks' `TasksCache`. Store after each successful
  `sync`; load at session start and emit `ListsUpdated`/`TasksUpdated` before any network
  call.
- Add `offline_rx` to `run_todo_session`. If offline: emit the cache, skip `sync`, and
  wait; answer every write with `TodoEvent::Error("You're working offline — …")`.
- On going online, sync immediately.
- Without this cache, Microsoft To Do would be blank on an offline launch — the one module
  with no existing persistence.

### [x] T4 — Task editor and lists (`S`)

- Guard `task_editor.rs` Save/Delete and `task_lists.rs` create/rename/delete as in A5.
- The Tasks view's grouping/sorting and My Day pins are local and keep working.
- Completing a recurring task while offline is refused (it is a server write); the row
  stays unchanged rather than optimistically rolling forward.

---

## Part F — Outbox folder in the folder pane

### [x] F1 — Add the tree row (`S`)

- `source/crates/app/src/folder_tree.rs`:
  - add `TreeItem::Outbox(u32)`, documented as the synthetic cross-account sending queue;
  - add an `outbox_count: u32` parameter to `build_multi_account_tree_model`;
  - insert the row in the root store in this order:
    `Unified`, `Favorites` (when present), `Outbox`, accounts, `SearchFolders`.
- `source/crates/app/src/window.rs` row binder: icon `mail-send-symbolic` (themed
  fallback), label **Outbox**, heading style like the other synthetic rows, and the count
  badge. No drop target, no folder ⋯ menu, no Empty button, no spinner.

### [x] F2 — Keep the count fresh (`S`)

- Include the outbox count in `folder_tree_signature` under a reserved pseudo-account key
  (the same pattern used for Favorites and Search Folders).
- `refresh_outbox_ui` (or the `OutboxContents` handler) calls `folder_tree_refresh` only
  when the total changes.
- Count every row in every account's `UiState::outbox`, failed included.

### [x] F3 — Add the view (`M`)

- `source/crates/app/src/window/state.rs`: add `MailView::Outbox`.
- Folder selection (`window.rs`): on `TreeItem::Outbox`, exit search, clear
  `current_mailbox`/`current_account`, set `MailView::Outbox`, show the outbox page in the
  message-list stack, clear the reading pane to `empty`, and refresh the list header.
- `current_view_title` (`window/support.rs`): Outbox → ("Outbox", "All accounts").
- `refresh_list_header`: disable the favorite star and Focused/Other tabs in this view.
- `refresh_message_loading_state` (`window/mail_actions.rs`): early-return for
  `MailView::Outbox` so it cannot replace the outbox page with `list`/`loading`/`empty`.
- `restore_or_default_initial_view`: add an `Outbox` selection target and a
  `find_outbox_index` helper.

### [x] F4 — Build the Outbox page (`M`)

- Add a `gtk::Stack` page named `outbox` in `message_list_stack`.
- Factor the row builder out of `fill_outbox_dialog` in `window/undo.rs` into a shared
  helper used by both the dialog and the page. Reuse:
  - subject fallback `(No subject)`;
  - `To: …` plus status ("Waiting to send", "Scheduled for …", "Couldn't send");
  - **Send now** / **Retry** (`RescheduleOutbox { not_before: now }`);
  - **Edit** (`CancelOutbox`; the existing `OutboxCancelled` handler reopens the
    composer);
  - per-account grouping with the account address as the heading.
- **List every item**, including one whose "Sending: …" toast is still up.
  `listed_outbox_items` deliberately hides that one for the banner/dialog; add an
  `all_outbox_items` helper for the folder.
- Empty state: "There are no items in this folder." plus a dim line explaining that
  messages appear here until they go out.
- Repaint from the same `OutboxContents` handler as the banner and dialog. Keep the
  existing banner → "Scheduled messages" dialog; sharing the row builder keeps actions
  consistent.

### [x] F5 — Restore the view across search and restart (`S`)

- Add `UiState::pre_search_view: Option<MailView>` and set it in `start_search`; use it in
  `exit_search` so search started from Outbox returns to Outbox (this also removes the
  current single-vs-unified guess from `current_mailbox`).
- Add `LAST_VIEW_OUTBOX` (`last-view-outbox`) to `settings.rs` and the gschema, plus an
  `outbox: bool` field on `LastSelection` with `#[serde(default)]` for the one-time legacy
  `last-view.json` import. `save` writes the new key; `load` prefers `unified`, then
  `outbox`, then the mailbox.
- Save the selection when the Outbox row is chosen; restore it on first paint like the
  unified view.

### [x] F6 — Offline interaction (`S`)

- Any folder can still be selected while offline; cached mail paints from disk because
  A3's guard stops the sync request before it sets `syncing`.
- The Outbox row and page work with no connection, reading the cache and the
  `UiState::outbox` mirror.
- "Send now" while offline reschedules the row to now; the drain sends it only after the
  account reconnects. The status line remains "Waiting to send".

---

## Part G — Docs and close-out

### [x] G1 — CHANGELOG entry (`S`)

Follow the existing style and cite **M-40**. State that the mail cache format is unchanged,
list the new GSettings keys and the Graph To Do task cache (the one new file), and
describe the full-scope offline mode and the Outbox folder.

### [x] G2 — Update `OUTLOOK_PARITY.md` (`S`)

- Flip M-40 in §5.1 from 🟡 to ✅ and update its "Today" column.
- Update §2.1: Mail ✅ 38 → 39, 🟡 5 → 4; total ✅ 124 → 125, 🟡 14 → 13; recompute the
  percentages (~79.6% at parity, ~8.3% partial).
- Add a Phase 1 roadmap row (next free number, e.g. **1.25**) if the roadmap is being kept
  current.
- Leave the Graph column at ✅: the Graph mail transport gets the same behaviour through B4.

Do **not** bump the version, commit, or push as part of this work.

---

## Tests (write them; do not run them in this change)

### Shared signal

- A tiny unit test that a watch receiver's initial `true` value is seen by
  `offline_receiver`-style subscribe and that `send(false)` wakes a `changed()` waiter.
- A regression note in each actor's docs: keep the sender alive or `changed()` returns
  `Err` forever.

### Mail

- `lookout-mail` offline integration test (Docker-gated, `#[ignore]`): start with
  `watch::channel(true)`, send a message, assert `OutboxQueued`/`OutboxContents` arrive
  with no network, then `send(false)` and assert `OutboxSent` and the Sent copy. Cover
  `CancelOutbox` while offline.
- Graph fake-session equivalent: assert no HTTP request while offline, then the queued
  send after going online.
- Pure helpers for the extracted outbox queue/cancel/reschedule paths, if the cache can be
  opened in a test.
- `DraftSaveFailed` handling in the composer status label.

### Calendar, People, Tasks

- Actor-level tests where a mock server is already available:
  - DAV session: offline start makes no request; a cached month is emitted; an offline
    `CreateEvent` emits `EventSaveFailed`; going online syncs.
  - Graph calendar: same with the fake client.
  - Subscription session: offline `SyncMonth` re-emits cached feeds and makes no request.
  - Contacts loop: offline write replies with the offline message; no fetch runs.
  - Google Tasks: offline start does not request a token; cached lists/tasks paint.
  - Graph To Do: new cache round-trip; offline start emits the cache and makes no request.
- App-level tests for the guards (`request_mailbox_sync` returns `false`,
  `send_receive_all` does not send) using the existing `UiState` test fixture.

### Outbox folder

- `folder_tree`: the Outbox row appears in the right position and carries the summed count;
  the signature changes when only the count changes.
- `window/support`: `current_view_title` for `MailView::Outbox`.
- `window/undo`: shared row builder — subject fallback, Bcc stripped, status text, account
  grouping, and "the folder includes an item hidden by `listed_outbox_items`".
- `last_view`: Outbox round-trip, legacy file without the new field, precedence when more
  than one flag is set.
- `settings`: the `work-offline` default.

---

## Manual verification checklist

1. Turn **Work Offline** on from Send / Receive. The title-bar indicator and Mail banner
   appear; mail, calendar, contacts, tasks and webcal stop polling.
2. Mail: cached folders and messages still open. An uncached body shows the offline page,
   not a spinner. F9 / Send-Receive does not leave a spinner.
3. Compose and send while offline: the message lands in **Outbox** with "Waiting to send".
   **Undo** withdraws it and reopens the composer. **Send now** / **Edit** work from the
   page. Quit and relaunch offline; the item and the Outbox row survive.
4. Go online: mail reconnects, the queued item sends, the Sent copy appears, the Outbox
   count returns to zero, and the indicator/banner disappear.
5. Calendar: the last synced month and tasks are visible offline; navigating months shows
   cached months where present. Creating/editing/deleting an event keeps the editor open
   with "You're working offline — reconnect to save". Going online syncs.
6. Scheduling Assistant while offline shows attendees as unknown/offline; **Find a room**
   is disabled or toasts.
7. Webcal: cached feed events show; changing month offline re-emits cached feeds; going
   online fetches.
8. People: the cached contact list and details work; edit/delete/import/merge keep the
   dialog open with the offline message; going online syncs.
9. Tasks: CalDAV, Google Tasks and Graph To Do lists/tasks appear from cache on an offline
   launch; edits are refused with the offline message; completing a recurring task does
   not roll it forward.
10. New account/actor created while offline does not connect until **Go online**.
11. Toggle back and forth twice in a row with a send in the undo-send window: the send
    never escapes while offline, and never duplicates when online.
12. Repeat the mail checks with a Microsoft 365 account on the Graph mail transport.

---

## Risks and notes

- **One sender, many receivers.** The watch sender must live as long as the app (in
  `UiState`). A dropped sender makes every `changed()` return `Err`; treat that as "never
  fires" and never busy-loop.
- **Visibility assumption.** The Outbox row is always visible. If the user prefers
  hide-on-empty, only F1/F2 and the folder-tree test change.
- **No offline mutation queue.** Calendar/People/Tasks writes are refused rather than
  queued. That is a deliberate limit; the alternative is a persistent mutation log, which
  belongs in its own plan.
- **Graph To Do is the one module without an existing cache.** T3 adds it so full-scope
  offline mode does not leave Microsoft tasks blank. If that is delayed, document the gap
  in the CHANGELOG and the parity file.
- **CalDAV tasks currently never replay.** T1 is small and closes the same gap on the
  CalDAV side; without it, offline Tasks shows Google/Graph lists only.
- **Do not lose the undo-send window.** Commands and the offline check run before the
  outbox drain in both mail sessions; keep that ordering.
- **Folder page vs banner.** The Outbox page must list every queued item;
  `listed_outbox_items` hides one during its own "Sending" toast and is for the
  banner/dialog only.
- **Editor optimism.** Event/task/contact editors close on save today. A5 changes them to
  keep the dialog open only when offline; online behaviour is unchanged.
