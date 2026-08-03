# Changelog

## 0.6.18 (2026-08-04)

### Changed

- **UI**: the reading pane now crossfades (100ms `Crossfade` transition) between its pages instead of snapping, so switching messages fades the old body out and the new body in. The message header (subject/avatar/recipients) now lives *inside* the crossfading "message" page alongside the body - grouped with the body's web/text views in a no-transition inner stack - so the header and body fade out and in together instead of the header popping out of sync with the content. Message switches already pass through the pane's "empty" placeholder, so both halves of the fade fire for free; the HTML page is held on the placeholder until WebKit actually finishes loading the new body (`load-changed` Finished) so the fade-in never reveals a still-blank white page. Same-page re-renders (e.g. the debug `.eml` opener loading into an already-visible page) route through "empty" explicitly to force the transition.
- **UI**: the Config view's "Appearance" section is now live (it was a disabled placeholder) with an "Animate transitions" switch that disables the reading pane's crossfades. It's the first real preference in the Phase 5 settings taxonomy and, like the layout toggles, is session-only until GSettings lands. Flipping it off swaps the stack's transition type to `None` and makes the body-render path skip its fade dance (no routing through the empty page, no waiting for the WebView to paint), restoring the instant pre-fade behavior.
- **UI**: the reading pane's crossfade is race-condition-proofed. The old per-render `load-changed` handler attached a fresh one-shot per HTML body render, so rapid selection changes could accumulate handlers that double-revealed or revealed a stale email after the user had already moved on; the reveal now goes through a single persistent handler gated by a `pending_html_reveal` flag in `UiState`, which the selection handler disarms on every selection change so an in-flight load for an abandoned message can never reveal. The text-body path also now waits out the full fade-out duration instead of a hardcoded 200ms (it reads the stack's real `transition_duration`) and defers its reveal through the same `reveal_message_page` helper, which re-checks `is_transition_running` so the next message can't pop in mid-fade on fast clicks.
- **UI**: the reading-pane header no longer jumps to the next email mid-fade. It was updated synchronously in the selection handler - before the previous message's fade-out even started - so during the 100ms crossfade you'd see the *next* message's subject/sender over the still-fading *previous* body. The header update is now deferred: the selection handler stashes the summary in a `pending_header` in `UiState`, and `render_body` applies it only when the new body actually renders (the pane is on the "empty" placeholder by then, so the swap is invisible until the message page fades back in).

## 0.6.17 (2026-08-04)

### Changed

- **Build**: `build.sh` now defaults to an optimized release build (`cargo build --workspace --release`, binary at `target/release/lookout`) instead of the unoptimized debug build. `--debug` opts back into the fast unoptimized build for day-to-day iteration, and the old `--release` flag still works unchanged.

### Fixed

- **Mail**: Delete/Archive/Report now recognize the Trash/Archive/Junk folders on providers that don't use the default names. The role heuristic (`guess_role_from_name`) only knew "Trash"/"Deleted Items"/"Deleted Messages" for Trash, so a provider whose trash is named "Bin" (Gmail en-GB, `[Gmail]/Bin`), "Recycle Bin", or just "Deleted" got role `Custom` and Delete failed with "No Trash folder found". Added those spellings (plus "Deleted" and "Recycle Bin") and covered each provider spelling with a regression test.
- **Mail**: deleting a message no longer drops the selection in the message list. The list is rebuilt from scratch on every `MessagesUpdated` event, and `remove_all` was resetting the `SingleSelection` to "nothing selected" - so after deleting (or archiving/snoozing) via the hover/toolbar button, the highlight was lost. The handler now snapshots the selected message (mailbox + UID) before the rebuild and restores it afterward: the same message if it's still present, otherwise the message that now occupies the deleted row's old position.

## 0.6.16 (2026-08-04)

### Added

- **UI**: the menu bar's Home/View buttons are now real ribbon tabs instead of disabled placeholders. Home and View are mutually-exclusive toggle tabs (the same `set_group` trick as the nav rail) that switch the ribbon content row below: Home shows the existing command toolbar (New/Reply/Reply All/Forward/Delete/Archive/Report/Snooze), and View is a new "Layout" group with three pane-visibility toggles - Folder pane, Reading pane, and the Mail screen's Calendar-overview pane - each hiding/showing its pane live via `set_visible` on the `Gtk.Paned` child. The tabs are Mail-only: they grey out while a Calendar or Config module is active (those keep their own non-tabbed command toolbars), and returning to Mail restores the last-active tab. The active tab is styled with a pressed-state background, and the overview-pane toggle is respected on module round-trips (hiding it in View stays hidden after visiting Calendar/Config). The layout toggles are session-only until Phase 5's GSettings landing.
- **UI**: the folder view now defaults to the first account's Inbox on startup - the first `FoldersUpdated` event auto-selects the first account's Inbox in the folder tree (accounts sort by label, folders Inbox-first), routing through the same `selected-item` handler as a real click, which sets the current mailbox and issues the `SyncMailbox` that fills the message list. Guarded by `current_mailbox` being unset, so later folder resyncs never yank the user's selection away.

## 0.6.14 (2026-08-03)

### Changed

- **Calendar**: the Day/Week/Work week time grids are now a real timeline instead of fixed per-hour buckets. Each grid is a split view: a fixed all-day band (drawn on its own canvas, never scrolls away) above a vertically scrollable 24-hour day (48px per hour slot, 8am-6pm shaded as business hours, hour markers down the left gutter). Event chips are positioned by their exact start/end time rather than dropped into a per-hour cell - so short and long events render to scale, overlapping events split their day column into side-by-side lanes, and multi-day events stretch across day columns as a single full-width chip. Switching to a time-grid view auto-scrolls it to the current time, and hovering a chip raises a tooltip with the event’s summary and time range plus a white highlight ring. The agenda lists are now grouped under “Today”/“Tomorrow”/date headers.

## 0.6.13 (2026-08-03)

### Fixed

- **Calendar**: the mini calendar now only bolds the current day and days that actually have events, while the sidebar’s “My calendars” list keeps account names bold and calendar row labels at normal weight.

## 0.6.12 (2026-08-03)

### Changed

- **Calendar**: the "My calendars" checklist's per-calendar checkboxes are now custom multi-select radio rows that finally render each calendar's assigned colour. The stock GTK/libadwaita `CheckButton` paints its `.check` indicator through an internal widget whose fill is drawn in a way that ignores display-level CSS overrides (`background-color`, `background-image` and `-gtk-icon-source` at `STYLE_PROVIDER_PRIORITY_APPLICATION` all left it stuck on the theme's accent colour), so the colour could never win. Each row is now a flat `ToggleButton` carrying a hand-drawn 16px radio indicator (a `DrawingArea` painted with Cairo): checked calendars show a solid disc in the calendar's colour with a white inner dot, unchecked ones a hollow ring in that colour - the same colour the calendar's event chips use. Row backgrounds are fully translucent in every state (base/hover/active/checked), so the sidebar background shows straight through the lines. Behaviour is unchanged - multiple calendars stay selectable, toggling still filters the grids, and newly discovered calendars still default to checked.

## 0.6.11 (2026-08-03)

### Added

- **Calendar**: the command toolbar's view-switcher is now fully live instead of a Month-only placeholder. Day, Work week, Week, Month and Split view are real, mutually-exclusive segmented options wired to the main panel's new view stack (anchored to a single shared date so every view agrees on what's displayed, and the header's Today/prev/next navigate in the active view's natural unit - days for Day, weeks for Week/Work week, months for Month/Split). Day and Week/Work week are read-only time grids (all-day row plus one cell per hour against an hour-ruler gutter); Month is the existing Sunday-first grid; Split pairs the month grid with a day-agenda list. An agenda-style list view (chronological, from the anchor date to its month's end) is also available as the Split view's right-hand pane, mirroring the Mail-screen overview pane's day list. All views remain read-only - event creation is still a separate roadmap item.

## 0.6.10 (2026-08-03)

### Fixed

- **Calendar**: Google (and most other CalDAV servers) pretty-print their `multistatus` XML - properties and their `<href>` values sit on their own indented lines. The WebDAV response parser was accumulating that formatting whitespace into property text, which corrupted the `calendar-home-set` href (trailing `\n    ` made the calendar-list PROPFIND hit a non-existent URL) and replaced `resourcetype`'s `collection,calendar` markers with a whitespace blob (so every calendar failed the calendar-type filter). Net result: connected Google accounts silently reported zero calendars and the "My calendars" checklist stayed empty - the exact behavior Evolution sidesteps via its own e-d-s parsing. Fixed by trimming final property values and treating whitespace-only text as absent for container properties; regression-tested with a Google-style pretty-printed response.

## 0.6.9 (2026-08-03)

### Fixed

- **Calendar**: the sidebar's "My calendars" checklist no longer stays blank while accounts are connected but haven't reported their calendars yet. It now renders one entry per connected calendar account (a dim account header), each with its discovered calendars as checkboxes - or, until they arrive, an inline status line: "Connecting…" while the CalDAV session is establishing, "No calendars found" if the account comes up with zero collections, or the session's error message when discovery fails. A "No calendars connected" placeholder shows when there are no calendar accounts at all, so the section always reflects reality instead of silently disappearing.

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
