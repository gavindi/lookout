# GNOME Evolution Data Server (EDS) integration

This plan connects Lookout to **Evolution Data Server**, the GNOME desktop's shared store of
address books, calendars and tasks. It covers three pieces of work:

1. **Read EDS address books.** Recipient autocomplete, the contact picker and People
   search would then see the desktop's contacts.
2. **Show Lookout's events in the GNOME Shell clock.** Lookout copies its events, one way,
   into an EDS calendar that it owns.
3. **Import accounts from Evolution.** Lookout reads Evolution's account files and pre-fills
   manual IMAP/SMTP accounts from them.

It also records two approaches that are **not** to be done (see
[Do not action](#do-not-action)).

**Current state:**

- Lookout runs its own stack: `lookout-dav` for CalDAV, CardDAV and webcal, `lookout-graph`
  for Microsoft 365, plus Google Tasks, LDAP and per-account SQLite caches. Nothing in the
  repo talks to EDS.
- GNOME Online Accounts (GOA) is reached over D-Bus in pure Rust through `lookout-goa` (`zbus`
  proxies in `source/crates/goa/src/proxies.rs`). The keyring is reached the same way through
  `secret-service`. EDS should follow that pattern.
- Directory lookups go through the `DirectoryBackend` trait
  (`source/crates/app/src/directory.rs:71`), which has a Graph backend and an LDAP backend.
  Hits come back as read-only `Suggestion::Directory` rows, and a failure means "no hits".
- Calendar months are written to `CalendarCache::store_month`
  (`source/crates/dav/src/cache.rs:208`) for every source, Microsoft 365 included. An event
  becomes iCalendar text through `lookout_dav::ical::build_vcalendar`
  (`source/crates/dav/src/ical.rs:509`).
- Manual accounts (`OtherAccount`, `source/crates/app/src/app_config.rs:89`) are IMAP/SMTP
  only. There is no standalone CalDAV or CardDAV account type yet.
- The Flatpak manifest (`source/flatpak/io.github.gavindi.Lookout.json`) and
  `source/snapcraft.yaml` give no access to EDS.

**Proposed decisions (to confirm before E1):**

- **Pure Rust over `zbus`, no C dependency.** EDS's D-Bus interfaces are internal and carry a
  version in their names (`Sources5`, `Calendar8`, `AddressBook10` at the time of writing).
  Lookout would detect the versions it knows at runtime and switch the feature off when
  they're missing, rather than linking `libecal` or `libebook`. E0.1 checks whether this holds
  up. If it doesn't, the fallback is bindings generated with `gir`, wrapped in the same crate.
- **Everything fails soft.** No EDS, an unknown interface version or a denied permission makes
  the feature unavailable, never an error dialog.
- **Address books (E1): on by default** when EDS is reachable. Reading is low-risk.
- **Shell mirror (E2): off by default.** It writes into the user's desktop data, so the user
  turns it on.
- **The Shell mirror stores expanded occurrences, not recurring series.** Lookout copies
  individual non-recurring events over a rolling window. This avoids reproducing recurrence
  rules, exceptions and time-zone details faithfully, and the Shell dropdown only needs
  instances.
- **Import (E3) covers mail accounts only.** Importing Evolution's CalDAV and CardDAV accounts
  has to wait until Lookout has manual DAV accounts.

Sizes: `S` under a day, `M` a few days, `L` a week or more.

---

## Phase 0 — Groundwork

### [ ] E0.1 — Spike: EDS D-Bus surface and sandbox behaviour (`M`)

The spike's output is a short notes section appended to this file, not code that ships.

- Introspect a current GNOME install (`busctl --user introspect`) and record the following
  for every EDS version Lookout supports (decide the floor, e.g. the version in the oldest
  GNOME runtime the Flatpak builds against):
  - the source registry bus name and `SourceManager` and `Source` interfaces, including
    `CreateSources` and how a source is removed;
  - `AddressBookFactory.OpenAddressBook` and the `AddressBook` methods for opening a book
    and running a query that returns vCards, plus the query syntax (EBook S-expressions,
    e.g. `(or (beginswith "full-name" "ann") (beginswith "email" "ann"))`);
  - `CalendarFactory.OpenCalendar` and the `Calendar` methods for creating, modifying,
    removing and listing objects, plus the operation flags.
- Confirm how the factory hands back a per-backend subprocess (bus name and object path),
  and what that means for sandbox permissions.
- Confirm how GNOME Shell decides which EDS calendars to show (the `[Calendar]` section's
  `Selected` key?), and that it picks up a calendar created while Shell is running.
- Confirm whether the `local` calendar backend can be made read-only to other clients, or
  whether edits made in GNOME Calendar simply get overwritten (see Open questions).
- Run the same calls inside the Flatpak with `--talk-name=org.gnome.evolution.dataserver.*`
  and record what works.
- Record whether Evolution's keyring items (the `org.gnome.Evolution.Data.Source` schema,
  attribute `e-source-uid`) are readable from the Flatpak build and the Snap build.

### [ ] E0.2 — `lookout-eds` crate (`M`)

Add `source/crates/eds` (package `lookout-eds`) to the workspace, shaped like `lookout-goa`.

- Depends on `zbus` (tokio feature), `lookout-core`, `thiserror` and `tracing`, and nothing
  else.
- `proxies.rs` holds the `zbus` proxies for the interfaces E0.1 records, one module per
  interface version.
- `discovery.rs` exports `Eds::connect() -> Result<Option<Eds>>`, which returns `Ok(None)`
  when the registry isn't on the bus or reports an interface version Lookout doesn't know.
- `sources.rs` lists sources as a plain `EdsSource { uid, display_name, kind, parent_uid,
  enabled, backend, goa_account_id }`, where `kind` is AddressBook, Calendar, TaskList or
  MailAccount. `goa_account_id` is read from the parent collection's `[GNOME Online
  Accounts]` section, so callers can recognise sources that belong to GOA.
- `error.rs` defines a small error enum. Callers outside the crate turn every error into
  "unavailable".
- Tests use a private D-Bus session (as the `goa` crate's tests do, if they do; otherwise
  `dbus-daemon --session --address=…` under a GTK-free test) with a fake registry object
  that answers `GetManagedObjects`. Cases:
  - the version is detected;
  - an unknown version is treated as unavailable;
  - sources are read, including a GOA-backed parent.

---

## Phase 1 — Read EDS address books (option 1)

### [ ] E1.1 — Querying address books (`M`)

In `lookout-eds`, add `address_book.rs`.

- `EdsAddressBook::open(eds, source_uid)` opens the book through the factory and keeps the
  subprocess proxy.
- `search(query, limit) -> Vec<VCard>`:
  - builds the S-expression from the trimmed, lower-cased query (`beginswith` on
    `full-name`, `nickname` and `email`, joined with `or`);
  - escapes `"` and `\` in the query;
  - parses each returned vCard with `lookout_core::vcard` (the parser `ContactRecord::card`
    already uses).
- Pure tests:
  - building the S-expression, including escaping and non-ASCII input;
  - turning a vCard with several email addresses into one hit per address.

### [ ] E1.2 — `EdsDirectoryBackend` (`M`)

Add `source/crates/app/src/eds_directory.rs`.

- It implements `DirectoryBackend`. `search` asks every enabled EDS address book that
  qualifies (see E1.3) at the same time, merges the results with
  `merge_directory_hits`, and returns an empty `Vec` on any failure.
- It maps each hit to `DirectoryPerson` (name, address, and organisation and title when the
  vCard has them).
- In `directory_source`, add one EDS backend for both the `Some(id)` case and the `None`
  case. Desktop address books aren't tied to an account, so the same backend serves every
  account.
- It keeps the existing gating (minimum query length, debounce, prefix cache) unchanged.
- Tests use a fake `DirectoryBackend` source to confirm:
  - EDS hits merge after stored contacts and other directories;
  - addresses are de-duplicated case-insensitively;
  - an unavailable EDS adds nothing.

### [ ] E1.3 — Avoid duplicate address books (`S`)

- By default, skip EDS address books whose `goa_account_id` matches a GOA account Lookout
  already syncs contacts for. Lookout's own CardDAV and Graph sync already shows those, so
  the same people would otherwise appear twice.
- Always include the local "Personal" book and books that don't belong to GOA, such as
  books created in Evolution or GNOME Contacts and LDAP books configured in Evolution.
- Write this rule as a pure function, `eligible_books(&[EdsSource], &HashSet<goa id>)`, and
  unit-test it.

### [ ] E1.4 — Setting and lifecycle (`S`)

- Add a GSettings key in `source/data/gschema/io.github.gavindi.Lookout.gschema.xml`:
  `eds-address-books` (bool, default `true`).
- Add a switch under **Settings → People**: "Include desktop address books (GNOME
  Contacts / Evolution)". Hide it, or show it greyed out with an explanation, when
  `Eds::connect()` returns `None`.
- Connect to EDS lazily, the first time a directory lookup runs. If EDS isn't on the bus,
  don't start it.
- Re-read the source list when the registry announces new or removed sources (the
  `ObjectManager` signals), so a book added in GNOME Contacts appears without a restart.

### [ ] E1.5 — Stretch: EDS books in the People module (`L`)

Optional, and only after E1.1–E1.4 have shipped.

- List eligible EDS books as read-only address books in the Contacts view, using a
  `ContactsAccountSnapshot`-style provider, with a "Desktop" section in the sidebar.
- No editing. Show "Open in GNOME Contacts" as the edit action.

---

## Phase 2 — Show Lookout's events in the GNOME Shell clock (option 2)

### [ ] E2.1 — The mirror calendar (`M`)

In `lookout-eds`, add `mirror.rs`.

- `ensure_mirror_source(eds) -> uid` finds or creates a single local calendar source with:
  - a fixed, stable UID (e.g. `io.github.gavindi.Lookout.mirror`);
  - the display name "Lookout";
  - `Selected=true`;
  - a colour that matches Lookout's accent;
  - a marker key, so Lookout can always recognise the source as its own.
- Create it through `SourceManager.CreateSources` over D-Bus, not by writing into
  `~/.config/evolution/sources`. That way the Flatpak needs no filesystem permission.
- `remove_mirror_source(eds)` deletes the source and all its events.
- Never touch a source without the marker key.

### [ ] E2.2 — Turning occurrences into mirror events (`S`)

Write this as a pure function in `source/crates/app/src/eds_mirror.rs`.

- The input is an `EventOccurrence` (`source/crates/core/src/calendar.rs:301`). The output
  is a VEVENT that doesn't repeat, built through `build_vcalendar` from a stripped-down
  `CalendarEvent` with no `rrule`, `exdates` or `recurrence_id`.
- The UID is stable: `lookout-<hash(calendar_id, uid, occurrence start)>`. Editing an event
  then modifies its copy instead of creating a new one.
- An `X-LOOKOUT-HASH` property holds a hash of the copied fields, so the sync step can skip
  events that haven't changed.
- What gets copied:
  - summary, start and end, the all-day flag and the time zone;
  - location;
  - attendees, never;
  - the description, only if the setting allows it (privacy, see E2.4).
- Cancelled occurrences, and events from hidden calendars (`AppConfig::hidden_calendars`),
  are left out.
- Unit tests:
  - the UID stays the same when the event is re-read;
  - all-day events and events with a time zone convert correctly;
  - the hash changes when the summary or time changes, but not when only an untracked field
    changes;
  - cancelled events and events on hidden calendars are dropped.

### [ ] E2.3 — Sync by comparison (`M`)

- Plan the changes with a pure function, `plan(desired: &[MirrorEvent], existing: &[(uid,
  hash)]) -> MirrorPlan { create, modify, remove }`, and unit-test it thoroughly. Cases:
  - the mirror is empty;
  - nothing has changed;
  - an event moved;
  - an event was deleted;
  - an event left the window.
- A worker task in the app carries out the plan:
  1. List what the mirror currently holds (its UIDs and `X-LOOKOUT-HASH` values).
  2. Send batched create, modify and remove calls (say 100 events per call).
- The window is a rolling **7 days back to 60 days ahead**. Read it from `CalendarCache`
  for every account and webcal subscription, on the worker thread and never on the GTK
  thread.
- When to sync:
  - after a calendar refresh writes a month that overlaps the window, debounced (say 5 s);
  - once at startup;
  - once a day, so the window keeps moving forward.
- Only one sync runs at a time. The pattern is `GraphPropertySyncs`: a later request
  coalesces into a single follow-up run.

### [ ] E2.4 — Setting, privacy and cleanup (`S`)

- Add GSettings keys:
  - `eds-mirror-calendar` (bool, default `false`);
  - `eds-mirror-details` (bool, default `false`), which copies descriptions as well as
    titles and locations.
- Add a switch under **Settings → Calendar**: "Show Lookout events in the GNOME clock and
  calendar". Below it, add a "Include event notes" switch that is active only when the
  first one is on.
- Turning the mirror off calls `remove_mirror_source`.
- Removing an account syncs straight away, so that account's events leave the mirror.
- Clearing caches (`clear_all_caches`) also clears the mirror, then refills it on the next
  sync.
- **Settings → Privacy** text: say that the mirror copies event titles and times into the
  desktop's calendar store, where other apps on the desktop can read them.

### [ ] E2.5 — Edits made in other apps (`S`)

The mirror is meant to be copied one way. Edits made in GNOME Calendar are overwritten or
removed at the next sync.

- If E0.1 finds a way to make the source read-only, use it.
- If not, the next sync's comparison restores the original. Mention this in the setting's
  subtitle ("read-only copy").
- Test: an extra, unknown UID in the mirror gets removed, and an event whose hash differs
  gets modified.

---

## Phase 3 — Import accounts from Evolution (option 3)

### [ ] E3.1 — Reading `.source` files (`S`)

In `lookout-eds`, add `keyfile.rs` with a small GKeyFile parser (pure, with no `glib`
dependency in this crate).

- It reads `~/.config/evolution/sources/*.source`, or the same accounts from `SourceManager`
  over D-Bus if E0.1 finds the Flatpak can't read the directory.
- It extracts mail accounts:
  - from `[Mail Account]` plus the backend section (`[Imapx Backend]` / `[Network]` /
    `[Authentication]` / `[Security]`), the IMAP host, port, user and security method;
  - from the linked `[Mail Transport]` source, the SMTP host, port, user and security
    method;
  - from `[Mail Identity]`, the display name and email address.
- Accounts whose collection parent is a GOA account are skipped, because Lookout already
  sees them through GOA.
- Tests use fixture files in `source/test-fixtures/eds/`:
  - a plain IMAP and SMTP account;
  - an account using STARTTLS and one using implicit TLS;
  - an account backed by GOA, which is skipped;
  - an account with a missing transport, imported with SMTP left blank;
  - a malformed file, which is skipped and logged.

### [ ] E3.2 — Building a pre-filled account (`S`)

- `to_other_account_draft(&EvolutionMailAccount) -> OtherAccountDraft` fills the
  `OtherAccount` fields and leaves the ID unset, so a new `other:<uuid>` is generated on
  save.
- Treat accounts whose email address (compared case-insensitively) or IMAP host and user
  already exist in Lookout as already present.
- Unit tests cover the field mapping, how each security method maps to the `use_tls` flags,
  and duplicate detection.

### [ ] E3.3 — Passwords (`S`)

- Search the keyring through the existing `secret-service` client for items with schema
  `org.gnome.Evolution.Data.Source` and `e-source-uid = <uid>`.
- If one is found and unlocked, use it to pre-fill the password, with the user's
  confirmation (see E3.4).
- If it can't be read (sandboxed, locked or missing), leave the password field empty and
  ask for it as usual. Never treat this as an error.
- Never copy or move Evolution's keyring item. Lookout stores its own copy under its own
  slot, the same way `SecretServiceKeyring::store_password` does.

### [ ] E3.4 — The import UI (`M`)

- In the add-account entry point (`other_accounts::show_add_account_dialog` and the
  first-run assistant), show **"Import from Evolution…"** when E3.1 finds at least one
  account that isn't already in Lookout.
- The dialog lists the accounts found, each with a checkbox, marking the ones already in
  Lookout and leaving them unticked. A note says that passwords are copied only when the
  keyring allows it.
- For each ticked account, run the existing validate-and-save path, then show the existing
  password prompt if no password was found.
- Add a GTK test that a list of two found accounts, one of them already present, shows two
  rows with one ticked.

---

## Packaging (all phases)

### [ ] P.1 — Flatpak (`S`)

- Add `--talk-name=org.gnome.evolution.dataserver.*` (a wildcard, because of the backend
  subprocess names; E0.1 confirms this).
- E3 needs nothing more if it reads accounts over D-Bus. Otherwise add
  `--filesystem=xdg-config/evolution/sources:ro`.
- Explain each new permission in `source/flatpak/README.md`. Flathub reviewers will ask
  about them.

### [ ] P.2 — Snap (`S`, may become "unsupported")

- Snap D-Bus permissions name exact bus names. Wildcards aren't allowed, so the backend
  subprocess names may not be reachable. If E0.1 shows E1 and E2 can't work under Snap,
  turn the features off there (they already fail soft) and say so in the settings
  subtitles.
- E3 needs `personal-files` read access to `~/.config/evolution/sources`, which requires
  Snap Store approval. Request it, or leave E3 out of the Snap build.

### [ ] P.3 — Debian package (`S`)

- Add `Recommends: evolution-data-server` in `[package.metadata.deb]`. It must not be a
  hard `Depends`, because Lookout runs fine without EDS.

---

## Suggested order

E0.1 → E0.2 → E1.1–E1.4 + P.1 → **ship** → E3.1–E3.4 → **ship** → E2.1–E2.5 → **ship** →
E1.5 (stretch).

E3 comes before E2 because it is self-contained and low-risk. E2 writes into the user's
desktop data, so it benefits from going last, once the crate has been in use for a while.

---

## Do not action

These were considered and rejected. Don't start them without revisiting this section.

### ✗ 4. Make EDS Lookout's calendar and contacts storage

Replace `lookout-dav`'s caches and sync with EDS as the system of record.

- **Why not:**
  - It is close to a rewrite of the calendar, tasks and contacts layers.
  - It would lose features that depend on Microsoft Graph: Focused Inbox
    classification, categories, follow-up flags, calendar sharing, `FetchMonth` backfill
    and event search over `event_search`.
  - Lookout would lose control over sync timing, caching and conflict handling.
  - EDS doesn't handle mail at all (that is Camel), so mail would still need Lookout's own
    stack.
  - It would tie Lookout to GNOME and break it on desktops without EDS.

### ✗ 5. An EDS plug-in that serves Lookout's data

Write an EDS backend module so EDS reads calendars and contacts straight from Lookout.

- **Why not:**
  - EDS plug-ins are C shared libraries that EDS loads into its own processes. A Flatpak or
    Snap app can't install one into the host's EDS.
  - The plug-in would have to read Lookout's SQLite caches from outside the app, with
    locking and schema-version coupling.
  - Its C ABI follows EDS's internal API, which changes between releases.
  - E2's one-way mirror gives the user-visible benefit (events in the Shell clock) for a
    fraction of the cost.

---

## Open questions

- **Minimum EDS version.** Which EDS releases (and so which interface versions) are
  supported? E0.1 proposes one; it needs confirming.
- **Writes in GNOME Calendar.** Can the mirror calendar be made read-only to GNOME Calendar?
  If not, is "overwritten on next sync" acceptable, or should the mirror be a webcal-type
  source served from an `.ics` file Lookout writes? (E0.1 checks whether EDS accepts local
  `file://` links for webcal.)
- **Tasks in the Shell.** Should E2 also copy tasks with due dates into an EDS task list,
  for GNOME Shell's task view and Endeavour? It is left out for now.
- **Reading EDS calendars into Lookout.** For example, a calendar created only in GNOME
  Calendar's local storage. This isn't one of options 1–3; it would be E1's calendar
  counterpart, and it needs its own plan if wanted.
