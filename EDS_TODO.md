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

### [x] E0.1 — Spike: EDS D-Bus surface and sandbox behaviour (`M`)

The spike's output is a short notes section appended to this file, not code that ships.
Done 2026-10-20; see [E0.1 spike notes](#e01-spike-notes).

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

### [x] E0.2 — `lookout-eds` crate (`M`)

Done 2026-10-20 (v0.9.253). Where it differs from the plan below, following the E0.1 notes:

- **One proxy set, not one module per version.** The bus names and the methods Lookout uses
  haven't changed since EDS 3.34. `proxies.rs` explains how to add a second set if EDS ever
  bumps a name.
- **No `lookout-core` dependency yet.** Nothing in E0.2 uses it; E1.1 adds it for the vCard
  parser.
- **Detection never starts EDS.** `Eds::connect` asks the bus who owns `Sources5`, and treats
  each factory as present when it is running or activatable. It returns `Ok(None)` for an
  unknown version, including EDS 3.28–3.32, which ran `Sources5` with older factories.
  `supports_address_books()` and `supports_calendars()` report the factories separately.
  `Eds::connect_with(connection)` takes an existing connection.
- **Source fields:**
  - `enabled` is the *effective* state: the source and all its ancestors must be enabled, as
    GNOME Shell decides.
  - `display_name` uses the user's locale (`DisplayName[de]=…`).
  - `goa_account_id` follows `Parent` up to the GOA collection.
  - Built-in mail accounts (`local`, `vfolder`, `rss`) are listed as `MailAccount`; E3 has to
    filter them out.
- **`keyfile.rs`** (the pure GKeyFile reader planned for E3.1) is added now, because listing
  sources needs it.
- **The tests start their own private `dbus-daemon`** with an empty service directory, so they
  run under plain `cargo test` without `dbus-run-session`. They never see or start the real
  EDS. Beyond the three planned cases, they cover a partial install, an old release, and a
  stopped but activatable registry that must not be started.
- **`cargo run -p lookout-eds --example list_sources`** prints the live desktop's sources.

The original plan:

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

### [x] E1.1 — Querying address books (`M`)

Done 2026-10-20 (v0.9.254). Where it differs from the plan below, following the E0.1 notes and
a check against the live desktop:

- **The query.** `search_query` builds
  `(and (exists "email") (or (beginswith "full_name" q) (beginswith "nickname" q) (beginswith "email" q)))`:
  - field names use underscores;
  - the query is trimmed but not lower-cased, because EDS already ignores case and accents;
  - `nickname` is kept for backends such as LDAP that translate the query themselves.
- **`(exists "email")` is new.** Real books are full of phone-only cards: on this desktop, one
  search returned 11 cards with only 4 addresses between them. Those cards would use up the
  caller's `limit`, so EDS now leaves them out.
- **What `search` returns.** It leaves out contact lists (`X-EVOLUTION-LIST:TRUE` or
  `KIND:group`) and cards that don't parse. It sorts by name, then applies the limit. A blank
  query never reaches EDS.
- **Added for E1.2:** `EdsPerson { name, address, organization, title }` and
  `people_from_card`, which gives one person per address. A card with no FN uses its structured
  name, or no name at all, as with the Microsoft 365 book.
- **Closing books.** `EdsAddressBook::close()` exists because EDS keeps a backend open for as
  long as the shared connection lives.
- **Tests.** Beyond the planned pure tests, a wire test in `fake_eds.rs` covers opening a book
  through a factory that returns a subprocess bus name and path. The fake backend refuses
  queries until `Open` has been called. The test also covers the exact query sent, the limit,
  and a blank search, which must not reach EDS.
- **Example:** `cargo run -p lookout-eds --example search_address_books -- <query>`.

The original plan:

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

### [x] E1.2 — `EdsDirectoryBackend` (`M`)

Done 2026-10-20 (v0.9.255). Notes:

- **One shared connection.** Every directory source shares one process-wide `LiveDesktopBooks`.
  It connects to EDS on the first lookup, through `Eds::connect`, which never starts EDS. It
  keeps each book open between lookups.
- **"Unavailable" is remembered for 60 s,** so a desktop without EDS doesn't pay a D-Bus round
  trip per keystroke, while an EDS that starts later is still found.
- **The source list is re-read on every lookup** (one `GetManagedObjects`, a few
  milliseconds), so books added, removed or switched off on the desktop take effect without a
  restart. The E1.4 item about `ObjectManager` signals is therefore only an optimisation now.
- **Failures:**
  - each book gets 3 s to open and 3 s to answer;
  - a book whose search fails is dropped and reopened on the next lookup (EDS may have
    restarted);
  - when the source list itself fails, EDS counts as unavailable.
- **Which books.** For now every enabled address book qualifies (`qualifying_books`); E1.3
  narrows it. There is no setting yet (E1.4).
- **Order.** The EDS backend goes last in `directory_source`, in both scopes, so a person found
  in another directory keeps that directory's entry.
- **Nameless cards.** A card with no name maps to `name: None`, which the suggestion rows
  already show as the bare address. No fallback name is needed.
- **Tests:**
  - mapping;
  - merging across books, case-insensitive de-duplication and the limit;
  - an unavailable desktop adds nothing;
  - qualifying books;
  - EDS hits come after another directory's, through a real `DirectorySource` with its
    debounce and worker. This test needs GTK and skips itself otherwise, like the other
    GTK tests;
  - EDS hits come after stored contacts, in `merge_contact_suggestions`;
  - an ignored live test, `live_desktop_books`, runs against the session's real EDS. Run it
    with `cargo test -p lookout-app live_desktop_books -- --ignored --nocapture`. On this
    desktop: 8 hits, and 9 ms for a warm lookup.
- **Follow-up, not done here:** in the composer, `merge_contact_suggestions` re-filters
  directory hits with `directory_matches_prefix`, which needs the display name or the address
  to start with the prefix. So EDS hits that matched only on the family name, the nickname or
  an accent-insensitive comparison ("smi" for "Dave Smith", "emile" for "Émile") are dropped
  there. The contact picker isn't affected. The Graph GAL already loses surname matches the
  same way. Relaxing the filter to match the start of any word in the name would help both.

The original plan:

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

### [x] E1.3 — Avoid duplicate address books (`S`)

Done 2026-10-20 (v0.9.256).

- **The rule.** `eligible_books(&[EdsSource], &HashSet<String>)` in
  `crates/app/src/eds_directory.rs` keeps every enabled address book, except one whose
  `goa_account_id` is in the set. That covers several books from the same account (Google's
  "Contacts" and "Other contacts", for example).
- **"Already syncs".** A GOA account counts as synced when Lookout runs a contacts session for
  it, CardDAV or Graph (the keys of `UiState::contact_cmd_tx`). `goa_account_id` maps the GOA
  object path Lookout uses as an account id to the `account_…` id EDS records.
- **What still appears.** A GOA account that is disabled in Lookout, or whose contacts Lookout
  doesn't sync, keeps its EDS book.
- **Snapshot.** The set is captured when a directory source is built, so a composer opened
  before contacts discovery finishes still searches those books. Duplicate addresses are
  merged anyway, so the cost is only wasted lookups.
- **Not done:** Evolution's per-book `[Autocomplete] IncludeMe` flag (E0.1 suggested honouring
  it). It isn't honoured, because GNOME Contacts never sets it and its default couldn't be
  confirmed. Worth revisiting if users ask why an Evolution book with autocompletion turned
  off still shows up.

- By default, skip EDS address books whose `goa_account_id` matches a GOA account Lookout
  already syncs contacts for. Lookout's own CardDAV and Graph sync already shows those, so
  the same people would otherwise appear twice.
- Always include the local "Personal" book and books that don't belong to GOA, such as
  books created in Evolution or GNOME Contacts and LDAP books configured in Evolution.
- Write this rule as a pure function, `eligible_books(&[EdsSource], &HashSet<goa id>)`, and
  unit-test it.

### [x] E1.4 — Setting and lifecycle (`S`)

Done 2026-10-20 (v0.9.256).

- **The setting.** The GSettings key `eds-address-books` (bool, default `true`) backs a
  **Config → People** switch, "Include desktop address books", with the subtitle "Suggest
  people from GNOME Contacts and Evolution when choosing recipients".
- **When EDS can't be reached.** At startup a check runs on the worker thread
  (`desktop_available`, which never starts EDS). If it fails, the switch is greyed out and
  its subtitle says no Evolution Data Server address books can be reached.
- **Changes apply straight away.** The switch drives a process-wide flag that every lookup
  reads, so composers already open follow it. The EDS backend is always added to
  `directory_source` and simply answers nothing while the switch is off.
- **Turning it off** also closes the open books and drops the EDS connection
  (`disconnect_desktop`).
- **The connection is lazy, and EDS is never started** (done in E1.2).
- **No `ObjectManager` signals.** Instead of listening for them, every lookup re-reads the
  source list (E1.2), so a book added in GNOME Contacts appears on the next keystroke without
  a restart, and no signal subscription has to be kept alive.

- Add a GSettings key in `source/data/gschema/io.github.gavindi.Lookout.gschema.xml`:
  `eds-address-books` (bool, default `true`).
- Add a switch under **Settings → People**: "Include desktop address books (GNOME
  Contacts / Evolution)". Hide it, or show it greyed out with an explanation, when
  `Eds::connect()` returns `None`.
- Connect to EDS lazily, the first time a directory lookup runs. If EDS isn't on the bus,
  don't start it.
- Re-read the source list when the registry announces new or removed sources (the
  `ObjectManager` signals), so a book added in GNOME Contacts appears without a restart.

### [x] E1.5 — Stretch: EDS books in the People module (`L`)

Done 2026-10-20 (v0.9.257).

- **What appears.** The People sidebar gains a **Desktop** section, after the account sections,
  with one row per eligible EDS book. These are the same books, connection and setting as the
  directory lookups (E1.2–E1.4), so GOA books Lookout already syncs stay out.
- **Data.** `UiState::desktop_contacts` holds the books (`DesktopBook { uid, display_name,
  cards }`). `EdsAddressBook::all_contacts()` lists a whole book with
  `(contains "x-evolution-any-field" "")`, falling back to `#t`. On this desktop that was
  10 books and 253 cards in 17 ms.
- **Read-only, using the existing marker.** Desktop entries carry an empty `href`, which is the
  People screen's display-only marker, shared with the Deleted bucket. So the editor, Delete
  and the ribbon's Edit stay off without new checks. Their account id is `eds:<source uid>`
  (`desktop_contacts::is_desktop_entry`).
- **"Open in GNOME Contacts"** replaces Edit in the details pane, and row activation does the
  same.
  - GNOME Contacts can't be pointed at an EDS UID: its `show-contact` action takes a folks
    individual id. So Lookout runs `gnome-contacts --email <first address>` (or
    `--search <name>`) when the command is installed alongside it, which selects the contact.
  - Otherwise (GNOME Contacts installed as a Flatpak, or Lookout sandboxed) it D-Bus-activates
    `org.gnome.Contacts`, which opens the app without selecting the contact.
  - When neither works, a toast says so. **P.1** needs `--talk-name=org.gnome.Contacts` for the
    D-Bus route from the Flatpak.
- **Favourites.** "Your Contacts" stays the accounts' contacts, but a starred desktop contact
  also shows under Favourites. Stars use the same local store, keyed by the `eds:` account id.
- **Refresh.** The section reloads:
  - when contacts discovery finishes, so the GOA accounts to skip are known (discovery now
    shows the People page when only desktop books exist);
  - whenever the People screen is shown;
  - every 15 minutes;
  - when the setting changes; switching it off clears the section.

  A reload that a newer one overtakes is dropped, and an unchanged result doesn't repaint.
- **The setting's subtitle** now mentions the People section.
- **Tests:**
  - lookout-eds wire tests: a whole-book listing keeps phone-only cards and contact lists, and
    the any-field query falls back to `#t`;
  - desktop entries are told apart by account id, are display-only and keep book order;
  - the GNOME Contacts arguments: address first, then name, never placeholder text;
  - a GTK-gated sidebar test: the Desktop header and book row, read-only members, Favourites but
    not "Your Contacts", and read-only list entries. It was checked to fail when the section is
    removed;
  - an ignored live test, `live_desktop_snapshot`.
- **Not done:**
  - desktop cards' `CATEGORIES` don't join the Categories section;
  - a desktop contact isn't found by the profile card's "is this a known contact" lookup
    (`find_contact_by_address`), so its address isn't treated as a stored contact elsewhere;
  - if the People screen showed its empty page and desktop books appear later (the setting is
    switched on, or EDS starts), the page switches only after the next contacts discovery,
    which is at restart.

- List eligible EDS books as read-only address books in the Contacts view, using a
  `ContactsAccountSnapshot`-style provider, with a "Desktop" section in the sidebar.
- No editing. Show "Open in GNOME Contacts" as the edit action.

---

## Phase 2 — Show Lookout's events in the GNOME Shell clock (option 2)

### [x] E2.1 — The mirror calendar (`M`)

Done 2026-10-20 (v0.9.259). Where it differs from the plan below, following the E0.1 custom-file design:

- **A read-only custom file, not a local calendar of EDS's own.** The source is a `local`
  calendar with `[Local Backend] CustomFile=file://…/eds-mirror.ics` and `Writable=false`.
  GNOME Calendar and Evolution show it as read-only, EDS reloads the file when it changes, and
  removing the source leaves no data directory behind (E0.1, "A read-only mirror is possible").
  E2.3 therefore rewrites one whole `VCALENDAR`, not individual events over D-Bus.
- **API.**
  - `ensure_mirror_source(eds, ics_file) -> Result<String>` returns the fixed
    `MIRROR_SOURCE_UID` (`io.github.gavindi.Lookout.mirror`).
  - `remove_mirror_source(eds) -> Result<bool>` removes the source and deletes the `.ics` file
    it pointed at, since EDS leaves custom files alone. An already-missing file isn't an error.
  - `MIRROR_DISPLAY_NAME`, `MIRROR_COLOR` and the UID are exported for the app.
- **The key file** has `DisplayName=Lookout`, `Enabled=true`, `Parent=local-stub`,
  `BackendName=local`, `Selected=true`, `Color=#4d9dff` (Lookout's accent blue in
  `data/themes/base.css`), the custom file marked `Writable=false`, and a `[Lookout]
  Mirror=true / Version=1` marker. `file://` URIs are percent-encoded, so a path with spaces or
  non-ASCII characters still works.
- **Only marked sources are touched.** A source with the mirror UID but without the marker is
  refused (`Error::SourceNotMirror`), never overwritten or removed. A mark is checked on every
  call, not remembered from an earlier run.
- **Creation and drift.**
  - `SourceManager.CreateSources` creates the source with the UID Lookout chooses; nothing is
    written to `~/.config/evolution/sources`.
  - An existing marked source is left alone when the keys Lookout owns match. When one has
    drifted (the data directory moved, an old display name or colour), its key file is
    rewritten through `Source.Writable.Write`. Under Snap that method isn't in the AppArmor
    profile (P.2), so a drifted source fails soft there; a correct one is never rewritten.
  - `ensure_mirror_source` refuses an EDS without the calendar factory
    (`Error::CalendarsUnavailable`) rather than creating a source nothing can read.
- **The file isn't created here.** E2.3 owns its contents. `ensure_mirror_source` validates
  that the path is absolute (EDS may run outside Lookout's sandbox and would resolve a relative
  path against its own working directory) and points the source at it.
- **Removal** calls `Source.Removable.Remove` first, then deletes the percent-decoded
  `CustomFile`, and only accepts a plain absolute `file://` URI, so a hand-edited source can't
  make Lookout delete an unrelated path. A refusal or a missing source leaves the file alone.
- **Tests:**
  - unit: the key file's contract, marker matching, URI round-trips (spaces, `%`, non-ASCII),
    relative-path refusal, and drift detection;
  - D-Bus wire tests against the fake registry: creation with the fixed UID and exact keys, an
    existing mirror left alone, a moved custom file rewritten, an unmarked source refused for
    both ensure and remove, removal deleting source and file, removal when absent, and an
    address-book-only EDS refused;
  - the fake registry now answers `CreateSources`, `Source.Writable` and `Source.Removable`.
- **Not done here:** writing or syncing the `.ics` (E2.3), the setting and app wiring (E2.4),
  or refreshing the source's colour when Lookout's accent changes.

The original plan:

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

### [x] E2.2 — Turning occurrences into mirror events (`S`)

Done 2026-10-21.

- **API.** `mirror_event(occurrence, include_description, hidden_calendars) ->
  Option<MirrorEvent>` in `source/crates/app/src/eds_mirror.rs`. `None` means "don't
  mirror this": the occurrence is cancelled or its calendar is hidden. `MirrorEvent`
  carries the stable `uid`, its `hash`, the `start`/`end` for E2.3's window bookkeeping,
  and `component`, the `VEVENT` (with any `VTIMEZONE`) ready to sit inside the mirror's
  one `VCALENDAR`.
- **The UID** is `lookout-` plus 128 bits of SHA-256 over the source calendar, source
  UID and occurrence start. It survives a re-read and an edit, while another instance
  of a series, another event or another calendar gets a different one.
- **The hash** is SHA-256 over a canonical text of exactly the copied fields - summary,
  start, end, all-day, time zone, location and (when allowed) description - so a change
  to an attendee, category, reminder or href never forces a rewrite. An all-day
  occurrence's stored time zone is dropped before hashing, because the copy doesn't use
  it.
- **What is copied:** summary, start/end, the all-day flag, the time zone and the
  location; the description only when `include_description`. Attendees, organizer,
  reminders, categories, sensitivity, "Show as", sequence and the conference URL are
  never copied. `rrule`, `rdates`, `exdates` and `recurrence_id` are cleared, so every
  copy is a single non-repeating event.
- **The property.** `build_vcalendar`'s `VCALENDAR` wrapper is cut off, keeping the
  `VEVENT` and its `VTIMEZONE` (EDS's custom-file source wants a whole calendar, and
  E2.3 concatenates these components), and `X-LOOKOUT-HASH:<hash>` is inserted before
  `END:VEVENT`.
- **Tests:** UID stability and uniqueness; all-day and zoned conversion; recurrence
  stripped; hash changes for each copied field but not for untracked ones; descriptions
  only when allowed and unchanged hashes when they aren't; cancelled and hidden
  occurrences dropped; the component is a bare, single `VEVENT` carrying its hash.

The original plan:

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

### [x] E2.3 — Sync by comparison (`M`)

Done 2026-10-21.

- **The file, not per-event D-Bus writes.** Following E0.1's custom-file design, a sync
  rewrites the mirror's whole `eds-mirror.ics` atomically (pid-suffixed temp file plus
  rename) when the content changed, so EDS's file monitor sees one coherent calendar.
  There are no `CreateObjects`/`ModifyObjects` calls, batch sizes or all-or-nothing
  handling. The file lives at `$XDG_DATA_HOME/lookout/eds-mirror.ics`
  (`eds_mirror::mirror_path()`); E2.4 hands that path to `ensure_mirror_source`.
- **The comparison.** `plan(desired: &[MirrorEvent], existing: &[(uid, hash)]) ->
  MirrorPlan { create, modify, remove }` is still the pure heart. `X-LOOKOUT-HASH` (and
  the UID) are read back from the existing file with `existing_events`; the file is
  rewritten only when the plan is non-empty or the file is missing/empty. This also
  removes an unknown UID someone added by hand.
- **The window.** A rolling 7 days back to 60 days ahead, filtered by overlap so a
  multi-day event that began before it still appears. `mirror_months` returns the
  first-of-month `CalendarCache` keys the window can touch, and the sync opens each
  account's cache and each webcal subscription's (`webcal-<id>`) on the blocking pool -
  never the GTK thread. Deduplication is by mirror UID, and events are ordered by start
  then UID.
- **The file.** `build_calendar` emits one `VCALENDAR` with a fixed header, the
  `VEVENT`s (and any `VTIMEZONE`s) from E2.2, deduplicated `VTIMEZONE`s by `TZID`, and a
  stable event order. `DTSTAMP` is generated per build, which is why the comparison uses
  event hashes rather than byte equality.
- **When.** A month that overlaps the window triggers a request with a 5 s trailing-edge
  debounce from both the CalDAV/Graph `OccurrencesUpdated` loop and the webcal
  `SubscriptionsUpdated` loop; the window also requests once at startup and once a day.
  `MirrorSyncState` allows one run at a time, and a request during a run coalesces into
  a single follow-up (the `GraphPropertySyncs` rule).
- **Still off.** `MirrorSyncState::enabled` defaults to false and nothing sets it until
  E2.4's setting, so the triggers are no-ops for now. `set_enabled` is the E2.4 entry
  point; turning it on requests an immediate sync, and changing the description setting
  refreshes the file.
- **Tests:** the plan's empty/unchanged/create/modify/remove cases; reading hashes back
  from a built calendar; timezone dedupe and event ordering; the month window and its
  overlap predicate; the rolling-window filter (past, far-future, cancelled and hidden
  events dropped); an end-to-end cache-to-file sync that writes once, skips an unchanged
  second run, and rewrites after a deletion; and the run/coalesce state machine.

The original plan:

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

### [x] E2.4 — Setting, privacy and cleanup (`S`)

Done 2026-10-21.

- **The settings.** `eds-mirror-calendar` (bool, default `false`) backs Config → Calendar
  → "Show Lookout events in the GNOME clock and calendar". `eds-mirror-details` (bool,
  default `false`) backs the dependent "Include event notes" switch, which is sensitive
  only while the mirror is on. Both are in the schema, the `settings.rs` fallback
  defaults, and the defaults test.
- **Enabling and disabling.** `eds_mirror::apply_enabled` connects to EDS, creates the
  source through `ensure_mirror_source(mirror_path())`, and only then turns the sync on;
  disabling turns the sync off, then calls `remove_mirror_source`, which also deletes the
  `.ics`. A failure while enabling shows a toast explaining it and resets the switch;
  startup failures are logged instead, because the mirror is off by default and a
  desktop without EDS shouldn't toast on every launch.
- **Availability.** At Config startup `mirror_available()` (which never starts EDS) greys
  the switch out with "Not available - this desktop has no Evolution Data Server
  calendar Lookout can reach" when the calendar factory isn't there. A stored "on" stays
  on, so it starts working when EDS appears.
- **Startup.** The rows are seeded with their notification handlers suppressed, then the
  stored setting is applied: enabling re-creates the EDS source if it has gone missing
  and requests an immediate sync.
- **Account removal.** Removing a manual account and disabling a GOA account both request
  a sync straight away; the account has already left `CalendarUiState::accounts`, so its
  cached events are no longer sources and drop out of the mirror.
- **Clearing caches.** `clear_all_caches` deletes the mirror file and requests a sync;
  the request (and the month resyncs that follow) refills it from the freshly fetched
  calendar caches.
- **Privacy.** Config → Privacy is now a real group with a "Desktop calendar mirror" row
  explaining that titles, times and locations (and notes when allowed) are copied into
  the desktop calendar store, where other apps can read them.
- **Tests.** The settings-defaults test covers both new keys; the mirror's own tests now
  include the E2.5 restoration case below.

### [x] E2.5 — Edits made in other apps (`S`)

Done 2026-10-21.

- **Read-only, by design.** E2.1 already creates the source with
  `[Local Backend] Writable=false`, and E0.1 verified that GNOME Calendar and Evolution
  see that flag and turn editing off. There is no further write path to block, so E2.5
  is solved by the existing custom-file source rather than new code; the setting's
  subtitle now says "read-only copy".
- **If something writes anyway.** The flag is advisory: another client's create/modify/
  remove calls can return OK but are kept in memory only and disappear when EDS reloads
  the file. Lookout's comparison still restores the file's intended contents, and adding
  `foreign_edits_are_restored_or_removed` pins both halves: a VEVENT whose
  `X-LOOKOUT-HASH` differs is a `modify`, and an unknown UID is a `remove`. That test
  brings `eds_mirror` to 19 unit tests.

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

### [x] P.1 — Flatpak (`S`)

Done 2026-10-21.

- **The permission.** `source/flatpak/io.github.gavindi.Lookout.json` now grants
  `--talk-name=org.gnome.evolution.dataserver.*`. The wildcard is needed because EDS uses
  several names under that prefix (the `Sources5`, `Calendar8` and `AddressBook10`
  factories, plus backend subprocess names that vary by build and process), and Flatpak
  matches one bus name per entry.
- **Verified by E0.1.** The spike ran listing, queries, `CreateSources`/`Write`/`Remove`
  and the custom-file mirror both on the host and inside a Flatpak with exactly this
  permission. It also covers `UserPrompter0`, which Lookout doesn't use.
- **No filesystem permission.** E3 will read each source's `Data` property over D-Bus, so
  `~/.config/evolution/sources` doesn't need to be mounted. The README records this.
- **No secrets permission yet.** E3.3 isn't implemented; without
  `--talk-name=org.freedesktop.secrets` it will fall back to asking for the password in
  the Flatpak build. Granting keyring access is a separate decision for that item.
- `source/flatpak/README.md` explains the new permission for reviewers.

The original plan:

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

### [x] P.3 — Debian package (`S`)

Done 2026-10-21.

- **The field.** `source/crates/app/Cargo.toml`'s `[package.metadata.deb]` now has
  `recommends = "evolution-data-server"`, which cargo-deb emits as `Recommends:`.
- **Not a dependency.** The existing `depends` list is unchanged. Lookout starts and runs
  without EDS; only the address-book and calendar-mirror features are unavailable.
- The RPM metadata is unchanged, since this item covers Debian packaging.

The original plan:

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

---

## E0.1 spike notes

Run on 2026-10-20 on Ubuntu 26.10 with EDS 3.56.2, GNOME Shell 51, GLib 2.90 and Flatpak 1.18.4.
Every call was a raw `Gio.DBusConnection` method call from a throwaway Python script, without
libecal or libebook, so it sends exactly what `zbus` would send. The script ran on the host, and
inside the sandbox through `flatpak run --talk-name=org.gnome.evolution.dataserver.*` on
`org.gnome.Platform//50`. Each test source was created, exercised and then removed. Older EDS
releases and the Snap were checked against upstream source, not run.

### Verdict

- **Pure Rust over `zbus` holds up.** Everything E1–E3 needs is a plain method call with
  string or array arguments, so the `gir` fallback isn't needed.
- **Minimum version: EDS 3.34 (September 2019).** The names `Sources5`, `Calendar8` and
  `AddressBook10` are the same in every release from 3.34 to `master` (checked in
  `CMakeLists.txt` at 3.34, 3.36, 3.40, 3.44, 3.46, 3.48, 3.50, 3.52, 3.56 and `master`).
  Releases 3.28–3.32 used `Calendar7` and `AddressBook9`, so they count as unavailable (this
  rules out RHEL 8).
  - Comparing the introspection XML for 3.34 and 3.56 shows no method was removed or changed.
    Later releases only added things: `AddressBook.ContainsEmail` and `Categories`, and the
    `AddressBookView` sort and `DupContacts` members.
  - So one set of proxies covers every supported release, and the "one module per version"
    split in E0.2 isn't needed yet.
- **The minimum version is set by the host's EDS, not the Flatpak runtime.** Lookout talks to
  the host's services over the bus and never loads the runtime's EDS libraries, so the "oldest
  GNOME runtime" test proposed in E0.1 doesn't apply.
- **Detecting EDS.** `Sources5`, `Calendar8` and `AddressBook10` all have D-Bus activation
  files, so any ordinary call starts EDS. To follow E1.4's "don't start it" rule, check with
  `NameHasOwner` or `GetNameOwner` first, or send calls with `NO_AUTO_START`. On a GNOME
  session EDS is always running anyway, because Shell's calendar server starts it.

### Bus names and objects

| Service | Bus name | Objects |
|---|---|---|
| Source registry | `org.gnome.evolution.dataserver.Sources5` | `/org/gnome/evolution/dataserver/SourceManager` (`SourceManager` and `ObjectManager`) and `…/SourceManager/Source_N`, one per source |
| Calendar factory | `org.gnome.evolution.dataserver.Calendar8` | `…/CalendarFactory`, plus per-backend objects at the path `Open*` returns |
| Address book factory | `org.gnome.evolution.dataserver.AddressBook10` | `…/AddressBookFactory`, plus per-backend objects |
| Credential prompts | `org.gnome.evolution.dataserver.UserPrompter0` | Not needed. The Flatpak wildcard also covers it, which does no harm. |

### Sources (`SourceManager`)

- **Listing.** `ObjectManager.GetManagedObjects` on `…/SourceManager` returns every source,
  100 sources in 8 ms. Each object's `org.gnome.evolution.dataserver.Source` interface carries
  `UID`, `Data` (the whole `.source` key file as text) and `ConnectionStatus`.
  - `Source_N` numbers aren't stable, so key sources by `UID`.
  - Watch `InterfacesAdded` and `InterfacesRemoved` to notice sources being added or removed
    (E1.4).
- **Extra interfaces on a source object:**
  - `Source.Writable.Write(s data)`, which replaces the key file;
  - `Source.Removable.Remove()`, only on sources the user can delete (books and calendars
    they created, and stand-alone mail accounts);
  - `Source.RemoteCreatable` and `Source.RemoteDeletable`, on collection backends;
  - `Source.OAuth2Support`.
- **Creating.** `SourceManager.CreateSources(a{ss})` maps each chosen UID to its key-file text.
  It returns nothing, and the source shows up through `InterfacesAdded` in under 0.2 s. Because
  the caller picks the UID, a fixed mirror UID works.
  - Groups EDS doesn't know survive the round trip, so a `[Lookout]` group with `Mirror=true`
    works as the marker key.
  - A source created this way always gets `Removable` and `Writable`.
- **Removing.** Call `Source.Removable.Remove()`.
  - **Leak:** for a local calendar or address book, EDS 3.56 deletes the `.source` file but
    leaves `~/.local/share/evolution/{calendar,addressbook}/<uid>/` (`calendar.ics` or
    `contacts.db`) on disk, and a Flatpak build has no access to delete it.
  - The custom-file design below creates no such directory (checked).
- **Key-file facts the E0.2 parser needs:**
  - The kind is the group name: `[Address Book]`, `[Calendar]`, `[Task List]`, `[Memo List]`,
    `[Mail Account]`, `[Mail Identity]`, `[Mail Transport]`, `[Collection]` or
    `[Mail Signature]`. `BackendName` is in that group.
  - `Enabled` and `Parent` are in `[Data Source]`.
  - Translated `DisplayName[xx]=` keys appear (for example on `system-calendar`), so the
    parser must accept `[locale]` key suffixes.
  - **GOA:** the `[GNOME Online Accounts] AccountId=account_…` group sits only on the
    `[Collection]` parent. Child books and calendars carry no GOA key, so `goa_account_id`
    means following `Parent`.
  - Built-in UIDs: `system-address-book` (the "Personal" book, not `Removable`),
    `system-calendar`, `system-task-list`, and the `*-stub` parents (`local-stub`,
    `webcal-stub`, `contacts-stub` and so on).
  - Address books have no `Selected` key. Evolution's "Use in autocompletion" checkbox is
    stored as `[Autocomplete] IncludeMe=`, which E1.3 could honour.

### Factories and backend processes

- **Opening.** `CalendarFactory.OpenCalendar(s uid)` (also `OpenTaskList` and `OpenMemoList`)
  and `AddressBookFactory.OpenAddressBook(s uid)` return `(object_path, bus_name)`. Then call
  `Open() -> as` on that object before running queries.
- **Where the backend runs.** Always use the pair the factory returns; never build it yourself.
  - **Default builds** (`ENABLE_BACKEND_PER_PROCESS=OFF`, the upstream default, as on this
    Ubuntu): the bus name is the factory's own name (for example `…Calendar8`), and the path is
    `/org/gnome/evolution/dataserver/Subprocess/<factory pid>/<n>`.
  - **Per-process builds:** the bus name is
    `org.gnome.evolution.dataserver.Subprocess.Backend.Calendarx<pid>x<n>` (or the
    `AddressBook` equivalent), and the path is `…/Subprocess/Backend/Calendar/<pid>/<n>`.
- **Sandboxes.** The Flatpak wildcard matches every name below
  `org.gnome.evolution.dataserver.`, so it covers both cases. Snap's AppArmor rules match on
  object paths, not bus names, so they cover both as well.
- **Lifetime.** The factory closes a backend when the client that opened it disconnects. Keep
  one connection open for as long as a handle is in use, and reopen the handle when the name
  owner changes (EDS restarted).
- **Speed.** Opening took under 35 ms, even for remote CardDAV and Microsoft 365 books, which
  answer from EDS's local cache.

### Address books

- **Methods:**
  - `GetContactList(s query) -> as`, which returns vCard 3.0 text;
  - `GetContactListUids`;
  - `GetView` (live results) and `GetCursor`;
  - `ContainsEmail`, which was added after 3.34, so treat `UnknownMethod` as unsupported.
- **Field names use underscores, as in `e_contact_field_name`:** `full_name`, `nickname`,
  `email`, `file_as`. vCard names such as `FN` also work.
  - **The hyphenated `"full-name"` in E0.1 and E1.1 silently returns no hits, with no
    error.**
- **`full_name` checks four fields.** It matches FN, the family name, the given name *or* the
  nickname, in both the SQLite backend and the generic one (`compare_name` in
  `e-book-backend-sexp.c`, and the full-name substitution in `e-book-sqlite.c`). So
  `(or (beginswith "full_name" q) (beginswith "email" q))` already covers nicknames. Prefixes
  of later words inside FN don't match ("smi" doesn't find "Dave Smith" when the card has no
  `N`).
- **Matching ignores case and accents:** `A` matches the same cards as `a`, `e` matches
  "Émile", and `é` matches "Example". Lookout doesn't need to lower-case the query.
- **`email beginswith`** matches the start of any of a card's email addresses (verified with
  a second, mixed-case address), not the domain.
- **Escaping:** `\"` and `\\` inside a string literal work. This was checked against a name
  containing both.
- **Queries some books reject:**
  - Local (SQLite) books reject `#t` with "Invalid Query". Use
    `(contains "x-evolution-any-field" "")` to match everything. CardDAV and Microsoft 365 books
    accept `#t`.
  - A malformed query returns an error (e-client-error 16, or e-cache-error 3). Treat it as
    "no hits".
- **The data can be thin.** The GOA Microsoft 365 book returned cards with only `EMAIL`, with
  no `FN` or `N`. E1.2 must fall back to the address as the display name.
- **Speed.** The first query on the Microsoft 365 book took 0.97 s (cold). After that, and on
  local books, queries took under 10 ms.

### Calendars

- **Methods:**
  - `CreateObjects(as ics, u opflags) -> as uids`;
  - `ModifyObjects(as ics, s mod_type, u opflags)`;
  - `RemoveObjects(a(ss) uid_rid, s mod_type, u opflags)`;
  - `GetObject(s uid, s rid) -> s` and `GetObjectList(s sexp) -> as`;
  - `GetView(s sexp) -> o`, `AddTimezone` and `GetTimezone`;
  - properties `Writable`, `Capabilities`, `Online` and `Revision`.
- **Each string must be one bare `VEVENT`.** A string wrapped in `VCALENDAR` is rejected with
  "Invalid object", and the getters also return bare `VEVENT`s. This means
  `build_vcalendar`'s output can't be passed straight in: events with a `TZID` would need
  `AddTimezone` first, or UTC times with `VALUE=DATE` for all-day events (both verified).
- **`mod_type`** is a list of GFlags nicks joined with `:`: `this`, `this-and-prior`,
  `this-and-future`, `all` or `only-this`.
  - An unknown nick is logged and treated as 0.
  - On the local backend, `this` and `all` work for events that don't repeat, and `only-this`
    returns NotSupported.
- **`opflags`** (`ECalOperationFlags`, since 3.34):
  - 0 means no flags (keep local, the default);
  - 1 = conflict-fail, 2 = use-newer, 4 = keep-server, 8 = write-copy;
  - 16 = disable iTIP messages.
  - The mirror uses 0.
- **Batches are all-or-nothing, and nothing is applied when one item fails:**
  - one duplicate UID fails the whole `CreateObjects` with "Object ID already exists"
    (e-cal-client-error 4);
  - one unknown UID fails the whole `RemoveObjects` or `ModifyObjects` with "Object not found"
    (error 1).
  - So a writer has to plan from a fresh listing and fall back to one call per item when a
    batch fails.
- **Parsing is lenient.** Junk lines inside a `VEVENT` are accepted, and a missing `UID` is
  generated by the backend.
- **`X-` properties survive the round trip,** so `X-LOOKOUT-HASH` would work.
- **Queries** `#t`, `(uid? "…")` and
  `(occur-in-time-range? (make-time "YYYYMMDDTHHMMSSZ") (make-time "…"))` all work.
- **Speed.** On the local backend, creating 100 events took 5 ms and removing them took 8 ms.
- **Errors** come back as names like
  `org.gtk.GDBus.UnmappedGError.Quark._e_2dcal_2dclient_2derror_2dquark.Code<N>`. The quark
  and code are encoded in the name.

### GNOME Shell

- **How Shell picks calendars.** `gnome-shell-calendar-server` (`org.gnome.Shell.CalendarServer`)
  filters sources through an `ESourceRegistryWatcher`. It takes a source that is enabled
  (parents included) and has a `[Calendar]` group with `Selected=true`. The source code is the
  same in Shell 45 and 51. It shows events only, never task lists or memos.
- **It picks changes up while running (verified on Shell 51):**
  - Events in a calendar created through `CreateSources` while Shell was running arrived as
    `EventsAddedOrUpdated` within about 2 s, with no restart.
  - Modifying an event sent `EventsAddedOrUpdated`, and removing one sent `EventsRemoved`.
  - Writing `Selected=false` through `Source.Writable.Write` sent `ClientDisappeared` at once.
  - Shell's event IDs are `<source uid>\n<event uid>\n<rid>`.

### A read-only mirror is possible, through a custom file

- **Plain local calendars can't be made read-only.** The `local` backend honours
  `[Local Backend] Writable=false` *only when `CustomFile` is set* (`get_source_writable` in
  `e-cal-backend-file.c`). A calendar whose data lives in EDS's own directory has no read-only
  switch.
- **With a custom file, it works (verified on the host and from the Flatpak):** a source with
  `CustomFile=file:///abs/path.ics` and `Writable=false`.
  - **Clients see it as read-only.** The backend reports `Writable=false`. GNOME Calendar
    (`notify::readonly` in `gcal-calendar.c`) and Evolution read that flag and turn editing off.
  - **The flag is advisory.** Another client's create, modify and remove calls still returned
    OK, but the change lived only in memory (the backend skips saving when it isn't writable),
    and the next reload of the file dropped it.
  - **EDS watches the file** (a GFileMonitor plus an mtime check) and reloads it within about
    0.3 s, both after an atomic rename and after an in-place rewrite. It compares the old and new
    contents and sends per-event add, modify and remove notifications, so Shell receives only
    the events that changed (verified from the signals).
  - **The file is a whole `VCALENDAR`,** so `build_vcalendar` output, including `VTIMEZONE`s,
    can be used as is. Stable UIDs (E2.2) still matter, because EDS matches events by UID.
  - **It leaves nothing behind.** EDS creates no data directory for the source, and `Remove()`
    leaves the file alone; Lookout deletes the file itself.
  - **It works from sandboxes.** Flatpak mounts `~/.var/app/<id>/` at the same absolute path
    on the host, so a file under `$XDG_DATA_HOME` is readable by the host's EDS with no extra
    permission (tested with an equivalent host-backed path). Under Snap, use
    `$SNAP_USER_COMMON`, not the per-revision `$SNAP_USER_DATA`.
- This answers the open question about whether EDS accepts `file://` links for webcal: a webcal
  source isn't needed.

### Sandboxes

- **Flatpak.** With no permission, every call fails with `ServiceUnknown`. With only
  `--talk-name=org.gnome.evolution.dataserver.*`, everything above behaved the same as on the
  host:
  - listing sources and opening books and calendars;
  - queries;
  - `CreateSources`, `Write` and `Remove`;
  - creating, modifying and removing events;
  - the custom-file source.

  GNOME Shell's calendar server isn't reachable from the sandbox, and isn't needed.
- **E3 needs no file access.** The `Data` property returns the full key file of every source,
  including mail accounts, identities and transports. So E3 can read Evolution's accounts over
  D-Bus without `--filesystem=xdg-config/evolution/sources:ro` or Snap `personal-files`.
- **Snap (from snapd source, not run).**
  - snapd has `calendar-service` and `contacts-service` interfaces. They are implicit on
    classic systems and set to `deny-auto-connection`, so each needs a manual `snap connect`
    or an auto-connect grant from the Store.
  - Their AppArmor rules match on object paths and interfaces. That covers the backend paths in
    both process modes, `ObjectManager`, `Source`, `Source.Removable` and
    `SourceManager.CreateSources`, but **not `Source.Writable`**. Under Snap the mirror source
    has to be created complete, and changing its name or colour means removing it and creating
    it again.

### Keyring

- **Evolution's items** have `xdg:schema=org.gnome.Evolution.Data.Source` and the attributes
  `e-source-uid` and `eds-origin`. On the host, the item for a manual IMAP account was found
  unlocked. The spike only searched and never fetched a secret.
- **Flatpak:**
  - `org.freedesktop.secrets` is unreachable by default (`ServiceUnknown`), and the manifest
    doesn't grant `--talk-name=org.freedesktop.secrets`. With that permission the items would
    be readable, because gnome-keyring has no per-app access control.
  - The Secret portal can't help here, because it only gives an app its own private key.
  - So in the current Flatpak, E3.3 falls back to asking for the password, as designed.
- **Snap:** the `password-manager-service` plug, already declared, gives full Secret Service
  access once connected, so the items are readable.
- **Side finding, not checked in the app:** for the same reason, `SecretService::connect` in
  `other_accounts.rs` should already fail in the Flatpak build, independently of EDS. It is
  worth checking separately.

### Recommended changes to the plan

1. **E0.2.**
   - Detect EDS by owner checks on the three bus names, without auto-starting it.
   - One proxy set (EDS 3.34+) is enough.
   - The parser accepts `DisplayName[xx]` keys and follows `Parent` to find the GOA collection.
   - Record `[Autocomplete] IncludeMe`.
2. **E1.1.**
   - Use `(or (beginswith "full_name" q) (beginswith "email" q))`, with underscores and no
     separate `nickname` term.
   - Trim the query but don't lower-case it.
   - Local books need `x-evolution-any-field` instead of `#t`.
3. **E1.2.** Fall back to the email address when a card has no FN.
4. **E1.3.** Consider skipping books whose `[Autocomplete] IncludeMe=false`, to match Evolution.
5. **E2: switch to the custom-file design.**
   - **E2.1:** create the source once, with the key file below; disabling the mirror is
     `Remove()` plus deleting the file.
   - **E2.3:** becomes "rewrite the `.ics` atomically when its content hash changes". No
     per-event D-Bus writes, batch sizes or all-or-nothing handling, and `X-LOOKOUT-HASH` is
     optional.
   - **E2.5:** is solved, because the copy is read-only in GNOME Calendar and Evolution.
   - If E2 stays with per-event D-Bus writes instead, it has to:
     - send bare `VEVENT`s;
     - plan each batch from a fresh listing, with a per-item fallback;
     - clear all events before `Remove()`, because of the data-directory leak.
6. **E3.1.** Read accounts from the `Data` property over D-Bus; no file permission is needed.
7. **P.1.** The wildcard `--talk-name` is enough. Optionally add
   `--talk-name=org.freedesktop.secrets` for E3.3 (see the side finding above).
8. **P.2.** Not "unsupported": plug `contacts-service` (E1, E3) and `calendar-service` (E2), and
   note that both need manual connection.

Proposed key file for the mirror source:

```ini
[Data Source]
DisplayName=Lookout
Enabled=true
Parent=local-stub

[Calendar]
BackendName=local
Color=#3584e4
Selected=true

[Local Backend]
CustomFile=file:///home/<user>/.var/app/io.github.gavindi.Lookout/data/lookout/eds-mirror.ics
Writable=false

[Lookout]
Mirror=true
Version=1
```
