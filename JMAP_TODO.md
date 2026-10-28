# JMAP mail support

This plan adds **JMAP** (RFC 8620 core, RFC 8621 mail) as an **additional mail transport** in
Lookout, alongside the existing IMAP/SMTP, Microsoft Graph, and CalDAV/CardDAV paths. It is not a
replacement for IMAP, not a replacement for Graph, and not a migration: no existing account is
migrated or switched automatically, no default changes, and no data is converted. A JMAP account
would be a new, manually added account for servers such as Fastmail and Stalwart (others: Cyrus
3.x, Apache James), using the same commands, events, cache, and UI as every other account.

Mail, folders, search, drafts and sending would all go over JMAP; calendars, contacts and tasks
stay on their existing DAV/Graph backends. The rest of the app sees the same
`AccountCommand` / `AccountEvent` contract it sees today.

**Status:** assessment only for JMAP. Nothing JMAP-specific is implemented. The transport-neutral
work this plan depends on is no longer hypothetical: Graph mail is implemented and opt-in, and most
of Lookout's mail features now sit above the protocol seam. JMAP is therefore a **protocol adapter**
added to an existing multi-protocol client, not a new connectivity stack.

**In one sentence:** JMAP would be an optional third mail transport for JMAP-speaking servers,
built on the transports and shared features Lookout already has, not a replacement for IMAP or
Graph.

**What this changes for existing users:** nothing. IMAP/SMTP stays the default, Graph stays the
Microsoft 365 opt-in, and DAV stays the calendar/contact/task path. A JMAP account would appear
only after someone adds one by hand, and it would then participate in the same folder pane, unified
inbox, search, notifications, outbox and reading/composer UI as any other account.

**Revision 2026-10-28 — "an addition, not a substitute", after Lookout's transport layer
matured:**

This revision re-bases the plan on the current codebase: it keeps the JMAP-specific protocol
work, drops work that is now shared, and records the additive positioning.

- **Additive by design.** JMAP is a third mail transport, selected per account. IMAP/SMTP remains
  the default for manual and GOA accounts; Graph remains the Microsoft 365-specific opt-in; JMAP
  would be offered only when the user chooses it for a JMAP-speaking server. No account is
  converted or switched automatically, no cache is migrated, and no existing connection path is
  removed.
- **Much of the plan is already built.** Graph mail (`MAIL_GRAPH_OVERVIEW.md` items 5.7.2–5.7.10)
  supplied the second implementation of the protocol seam, a standalone session actor, the
  remote-id/synthetic-Uid model, the shared cache, per-account transport selection, and the
  three-level test pattern. JMAP no longer has to invent those pieces.
- **Most of the feature surface is already transport-neutral.** Since the first JMAP assessment,
  Lookout has gained a unified inbox, Focused Inbox (including a local classifier for non-Microsoft
  accounts), categories, follow-up flags, snooze, conversations, search, attachment handling,
  outbox/undo/schedule send, notifications, and the reading/composer UI. These are account features
  above the seam; a JMAP account inherits them instead of reimplementing them. Importance stays
  Graph-only; rules, automatic replies and Sieve keep their capability gates, with JMAP backends
  in J7 where the server offers them.
- **The remaining work is JMAP-specific.** The wire client and types, request batching,
  EventSource/WebSocket push, the account-wide `Email/changes` model, `EmailSubmission`, the
  protocol choice in the manual-account form, and JMAP fixtures/tests. J4 (the session actor) is
  still the bulk; J2/J3/J6 are smaller because Graph already exercised the same paths.
- **Keep PIM where it is.** JMAP Calendars/Contacts (and any task extension) are out of scope:
  `lookout-dav` already covers Fastmail's and Stalwart's CalDAV/CardDAV endpoints, and Graph covers
  Microsoft 365. A JMAP account should not fork a second calendar/contact stack.
- **Retain the 2026-10-22 decisions:** Graph is the template; the actor lives in `lookout-jmap`;
  IMAP's `connect_and_run` is left alone; `Email/changes` is account-wide per type; `UidValidity`
  does not bump on `cannotCalculateChanges`/`tooManyChanges`; JMAP does not inherit Graph's
  Pin/junk/folder-move/search/Sieve degradations.

**Revision 2026-10-22 — after `MAIL_GRAPH_OVERVIEW.md` landed (retained):**

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
- A JMAP account would not inherit several of Graph's transport degradations: Pin (`$flagged`),
  native `$junk`/`$notjunk`, keyword snooze where custom keywords are accepted, folder move
  (`Mailbox/set parentId`), and stronger search. Focused Inbox still works through Graph where
  native, or through the local classifier elsewhere; Importance remains Graph-only.

## Scope: an addition to connectivity, not a substitute

Lookout already speaks more than one protocol. JMAP would be another option in that list, not a
replacement for any entry in it:

```
Lookout connectivity
├── lookout-mail   IMAP/SMTP        default mail transport
├── lookout-graph  Microsoft Graph  Microsoft 365 mail opt-in + native PIM
├── lookout-dav    CalDAV/CardDAV   calendars, contacts, tasks
└── lookout-jmap   JMAP             proposed additional mail transport
```

| Existing path or layer | What it covers | Effect of adding JMAP |
|---|---|---|
| **IMAP/SMTP** (`lookout-mail`) | The default mail transport for GOA and manual accounts; IDLE, Sieve, IMAP keywords | Unchanged. A JMAP account is another manual-account choice, not a migration of existing accounts. |
| **Microsoft Graph** (`lookout-graph`) | Microsoft 365 mail behind an opt-in, plus native calendar/contacts/To Do | Unchanged. Microsoft does not offer JMAP, so Graph remains the Microsoft 365 path. |
| **CalDAV/CardDAV** (`lookout-dav`) | Calendars, contacts and tasks for Fastmail, Stalwart and other DAV servers | Unchanged and reused. JMAP Calendars/Contacts (and any task extension) are out of scope. |
| **Manual accounts** (`OtherAccount`, keyring) | IMAP/SMTP accounts added by hand | Gains a protocol choice; existing IMAP accounts load and behave exactly as before. |

**In scope:** JMAP mail, folders, envelope sync, bodies, attachments, raw messages, keywords,
flags, move/copy, folder management, drafts, sending, search, push, and the account/discovery
plumbing needed to reach those.

**Out of scope:** replacing or migrating IMAP/Graph; changing defaults; a GNOME Online Accounts
JMAP provider or Evolution import; JMAP Calendars/Contacts (and any task extension); OAuth for JMAP
in the first pass (Basic and Bearer first); server-specific extensions beyond advertised
capabilities; shared or delegated JMAP mailboxes until a server exposes them in a usable way.

## Current state

- Lookout's connectivity is already additive and multi-protocol. Mail goes over IMAP/SMTP through
  `lookout-mail`; Microsoft 365 adds Graph; CalDAV/CardDAV go through `lookout-dav`. No Lookout
  crate speaks JMAP, and there is no JMAP account type.
- `lookout-graph` speaks the same `AccountCommand` / `AccountEvent` seam as `lookout-mail` and is
  selected per account at connect time. It is the second complete mail transport and the direct
  template for a third. Its session, client, mappers, identity helpers and tests are the working
  proof that a transport can live outside `lookout-mail`.
- The app-side feature set has grown since the original assessment: unified inbox, Focused Inbox
  (Graph-native for Microsoft 365, local classifier elsewhere), categories, follow-up flags,
  snooze, conversations, FTS5 search, attachments and raw MIME, the local outbox with
  undo/schedule/offline send, notifications, and the shared reading and composer UI. Rules,
  automatic replies and Sieve already have UI; the JMAP backends for those are J7. Adding a
  transport does not require reimplementing any of these app-side behaviours.
- Lookout began as a port of Bulwark Webmail *away from* JMAP (see the first CHANGELOG entry). So
  several of its models (identities, keywords, folders) already line up with JMAP's.
- `MAIL_GRAPH_OVERVIEW.md` §3–5 is no longer just a design: §3 records the architecture as wired,
  §4 the tasks that landed, and §5 the risks that remain (live MIME evidence is R7). JMAP fits that
  architecture and can reuse most of its plumbing.
- GNOME Online Accounts has no JMAP provider, and Evolution has no JMAP accounts to import. So a
  JMAP account has to be added by hand.

## Why JMAP is a useful addition (not a replacement for IMAP or Graph)

JMAP is worth adding for the servers that speak it, not as a better way to connect Microsoft 365.
Microsoft does not offer JMAP; Fastmail and Stalwart do not offer Graph. The comparison below sets
expectations for a JMAP account and shows which Graph limitations are protocol-specific. It is not
a case for moving any existing account.

Of the four Graph-mail risks recorded in `MAIL_GRAPH_OVERVIEW.md` §5, three don't apply to JMAP,
and the fourth is largely retired by the Graph implementation:

| Graph risk | JMAP |
|---|---|
| **R1 No push** | JMAP pushes changes over EventSource (`eventSourceUrl`) and WebSocket (RFC 8887). A desktop app can use either, so new mail arrives about as fast as with IDLE. |
| **R2 Weak search** | `Email/query` filters (`text`, `from`, `to`, `subject`, `before`, `after`, `hasKeyword`, `inMailbox`, `minSize`, `maxSize`) map closely onto `search_query.rs`'s `SearchQuery`. |
| **R3 MIME fidelity** | Every Email has a `blobId` that downloads its exact RFC 5322 bytes, so `body::parse_body` and `preview_from_raw` work unchanged. iMIP invites, TNEF, `Authentication-Results` and read receipts keep working. |
| **R4 Identity churn** | **Largely retired.** 5.7.3 landed exactly this model: `MailboxId = "{account}:{remote id}"`, an account-unique synthetic `Uid` map, and one stable `UidValidity` per account. JMAP reuses it (J2); the work is generalizing or mirroring the cache tables, not redesigning `Uid`/`UidValidity` call sites. |

A JMAP account would not inherit the transport-specific compromises Graph had to make in 5.7.7.
This is about per-protocol feature parity, not about replacing Graph:

| Graph gap (5.7.7) | JMAP |
|---|---|
| **Pin** unavailable (M-13) | `$flagged` is the standard keyword behind Pin. |
| **Snooze** local cache only (M-11) | Keyword + Scheduled folder, as on IMAP, where the server accepts custom `$Lookout-*` keywords; local snooze remains the fallback. |
| **`$Junk` / `$NotJunk`** local hints (M-17) | Native `$junk` / `$notjunk` keywords. |
| **Folder move** hidden | `Mailbox/set` with a new `parentId`. |
| **Search** best-effort (M-18) | `Email/query` filter; local FTS5 remains primary. |
| **Sieve** hidden (M-14) | JMAP for Sieve Scripts (RFC 9661) in J7. |
| **Focused / Importance** native | No native JMAP property. Focused Inbox uses the local classifier for a JMAP account, as it does for IMAP; Importance stays Graph-only and hidden. |

What a JMAP account shares with every other account is at least as important as what is different:
the unified inbox, folder tree, conversations, local search, categories, flags, snooze, attachment
handling, outbox/undo/schedule send, notifications, the composer and the existing UI for rules and
automatic replies are already above the transport seam. The transport only has to supply the JMAP
operations behind them.

## What already exists to reuse

### What Graph and the transport-neutral features already solved

Roughly half of the original JMAP plan is now existing infrastructure. This table is the short
version; the subsections below give the details.

| JMAP need | Already in Lookout | Remaining JMAP work |
|---|---|---|
| Protocol seam | `AccountCommand` / `AccountEvent`; Graph is a second implementation | None; implement the same contract |
| Session actor | `run_graph_mail_session` (about 2,400 lines) with cache replay, sync, fetch, writes, outbox, search | Adapt the shell; replace Graph calls with JMAP calls |
| Per-account cache | `lookout_mail::Cache`, shared by IMAP and Graph | None; add JMAP-specific state tables |
| Remote identity | `graph_uids` / `graph_uid_space`, stable `UidValidity`, `MailboxId` prefix helpers | Generalize or mirror for JMAP |
| App wiring | `OtherAccount`, keyring, `connect_other_account`, per-account `uses_graph_mail` | Add a protocol field and a JMAP branch |
| Outbox | `outbox` table, undo/schedule/offline, Graph drain | Add an `EmailSubmission` drain |
| MIME | `parse_body`, `preview_from_raw`, draft builders | None; JMAP's blob is RFC 5322 |
| Search | FTS5 + `SearchQuery` | Optional JMAP `Email/query`; local stays primary |
| Feature UI | Unified inbox, focus, categories, flags, snooze, conversations, attachments, notifications | None; inherit |
| Tests | wiremock, cache fixtures, fake server, live harness | Copy the pattern; add JMAP fixtures/testcontainer |
| Capability gating | `SieveAdvertised(false)`, Graph-only handles as `None` | Emit JMAP capabilities; J7 adds the extras |

The genuinely new work is concentrated in J1 (the JMAP wire protocol), the JMAP-specific half of
J2 (account-wide state and multi-mailbox semantics), J4 (the JMAP operations behind the session
shell), J5 (submission) and J8 (JMAP fixtures and the Stalwart test). J3, J6 and J9 are
adaptations of patterns that already work.

### The protocol seam, and a second implementation

`AccountCommand` and `AccountEvent` (`source/crates/mail/src/session.rs:355` and `:762`) have 45
commands and 41 events. The app's event loop, message list, reading pane, composer, undo and
outbox UI talk to a mail account only through them. A third implementation only needs the same
**contract** (the two command channels and the event channel), not the same argument list as
`run_account_session` (`session.rs:1320`).

`lookout_graph::mail::session::run_graph_mail_session`
(`source/crates/graph/src/mail/session.rs:115`) is the second implementation, and the template for
the third. It already proves every part of the seam outside IMAP:

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

- `Cache::upsert_messages` (`cache.rs:1163`), the incremental counterpart of
  `replace_messages`.
- `Cache::load_summaries`, `flagged_messages`, `update_flags_many`,
  `update_keywords_many`, `snooze_messages`, `unsnooze_messages`, `purge_expired_snoozed`,
  `next_snooze_wake`, `enqueue_outbox`, `due_outbox`, `next_outbox_not_before` — all public and
  used by the Graph actor.
- The `graph_uids` / `graph_uid_space` tables (`Cache::graph_uids` at `cache.rs:2947`,
  `Cache::graph_uid_validity` at `:2932`): one account-unique synthetic `Uid` per remote message
  id, allocated in one `IMMEDIATE` transaction, reversible with `graph_ids_for` (`:2989`),
  non-allocating for tombstones with `graph_uids_existing` (`:2975`), pruned by
  `prune_graph_uids` (`:3007`).
- `graph_folder_cursors` (`cache.rs:2588`) and its setters: the persisted-cursor pattern, though
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
`plan_folder_change` and `plan_create_mailbox` (`session.rs:5572`, `:5730`, `:5800`) assume IMAP
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
  (`source/crates/app/src/window/connect.rs:1494`).
- The Graph opt-in is the precedent for selecting a transport per account: `uses_graph_mail` is
  computed at connect time (`connect.rs:267`), the branch spawns `run_graph_mail_session`
  (`connect.rs:384`–`:397`), and enabling/disabling tears the session down and drops the
  per-account cache because the two folder/Uid spaces cannot be merged
  (`MAIL_GRAPH_OVERVIEW.md` 5.7.6). JMAP is a manual-account protocol choice rather than a
  Microsoft 365 opt-in, but the same per-account selection and cache-drop rule applies when
  switching protocols.
- `AccountHandle::uses_graph_mail` (`source/crates/app/src/window/state.rs:119`) is used to hide
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

### Features the transport gets for free

These already live above the protocol seam and must not be reimplemented in `lookout-jmap`:

- the unified inbox, folder tree, favorites and per-account settings;
- conversations/thread grouping, sorting, filters and the message list;
- local FTS5 search, search history and refiners (JMAP `Email/query` is secondary);
- categories, follow-up flags, Pin, snooze and the local $Junk hints, where the server supports
  the underlying keywords;
- Focused Inbox (Graph-native for Microsoft 365, local classifier for other accounts);
- attachments, inline images, TNEF, raw `.eml`, print and unsubscribe;
- the local outbox with undo send, scheduled send and offline queuing;
- the existing rules/automatic-replies/Sieve UI shells (J7 supplies the JMAP backends);
- notifications, the reading pane and the composer.

## How Lookout's mail features map onto JMAP

The left column is the feature as it exists today. Because most of it is already transport-neutral,
only the JMAP column is new work.

| Lookout | JMAP | Notes |
|---|---|---|
| Folders, counts, roles | `Mailbox/get`, `Mailbox/changes`, `Mailbox/set`, `role`, `parentId`, `totalEmails` / `unreadEmails` | The server reports counts, so the IMAP STATUS drain isn't needed. `Mailbox/set` supports create, rename, move (`parentId`) and destroy. |
| Envelope sync | `Email/query` + `Email/get`, `Email/changes`, `Email/queryChanges` | `Email/changes` is **account-wide per type**, not per folder. `Email/get` returns `mailboxIds`, so one change can update or remove rows in every folder it belongs to. |
| New-mail push | `StateChange` over EventSource (`eventSourceUrl`), WebSocket (RFC 8887) | Primary delivery. Keep the Graph-style poll loop as the fallback when push is absent or fails. |
| Flags, keywords, snooze, `$Junk`, Pin | patch `keywords` (`$seen`, `$flagged`, `$answered`, `$draft`, `$junk`, `$notjunk`, `$Lookout-*`) | `$flagged` is Pin; `$junk`/`$notjunk` are native; follow-up flags and tags keep the existing `$Lookout-*` atoms. Custom-keyword support varies by server; fall back to local snooze/hints. |
| Move, copy, revert | patch `mailboxIds`; `Email/copy` for a copy; `Email/set` destroy for undoing a copy | An Email id is stable across a move. An Email may be in several mailboxes, so "move" means add the target and remove the source, not replace the whole set (risk 5). |
| Body, attachment, raw message | `Email/get` `bodyStructure` / `bodyValues`; `blobId` + `downloadUrl` | Raw blob is exact RFC 5322 for `parse_body`. Part numbers use the blob id in a filename-safe form, as `graph_part_number` does. |
| Drafts | blob upload + `Email/import` into Drafts with `$draft` | Preserves the compose session's stable `Message-ID` and Bcc. Replace by Message-ID, exactly as the IMAP and Graph paths do. |
| Send, outbox | `EmailSubmission/set` + `onSuccessUpdateEmail` (Drafts → Sent) | Import the draft Email first. The server files the sent copy; `onSuccessUpdateEmail` moves the draft out of Drafts and clears `$draft`. Bcc rides the submission `Envelope`, not a header. The local outbox keeps undo send, scheduled send and offline queuing. `FUTURERELEASE` is optional. |
| Send-as | `Identity/get` | Map into Lookout's identities (read-only sync or send-time validation); local identities remain the composer's source unless the user adopts a server identity. |
| Search | `Email/query` filter | `text`/`from`/`to`/`cc`/`subject`/`body`, `before`/`after`, `minSize`/`maxSize`, `hasAttachment`, `hasKeyword`/`notKeyword`, `inMailbox`. Follow-up is `$Lookout-flagged` and Pin is `$flagged`; multiple categories need an AND `FilterOperator`. Local FTS5 is still primary; `is:mentions` stays local. |
| Threads | `threadId`, plus `references` / `inReplyTo` | Use `jmap:{threadId}` like Graph's `graph:{conversationId}`, or compute from references if IMAP-compatible keys are ever required. |
| Automatic replies | `VacationResponse` | Optional; only when the server advertises the capability. |
| Rules | JMAP for Sieve Scripts (RFC 9661) | Later (J7). Stays hidden until then. |
| Quota | JMAP Quotas extension, when advertised | Later (J7). |
| Focused / Importance | No native JMAP property | Importance is Graph-only and hidden; the local Focused Inbox classifier still works for a JMAP account, as it does for IMAP. |
| Calendars, contacts, tasks | JMAP Calendars / Contacts / no standard tasks | **Out of scope.** Fastmail and Stalwart also serve CalDAV/CardDAV, which `lookout-dav` already handles; Microsoft 365 stays on Graph. |

**Authentication.** Stalwart and Cyrus accept Basic. Fastmail requires an API token (Bearer) or
OAuth for JMAP; its app passwords don't work. Start with Basic + Bearer; OAuth can come later.

**Discovery.** `https://<domain>/.well-known/jmap` returns the Session object (`apiUrl`,
`downloadUrl`, `uploadUrl`, `eventSourceUrl`, `primaryAccounts`, `capabilities`, `state`). Follow
redirects, and allow an explicit session URL for servers that don't publish the well-known
endpoint.

## Proposed decisions (to confirm before J1)

- **JMAP is an additional transport, selected per account.** It does not replace IMAP/SMTP or
  Graph, does not change either default, does not migrate an existing account, and does not
  remove a connection path. A user who never adds a JMAP account sees no change. A user who does
  gets another account beside the existing ones, participating in the same UI.
- **Scope is mail only.** Calendars, contacts and tasks stay on `lookout-dav`/`lookout-graph`.
  JMAP Calendars, Contacts and any task extension are separate future projects, not part of this
  one. This keeps the addition small and avoids a second PIM stack.
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
  carrying copies. Leave `connect_and_run` (`session.rs:1572`) alone. If duplication later
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
  trait or pass a loader into its config. The existing IMAP/SMTP fields remain for IMAP
  accounts, and the protocol choice is additive.
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
- **No new feature UI.** The only user-visible additions are the protocol choice in the manual
  account form, the JMAP-specific fields, and capability-gated extras (J7). Everything else is
  the existing UI talking to another transport.

## Risks

1. **A third transport is a third maintenance surface.** Each protocol adds auth, discovery,
   server quirks, fixtures and a live harness. Mitigations: keep JMAP a manual, opt-in account
   rather than a default; share the session shell and cache; fence optional features behind
   advertised capabilities; keep PIM on DAV/Graph so there is no second calendar stack.
2. **Expectation management.** "JMAP support" can be misread as replacing IMAP or Graph, or as
   fixing every Graph limitation for Microsoft 365. It does neither: Microsoft does not offer
   JMAP, and existing accounts keep their transport. Say "an additional account type" in the
   README, the account form and release notes, and keep JMAP out of the Microsoft 365 path.
3. **The identity model (Graph R4) — largely retired.** The Graph transport already maps remote
   ids onto `Uid`/`UidValidity` and proves it through a fake-server test. The remaining risk is
   the *generalization* (renaming or mirroring the cache tables) and the multi-mailbox case
   below, not the identity type itself. `MailboxId` helpers must strip the account prefix and
   never split on `:` — `other:` and `shared:` account ids contain a colon (Graph R8).
4. **Duplication vs a shared shell.** `connect_and_run` is no longer the critical path, but
   copying the Graph actor creates two shells that can drift. Mitigations: export the few shared
   helpers now (J3), keep the Graph fake-server test green, and give JMAP its own equivalent.
   The optional shell extraction can follow later. The IMAP regression risk of the original J3 is
   removed.
5. **Emails in several mailboxes.** JMAP allows one Email in several mailboxes, unlike Graph's
   single folder. Folder views work as they are, but:
   - a move must add the target and remove the source, not replace the whole `mailboxIds` set
     unless the user means "move everywhere";
   - a change in `mailboxIds` must remove the row from every mailbox the message left;
   - `destroyed` means the Email is gone from all mailboxes;
   - unread counts come from `Mailbox/get`, not from local arithmetic;
   - a unified-inbox merge may show the same message twice if it is in two mailboxes; dedupe by
     `messageId`/`threadId` where the UI expects one row.
6. **Servers differ.** Fastmail is the reference implementation and Stalwart is solid; Cyrus
   needs configuring. Custom keyword support, EventSource/WebSocket availability, submission
   behaviour, vacation and Sieve support all vary. Test against Stalwart in a testcontainer, the
   way the IMAP tests use GreenMail, and keep a Fastmail token check manual.
7. **Push reliability.** EventSource is an advantage only if it is implemented well: long-lived
   HTTP streams through proxies, reconnect/backoff, `ping` handling, session expiry, and a
   polling fallback. Keep the Graph poll loop as a tested fallback, not an afterthought.
8. **Change-state invalidation.** `cannotCalculateChanges` (from `/changes`) and
   `tooManyChanges` (from `Email/queryChanges`) force a full resync. The state is account-wide,
   so one invalid state can resnapshot every folder. The Uid map must survive it (stable
   `UidValidity`), and the cursor must be cleared only after a successful resnapshot, so a failed
   resync cannot skip changes.
9. **Request batching and limits.** Back-references (`#callId/result/path`), out-of-order
   responses, per-method errors, `maxObjectsInGet`/`maxObjectsInSet`, upload sizes and concurrent
   upload limits all need handling in J1. A batch that partially fails must leave the cache and
   cursor consistent.
10. **Submission is a two-step, not a send.** `EmailSubmission/set` needs an existing Email, so
    the outbox drain must upload/import the draft first, then submit it, then file the sent copy
    through `onSuccessUpdateEmail`. Duplicate drafts on retry, a missing Drafts/Sent mailbox,
    identity selection for send-as, the undo window (`undoStatus`) and optional `FUTURERELEASE`
    all need explicit handling in J5. Bcc belongs in the submission `Envelope` (`rcptTo`), not in
    a MIME header — do not copy Graph's `with_bcc_header` workaround.
11. **Body and attachment cost.** `Email/get` with `bodyValues` can return large bodies; the raw
    blob is one download per message. Prefetch (the existing policy) and the FTS body upgrade
    should keep first paint fast. `downloadUrl` is a template and may be session-scoped, so cache
    bytes rather than URLs.
12. **Transport switching is per account, not global.** Switching an account from IMAP to JMAP
    (or back) must drop *that account's* per-account cache, as 5.7.6 does for Graph, because the
    folder/Uid spaces differ. Other accounts and their caches are untouched. The add-account
    dialog must hide the IMAP/SMTP fields for a JMAP account, and the Config row should show the
    JMAP session URL rather than IMAP/SMTP hosts.
13. **Keyword variance.** `$junk`, `$notjunk` and custom `$Lookout-*` keywords are standard-ish
    but server support varies. A server that rejects a custom keyword must degrade to the local
    behaviour (snooze row, local junk hint) rather than fail the command.
14. **Auth and discovery edge cases.** Fastmail app passwords don't work; Basic vs Bearer must be
    explicit. `.well-known` may redirect or be absent; a self-hosted server may need an explicit
    session URL. Never log the token, and require TLS.

## Task breakdown

Effort: **S** ≤ 1 day · **M** ≤ 1 week · **L** multi-week. The "Leverage" column records what
already exists, so the estimate reflects an addition to the current codebase rather than the
original greenfield plan.

| # | Task | Effort | Leverage |
|---|---|---|---|
| J0 | **Spike.** Session discovery, `Mailbox/get`, `Email/query` + `Email/get`, blob download and an EventSource handshake in `lookout-jmap`, tried against Stalwart (in a container) and a Fastmail token. Confirm a downloaded message parses the same through `parse_body`, and confirm the account-wide `Email/changes` model against a move and a multi-mailbox message. | S | Graph's offline/live spike pattern; shared cache and parser |
| J1 | **`lookout-jmap` transport.** Session discovery and capabilities, batched method calls with back-references, `Mailbox` / `Email` / `Identity` / `EmailSubmission` / `VacationResponse` types, blob upload/download, EventSource (WebSocket optional) with a polling fallback, error/limit handling, wiremock tests and fixtures. The mappers must handle JMAP's array-valued `messageId` / `inReplyTo` / `references` and its map-valued `mailboxIds` / `keywords`, which differ from Graph's scalar shapes. | M | Graph HTTP/retry/error patterns; no JMAP wire code yet |
| J2 | **Identity and state plumbing.** Generalize or mirror the Graph Uid map (`remote_uids` / `remote_uid_space`, or `jmap_*`), add account-wide `Email`/`Mailbox` state persistence, JMAP `MailboxId`/part-number helpers, and `Cache::mailboxes_for_uid` if needed. Write up in SCAFFOLD.md §3/§4. | S–M | `graph_uids`/`graph_uid_space`, `MailboxId` helpers, cache; account-wide states are new |
| J3 | **Shared helpers.** Export `cache_op`, `outbox_retry_delay` and `MAX_OUTBOX_ATTEMPTS` from `lookout-mail`; switch `lookout-graph` to them. No IMAP changes. Optional later: extract the transport-neutral session shell, with the Graph fake-server test as the net. | S | Copies already exist in IMAP and Graph; this is an export/switch |
| J4 | **`run_jmap_account_session`.** Folder and account-level envelope sync, EventSource/poll loop, body/attachment/raw fetch, keywords and Pin, junk, follow-up and tags, move/copy/revert, folder create/rename/move/delete, snooze, drafts, search. Use the Graph actor as the template. | L | Working Graph session shell; JMAP operations are the new part |
| J5 | **Sending.** `Identity/get` and identity selection, draft import, `EmailSubmission/set` with `onSuccessUpdateEmail`, Sent filing, and the local outbox drain (undo send, scheduled send, offline queue). Build the submission `Envelope` from the outbox recipients, with Bcc in `rcptTo` rather than a header (unlike Graph's `with_bcc_header` workaround). `FUTURERELEASE` optional. | M | Local outbox, MIME builders and Graph drain pattern; submission is new |
| J6 | **App wiring.** `OtherAccount.protocol`, the `PROTOCOL_JMAP` keyring slot, a protocol choice in the add-account dialog (address, username, password or token, optional session URL), discovery/validation, the branch in `connect_other_account`, the Config row, hiding IMAP-only fields, and dropping the per-account cache on a protocol switch. | S–M | `OtherAccount`, keyring, `connect_other_account`, Graph's per-account selection pattern |
| J7 | **Extras, each only when the server offers it.** `VacationResponse` for automatic replies, JMAP Sieve for rules, quota. Reuse the existing Sieve/automatic-replies UI where possible. | M | Existing Sieve/automatic-replies UI; JMAP bindings are new and optional |
| J8 | **Tests.** A Stalwart testcontainer integration test in `source/crates/jmap/tests/`, parallel to `source/crates/mail/tests/imap_integration.rs`; a fake-server session test adapted from `graph_mail_session.rs`; cache tests adapted from `graph_mail_cache.rs`; and fixtures under `source/test-fixtures/jmap/`. | M | Three-level Graph test pattern; JMAP fixtures/testcontainer are new |
| J9 | **Docs.** README, SCAFFOLD.md §3/§4, this file. CHANGELOG and version only when asked. Add OUTLOOK_PARITY.md rows only if JMAP changes a parity statement. | S | Existing docs structure |

**Order:** J0 → J1 ∥ J2 → J3 → J4 → J5 → J6 → J7 ∥ J8 → J9.

**Rough total:** 4–6 weeks of focused work, including J7 at the upper end when the server-specific
extras are in the same push (J7 alone is roughly a week). The 2026-10-22 estimate was 5–7 weeks;
the reduction reflects the Graph transport and the transport-neutral features that have landed
since, not a claim that the JMAP wire protocol is small. J4 remains the bulk and the main unknown.
If EventSource, batching or submission prove awkward, add a week.

Users see no change from J0–J3. J6 adds an option to the manual-account form; existing IMAP and
Graph accounts keep their transport and their cache. Nothing about the default connection path
changes.

## Verification checklist

### Additive behaviour

- [ ] Adding a JMAP account leaves every existing IMAP, Graph and DAV account untouched: same
      transport, same settings, same cache, no migration and no default change.
- [ ] A JMAP account coexists with IMAP and Graph accounts in the folder pane, unified inbox,
      search, notifications and per-account settings.
- [ ] Calendars, contacts and tasks still come from `lookout-dav`/`lookout-graph`; no JMAP PIM
      code or UI is added.
- [ ] The manual-account form offers JMAP as an additional protocol choice; existing IMAP
      accounts load and edit exactly as before.
- [ ] Switching one account's protocol clears only that account's cache (the 5.7.6 rule); other
      accounts are unaffected.
- [ ] No JMAP account is routed through the Microsoft 365 Graph path, and no Graph account is
      presented as a JMAP candidate.

### Protocol and feature behaviour

- [ ] Commands and events go through the existing `AccountCommand` / `AccountEvent` contract,
      and the UI never calls the server directly.
- [ ] JMAP property names and capability URNs live in `lookout-jmap`, never in feature code.
- [ ] Every cache format change is noted in the CHANGELOG ("no data-format change" otherwise).
- [ ] A capability the server lacks hides its UI element or falls back to the client. It never
      shows an error.
- [ ] Fixtures under `source/test-fixtures/jmap/` for every wire shape.
- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`
      are clean.
- [ ] `cargo test -p lookout-jmap` (wiremock; run the Stalwart JMAP integration test explicitly,
      as with the IMAP one) and `cargo test -p lookout-mail` pass; `cargo test -p lookout-graph`
      still passes after J3; the IMAP integration test is unchanged.
- [ ] A moved Email keeps its synthetic Uid; an Email in several mailboxes has one Uid and one
      row per mailbox; a `destroyed` id doesn't burn a new Uid.
- [ ] `cannotCalculateChanges` / `tooManyChanges` resyncs under the same Uids; a lost Uid map
      bumps `UidValidity`, and the state is only cleared after a successful resnapshot.
- [ ] A message that leaves a folder is removed from that folder's cache when `Email/changes`
      reports the update.
- [ ] Manual check with a Stalwart or Fastmail account: folders, list, open, move and undo,
      flag and Pin, snooze, junk, draft, send with undo send, search, and new mail arriving by
      push within seconds. Repeat once with push disabled to confirm the polling fallback, and
      once with an IMAP account and a Graph account connected to confirm the three transports
      coexist in the unified inbox and search.
