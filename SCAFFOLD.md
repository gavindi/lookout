# Lookout — Architectural Scaffold

This document describes how Lookout is put together: the crate boundaries, the actor/event
architecture that talks to IMAP and CalDAV/CardDAV servers, the GTK4/libadwaita UI layer built on
top of it, and the important data structures and pipelines that connect them. It's a map for
navigating the codebase, not a tutorial — read it alongside the source, not instead of it.

Line numbers cited below are accurate as of the time of writing but will drift as the code
changes; treat them as a starting point for `grep`, not a guarantee.

## 1. What Lookout is

A native GNOME mail client written in Rust (GTK4 + libadwaita + WebKitGTK), positioned as a
reimplementation of Outlook's desktop experience: Mail (IMAP/SMTP), Calendar (CalDAV), People
(CardDAV), Tasks (CalDAV VTODO + Google Tasks), and a local-data AI assistant ("Lookout" tab).
Accounts are sourced from **GNOME Online Accounts** (GOA) wherever possible — Lookout holds no
credentials of its own for GOA accounts, only fetching short-lived tokens/passwords from GOA over
D-Bus on each (re)connect. Manually-configured ("other") IMAP/SMTP accounts are supported
alongside GOA ones, with secrets held in the GNOME keyring.

## 2. Workspace layout and crate dependency graph

```
source/crates/
├── core/         lookout-core   — pure domain types, zero I/O
├── goa/          lookout-goa    — GNOME Online Accounts discovery (zbus/D-Bus)
├── mail/         lookout-mail   — IMAP/SMTP account-session actor + SQLite cache
├── dav/          lookout-dav    — CalDAV/CardDAV client + iCalendar/RRULE + SQLite caches
├── imap-proto/   vendored fork of `imap-proto` (one upstream fix, patched via [patch.crates-io])
├── async-imap/   vendored fork of `async-imap` (in-place compress upgrade, buffer perf fix)
└── app/          lookout-app    — the `lookout` binary: GTK4/libadwaita UI
```

Dependency direction is strictly one-way and fan-out from `core`:

```
lookout-core  (no I/O deps at all — tokio-free, zbus-free, gtk-free)
   ↑     ↑
   |     └── lookout-dav   (reqwest, quick-xml, icalendar, rrule, rusqlite)
   └── lookout-mail        (async-imap[patched], lettre, mail-parser, rusqlite)
                 ↑              ↑            ↑
                 |              |            |
             lookout-goa    lookout-dav   lookout-mail
                 \              |            /
                  \             |           /
                        lookout-app (gtk4, libadwaita, webkit6, zbus, ksni, secret-service)
```

Both `lookout-mail` and `lookout-dav` are explicitly documented as having **no dependency on
`lookout-goa`/`zbus`** — each takes credentials through a small trait (`CredentialProvider` /
`CalendarCredentialProvider`) instead, so both crates are unit-testable (and, for `lookout-mail`,
integration-testable against a real IMAP server via `tests/imap_integration.rs`) without any
GNOME/D-Bus session present. `lookout-app` is the only crate that touches GOA, GTK, or D-Bus tray
integration — it is the composition root.

**The mail and calendar engines mirror each other by design.** `lookout-dav`'s own crate doc
comment calls itself a client "mirroring `lookout-mail`'s IMAP session actor," and this mirroring
shows up everywhere: both expose a `Command`/`Event` channel-pair actor per account, both cache
disposable, server-re-fetchable data in a per-account SQLite file under `$XDG_CACHE_HOME`, both
treat their cache as a non-authoritative fast-paint hint (not source of truth), and both take
credentials through a trait rather than a concrete GOA dependency.

## 3. Domain model — `lookout-core`

Zero I/O dependencies by design (`lib.rs:1-4`) so it can be exercised with plain `cargo test` and
reused by any future front end. Eight modules, all re-exported flat from `lib.rs`.

### 3.1 Opaque identifiers (`ids.rs`)

Neither IMAP nor CalDAV expose durable opaque object ids, so Lookout manufactures its own
composite string keys:

| Type | Shape | Notes |
|---|---|---|
| `AccountId(String)` | GOA D-Bus object path, or `other:<uuid>` | |
| `Uid(u32)` | IMAP UID | Unique only within `(MailboxId, UidValidity)` |
| `UidValidity(u32)` | IMAP UIDVALIDITY | A change invalidates every cached UID for that mailbox |
| `MailboxId(String)` | `"{account_id}:{folder_path}"` | Synthesized — IMAP has no folder id |
| `CalendarId(String)` | `"{account_id}:{calendar_href}"` | Same synthesis pattern for CalDAV |
| `EventUid(String)` | iCalendar `UID` | Unique only within a `CalendarId` |
| `TaskUid(String)` | VTODO `UID` | Same scoping as `EventUid` |

### 3.2 Mail types (`email.rs`, `mailbox.rs`)

- **`EmailSummary`** — the list-row "weight" projection: cheap to fetch (`ENVELOPE` + `FLAGS` +
  `RFC822.SIZE` + `BODYSTRUCTURE`, no body) and cheap to cache. Carries `uid`, `mailbox`,
  `message_id`/`in_reply_to`/`references` (threading inputs), `thread_key: ThreadKey` (precomputed
  at sync time), `subject`, `from`/`to`/`cc`, `date`, `flags: BTreeSet<SystemFlagBit>`,
  `keywords: BTreeSet<String>`, `size`, `has_attachment`, `has_calendar`, `preview: Option<String>`,
  and `structure: Option<Vec<BodyPart>>` (the BODYSTRUCTURE walk, reused later to drive a
  partial-fetch body request). `Serialize`/`Deserialize` — this is the JSON payload stored in the
  cache's `messages` table.
- **`EmailBody`** — the fetched-on-open renderable body: `text_body`/`html_body`, `calendar_ics`
  (iMIP payload), `parts: Vec<BodyPart>` (attachment/inline-image metadata only — text parts are
  already merged into `text_body`/`html_body`), `headers`, `auth_results`. Also JSON-serialized,
  as the `bodies` table's BLOB payload.
- **`BodyPart`** — one MIME leaf: `part_number` (IMAP section path, e.g. `"1.2"`), `content_type`,
  `charset`, `transfer_encoding`, `filename`, `cid` (for `cid:` resolution), `size`,
  `is_attachment`.
- **`SystemFlagBit`** — `Seen | Answered | Flagged | Deleted | Draft` (RFC 3501 §2.3.2), excluding
  `\Recent` deliberately (session-scoped, not cacheable).
- **Color tags** are implemented as IMAP custom keywords: `$Lookout-tag-<key>`
  (`TAG_KEYWORD_PREFIX`), following the `$`-prefix convention other clients use for custom
  keywords. `tag_keyword`/`tag_key_from_keyword`/`sanitize_tag_key` round-trip between the two.
- **`Mailbox`** — `id`, `account_id`, `name`, `parent: Option<MailboxId>`, `delimiter`,
  `role: MailboxRole`, `uidvalidity`, `uidnext`, `highest_modseq: Option<u64>` (CONDSTORE/QRESYNC
  only), `total`/`unread`, `flags`, `subscribed`. `MailboxRole` (`Inbox|Sent|Drafts|Trash|Junk|
  Archive|Custom`) is resolved from IMAP SPECIAL-USE (RFC 6154) with a name-guessing fallback for
  servers that don't advertise it.
- Auth-results types (`SpfResult`/`DkimResult`/`DmarcResult`/`AuthenticationResults`), read-receipt
  parsing (`parse_disposition_notification_to`, RFC 8098), List-Unsubscribe parsing
  (`ListUnsubscribe`, RFC 2369/8058), and iMIP method/invitation parsing (`ImipMethod`,
  `ImipInvitation`, RFC 5546) round out this module.

### 3.3 Calendar types (`calendar.rs`)

- **`CalendarEvent`** — a VEVENT *master* (not an expansion): `uid`, `calendar_id`,
  `summary`/`description`/`location`, `start`/`end`, `all_day`, `rrule: Option<String>` (kept
  *raw* — RRULE parsing lives in `lookout-dav`, not here), `recurrence_id`/`recurrence_range`
  (override semantics), `exdates`/`rdates`, `href`/`etag` (write targets — `etag` becomes an
  `If-Match` precondition), `attendees`, `organizer`, `categories`, `sensitivity`,
  `transparency`, `reminder_minutes_before`, `conference_url`.
- **`EventOccurrence`** — the thinner, renderable expansion of one RRULE instance (or a
  non-recurring event as-is), carrying `master_start`/`master_end`/`master_href`/`master_etag` so
  the editor can open the true series master without a re-lookup.
- **`CalendarTask`** — the VTODO mirror of `CalendarEvent`; no recurrence modeled.
- **`WebcalSubscription`** — a fetch-only feed: `{ id, display_name, url }`, surfaced as a
  synthetic `CalendarId("webcal:<id>")`.

### 3.4 Threading algorithm (`thread.rs`)

`compute_thread_keys` builds a **union-find (disjoint-set)** over normalized Message-IDs —
deliberately not a full JWZ container tree, since the UI only needs a flat grouping key, not a
reply hierarchy:

1. For each message, build its ancestor chain from `References` (oldest-first), falling back to a
   single-element `In-Reply-To` chain.
2. Union every adjacent pair in the chain, then union the chain's last id with the message's own
   Message-ID.
3. Within each connected component, pick the canonical `ThreadKey`: the id that's referenced by
   another message but has no ancestor chain of its own (the "true root"); if that root isn't in
   the fetched set (it lives in another mailbox), fall back to the earliest-dated message in the
   component — keeping the key stable across re-fetches.
4. A message with no Message-ID at all falls back to `"subject:<normalized>"` (stripping recursive
   `Re:`/`Fwd:`/`Fw:` prefixes).

`ThreadKey` is computed once at sync time and persisted on `EmailSummary`; `message_list.rs` in
the app crate does the *grouping* (not the key computation) when threaded view is on.

### 3.5 Trust model (`trust.rs`)

Decides reading-pane *display* behavior (a banner), not a security boundary — WebKit's own
`decide-policy` handler is the actual blocker. `TrustLevel::Images` (default) allows only
`image/*`; `TrustLevel::AllContent` also allows stylesheets/fonts/media (link-click/iframe vetoes
and JS-disable still apply regardless). `sender_matches_trust_entry` supports exact
(`name@example.com`) and whole-domain (`@example.com`) entries. `html_remote_content_scan` scans
decoded HTML for `http(s)://` refs behind a subresource marker (`src=`, `srcset=`, CSS `url()`,
`@import`, `poster=`, `<link href>`), classifying by extension into images vs. other — plain
`<a href>` navigation is excluded, and `cid:`/`data:`/relative URLs never match by construction.

### 3.6 Contacts (`vcard.rs`)

A hand-rolled RFC 6350/2426 vCard parser/serializer accepting both vCard 3.0 (Google's CardDAV
export) and 4.0. `VCard::parse_all` isolates one card's parse failure from the rest of a
multi-card `.vcf` document. `Birthday { date, omit_year }` distinguishes a recurring yearless
birthday (`BDAY:--MMDD` or Apple's `X-APPLE-OMIT-YEAR`) from a one-time date.

### 3.7 Identities and signatures (`identity.rs`, `signature.rs`)

`Identity` is purely local SMTP `MAIL FROM` sending-identity config (name, email, reply-to, bcc,
text/HTML signature) — no server-stored counterpart, unlike e.g. JMAP identities. `Signature` is
a user-authored rich-text signature, stored globally (not pinned to one account) in `AppConfig`.

### Serde convention

Nearly every domain type derives `Serialize`/`Deserialize`. `EmailSummary` and `EmailBody` are the
two types actually persisted as JSON (in `lookout-mail`'s cache); their many `#[serde(default)]`
fields exist specifically so old cached JSON rows keep deserializing as the schema grows.
`TrustLevel` is the notable exception — encoded manually as `i64` rather than through serde.

## 4. Mail engine — `lookout-mail`

Crate doc: "IMAP/SMTP account-session actor and local cache." One
`session::run_account_session` future runs per connected account, driven by `AccountCommand`s and
emitting `AccountEvent`s over `async_channel`.

### 4.1 The `AccountCommand` / `AccountEvent` protocol

This channel pair is the seam between the mail engine and the UI — the app crate never touches
IMAP directly, only sends commands and reacts to events.

**`AccountCommand`** (`session.rs`) — every variant: `SyncMailbox`, `FetchBody`,
`FetchAttachment`, `FetchRawMessage`, `Refresh`, `SendMessage(Box<ComposedMessage>)`, `Reconnect`,
`SetPrefetchPolicy`, `MoveMessage`/`MoveMessages`/`MoveMessagesTo`, `EmptyMailbox`,
`SnoozeMessage`/`SnoozeMessages`, `StoreFlags`/`StoreFlagsMany`, `StoreKeywords`/
`StoreKeywordsMany`, `PrefetchBodies`, `SaveDraft`/`DeleteDraft`, `SearchMailbox`, `Shutdown`.

Two command channels feed one actor: a background one and an `interactive_commands` one — the
latter carries user-facing fetches (`FetchBody`/`FetchAttachment`/`FetchRawMessage`) and is
drained *first* on every wake, so a click never queues behind a sync/search/move already in
flight.

**`AccountEvent`** — every variant: `ConnectionStateChanged`, `FoldersUpdated`, `MessagesUpdated`,
`MailboxSyncStarted`, `PrefetchStarted`/`PrefetchFinished`, `NewMessages`, `PreviewsFetched`,
`BodyFetched`, `PartFetched`/`PartFetchFailed`, `RawMessageFetched`/`RawMessageFetchFailed`,
`SendCompleted`/`SendFailed`, `MoveFailed`, `StoreFlagsFailed`, `DraftSaved`, `MessageMoved`,
`MessageSnoozed`, `MailboxExpunged`, `SearchResults`, `Error`.

### 4.2 The actor loop

- **`run_account_session`** — entry point: opens the cache, does a **fast first paint** from
  cached folders/INBOX messages *before connecting at all*, fires-and-forgets a one-time FTS
  backfill and cache maintenance (`Cache::run_maintenance`) onto the blocking pool (unawaited —
  must not delay login), then loops calling `connect_and_run` with **exponential backoff** (1s
  doubling to a 60s cap) on error. A command that arrives while disconnected is *carried* into the
  next connection attempt rather than dropped.
- **`connect_and_run`** (~1700 lines, the actor's core) — a plain `async fn` driven by
  `tokio::select!`, not a raw OS thread:
  1. Connect + login (TLS via `rustls`, `XOAUTH2` or password auth), then negotiate capabilities:
     `MOVE`, `LIST-STATUS`, `UIDPLUS`, `CONDSTORE`/`QRESYNC` (via `ENABLE`), `COMPRESS=DEFLATE`.
     Every capability is a soft downgrade on rejection, never fatal.
  2. Initial folder list + INBOX sync.
  3. **Main loop**: checks for an already-queued command *before* entering IDLE (avoids paying two
     round trips for work already in hand); otherwise enters IMAP IDLE with a timeout slice
     (25 min normally, or the prefetch policy's `batch_interval` — default 30s — when aggressive
     prefetch is on), racing the IDLE wait against both command channels via `tokio::select!`. A
     server push (EXISTS/EXPUNGE/etc.) re-runs `sync_mailbox` on the current folder; a command
     dispatches through one large `match` over every `AccountCommand` variant, draining both
     channels to empty before returning to IDLE so a burst of commands is serviced in one pass.
     A cooperative folder-count STATUS drain and the background body-prefetch batch (§4.3) run at
     the tail of each iteration, each yielding back to command-dispatch if anything is queued.

- **`sync_mailbox`** — the shared full/delta envelope sync (called at connect, on IDLE wake, on
  `Refresh`/`SyncMailbox`, and after mutations). Three phases:
  1. **Membership + flags** — skips its own SELECT if already on this folder. With CONDSTORE and a
     known `highest_modseq` baseline, does a `FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE
     <baseline>)` delta (RFC 7162) instead of a full flag fetch; membership comes from `UID SEARCH
     ALL` or, with QRESYNC, from `VANISHED` reporting on `SELECT (QRESYNC ...)`.
  2. **New-arrival envelope fetch** — one `UID FETCH` for uncached UIDs only, chunked to a 40 KiB
     command-line budget. Emits `AccountEvent::NewMessages` for the newly-arrived unread subset
     (never on first sync).
  3. Thread-key recompute only when membership/Message-IDs actually changed — a steady-state delta
     wake skips the O(mailbox) union-find pass entirely.
  - **Inline body fetch**: up to 25 newest new messages (under a 2 MiB cap) get their bodies
    fetched right inside the sync, so a brand-new email is cache-warm the moment it's visible
    rather than waiting for the prefetch pass.
  - **Preview fill-in**: up to 50 missing snippets per sync, `BODY.PEEK[]<0,16384>` byte-prefix
    fetches, emitted as a narrow `PreviewsFetched` patch rather than a full `MessagesUpdated`.

### 4.3 Background body-prefetch

`PrefetchState` tracks `mailboxes` (processing order, INBOX excluded), a `current` index,
`pending_uids`, and structures learned during the envelope pass (so body fetches can request
text-parts-only). `PrefetchPolicy` (live-updatable via `SetPrefetchPolicy`) governs `aggressive`
(off = cooperative default), `batch_interval` (default 30s), `folder_limit` (default 200
newest/folder), `batch_size` (default 1 body/round-trip), `refresh_interval` (default 1h before a
finished pass restarts). Pacing is entirely cooperative: every phase checks whether a command is
queued before issuing an IMAP round trip and yields back to the main loop if so — one command in
flight is the worst-case delay a user action ever queues behind, since the connection is
one-command-at-a-time. A `Refresh` extends the current prefetch's mailbox list rather than
resetting it wholesale, so a large in-progress folder is never restarted from zero.

### 4.4 Message-body assembly pipeline

Two paths converge on the same `EmailBody`:

1. **Partial-fetch path** (normal — when `EmailSummary.structure` is known): one `UID FETCH` for
   `BODY.PEEK[HEADER]` plus `BODY.PEEK[<n>]` for each text/plain, text/html, and text/calendar
   leaf by IMAP section path; `body::assemble_body_from_parts` decodes each part (reusing
   `mail_parser` for charset/transfer-decoding) and applies `multipart/alternative` semantics to
   pick real vs. fallback text.
2. **Whole-message fallback** (no known structure, or the partial fetch yielded no text): a
   `BODY.PEEK[]` whole-message fetch, run through `mail_parser::MessageParser`, computing each
   attachment's IMAP-equivalent section path itself so a later on-demand attachment fetch can
   still target it correctly.

Either way, the result is persisted via `Cache::store_body`/`store_bodies`, which also
re-indexes the message in the FTS5 index with the full decoded text (upgrading from a
preview-only index row). Attachment bytes proper never flow through this pipeline — they're a
separate on-demand path (`FetchAttachment`) caching to a flat file.

### 4.5 Local cache — SQLite schema (`cache.rs`)

Per-account file at `$XDG_CACHE_HOME/lookout/mail/<sanitized-account-id>.sqlite3`, WAL mode,
`synchronous=NORMAL` (data is disposable/rebuildable from the server):

```sql
CREATE TABLE mailboxes (mailbox_id TEXT PRIMARY KEY, account_id TEXT NOT NULL, data TEXT NOT NULL);

CREATE TABLE messages (
    mailbox_id TEXT NOT NULL, uid INTEGER NOT NULL, uidvalidity INTEGER NOT NULL,
    data TEXT NOT NULL, PRIMARY KEY (mailbox_id, uid)
);
CREATE INDEX messages_by_mailbox ON messages (mailbox_id);

CREATE TABLE snoozed (
    mailbox_id TEXT NOT NULL, uid INTEGER NOT NULL, snoozed_until INTEGER NOT NULL,
    PRIMARY KEY (mailbox_id, uid)
);

CREATE TABLE bodies (
    mailbox_id TEXT NOT NULL, uid INTEGER NOT NULL, uidvalidity INTEGER NOT NULL,
    data BLOB NOT NULL, PRIMARY KEY (mailbox_id, uid)
);
CREATE INDEX bodies_by_mailbox ON bodies (mailbox_id);

CREATE TABLE addresses (
    address TEXT PRIMARY KEY, name TEXT, seen_count INTEGER NOT NULL, last_seen INTEGER NOT NULL
);
CREATE INDEX addresses_by_count ON addresses (seen_count DESC);

CREATE VIRTUAL TABLE search_fts USING fts5(
    mailbox_id UNINDEXED, uid UNINDEXED, subject, sender, recipients, body,
    tokenize = 'unicode61 remove_diacritics 2'
);
```

- `mailboxes.data`/`messages.data` are JSON text (`Mailbox`/`EmailSummary`); `bodies.data` is a
  JSON BLOB (`EmailBody`).
- `addresses` is a deliberately cumulative, capped (20,000 rows) address book feeding the
  composer's recipient autocomplete — entries outlive the envelopes that introduced them; least-
  contacted/oldest rows are pruned once over the cap.
- `search_fts` is populated by delete-then-insert (FTS5 has no UPDATE) from `replace_messages`
  (envelope sync) and `store_body`/`store_bodies` (which upgrade the indexed text from
  preview-only to the full decoded body, capped at 256 KiB). Queries are sanitized so every bare
  word becomes a quoted phrase, ANDed — neutralizing FTS5 query-syntax injection.
- Schema versioning via `PRAGMA user_version` triggers a one-time full wipe of `messages` or
  `bodies` on a format change (e.g. `EmailBody`'s JSON shape changing).
- Periodic maintenance (`run_maintenance`, called once per session start on the blocking pool):
  sweeps orphaned flat files with no matching `messages` row, migrates to
  `auto_vacuum=INCREMENTAL` on first run (a one-time full `VACUUM`), and runs `PRAGMA
  incremental_vacuum` on every run afterward to reclaim freed pages cheaply.

**Flat-file storage** (outside SQLite, under the same cache root): attachments at
`attachments/<account>/<mailbox-hash>-<uidvalidity>-<uid>-<part>.bin`; raw `.eml` exports at
`messages/<account>/<mailbox-hash>-<uidvalidity>-<uid>.eml`. Both written atomically (temp file +
rename). A purge sweep removes both stores' files for expunged `(uid, uidvalidity)` pairs.

### 4.6 Outgoing mail (`send.rs`)

SMTP via `lettre` (`AsyncSmtpTransport<Tokio1Executor>`), MIME building via
`mail_builder::MessageBuilder`. `build_raw_message` assembles raw RFC 5322 bytes from a
`ComposedMessage`, branching into `multipart/report` for read receipts (RFC 8098 §3),
`multipart/alternative` with a `text/calendar;method=...` part for iMIP invites (RFC 6047 §3.3),
or plain text/HTML `multipart/alternative` + attachments otherwise. Port 465 → implicit TLS;
anything else → STARTTLS (chosen by port, since GOA's TLS/SSL flags are ambiguous for some
providers). Drafts are handled in `session.rs`, not `send.rs`: `SaveDraft`/`DeleteDraft` append to
(and, on replace, first delete) the Drafts mailbox, keyed by a stable per-compose-session
Message-ID.

### 4.7 Auth and connection (`auth.rs`, `connection.rs`)

`Credential::Password(String) | OAuth2AccessToken(String)`, fetched fresh from the caller's
`CredentialProvider` on every (re)connect — never cached in this crate. OAuth2 uses SASL `XOAUTH2`
(`XOAuth2Authenticator`). TLS is built from OS-native root certs (`rustls_native_certs`, skipping
individual bad certs rather than failing the whole store) with TCP keepalive tuned specifically to
detect a silently-dropped connection during a long IDLE wait.

## 5. Calendar/contacts engine — `lookout-dav`

Crate doc: "Minimal CalDAV client + iCalendar/RRULE parsing + a `CalendarSession` actor, mirroring
`lookout-mail`'s IMAP session actor." Same credential-provider-trait convention as `lookout-mail`.

### 5.1 `DavClient` — the low-level HTTP layer (`client.rs`)

Holds no credentials — every request takes a fresh `&Credential`. Auth is plain HTTP headers per
request (`basic_auth` for `Password`, `bearer_auth` for `OAuth2AccessToken`) — CalDAV's actual auth
mechanism, unlike IMAP's SASL-inside-AUTHENTICATE. Implements: PROPFIND/REPORT/`sync-collection`,
calendar-home/addressbook-home discovery (RFC 4791/6352), `list_calendars`/`list_addressbooks`,
`fetch_events_in_range` (a `calendar-query` REPORT), `fetch_tasks` (a `todo-query` REPORT, no
time-range filter — tasks may carry only DUE, only DTSTART, or neither),
`sync_addressbook`/`sync_addressbook_delta` (RFC 6578 incremental sync, 404 responses mapped to
deletions), a full-fetch fallback via `addressbook-multiget` (deliberately not `sync-collection`
nor an empty-filter `addressbook-query` — Google CardDAV treats an empty filter as "match
nothing"), and `put`/`delete` for both calendar objects and vCards (etag-guarded: `If-Match` on
update, `If-None-Match: *` on create). A free function `fetch_webcal_ics` does an unauthenticated
GET for public `.ics` documents, capped at 5 MiB.

### 5.2 `CalendarSession` — the actor (`session.rs`)

CalDAV has no IMAP-IDLE equivalent, so this is a **fixed-interval poll** (5 minutes) rather than a
long-poll.

**`CalendarCommand`**: `SyncMonth`, `FetchMonth`, `Refresh`, `Reconnect`,
`CreateEvent`/`UpdateEvent`/`DeleteEvent`, `SyncTasks`, `CreateTask`/`UpdateTask`/`DeleteTask`,
`Shutdown`.

**`CalendarSessionEvent`**: `ConnectionStateChanged`, `CalendarsUpdated`, `OccurrencesUpdated`,
`TasksUpdated`, `Error`, and `EventSaveFailed { uid, recurrence_id, message }` — kept distinct from
plain `Error` so the UI can roll back exactly the occurrence it moved, explicitly mirroring
`lookout_mail::AccountEvent::MoveFailed`.

Loop structure mirrors `lookout-mail`'s: an outer reconnect-with-backoff loop (1s doubling to 60s)
calling an inner `connect_and_run` that discovers calendars, does an initial sync, then loops
emitting `Idle`/`Busy` and racing `sleep(POLL_INTERVAL)` against the command channel via
`tokio::select!`. Per-calendar fetch failures are logged and skipped, not fatal — contrasted
explicitly against `lookout-mail`'s single-mailbox design, since one CalDAV account can have many
calendars.

`sync_month` fetches a ±7-day padded window (so cross-boundary recurring/multi-day events are
caught) then clips to the exact month; groups fetched VEVENTs by UID into masters vs.
RECURRENCE-ID overrides and hands them to `recurrence::expand_master_with_overrides`.
`write_event`/`write_task` serialize via `ical::build_vcalendar`/`build_vtodo_calendar`, PUT with
an etag precondition, and resync the affected month(s) on success or emit `EventSaveFailed` on
failure.

### 5.3 iCalendar parsing (`ical.rs`) and RRULE expansion (`recurrence.rs`)

Built on the `icalendar` crate rather than a hand-rolled parser. `convert_event`/`convert_todo`
map `icalendar::Event`/`Todo` onto `lookout_core::CalendarEvent`/`CalendarTask`. RRULE strings are
kept **raw** on `CalendarEvent.rrule` — no expansion happens in `ical.rs`; `recurrence.rs` (built
on the `rrule` crate) is the sole consumer, invoked from both `session.rs::sync_month` and
`subscription.rs::sync_feed`. `expand_master_with_overrides` merges a recurring master with its
`RECURRENCE-ID` siblings so an override doesn't double-render next to the master instance it
replaces; `RANGE=THISANDFUTURE` overrides replace their anchor and every later instance.
Timezone resolution falls back through `chrono_tz` then, for unrecognized TZIDs (Outlook/Exchange
stamping Windows zone names like `"W. Europe Standard Time"`), a CLDR-sourced Windows→IANA lookup
table in `tzmap.rs`.

### 5.4 XML handling (`xml.rs`)

A hand-rolled streaming parser over `quick_xml`'s namespace-resolving reader — not a DOM/serde
deserializer. Props are keyed by *resolved namespace URI*, not raw prefix, so nonstandard-prefix
servers parse correctly. Request bodies (PROPFIND/`calendar-query`/`todo-query`/
`addressbook-multiget`/`sync-collection`) are built as plain string templates.

### 5.5 Local caches (`cache.rs`) — two independent schemas

Both per-account SQLite files under `$XDG_CACHE_HOME/lookout/{calendar,contacts}/`, explicitly
documented as the CalDAV mirror of `lookout-mail::Cache` — same non-authoritative fast-paint
philosophy, with one key exception:

```sql
-- CalendarCache
CREATE TABLE occurrences (month TEXT PRIMARY KEY, data TEXT NOT NULL);  -- JSON array of EventOccurrence
CREATE TABLE tasks (data TEXT NOT NULL);                                 -- single-row JSON array of CalendarTask

-- ContactsCache
CREATE TABLE address_books (href TEXT PRIMARY KEY, displayname TEXT NOT NULL, sync_token TEXT);
CREATE TABLE contacts (href TEXT PRIMARY KEY, book_href TEXT NOT NULL, etag TEXT, card TEXT NOT NULL);
```

`ContactsCache` is the one DAV cache that **is** authoritative for something: it persists each
address book's RFC 6578 `sync_token`, so polls can run incremental `sync-collection` REPORTs, and
it's the baseline the People screen's "Deleted" bucket diffs against across restarts.
`CalendarCache` is a pure fast-paint hint, always superseded by the next live sync.

### 5.6 Synthesized/derived calendars

- **`birthdays.rs`** — pure transform, no I/O: turns already-synced `ContactRecord`s into
  all-day `EventOccurrence`s (one per contact with a usable name + birthday), stamped with a
  synthetic `CalendarId` and a deterministic UID (`birthday:<account_id>:<contact-href-tail>`) so
  existing calendar-id-keyed UI mechanisms work unchanged. Age omitted when `Birthday::omit_year`
  is set; Feb 29 shifts to Feb 28 in non-leap years.
- **`subscription.rs`** — the fetch-only cousin of `run_calendar_session` for read-only webcal
  (ICS URL) feeds: same 5-minute poll cadence, but no auth, no discovery, no write commands, no
  reconnect/backoff (a failed fetch just keeps the last-good cached occurrences and sets a
  per-feed `error`, retried on the next poll), and no `sync-collection`/etag tracking — every poll
  is a full re-GET-and-reparse bounded by the 5 MiB cap.

## 6. Account discovery — `lookout-goa`

Talks to GNOME Online Accounts over the session D-Bus (`org.gnome.OnlineAccounts`) via `zbus`
rather than linking `libgoa-1.0` (no maintained Rust GIR binding exists). **Never caches
credentials** — passwords and OAuth2 tokens are fetched fresh from GOA per connection attempt and
held only in memory for the session. `GoaClient::list_mail_accounts`/`list_calendar_accounts`/
`list_contacts_accounts` return `GoaMailAccount`/`GoaCalendarAccount`/`GoaContactsAccount`, each
carrying an `AuthMethod`/`CalendarAuthMethod`/`ContactsAuthMethod` (`OAuth2` or
`Password { <slot-id> }`) describing *how* to ask GOA for a credential, not the credential itself.
Microsoft 365 accounts are special-cased (`GoaMailAccount::is_microsoft_365`): GOA's own token for
them carries only Graph scopes, unusable against Exchange Online IMAP/SMTP, so `lookout-app`
supplies its own OAuth2 credentials instead (see §8.5, `microsoft_oauth.rs`).

## 7. Vendored IMAP stack — `async-imap`, `imap-proto`

Local forks, patched via `[patch.crates-io]` in the workspace `Cargo.toml`:
- **`imap-proto`**: one unreleased upstream fix — `is_char` accepts 8-bit bytes so servers sending
  raw UTF-8 in ENVELOPE fields parse instead of failing the whole response stream.
- **`async-imap`**: `Session::map_stream`, an in-place stream swap letting the mail session enable
  `COMPRESS DEFLATE` without losing the connection if the server rejects it; and a buffering fix in
  `imap_stream.rs` — response buffering used to copy the entire unconsumed tail into a fresh
  allocation on every parsed response (O(k²) over a multi-response burst), now parses into an
  owned `Response` and advances the buffer in place.

## 8. Desktop application — `lookout-app`

The `lookout` binary. This is the composition root: it's the only crate that imports `gtk`, `adw`,
`webkit`, `zbus`, `ksni`, or `secret-service`, and the only crate with a dependency on
`lookout-goa`.

### 8.1 Process startup and the threading model

`main()` (`main.rs`) builds an `adw::Application` (`APP_ID = "io.github.gavindi.Lookout"`), builds
the single `worker::Worker` *before* any window exists, hands it to `launcher_entry::init`/
`tray::init` (both need its tokio reactor for D-Bus work), registers a `--hidden` CLI flag for
login autostart, and on `activate` builds (or re-presents) the one `adw::ApplicationWindow` via
`window::build_window`.

**`worker.rs`** is the tokio/glib bridge — "the standard gtk-rs pattern for combining a tokio
reactor with a glib main loop without running tokio inside it." One OS thread runs a
`tokio::runtime::Builder::new_multi_thread()` runtime and blocks forever keeping it alive;
`Worker::spawn(future)` is the single entry point everything in `window.rs` uses to push
IMAP/SMTP/CalDAV/GOA-D-Bus I/O off the GTK thread. It is *not* a task-queue/scheduler abstraction —
just a shared runtime handle. Every account session actor (`run_account_session`,
`run_calendar_session`, the webcal/Google-Tasks pollers) is spawned onto it, and every result/event
crosses back to the UI thread over an `async_channel`, consumed via `glib::spawn_future_local`.

**`background.rs`** is unrelated to `worker.rs` despite the name — it implements "run without a
visible window": the XDG Background portal (`org.freedesktop.portal.Background` v2, requesting
`autostart: true` with `commandline: ["lookout", "--hidden"]`) with a self-managed
`~/.config/autostart/*.desktop` file as a fallback when the portal is absent or denies.

### 8.2 `UiState` and `CalendarUiState` — the two central state structs

`window.rs` (18,143 lines) holds two sibling per-domain state blobs rather than one giant struct.

**`UiState`** (mail/reading-pane/session plumbing) groups roughly into:
- **Account handles & discovery bookkeeping**: `accounts: HashMap<AccountId, AccountHandle>`
  (per-account `cmd_tx`/`interactive_cmd_tx`, folders, a read-side `Arc<lookout_mail::Cache>` for
  the composer's autocomplete, an optional `GraphPinClient`), `goa_accounts`, `goa_client`,
  `goa_unavailable`, `contacts_by_account`, `contact_cmd_tx`, `app_config`, `settings`.
- **Navigation/selection**: `current_account`/`current_mailbox`, `mail_view` (`Single |
  UnifiedInbox | Search`), `pending_message_selection` (deep-link target from an AI chat link),
  `unified_snapshots` (per-mailbox sets merged for "All Inboxes").
- **Optimistic-update stashes**, one per operation kind so concurrent in-flight ops don't clobber
  each other's rollback snapshot: `pending_optimistic_removals` (delete/archive/junk),
  `pending_optimistic_flag_changes` (read/unread), `pending_optimistic_pinned_changes`.
- **Reading-pane/body-fetch pipeline**: `pending_body_request`, `pending_attachment`,
  `pending_raw_message`, `pending_cid` (inline image fetches), `body_cache` (bounded LRU),
  `rendered_message` (identity of what's on-screen), plus banner state for List-Unsubscribe,
  iMIP invitations, read receipts, and remote-content trust.
- **Sync/dedup tracking sets**: `syncing: HashSet<MailboxId>` (outstanding `SyncMailbox`
  requests), `prefetching: HashSet<MailboxId>` (kept *separate* from `syncing` so background
  prefetch never wrongly suppresses a real user-triggered sync), `refreshing: HashSet<AccountId>`
  (outstanding `Refresh`), `folder_row_spinners` (live spinner widgets bound to those sets — a
  `Vec` per mailbox because a starred folder has two simultaneous rows).
- **List display**: `sort_key`/`sort_descending`, `favorites: HashSet<MailboxId>`.
- **Composer/identity relay hooks**: late-bound `Rc<dyn Fn()>` callbacks
  (`composer_identities_refresh`, `manage_signatures`, `follow_up_status`/`follow_up_toggle`) —
  late-bound because the row factories that need them exist before the state that drives them
  does.
- **Search**: `search_active`/`search_query`/`search_results`/`search_pending`.

**`CalendarUiState`** is the calendar-domain sibling: per-CalDAV-account handles
(`CalendarAccountHandle`, including a per-month occurrence cache for the dashboard's upcoming-
events window), `displayed_month`, `checked_calendar_ids` (sidebar checklist), `calendar_colors`,
webcal subscription handles, a synthesized `birthdays` handle, Google Tasks handles, a local-tasks
fallback store (`CalendarId("local")`, used when no connected calendar supports VTODO), and its own
`pending_calendar_moves` optimistic-rollback map for drag-to-reschedule.

### 8.3 Event dispatch — the uniform actor-consumption pattern

Every session type (mail, CalDAV, webcal, Google Tasks) is consumed the same way: the worker
thread runs the actor and sends events over a **bounded** `async_channel`; the UI thread runs a
`glib::spawn_future_local` loop that:

1. Blocks on the first event.
2. **Batch-drains** everything currently queued via `try_recv` — the startup burst (cache replay,
   live sync, previews) queues duplicate snapshot events that must repaint once per batch, not
   once per copy.
3. **Collapses** the batch (`collapse_account_events`/`collapse_calendar_events`/etc.),
   deduplicating whole-snapshot events keyed by a `SnapshotKey` so only the *last* copy of a
   supersedable event (e.g. the latest `MessagesUpdated` for a given mailbox) survives.
4. Runs one large `match` over every event variant, each arm mutating `UiState`/`CalendarUiState`
   then calling a targeted repaint helper (`rebuild_folder_tree`, `message_list.repopulate`,
   `dashboard_refresh()`, a toast on error) rather than redrawing the whole window.

There are 35 `glib::spawn_future_local` call sites in `window.rs` — one per connected-account
event loop plus assorted one-shot async UI tasks. Command channels, in contrast, are unbounded
and sent via `send_blocking` straight from the GTK thread.

**Startup discovery order** (tail of `build_window`): webcal subscriptions connect first (they
aren't GOA accounts); manually-configured "other" accounts connect *synchronously* next (reading
`app_config.other_accounts`); then GOA mail/contacts/tasks/calendar discovery each run as an
independent async round trip (`GoaClient::connect` → `list_*_accounts` on the worker, result
delivered back over a one-shot channel), connecting every *enabled* discovered account and
independently driving that module's own empty-vs-populated page.

### 8.4 Widget tree and navigation

Root page switcher is a plain `gtk::Stack` (`root_stack`, not `adw::ViewStack` — homogeneous
sizing deliberately disabled on both axes so a hidden page's async fill-in can't reflow the
visible one), with named children `"empty"/"mail"`, `"calendar-empty"/"calendar"`,
`"tasks-empty"/"tasks"`, `"lookout-empty"/"lookout"`, `"contacts-empty"/"contacts"`. A nav rail of
toggle buttons swaps `root_stack`'s visible child; a separate Home/View ribbon toggle swaps a
per-module command-toolbar stack alongside it.

- **Mail**: three `card_section`-wrapped panes — folder tree (`gtk::TreeListModel` built by
  `folder_tree::build_multi_account_tree_model`), message list (`message_list::MessageListModel`,
  §8.5), reading pane (a `webkit::WebView` for HTML + a header widget from `message_header.rs`) —
  combined via nested `gtk::Paned`s.
- **Calendar**: built by a dedicated `calendar_view::build_main()`/`build_sidebar()` pair, unlike
  Mail's inline construction.
- **Tasks**, **Lookout** (AI dashboard): each a single `card_section` around a module-owned root
  widget (`tasks_view::build_tasks_view()`, `lookout_view::build_lookout_view()`).
- **Contacts**: the one module built *inline* in `window.rs` rather than via a `build_*()`
  function — `contacts_view.rs` instead supplies pure population/logic functions
  (`rebuild_contacts_list_ui`, `sync_contacts_account`, `show_contact_editor_for`, etc.) that
  operate on window-owned widgets.
- **Settings**: `config_view::build()` returns its own `ConfigView` struct with an internal
  `paned`; individual rows are wired in `build_window` with `connect_active_notify`/
  `connect_value_notify` handlers writing straight through to `SettingsStore`.

### 8.5 Feature modules

**`message_list.rs`** — the message list's model layer, factored out to be pure-function-testable
without a GTK main loop. `SortKey` (`Date|Sender|Subject`, only `Date` produces date-section
headers), `ListFilter` (`All|Unread|Flagged`), `DateBucket` (`Today|Yesterday|ThisWeek|...|
Year(i32)|Pinned` — a bucket's identity is fixed forever by embedding the message's own year, so
collapse-state survives time passing). Thread grouping (`group_threads`) scans newest-first,
grouping by `(mailbox, thread_key)` — an empty thread key (uncomputed/legacy cache) renders as a
lone message rather than merging, and a "thread" of exactly one message renders as itself, not a
header. `MessageListModel::repopulate` stores the full unfiltered set into `truth`, then
`recompute_and_apply` snapshots it and either computes inline or, above
`BACKGROUND_REPOPULATE_THRESHOLD` (5,000 messages — chosen from a release-build timing probe
against a ~16ms frame budget), dispatches the pure-data `compute_layout` to
`tokio::task::spawn_blocking` and re-checks a `generation` counter before applying the result, so a
stale reply from a superseded call is discarded. A `RowDiff`/`TrackedStore` pair finds the common
prefix/suffix between old and new layouts so only the changed middle range is spliced into the
live `gio::ListStore`.

**`calendar_view.rs`** — five view modes as `gtk::Stack` children: `workweek`/`week`/`day` (all
built via a shared time-grid builder), `split` (a `MonthGrid` + `AgendaView` side-by-side pane —
there's no standalone "agenda" stack entry), and `month` (default). Time-grid views use two
stacked `gtk::DrawingArea`s (all-day band + scrollable timed canvas) with custom cairo
`set_draw_func` painting; overlapping same-day events are laid out via a greedy interval-
partitioning `assign_lanes` algorithm. Data-in/widget-out: `set_anchor`/`set_occurrences` are the
two inputs, both triggering a full `refresh`.

**`compose.rs`** — plain (`gtk::TextView`) and rich (contenteditable `webkit::WebView`,
`hardware_acceleration_policy(Never)`) editors coexist, one live at a time; HTML mode sends
`multipart/alternative` with both. `text_to_html` converts a plain-text prefill (quoted replies
included) into simple HTML to seed the rich editor — notably the *plain-text* prefill is always
the seed, not the original message's HTML. Draft autosave runs every 5s, diffing against a
snapshot and appending via `AccountCommand::SaveDraft` keyed by a stable per-session Message-ID;
Send first issues `DeleteDraft` (same command channel, so ordering holds) before sending.

**`contacts_view.rs`** / **`contacts_editor.rs`** — CardDAV account discovery + per-account
`sync_contacts_account` poll loop (RFC 6578 deltas), a left category tree (`ContactsBucketKind`:
`AllContacts|Favourites|ContactLists|Deleted|Category(String)`) + right contact list, `.vcf`
import/export. The editor dialog starts from a clone of the existing card and only rewrites
exposed fields, so unedited custom properties (`X-` extensions, `KIND:group`, etc.) ride along
untouched.

**`event_editor.rs`** — the calendar's counterpart to `compose.rs` (a modal form, since Calendar
has no reading-pane slot). Recurrence UI lives in a separate `recurrence.rs` module
(`RecurrenceRule`/`Frequency`/`RecurrenceEnd`, with `parse_rrule_string`/`to_rrule_string`/
`describe` round-tripping and humanizing RFC 5545 RRULEs). Edit scope for a recurring series
(single instance vs. "this and future") is expressed by setting `recurrence_id`/
`recurrence_range` on the saved `CalendarEvent`.

**`lookout_view.rs`** — see §9 (AI assistant pipeline) for the chat rendering/tool-calling
architecture; `folder_tree.rs` — see the widget-tree note above (§8.4), data model is `FolderNode`
(parent/child linking kept as a UI-layer concern, deliberately out of `lookout-core`) and
`TreeItem` (`Unified|Favorites|Account|Folder|Favorite` — `Favorite` is a second, duplicate row for
a starred mailbox).

**`google_tasks.rs`** — Google's CalDAV is VEVENT-only, so VTODO tasks can't live there; this
talks to the separate Google Tasks REST API with its own OAuth2 scope via the same public-client
PKCE+loopback flow as `microsoft_oauth.rs` (GOA's Google accounts don't carry the Tasks scope).

**`microsoft_oauth.rs`** — GOA's Microsoft 365 provider only requests Graph scopes, which don't
authenticate to Exchange Online IMAP/SMTP (verified: a Graph-scoped token gets `NO AUTHENTICATE
failed`). Lookout runs its own public-client authcode+PKCE+loopback OAuth2 flow instead (the same
approach Thunderbird uses), persisting a refresh token under `$XDG_DATA_HOME/lookout/oauth/`.

**`graph_pin.rs`** — mirrors Lookout's IMAP-`\Flagged`-driven pin state to Outlook's own MAPI
`PR_RENEW_TIME` property via Microsoft Graph (write-only; reading real-Outlook-made pins back is
explicitly out of scope), since IMAP has no way to reach that property and EWS is being retired.

**Smaller modules** (one line each): `reminders.rs` — VALARM-driven desktop notifications, fire/
snooze state in `ui_state_db`; `recipient_entry.rs` — composer chip fields, repopulate-from-truth
like `MessageListModel`; `signatures.rs`/`identities.rs` — Config editors writing through to
`AppConfig`; `tags.rs` — client-side color-tag definitions (JSON file), applied server-side as
IMAP keywords; `trusted_senders.rs` — remote-content trust dialog; `task_editor.rs`/
`tasks_view.rs` — VTODO editor and grouped list; `other_accounts.rs` — manual IMAP/SMTP account
management, keyring-backed; `theme.rs` — bundled named-color CSS theming; `background_image.rs`
— window-background image persistence; `shortcuts.rs` — keyboard shortcuts matched by physical
keycode (layout-independent).

### 8.6 Four persistence layers, deliberately kept separate

| Layer | File | Holds | Wiped by "Clear all caches"? |
|---|---|---|---|
| `lookout_mail::Cache` | `~/.cache/lookout/mail/<account>.sqlite3` | Synced IMAP data — mailboxes, envelopes, bodies, FTS index | Yes — disposable, re-fetchable |
| `lookout_dav::{CalendarCache,ContactsCache}` | `~/.cache/lookout/{calendar,contacts}/<account>.sqlite3` | Synced CalDAV/CardDAV data | Yes — disposable, re-fetchable |
| `ui_state_db.rs` | `~/.cache/lookout/ui-state.sqlite` | Starred contacts, reminder fire/snooze state, local-only tasks, trusted-sender decisions | **No** — UI-concern state, not re-fetchable from a server |
| `app_config.rs` | `~/.config/lookout/settings.json` | Identities, signatures, per-account signature defaults, folder-role overrides, webcal subscriptions, manually-configured accounts | No — user configuration |
| `settings.rs` | GSettings (`io.github.gavindi.Lookout` schema) | ~35 scalar preferences: theme, layout, sort, prefetch tuning, feature toggles | No — user configuration |

This is the clearest architectural line in the app: a *cache* is disposable and always
re-derivable from a live server round trip; *state* (`ui_state_db`) and *config*
(`app_config`/GSettings) are not, and the "Clear all caches" action is scoped precisely to the
former. `ui_state_db`'s schema version bump wipes only `starred_contacts` — the other three tables
(reminders, local tasks, trust decisions) are never wiped by a version bump either, since none of
them are recomputable.

Secrets never live in any of these files: GOA-account credentials come fresh from GOA per
connection; "other" accounts' and the AI assistant's API token live in the GNOME keyring via
Secret Service D-Bus; OAuth2 refresh tokens (Microsoft, Google Tasks) live in their own
`0600`-permissioned JSON files under `$XDG_DATA_HOME/lookout/oauth/`.

### 8.7 Notification / tray / launcher integration

`tray.rs` (StatusNotifierItem via `ksni`) and `launcher_entry.rs` (Unity/Ubuntu-dock
`LauncherEntry` badge) are both initialized in `main.rs` *before* the window exists, since they
need the worker's tokio reactor for their D-Bus registration. `mail_notifications.rs` registers
`app.raise-window`/`app.open-mailbox` GActions once, then fires/withdraws per-mailbox new-mail
notifications from inside the mail event loop. All three converge on one repaint choke point,
`refresh_unread_indicators`, called whenever folder/message state changes.

## 9. AI assistant pipeline ("Lookout" tab)

**Protocol**: OpenAI-compatible chat completions (`assistant.rs`). The API base URL is a GSettings
string; the API token lives in the GNOME keyring (never in dconf/settings.json). `chat_completions`
is one raw `POST {base}/chat/completions` round trip (30s timeout — a request may itself be an
LLM-internal multi-step answer).

**Tool-calling** (`assistant_tools.rs`) is OpenAI-style function calling over the app's **own local
data only** — SQLite caches + FTS5 index per mail account, CardDAV contact snapshots, the merged
task set — no live IMAP/CalDAV traffic is ever triggered by a tool call, and nothing leaves the
machine beyond the request the prompt itself provokes. Six read-only tools are exposed:
`recent_emails`, `search_emails` (FTS over subject/sender/recipients/body), `top_contacts`,
`list_contacts`, `list_tasks`, `upcoming_events`. **No write/compose/send tool exists** — the
assistant cannot take action, only answer from local data.

The agentic loop (`chat_with_tools`) runs up to 8 turns: POST with `tools`/`tool_choice: "auto"`;
if the reply carries no `tool_calls`, its `content` is the final answer; otherwise each tool call
is executed (on the blocking pool) and fed back as a `{role: "tool", ...}` message — a **failed
tool call becomes a JSON error payload returned to the model**, not an aborted conversation, so it
can retry with narrower parameters. Exceeding the turn limit is an explicit error.

**Rendering** (`lookout_view.rs`): replies render through a hand-rolled markdown-to-HTML pass over
fully-escaped text (headings, bold/italic/code, fenced code blocks, links, images, lists), with one
deliberate passthrough — a fenced block tagged ` ```html ` is inserted verbatim, letting the
assistant embed inline SVG/tables that markdown can't express, still constrained by the reading
surface's CSP. Replies are split into visual "cards" on each top-level heading
(`chat_reply_cards_html`) and loaded into a `webkit::WebView` (`chat_output`) with JS disabled.

**Chat links** (`chat_links.rs`): a custom `lookout-action:` URI scheme lets a reply deep-link to
a message or event. Message links deliberately use a short `<uid>:<mailbox>` payload rather than
JSON — an LLM has to reproduce the URI byte-for-byte, and a long percent-encoded JSON blob is far
more likely to get mangled by the model than a short delimited pair.

## 10. Build, packaging, and CI

- **`build.sh`** (repo root) — thin wrapper: `cargo build --workspace` (release by default),
  optionally `cargo deb --no-build -p lookout-app` for a `.deb`, with `SOURCE_DATE_EPOCH` pinned to
  the last git commit's timestamp for reproducibility.
- **Packaging manifests** live alongside the crates: `source/Cargo.toml`'s `[package.metadata.deb]`
  and `[package.metadata.generate-rpm]` (both driven off `crates/app/Cargo.toml`), `source/
  snapcraft.yaml` (Snap), `source/flatpak/io.github.gavindi.Lookout.json` (Flatpak, built by CI).
  All four package the same runtime dependencies: GTK4 ≥4.14, libadwaita ≥1.5, WebKitGTK 6.0,
  GLib.
- **App metadata** under `source/data/`: the `.desktop` file, AppStream `metainfo.xml` (release
  history — the same one this repo's `CHANGELOG.md` entries get mirrored into), the GSettings
  schema (`gschema/io.github.gavindi.Lookout.gschema.xml`, defining every key `settings.rs`
  reads), icons, and the compiled GResource bundle (themes, bundled artwork) that `main.rs`
  registers at startup via `resources::register`.
- **`source/test-fixtures/`** — sample `.eml`/`.ics` files used by tests and the debug "Open .eml"
  viewer.
- **CI** (`.github/workflows/ci.yml`, `release.yml`) — builds, clippy (`-D warnings`), and tests
  the workspace with the crate's actual feature set (not `--all-features`, which would force-
  enable `async-imap`'s mutually-exclusive `runtime-tokio`/`runtime-async-std` features
  simultaneously); a `greenmail` job runs `lookout-mail`'s IMAP integration test against a real
  ephemeral IMAP/SMTP server in a container.

## 11. Where to look for common tasks

| Task | Start here |
|---|---|
| Add an IMAP command/event | `crates/mail/src/session.rs` — extend `AccountCommand`/`AccountEvent`, dispatch in `connect_and_run`'s `match`, handle in `window.rs`'s account event loop |
| Add a CalDAV command/event | `crates/dav/src/session.rs`, same pattern via `CalendarCommand`/`CalendarSessionEvent` |
| Add a cached field to a message | `crates/core/src/email.rs` (`EmailSummary`/`EmailBody`, remember `#[serde(default)]`), bump `ENVELOPE_CACHE_VERSION`/`BODY_CACHE_VERSION` in `crates/mail/src/cache.rs` if the shape changes incompatibly |
| Add a Settings toggle | `crates/app/src/settings.rs` (GSettings key) + the app's `data/gschema/*.xml` + wire it into `config_view.rs`/`build_window` |
| Add a UI-persisted (non-cache) preference | `crates/app/src/ui_state_db.rs` if it's per-item state, `app_config.rs` if it's structured config |
| Add an AI assistant tool | `crates/app/src/assistant_tools.rs` — `tool_definitions()` + a branch in `execute_tool()`, keep it read-only over local caches |
| Add a new pane/module to the shell | `crates/app/src/window.rs`'s `root_stack` + nav rail wiring, following the data-in/widget-out convention (`calendar_view.rs`/`folder_tree.rs`/`config_view.rs` are the cleanest examples) |
