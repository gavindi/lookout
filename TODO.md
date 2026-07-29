# TODO

Derived from the implementation plan (`webmail/` → Lookout port). Phase 1 is the current milestone; Phases 2-5 are the scoped roadmap for later work.

## Phase 1 — Mail MVP

- [x] Cargo workspace scaffolding (`lookout-core`, `lookout-goa`, `lookout-mail`, `lookout-app`)
- [x] `.desktop` file (`data/io.github.gavindi.Lookout.desktop`, passes `desktop-file-validate`), AppStream metainfo (passes `appstreamcli validate` bar the placeholder homepage URL), placeholder app icon
  - [ ] GResource bundle - nothing to bundle yet since the UI is built programmatically with no `.ui` XML templates (a deliberate Phase 1 choice, see the plan)
- [x] `AdwApplication` shell with empty-state page ("No mail accounts configured") that spawns `gnome-control-center online-accounts`
- [x] Window chrome polish: background image behind the mail panes, translucent folder pane (and empty-state reading pane) so it shows through, per-`MailboxRole` folder icons in the tree, a view-switcher rail on the window's left edge (Mail only so far - scaffold for Phase 3/4's Calendar/Contacts views), and an Outlook-style menu bar + command toolbar stacked below the title bar. File→Quit and Help→About are wired to real `gio::SimpleAction`s; Home/View and the toolbar's Delete/Archive/Report/Flag/Snooze/More buttons are visible-but-disabled placeholders pending the features below
- [x] `lookout-goa`: zbus proxies for `ObjectManager`/`Account`/`Mail`/`PasswordBased`/`OAuth2Based`; `list_mail_accounts()`
- [x] Wire account discovery into startup → folder tree for the default account
- [x] `lookout-mail`: `AccountSession` actor — `LOGIN`/`AUTHENTICATE XOAUTH2`, `LIST (SPECIAL-USE)`, `SELECT`, bounded envelope fetch, IDLE loop with instant command interruption
- [x] SQLite cache for mailboxes/messages (`$XDG_CACHE_HOME/lookout/mail/`), used for fast first paint before the live IMAP fetch completes
  - [ ] Flat-file `.eml`/attachment cache (bodies are still fetched fresh each time, not cached)
- [x] Folder tree UI wired to live data (`Gtk.TreeListModel`)
- [x] Message list UI wired to live data (flat, `thread_key` computed but not yet grouped in UI)
- [x] Message viewer: body fetch → `mail-parser` → sandboxed WebView with `Gtk.TextView` fallback
  - [ ] Switch from whole-message fetch to `BODYSTRUCTURE`-driven partial fetch
  - [ ] Inline `cid:` image resolution via a custom WebKit URI scheme handler
- [x] Compose window: plain-text body, `mail-builder` MIME, `lettre` SMTP send (XOAUTH2/password), `APPEND` to Sent - validated live end-to-end against Gmail
  - [ ] reply/reply-all/forward entry points (currently new-message only)
  - [ ] Rich-text/contenteditable WebView body (currently plain-text only, per the plan's own descope fallback)
  - [ ] Recipient chip widget with autocomplete (currently comma-separated text)
  - [ ] Autosave drafts
  - [ ] Multiple sending identities (currently always sends as the account's own address)
- [x] Connection lifecycle: state machine, backoff reconnect, IDLE re-issue before RFC 2177 timeout, `Gio.NetworkMonitor`-driven reconnect (cuts the backoff wait short once connectivity is back)
- [x] `Adw.ToastOverlay` for connection/send errors
- [x] Flatpak manifest spike (`flatpak/io.github.gavindi.Lookout.json`) - GOA permission (`--talk-name=org.gnome.OnlineAccounts`) confirmed against Geary's real, shipping manifest, not guessed; see `flatpak/README.md` for exactly what's still needed before this actually builds:
  - [ ] Generate `cargo-sources.json` via Flathub's `flatpak-cargo-generator.py` (~150 transitive deps)
  - [ ] Install `flatpak-builder` + `org.gnome.Sdk//49` + the rust-stable SDK extension and do a real build+run (none of that is available in this environment)
  - [ ] Fix "Open Online Accounts Settings" for sandboxed runs - it currently shells out to `gnome-control-center` via `std::process::Command`, which doesn't work inside a Flatpak sandbox; needs `org.gnome.ControlCenter` D-Bus activation instead

### Testing/verification (Phase 1)

- [x] Unit tests: JWZ threading, mailbox-role heuristics, header/address parsing, SQLite cache round-trip
- [x] Manual smoke test against real GOA accounts (OAuth2 + read-only IMAP flow validated live against Gmail)
- [x] Manual send test: self-addressed email sent via SMTP and confirmed present in Sent via `APPEND` (live against Gmail)
- [x] Fake-GOA D-Bus test service (`crates/goa/tests/fake_goa.rs`, zbus server-side `#[interface]`) - **run and passing** under `dbus-run-session -- cargo test -p lookout-goa --test fake_goa`; real D-Bus-wire coverage of discovery + credential fetch (OAuth2 and password paths, plus the two real-world "unusable Mail interface" cases seen live: `ImapSupported=false` and no Mail interface at all)
- [x] Protocol integration test: GreenMail (`crates/mail/tests/imap_integration.rs`, `testcontainers`) covering LOGIN/LIST/APPEND/SMTP send via the real `run_account_session` actor, gated behind a `test-utils` Cargo feature (self-referencing dev-dependency, never reachable from production builds) that adds an insecure-TLS test connector for GreenMail's self-signed cert. **Written carefully (GOA/port/env-var details cross-checked against GreenMail's own source and Docker docs) but never executed** - no Docker available in the environment that wrote it. `#[ignore]`d so `cargo test` skips it by default; run with `cargo test -p lookout-mail --features test-utils --test imap_integration -- --ignored` once Docker is available, and treat that first run as the real validation.
- [x] `test-fixtures/` sample `.eml` set (plain text, HTML+inline CSS, HTML+`cid:`, HTML+external images, malformed HTML) + dev-only "open .eml" debug action
- [x] CI: `cargo fmt --check`, `cargo clippy -D warnings`, unit tests, fake-goa tests, GreenMail integration tests, build-only job against GTK4/libadwaita/webkitgtk-6.0 dev packages
- [ ] Manual milestone smoke test covering compose+send and reconnect-after-sleep on both an OAuth2 and a password-based account

## Phase 2 — Mail advanced (roadmap)

- [ ] Message delete (`\Deleted` flag + `EXPUNGE`, or move-to-Trash) - backs the toolbar's Delete button
- [ ] Move-to-folder actions: Archive and Report-as-junk (mark junk + move to the account's Junk mailbox) - backs the toolbar's Archive/Report buttons
- [ ] Client-side snooze (hide a message and resurface it later - no IMAP-native equivalent, same approach as Gmail/Outlook desktop) - backs the toolbar's Snooze button
- [ ] Ribbon-style Home/View tab content once there's something to put in them - the menu bar's Home/View buttons are disabled placeholders until then
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
