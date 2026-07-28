# Changelog

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
