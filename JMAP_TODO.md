# JMAP mail support

This plan adds **JMAP** (RFC 8620 core, RFC 8621 mail) as a second mail protocol next to
IMAP/SMTP, for servers such as Fastmail and Stalwart (others: Cyrus 3.x, Apache James).
JMAP accounts would be manual accounts. Mail, folders, search, drafts and sending all go over
JMAP, and the rest of the app sees the same commands and events it sees today.

**Status:** assessment only. Nothing is implemented.

**Current state:**

- Mail goes over IMAP/SMTP through `lookout-mail`. Microsoft 365 adds Graph, and CalDAV/CardDAV
  go through `lookout-dav`. Nothing in the repo speaks JMAP.
- Lookout began as a port of Bulwark Webmail *away from* JMAP (see the first CHANGELOG entry).
  So several of its models (identities, keywords) already line up with JMAP's.
- `MAIL_GRAPH_OVERVIEW.md` §3–4 already designs a second mail transport (Graph mail, still
  deferred). JMAP fits that design, and fits it better than Graph did (see
  [Why JMAP fits better than Graph mail](#why-jmap-fits-better-than-graph-mail)).
- GNOME Online Accounts has no JMAP provider, and Evolution has no JMAP accounts to import. So
  a JMAP account has to be added by hand.

## Why JMAP fits better than Graph mail

Of the four Graph-mail risks recorded in `MAIL_GRAPH_OVERVIEW.md` §5, three don't apply to
JMAP:

| Graph risk | JMAP |
|---|---|
| **R1 No push** | JMAP pushes changes over EventSource (`eventSourceUrl`) and WebSocket (RFC 8887). A desktop app can use either, so new mail arrives about as fast as with IDLE. |
| **R2 Weak search** | `Email/query` filters (`text`, `from`, `to`, `subject`, `before`, `after`, `hasKeyword`, `inMailbox`) map almost 1:1 onto `search_query.rs`'s `SearchQuery`. |
| **R3 MIME fidelity** | Every Email has a `blobId` that downloads its exact RFC 5322 bytes, so `body::parse_body` and `preview_from_raw` work unchanged. iMIP invites, TNEF, `Authentication-Results` and read receipts keep working. |
| **R4 Identity churn** | **Still applies.** See [Risks](#risks). |

## What already exists to reuse

- **The protocol seam.** `AccountCommand` / `AccountEvent`
  (`source/crates/mail/src/session.rs:345`, `:743`) have about 45 variants each. The app's event
  loop, message list, reading pane, composer, undo and outbox UI talk to a mail account only
  through them. A second actor with the same signature as `run_account_session`
  (`session.rs:1297`) leaves the UI untouched.
- **The cache.** `lookout_mail::Cache` (`source/crates/mail/src/cache.rs`) holds mailboxes,
  messages, bodies, snoozed messages, the outbox, the FTS index and addresses. It's keyed
  `(mailbox_id, uid, uidvalidity)`, so it can be reused as long as JMAP ids map to a synthetic
  `Uid`.
- **Parsing and building.** `body::parse_body`, `body::preview_from_raw`,
  `send::build_raw_message`, `send::build_draft_raw_message` and `search_query::parse`.
- **Outbox and scheduler helpers** in `session.rs`: `outbox_retry_delay`, `due_wakes`,
  `cap_slice_for_outbox`, `scheduled_folder`, `plan_folder_change` and `plan_create_mailbox`.
  They are already free functions and can be shared as `pub(crate)`.
- **The HTTP client pattern.** `source/crates/graph/src/client.rs` (reqwest + serde, wiremock
  tests, `Retry-After` handling) and the shared `crate::http::client()`.
- **Manual accounts.** `OtherAccount` (`source/crates/app/src/app_config.rs:89`), the keyring
  slots in `other_accounts.rs` (`KeyringStore`, `OtherCredentialProvider`), and
  `connect_other_account` (`source/crates/app/src/window/connect.rs:1457`), which builds an
  `AccountConfig` and spawns `run_account_session`.
- **Capability gating.** The Sieve UI appears only after `AccountEvent::SieveAdvertised(true)`,
  and the Graph extras (Focused, categories, Graph rules) are already limited to Microsoft 365
  accounts. A JMAP session that never emits those events leaves those parts of the UI hidden,
  with nothing new to gate.

## How Lookout's mail features map onto JMAP

| Lookout | JMAP | Notes |
|---|---|---|
| Folders, counts, roles | `Mailbox/get`, `Mailbox/changes`, `role`, `totalEmails` / `unreadEmails` | The server reports counts, so the STATUS drain isn't needed. |
| Envelope sync | `Email/query` + `Email/get` (selected properties), `Email/changes`, `Email/queryChanges` | Each type's `state` string is the delta cursor. |
| New-mail push | `StateChange` over EventSource | Replaces the IDLE supervisor. Falls back to polling. |
| Flags, keywords, snooze, `$Junk` | patch `keywords` (`$seen`, `$flagged`, `$answered`, custom atoms) | Same atoms as IMAP, so keyword snooze and follow-up flags port directly. |
| Move, copy, revert | patch `mailboxIds` | An Email's id doesn't change when it moves, which makes undo simpler. |
| Body, attachment, raw message | blob download (`downloadUrl`) | Reuses the existing parser. |
| Drafts | blob upload + `Email/import` into Drafts with `$draft` | |
| Send, outbox | `EmailSubmission/set` + `onSuccessUpdateEmail` (Drafts → Sent) | The server files the sent copy. The local outbox keeps undo send and scheduled send. `FUTURERELEASE` is optional. |
| Send-as | `Identity/get` | Mapped onto Lookout's existing identities. |
| Search | `Email/query` filter | Local FTS still applies. |
| Automatic replies | `VacationResponse` (RFC 8621 §8) | Optional, only when the server offers the capability. |
| Rules | JMAP for Sieve Scripts (RFC 9661) | Later. Stays hidden until then. |
| Calendars, contacts | JMAP Calendars / Contacts | **Out of scope.** Fastmail and Stalwart also serve CalDAV/CardDAV, which `lookout-dav` already handles. |

**Authentication.** Stalwart and Cyrus accept Basic. Fastmail requires an API token (Bearer)
or OAuth for JMAP; its app passwords don't work.

**Discovery.** `https://<domain>/.well-known/jmap` returns the Session object (`apiUrl`,
`downloadUrl`, `uploadUrl`, `eventSourceUrl`, `primaryAccounts`).

## Proposed decisions (to confirm before J1)

- **A new crate, `source/crates/jmap` (`lookout-jmap`), for the wire protocol only.** It
  covers Session discovery, batched method calls with back-references, typed `Mailbox` /
  `Email` / `Identity` / `EmailSubmission` / `VacationResponse`, blob upload and download, the
  EventSource stream and error types. It's built on reqwest + serde with wiremock tests and
  fixtures under `source/test-fixtures/jmap/`. Write it in-house rather than depend on the
  `jmap-client` crate: that crate's maintenance is uncertain, and `graph`/`dav` were built the
  same in-house way.
- **The session actor lives in `lookout-mail`**, as `source/crates/mail/src/jmap_session.rs`:
  `run_jmap_account_session(config, credentials, cmd_rx, interactive_rx, evt_tx)`. Being inside
  the crate gives it access to `Cache`, `cache_op`, the outbox and the shared helpers, and it
  emits the existing `AccountEvent`s.
- **Account settings stay backward-compatible.** `OtherAccount` gains
  `#[serde(default)] protocol: MailProtocol { Imap, Jmap }`, `jmap_session_url`,
  `jmap_username` and `jmap_auth: Basic | Bearer`, and existing `settings.json` files still load
  as IMAP. A new keyring slot, `PROTOCOL_JMAP`, holds the password or token.
- **Synthetic UIDs** (see [Risks](#risks), item 1).

## Risks

1. **The identity model (Graph R4).** `Uid(u32)` / `UidValidity` appear at about 168 call
   sites in the app and in the cache's primary key. Proposed design:
   - An account-wide `jmap_ids(uid INTEGER PRIMARY KEY AUTOINCREMENT, email_id TEXT UNIQUE)`
     table gives each JMAP Email id a synthetic uid.
   - One fixed `UidValidity` per account, bumped when the server answers
     `cannotCalculateChanges`, which forces a full resync.
   - An email in several mailboxes keeps the same uid in each `(mailbox_id, uid)` row.
   - `MailboxId` becomes `"{account}:{jmap mailbox id}"`.
   - The table is append-only, so the cache doesn't need a `user_version` wipe.
2. **The session monolith.** `connect_and_run` (`session.rs:1517` to about line 4850) mixes
   IMAP with outbox, snooze-wake, prefetch and cache bookkeeping. The transport-neutral parts
   need pulling out first (J3); otherwise the JMAP actor duplicates 1,000–2,000 lines. This is
   the main regression risk for existing IMAP users.
3. **Emails in several mailboxes.** JMAP allows one message in several mailboxes, like labels.
   Folder views work as they are, but unread counts and "move" need care when an email is in
   more than one.
4. **Servers differ.** Fastmail is the reference implementation and Stalwart is solid; Cyrus
   needs configuring. Test against Stalwart in a testcontainer, the way the IMAP tests use
   GreenMail.

## Task breakdown

Effort: **S** ≤ 1 day · **M** ≤ 1 week · **L** multi-week.

| # | Task | Effort |
|---|---|---|
| J0 | **Spike.** Session discovery, `Mailbox/get`, `Email/query` and blob download in `lookout-jmap`, tried against Stalwart (in a container) and a Fastmail token. Confirm a downloaded message parses the same through `parse_body`. | S |
| J1 | **`lookout-jmap` transport.** Request batching, every mail method in the mapping above, EventSource, upload/download, wiremock tests. | M |
| J2 | **Identity model.** The `jmap_ids` table and the `UidValidity` rules above, written up in SCAFFOLD.md §3/§4. | M |
| J3 | **Refactor.** Move the transport-neutral outbox, snooze, prefetch and cache bookkeeping out of `connect_and_run`. No behaviour change; the IMAP tests stay green. | M–L |
| J4 | **`run_jmap_account_session`.** Folder and envelope sync, the push/poll loop, body/attachment/raw fetch, flags and keywords, move/copy/revert, folder create/rename/move/delete, snooze, junk, drafts, search. | L |
| J5 | **Sending.** Outbox drain through `EmailSubmission`, `Identity` mapping, Sent filing. The local outbox keeps undo send and scheduled send. | M |
| J6 | **App wiring.** `OtherAccount.protocol`, the keyring slot, a protocol choice in the add-account dialog (the JMAP form asks for address, password or token, and an optional URL that's discovered otherwise), the branch in `connect_other_account`, and hiding the IMAP-only fields. | M |
| J7 | **Extras, each only when the server offers it.** `VacationResponse` for automatic replies, JMAP Sieve for rules, quota. | M |
| J8 | **Tests.** A Stalwart testcontainer integration test alongside `source/crates/mail/tests/imap_integration.rs`, a fake-server session test, and fixtures. | M |
| J9 | **Docs.** README, SCAFFOLD.md, OUTLOOK_PARITY.md rows. CHANGELOG and version only when asked. | S |

**Order:** J0 → J1 ∥ J2 → J3 → J4 → J5 → J6 → J7 ∥ J8 → J9.

**Rough total:** 6–10 weeks of focused work. Users see no change from J0–J2, or from J6 while
it stays hidden behind the protocol choice.

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
      Stalwart test, pass; the IMAP integration test still passes after J3.
- [ ] Manual check with a Stalwart or Fastmail account: folders, list, open, move and undo,
      flag, snooze, draft, send with undo send, search, and new mail arriving by push within
      seconds.
