# TODO

Derived from the implementation plan (`webmail/` → Lookout port). Phase 1 is the current milestone; Phases 2-5 are the scoped roadmap for later work.

## Phase 1 — Mail MVP

- [x] Cargo workspace scaffolding (`lookout-core`, `lookout-goa`, `lookout-mail`, `lookout-app`)
- [ ] `.desktop` file, GResource bundle
- [x] `AdwApplication` shell with empty-state page ("No mail accounts configured") that spawns `gnome-control-center online-accounts`
- [x] `lookout-goa`: zbus proxies for `ObjectManager`/`Account`/`Mail`/`PasswordBased`/`OAuth2Based`; `list_mail_accounts()`
- [x] Wire account discovery into startup → folder tree for the default account
- [x] `lookout-mail`: `AccountSession` actor — `LOGIN`/`AUTHENTICATE XOAUTH2`, `LIST (SPECIAL-USE)`, `SELECT`, bounded envelope fetch, IDLE loop with instant command interruption
- [ ] SQLite cache schema (`mailboxes`, `messages`) + flat-file `.eml`/attachment cache under `$XDG_CACHE_HOME`
- [x] Folder tree UI wired to live data (`Gtk.TreeListModel`)
- [x] Message list UI wired to live data (flat, `thread_key` computed but not yet grouped in UI)
- [x] Message viewer: body fetch → `mail-parser` → sandboxed WebView with `Gtk.TextView` fallback
  - [ ] Switch from whole-message fetch to `BODYSTRUCTURE`-driven partial fetch
  - [ ] Inline `cid:` image resolution via a custom WebKit URI scheme handler
- [ ] Compose window: new/reply/reply-all/forward, plain-entry recipients, contenteditable WebView body (or plain-text if descoped), `mail-builder` MIME, `lettre` SMTP send (XOAUTH2), `APPEND` to Sent, autosave drafts
- [x] Connection lifecycle: state machine, backoff reconnect, IDLE re-issue before RFC 2177 timeout
  - [ ] `Gio.NetworkMonitor`-driven reconnect (react to network-down/up instead of blind backoff)
- [x] `Adw.ToastOverlay` for connection/send errors
- [ ] Packaging skeleton: `.desktop`, GResource, Flatpak manifest spike (verify GOA D-Bus reachability from the sandbox)

### Testing/verification (Phase 1)

- [x] Unit tests: JWZ threading, mailbox-role heuristics, header/address parsing
- [x] Manual smoke test against real GOA accounts (OAuth2 + read-only IMAP flow validated live against Gmail)
- [ ] Protocol integration tests: GreenMail (`testcontainers`) covering LOGIN/LIST/FETCH/APPEND/IDLE and SMTP send-and-verify
- [ ] Fake-GOA D-Bus test service (zbus server-side `#[interface]`) for CI coverage of the GOA layer without a live session
- [ ] `test-fixtures/` sample `.eml` set (plain text, HTML+inline CSS, HTML+`cid:`, HTML+external images, malformed HTML) + dev-only "open .eml" debug action
- [ ] CI: `cargo fmt --check`, `cargo clippy -D warnings`, unit tests, fake-goa tests, GreenMail integration tests, build-only job against GTK4/libadwaita/webkitgtk-6.0 dev packages
- [ ] Manual milestone smoke test covering compose+send and reconnect-after-sleep on both an OAuth2 and a password-based account

## Phase 2 — Mail advanced (roadmap)

- [ ] Full-text search: SQLite FTS5 over cache + IMAP `SEARCH` fallback
- [ ] Internal drag-drop (move/tag) + external drag-out (`.eml`/`.zip`)
- [ ] Collapsible thread UI (reuse the folder tree's `TreeListModel` trick)
- [ ] Color-tag keywords (`$Lookout-tag-*` namespace) + tag management
- [ ] Recipient-chip/autocomplete composer widget
- [ ] Unified mailbox + cross-account views; full multi-account switcher
- [ ] Batch actions + `Gtk.MultiSelection`
- [ ] Hover quick-actions
- [ ] Physical-keycode global keyboard shortcuts
- [ ] Print support
- [ ] List-Unsubscribe banner
- [ ] External-content trust-sender flow
- [ ] RFC 8098 read-receipt (MDN) generation

## Phase 3 — Calendar (roadmap)

- [ ] `lookout-dav`: thin CalDAV/WebDAV layer over `reqwest`
- [ ] iCalendar modeling + recurrence expansion (`icalendar` + `rrule`, spike to confirm maturity)
- [ ] GOA `Calendar` interface for endpoint + credentials
- [ ] Custom-drawn month/week/day/agenda views
- [ ] Drag-reschedule; recurring edit scopes (this / this-and-following / all)
- [ ] iMIP invitation banners in the mail viewer
- [ ] `.ics` import / webcal subscription
- [ ] Mini-calendar sidebar widget
- [ ] VTODO tasks
- [ ] Birthday calendar synthesized from Contacts
- [ ] `Gio.Notification` event alerts

## Phase 4 — Contacts (roadmap)

- [ ] Shared `lookout-dav` plumbing for CardDAV (incl. RFC 6578 sync-collection REPORT)
- [ ] vCard parser/writer (RFC 6350) — no crate verified robust enough; likely hand-rolled
- [ ] Address book CRUD, groups, vCard import/export with duplicate detection
- [ ] Shared `ContactsProvider` trait consumed by mail composer, calendar attendees, and the contacts app

## Phase 5 — Settings/theming (roadmap)

- [ ] `AdwPreferencesWindow` mirroring Bulwark's tab taxonomy (General/Appearance/Layout/Mail/Privacy/Apps/Advanced)
- [ ] GSettings (scalars) + serde config file (relational data: identities, folder-role overrides, tag colors)
- [ ] libadwaita named-color theming; optional bundled flat-token themes (web-only CSS-injection "skin" themes have no native equivalent and are out of scope)
