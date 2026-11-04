# Work offline and the Outbox (M-40)

This plan implements the last Mail gap in `OUTLOOK_PARITY.md` §5.1 (**M-40**): a
user-controlled **Work Offline** mode and an **Outbox** folder in the folder pane.

Lookout already has a persistent local outbox, schedule send, undo send and cache-first
reads. What it lacks is a switch that puts the mail sessions into an offline state (so a
launch or a toggle stops every IMAP/Graph round trip) and a folder that lists the queue.
It also cannot currently queue a send while the account is disconnected: the session only
handles `SendMessage` inside a live connection, so the command waits for reconnect instead
of landing in the outbox.

**Status:** planned; nothing is implemented yet.

**Effort:** `M` for the recommended mail-only scope (roughly 3–5 days). `L` if Calendar,
People and Tasks are also paused by the toggle.

**Data-format change:** none. The outbox table, its schema version and the per-account
mail caches are untouched. The only persisted additions are GSettings keys.

---

## Decisions to confirm before starting

### D1 — Scope of the toggle

- **Recommended:** mail only (IMAP and the optional Graph mail transport). M-40 is a Mail
  row, and both transports share the same `AccountCommand`/`AccountEvent` seam.
- **Full:** also pause Calendar, Contacts and Tasks. That needs the same offline state in
  `CalendarCommand`, `ContactCommand`, `GoogleTasksCommand` and `Graph`'s `TodoCommand`,
  and doubles the session work. Treat it as a follow-up item (M-40b) unless confirmed.

### D2 — Shape of the Outbox row

- **Recommended:** one synthetic top-level **Outbox** row shared by every account, in the
  same style as **All Inboxes**. It keeps the tree model simple and matches the existing
  synthetic-row pattern. The list groups items by account.
- **Alternative:** one Outbox row inside each account group, closer to Outlook's per-account
  Outbox. It needs virtual account children, a second selection target and more restore
  logic.

Both use the same list page and actions; only the tree shape differs.

### D3 — Visibility

Outlook keeps Outbox in the folder list even when empty. **Recommended:** always visible,
with the count badge showing `0` as no badge. Hide-on-empty is possible but makes the
feature hard to discover on first run.

---

## Current state (verified in the repo)

- **Outbox storage.** `source/crates/mail/src/cache.rs` owns the per-account `outbox` table.
  `SendMessage` is handled in `source/crates/mail/src/session.rs`, converted to an
  `OutboxMessage` payload, queued by `Cache::enqueue_outbox`, and reported through
  `AccountEvent::OutboxQueued` / `OutboxContents`. The drain delivers due rows over SMTP
  (IMAP) or `sendMail` (Graph), then appends the Sent copy.
- **Scheduling and undo send.** `not_before` on the row backs both; `CancelOutbox` and
  `RescheduleOutbox` withdraw/reschedule a row. The banner and the "Scheduled messages"
  dialog live in `source/crates/app/src/window/undo.rs`
  (`listed_outbox_items`, `outbox_banner_text`, `refresh_outbox_ui`,
  `present_outbox_dialog`, `fill_outbox_dialog`).
- **Transports.** The IMAP session (`run_account_session`) and the Graph mail session
  (`lookout_graph::run_graph_mail_session`) both consume the same command/event contract.
  Both replay cached folders, messages, the outbox and flagged mail before connecting.
- **Offline reads.** Selecting a cached folder reads from the account cache; an uncached
  body shows the offline/error page; `Gio.NetworkMonitor` sends `AccountCommand::Reconnect`
  when connectivity returns.
- **Missing pieces.** No `work-offline` setting, no `SetOffline` command, no
  `ConnectionState::Offline`, no `TreeItem::Outbox`, and no offline command pump, so a send
  issued while disconnected is held instead of queued.

---

## In scope

- A persisted Work Offline preference, a Send / Receive ribbon toggle, and a visible
  "Working offline" banner with a **Go online** action.
- Stopping and restarting both mail transports without quitting Lookout.
- Queueing, cancelling, rescheduling and retrying outbox rows while offline.
- An Outbox row in the folder pane and a list page for queued, scheduled and failed mail.
- Startup while offline: cached folders and mail render, the outbox survives, nothing
  connects until the user goes online.
- Unit tests for the pure pieces and integration tests (written, not run here) for both
  transports.

## Out of scope

- A local draft store. Autosave to IMAP Drafts cannot work offline; the composer keeps its
  text in memory and reports that the draft was not saved. The outgoing message itself is
  still preserved in the outbox payload.
- Pausing Calendar, Contacts, Tasks, webcal subscriptions or Google Tasks (unless D1 says
  full scope).
- POP3, Exchange on-premises and any new OAuth scope.
- A new outbox payload format or cache migration.
- Outlook-style "work offline" per account. The toggle is app-wide.

---

## Part A — Work Offline (mail)

### [ ] A1 — Add the persisted preference (`S`)

- Add `WORK_OFFLINE` (`work-offline`) to `source/crates/app/src/settings.rs`, seeded
  `false` in the in-memory defaults used by tests.
- Add the `b` key to
  `source/data/gschema/io.github.gavindi.Lookout.gschema.xml`, default `false`, with a
  description that says mail stays cached but is neither sent nor received while it is on.
- Mirror the value into `UiState` so hot paths (`request_mailbox_sync`,
  `request_account_refresh`, `send_receive_all`) do not each hit GSettings. Load it once in
  `build_window`, near the other preference reads.

### [ ] A2 — Add the protocol seam (`S`)

- `source/crates/mail/src/session.rs`:
  - `AccountCommand::SetOffline { offline: bool }`, documented as "stop or resume all
    network traffic; queued outbox rows and local cache reads are unaffected".
  - `ConnectionState::Offline`, distinct from `Disconnected` (the app can render it).
  - Mention in the enum docs that `SetOffline(true)` is handled ahead of a due row, so a
    user who clicks offline during the undo-send window wins.
- The existing `AccountCommand::Reconnect` remains the "retry now" hint and is a no-op
  while offline.

### [ ] A3 — Pass the initial state at session start (`M`)

- Extend `run_account_session(..., start_offline: bool)` and
  `GraphMailSessionConfig` with `offline: bool`. An app launched offline must never make a
  first connection just to be told to stop.
- Update every constructor:
  - `source/crates/app/src/window/connect.rs` (`connect_account` and
    `connect_other_account`);
  - `source/crates/mail/tests/imap_integration.rs` (two sessions);
  - `source/crates/mail/examples/smoke_test.rs` and `send_test.rs`;
  - `source/crates/graph/tests/graph_mail_session.rs` and
    `graph_mail_integration.rs`;
  - `GraphMailSessionConfig::new` and any other struct literal.
- Default to `false` in tests/examples so their behaviour is unchanged.

### [ ] A4 — IMAP offline loop (`M`)

In `run_account_session` (the outer reconnect loop):

- On entry, after the cache replay and event emission, if offline:
  - emit `ConnectionStateChanged(ConnectionState::Offline)`;
  - do **not** create the IDLE supervisor;
  - wait on both command channels for `SetOffline { offline: false }`, `Shutdown`, and the
    cache-only outbox commands (A5);
  - reset the backoff to its initial value when going online.
- On `SetOffline { offline: true }` while connected:
  - log out (best-effort), abort the IDLE supervisor, emit `Offline`;
  - run the offline command pump until told to go online or shut down.
- A command that arrives offline must not accumulate forever in `carried_command`. Only
  the outbox commands are consumed offline; every network-only command is answered where
  it can be (see A5) and otherwise dropped with a debug log, because the reconnect sync
  supersedes it.

### [ ] A5 — Offline command pump (`M`)

Extract the cache-only bodies out of the three current handlers so the same code runs
connected and offline:

- `SendMessage { message, not_before, client_token }` → serialize
  `OutboxMessage::from_composed`, `enqueue_outbox`, emit `OutboxQueued` and
  `OutboxContents`, update the next-due time. No SMTP/Graph call.
- `CancelOutbox { id }` → `Cache::cancel_outbox`, emit `OutboxCancelled` (with the payload
  when it decodes) or `OutboxCancelFailed`, emit `OutboxContents`.
- `RescheduleOutbox { id, not_before }` → `Cache::reschedule_outbox`, update the next-due
  time, emit `OutboxContents`.

Network-only commands handled explicitly while offline:

- `FetchAttachment` → `AccountEvent::PartFetchFailed` with "You're working offline."
- `FetchRawMessage` → `AccountEvent::RawMessageFetchFailed` with the same wording.
- `FetchBody` → no event exists today; rely on the existing body-fetch timeout and the
  offline page. If the app already has a pending body request when `Offline` is emitted,
  show the offline error page immediately (see A7).
- `SaveDraft` → a friendly failure (A9) rather than silence.
- Moves, flags, snooze, folders, search: drop and log; the reconnect sync is
  authoritative.
- `SetPrefetchPolicy`, `ConfigureLocalFocus`, `Reconnect`: cache/memory updates, no
  network; safe to apply offline.

### [ ] A6 — Graph mail offline mode (`M`)

In `source/crates/graph/src/mail/session.rs`:

- Add `offline` state to `GraphMailSession`, set from the config and `SetOffline`.
- Skip `initial_sync`, `sync_folders`, `sync_mailbox`, snooze wake-ups and the outbox drain
  while offline. `replay_cache` still runs, so folders, the open folder, the outbox and
  flagged mail paint at once.
- Keep the command loop alive: `handle_command` still processes sends, cancels,
  reschedules and `SetOffline`.
- Going online sets `next_poll = Utc::now()` and lets the normal poll/sync path catch up.
- `ConnectionState::Offline` on entering the mode; `Connecting`/`Idle` on leaving it.

### [ ] A7 — App wiring and guards (`M`)

- Add `UiState::work_offline: bool` (mirrored from A1) and
  `UiState::offline_banner: Option<adw::Banner>`.
- Add `set_work_offline(state, offline)`:
  - write the setting;
  - send `AccountCommand::SetOffline { offline }` on every `AccountHandle.cmd_tx` and
    every `interactive_cmd_tx` (interactive is to catch a queued fetch first);
  - reveal/hide the banner (A8);
  - show a toast ("Working offline" / "You're back online").
- Guards:
  - `request_mailbox_sync` returns `false` without marking the mailbox `syncing` while
    offline. This prevents a spinner that can never clear.
  - `request_account_refresh` returns without sending.
  - `send_receive_all` / `resync_current_view` toast "You're working offline" and return.
  - `Gio.NetworkMonitor`'s `network-changed` handler skips `Reconnect` while offline.
  - Any `AccountCommand::Reconnect` sent from elsewhere is harmless (the session ignores
    it offline), but the guard avoids pointless channel traffic.
- On startup, if offline, mark every discovered account as connecting-but-offline; cached
  folders come from the session's replay and are enough to populate the tree.
- When the toggle flips online, clear any stale `refreshing`/`syncing` markers left from a
  previous attempt so the next sync can start.
- If a body was pending when offline was entered, show
  `show_body_fetch_error` / the offline page instead of leaving the spinner up.

### [ ] A8 — Ribbon and banner (`S`)

- `source/crates/app/src/window.rs`: on the **Send / Receive** tab, add a **Preferences**
  group with a toggle via `RibbonGroup::toggle` — icon
  `network-offline-symbolic` when on, `network-transmit-receive-symbolic` when off,
  label **Work Offline**. Keep its active state in sync with the setting and with any
  changes from elsewhere.
- Add an `adw::Banner` to the message box, above the existing outbox and automatic-replies
  banners:
  - text "Working offline — messages won't be sent or received." (markup-escaped);
  - button **Go online**, which calls `set_work_offline(state, false)`;
  - revealed only while offline.
- While a folder is selected offline, its rows still render from cache. The banner is the
  one persistent visual cue; no status-bar widget is needed.
- Keep the Send / Receive All button enabled but guarded (toast), matching Outlook's
  "softer" refusal rather than a disabled control.

### [ ] A9 — Composer status while offline (`S`)

`SaveDraft` cannot reach IMAP Drafts offline. Avoid a composer stuck on "Saving draft…":

- Add `AccountEvent::DraftSaveFailed { message_id, message }` (handled in the Graph session
  too, or emitted only by the offline pump and documented as IMAP-only if that is
  simpler).
- `source/crates/app/src/compose.rs` sets the status label to "Not saved — working
  offline" (or "Draft not saved") and leaves the text in the editor.
- Send still queues through A5; the outbox payload is independent of the draft copy.
- On reconnect the next autosave or an explicit Save Draft stores the draft normally.

---

## Part B — Outbox folder in the folder pane

### [ ] B1 — Add the tree row (`S`)

- `source/crates/app/src/folder_tree.rs`:
  - add `TreeItem::Outbox(u32)`, documented as the synthetic cross-account sending queue;
  - add an `outbox_count: u32` parameter to `build_multi_account_tree_model` (or a small
    `OutboxSummary` struct if more fields follow);
  - insert the row in the root store. Recommended order:
    `Unified`, `Favorites` (when present), `Outbox`, accounts, `SearchFolders`.
- `source/crates/app/src/window.rs` row binder: icon `mail-send-symbolic` (with a themed
  fallback), label **Outbox**, heading style like the other synthetic rows, and the count
  badge. No drop target, no folder ⋯ menu, no Empty button, no spinner.
- A zero count hides the badge, exactly as other folder rows do.

### [ ] B2 — Keep the count fresh (`S`)

- Include the outbox count in `folder_tree_signature` under a reserved pseudo-account key
  (the same pattern used for Favorites and Search Folders) so a change repaints the row.
- `refresh_outbox_ui` (or the `OutboxContents` handler) calls `folder_tree_refresh` when
  the total changes. Avoid rebuilding on every event where the count is unchanged.
- The count is every row in every account's `UiState::outbox`, failed included.

### [ ] B3 — Add the view (`M`)

- `source/crates/app/src/window/state.rs`: add `MailView::Outbox`.
- Folder selection (`window.rs`): on `TreeItem::Outbox`, exit search, clear
  `current_mailbox`/`current_account`, set `MailView::Outbox`, set the list stack to the
  outbox page, clear the reading pane to `empty`, refresh the list header.
- `current_view_title` (`window/support.rs`): Outbox → ("Outbox", "All accounts").
- `refresh_list_header`: disable the favorite star and the Focused/Other tabs in this
  view (no single mailbox).
- `refresh_message_loading_state` (`window/mail_actions.rs`): early-return for
  `MailView::Outbox`, so it cannot replace the outbox page with `list`/`loading`/`empty`.
- Folder-tree restore (`restore_or_default_initial_view`): add an `Outbox` selection
  target and a `find_outbox_index` helper.
- Optional selection polish: while in Outbox, the message-list selection is empty, so
  bulk actions and keyboard mail commands are naturally no-ops. Do not repopulate the
  message list model with outbox rows; keep the views separate.

### [ ] B4 — Build the Outbox page (`M`)

- Add a `gtk::Stack` page named `outbox` in `message_list_stack` in `window.rs`.
- Factor the row builder out of `fill_outbox_dialog` in `window/undo.rs` into a shared
  helper (e.g. `fill_outbox_rows(state, container, items, mode)`), used by both the
  dialog and the page. Reuse:
  - subject fallback `(No subject)`;
  - `To: …` plus the status line ("Waiting to send", "Scheduled for …",
    "Couldn't send");
  - **Send now** / **Retry** (`RescheduleOutbox { not_before: now }`);
  - **Edit** (`CancelOutbox`; the existing `OutboxCancelled` handler reopens the
    composer);
  - per-account grouping with the account address as the group heading.
- **Important:** the folder page lists *all* `UiState::outbox` items, including one whose
  "Sending: …" toast is still up. `listed_outbox_items` deliberately hides that one for
  the banner/dialog; do not reuse it here. Add an `all_outbox_items` helper if that keeps
  the difference obvious.
- Empty state: "There are no items in this folder." (Outlook's wording) and a dim line
  explaining that messages you send appear here until they go out.
- The page repaints from the same `OutboxContents` handler as the banner and dialog.
- Keep the existing banner → "Scheduled messages" dialog. The page and the dialog share
  the row builder, so their actions cannot drift.

### [ ] B5 — Restore the view across search and restart (`S`)

- Add `UiState::pre_search_view: Option<MailView>` and set it in `start_search`; use it in
  `exit_search` so search started from Outbox returns to Outbox (this also removes the
  current single-vs-unified guess from `current_mailbox`).
- Add `LAST_VIEW_OUTBOX` (`last-view-outbox`) to `settings.rs` and the gschema, plus an
  `outbox: bool` field on `LastSelection` with `#[serde(default)]` for the one-time
  legacy `last-view.json` import. `save` writes the new key; `load` prefers `unified`,
  then `outbox`, then the mailbox. Update the round-trip test.
- Save the selection when the Outbox row is chosen; restore it on first paint like the
  unified view.

### [ ] B6 — Offline interaction (`S`)

- Selecting any folder while offline must not start a sync (A7), so no spinner sticks.
- Cached folders still paint from the account cache.
- The Outbox row and page work with no connection at all, since they read the cache and
  the `UiState::outbox` mirror.
- "Send now" while offline reschedules the row to now; the drain sends it only after the
  account is back online. The row's status line remains "Waiting to send".

---

## Part C — Docs and close-out

### [ ] C1 — CHANGELOG entry (`S`)

Follow the existing style and cite **M-40**. State that there is no data-format change
and list the new GSettings keys. Describe the offline mode, the queue-while-offline fix,
and the Outbox folder.

### [ ] C2 — Update `OUTLOOK_PARITY.md` (`S`)

- Flip M-40 in §5.1 from 🟡 to ✅ and update its "Today" column.
- Update §2.1: Mail ✅ 38 → 39, 🟡 5 → 4; total ✅ 124 → 125, 🟡 14 → 13; recompute the
  percentages (~79.6% at parity, ~8.3% partial).
- Add a Phase 1 roadmap row (next free number, e.g. **1.25**) if the roadmap is being kept
  current, or note M-40 as closed in the phase table.
- Leave the Graph column at ✅; the Graph transport gets the same behaviour through A6.

Do **not** bump the version, commit, or push as part of this work.

---

## Tests (write them; do not run them in this change)

### Unit tests

- `folder_tree`:
  - the Outbox row appears in the recommended position and carries the summed count across
    several accounts;
  - the signature changes when only the outbox count changes;
  - existing model tests updated for the new parameter.
- `window/support`: `current_view_title` for `MailView::Outbox`.
- `window/undo` / the shared row builder: subject fallback, Bcc stripped from the
  recipient line, failed/scheduled/waiting status text, account grouping, and "the folder
  page includes an item hidden by `listed_outbox_items`".
- `last_view`: Outbox round-trip, legacy file without the new field, and precedence when
  more than one flag is set.
- `settings`: the `work-offline` memory default and toggle round-trip.
- Mail session: `SetOffline` transitions and the offline pump's queue/cancel/reschedule
  behaviour, if a pure seam can be factored out; otherwise cover it in the integration
  tests below.

### Integration tests

- **IMAP** (`source/crates/mail/tests/imap_integration.rs`, Docker-gated and `#[ignore]`):
  start a session with `start_offline = true`; `SendMessage` yields `OutboxQueued` and
  `OutboxContents` with no network; `SetOffline(false)` eventually yields `OutboxSent` and
  the Sent copy. Also cover `CancelOutbox` while offline.
- **Graph** (the fake-Graph session test): the same sequence with the Graph mail session,
  asserting no HTTP request is made while offline.
- **App-level** (if the existing fixture harness allows): `send_receive_all` and
  `request_mailbox_sync` do not touch a session while offline, and `NetworkMonitor`
  recovery does not reconnect.

---

## Manual verification checklist

1. Turn **Work Offline** on from Send / Receive. The banner appears, folder syncing stops,
   cached mail still opens, and Nudge/Send-Receive does not leave a spinner.
2. Compose and send an ordinary message while offline: the "Sending" toast appears, the
   message lands in **Outbox**, and the status reads "Waiting to send". **Undo** withdraws
   it and reopens the composer.
3. From the Outbox page, try **Send now** (still offline), **Edit**, and **Retry** on a
   failed row. Confirm the dialog's actions still behave the same.
4. Quit and relaunch while still offline: the Outbox row, its count, the queued item and
   the last-open Outbox view all survive.
5. Turn the toggle off: accounts reconnect, the item is delivered ("Message sent"), the
   Sent copy exists, the Outbox count returns to zero, and the banner disappears.
6. Create a scheduled send while offline; confirm it does not fire until the account is
   back online and the scheduled time has passed.
7. Repeat with a Microsoft 365 account using the Graph mail transport.
8. Open an uncached message while offline: the "Message unavailable"/offline page appears
   rather than a permanent spinner.
9. Start Lookout offline with several accounts configured: cached folders and mail render,
   no account attempts to connect, and the banner is visible.
10. While offline, type in a composer for longer than the autosave interval: the status
    reads "Not saved — working offline" rather than staying on "Saving draft…".
11. Switch to another module (Calendar/People/Tasks) and back: if D1 is mail-only, confirm
    the banner's wording is honest about mail being offline; if full scope is chosen,
    confirm those modules also stop polling.

---

## Risks and notes

- **Global state vs per-account sessions.** The toggle is app-wide, but sessions are
  per-account. Broadcasting one command per account is simple and keeps each session's
  reconnect logic local. New accounts connected while offline must receive the initial
  state (A3), not just the broadcast.
- **Do not lose the undo-send window.** A `SetOffline(true)` that arrives during the
  undo-send delay must prevent the drain. The command drain runs before the drain in both
  sessions today; keep that ordering when adding the offline check.
- **The banner and the folder count are separate cues.** The count comes from
  `UiState::outbox`; the banner also covers automatic replies and failures. Keep the
  outbox banner's text unchanged so existing tests stay valid.
- **`listed_outbox_items` is not the folder's item source.** It hides a message during its
  own "Sending" toast. The folder must show everything; that is the one behavioural
  difference between the two surfaces.
- **No new cache table.** If a future local-draft store is added, it belongs in a separate
  plan; do not smuggle it into M-40.
- **Mail-only scope must be visible.** If Calendar/Contacts/Tasks keep syncing, the banner
  should name mail ("Working offline — mail won't be sent or received") rather than
  implying the whole app is disconnected.
