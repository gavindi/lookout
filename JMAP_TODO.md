# JMAP mail support

This plan adds **JMAP** (RFC 8620 core, RFC 8621 mail) as a second mail protocol next to
IMAP/SMTP, for servers such as Fastmail and Stalwart (others: Cyrus 3.x, Apache James).
JMAP accounts would be manual accounts. Mail, folders, search, drafts and sending all go over
JMAP, and the rest of the app sees the same commands and events it sees today.

**Status:** assessment only for JMAP. Nothing JMAP-specific is implemented. The Graph mail
transport this plan builds on is now implemented and opt-in (`MAIL_GRAPH_OVERVIEW.md` items
5.7.2–5.7.10), so the architecture, identity model, cache strategy and app wiring are no longer
hypothetical.

**Revision 2026-10-22 (after `MAIL_GRAPH_OVERVIEW.md` landed):**

- Graph mail is no longer a deferred design. `lookout-graph` now contains a complete second
  account-session actor (`run_graph_mail_session`), its HTTP client, pure mappers, identity
  helpers, cache additions, fake-server tests and a live-tenant harness. JMAP should be built the
  same way, from that working template.
- The identity model (Graph R4) is largely solved: 5.7.3 landed the synthetic-Uid map, the stable
  per-account `UidValidity`, `MailboxId = "{account}:{remote id}"`, and filename-safe part numbers.
  J2 is now a generalization/mirroring task, not a redesign.
- The old J3 ("pull the transport-neutral parts out of `connect_and_run`") is no longer on the
  critical path. The Graph actor proves a compact standalone session is viable. J3 shrinks to
  exporting the few genuinely shared helpers; **IMAP's `connect_and_run` is left alone**.
- The session should live in `lookout-jmap`, mirroring `lookout-graph`, rather than inside
  `lookout-mail`.
- JMAP's change model is **account-wide per type** (`Email/changes`), not per-folder like Graph's
  delta. The *discipline* of persisted cursors is reusable; the per-folder cursor table is not.
- `UidValidity` must **not** be bumped on `cannotCalculateChanges` (from `/changes`) or
  `tooManyChanges` (from `Email/queryChanges`); a full resync can run under the same synthetic
  Uids, as 5.7.3 established for an expired Graph cursor.
- JMAP restores several capabilities Graph had to degrade: Pin (`$flagged`), native
  `$junk`/`$notjunk`, keyword snooze where custom keywords are accepted, folder move
  (`Mailbox/set parentId`), and stronger search. Focused/Importance remain Outlook-only.

## Current state

- Mail goes over IMAP/SMTP through `lookout-mail`. Microsoft 365 adds Graph, and CalDAV/CardDAV
  go through `lookout-dav`. Nothing in the repo speaks JMAP.
- `lookout-graph` now speaks the same `AccountCommand` / `AccountEvent` seam as `lookout-mail`
  and is selected per account at connect time. It is the second complete mail transport and the
  direct template for a third.
- Lookout began as a port of Bulwark Webmail *away from* JMAP (see the first CHANGELOG entry).
  So several of its models (identities, keywords) already line up with JMAP's.
- `MAIL_GRAPH_OVERVIEW.md` §3–5 is no longer just a design: §3 records the architecture as
  wired, §4 the tasks that landed, and §5 the risks that remain (live MIME evidence is R7).
  JMAP fits that architecture and can reuse most of its plumbing.
- GNOME Online Accounts has no JMAP provider, and Evolution has no JMAP accounts to import. So
  a JMAP account has to be added by hand.

## Why JMAP fits better than Graph mail

Of the four Graph-mail risks recorded in `MAIL_GRAPH_OVERVIEW.md` §5, three don't apply to
JMAP, and the fourth is largely retired by the Graph implementation:

| Graph risk | JMAP |
|---|---|
| **R1 No push** | JMAP pushes changes over EventSource (`eventSourceUrl`) and WebSocket (RFC 8887). A desktop app can use either, so new mail arrives about as fast as with IDLE. |
| **R2 Weak search** | `Email/query` filters (`text`, `from`, `to`, `subject`, `before`, `after`, `hasKeyword`, `inMailbox`, `minSize`, `maxSize`) map closely onto `search_query.rs`'s `SearchQuery`. |
| **R3 MIME fidelity** | Every Email has a `blobId` that downloads its exact RFC 5322 bytes, so `body::parse_body` and `preview_from_raw` work unchanged. iMIP invites, TNEF, `Authentication-Results` and read receipts keep working. |
| **R4 Identity churn** | **Largely retired.** 5.7.3 landed exactly this model: `MailboxId = "{account}:{remote id}"`, an account-unique synthetic `Uid` map, and one stable `UidValidity` per account. JMAP reuses it (J2); the work is generalizing or mirroring the cache tables, not redesigning `Uid`/`UidValidity` call sites. |

JMAP also restores several capabilities Graph had to degrade in 5.7.7:

| Graph gap (5.7.7) | JMAP |
|---|---|
| **Pin** unavailable (M-13) | `$flagged` is the standard keyword behind Pin. |
| **Snooze** local cache only (M-11) | Keyword + Scheduled folder, as on IMAP, where the server accepts custom `$Lookout-*` keywords; local snooze remains the fallback. |
| **`$Junk` / `$NotJunk`** local hints (M-17) | Native `$junk` / `$notjunk` keywords. |
| **Folder move** hidden | `Mailbox/set` with a new `parentId`. |
| **Search** best-effort (M-18) | `Email/query` filter; local FTS5 remains primary. |
| **Sieve** hidden (M-14) | JMAP for Sieve Scripts (RFC 9661) in J7. |
| **Focused / Importance** native | Still absent in JMAP; those Graph-only extras stay hidden for a manual JMAP account. |

## What already exists to reuse

### The protocol seam, and a second implementation

`AccountCommand` and `AccountEvent` (`source/crates/mail/src/session.rs:345` and `:743`) have 44
commands and 41 events. The app's event loop, message list, reading pane, composer, undo and
outbox UI talk to a mail account only through them. A third implementation only needs the same
**contract** (the two command channels and the event channel), not the same argument list as
`run_account_session` (`session.rs:1301`).

`lookout_graph::mail::session::run_graph_mail_session`
(`source/crates/graph/src/mail/session.rs:115`) is that third implementation for Graph. It
already proves every part of the seam outside IMAP:

- cache replay before the first network call;
- folder sync and per-folder message sync;
- `MessagesUpdated`, `NewMessages`, `NewMailArrived`, `FlaggedMessages`, `OutboxContents`;
- body, attachment and raw-message fetches, with the existing `BodyFetched` / `PartFetched` /
  `RawMessageFetched` events and failure events;
- native property writes and their cache overlays;
- move/copy/revert, folder create/rename/delete, snooze, drafts, search;
- the local outbox drain, retry, cancel and reschedule paths;
- capability gating (`SieveAdvertised(false)` and `SieveCapabilities` when the transport has no
  ManageSieve).

The JMAP actor should be written from this template. It is expected to be around 2,000 lines,
not a 1,000–2,000-line copy of IMAP plus a wire client.

### The cache and the identity model

`lookout_mail::Cache` (`source/crates/mail/src/cache.rs`) is reused as-is, as 5.7.4 proved for
Graph: one per-account SQLite file, the same `messages`/`bodies`/`search_fts`/`snoozed`/`outbox`
tables, and the same flat files. Relevant additions that already exist:

- `Cache::upsert_messages` (`cache.rs:1153`), the incremental counterpart of
  `replace_messages`.
- `Cache::load_summaries`, `flagged_messages`, `update_flags_many`,
  `update_keywords_many`, `snooze_messages`, `unsnooze_messages`, `purge_expired_snoozed`,
  `next_snooze_wake`, `enqueue_outbox`, `due_outbox`, `next_outbox_not_before` — all public and
  used by the Graph actor.
- The `graph_uids` / `graph_uid_space` tables (`Cache::graph_uids` at `cache.rs:2800`,
  `Cache::graph_uid_validity` at `:2785`): one account-unique synthetic `Uid` per remote message
  id, allocated in one `IMMEDIATE` transaction, reversible with `graph_ids_for` (`:2842`),
  non-allocating for tombstones with `graph_uids_existing` (`:2828`), pruned by
  `prune_graph_uids` (`:2860`).
- `graph_folder_cursors` (`cache.rs:2578`) and its setters: the persisted-cursor pattern, though
  JMAP's *shape* is account-wide rather than per folder (see J2).
- The Graph identity helpers (`source/crates/graph/src/mail/identity.rs`):
  `"{account}:{folder id}"` built with `MailboxId::new` and parsed back **only** by stripping the
  known account prefix, never by splitting on `:` (Graph R8); and `graph_part_number`, the
  base64url form for attachment part numbers so a `/` in a remote id can't escape the attachment
  directory.

JMAP's ids have the same properties: Email ids are stable across moves, mailbox ids are opaque,
and blob ids need a filename-safe form. J2 should generalize this model rather than invent a
parallel one.

### The session shell

The Graph actor's transport-neutral shell is the reusable part:

- `replay_cache`, `initial_sync`, `emit_mailbox`, `emit_flagged`, `emit_outbox`;
- `service_due` (snooze wake + outbox due), `next_sleep`, the interactive/background command
  drain;
- `queue_send`, `cancel_outbox`, `reschedule_outbox`, `drain_outbox`, `outbox_retry_delay`;
- `snooze` / `unsnooze`, count adjustment (`bump_counts`, `adjust_unread`,
  `adjust_move_counts`);
- folder helpers (`folder_id`, `role_of`, `publish_folders`), folder CRUD, drafts, search.

The IMAP-specific folder planners are **not** reusable: `scheduled_folder`,
`plan_folder_change` and `plan_create_mailbox` (`session.rs:5393`, `:5551`, `:5621`) assume IMAP
paths and a delimiter. The Graph actor correctly uses id-based folder operations instead. JMAP
mailboxes also have ids and a `parentId`, so J4 should follow the Graph shape.

### Parsing, building and the outbox

`body::parse_body` (`source/crates/mail/src/body.rs:16`), `body::preview_from_raw` (`:948`),
`send::build_raw_message` (`source/crates/mail/src/send.rs:223`),
`send::build_draft_raw_message` (`:208`), and the exported `OutboxMessage`, `OutboxState`,
`ComposedMessage` types and `outbox_items` are all public. 5.7.5 added the exports the Graph
actor needed; JMAP can use the same raw-MIME path for drafts and submissions.

### App wiring and manual accounts

- `OtherAccount` (`source/crates/app/src/app_config.rs:89`), the keyring slots in
  `source/crates/app/src/other_accounts.rs` (`KeyringStore` at `:59`,
  `OtherCredentialProvider` at `:171`), and `connect_other_account`
  (`source/crates/app/src/window/connect.rs:1485`).
- The Graph opt-in is the precedent for selecting a transport per account: `uses_graph_mail` is
  computed at connect time (`connect.rs:260`), the branch spawns `run_graph_mail_session`
  (`connect.rs:375`–`:388`), and enabling/disabling tears the session down and drops the
  per-account cache because the two folder/Uid spaces cannot be merged
  (`MAIL_GRAPH_OVERVIEW.md` 5.7.6). JMAP is a manual-account protocol choice rather than a
  Microsoft 365 opt-in, but the same cache-drop rule applies when switching protocols.
- `AccountHandle::uses_graph_mail` (`source/crates/app/src/window/state.rs:109`) is used to hide
  Graph-unsupported UI (Pin, folder move, Graph-only extras). A JMAP account must **not** set it:
  JMAP supports Pin and folder move. The Graph-only extras are already `None` for a manual
  account, so most JMAP gating comes free.

### Tests and fixtures

The Graph suite is the model:

- wiremock tests per `MailClient` endpoint (`source/crates/graph/src/mail/client.rs`);
- fixture round-trips through a real cache (`source/crates/graph/tests/graph_mail_cache.rs`);
- a fake Graph server driving the real session (`source/crates/graph/tests/graph_mail_session.rs`);
- an ignored, read-only live-tenant run (`source/crates/graph/tests/graph_mail_integration.rs`);
- synthetic wire fixtures under `source/test-fixtures/outlook/`.

JMAP needs the same three levels plus a Stalwart testcontainer, and its fixtures belong under
`source/test-fixtures/jmap/`.

### HTTP, retries and limits

`source/crates/graph/src/client.rs` shows the house pattern: one shared `reqwest::Client`,
`Retry-After` handling, `get_bytes_with_headers` for binary reads, `post_raw` for MIME uploads,
and `DeltaPage` paging. JMAP needs a similar client with two additions Graph does not have:

- **request batching** (one POST to `apiUrl` with a `methodCalls` array and `#result`
  back-references), and
- **EventSource** streaming for push.

The app's `crate::http::client()` is app-only; a `lookout-jmap` crate should own its shared
client the way `lookout-graph` owns its own.

### Capability gating

The Sieve UI appears only after `AccountEvent::SieveAdvertised(true)`, and the Graph extras
(Focused, categories, Graph rules, Graph automatic replies) are already limited to Microsoft 365
accounts by the `graph_*` handles being `None`. A JMAP session can emit
`SieveAdvertised(false)` until J7 and simply not emit the Graph-only events. No new
capability-gating mechanism is needed; J7 adds the JMAP-specific ones (vacation, Sieve, quota).

## How Lookout's mail features map onto JMAP

| Lookout | JMAP | Notes |
|---|---|---|
| Folders, counts, roles | `Mailbox/get`, `Mailbox/changes`, `Mailbox/set`, `role`, `parentId`, `totalEmails` / `unreadEmails` | The server reports counts, so the IMAP STATUS drain isn't needed. `Mailbox/set` supports create, rename, move (`parentId`) and destroy. |
| Envelope sync | `Email/query` + `Email/get`, `Email/changes`, `Email/queryChanges` | `Email/changes` is **account-wide per type**, not per folder. `Email/get` returns `mailboxIds`, so one change can update or remove rows in every folder it belongs to. |
| New-mail push | `StateChange` over EventSource (`eventSourceUrl`), WebSocket (RFC 8887) | Primary delivery. Keep the Graph-style poll loop as the fallback when push is absent or fails. |
| Flags, keywords, snooze, `$Junk`, Pin | patch `keywords` (`$seen`, `$flagged`, `$answered`, `$draft`, `$junk`, `$notjunk`, `$Lookout-*`) | `$flagged` is Pin; `$junk`/`$notjunk` are native; follow-up flags and tags keep the existing `$Lookout-*` atoms. Custom-keyword support varies by server; fall back to local snooze/hints. |
| Move, copy, revert | patch `mailboxIds`; `Email/copy` for a copy; `Email/set` destroy for undoing a copy | An Email id is stable across a move. An Email may be in several mailboxes, so "move" means add the target and remove the source, not replace the whole set (risk 3). |
| Body, attachment, raw message | `Email/get` `bodyStructure` / `bodyValues`; `blobId` + `downloadUrl` | Raw blob is exact RFC 5322 for `parse_body`. Part numbers use the blob id in a filename-safe form, as `graph_part_number` does. |
| Drafts | blob upload + `Email/import` into Drafts with `$draft` | Preserves the compose session's stable `Message-ID` and Bcc. Replace by Message-ID, exactly as the IMAP and Graph paths do. |
| Send, outbox | `EmailSubmission/set` + `onSuccessUpdateEmail` (Drafts → Sent) | Import the draft Email first. The server files the sent copy; `onSuccessUpdateEmail` moves the draft out of Drafts and clears `$draft`. Bcc rides the submission `Envelope`, not a header. The local outbox keeps undo send, scheduled send and offline queuing. `FUTURERELEASE` is optional. |
| Send-as | `Identity/get` | Map into Lookout's identities (read-only sync or send-time validation); local identities remain the composer's source unless the user adopts a server identity. |
| Search | `Email/query` filter | `text`/`from`/`to`/`cc`/`subject`/`body`, `before`/`after`, `minSize`/`maxSize`, `hasAttachment`, `hasKeyword`/`notKeyword`, `inMailbox`. Follow-up is `$Lookout-flagged` and Pin is `$flagged`; multiple categories need an AND `FilterOperator`. Local FTS5 is still primary; `is:mentions` stays local. |
| Threads | `threadId`, plus `references` / `inReplyTo` | Use `jmap:{threadId}` like Graph's `graph:{conversationId}`, or compute from references if IMAP-compatible keys are ever required. |
| Automatic replies | `VacationResponse` | Optional; only when the server advertises the capability. |
| Rules | JMAP for Sieve Scripts (RFC 9661) | Later (J7). Stays hidden until then. |
| Quota | JMAP Quotas extension, when advertised | Later (J7). |
| Focused / Importance | No native JMAP property | Graph-only extras remain hidden on a manual JMAP account. |
| Calendars, contacts | JMAP Calendars / Contacts | **Out of scope.** Fastmail and Stalwart also serve CalDAV/CardDAV, which `lookout-dav` already handles. |

**Authentication.** Stalwart and Cyrus accept Basic. Fastmail requires an API token (Bearer) or
OAuth for JMAP; its app passwords don't work. Start with Basic + Bearer; OAuth can come later.

**Discovery.** `https://<domain>/.well-known/jmap` returns the Session object (`apiUrl`,
`downloadUrl`, `uploadUrl`, `eventSourceUrl`, `primaryAccounts`, `capabilities`, `state`). Follow
redirects, and allow an explicit session URL for servers that don't publish the well-known
endpoint.

## Proposed decisions (to confirm before J1)

- **A new crate, `source/crates/jmap` (`lookout-jmap`), mirroring `lookout-graph`'s layout.**
  `client.rs` for HTTP, Session discovery, batching, EventSource and errors; `mail/client.rs`
  for the mail methods; `mail/message.rs` for pure `EmailSummary` / `EmailBody` / `BodyPart`
  mapping; `mail/identity.rs` for `MailboxId`/`Uid`/part-number helpers; `mail/session.rs` for
  the actor; wire types in `mail/mod.rs`. Fixtures under `source/test-fixtures/jmap/`. Write it
  in-house rather than depend on the `jmap-client` crate: that crate's maintenance is uncertain,
  and `graph`/`dav` were built the same way.
- **The session actor lives in `lookout-jmap`**, not `lookout-mail`:
  `run_jmap_account_session(config, credentials, cmd_rx, interactive_rx, evt_tx)`. The Graph
  actor proves a session can live outside the mail crate using the public `Cache` and
  `lookout_mail::session` types. This keeps `lookout-mail` IMAP-owned and puts the actor beside
  the protocol it speaks. The app spawns it exactly as it spawns `run_graph_mail_session`.
- **Build from the Graph actor; do not refactor IMAP.** J3 is a small export of genuinely shared
  helpers (`cache_op`, `outbox_retry_delay`, `MAX_OUTBOX_ATTEMPTS`) so Graph and JMAP stop
  carrying copies. Leave `connect_and_run` (`session.rs:1521`) alone. If duplication later
  proves painful, extract a `lookout_mail::remote_session` shell as a separate refactor with the
  Graph fake-server test as the regression net — not as a prerequisite for JMAP.
- **Identity and state tables.** Promote the Graph model to transport-neutral names, or mirror it
  as a fallback:
  - **Recommendation:** rename/migrate `graph_uids` → `remote_uids` and `graph_uid_space` →
    `remote_uid_space`, backfilling the existing rows in one append-only migration, and keep the
    current Graph methods as thin wrappers while the Graph actor is still on them. A JMAP
    account's cache is used by one transport at a time, but the abstraction is now justified by
    two non-IMAP transports.
  - **If migration risk is unwanted:** add `jmap_uids` / `jmap_uid_space` with exactly the same
    shape. Do not invent an `AUTOINCREMENT` primary key; the Graph table's
    `(remote_id PRIMARY KEY, uid UNIQUE)` plus a `next_uid` counter is the proven shape and
    detects `u32` exhaustion.
  - **State strings are different.** `Email/changes` is account-wide, so do not reuse
    `graph_folder_cursors`. Add a small `remote_states(kind TEXT PRIMARY KEY, state TEXT,
    updated_at)` (or `jmap_states`) for the `Email` and `Mailbox` states across the account.
    Reuse the cursor *discipline*: persist after the cache write, never before; clear on
    `cannotCalculateChanges` / `tooManyChanges`; resnapshot under the same Uids.
- **`UidValidity` rule (changed from the first draft).** One random nonzero value per account,
  fixed when the Uid map is created. `cannotCalculateChanges` / `tooManyChanges` does **not**
  bump it: the Email ids are unchanged, so a full resync under the same synthetic Uids keeps
  cached bodies, attachments, search rows and undo references. Bump it only when the map is lost
  (cache deleted or cleared), exactly as 5.7.3 decided for Graph.
- **Change model.** Keep one account-level `Email` state and one `Mailbox` state. On a
  `StateChange`, call `Email/changes`; for each changed id, `Email/get` the selected properties
  (including `mailboxIds` and `keywords`), then:
  - upsert a row in every mailbox in `mailboxIds`;
  - remove the row from mailboxes the message has left;
  - delete rows for `destroyed` ids using the non-allocating Uid lookup, so a message deleted
    before it was ever cached doesn't burn a Uid.
  Initial folder snapshots use `Email/query` (`inMailbox`) + `Email/get`. `Email/queryChanges`
  is optional; local sorting over the cached folder is enough for ordering. Add a
  `Cache::mailboxes_for_uid` helper if the cache can't otherwise find the rows a changed Email
  has left.
- **Account settings stay backward-compatible.** `OtherAccount` gains
  `#[serde(default)] protocol: MailProtocol { Imap, Jmap }`, `jmap_session_url: Option<String>`,
  `jmap_username: Option<String>` and `jmap_auth: Basic | Bearer`; existing `settings.json`
  files still load as IMAP. A new keyring slot, `PROTOCOL_JMAP`, holds the password or token.
  The JMAP form asks for address, username (defaulting to the address), secret, and an optional
  session URL; discovery (`/.well-known/jmap`) validates the server on save. Do not overload
  `lookout_mail::session::CredentialProvider` (IMAP/SMTP); give JMAP its own small credential
  trait or pass a loader into its config.
- **Push first, poll fallback.** EventSource is the primary path. If `eventSourceUrl` is absent,
  the stream fails repeatedly, or the server advertises no push, fall back to the Graph-style
  poll loop (open folder + Inbox each tick, one background folder in rotation, folder state
  refresh on a slower cadence). Handle EventSource `ping` events and reconnect with backoff;
  on reconnect, call `/changes` from the last persisted state rather than replaying the stream.
- **Capabilities are parsed once and passed around.** A `JmapCapabilities` type should expose
  `core` limits (`maxObjectsInGet`, `maxObjectsInSet`, `maxSizeUpload`,
  `maxConcurrentUpload`, `maxSizeRequest`), the mail account id, and optional capabilities
  (`submission`, `vacationresponse`, `sieve`, `quota`, `websocket`). No capability URN belongs
  in feature code.

## Risks

1. **The identity model (Graph R4) — largely retired.** The Graph transport already maps remote
   ids onto `Uid`/`UidValidity` and proves it through a fake-server test. The remaining risk is
   the *generalization* (renaming or mirroring the cache tables) and the multi-mailbox case
   below, not the identity type itself. `MailboxId` helpers must strip the account prefix and
   never split on `:` — `other:` and `shared:` account ids contain a colon (Graph R8).
2. **Duplication vs a shared shell.** `connect_and_run` is no longer the critical path, but
   copying the Graph actor creates two shells that can drift. Mitigations: export the few shared
   helpers now (J3), keep the Graph fake-server test green, and give JMAP its own equivalent.
   The optional shell extraction can follow later. The IMAP regression risk of the original J3 is
   removed.
3. **Emails in several mailboxes.** JMAP allows one Email in several mailboxes, unlike Graph's
   single folder. Folder views work as they are, but:
   - a move must add the target and remove the source, not replace the whole `mailboxIds` set
     unless the user means "move everywhere";
   - a change in `mailboxIds` must remove the row from every mailbox the message left;
   - `destroyed` means the Email is gone from all mailboxes;
   - unread counts come from `Mailbox/get`, not from local arithmetic;
   - a unified-inbox merge may show the same message twice if it is in two mailboxes; dedupe by
     `messageId`/`threadId` where the UI expects one row.
4. **Servers differ.** Fastmail is the reference implementation and Stalwart is solid; Cyrus
   needs configuring. Custom keyword support, EventSource/WebSocket availability, submission
   behaviour, vacation and Sieve support all vary. Test against Stalwart in a testcontainer, the
   way the IMAP tests use GreenMail, and keep a Fastmail token check manual.
5. **Push reliability.** EventSource is an advantage only if it is implemented well: long-lived
   HTTP streams through proxies, reconnect/backoff, `ping` handling, session expiry, and a
   polling fallback. Keep the Graph poll loop as a tested fallback, not an afterthought.
6. **Change-state invalidation.** `cannotCalculateChanges` (from `/changes`) and
   `tooManyChanges` (from `Email/queryChanges`) force a full resync. The state is account-wide,
   so one invalid state can resnapshot every folder. The Uid map must survive it (stable
   `UidValidity`), and the cursor must be cleared only after a successful resnapshot, so a failed
   resync cannot skip changes.
7. **Request batching and limits.** Back-references (`#callId/result/path`), out-of-order
   responses, per-method errors, `maxObjectsInGet`/`maxObjectsInSet`, upload sizes and concurrent
   upload limits all need handling in J1. A batch that partially fails must leave the cache and
   cursor consistent.
8. **Submission is a two-step, not a send.** `EmailSubmission/set` needs an existing Email, so
   the outbox drain must upload/import the draft first, then submit it, then file the sent copy
   through `onSuccessUpdateEmail`. Duplicate drafts on retry, a missing Drafts/Sent mailbox,
   identity selection for send-as, the undo window (`undoStatus`) and optional `FUTURERELEASE`
   all need explicit handling in J5. Bcc belongs in the submission `Envelope` (`rcptTo`), not in
   a MIME header — do not copy Graph's `with_bcc_header` workaround.
9. **Body and attachment cost.** `Email/get` with `bodyValues` can return large bodies; the raw
   blob is one download per message. Prefetch (the existing policy) and the FTS body upgrade
   should keep first paint fast. `downloadUrl` is a template and may be session-scoped, so cache
   bytes rather than URLs.
10. **Transport switching.** Switching an IMAP account to JMAP (or back) must drop the
    per-account cache, as 5.7.6 does for Graph, because the folder/Uid spaces differ. The
    add-account dialog must hide the IMAP/SMTP fields for a JMAP account, and the Config row
    should show the JMAP session URL rather than IMAP/SMTP hosts.
11. **Keyword variance.** `$junk`, `$notjunk` and custom `$Lookout-*` keywords are standard-ish
    but server support varies. A server that rejects a custom keyword must degrade to the local
    behaviour (snooze row, local junk hint) rather than fail the command.
12. **Auth and discovery edge cases.** Fastmail app passwords don't work; Basic vs Bearer must be
    explicit. `.well-known` may redirect or be absent; a self-hosted server may need an explicit
    session URL. Never log the token, and require TLS.

## Task breakdown

Effort: **S** ≤ 1 day · **M** ≤ 1 week · **L** multi-week.

| # | Task | Effort |
|---|---|---|
| J0 | **Spike.** Session discovery, `Mailbox/get`, `Email/query` + `Email/get`, blob download and an EventSource handshake in `lookout-jmap`, tried against Stalwart (in a container) and a Fastmail token. Confirm a downloaded message parses the same through `parse_body`, and confirm the account-wide `Email/changes` model against a move and a multi-mailbox message. | S |
| J1 | **`lookout-jmap` transport.** Session discovery and capabilities, batched method calls with back-references, `Mailbox` / `Email` / `Identity` / `EmailSubmission` / `VacationResponse` types, blob upload/download, EventSource (WebSocket optional) with a polling fallback, error/limit handling, wiremock tests and fixtures. The mappers must handle JMAP's array-valued `messageId` / `inReplyTo` / `references` and its map-valued `mailboxIds` / `keywords`, which differ from Graph's scalar shapes. | M |
| J2 | **Identity and state plumbing.** Generalize or mirror the Graph Uid map (`remote_uids` / `remote_uid_space`, or `jmap_*`), add account-wide `Email`/`Mailbox` state persistence, JMAP `MailboxId`/part-number helpers, and `Cache::mailboxes_for_uid` if needed. Write up in SCAFFOLD.md §3/§4. | S–M |
| J3 | **Shared helpers.** Export `cache_op`, `outbox_retry_delay` and `MAX_OUTBOX_ATTEMPTS` from `lookout-mail`; switch `lookout-graph` to them. No IMAP changes. Optional later: extract the transport-neutral session shell, with the Graph fake-server test as the net. | S |
| J4 | **`run_jmap_account_session`.** Folder and account-level envelope sync, EventSource/poll loop, body/attachment/raw fetch, keywords and Pin, junk, follow-up and tags, move/copy/revert, folder create/rename/move/delete, snooze, drafts, search. Use the Graph actor as the template. | L |
| J5 | **Sending.** `Identity/get` and identity selection, draft import, `EmailSubmission/set` with `onSuccessUpdateEmail`, Sent filing, and the local outbox drain (undo send, scheduled send, offline queue). Build the submission `Envelope` from the outbox recipients, with Bcc in `rcptTo` rather than a header (unlike Graph's `with_bcc_header` workaround). `FUTURERELEASE` optional. | M |
| J6 | **App wiring.** `OtherAccount.protocol`, the `PROTOCOL_JMAP` keyring slot, a protocol choice in the add-account dialog (address, username, password or token, optional session URL), discovery/validation, the branch in `connect_other_account`, the Config row, hiding IMAP-only fields, and dropping the per-account cache on a protocol switch. | S–M |
| J7 | **Extras, each only when the server offers it.** `VacationResponse` for automatic replies, JMAP Sieve for rules, quota. Reuse the existing Sieve/automatic-replies UI where possible. | M |
| J8 | **Tests.** A Stalwart testcontainer integration test alongside `source/crates/mail/tests/imap_integration.rs`, a fake-server session test adapted from `graph_mail_session.rs`, cache tests adapted from `graph_mail_cache.rs`, and fixtures under `source/test-fixtures/jmap/`. | M |
| J9 | **Docs.** README, SCAFFOLD.md §3/§4, this file. CHANGELOG and version only when asked. Add OUTLOOK_PARITY.md rows only if JMAP changes a parity statement. | S |

**Order:** J0 → J1 ∥ J2 → J3 → J4 → J5 → J6 → J7 ∥ J8 → J9.

**Rough total:** 5–7 weeks of focused work, down from 6–10. The saving is concentrated in J2
(generalize a proven model), J3 (no IMAP refactor) and J6 (reuse the per-account transport
wiring); J4 remains the bulk and the main unknown. If EventSource, batching or submission prove
awkward, add a week. Users see no change from J0–J2, or from J6 while it stays behind the
protocol choice.

## Verification checklist

- [ ] Commands and events go through the existing `AccountCommand` / `AccountEvent` contract,
      and the UI never calls the server directly.
- [ ] JMAP property names and capability URNs live in `lookout-jmap`, never in feature code.
- [ ] Every cache format change is noted in the CHANGELOG ("no data-format change" otherwise).
- [ ] A capability the server lacks hides its UI element or falls back to the client. It never
      shows an error.
- [ ] Fixtures under `source/test-fixtures/jmap/` for every wire shape.
- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`
      are clean.
- [ ] `cargo test -p lookout-jmap` (wiremock) and `cargo test -p lookout-mail`, including the
      Stalwart JMAP test, pass; `cargo test -p lookout-graph` still passes after J3; the IMAP
      integration test is unchanged.
- [ ] A moved Email keeps its synthetic Uid; an Email in several mailboxes has one Uid and one
      row per mailbox; a `destroyed` id doesn't burn a new Uid.
- [ ] `cannotCalculateChanges` / `tooManyChanges` resyncs under the same Uids; a lost Uid map
      bumps `UidValidity`, and the state is only cleared after a successful resnapshot.
- [ ] A message that leaves a folder is removed from that folder's cache when `Email/changes`
      reports the update.
- [ ] Manual check with a Stalwart or Fastmail account: folders, list, open, move and undo,
      flag and Pin, snooze, junk, draft, send with undo send, search, and new mail arriving by
      push within seconds. Repeat once with push disabled to confirm the polling fallback.
