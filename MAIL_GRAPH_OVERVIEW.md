# MAIL_GRAPH_OVERVIEW — Phase 5, item 5.7 (Optional Graph mail transport)

**Status:** ✅ implemented and closed out as an optional, opt-in transport. 5.7.1 is partial (offline evidence only); 5.7.2–5.7.10 landed. IMAP remains the default and the recommended transport. The remaining live-tenant validation of MIME fidelity and real delta cost is recorded as R7, not as unfinished code.
**Covers:** [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) §4.4, §5.1, §5.6, §7 item 5.7; ID **A-06** (Microsoft 365 mail).
**Companion docs:** [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) (parity plan), [SCAFFOLD.md](SCAFFOLD.md) (architecture), [CHANGELOG.md](CHANGELOG.md) (history).

> OUTLOOK_PARITY.md §7 item 5.7 points here. This file records the assessment of
> that item and, if the deferral is ever lifted, the task breakdown to implement it.

---

## 1. What item 5.7 is

> **5.7 Optional Graph mail transport** (A-06) — *"Only if 5.4 proves too
> costly: full Graph mail sync as an alternative to IMAP for Microsoft 365
> accounts."* Effort: **L**.

By default a Microsoft 365 account is **mail over IMAP/SMTP** with Lookout's own
XOAUTH2 (`source/crates/app/src/microsoft_oauth.rs`), and Graph is layered on top
for the things Exchange IMAP cannot carry or expose. Item 5.7.6 adds an
experimental per-account opt-in that replaces the IMAP/SMTP half with the Graph
session; IMAP remains the default and the recommended transport.

| Feature | How it works today | Where |
|---|---|---|
| Categories (M-09) | local `local_categories` overlay + Graph `categories` PATCH | item 5.4 |
| Follow-up flag (M-10) | `$Lookout-flag-*` keywords, or the `follow_up_flags` cache table on Exchange, mirrored to Graph `flag` | items 1.16, 5.4 |
| Importance (M-12) | `local_importance` overlay + Graph `importance` | item 5.4 |
| Focused / Other (M-03) | `local_inference` + `inference_overrides` + Graph `inferenceClassification` | items 2.12, 5.4 |
| Rules (M-14) | client / Sieve / Graph `messageRules` | item 2.5 |
| Automatic replies (M-24) | Graph `automaticRepliesSetting` / Sieve `vacation` | item 2.11 |
| Schedule send (C-08) | Graph `sendMail` + `PidTagDeferredSendTime`, local outbox fallback | items 1.14, 5.5 |
| Shared mailboxes (M-42) | add-by-address over IMAP (login form unverified) + Graph folder/send-as transport ready | item 5.6 |
| Calendar / contacts / To Do | native Graph actors (calendar, contacts, todo) | items 5.1–5.3 |

**5.7 is a conditional fallback.** The condition ("only if 5.4 proves too
costly") has **not fired**: 5.4 landed across five sprints (transport → cache
overlays → session seam → app wiring → Focused/Importance UI) and A-06 is
marked ✅. The layered IMAP + Graph design is working.

## 2. Recommendation

**Keep IMAP as the default.** Graph mail is available only behind the
per-account, experimental switch from item 5.7.6; do not make it the transport
of record for Microsoft 365.

Reasons the default stays IMAP:

1. The trigger in the item's own text has not fired — 5.4 was not too costly.
2. A Graph transport is **strictly worse for real-time delivery**: Graph change
   notifications require a **publicly accessible HTTPS `notificationUrl`**, which
   a desktop app cannot host (Microsoft's `/subscriptions` docs). So Graph mail is
   **delta polling only**, a latency regression against today's dedicated IDLE
   connection (`run_idle_supervisor`).
3. Graph's message query surface is far weaker than IMAP + local FTS5:
   a per-folder delta supports only `$filter=receivedDateTime ge|gt …`,
   `$orderby=receivedDateTime desc`, and **no `$search`**.
4. The identity model (`Uid`/`UidValidity`) is IMAP-shaped and threaded through
   every command; replacing it is the largest single source of churn for
   uncertain benefit.

**Revisit if a concrete trigger appears**, e.g.:

- The Exchange Online shared-mailbox IMAP login (M-42) proves unusable in
  practice; or
- Microsoft further restricts IMAP on Exchange Online (keywords, `MOVE`, IDLE);
  or
- Real-time sync is judged unnecessary and a polling transport is acceptable.

### 2.1 Spike result so far (item 5.7.1, offline only)

5.7.1 was run **offline** — no live Microsoft 365 tenant was available — so its
two evidence questions remain open:

- **(a) MIME fidelity** and **(b) delta paging cost** could **not** be measured
  against Graph. The spike landed the harness to measure them (§4, 5.7.1) and
  its wiremock proof, but the go/no-go **cannot** be settled from fixtures alone:
  the question is whether Graph's `/$value` bytes match the IMAP body, and only
  a real mailbox can answer it.
- **(c) Trigger review** — answered from Microsoft's published guidance rather
  than a live test, and it **confirms the trigger has not fired**: Microsoft is
  retiring **Basic authentication** for IMAP/POP/SMTP (final SMTP AUTH deadline
  **30 April 2026**), **not** the IMAP protocol; OAuth `XOAUTH2` — which Lookout
  already uses — stays the supported path, and no Exchange Online restriction of
  IMAP keywords, `MOVE` or IDLE is announced. The one unverified trigger
  candidate is unchanged: the shared-mailbox IMAP login form (M-42).

**Net: IMAP stays the default, and 5.7.1 is complete only as far as a
tenant-less spike can take it.** Item 5.7.6 exposes the Graph session behind an
opt-in so the transport can be tried on a real mailbox; 5.7.9 adds the
read-only live-tenant harness that can re-run (a) and (b), and 5.7.10 closes
out the documentation. Running that harness against a tenant is what would move
the default; until then the opt-in transport is complete, but its live MIME
fidelity and real delta cost remain R7.

## 3. Architecture (as wired by 5.7.6)

Mirror the existing Graph/DAV actors: a **second account-session actor** speaking
the **same `AccountCommand` / `AccountEvent` contract** as
`lookout_mail::session::run_account_session`, selected **per account** at connect
time, so the app's event loop, message list, reading pane, composer and outbox
are untouched.

```
source/crates/app/src/window/connect.rs
  └─ if account is Microsoft 365 AND graph mail opted in
        worker.spawn(lookout_graph::run_graph_mail_session(mail_config, client, cmd_rx, interactive_cmd_rx, evt_tx))
     else
        worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, interactive_cmd_rx, evt_tx))
```

Key properties:

- One protocol seam (`AccountCommand`/`AccountEvent`), reused verbatim — §10
  "Protocol seam first".
- Graph is the transport of record for bodies **and** properties; the 5.4
  overlay tables (`local_categories`, `local_importance`, `local_inference`)
  become redundant on this transport.
- The local outbox / scheduler (item 0.3) still owns undo send, schedule send
  and the offline outbox; Graph `sendMail` + `PidTagDeferredSendTime` remains the
  server-side path (item 5.5).
- Native writes replace the overlay+mirror dance: `isRead`, `flag`,
  `categories`, `importance`, `inferenceClassification`, `move`, `copy`,
  `delete` are all direct Graph PATCH/POST.

### 3.1 Transport reconciliation (item 5.7.7)

Walk of `OUTLOOK_PARITY.md` §5.1 on the Graph transport. ✅ = native,
🟡 = degraded but working, ⛔ = unavailable and hidden, — = not implemented on
any transport (so the transport cannot regress it). The same marks are in the
**Graph** column of §5.1.

| ID | Feature | Graph mail | Note |
|---|---|---|---|
| M-01 | Multi-account, IDLE push, offline cache | 🟡 | Polling every `poll_interval` (45 s default), not IDLE; the cache and multi-account handling are native (R1). |
| M-02 | Unified inbox | ✅ | Client-side merge over the shared cache. |
| M-03 | Focused / Other inbox | ✅ | Native `inferenceClassification` reads and writes, including sender overrides. |
| M-04 | Conversation view | ✅ | `graph:{conversationId}` thread key; same Sent-Items-only reach as IMAP. |
| M-05 | Reading pane right / bottom / off | ✅ | Client-side layout. |
| M-06 | Density and preview lines | ✅ | Client-side presentation. |
| M-07 | Sort / arrange by | ✅ | Local sort and grouping. |
| M-08 | Filter menu | ✅ | Local filters, including Pinned (empty while Pin is unavailable). |
| M-09 | Categories | ✅ | Native `categories` / `masterCategories`; tag keys map through the master list. |
| M-10 | Flag for follow-up with dates | ✅ | Native Graph `flag`, with the cache overlay keeping the list in step until the next delta. |
| M-11 | Snooze | 🟡 | Local cache snooze only; unlike the IMAP keyword + Scheduled folder, it isn't shared with Outlook or another install. |
| M-12 | Importance display | ✅ | Native Graph `importance`. |
| M-13 | Pin | ⛔ | Graph has no Pin counterpart. The ribbon button is disabled, the row hover action is disabled, and the context-menu entry is omitted (5.7.7). |
| M-14 | Rules | 🟡 | Client and Graph `messageRules` backends work; the Sieve backend is hidden because the server never advertises ManageSieve. |
| M-15 | Quick Steps | ✅ | Local chains, unchanged. |
| M-16 | Sweep | ✅ | Reads the local cache and issues Graph moves. |
| M-17 | Junk: safe / blocked senders | 🟡 | Moves, blocked lists and client enforcement work; `$Junk`/`$NotJunk` are local cache hints because Graph has no keyword. |
| M-18 | Search syntax and refiners | 🟡 | Local FTS5 is the primary search; Graph `$search` is best-effort and cannot mirror every `search_query.rs` operator. |
| M-19 | Search Folders | ✅ | Local saved searches over the FTS index. |
| M-20 | Ignore / Clean up conversation | ✅ | Local thread-key store; the Graph thread key is stable. |
| M-21 | Undo | 🟡 | Every available action keeps its undo; Pin has none because Pin itself is unavailable. |
| M-22 | Mark-as-read options | ✅ | Client-side policy over native `isRead`. |
| M-23 | Read receipts | ✅ | MIME `$value` preserves the request; the RFC 8098 reply uses the normal send path. |
| M-24 | Automatic replies | ✅ | Native `automaticRepliesSetting`. |
| M-25 | @mentions | ✅ | MIME/text body parsing feeds the same detector. |
| M-26 | Message recall | — | Absent on both transports. |
| M-27 | Delivery receipts (DSN) | — | Absent on both transports. |
| M-28 | Print | ✅ | WebKit print of the MIME/JSON body. |
| M-29 | Save as `.eml` / `.msg` / PDF | ✅ | `.eml` comes from the raw MIME cache; the `.msg`/PDF gap is unchanged. |
| M-30 | View source / message headers | ✅ | `FetchRawMessage` serves the MIME source. |
| M-31 | Open message in new window | ✅ | Raw-MIME render. |
| M-32 | Attachment preview | ✅ | Same inline `cid:` support; the in-app preview gap is unchanged. |
| M-33 | Download / save all attachments, zip | ✅ | Graph attachment bytes, cached like IMAP parts. |
| M-34 | Remote content / trusted senders | ✅ | Client-side policy. |
| M-35 | Phishing / unverified-sender banners | — | Not built yet; the MIME read now supplies the headers it needs. |
| M-36 | Translate | — | Absent on both transports. |
| M-37 | Immersive Reader / zoom | ✅ | Client-side zoom and theme. |
| M-38 | Message context menu | 🟡 | The full menu works; Pin is omitted (M-13). |
| M-39 | Notifications | ✅ | Poll-driven; new-mail latency follows M-01. |
| M-40 | Work offline / outbox | ✅ | Cache-first reads and the local outbox are transport-neutral. |
| M-41 | List-unsubscribe | ✅ | Headers come from the MIME read; both actions use the existing paths. |
| M-42 | Shared mailboxes / delegate access | 🟡 | Shared mailboxes still connect over IMAP under the parent's token; the Graph shared-mailbox routes remain ready but unsurfaced. |
| M-43 | Archive folder | ✅ | Graph folder-role move. |
| M-44 | Drag and drop | ✅ | Graph move/copy; drag-out `.eml` uses the raw cache. |
| M-45 | TNEF / winmail.dat | ✅ | MIME `$value` feeds the existing decoder; live-tenant fidelity is still R3's open question. |
| M-46 | Voting buttons | — | Absent on both transports. |

**§6.11 UX check:** the transport adds no new dialogs or errors. Placement
follows Outlook except where the capability is absent (Pin is disabled/omitted,
Move and Sieve were already hidden in 5.7.6); wording uses “Graph mail” in the
disabled tooltip and the Config row; defaults are unchanged; the Pin shortcut
remains bound but the disabled button makes it a no-op; all available actions
keep their Undo; and no empty/error state changes, because a missing capability
is hidden rather than surfaced as a failure. No screenshot changes are needed.

### 3.2 Refresh cadence and cursors (item 5.7.8)

Graph has no push (R1), so the session polls. The cadence is deliberately
small and documented rather than configurable: a shorter interval multiplies
Graph round trips and throttling; a longer one widens the latency regression
against IMAP IDLE.

- The **open folder and the Inbox** are polled every `poll_interval`
  (`DEFAULT_POLL_INTERVAL`, 45 s). One further folder joins each tick in
  rotation, so with `N` folders that folder is polled every `N` ticks
  (`N × 45 s`).
- The **folder hierarchy** delta runs every fourth poll (about three minutes),
  refreshing counts and new/renamed/deleted folders.
- **Worst-case latency**: up to 45 s for the open folder or Inbox, and up to
  `N × 45 s` for a folder the user is not looking at. This is the accepted
  no-IDLE trade-off; IMAP stays the default for accounts that need push.
- **Cursor persistence**: each folder's `@odata.deltaLink` is stored in the
  cache's `graph_folder_cursors` table (`Cache::graph_folder_cursors`,
  `set_graph_folder_cursor`, `clear_graph_folder_cursor`). The session loads
  them at startup, so a restart reads only changes instead of a full folder
  snapshot. A cursor advances only after its page has been stored, so a failed
  cache write cannot skip changes. An expired cursor (HTTP 410) is cleared both
  in memory and on disk, and that folder resyncs as a snapshot under the same
  Uids (5.7.3); a deleted folder's cursor is cleared with the folder, and an
  envelope-format wipe drops every cursor because the rows they described are
  gone.
- **Offline/outbox**: none of this affects the local outbox. Undo send,
  schedule send and the offline queue still live in the cache's `outbox`; due
  rows wake the session independently of the poll clock and are sent through
  Graph's MIME `sendMail`, exactly as before (5.7.5 / item 5.5).

## 4. Task breakdown

Effort: **S** ≤ 1 day · **M** ≤ 1 week · **L** multi-week.

| # | Task | Scope | Effort |
|---|---|---|---|
| 5.7.1 | **Spike + go/no-go** — 🟡 partial (offline only) | **Landed:** the harness to measure the two evidence questions — `MailClient::get_mime` (`GET /me/messages/{id}/$value`, `Prefer: IdType="ImmutableId"`), `MailClient::folder_delta` (`GET /me/mailFolders/{id}/messages/delta`) and `GraphClient::get_bytes_with_headers`, with wiremock tests and the `test-fixtures/outlook/messages-delta.json` page fixture. Question (c) is answered from Microsoft's published guidance (§2.1): Basic auth is going, IMAP is not. **Outstanding (needs a live tenant):** (a) `/$value` MIME fidelity for an iMIP invite, a TNEF message and an `Authentication-Results` message — diffed against the IMAP body; (b) delta paging cost on a large mailbox and folder. Capture those three `.eml` fixtures when a tenant is available. | S |
| 5.7.2 | **Graph mail transport** — ✅ landed (wired by 5.7.6) | **Landed** in `source/crates/graph/src/mail/`; item 5.7.6 selects it for an opted-in account. **Operations** (`MailClient`): `own_mail_folders_delta` (the whole hierarchy through `/me/mailFolders/delta`, since the plain list is top-level only), `well_known_folders`, `folder_messages_delta` at row weight, `get_message` (HTML body + headers), `move_message`/`copy_message`/`delete_message`, `update_message` (one `MessagePatch` for isRead / flag / categories / importance / inferenceClassification), `list_attachments` + `get_attachment_bytes`, `create_draft`/`update_draft`/`create_reply`/`create_reply_all`/`create_forward`/`send_draft`, `create_folder`/`rename_folder`/`delete_folder`, and `search_messages` (`$search`, unordered, capped). `GraphError::is_delta_reset` flags an expired cursor (410). **Pure mappers** (`mail/message.rs`): `MailFolder` (the 5.6 `SharedMailFolder` is now an alias of it) → `Mailbox`, `GraphMessage` → `EmailSummary`, attachments → `BodyPart`, the JSON body → `EmailBody`, with the `MailboxId`/`Uid` passed in so 5.7.3 owns the identity model. **Inputs for later tasks:** a part's `part_number` is the Graph attachment id, made filename-safe by 5.7.3; the thread key is `graph:{conversationId}` because Graph summaries carry no `References` (5.7.4 must keep it); `size` is 0 and Pin has no Graph counterpart (5.7.7); `calendar_ics` needs the MIME read (R3). Covered by wiremock tests per operation and four synthetic fixtures (`mail-folders-delta.json`, `message-summary-delta.json`, `message-full.json`, `message-attachments.json`). | M |
| 5.7.3 | **Identity model** — ✅ landed (used by 5.7.5) | **Landed.** `MailboxId = "{account}:{graph folder id}"`, parsed back only by stripping the account prefix (`lookout_graph::graph_folder_id`), never by splitting on `:`. A **synthetic `Uid`** per Graph immutable id comes from the cache's new `graph_uids` map (`Cache::graph_uids`, one `IMMEDIATE` transaction, reverse lookup `graph_ids_for`, pruned by `run_maintenance`). Uids are unique per **account**, so a moved message keeps its Uid and a copy gets a new one; `allocation_order` hands new ones out oldest-first. **`UidValidity` decision (changed from the original "bump on a delta reset"):** one random nonzero value per account (`graph_uid_space`), changed only when the map is lost (cache deleted or cleared). An expired delta cursor (410) resyncs the folder under the **same** Uids, because bumping would throw away cached bodies, attachments, search rows and undo references for messages that haven't changed. Random rather than time-based so a map recreated within the same second can't reuse a validity. **Fix to 5.7.2:** a part's `part_number` is now the attachment id in base64url form (`graph_part_number`), since a raw Graph id's `/` would have pointed the attachment flat file outside its directory. Documented in SCAFFOLD §3.1/§4.5. | M |
| 5.7.4 | **Cache strategy** — ✅ decided and proven (used by 5.7.5) | **Decision: reuse the per-account `lookout_mail::Cache`**, no separate Graph cache file and no `user_version` migration. A Graph account keeps the same tables as an IMAP one, so FTS5 search, conversation lookups, snoozes, the outbox, the overlays and the flat files all work unchanged. **Added:** `Cache::upsert_messages`, the incremental counterpart of `replace_messages` for a delta that reports only changes (removals go through the existing `delete_messages`); it promotes `partial` rows and keeps full-body search text. **Rules for the session (5.7.5):** (1) store each folder's delta with `upsert_messages` under the 5.7.3 Uids and validity, and its tombstones with `delete_messages`; a move is a removal in one folder plus an upsert of the same Uid in the other. (2) Write each record's categories, importance, Focused/Other and flag through the existing `apply_remote_mail_properties` (categories mapped to Lookout keys, as the 5.4 sync does, `delta_cursor` = `None`), so the overlays always match Graph and a stale overlay from IMAP days is cleared; local optimistic writes keep their `updated_at` guard. A message without a Message-ID (some drafts) can't carry overlay properties. (3) The `graph:{conversationId}` thread key is stored as given; only the IMAP session calls `compute_thread_keys`. (4) Graph folder ids aren't paths: forget a deleted folder by its own id, not `forget_mailbox_tree` (whose subtree walk is delimiter-based), and a rename keeps the id, so no `rekey_snoozed`. (5) Per-folder delta cursors are 5.7.8's; the single-row `graph_delta_cursor` stays the 5.4 property sync's. Proven by `crates/graph/tests/graph_mail_cache.rs` (the synthetic fixtures through the mappers into a real cache: folders, threads, flags, search, overlays, a move, a body) and an `upsert_messages` unit test. | M |
| 5.7.5 | **Session actor** — ✅ landed (wired by 5.7.6) | **Landed** as `source/crates/graph/src/mail/session.rs`: `run_graph_mail_session(config, client, cmd_rx, interactive_cmd_rx, evt_tx)` speaks the existing `AccountCommand`/`AccountEvent` contract, so nothing above the protocol seam changes. **Sync:** the folder hierarchy through `/me/mailFolders/delta` (well-known names resolved for roles), and each folder's messages delta into the shared cache - `upsert_messages` under 5.7.3's Uids, tombstones through `graph_uids_existing` + `delete_messages` (no Uid burnt for an unseen id), property records through `apply_remote_mail_properties` with a `taken_at` captured *before* the read. A read that starts from no cursor (first sync or a 410 reset) is the folder's whole snapshot and diffs with `replace_messages`, since such a page carries no tombstones; an incremental page upserts. A delta page is filtered by `parentFolderId`, and an expired cursor resyncs under the same Uids. There is no push, so the open folder is polled every `poll_interval` (default 45 s) and one other folder per tick in rotation, with the folder delta every fourth poll. **Reads:** `FetchBody` prefers `/$value` MIME (`parse_body`, so iMIP/`Authentication-Results` survive R3) and falls back to Graph's JSON body, keeping Graph attachment ids as part numbers; `FetchAttachment` and `FetchRawMessage` cache and emit the existing events. **Writes:** read state (`isRead`), follow-up flag, importance, Focused/Other (plus a native sender override for "Always move…"), categories from tag keys, move/copy/revert, snooze (local, like the IMAP fallback), junk (local keyword hint - the caller moves the folder), drafts (raw MIME, preserving the stable `Message-ID` and Bcc), folder create/rename/delete, `search_messages`, and the local outbox drained through Graph's MIME `sendMail` (Bcc header added for the wire). **Known gaps for 5.7.7:** no Pin, no Sieve (`SieveAdvertised(false)`), no folder move, remote `$search` is best-effort only, and a tag key Graph hasn't shown yet is written as its key. **Tests:** pure-helper unit tests plus `crates/graph/tests/graph_mail_session.rs`, a fake-Graph session that syncs folders/messages, reads a MIME body and performs a native `PATCH`. New plumbing: `GraphClient::post_raw`, `MailClient::{send_mime, create_draft_mime, resolve_message_id_in, get_summary}`, `Cache::graph_uids_existing`, and `lookout-mail`'s `OutboxMessage`/`OutboxState`/`build_draft_raw_message`/`outbox_items` exports. Per-folder cursor persistence landed in 5.7.8. | L |
| 5.7.6 | **App wiring + opt-in** — ✅ landed | **Landed.** Per-account **"Use Graph mail (experimental)"** switch in Config → Accounts (`AppConfig::graph_mail_accounts`, `graph_mail_enabled_for` / `set_graph_mail_enabled`, like `directory_accounts`; the switch has its own `GraphMailToggle` callback and is shown only for the account that owns a Microsoft 365 mailbox). Enabling runs Graph consent first (`MicrosoftOAuth::ensure_graph_mail_consent`: the combined Outlook grant plus Graph `Mail.Send`, which the outbox needs and `Mail.ReadWrite` does not grant), then tears the mail session down, drops the per-account cache and reconnects; disabling does the same in reverse, so neither transport ever sees the other's folder/Uid space. `connect.rs` picks `run_graph_mail_session` over `run_account_session` and records `AccountHandle::uses_graph_mail`. IMAP-only surfaces degrade: the Sieve backend disappears from the rule editor while no connected account advertises ManageSieve, the folder menu hides **Move** (Graph has no folder move), and the Config row shows "Microsoft Graph (experimental)" instead of the IMAP/SMTP hosts. The item-5.4 property mirror is suppressed on this transport (`AccountHandle::graph_mail` stays `None`) because the session owns reads and writes; the category master list, Graph rules and automatic replies keep working. Shared mailboxes stay on IMAP until the shared-mailbox Graph routes exist. | M |
| 5.7.7 | **Feature-gap reconciliation** — ✅ landed | **Landed.** §3.1 walks every `OUTLOOK_PARITY.md` §5.1 row on the Graph transport: 32 native, 8 degraded, 1 unavailable, 5 not implemented on any transport. `OUTLOOK_PARITY.md` §5.1 gains a **Graph** column and §2.1 a transport-only scorecard row, and the Accounts/M365 verdict names the opt-in. **Pin (M-13)** was the one action still visible but unsupported: the Graph session has no `\Flagged` counterpart, so it now degrades instead of silently losing the pin - the ribbon button is disabled with the tooltip "Pin isn't available on Graph mail", the row hover pin is disabled, and the context-menu Pin/Unpin entry is omitted (`MessageMenuState::can_pin`); `set_messages_pinned` also filters Graph-transport targets so a shortcut or mixed selection can't send a no-op `StoreFlagsMany`. Snooze and `$Junk` stay visible because the Graph session implements them locally; Sieve and folder move were already hidden by 5.7.6. §6.11: no new dialogs or errors, unchanged defaults, available actions keep Undo, and no empty/error-state regressions. | M |
| 5.7.8 | **Refresh cadence** — ✅ landed | **Landed.** The cadence stays the 45 s `DEFAULT_POLL_INTERVAL` from 5.7.5 (open folder + Inbox every poll, one more folder per tick, folder hierarchy every fourth poll), now documented in §3.2 and the README with the no-IDLE latency trade-off. **Per-folder message delta cursors are persisted** in the new `graph_folder_cursors` cache table: the session loads them at startup, stores each after its page has been applied (so a failed cache write cannot skip changes), clears one on a 410 before resnapshotting under the same Uids, and drops it when its folder is deleted. The single-row `graph_delta_cursor` still belongs to the 5.4 property sync. The local outbox remains independent of polling: due rows wake the session and go out through Graph's MIME `sendMail`, so undo send, schedule send and offline queuing are unchanged. | S |
| 5.7.9 | **Tests** — ✅ landed | **Landed.** Every `MailClient` endpoint has a wiremock test in `source/crates/graph/src/mail/client.rs` (folder hierarchy and message deltas, rows/body, attachments, MIME `$value`, move/copy/delete, `PATCH`, drafts/send, folder CRUD, search, 410 handling, scope and `Prefer` headers). Fixture round-trips live in `source/crates/graph/tests/graph_mail_cache.rs` (folders, delta page, message with attachments, body) and `source/crates/graph/tests/graph_mail_session.rs` (the checked-in `invite.eml` and `auth-results.eml` through Graph's `/$value` path, asserting the iTIP method, Outlook extensions and RFC 8601 headers survive). The fake-Graph session test now also proves a later sync follows the persisted per-folder cursor, not just that the cursor was written. **Integration harness:** `source/crates/graph/tests/graph_mail_integration.rs` parallels `lookout-mail/tests/imap_integration.rs` - an `#[ignore]`d, read-only live-tenant run (token from `LOOKOUT_GRAPH_MAIL_TOKEN`, optional `LOOKOUT_GRAPH_BASE_URL`) that drives the real session through folder sync, the Inbox delta, one body read and the shared cache. It is the harness for R3/R5/R7; it was not run here because no tenant token is available. | M |
| 5.7.10 | **Close-out** — ✅ landed | **Landed.** One `Unreleased` CHANGELOG entry cites 5.7 / A-06 and lists the tests + close-out. `OUTLOOK_PARITY.md` flips 5.7 to ✅ as a complete opt-in transport, adds the Graph mail endpoints to §4.4, and updates the §2.1 verdict, §5.1 transport note and §5.6/A-06 row; `README.md` describes the finished opt-in transport; and this file's status, task table and close-out section now record it as done. The live-tenant evidence remains R7, so the recommendation is still "IMAP by default". | S |

**Order:** 5.7.1 (gate) → 5.7.2 ∥ 5.7.3 → 5.7.4 → 5.7.5 → 5.7.6 → 5.7.7 → 5.7.8 → 5.7.9 → 5.7.10.

**Dependencies (all landed):** 0.3 outbox, 0.7 `GraphClient`, 5.4 delta + property
mapping + Message-ID map, 5.5 `sendMail`/deferred send, 5.6 shared-mailbox
folder/send-as transport. New consent must stay **incremental per feature**
(§4.4): Graph mail reuses `Mail.ReadWrite` / `Mail.Send` (shared variants only
for shared mailboxes).

## 5. Risks and hard blockers

| # | Risk / blocker | Impact | Mitigation / decision |
|---|---|---|---|
| R1 | **No push.** Change notifications need a public HTTPS `notificationUrl`; desktop apps cannot host one. | Sync is poll-only; new-mail latency regresses vs IDLE. | Accept as a known regression, or **don't do 5.7**. Interval polling only. |
| R2 | **Search.** Delta/`$filter` support a tiny subset; no `$search`. | Remote search can't match `search_query.rs`. | Rely on local FTS5 cache (as now); document the gap. |
| R3 | **MIME fidelity.** Invite card, TNEF, `Authentication-Results`, read receipts, inline `cid:` depend on exact MIME. | CAL/M-45/S-03/read-receipt features could break on Graph bodies. | Gate on the 5.7.1 spike with fixtures. If MIME differs materially, stay on IMAP. **Still unvalidated** — 5.7.1 ran without a tenant (§2.1). |
| R4 | **Identity model churn.** `Uid`/`UidValidity` are IMAP-shaped and used everywhere. | Large blast radius; undo/conversation/flagged/outbox correctness. | Done as an isolated step (5.7.3): Graph ids map onto the existing types through a persisted, account-unique Uid map with a stable validity, so no IMAP-shaped code changes. 5.7.5 now drives it end to end in a fake-Graph test; a live-tenant run is still outstanding. |
| R8 | **`mailbox_account_id` splits on the first `:`** (`app/src/window/support.rs`). `other:` and `shared:` account ids contain a colon, so it returns the wrong account for them. | Affects manual accounts today, and shared mailboxes if they ever move to the Graph transport. GOA Microsoft 365 accounts are unaffected. | Fix separately (scheduled after 5.7.3); 5.7.3's helpers never split on `:`. |
| R5 | **Throttling / cost.** Per-message body and attachment round trips. | Large mailboxes hit Graph throttling; slower first paint. | Batch, `$select` narrowly, rely on the client's `Retry-After` handling (already present). **Cost still unmeasured** — question (b) needs a tenant (§2.1). |
| R6 | **Feature regressions.** Sieve, `$Junk`/`$NotJunk`, keyword snooze, IMAP capability probes have no Graph equivalent. | Some M-rows degrade on Microsoft 365. | 5.7.7 reconciliation; hide/disable clearly (§10 "degrade softly"). |
| R7 | **Spike can't reach go/no-go without a tenant.** 5.7.1's evidence questions (a) and (b) need a real Microsoft 365 mailbox; fixtures and wiremock pin the shape, not Graph's actual bytes or latency. | 5.7 stays off by default on an assumption rather than measured evidence. | Run `source/crates/graph/tests/graph_mail_integration.rs` (read-only; `LOOKOUT_GRAPH_MAIL_TOKEN`) against a tenant and capture the three `.eml` fixtures before any green-light. The harness landed in 5.7.9; the run is still outstanding. |

## 6. Non-goals

- Server-side push / notifications (blocked, R1).
- Remote `$search` and full IMAP `SEARCH` parity (blocked, R2).
- Sieve / ManageSieve over Graph.
- Reproducing IMAP keyword fidelity where Graph has no equivalent.
- Exchange on-premises (EWS) — §9 non-goal.

## 7. Verification checklist (per §6.11 / §10)

- [x] Commands/events route through the existing `AccountCommand`/`AccountEvent`
      contract; no server call from the UI.
- [x] Interop strings (Graph property names, OData filters) live in
      `core/src/interop/` or `lookout_graph`, never inline in feature code.
- [x] Cache migrations note any format change in the CHANGELOG ("no data-format
      change" otherwise).
- [x] Missing capability degrades to a hidden UI element or a client fallback,
      never an error.
- [x] New fixtures under `source/test-fixtures/outlook/` for every artefact.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, workspace tests green.
      `fmt` and `clippy` are clean; the workspace test run is left to the
      release owner (automated tests are not run in this environment). The
      live-tenant harness is the separate R7 run.
- [x] §6.11 UX checklist passes for any user-facing change.

## 8. Close-out (item 5.7.10)

**Status: done, as an optional, opt-in transport.** IMAP remains the default.
All implementation tasks (5.7.2–5.7.10) have landed; 5.7.1's MIME and paging
evidence remains partial because it was run without a tenant.

The open item is evidence, not code: run
`source/crates/graph/tests/graph_mail_integration.rs` against a real Microsoft
365 mailbox (see its module docs for `LOOKOUT_GRAPH_MAIL_TOKEN`) and capture the
three `.eml` fixtures described in R7. If Graph's MIME bytes or real delta cost
differ materially, the opt-in transport may need adjustment before it is
promoted; the default does not change until that run says otherwise.

No new data format is introduced by 5.7.9/5.7.10. The only schema addition in
5.7 is the append-only `graph_folder_cursors` table from 5.7.8.
