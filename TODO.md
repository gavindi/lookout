# TODO

Derived from the implementation plan (`webmail/` → Lookout port). Phase 1 is the current milestone; Phases 2-5 are the scoped roadmap for later work.

## Phase 1 — Mail MVP

- [x] Cargo workspace scaffolding (`lookout-core`, `lookout-goa`, `lookout-mail`, `lookout-app`)
- [x] `.desktop` file + AppStream metainfo + placeholder icon (validates with `desktop-file-validate`/`appstreamcli validate`)
  - [ ] GResource bundle - nothing to bundle yet since the UI is built programmatically with no `.ui` XML templates (a deliberate Phase 1 choice, see the plan)
- [x] `AdwApplication` shell with empty-state page that spawns `gnome-control-center online-accounts`
- [x] Window chrome: background image, translucent panes, per-role folder icons, view-switcher rail, Outlook-style menu bar + command toolbar (quit/about/layout, ribbon tabs live; Flag/More/Filter are disabled placeholders)
- [x] `lookout-goa`: zbus proxies for GOA interfaces; `list_mail_accounts()`
- [x] Wire account discovery into startup → folder tree for the default account
- [x] `lookout-mail`: `AccountSession` actor — LOGIN/XOAUTH2, LIST (SPECIAL-USE), SELECT, whole-folder envelope fetch, IDLE loop with instant command interruption
- [x] Format-versioned SQLite cache for mailboxes/messages (fast first paint before live fetch)
  - [x] Flat-file `.eml`/attachment cache — attachment bytes are fetched on demand (`UID FETCH BODY.PEEK[<part>]` + transfer-decoding), cached as flat files keyed by `(mailbox, uidvalidity, uid, part)` under `$XDG_CACHE_HOME/lookout/mail/attachments/`, and saveable from an attachment strip in the reading pane (`Gtk.FileDialog` save; XDG-portal friendly). `EmailBody::parts` metadata rides on every summary so the on-demand fetch can target the right part number
  - [ ] Whole-message `.eml` export/cache (the partial-fetch path never assembles a raw message; a full `BODY.PEEK[]` fetch + "Save as .eml" is still open)
- [x] Folder tree UI wired to live data (`Gtk.TreeListModel`)
- [x] Message list UI (`message_list.rs`): Outlook-style rows, collapsible date sections, pane header with sync/sort-key/sort-direction/favorite controls
  - [x] Body previews: second `BODY.PEEK[]<0.16384>` pass, snippets carried across resyncs
  - [x] Message-list filter (All/Unread/Flagged) applied at the `repopulate` choke point
- [x] Message viewer: body fetch → `mail-parser` → sandboxed WebView with `Gtk.TextView` fallback
  - [x] `BODYSTRUCTURE`-driven partial fetch (text parts only by part number, assembled-body JSON cache)
  - [ ] Inline `cid:` image resolution via a custom WebKit URI scheme handler
- [x] Compose window: plain-text body, SMTP send (XOAUTH2/password), `APPEND` to Sent — validated end-to-end against Gmail
  - [x] reply/reply-all/forward entry points opening in place of the reading pane (threading headers carried)
  - [x] Rich-text/contenteditable WebView body — `multipart/alternative` send, default mode settable under Config → Mail
  - [x] Recipient chip widget with autocomplete (local address book + CardDAV suggestions)
  - [x] Autosave drafts (5s tick, in-place `APPEND` to Drafts, saved on Cancel, deleted on Send)
  - [ ] Multiple sending identities (currently always sends as the account's own address)
- [x] Connection lifecycle: state machine, backoff reconnect, pre-timeout IDLE re-issue, `Gio.NetworkMonitor`-driven reconnect
- [x] `Adw.ToastOverlay` for connection/send errors
- [x] Flatpak manifest spike (`flatpak/io.github.gavindi.Lookout.json`) with GOA `--talk-name` permission confirmed against Geary's shipping manifest
  - [ ] Generate `cargo-sources.json` via Flathub's `flatpak-cargo-generator.py` (~150 transitive deps)
  - [ ] Install `flatpak-builder` + `org.gnome.Sdk//49` + the rust-stable SDK extension and do a real build+run (none of that is available in this environment)
  - [ ] Fix "Open Online Accounts Settings" for sandboxed runs - it currently shells out via `std::process::Command`; needs `org.gnome.ControlCenter` D-Bus activation instead

### Testing/verification (Phase 1)

- [x] Unit tests: JWZ threading, mailbox-role heuristics, header/address parsing, SQLite cache round-trip
- [x] Manual smoke test against real GOA accounts (OAuth2 + read-only IMAP, live against Gmail)
- [x] Manual send test: SMTP send + `APPEND` to Sent verified live (Gmail)
- [x] Fake-GOA D-Bus test service (`crates/goa/tests/fake_goa.rs`) — passing under `dbus-run-session -- cargo test -p lookout-goa --test fake_goa`
- [x] GreenMail protocol integration test (`crates/mail/tests/imap_integration.rs`, `testcontainers`) — written but not executed (no Docker here); `#[ignore]`d, run with `cargo test -p lookout-mail --features test-utils --test imap_integration -- --ignored` and treat that first run as the real validation
- [x] `test-fixtures/` sample `.eml` set + dev-only "open .eml" debug action
- [x] CI: fmt, clippy `-D warnings`, unit/fake-goa/GreenMail tests, build-only job against GTK4/libadwaita/webkitgtk dev packages
- [x] Manual milestone smoke test (compose+send and reconnect-after-sleep on OAuth2 and password accounts)

## Phase 2 — Mail advanced (roadmap)

- [x] Message delete — IMAP MOVE (RFC 6851) else COPY + STORE `\Deleted` + EXPUNGE; backs the toolbar Delete button
- [x] Archive / Report-as-junk (same MOVE/COPY+EXPUNGE path to the account's Archive/Junk mailbox)
- [x] Client-side snooze — local `snoozed` SQLite table, resurfaced on next sync; backs the Snooze button
- [x] Ribbon-style Home/View tabs — Home toolbars + View layout toggles (persisted via GSettings)
- [x] Message-list sorting (date/sender/subject, either direction) + favorites section pinned to the folder tree (GSettings)
- [x] Full-text search: per-account SQLite FTS5 index + IMAP `SEARCH` fallback (SearchBar, 300ms debounce, launches from any folder)
- [ ] Internal drag-drop (move/tag) + external drag-out (`.eml`/`.zip`)
- [ ] Collapsible thread UI (reuse the folder tree's `TreeListModel` trick)
- [x] Color-tag keywords (`$Lookout-tag-*` namespace) + "Manage tags…" dialog — server-side `STORE` atoms, cache-patched, per-tag row dots, `PERMANENTFLAGS \*` required
- [x] Recipient-chip/autocomplete composer widget (see Phase 1 compose entry)
- [x] Message flags (`STORE`) — mark-as-read on open (via `BODY.PEEK`) and toolbar Flag toggle (`\Flagged`)
- [x] Unified mailbox + cross-account views; full multi-account switcher
- [x] Batch actions + `Gtk.MultiSelection` — multi-select rows with per-row checkboxes, batched move/flag/snooze per (account, mailbox) group, new Mark read/unread button
- [x] Hover quick-actions
- [ ] Physical-keycode global keyboard shortcuts
- [ ] Print support
- [ ] List-Unsubscribe banner
- [ ] External-content trust-sender flow
- [ ] RFC 8098 read-receipt (MDN) generation

## Phase 3 — Calendar (roadmap)

- [x] `lookout-dav`: thin CalDAV/WebDAV layer over `reqwest` — RFC 4791 discovery + `calendar-query` REPORT, mirroring `lookout-mail`'s actor architecture (5-minute polling instead of IMAP IDLE)
- [x] iCalendar modeling + recurrence expansion (`icalendar` + `rrule`, `TZID`→UTC via `chrono-tz`)
- [x] GOA `Calendar` interface for endpoint + credentials (password-slot id + Google `Calendar.Uri` special-casing still unverified live)
- [x] Custom-drawn month view (Outlook-style, Sunday-first grid, mini-calendar sidebar + "My calendars" checklist filter)
- [x] Week/Day/agenda-view + split view: all-day band over a scrollable positioned-chip hour timeline, auto-scroll-to-now, tooltips
- [ ] Drag-reschedule; recurring edit scopes (this / this-and-following / all)
- [ ] iMIP invitation banners in the mail viewer
- [ ] `.ics` import / webcal subscription
- [x] Mini-calendar sidebar widget (calendar view's 240px sidebar + compact copy docked on the Mail screen with agenda list)
- [ ] VTODO tasks
- [ ] Birthday calendar synthesized from Contacts
- [ ] `Gio.Notification` event alerts
- [x] Event create/edit (the "New Event" toolbar button is live)

## Phase 4 — Contacts (roadmap)

- [x] Shared `lookout-dav` plumbing for CardDAV (incl. RFC 6578 sync-collection REPORT)
- [x] vCard parser/writer (RFC 6350) — hand-rolled in `lookout-core`, wired through `lookout-dav`
- [x] Contacts UI tab ("People" in nav rail/toolbar)
  - [x] Split-pane layout: left navigation tree, right content list
  - [x] Per-account category tree (account header + Your Contacts/Favourites/Your contact lists/Deleted, plus cross-account "Categories" section)
  - [x] Selection wiring: left selection drives the right-side query
  - [x] Right pane contact rows (name, primary email, avatar/org); Favourites star toggles persisted in the UI-state database
  - [x] Contact details dialog (emails, phones, addresses, org/title, notes)
  - [x] Deleted bucket (in-memory diff of CardDAV polls)
- [ ] Address book CRUD, groups, vCard import/export with duplicate detection
- [ ] Shared `ContactsProvider` trait consumed by mail composer, calendar attendees, and the contacts app
  - [x] Mail composer consumes `ContactsProvider` (mail-cache + CardDAV suggestions)
  - [ ] Calendar attendees and dedicated contacts app still pending

## Phase 5 — Settings/theming (roadmap)

- [x] `AdwPreferencesWindow`-mirroring in-window Config view (`config_view.rs`, third nav-rail button) — live account overview, Appearance (animate transitions, window background), Mail switches, "Clear all caches"; General/Layout/Privacy/Apps still disabled placeholders
- [x] GSettings (scalars, schema from system or `build.rs` `OUT_DIR`; in-memory fallback) + serde config file (`app_config.rs`: identities, folder-role overrides)
- [ ] libadwaita named-color theming; optional bundled flat-token themes (web-only CSS-injection "skin" themes have no native equivalent and are out of scope)