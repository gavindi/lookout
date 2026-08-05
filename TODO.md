# TODO

Derived from the implementation plan (`webmail/` → Lookout port). Phase 1 is the current milestone; Phases 2-5 are the scoped roadmap for later work.

## Phase 1 — Mail MVP

- [x] Cargo workspace scaffolding (`lookout-core`, `lookout-goa`, `lookout-mail`, `lookout-app`)
- [x] `.desktop` file (`data/io.github.gavindi.Lookout.desktop`, passes `desktop-file-validate`), AppStream metainfo (passes `appstreamcli validate` bar the placeholder homepage URL), placeholder app icon
  - [ ] GResource bundle - nothing to bundle yet since the UI is built programmatically with no `.ui` XML templates (a deliberate Phase 1 choice, see the plan)
- [x] `AdwApplication` shell with empty-state page ("No mail accounts configured") that spawns `gnome-control-center online-accounts`
- [x] Window chrome polish: background image behind the mail panes, translucent folder pane (and empty-state reading pane) so it shows through, per-`MailboxRole` folder icons in the tree, a view-switcher rail on the window's left edge (Mail only so far - scaffold for Phase 3/4's Calendar/Contacts views), and an Outlook-style menu bar + command toolbar stacked below the title bar. File→Quit and Help→About are wired to real `gio::SimpleAction`s; Delete/Archive/Report/Snooze, Reply/Reply All/Forward, and the Home/View ribbon tabs are now live (see Phase 2 and below); the toolbar's Flag/More buttons and the message-list header's Filter button remain visible-but-disabled placeholders
- [x] `lookout-goa`: zbus proxies for `ObjectManager`/`Account`/`Mail`/`PasswordBased`/`OAuth2Based`; `list_mail_accounts()`
- [x] Wire account discovery into startup → folder tree for the default account
- [x] `lookout-mail`: `AccountSession` actor — `LOGIN`/`AUTHENTICATE XOAUTH2`, `LIST (SPECIAL-USE)`, `SELECT`, bounded envelope fetch, IDLE loop with instant command interruption
- [x] SQLite cache for mailboxes/messages (`$XDG_CACHE_HOME/lookout/mail/`), used for fast first paint before the live IMAP fetch completes
  - [ ] Flat-file `.eml`/attachment cache (bodies are still fetched fresh each time, not cached)
- [x] Folder tree UI wired to live data (`Gtk.TreeListModel`)
- [x] Message list UI wired to live data (`message_list.rs`) - Outlook-style rows (initials avatar, sender/subject/date columns, dimmed body preview, unread accent), grouped under collapsible date sections (Today / Yesterday / This Week / … / Older) on a `Gtk.TreeListModel`, plus a pane header naming the open folder/account with Sync, sort-key (date/sender/subject), sort-direction and folder-favorite controls. `thread_key` is computed but conversations are still not grouped in the UI (see Phase 2's collapsible thread UI)
  - [x] Body previews - `sync_mailbox` runs a second `BODY.PEEK[]<0.16384>` pass for previewless messages and the snippets are carried across resyncs via `Cache::load_previews`
  - [x] Message-list filter (unread/flagged) - the header's Filter button is a live `MenuButton` (radio items All/Unread/Flagged bound to a `win.list-filter` stateful action, mirroring the sort-key menu). The filter is applied inside `MessageListModel::repopulate` - the single choke point every rebuild passes through - against a new *unfiltered* source of truth: `repopulate` now stashes the full message set in the model before filtering (see `MessageListModel::all_messages`), so the `displayed` snapshot stays the filtered subset the next sync diffs against while `set_filter`/the sort controls re-render from the full set. The active filter is part of the no-op-rebuild comparison, so a flag flip on a filtered-out message costs no rebuild and `Unread`↔`All` round-trips without losing a message
- [x] Message viewer: body fetch → `mail-parser` → sandboxed WebView with `Gtk.TextView` fallback
  - [ ] Switch from whole-message fetch to `BODYSTRUCTURE`-driven partial fetch
  - [ ] Inline `cid:` image resolution via a custom WebKit URI scheme handler
- [x] Compose window: plain-text body, `mail-builder` MIME, `lettre` SMTP send (XOAUTH2/password), `APPEND` to Sent - validated live end-to-end against Gmail
  - [x] reply/reply-all/forward entry points - pre-filled subject/quoted body/`In-Reply-To`/`References` threading, Reply-All carries over To/Cc minus the account's own address. New/Reply/Reply-All/Forward all open in place of the reading pane's content (a `"compose"` page in its `gtk::Stack`) rather than a separate modal window
  - [x] Rich-text/contenteditable WebView body - a "Rich text" switch in the composer flips between the plain `Gtk.TextView` and a contenteditable WebKit `WebView` with a formatting toolbar (bold/italic/underline/strikethrough, lists, font size, text color, links); rich send emits `multipart/alternative` HTML+text, and the default mode is settable under Config → Mail
  - [x] Recipient chip widget with autocomplete - To/Cc (and a new Bcc field) are `RecipientEntry` widgets (`recipient_entry.rs`): each recipient is a removable pill in a wrapping `Gtk.FlowBox` with the text entry trailing the last chip, committed on Enter/comma/semicolon/Tab, removed by its × or by Backspace on an empty entry. Tokenizing respects quoted display names, so `"Lovelace, Ada" <ada@example.com>` stays one recipient; an implausible address is styled as a warning rather than rejected. Typing now merges two sources: a local address book harvested out of synced envelopes (`Cache::record_addresses`/`search_addresses`, ranked by correspondence frequency) plus CardDAV contacts fetched via `lookout-dav` and refreshed periodically in the background; the lookup remains synchronous from UI-owned state/read-side cache handles rather than an `AccountCommand` round trip, since a keystroke can't wait on the IMAP session
  - [x] Autosave drafts - a 5-second tick compares the composer's fields against the last saved snapshot and, when they differ (and aren't trivially empty), `APPEND`s the message to the account's Drafts mailbox via `AccountCommand::SaveDraft`, replacing the previous autosave in place by a stable per-compose-session `Message-ID`. Cancel saves once more so closing never loses work; Send deletes the stored draft first. Accounts without a Drafts mailbox get one `CREATE`d
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
- [x] Manual milestone smoke test covering compose+send and reconnect-after-sleep on both an OAuth2 and a password-based account

## Phase 2 — Mail advanced (roadmap)

- [x] Message delete - move-to-Trash via IMAP MOVE (RFC 6851) where supported, else COPY + STORE `\Deleted` + EXPUNGE - backs the toolbar's Delete button
- [x] Move-to-folder actions: Archive and Report-as-junk (move to the account's Archive/Junk mailbox via the same MOVE/COPY+EXPUNGE path) - backs the toolbar's Archive/Report buttons
- [x] Client-side snooze (hide a message and resurface it later - no IMAP-native equivalent, same approach as Gmail/Outlook desktop; tracked in a local SQLite `snoozed` table, re-checked on next sync rather than a dedicated timer) - backs the toolbar's Snooze button
- [x] Ribbon-style Home/View tab content - Home and View are now real, mutually-exclusive ribbon tabs in the menu bar row that switch the ribbon content row below. Home shows the command toolbar (New mail, Delete, Archive, Report, Reply, Reply All, Forward, Snooze); View is a new "Layout" group of pane-visibility toggles (Folder pane / Reading pane / Calendar overview). The tabs are Mail-only - they grey out while a Calendar/Config module is active (those keep their own non-tabbed command toolbars), and the layout toggles are session-only until Phase 5 GSettings lands
- [x] Message-list sorting (By Date / By Sender / By Subject, either direction, applied at the single `MessageListModel::repopulate` choke point) and a Favorites section pinned to the top of the folder tree, starred from the list header - both session-only until Phase 5 GSettings lands
- [ ] Full-text search: SQLite FTS5 over cache + IMAP `SEARCH` fallback
- [ ] Internal drag-drop (move/tag) + external drag-out (`.eml`/`.zip`)
- [ ] Collapsible thread UI (reuse the folder tree's `TreeListModel` trick)
- [ ] Color-tag keywords (`$Lookout-tag-*` namespace) + tag management
- [x] Recipient-chip/autocomplete composer widget (see Phase 1's compose entry - `recipient_entry.rs`, with completions from a cache-backed address book)
- [x] Message flags (`STORE`) - `AccountCommand::StoreFlags` adds/removes IMAP system flags on a message, backing two things: opening a message in the reading pane marks it `\Seen` (bodies are fetched with `BODY.PEEK`, so the server never sets it), and the toolbar's Flag button toggles `\Flagged`, drawn as an amber marker in the message row. The command SELECTs the message's own folder when it isn't the open one - mark-as-read races folder switches, and the unified view mixes mailboxes - and the main loop's existing re-select puts the session back before the next IDLE. Successful stores patch the cached summary in place (`Cache::update_flags`) and re-emit from cache rather than re-syncing, so a mark-as-read costs one `STORE` and no fetch
- [x] Unified mailbox + cross-account views; full multi-account switcher
- [ ] Batch actions + `Gtk.MultiSelection`
- [x] Hover quick-actions
- [ ] Physical-keycode global keyboard shortcuts
- [ ] Print support
- [ ] List-Unsubscribe banner
- [ ] External-content trust-sender flow
- [ ] RFC 8098 read-receipt (MDN) generation

## Phase 3 — Calendar (roadmap)

- [x] `lookout-dav`: thin CalDAV/WebDAV layer over `reqwest` - RFC 4791 discovery (principal → calendar-home-set → calendar list) + `calendar-query` REPORT event fetch, mirroring `lookout-mail`'s actor architecture (`CalendarSession`, polling every 5 minutes in place of IMAP IDLE)
- [x] iCalendar modeling + recurrence expansion (`icalendar` + `rrule`) - `TZID`→UTC resolution via `chrono-tz`, window-bounded RRULE expansion
- [x] GOA `Calendar` interface for endpoint + credentials - discovery (`list_calendar_accounts`) confirmed live; the `calendar-password` credential-slot id and Google `Calendar.Uri` special-casing are implemented but still unverified against a real live account
- [x] Custom-drawn month view (Outlook-style, Sunday-first grid, mini-calendar sidebar + "My calendars" checklist filter)
  - [x] Week/Day/Agenda views and the Split-view layout (the command toolbar's view-switcher is fully live: Day, Work week, Week, Month and Split are mutually-exclusive segmented options backed by a shared-anchor view stack. Day/Week/Work week are read-only time grids - a fixed all-day band above a scrollable positioned-chip hour timeline; Month is the original Sunday-first grid; Split pairs the month grid with a chronological agenda list for the anchor's month. Today/prev/next navigate in the active view's natural unit)
- [x] Time-grid polish: scrollable hour timeline, positioned (not bucket-per-hour) event chips, multi-day event spans, agenda grouping by day (each Day/Week/Work week grid is now a split view - a fixed all-day band that never scrolls away above a vertically scrollable 24-hour timeline, chips positioned by their exact start/end time with side-by-side lanes for overlapping events and full-width spans for multi-day events, auto-scroll-to-now on view switch, and a hover tooltip + highlight ring per chip; the agenda lists group by day under "Today"/"Tomorrow"/date headers)
- [ ] Drag-reschedule; recurring edit scopes (this / this-and-following / all)
- [ ] iMIP invitation banners in the mail viewer
- [ ] `.ics` import / webcal subscription
- [x] Mini-calendar sidebar widget - both the Calendar view's own sidebar (240px) and a second copy docked to the Mail screen's far right (with a day-agenda list underneath, filtered to checked calendars), the latter at half width via the `.mini-calendar-compact` day-button metrics
- [ ] VTODO tasks
- [ ] Birthday calendar synthesized from Contacts
- [ ] `Gio.Notification` event alerts
- [ ] Event create/edit (the "New Event" toolbar button is still a disabled placeholder - the calendar view is read-only today)

## Phase 4 — Contacts (roadmap)

- [x] Shared `lookout-dav` plumbing for CardDAV (incl. RFC 6578 sync-collection REPORT)
- [x] vCard parser/writer (RFC 6350) — hand-rolled parser/writer in `lookout-core` with CardDAV parsing wired through `lookout-dav`
- [ ] Contacts UI tab
  - [ ] Layout scaffold: split-pane Contacts view with left navigation and right content list
  - [ ] Left pane model: per-account contact category tree/list (e.g. All contacts, groups, directories/address books)
  - [ ] Selection wiring: selecting an account/category updates the right-side list query/filter
  - [ ] Right pane list: contact rows (name, primary email, optional avatar/org) bound to the active left-side selection
  - [ ] Contact details dialog: clicking a contact opens a dialog showing full contact information (emails, phones, addresses, org/title, notes)
- [ ] Address book CRUD, groups, vCard import/export with duplicate detection
- [ ] Shared `ContactsProvider` trait consumed by mail composer, calendar attendees, and the contacts app
  - [x] Mail composer consumes `ContactsProvider` (mail-cache + CardDAV-backed suggestions)
  - [ ] Calendar attendees and dedicated contacts app still pending

## Phase 5 — Settings/theming (roadmap)

- [x] `AdwPreferencesWindow` mirroring Bulwark's tab taxonomy (General/Appearance/Layout/Mail/Privacy/Apps/Advanced) - in-window Config view exists (`config_view.rs`, third nav-rail button) with a live account overview, a live Appearance section ("Animate transitions" switch plus the "Window background" image picker and "Restore default background" action), a live Mail section ("Load images from the web" and "Rich text" switches), and a working Advanced → "Clear all caches" action; the General/Layout/Privacy/Apps sections are still disabled placeholders
- [ ] GSettings (scalars) + serde config file (relational data: identities, folder-role overrides, tag colors) - the layout toggles, "Animate transitions", the sort key/direction, folder favorites, and the Mail-section switches are still session-only; only calendar colours and the window-background choice persist today, as best-effort plain files under `$XDG_CONFIG_HOME/lookout/`
- [ ] libadwaita named-color theming; optional bundled flat-token themes (web-only CSS-injection "skin" themes have no native equivalent and are out of scope)
