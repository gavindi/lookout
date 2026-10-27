# Focused Inbox — Phase 2, item 2.12 (M-03)

Implementation plan for [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) §7 item **2.12**, the last
remaining Phase 2 item.

**Current state:** the Microsoft 365 half already landed with item 5.4 — Graph
`inferenceClassification` is read through the `/me/messages/delta` feed, `SetFocused`
writes it back, and sender overrides sync through `/me/inferenceClassification/overrides`.
What is missing is the local classifier for non-Microsoft accounts:

- `refresh_list_header` and the message menu show the tabs/submenu only for an account
  with a consented Graph client (`window/support.rs:605`, `window/mail_actions.rs:504`).
- There is no opt-in setting for a local classifier.
- `stop_moving_focused` returns early when there is no Graph client
  (`window/mail_actions.rs:921`).

**Confirmed decisions:**

- One global **Settings → Mail** switch, default **off**.
- Classification priority: explicit sender override → known sender (contacts or
  Sent) → `List-Id` bulk → **Focused** default. Unknown senders stay Focused;
  mailing lists go to Other.

---

## Tasks

### [x] 2.12.1 — Pure classifier (`S`)

Add `source/crates/core/src/interop/focused.rs`, register it in `interop/mod.rs`
and re-export from `interop` / `lib.rs`.

- `pub fn classify(sender: Option<&str>, list_id: Option<&str>,
  override_class: Option<InferenceClassification>, known: bool)
  -> InferenceClassification`
- `normalize_address` (trim/lowercase).
- Priority exactly as decided above; no I/O, no GTK, no Microsoft-specific strings
  leaking into feature code.
- Unit tests:
  - an override wins over everything;
  - a known sender beats `List-Id`;
  - `List-Id` alone → Other;
  - unknown sender → Focused;
  - sender matching is case-insensitive;
  - missing sender falls through to Focused.

### [x] 2.12.2 — Cache support (`M`)

Touch `source/crates/mail/src/cache.rs`.

- New append-only table in the schema block (no `user_version` bump):

  ```sql
  CREATE TABLE IF NOT EXISTS sent_correspondents (
      address TEXT PRIMARY KEY,
      last_seen INTEGER NOT NULL
  );
  ```

- `record_sent_correspondents(&[EmailSummary])`: union of `to`/`cc`, lowercased.
- `sent_correspondents() -> HashSet<String>`.
- `rebuild_sent_correspondents(&[MailboxId])`: scan the cached rows (including
  prefetch `partial` rows) for the account's Sent folders.
- `classify_local_inference(&[EmailSummary], contacts) -> Result<usize>`:
  - load `inference_overrides` and the Sent set;
  - run the pure classifier over messages with no existing `local_inference` row;
  - `INSERT OR IGNORE` only the `Other` results;
  - return how many rows were inserted.
- Tests:
  - Sent recipients are recorded/read back and rebuild from cached rows;
  - list mail from an unknown sender → Other;
  - known sender → Focused;
  - explicit override respected;
  - existing rows preserved;
  - a second pass inserts nothing.

### [x] 2.12.3 — Session classification (`L`)

- `source/crates/mail/src/config.rs`: add to `AccountConfig`:
  - `local_focus_enabled: bool`
  - `local_focus_contacts: Vec<String>`
  
  Update the two app constructors (`window/connect.rs:229`, `:1475`) plus
  `mail/examples/*` and `mail/tests/imap_integration.rs`.
- `source/crates/mail/src/session.rs`:
  - add `AccountCommand::ConfigureLocalFocus { enabled, contacts, backfill }`;
  - add a session-local `LocalFocusSettings` seeded from `AccountConfig` in
    `run_account_session`, passed through `connect_and_run` so it survives reconnects;
  - in `sync_mailbox`, before `emit_messages` (which already overlays every local
    property through `overlay_follow_ups`):
    - if enabled and the mailbox role is `Inbox`, classify the `new_messages` set
      (this covers the first sync too);
    - if the mailbox role is `Sent`, record its recipients;
  - `ConfigureLocalFocus` handler: update enabled/contacts; when
    `backfill && enabled`, rebuild the Sent set, classify cached Inbox messages,
    and re-emit only if rows were inserted.
- Tests at the cache/helper level (the mail crate has no fake IMAP server):
  classifier + `classify_local_inference` integration, with a code comment noting
  that first-sync classification happens before the command loop starts.

### [x] 2.12.4 — Tabs and message menu (`S`)

- `source/crates/app/src/window/state.rs`: add `microsoft_365: bool` to
  `AccountHandle`; set it in both connect paths and the test constructor in
  `window/tests.rs`.
- New helper `focused_inbox_available(state, account_id)` in
  `window/support.rs`:
  - Microsoft 365 → a consented Graph client;
  - all other accounts → `MAIL_FOCUSED_INBOX_LOCAL`.
- Use the helper in `refresh_list_header` (replacing the Graph-only check) and in
  `open_message_menu` for `MenuChoices.focused_inbox` (`mail_actions.rs:504`).
- Tests:
  - availability matrix: Microsoft 365 consent/no consent, local setting on/off;
  - the Focused Inbox submenu appears for an enabled local account.

### [x] 2.12.5 — Opt-in setting and contact propagation (`M`)

- `source/crates/app/src/settings.rs`:
  `MAIL_FOCUSED_INBOX_LOCAL = "mail-focused-inbox-local"`, default `false`.
- GSettings schema: matching boolean key with a description explaining it applies
  to non-Microsoft accounts.
- `source/crates/app/src/config_view.rs`: `focused_inbox_local_row: adw::SwitchRow`
  in the Mail group — “Sort the Inbox into Focused and Other on non-Microsoft
  accounts” — plus the `ConfigView` field and startup seeding in `window.rs`.
- Handler: persist the setting, send
  `ConfigureLocalFocus { enabled, contacts, backfill: enabled }` to every
  non-Microsoft 365 account, then `refresh_list_header`.
- Helper `local_focus_contacts()`: deduped lowercase union of
  `contacts_by_account[*].suggestions`; push to non-M365 sessions at connect and
  after contact snapshots change (`contacts_view.rs`, around the
  `contacts_by_account.insert`), skipping identical pushes.
- Tests: settings default/round-trip and the contact-set assembly.

### [x] 2.12.6 — Local “Always move / Stop moving” (`S`)

- Refactor `stop_moving_focused` (`window/mail_actions.rs:921`): always send
  `AccountCommand::ClearInferenceOverride`; only do the Graph override
  lookup/deletion when a consented client exists.
- Confirm `set_focused` with `FocusedScope::Sender` already persists a local
  override on non-Graph accounts (it does through `session.rs:4441`); add a test
  that the command is issued without Graph.
- No reclassification of older mail from that sender — matches Outlook’s
  “future mail” semantics.

### [x] 2.12.7 — Close-out (`S`)

- `OUTLOOK_PARITY.md`:
  - mark 2.12 ✅;
  - update the M-03 “Gap” cell (remove “until 2.12’s local heuristic”);
  - fix any §2.1 wording that says the tabs are hidden for non-Microsoft accounts.
- `CHANGELOG.md`: “Mail — local Focused Inbox classifier (Phase 2, item 2.12 /
  M-03)” with the touched files.
- Version bump across `source/Cargo.toml`, `snapcraft.yaml`, the metainfo and the
  release workflow, matching the existing release convention.
- Run `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and the workspace tests.

---

## Order

`2.12.1 → 2.12.2 → 2.12.3 → 2.12.4 → 2.12.5 → 2.12.6 → 2.12.7`

(2.12.4 and 2.12.5 can run in parallel once 2.12.3 is in.)

## Out of scope / noted follow-ups

- Bumping `ENVELOPE_CACHE_VERSION` to fetch `Precedence`/`Auto-Submitted` for
  broader bulk detection; the initial classifier uses the already-fetched `List-Id`.
- Focused tabs in All Inboxes and search results (current behaviour keeps them
  Inbox-only).
- Per-account opt-in, Outlook’s server-side Focused Inbox on/off state, and
  learning from user moves beyond the explicit “Always move” override.
