# Changelog

## 0.6.8 (2026-08-03)

### Added

- **UI**: the Config view's "Advanced" section is now live (rather than a disabled placeholder) with a "Clear all caches" action that deletes the on-disk mail SQLite caches (`$XDG_CACHE_HOME/lookout/mail/`), drops the in-memory calendar occurrences, and asks every connected Mail/Calendar account to resync so the caches rebuild from the servers immediately instead of on next launch.

## 0.6.7 (2026-08-03)

### Added

- **UI**: a new Config view on the left nav rail (third toggle button, `preferences-system-symbolic`, grouped with Mail/Calendar) showing a settings screen (`config_view.rs`): a live account overview - one row per connected Mail account (display name, email, IMAP/SMTP `host:port` from the account's stored connection config) and per Calendar account (display name, CalDAV base URL) - plus disabled placeholder sections mirroring the Phase 5 settings taxonomy (General/Appearance/Layout/Mail/Privacy/Apps/Advanced). Rows repopulate on every switch to the view and again whenever account discovery lands. An "Add account…" row (and matching command-toolbar button) opens GNOME Online Accounts settings, same invocation as the empty-state page's button.
- **UI**: the Config nav-rail button is anchored to the bottom of the rail (below a `vexpand(true)` spacer), matching the Mail/Calendar buttons at the top - a standard app-shell "settings at the bottom" placement.

## 0.6.6 (2026-08-02)

### Changed

- **UI**: the message-row hover quick actions (Archive/Delete/Reply) are now densely packed (zero spacing between buttons) and sit on a solid, opaque pill background (new `.hover-quick-actions`/`.hover-quick-action` CSS classes) overlaid on top of the message line, so they no longer blend transparently into the row content.
- **Build**: `build.sh` now builds everything in the repo - the Rust Cargo workspace and the Next.js webmail frontend - and reports the resulting binary path (`target/debug/lookout` or `target/release/lookout`) at the end of the build.

## 0.6.5 (2026-08-02)

### Changed

- **UI**: the message list now shows each message's date only, formatted according to the system's regional settings (GLib's locale-aware `%x`). Emails received within the last 24 hours instead show the time (`%X`) rather than the date, so fresh messages are easier to spot at a glance.

## 0.6.4 (2026-08-01)

### Fixed

- **Build**: resolved the app build breakage in the window UI by updating the message-row implementation to match the current GTK 4 APIs and mailbox identifier layout, restoring a successful local build and launch.

### Added
 - **Hover Quick Actions**: for message rows are now working as well.
 - **Calendar Sidebar**: the "My calendars" list now repopulates from GOA-discovered CalDAV collections as accounts connect, and newly discovered calendars are shown by default in the month view.

## 0.6.3 (2026-07-30)

### Added

- **UI**: the reading pane now has its own header above the message body (new `message_header` module) - subject line, a colored initials avatar, sender name/email, a "To:" recipient line, the message date, and a second set of Reply/Reply-All/Forward buttons (duplicating the top command toolbar's by design). Populated the instant a message is selected from its `EmailSummary` rather than waiting on the async body fetch, and hidden automatically (via the reading pane's existing stack-page signal) whenever the empty placeholder or the in-place composer is showing instead.

## 0.6.2 (2026-07-30)

### Changed

- **UI**: New Message, Reply, Reply All, and Forward now open the composer in place of the reading pane's content instead of as a separate modal window - a new `"compose"` page in the reading pane's existing `gtk::Stack`, matching how it already flips between the HTML/plain-text/empty views. Cancel or Send returns to whatever was showing before (the same message for Reply/Forward, the empty placeholder for New Message). Selecting a different message while composing silently abandons it, consistent with the app's existing no-confirmation-dialog convention.

## 0.6.1 (2026-07-30)

### Added

- **Mail**: Reply, Reply All, and Forward toolbar buttons now open the compose window pre-filled from the currently selected/open message - `Re: `/`Fwd: ` subject (not doubled up if already present), quoted original body ("On {date}, {sender} wrote:" with `> `-prefixed lines for Reply/Reply-All; a "Forwarded message" header block with the original body verbatim for Forward), and correct `In-Reply-To`/`References` threading headers for Reply/Reply-All (Forward starts a new, unthreaded conversation). Reply-All also carries over the original's other To/Cc recipients, excluding the replying account's own address.
- **Mail**: the compose window gained a Cc field (previously To/Subject/body only), needed for Reply-All to actually carry Cc recipients through to the sent message.

### Fixed

- **Mail**: fixed a would-be double-bracketing bug in the new reply-threading code before it ever shipped - `mail-builder`'s outgoing `Message-Id`/`References`/`In-Reply-To` headers add their own `<>` wrapping, so raw header values pulled from an original message (which already have brackets) are now stripped of them first.

## 0.6.0 (2026-07-30)

### Added

- **Mail**: Delete, Archive, and Report-as-junk toolbar buttons are now live - each moves the selected message into the account's Trash/Archive/Junk mailbox via IMAP MOVE (RFC 6851) where the server supports it, falling back to COPY + STORE `\Deleted` + EXPUNGE otherwise. If an account has no mailbox for that role, the move fails with a toast rather than silently permanent-deleting.
- **Mail**: Snooze is now live - hides the selected message from the list until tomorrow at 9:00 AM local time. Purely client-side (IMAP has no native snooze concept): tracked in a new `snoozed` table in the local per-account SQLite cache, filtered out of what's shown to the UI while still being fetched/cached normally. Snooze expiry is only re-checked on the next sync (IDLE-triggered or explicit), not on a dedicated timer.
- All four actions surface an `AdwToast` on completion ("Deleted"/"Archived"/"Reported as junk"/"Snoozed until tomorrow 9:00 AM") and trigger an immediate folder + message-list resync, matching the existing toast-only feedback convention.

## 0.5.3 (2026-07-30)

### Added

- **UI**: the Mail screen now has a calendar overview pane docked to the far right of the window, spanning the same full height as the nav rail (a sibling in `window_body`, not nested inside `root_stack`) - a mini month-picker plus a day-agenda list below it. Clicking a date shows that day's events (filtered to checked calendars) and triggers a background `CalendarCommand::SyncMonth` for that month without disturbing the main Calendar view's own displayed month. Today's events populate automatically shortly after a calendar account connects, via each account's existing auto-sync-current-month behavior. Hidden while the Calendar view is active, since that view already has its own sidebar mini-calendar. Resizable via a new `content_and_overview_paned` split, matching every other pane boundary in the app.

### Fixed

- **UI**: the overview pane's mini-calendar was rendering roughly 2x wider than intended and had no resize handle - it was appended directly into a plain `gtk::Box` with nothing constraining its natural width (unlike the Calendar view's own sidebar, which caps width via `build_sidebar()`'s `width_request`). Added a matching `width_request(240)` and moved the pane into the new resizable `content_and_overview_paned` split.

## 0.5.2 (2026-07-30)

### Changed

- **UI**: the main (large) calendar grid now has a dark grey (`#2e2e32`) rounded-corner background (new `.calendar-main-background` class) instead of showing the window background image through it, with margins matching the sidebar's `card_section` gap so both panels sit evenly off the paned divider/window edges.
- **UI**: the nav rail now spans the window's full height *below the title bar* rather than only beside the mail/calendar content - `window_header` stays the one real title bar spanning the full window width at the very top; the menu bar, command toolbar, and content now live in a new `inner_content_box` to the rail's right instead of stacking above it as `AdwToolbarView` top bars.
- **UI**: the menu bar and icon command toolbar are now visually grouped - a shared black `.window-toolbars-background` behind both rows, with the icon toolbar additionally boxed into its own `#2e2e32` dark grey, rounded-corner (`.window-icon-toolbar-background`), 6px-margined subgroup distinct from the menu bar above it.

## 0.5.1 (2026-07-30)

### Added

- **Calendar**: reworked the calendar view to match an Outlook-style month layout. The main grid is now Sunday-first with day numbers top-left in a flat hairline grid (new `.calendar-day-cell`/`.calendar-today-cell` CSS classes) instead of libadwaita's rounded `.card` panels; today gets a bordered highlight. A new resizable sidebar (mirroring the Mail folder pane's split) holds a mini month-picker (`MiniCalendar`, clicking a day jumps the main grid to that month) and a live "My calendars" checklist - unchecking a calendar actually filters its events out of the grid, via new `refresh_calendar_checklist`/`refresh_displayed_calendar_view` helpers. A Calendar-specific command toolbar (New event, Day/Work week/Week/Month/Split-view segmented control, Filter/Share/Print - all disabled placeholders except the working Month view) now swaps in for the Mail toolbar via a new `view_toolbar_stack` when the nav rail's Calendar button is active.

## 0.5.0 (2026-07-30)

### Added

- **Calendar**: Phase 3 basics - a new `lookout-dav` crate is a CalDAV client mirroring `lookout-mail`'s architecture: RFC 4791 discovery (principal → calendar-home-set → calendar list), a `calendar-query` REPORT event fetch over `reqwest`, iCalendar parsing (`icalendar` crate, with `TZID`→UTC resolution via `chrono-tz`), and window-bounded RRULE expansion (`rrule` crate). A `CalendarSession` actor mirrors `AccountSession`'s backoff/command-loop shape, polling every 5 minutes in place of IMAP IDLE (CalDAV has no long-poll equivalent).
- **Calendar**: `lookout-goa` now discovers GOA `Calendar`-interface accounts (`list_calendar_accounts`) alongside Mail, as a fully independent account set - a Calendar-only account (no Mail interface) is now correctly picked up, confirmed via an extended fake-GOA D-Bus test.
- **Calendar**: `lookout-core` gains `CalendarId`/`EventUid` ids and `CalendarInfo`/`CalendarEvent`/`EventOccurrence` types, following the existing `Mailbox`/`MailboxId` conventions.
- **UI**: the nav rail's Mail button is now grouped with a new Calendar button, switching to a read-only month-grid view (`calendar_view.rs`) with prev/next/Today navigation. Backed by `spawn_calendar_discovery`/`connect_calendar_account`, mirroring the existing Mail account-discovery wiring, unioning every connected calendar account's events for the displayed month.
- **Testing**: 20 new `lookout-dav` unit/integration tests, including a full `wiremock`-backed discover→list-calendars→fetch-events flow against a mocked CalDAV server.

### Known limitations (tracked for follow-up in `TODO.md`)

- Read-only month view only - no week/day/agenda views, event create/edit/drag, iMIP invitation banners, `.ics` import/webcal, VTODO tasks, birthdays, or notifications yet.
- The GOA `calendar-password` credential-slot id and whether Google's `Calendar.Uri` needs special-casing beyond the generic discovery flow are both unverified against a real live account.

### Fixed

- **Calendar**: a `RefCell` double-borrow panic in `calendar_view::build()` that crashed the app on startup - the initial `set_month` call was passed a still-borrowed value from the same `RefCell` it then tried to `borrow_mut()`.

## 0.4.0 (2026-07-29)

### Added

- **UI**: an Outlook-style menu bar (File / Home / View / Help) and command toolbar (New mail, Delete, Archive, Report, Flag/Unflag, Snooze, More) now span the full window width above the nav rail and mail panes, stacked below the title bar via `AdwToolbarView::add_top_bar`. File → Quit and Help → About are wired to real `gio::SimpleAction`s (`app.quit`, `app.about`, the latter presenting a real `AdwAboutDialog`); everything else without a backing implementation yet (Home, View, Delete, Archive, Report, Flag/Unflag, Snooze, More) is a real but disabled button rather than a dead click. `compose_button` ("New mail") moved out of the header bar into this new toolbar.

## 0.3.7 (2026-07-29)

### Changed

- **UI**: the window background image now renders at full opacity - removed the `set_opacity(0.5)` call on the `gtk::Picture`, which had been carried over from earlier tuning.
- **UI**: the view-switcher rail now has a 6px left margin, matching the gap `card_section`'s default margin already puts between the rail and the folder pane, so the spacing reads as even on both sides of the rail.

## 0.3.6 (2026-07-29)

### Changed

- **UI**: the view-switcher rail's "Mail" button now shows Lookout's own full-color app icon (`data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg`, embedded the same `include_bytes!` way as the background image) instead of the flat `mail-unread-symbolic` icon.

## 0.3.5 (2026-07-29)

### Added

- **UI**: a narrow view-switcher rail now runs along the window's very left edge, outside the folder pane, holding a single active "Mail" icon button for now. Deliberately unstyled (no `.card`, no background), so the window background image shows straight through it. Sits outside `root_stack` so it stays visible on both the "no accounts" empty state and the normal mail view. Only one view exists today, but the button is grouped so more views (Calendar/Contacts/etc., mirroring the reference webmail app's `NavigationRail`) can be appended later as mutually-exclusive toggle buttons.

## 0.3.4 (2026-07-29)

### Added

- **UI**: folder-pane tree rows now show a role-appropriate icon to the left of each folder's label (Inbox/Sent/Drafts/Trash/Junk/Archive each get a distinct icon, other folders get a generic folder icon), mirroring the reference webmail app's per-role sidebar icons. Driven by the `MailboxRole` each folder already carries - no new data plumbing needed, just a `role` -> icon-name mapping in `window.rs`. Account group header rows stay icon-less.

## 0.3.3 (2026-07-28)

### Changed

- **UI**: swapped the embedded window background image from `backgropund1.jpg` to `background2.png` (`Assets/backgrounds/`), still compiled into the binary via `include_bytes!` and drawn the same way (reduced opacity, behind the mail panes).

## 0.3.2 (2026-07-28)

### Changed

- **UI**: the folder pane and message-list pane now sit flush against each other - no gap, no rounded corners on the touching edge (new `.card-flush-end`/`.card-flush-start` CSS classes), and the `Gtk.Paned` handle between them shrinks from 12px to 6px with no painted background (scoped via a new `.seamless-paned` class so the message/reading split keeps its usual handle). Hovering the boundary still shows a resize cursor and drags as before; only the visual gap and handle went away.

## 0.3.1 (2026-07-28)

### Changed

- **UI**: the reading pane's empty state ("No Message Selected") no longer shows an icon or title - it's now a bare placeholder that goes fully transparent (zero alpha, via a new `.reading-pane-transparent` CSS class toggled off the reading stack's `visible-child-name`) so the window background image shows straight through with no card tint. The pane reverts to its normal opaque card as soon as a message is displayed.

## 0.3.0 (2026-07-28)

### Added

- **UI**: the main window now renders a background image (`Assets/backgrounds/backgropund1.jpg`) behind the mail panes, embedded into the binary at compile time and drawn at reduced opacity so it doesn't compete with the foreground UI.

### Changed

- **UI**: the folder pane's translucent background is now a hardcoded black at 50% alpha instead of tracking the current theme's card color, so it reads consistently against the new background image in both light and dark mode.

## 0.2.3 (2026-07-28)

### Added

- **Accounts**: full multi-account support - every GOA account with Mail enabled now connects and syncs concurrently, not just the first one discovered. The folder pane shows one collapsible group per account (auto-expanded on first load), each expanding into that account's own folder tree. Confirmed live against 3 real Gmail accounts connecting and syncing their inboxes simultaneously.
- **Accounts**: compose's "From" address now defaults to whichever account's folder or message is currently selected, instead of being hardcoded to a single connected account.

### Changed

- **UI**: the reading pane now has a 300px minimum-height floor so it can't be squeezed down to something unusably short if the window itself is resized very short.
- **Accounts**: `lookout-goa`'s `GoaClient` is now `Clone` (cheap - `zbus::Connection` is itself Arc-backed), so one D-Bus connection is shared across all accounts' credential providers instead of opening a redundant one per account.

### Fixed

- **Accounts**: a background IDLE resync on one account could no longer clobber the message list while viewing a different account's folder - only `MessagesUpdated` events matching the currently-selected mailbox are applied to the list.

## 0.2.2 (2026-07-28)

### Changed

- **UI**: replaced the nested `AdwNavigationSplitView`s with nested `Gtk.Paned`, giving the three mail panes (Folders / Messages / Reading) a real draggable resize handle. Each pane now renders as its own rounded `.card` panel with a visible gap between them, header-less (the per-card `AdwHeaderBar`s were dropped entirely - titles weren't load-bearing and removing them let the cards read more cleanly). The compose button moved from the Messages pane's header to the single top-level window header bar.
- **UI**: default window size increased from 1100x720 to 1600x900.

### Fixed

- **UI**: every per-card header bar was independently showing its own minimize/maximize/close buttons (four sets of window controls in one window) - `AdwNavigationSplitView`/`AdwNavigationPage` used to coordinate which single header owned the window chrome, and that coordination was lost when they were replaced. Fixed by giving the window exactly one real title bar and disabling title buttons everywhere else. Moot now that the per-card headers are gone entirely, but the underlying lesson (something has to own the window chrome once you stop using the Navigation widgets) stays relevant for future layout work.
- **UI**: removed `Gtk.Paned`'s default painted grey separator line via a scoped CSS provider - the card margins already provide the visual gap between panes, so the painted handle on top of that just looked like a stray line. The handle keeps a comfortable draggable hit-area; only its background/border painting was stripped.

## 0.2.1 (2026-07-28)

### Fixed

- **Mail**: the HTML reading pane always rendered blank white, regardless of content. The `decide-policy` handler installed to block link-clicks from navigating the viewer was blocking *every* `NavigationAction` decision, including the WebView's own initial `load_html()` load - so no message body ever actually rendered, silently. Now distinguished via `NavigationAction::is_user_gesture()`: real clicks are still blocked, the programmatic initial load isn't. Confirmed fixed against a real HTML email (a Google Calendar invite, full styling and all).

## 0.2.0 (2026-07-28)

### Added

- **Mail**: Compose window (plain-text body) with Send, wired through to a real SMTP submission (`lettre`, XOAUTH2/password auth) followed by an `APPEND` of the sent message into the account's Sent mailbox. Validated live end-to-end against Gmail, including confirming the archived copy in Sent.
- **Mail**: Local SQLite cache of mailbox and message metadata per account (`$XDG_CACHE_HOME/lookout/mail/`), used for a fast first paint on startup before the live IMAP fetch completes.
- **Mail**: `Gio.NetworkMonitor`-driven reconnect - a disconnected session's exponential backoff wait is cut short as soon as connectivity is reported back, instead of always waiting out the current delay.
- **Packaging**: `.desktop` entry and AppStream metainfo (both pass their respective validators), a placeholder app icon, and a Flatpak manifest spike with the GOA session-bus permission (`--talk-name=org.gnome.OnlineAccounts`) confirmed against Geary's real, shipping manifest rather than guessed. Not yet a working Flatpak build - see `flatpak/README.md` for exactly what's left (vendored cargo sources, `flatpak-builder` itself, and a sandboxed-runtime fix for the "Open Online Accounts Settings" button).
- **Testing**: a fake-GOA D-Bus test service (`crates/goa/tests/fake_goa.rs`) giving real D-Bus-wire coverage of account discovery and credential fetching without a live GNOME session - run and passing. A GreenMail-backed IMAP/SMTP integration test (`crates/mail/tests/imap_integration.rs`) exercising the real session actor end-to-end - written and compiling clean, but not yet run (no Docker in the environment that wrote it); `#[ignore]`d until someone with Docker runs it for real.

### Known limitations (tracked for follow-up)

- Compose is plain-text only (no rich-text/contenteditable body, no recipient chips) and single-identity (always sends as the account's own address).

## 0.1.0 (2026-07-27)

Initial scaffolding of Lookout, a native GNOME/libadwaita mail client written in Rust, reimplementing the UI and functionality of [Bulwark Webmail](webmail) against IMAP/SMTP via GNOME Online Accounts instead of JMAP.

### Added

- **Accounts**: GNOME Online Accounts discovery over D-Bus (`lookout-goa`) - lists Mail-enabled accounts, filters out accounts without usable IMAP/SMTP support, and fetches OAuth2 access tokens or passwords on demand without caching credentials locally.
- **Mail**: IMAP account-session actor (`lookout-mail`) with XOAUTH2 and password login, `LIST (SPECIAL-USE)` folder discovery with role detection (Inbox/Sent/Drafts/Trash/Junk/Archive), envelope fetch with RFC 2047 subject/name decoding, and full-message body fetch/parsing via `mail-parser`.
- **Mail**: IMAP IDLE-based live sync, with on-demand commands (open a folder, fetch a message body) interrupting IDLE immediately rather than waiting on a timeout.
- **Mail**: Client-side conversation threading (JWZ-style, adapted for IMAP's lack of a native thread id) in `lookout-core`.
- **UI**: GTK4/libadwaita application shell (`lookout-app`) with a nested `AdwNavigationSplitView` layout (folders / messages / reading pane), an empty-state page that deep-links to `gnome-control-center online-accounts` when no mail account is configured, a folder tree, a message list, and a reading pane that renders HTML mail in a sandboxed WebKit view (JavaScript disabled, navigation blocked) with a plain-text fallback.
