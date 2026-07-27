# Changelog

## 0.1.0 (2026-07-27)

Initial scaffolding of Lookout, a native GNOME/libadwaita mail client written in Rust, reimplementing the UI and functionality of [Bulwark Webmail](webmail) against IMAP/SMTP via GNOME Online Accounts instead of JMAP.

### Added

- **Accounts**: GNOME Online Accounts discovery over D-Bus (`lookout-goa`) - lists Mail-enabled accounts, filters out accounts without usable IMAP/SMTP support, and fetches OAuth2 access tokens or passwords on demand without caching credentials locally.
- **Mail**: IMAP account-session actor (`lookout-mail`) with XOAUTH2 and password login, `LIST (SPECIAL-USE)` folder discovery with role detection (Inbox/Sent/Drafts/Trash/Junk/Archive), envelope fetch with RFC 2047 subject/name decoding, and full-message body fetch/parsing via `mail-parser`.
- **Mail**: IMAP IDLE-based live sync, with on-demand commands (open a folder, fetch a message body) interrupting IDLE immediately rather than waiting on a timeout.
- **Mail**: Client-side conversation threading (JWZ-style, adapted for IMAP's lack of a native thread id) in `lookout-core`.
- **UI**: GTK4/libadwaita application shell (`lookout-app`) with a nested `AdwNavigationSplitView` layout (folders / messages / reading pane), an empty-state page that deep-links to `gnome-control-center online-accounts` when no mail account is configured, a folder tree, a message list, and a reading pane that renders HTML mail in a sandboxed WebKit view (JavaScript disabled, navigation blocked) with a plain-text fallback.

### Known limitations (tracked for follow-up)

- No local message cache (SQLite) yet - each folder is fetched fresh from the server.
- No compose/send UI yet.
- No reconnect/offline handling beyond basic backoff.
- No packaging (`.desktop` file, Flatpak manifest) yet.
- No automated integration tests (GreenMail, fake-GOA D-Bus service) yet.
