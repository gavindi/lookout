<p align="center">
  <img src="assets/icons/Lookout_App_3.svg" alt="Lookout" width="256">
</p>

# Lookout

A native GNOME mail client written in Rust, built on GTK 4, libadwaita, and WebKitGTK. Lookout is a reimplementation of Microsoft Outlook as a desktop application, talking to your mail directly over IMAP/SMTP, your calendars over CalDAV, and your contacts over CardDAV — with Microsoft 365 calendars and Outlook-side properties (categories, follow-up flags, rules, automatic replies) synced natively through the Microsoft Graph API. Accounts come from **GNOME Online Accounts**, or you can add any IMAP/SMTP server by hand.

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/P5P21M7MBS)


**Platform:**
[![License](https://img.shields.io/badge/license-GPL--3.0-green.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux-FCC624?logo=linux&logoColor=black)](https://github.com/gavindi/lookout)

**Packages:**
[![Fedora](https://img.shields.io/badge/Fedora-RPM-51A2DA?logo=fedora&logoColor=white)](https://github.com/gavindi/lookout/releases/latest)
[![Ubuntu](https://img.shields.io/badge/Ubuntu-DEB-E95420?logo=ubuntu&logoColor=white)](https://github.com/gavindi/lookout/releases/latest)
[![Flatpak](https://img.shields.io/badge/Flatpak-bundle-4A90D9?logo=flatpak&logoColor=white)](https://github.com/gavindi/lookout/releases/latest)
[![Snap](https://img.shields.io/badge/Snap-package-82BEA0?logo=snapcraft&logoColor=white)](https://github.com/gavindi/lookout/releases/latest)


## Features

- **Mail** — multi-account IMAP sync with IDLE live updates, a unified "All Inboxes" view, folder tree with per-role icons and Favorites, collapsible conversations, and an HTML/plain-text reading pane rendered in a sandboxed WebKit view. Full-text search across every account from a local SQLite FTS5 index, with a query language (`from:`, `to:`, `subject:`, `has:attachment`, `is:unread`, `category:`, `received:`, `size:`, `folder:`), refiner chips, search history, and a scope dropdown. Per-message categories with Outlook's 25-colour palette (two-way sync on Microsoft 365), follow-up flags with due dates (synced with Outlook's own flag on Microsoft 365), pin, snooze (server-side through a Scheduled folder where keywords are supported), @mention detection with a Mentions-me filter, save-all/zip for attachments, TNEF (`winmail.dat`) decoding, print, list-unsubscribe, and read receipts.
- **Compose** — new/reply/reply-all/forward (caret starts above the quoted content, ready to type) with correct threading headers, forward-with-attachments or forward-as-attachment, rich HTML editing with inline images, tables, emoji, and drag-and-drop files, To/Cc/Bcc address lines with inline autocomplete, a searchable address-book picker, and contact lists that expand into their members. Multiple identities per account, rich per-account signatures (separate new/reply defaults), My Templates, undo send, schedule send, importance and sensitivity, WebKit spell checking with a language picker (F7), draft autosave with reopen-where-you-left-off, an attachment reminder, and a pop-out window. While composing in the reading pane the formatting toolbar takes over the main action bar; popping the composer out moves the controls into its own window. Sent via SMTP (XOAUTH2 or password) and copied into your Sent mailbox.
- **Organization** — rules that move, copy, delete, flag, categorize, forward, or redirect, running in Lookout, on Microsoft 365 via Graph `messageRules`, or on any Sieve-capable server (so they keep working while the app is closed), with a full manager and a "create rule from message" action. Safe/blocked sender lists, RFC 5788 junk keywords (`$Junk`/`$NotJunk`), Report Phishing, and automatic replies (Out-of-Office) per account with optional schedules, backed by Graph `automaticRepliesSetting` or Sieve `vacation`.
- **Mail actions** — delete (move-to-Trash), archive, report-as-junk, pin, mark read/unread, snooze, and batch actions on multi-selection, all backed by real IMAP MOVE/COPY+EXPUNGE, with a full undo toast/Ctrl+Z stack for moves, deletes, flags, categories, and read state. Drag messages onto folders or categories, or out of the window as `.eml` files.
- **Calendar** — a CalDAV-backed Outlook-style calendar (Microsoft 365 accounts use a native Graph backend once Outlook sync is enabled): day/work week/week/month/List/Schedule/split views, mini-calendar sidebar, an upcoming-events overview on the Mail screen, event create/edit with recurrence (editable per occurrence), categories, per-event time zones and a second grid time zone, configurable week start and working hours/days, and a full right-click event menu (duplicate, copy to another calendar, save as `.ics`, show as, private). Drag to reschedule, Ctrl-drag to copy, drag the edge to resize. Organizer invitations are sent as iMIP (or deferred to an RFC 6638 server): attendees answer from the invite card, replies update a Tracking view, times can be proposed (COUNTER/DECLINECOUNTER), meetings can be forwarded, and updates/cancellations go out automatically. A Scheduling Assistant tab shows attendee free/busy (RFC 6638 or Graph `getSchedule`) with suggested times, Teams/Zoom/Meet/Webex/Jitsi links get a Join button, holiday calendars are one click per country, and `.ics` import/export and webcal subscriptions are built in. A synthesized birthday calendar rounds it out, and event alerts arrive as notifications (Open / Snooze / Dismiss) with a catch-up for alerts missed while the app was closed.
- **People** — CardDAV contacts with full create/edit/delete, groups, favorites, per-account buckets, and a Deleted list that survives restarts. Full field editing (anniversary, IM, website, photo, custom properties), a global "File as" preference with per-contact overrides, a profile card on hover/click of a sender or attendee (recent mail, upcoming shared events, send email / schedule meeting), and `.vcf` plus Outlook CSV import/export with duplicate handling. Autocomplete across the composer, invites, and the People tab.
- **Tasks** — CalDAV to-dos (plus Google Tasks and local lists) grouped by Overdue / Today / This week / Later / Completed, with a full editor for due dates, priorities, categories, and notes, sort/group options, a Flagged-email section that mirrors follow-up flags, and a "create task from message" action.
- **Microsoft 365** — Microsoft Graph is a first-class backend: opt in from Config and the account gets a native calendar, and categories, follow-up flags, rules, and automatic replies sync with Outlook itself; pins mirror across. Access is granted explicitly and can be revoked at any time; mail runs over IMAP/SMTP with Lookout's own OAuth2 (PKCE) client by default. A per-account **Use Graph mail (experimental)** switch can move mail onto Graph instead: it polls rather than using IMAP IDLE (up to about 45 seconds for the open folder and Inbox, longer for other folders, because a desktop app can't host Graph's push notifications), and Pin, Sieve rules and folder moves aren't available on that transport. Each folder's delta cursor is cached, so restarts resume incrementally, and the local outbox still handles undo send, scheduled send and offline sending.
- **Security** — messages render sandboxed with remote content blocked until you allow it (per-sender trust list), SPF/DKIM/DMARC authentication results and display-name/address mismatches raise banners, mail from outside your domains can be tagged external, hover-a-link shows the real destination with a confirmation on mismatch, and Report Phishing forwards the message to a configured address before junking it.
- **Lookout dashboard** — most-contacted people, email volume by time of day, outstanding tasks, and upcoming events, live from your local caches, plus an optional AI assistant (any OpenAI-compatible endpoint; read-only local tools; API token kept in the keyring).
- **Desktop presence** — an optional tray icon (StatusNotifierItem) with the total unread count and an Open Lookout / New mail / Quit menu, plus an optional dock badge with the same count; both toggleable in Settings.
- **Config** — a Settings dialog (modal over the main window): connected Mail/Calendar/Contacts accounts and endpoints, appearance (System / Flat Dark / Flat Light themes + accent colour + window background), Mail options (layout, reading pane, conversations, density/preview lines, mark-as-read, undo-send delay, junk lists, rules, templates), Calendar preferences (week start, working days/hours, time zones, default reminder), Signatures, Identities, Automatic replies, Google Tasks, Assistant, remappable keyboard shortcuts, trusted senders, and cache management.
- Built around `GNOME Online Accounts` — add an account in system settings and it just shows up — or add any IMAP/SMTP account directly in Lookout, with passwords kept in the GNOME login keyring.

See [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) for the feature-by-feature gap analysis against Outlook and the phased plan to close it, and [SCAFFOLD.md](SCAFFOLD.md) for the architecture.

![Reading a message in dark mode](assets/screenshots/mail_read_dark_1.png)

## Project layout

```
.
├── assets/                   # App icons and screenshots
├── source/                   # Rust Cargo workspace (the desktop app)
│   ├── crates/
│   │   ├── core/             # Domain types, threading, mailbox roles, Outlook interop
│   │   ├── goa/              # GNOME Online Accounts D-Bus discovery + credentials
│   │   ├── mail/             # IMAP/SMTP account-session actor + SQLite cache
│   │   ├── dav/              # CalDAV/CardDAV client + iCalendar/vCard/recurrence handling
│   │   ├── graph/            # Microsoft Graph client (auth, paging, throttling) + calendar backend
│   │   ├── imap-proto/       # Vendored imap-proto fork (UTF-8 envelope fix, patched via Cargo)
│   │   ├── async-imap/       # Vendored async-imap fork (COMPRESS stream swap, buffer perf fix)
│   │   └── app/              # GTK4/libadwaita UI (lookout binary)
│   ├── data/                 # .desktop file, AppStream metainfo, icons, GSettings schema, GResource bundle, themes
│   ├── test-fixtures/        # Sample .eml/.ics fixtures plus captured Outlook interop artefacts used by tests
│   ├── flatpak/              # Flatpak manifest (built by CI)
│   └── snapcraft.yaml        # Snap packaging manifest
├── build.sh                  # Builds the Rust workspace (optionally packages a .deb)
├── MARKETING.md              # Marketing copy and feature walkthrough
├── OUTLOOK_PARITY.md         # Outlook gap analysis, interop reference, and roadmap
├── SCAFFOLD.md               # Architecture guide
└── CHANGELOG.md              # Version history
```

## Prerequisites

- Rust (stable) — via [rustup.rs](https://rustup.rs)
- GTK 4, libadwaita, and WebKitGTK development packages (checked via `pkg-config`):

```bash
# Debian/Ubuntu
sudo apt-get install libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev libglib2.0-dev pkg-config
```

To run the app you also need a GNOME desktop session with at least one **mail-enabled** account configured in *Settings → Online Accounts* (an empty state otherwise deep-links you to `gnome-control-center online-accounts`), or you can add an IMAP/SMTP account directly from Lookout's account dialog.

## Build and run

```bash
# Build the Rust workspace
./build.sh                # release build (default)
./build.sh --debug        # debug build
./build.sh --deb          # release build + .deb package (requires cargo-deb)

# Or just the desktop app
cd source
cargo build               # binary at source/target/debug/lookout
cargo run                 # build and launch
```

`./build.sh` prints the resulting binary path; a release build lands at `source/target/release/lookout`.

## Testing

```bash
cd source

# Unit tests (whole workspace). Includes lookout-eds's fake-EDS D-Bus tests,
# which start their own private dbus-daemon (needs `dbus-daemon` on PATH)
cargo test --workspace

# Fake-GOA D-Bus test on an isolated session bus (real D-Bus-wire coverage
# of account discovery and credential fetching, no live GNOME session needed)
dbus-run-session -- cargo test -p lookout-goa --test fake_goa

# GreenMail IMAP/SMTP integration test - requires Docker, skipped by default
cargo test -p lookout-mail --features test-utils --test imap_integration -- --ignored

# Linting / formatting
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, unit tests, the fake-GOA D-Bus test, the Docker-gated GreenMail test, and a full build; `.github/workflows/release.yml` packages `.deb`/`.rpm` artifacts and builds the Flatpak and Snap bundles for each release.

## Roadmap

Lookout tracks its remaining distance to Outlook in [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md): a feature-by-feature gap analysis (mail, compose, calendar, people, tasks, accounts, security, desktop), an Outlook interop reference (headers, iCalendar properties, Graph endpoints, file formats, keyboard maps), and a phased roadmap from the completed foundations through Microsoft 365 native support. That document is kept current as features land.

## License

GPL-3.0-or-later.

[![Buy Me a Beer](https://img.shields.io/badge/Buy_Me_a_Beer-🍺-FF5F5F?style=flat)](https://buymeacoffee.com/gavindi)
