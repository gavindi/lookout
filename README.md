# Lookout

A native GNOME mail client written in Rust, built on GTK 4, libadwaita, and WebKitGTK. Lookout is a reimplementation of Microsoft Outlook as a desktop application, talking to your mail directly over IMAP/SMTP (and CalDAV for calendars), with accounts sourced from **GNOME Online Accounts**.

## Features

- **Mail** — multi-account IMAP sync with IDLE live updates, folder tree with per-role icons, message list, and an HTML/plain-text reading pane rendered in a sandboxed WebKit view.
- **Compose** — new/reply/reply-all/forward with correct threading headers, sent via SMTP (XOAUTH2 or password) and copied into your Sent mailbox.
- **Mail actions** — delete (move-to-Trash), archive, report-as-junk, and client-side snooze, all backed by real IMAP MOVE/COPY+EXPUNGE.
- **Calendar** — a CalDAV-backed, read-only Outlook-style month view with a mini-calendar sidebar and "My calendars" checklist, plus a day-agenda overview docked to the Mail screen.
- **Config** — a nav-rail settings view showing your connected Mail/Calendar accounts and endpoints.
- Built around `GNOME Online Accounts` — add an account in system settings and it just shows up, no credential handling in Lookout itself.

See [TODO.md](TODO.md) for the full roadmap (Phases 1–5) and what's shipped vs. planned.

## Project layout

```
.
├── source/                  # Rust Cargo workspace (the desktop app)
│   ├── crates/
│   │   ├── core/            # Domain types, threading, mailbox roles
│   │   ├── goa/             # GNOME Online Accounts D-Bus discovery + credentials
│   │   ├── mail/            # IMAP/SMTP account-session actor + SQLite cache
│   │   ├── dav/             # CalDAV client + iCalendar/recurrence handling
│   │   └── app/             # GTK4/libadwaita UI (lookout binary)
│   ├── data/                # .desktop file, AppStream metainfo, icons
│   ├── test-fixtures/       # Sample .eml files for the debug viewer
│   └── flatpak/             # Flatpak manifest spike (not yet buildable)
├── webmail/                 # Bulwark webmail app (Next.js, separate repo)
├── build.sh                 # Builds the Rust workspace and the webmail frontend
└── CHANGELOG.md             # Version history
```

## Prerequisites

- Rust (stable) — via [rustup.rs](https://rustup.rs)
- GTK 4, libadwaita, and WebKitGTK development packages (checked via `pkg-config`):

```bash
# Debian/Ubuntu
sudo apt-get install libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev libglib2.0-dev pkg-config
```

To run the app you also need a GNOME desktop session with at least one **mail-enabled** account configured in *Settings → Online Accounts* (an empty state otherwise deep-links you to `gnome-control-center online-accounts`).

## Build and run

```bash
# Build the whole repo (Rust workspace + webmail frontend)
./build.sh                # debug build
./build.sh --release      # optimized build

# Or just the desktop app
cd source
cargo build               # binary at source/target/debug/lookout
cargo run                 # build and launch
```

`./build.sh` prints the resulting binary path; a release build lands at `source/target/release/lookout`.

## Testing

```bash
cd source

# Unit tests (whole workspace)
cargo test --workspace

# Fake-GOA D-Bus test on an isolated session bus (real D-Bus-wire coverage
# of account discovery and credential fetching, no live GNOME session needed)
dbus-run-session -- cargo test -p lookout-goa --test fake_goa

# GreenMail IMAP/SMTP integration test - requires Docker, skipped by default
cargo test -p lookout-mail --features test-utils --test imap_integration -- --ignored

# Linting / formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, unit tests, the fake-GOA D-Bus test, the Docker-gated GreenMail test, and a full build.

## Roadmap

See [TODO.md](TODO.md) for the phase-by-phase roadmap — Phase 1 (Mail MVP) is the current milestone, with Mail-advanced, Calendar, Contacts, and Settings/theming scoped as later phases.

## License

GPL-3.0-or-later.
