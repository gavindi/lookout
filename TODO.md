# TODO

Phase 1 is the current milestone; Phases 2-5 are the scoped roadmap (derived from the `webmail/` → Lookout implementation plan).

## Phase 1 — Mail MVP

- [x] Cargo workspace scaffolding (`lookout-core`, `lookout-goa`, `lookout-mail`, `lookout-app`)
- [x] `.desktop` file + AppStream metainfo + placeholder icon
- [x] GResource bundle
- [x] `AdwApplication` shell with empty-state page that spawns `gnome-control-center online-accounts`
- [x] Window chrome: background image, translucent panes, per-role folder icons, view-switcher rail, Outlook-style menu bar + command toolbar
- [x] `lookout-goa`: zbus proxies for GOA interfaces; `list_mail_accounts()`
- [x] Wire account discovery into startup → folder tree for the default account
- [x] `lookout-mail`: `AccountSession` actor — LOGIN/XOAUTH2, LIST (SPECIAL-USE), SELECT, whole-folder envelope fetch, IDLE loop with instant command interruption
- [x] Format-versioned SQLite cache for mailboxes/messages
- [x] Flat-file attachment cache — on-demand `UID FETCH BODY.PEEK[<part>]` + transfer-decoding, keyed `(mailbox, uidvalidity, uid, part)`, saveable from the reading pane's attachment strip
- [x] Whole-message `.eml` export/cache — on-demand `BODY.PEEK[]` → flat-file raw-message cache (keyed `(mailbox, uidvalidity, uid)`), "Save as .eml…" in the ribbon More menu, whole-message body-fallback bytes reused
- [x] Folder tree UI wired to live data (`Gtk.TreeListModel`)
- [x] Message list UI (`message_list.rs`): Outlook-style rows, collapsible date sections, pane header with sync/sort/favorite controls
- [x] Body previews: second `BODY.PEEK[]<0.16384>` pass, snippets carried across resyncs
- [x] Message-list filter (All/Unread/Flagged)
- [x] Message viewer: body fetch → `mail-parser` → sandboxed WebView with `Gtk.TextView` fallback
- [x] `BODYSTRUCTURE`-driven partial fetch (text parts only by part number, assembled-body JSON cache)
- [x] Inline `cid:` image resolution via a custom WebKit URI scheme handler
- [x] Compose window: plain-text body, SMTP send (XOAUTH2/password), `APPEND` to Sent — validated end-to-end against Gmail
- [x] reply/reply-all/forward entry points in place of the reading pane (threading headers carried)
- [x] Rich-text/contenteditable WebView body — `multipart/alternative` send, default mode under Config → Mail
- [x] Recipient chip widget with autocomplete (local address book + CardDAV suggestions)
- [x] Autosave drafts (5s tick, in-place `APPEND` to Drafts, saved on Cancel, deleted on Send)
- [x] Multiple sending identities — per-account From dropdown in the composer (default = account's own address), display name in `From:`, per-identity Reply-To/Bcc, manage dialog (`settings.json`) from the composer and Config → Accounts
- [x] Connection lifecycle: state machine, backoff reconnect, pre-timeout IDLE re-issue, `Gio.NetworkMonitor`-driven reconnect
- [x] `Adw.ToastOverlay` for connection/send errors
- [x] Flatpak manifest spike with GOA `--talk-name` permission
- [x] Generate `cargo-sources.json` via `flatpak-cargo-generator.py` (CI `build.yaml` flatpak job)
- [x] Install `flatpak-builder` + `org.gnome.Sdk//49` + rust-stable SDK extension; real build+run (CI flatpak job builds the bundle)
- [x] Fix "Open Online Accounts Settings" for sandboxed runs — `online_accounts.rs` D-Bus activation (`org.freedesktop.Application.ActivateAction` on `org.gnome.Settings`, `ControlCenter.ActivatePanel` fallback, shell fallback)

### Testing/verification (Phase 1)

- [x] Unit tests: JWZ threading, mailbox-role heuristics, header/address parsing, SQLite cache round-trip
- [x] Manual smoke test against real GOA accounts (live Gmail)
- [x] Manual send test: SMTP + `APPEND` to Sent verified live (Gmail)
- [x] Fake-GOA D-Bus test service (`crates/goa/tests/fake_goa.rs`)
- [x] GreenMail protocol integration test (`crates/mail/tests/imap_integration.rs`) — written, `#[ignore]`d (no Docker here); first real run is the validation
- [x] `test-fixtures/` sample `.eml` set + dev-only "open .eml" debug action
- [x] CI: fmt, clippy `-D warnings`, unit/fake-goa/GreenMail tests, build-only job against GTK4/libadwaita/webkitgtk
- [x] Manual milestone smoke test (compose+send and reconnect-after-sleep)

## Phase 2 — Mail advanced (roadmap)

- [x] Message delete — IMAP MOVE (RFC 6851) else COPY + STORE `\Deleted` + EXPUNGE
- [x] Archive / Report-as-junk (same MOVE/COPY+EXPUNGE path)
- [x] Client-side snooze — local `snoozed` SQLite table, resurfaced on next sync
- [x] Ribbon-style Home/View tabs — Home toolbars + View layout toggles (GSettings)
- [x] Message-list sorting (date/sender/subject) + favorites section pinned to the folder tree (GSettings)
- [x] Full-text search: per-account SQLite FTS5 index + IMAP `SEARCH` fallback (SearchBar, 300ms debounce)
- [ ] Internal drag-drop (move/tag) + external drag-out (`.eml`/`.zip`)
- [ ] Collapsible thread UI
- [x] Color-tag keywords (`$Lookout-tag-*`) + "Manage tags…" dialog — server-side `STORE` atoms, cache-patched, per-tag row dots
- [x] Recipient-chip/autocomplete composer widget (see Phase 1)
- [x] Message flags (`STORE`) — mark-as-read on open, toolbar Flag toggle
- [x] Unified mailbox + cross-account views; full multi-account switcher
- [x] Batch actions + `Gtk.MultiSelection` — per-row checkboxes, batched move/flag/snooze per (account, mailbox), Mark read/unread
- [x] Hover quick-actions
- [ ] Physical-keycode global keyboard shortcuts
- [ ] Print support
- [x] List-Unsubscribe banner — `Adw.Banner` in the reading pane, RFC 8058 one-click POST with mailto fallback into the composer
- [ ] External-content trust-sender flow
- [ ] RFC 8098 read-receipt (MDN) generation

## Phase 3 — Calendar (roadmap)

- [x] `lookout-dav`: thin CalDAV/WebDAV layer over `reqwest` — RFC 4791 discovery + `calendar-query` REPORT (5-minute polling)
- [x] iCalendar modeling + recurrence expansion (`icalendar` + `rrule`, `TZID`→UTC via `chrono-tz`)
- [x] GOA `Calendar` interface for endpoint + credentials
- [x] Custom-drawn month view (Sunday-first grid, mini-calendar sidebar + "My calendars" checklist filter)
- [x] Week/Day/agenda view + split view: all-day band, auto-scroll-to-now, tooltips
- [x] Select-then-edit in the main grids: clicking a Month-grid day (highlight + re-anchor every view) or a Day/Week time slot (highlight) selects it; clicking the selection again opens the New Event editor prefilled for that day (9am) or time
- [x] Click-and-drag range selection in the Day/Week grids — dragging across hours and day columns highlights a whole range (interior columns fill as full days); clicking it opens the editor spanning exactly the selection (a `suggested_end` prefill, so a 9:00–11:00 drag is a two-hour event)
- [ ] Drag-reschedule; recurring edit scopes (this / this-and-following / all)
- [x] iMIP invitation banners in the mail viewer — `text/calendar` parts fetched with the body, `METHOD:REQUEST` (Accept/Maybe/Decline → RFC 6047 `METHOD:REPLY` + calendar upsert), `METHOD:CANCEL` (remove-from-calendar), `METHOD:REPLY` (informational)
- [ ] `.ics` import / webcal subscription
- [x] Mini-calendar sidebar widget (240px sidebar + compact copy docked on the Mail screen)
- [ ] VTODO tasks
- [ ] Birthday calendar synthesized from Contacts
- [ ] `Gio.Notification` event alerts
- [x] Event create/edit ("New Event" toolbar button)
  - [x] Attendee invites (`ATTENDEE`/`ORGANIZER`), a recurrence/series builder (previously read-only display only), a video-call link (`CONFERENCE`), and categories/sensitivity/busy-free/reminder (`CATEGORIES`/`CLASS`/`TRANSP`/`VALARM`) via a new toolbar
  - [x] Live preview pane (mini-calendar + day schedule strip) reusing the main Calendar tab's own widgets

## Phase 4 — Contacts (roadmap)

- [x] Shared `lookout-dav` plumbing for CardDAV (incl. RFC 6578 sync-collection REPORT)
- [x] vCard parser/writer (RFC 6350) — hand-rolled in `lookout-core`, wired through `lookout-dav`
- [x] Contacts UI tab ("People")
  - [x] Split-pane layout: left navigation tree, right content list
  - [x] Per-account category tree (account header + Your Contacts/Favourites/Your contact lists/Deleted, plus "Categories" section)
  - [x] Selection wiring: left selection drives the right-side query
  - [x] Right pane contact rows (name, primary email, avatar/org); Favourites star persisted in the UI-state database
  - [x] Contact details dialog (emails, phones, addresses, org/title, notes)
  - [x] Deleted bucket (in-memory diff of CardDAV polls)
- [ ] Address book CRUD, groups, vCard import/export with duplicate detection
- [ ] Shared `ContactsProvider` trait consumed by mail composer, calendar attendees, and the contacts app
  - [x] Mail composer consumes `ContactsProvider`
  - [x] Calendar attendees consume it too - autocomplete merges mail-history + CardDAV suggestions across every connected account (not scoped to one, since an invitee isn't tied to the event's own calendar account)
  - [ ] Dedicated contacts app still pending

## Phase 5 — Settings/theming (roadmap)

- [x] In-window Config view (`config_view.rs`) — live account overview, Appearance (transitions, window background), Mail switches, "Clear all caches"; General/Layout/Privacy/Apps placeholders
- [x] GSettings (schema from system or `build.rs` `OUT_DIR`; in-memory fallback) + serde config file (`app_config.rs`)
- [ ] libadwaita named-color theming; optional bundled flat-token themes
