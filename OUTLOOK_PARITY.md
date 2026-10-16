# Lookout — Outlook Parity Plan

A gap analysis of Lookout against Microsoft Outlook, covering features (§5) and UI/UX (§6), plus a phased roadmap to close the gaps (§7). It succeeds the completed Phase 1–5 `TODO.md` (removed in 091732a). The README calls Lookout "a reimplementation of Microsoft Outlook"; this file defines what that means feature by feature and how to get there.

Companion docs: [SCAFFOLD.md](SCAFFOLD.md) (architecture), [CHANGELOG.md](CHANGELOG.md) (history).

---

## 1. Purpose and method

**Reference product.** The **new Outlook for Windows / Outlook on the web** is the primary reference, because it is where Microsoft is putting its effort and its feature set is the one users now expect. Features that exist only in classic Outlook desktop (Win32) are tagged *classic-only* and ranked lower.

**What "reverse-engineering Outlook" means here.** It means reproducing behaviour and wire formats, not taking binaries apart. Every interop detail in this document comes from one of these sources:

- Observable UX (menus, defaults, keyboard maps, flows).
- Microsoft's published Open Specifications: [MS-OXMSG], [MS-OXOCAL], [MS-OXCICAL], [MS-OXPROPS], [MS-OXTNEF], [MS-PST], [MS-OXOFLAG], [MS-OXOMSG].
- The Microsoft Graph v1.0 API.
- What Outlook actually puts on the wire: MIME headers, iCalendar `X-MICROSOFT-*` / `X-MS-OLK-*` properties, TNEF payloads. These should be captured as fixtures in `source/test-fixtures/`.

Decompiling or disassembling Outlook is a non-goal (§9): there's licence risk and no need, given the specs above.

**Status legend**

| Mark | Meaning |
|---|---|
| ✅ | Present: Outlook-equivalent behaviour exists |
| 🟡 | Partial: exists, but narrower than Outlook |
| ❌ | Absent |
| ⛔ | Not feasible over IMAP/CalDAV; needs Exchange/Graph, or is explicitly out of scope |

**Effort:** **S** = ≤ 1 day · **M** = ≤ 1 week · **L** = multi-week.

**IDs** (`M-07`, `CAL-12`, …) are stable. Cite them in commit messages and CHANGELOG entries, e.g. "Mail — undo send (M-21)".

---

## 2. Executive summary

### 2.1 Parity scorecard

Counts are tallied from the §5 tables.

| Area (§) | ✅ | 🟡 | ❌ | ⛔ | Verdict |
|---|---|---|---|---|---|
| Mail (5.1) | 35 | 6 | 3 | 2 | Solid IMAP core (sync, IDLE, FTS search, unified inbox, pin, the shared category master list with Outlook's 25 presets, drag-and-drop, undo, snooze synced across installs, follow-up flags with due dates, Outlook's right-click menu, stacked conversations in the reading pane, save-all/zip, RFC 5788 junk keywords, TNEF decode, `@mentions` detection with a filter, rules across client, Graph and Sieve backends, automatic replies across Graph and Sieve, Focused Inbox with Outlook's Move to / Always / Stop over Graph (M-03), Quick Steps in the ribbon and the `Ctrl+Shift+1`…`9` slots, and the organising layer — **Sweep**, Search Folders and Ignore / Clean up conversation). No `.msg`; no delivery receipts. |
| Compose (5.2) | 15 | 3 | 4 | 1 | Good rich editor and identities, with undo send, schedule send, emoji, file drop, an attachment reminder, sensitivity, and forwarding with attachments or forward-as-attachment. |
| Calendar (5.3) | 25 | 6 | 3 | 0 | Views and drag-to-edit are good, and week start, working hours, a second time zone, the full five-state show-as and per-event time zones apply live. The view switcher carries Day/Work week/Week/Month, the horizontal **Schedule** view and the **List** view (3.11). Events share the category master list (2.1), and a right-click offers Outlook's event menu with Ctrl-drag to copy (3.12). Organizer invitations send as iMIP (or defer to an RFC 6638 server): REQUEST on save, update with `SEQUENCE++`, CANCEL on delete; attendees answer from the invite card's split buttons (comment, edit the reply in the composer, or don't send), incoming replies update the organizer's event, with a Tracking view in the editor, and time proposals round-trip (COUNTER/DECLINECOUNTER, with meeting forward). The event editor's **Scheduling Assistant** tab shows attendee availability (RFC 6638 `VFREEBUSY`, the organizer's own cache as a fallback, or Graph `getSchedule` for Microsoft 365, 5.1) with Suggested times; the editor's room suggestions from Graph `places` remain to be wired. |
| People (5.4) | 10 | 1 | 1 | 0 | CRUD, groups and full field editing (anniversary, IM, URL, custom fields, photo) are fine, and the profile card opens on hover/click of any person reference (4.2). Contact lists (`KIND:group` cards) are offered in compose (4.3). CSV import/export matches Outlook's format (4.5). The Graph GAL reaches `/me/people` and `/users` (5.2), the directory merge seam plus the shared debounced `DirectorySource` landed (4.7.1–4.7.2), and the Graph backend and LDAP config model followed (4.7.3–4.7.4); the LDAP backend, wiring and picker/People UI remain (4.7). No merge UI. |
| Tasks (5.5) | 8 | 0 | 3 | 1 | VTODO, Google Tasks and Microsoft To Do (Graph) CRUD, sharing the category master list (2.1), with a navigation pane of smart lists and per-source lists (**My Day**, **Flagged email**, **Tasks**), list create/rename/delete across every backend, and My Day's per-day pin set with due-today/overdue suggestions (4.8, 4.9). No reminders, recurrence or subtasks. |
| Accounts / Microsoft 365 (5.6) | 6 | 0 | 2 | 0 | Microsoft 365 is mail over IMAP/SMTP, and its **calendar** (5.1), **contacts** (5.2) and **To Do** (5.3) are now native via Graph. Focused Inbox, per-message categories, importance and follow-up flags now sync with Outlook (5.4). |
| Security (5.7) | 6 | 0 | 2 | 1 | Remote-image blocking, a sandbox, sender-authentication banners, the external-sender tag, link safety and report phishing exist. No S/MIME/PGP. |
| Desktop / extras (5.8) | 4 | 4 | 4 | 0 | Themes, tray and remappable shortcuts are good. No Outlook keymap preset, PST import or Copilot-style actions. |
| **Total** | **109** | **21** | **22** | **5** | ~69% at parity, ~13% partial |

### 2.2 Top 10 gaps (ranked by user impact)

1. ✅ ~~**Microsoft 365 To Do** (A-05) - done: `/me/todo/lists` and per-list tasks sync through `lookout_graph::todo` into the Tasks view as synthetic `todo:<list id>` sources, sharing the account's "Sync with Outlook" Graph consent (5.3).~~
2. ✅ ~~**Sending meeting invitations as organizer** (CAL-14) - done: client iMIP REQUEST/update/CANCEL, or RFC 6638 server scheduling.~~
3. ✅ ~~**Rules** (M-14) — done across client, Microsoft 365 (Graph) and Sieve backends (2.5).~~
4. **Undo send and undo actions** (M-21, C-09).
5. **Spell check** (C-02). WebKit can do this; it's switched off.
6. **Flags with due dates / follow-up**, and **importance** (M-10, M-12, C-05).
7. **Calendar preferences**: week start, working hours, time zones (CAL-08…CAL-11).
8. **Scheduling Assistant / free/busy** (CAL-19) — CalDAV RFC 6638 and a local fallback landed with 3.8, and Graph `getSchedule` with 5.1; room suggestions from Graph `places` remain to be wired into the editor.
9. **Reopen drafts; forward with attachments** (C-12, C-13).
10. ✅ ~~**Automatic replies (Out of Office)** (M-24) - done: a per-account Settings page (on/off, optional time range, messages) backed by Graph `automaticRepliesSetting` on Microsoft 365 or an RFC 5230 Sieve `vacation` block elsewhere, with an "Automatic replies are on" banner in Mail and a Turn off action.~~

### 2.3 UI/UX at a glance

Lookout's shell already follows new Outlook's layout: app rail, File/Home/View/Help ribbon, folder / list / reading panes, a My Day-style right pane, and Outlook's calendar view names. The gaps are in the details an Outlook user notices within minutes (full spec in §6):

- An **icon-only ribbon**, styled differently in each module; View is greyed out outside Mail.
- The subject **squeezed onto the sender line** in the message list, and column headers shown over multi-line rows.
- **Folder order** that's alphabetical rather than Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive.
- **Calendar clicks open the full editor** instead of Outlook's quick-create and peek popovers; Today/arrows sit on the wrong side.
- Settings grouped differently from Outlook's Accounts / General / Mail / Calendar / People.

### 2.4 Three strategic decisions

1. **Add a `lookout-graph` crate** (Phase 0, Phase 5). Graph is the only route to Outlook's server-side features on Microsoft 365: categories, flags, Focused Inbox, rules, automatic replies, free/busy, Teams links, To Do, GAL. OAuth already works (`source/crates/app/src/microsoft_oauth.rs`, `graph_pin.rs`); only the scopes and a client are missing. Build it to the same actor contract as `lookout-dav` (`CalendarSession`-shaped commands and events), so the calendar, people and tasks UI doesn't change.
2. **Add a local outbox and scheduler** (Phase 0). A single mechanism, a persistent queue of outgoing mail with a `not_before` timestamp, provides **undo send**, **schedule send** and an **offline outbox**. Outlook has all three; Lookout currently sends synchronously from `SendMessage`.
3. **Add an Outlook interop layer** (`source/crates/core/src/interop/`, Phase 0). Keep the mapping tables in one place: importance and sensitivity headers, `X-MICROSOFT-CDO-*` busy status, the category colour presets, flag semantics, TNEF and `.msg` decoding. Feature code then never hard-codes Microsoft-specific strings.

---

## 3. Architecture baseline: where new features plug in

There is a full map in SCAFFOLD.md. These are the extension points the roadmap relies on:

| Seam | Location | Used by |
|---|---|---|
| Mail command/event protocol | `AccountCommand` / `AccountEvent` in `source/crates/mail/src/session.rs` | Any new server-side mail operation (new variants such as `SetImportance`, `ScheduleSend`, `PutSieveScript`) |
| Mail cache (per account SQLite, `PRAGMA user_version` migrations) | `source/crates/mail/src/cache.rs` | Outbox queue, snooze keyword mirror, rules log, search folders |
| DAV actor (calendar, contacts, tasks) | `source/crates/dav/src/session.rs`, `ical.rs`, `recurrence.rs` | iMIP REQUEST/CANCEL, VFREEBUSY, VALARM on VTODO, RELATED-TO |
| Local UI state DB | `source/crates/app/src/ui_state_db.rs` | Safe/blocked senders, reminder dismissals, recent folders |
| JSON config | `source/crates/app/src/app_config.rs` (`settings.json`) | Rules (client-side), Quick Steps, category master list, signature defaults, My Templates snippets |
| GSettings | `source/data/gschema/io.github.gavindi.Lookout.gschema.xml` | Scalar preferences (week start, reading-pane position, mark-as-read delay) |
| UI state and dispatch | `source/crates/app/src/window.rs` (`build_window`) plus the `window/` submodules it wires up (`state.rs` holds `UiState`/`CalendarUiState`) | All UI |
| Context menus | `source/crates/app/src/context_menu.rs` (builder); `folder_menu.rs`, `message_menu.rs` (1.17) | Folder and message menus today; event, contact and task menus next |
| Shortcut registry | `source/crates/app/src/shortcuts.rs` | Outlook keymap preset, new actions |
| Notifications | `mail_notifications.rs`, `reminders.rs` (`ReminderEngine`), `tray.rs`, `launcher_entry.rs` | Reminders window, scheduled-send notices |
| Microsoft auth | `microsoft_oauth.rs` (`EXCHANGE_SCOPES`, `GRAPH_SCOPES`, `COMBINED_SCOPES`) | `lookout-graph` |
| AI assistant | `assistant.rs`, `assistant_tools.rs` (6 read-only tools) | Copilot-style actions (X-05) |

---

## 4. Outlook interop reference (the "RE" section)

Wire-level facts that implementations must match. Anything marked **verify** should be confirmed against a captured sample from real Outlook and the matching fixture added to `source/test-fixtures/`.

### 4.1 Message headers

| Concept | Outlook writes | Outlook reads | Lookout plan |
|---|---|---|---|
| Importance | `Importance: High\|Normal\|Low`, `X-Priority: 1 (Highest)` … `5 (Lowest)`, `X-MSMail-Priority: High\|Normal\|Low` | Any of these, plus RFC 2156 `Priority: urgent\|normal\|non-urgent` | Parse all four into `EmailSummary.importance` (needs envelope header fetch, `BODY.PEEK[HEADER.FIELDS (...)]`). Write all three Outlook headers on send. |
| Sensitivity | `Sensitivity: Personal\|Private\|Company-Confidential` (RFC 2156) | Same | ✅ Compose dropdown (6.5) and reading-pane/popped-out banner. |
| Read receipt | `Disposition-Notification-To:` | Same | ✅ already (`send.rs`, RFC 8098). |
| Delivery receipt | SMTP DSN `NOTIFY=SUCCESS,FAILURE` (RFC 3461) | DSN reports (`multipart/report; report-type=delivery-status`) | Needs DSN support in the SMTP layer (**verify** `lettre`); render DSNs as a status card. |
| Follow-up flag (internet) | `X-Message-Flag: Follow up` (classic, to internet recipients); `Reply-By:` (RFC 2156) | Same | Display "Follow up / Reply by" banner. Parsing exists (`follow_up_from_headers`); showing it needs the two headers in the envelope fetch and a cache version bump (UX-33). |
| TNEF | `application/ms-tnef` part named `winmail.dat` | — | Decode (§4.6). |
| Auth results | n/a (server adds `Authentication-Results:`, RFC 8601) | Drives the "unverified sender" banner | ✅ `auth_results_from_headers` (6.1) fills `EmailBody.auth_results`; the reading pane shows the failure/unverified banner. |

### 4.2 IMAP keywords: cross-client conventions

Exchange does **not** expose Outlook categories or rich flags over IMAP. On Microsoft 365 those exist only through Graph. On other servers Lookout picks the convention:

| Concept | Convention | Lookout today | Plan |
|---|---|---|---|
| Flagged | `\Flagged` | Used for **Pin** (`graph_pin.rs` mirrors it to Outlook's pin MAPI property on Microsoft 365) | Keep. Follow-up flags are separate (next row). |
| Follow-up flag | No standard (Thunderbird has none; Outlook's `followupFlag` is MAPI/Graph only) | `$Lookout-flagged`, plus `$Lookout-flag-due-<unix>` (UTC midnight of the due date) when dated; a completed flag is `$Lookout-flag-done` alone (`core/src/interop/flags.rs`). Where the folder's `PERMANENTFLAGS` lack `\*` (Exchange IMAP) the flag lives in the cache's `follow_up_flags` table, keyed by Message-ID; on Microsoft 365 that table is synced both ways with Graph `flag` (`app/src/graph_mail.rs`) ✅ | The fixed `$Lookout-flagged` atom allows a later `UID SEARCH KEYWORD` for flagged mail outside the cached window. |
| Categories | No standard. Thunderbird uses `$label1…$label5`; Lookout uses `$Lookout-tag-<key>` (`tags.rs`) | 🟡 Lookout-only | Keep the keyword store. On Microsoft 365 also sync to Graph `categories` (§4.4). Optionally read Thunderbird `$label*`. |
| Junk | `$Junk` / `$NotJunk` (RFC 5788 registry) | Set on Report Junk / Not Junk (1.24) and when a blocked sender's mail is junked (2.9) | Set on Report Junk / Not Junk so server filters learn. |
| Forwarded / answered | `$Forwarded`, `\Answered` | **verify** whether set on send | Set both; show the reply/forward indicator icons in the list. |
| Snooze | No standard. New Outlook moves the item to a server **"Scheduled"** folder and keeps the wake time in a MAPI property | Snoozed mail moves to a top-level "Scheduled" folder (created on demand) carrying `$Lookout-snooze-<unix>` (`core/src/interop/snooze.rs`); any connected Lookout install wakes it back to the Inbox, unread. Where the folder's `PERMANENTFLAGS` lack `\*` (Exchange IMAP, typically) the snooze stays local (`cache.rs` `snoozed` table) ✅ | Can't interoperate with Outlook's own snooze over IMAP (⛔). |

### 4.3 iCalendar / iTIP

| Concept | Outlook encoding | Lookout plan |
|---|---|---|
| Show-as (busy status) | `TRANSP` plus `X-MICROSOFT-CDO-BUSYSTATUS: FREE\|TENTATIVE\|BUSY\|OOF\|WORKINGELSEWHERE`; `X-MICROSOFT-CDO-INTENDEDSTATUS` on invites | Read and write both. Map Busy/Free to `TRANSP` for non-Outlook clients (CAL-05). |
| All-day | `DTSTART;VALUE=DATE` plus `X-MICROSOFT-CDO-ALLDAYEVENT:TRUE` | Write both. |
| Importance | `X-MICROSOFT-CDO-IMPORTANCE: 0\|1\|2`, `PRIORITY` | Round-trip. |
| Time zones | `TZID` with **Windows** zone names (e.g. "W. Europe Standard Time") plus embedded `VTIMEZONE` | Reading ✅ (`core/src/tzmap.rs` `windows_to_iana`); writing emits IANA `TZID` plus a generated `VTIMEZONE` (CAL-11, 3.2). |
| Invitation | `METHOD:REQUEST`, `ORGANIZER`, `ATTENDEE;ROLE=REQ-PARTICIPANT\|OPT-PARTICIPANT;RSVP=TRUE` (RFC 5546 / iMIP RFC 6047) | Use the existing `build_imip_vcalendar(…, ImipMethod::Request)` (CAL-14). |
| Cancel | `METHOD:CANCEL`, `STATUS:CANCELLED`, incremented `SEQUENCE` | Same builder, `ImipMethod::Cancel`. |
| Propose new time | iTIP `COUNTER` / `DECLINECOUNTER` (pinned by `test-fixtures/outlook/counter.eml`) | ✅ `ImipMethod::Counter`/`DeclineCounter`; the invite card proposes, the organizer accepts (applies and re-sends the update) or declines (CAL-17). |
| Disallow proposals | `X-MICROSOFT-DISALLOW-COUNTER:TRUE` | ✅ Respect it: the invite card hides "Propose new time" (`cdo::disallow_counter`; `test-fixtures/outlook/disallow-counter.eml`). |
| Online meeting | `X-MICROSOFT-SKYPETEAMSMEETINGURL`, `X-MICROSOFT-ONLINEMEETINGCONFLINK`, plus the link in DESCRIPTION (LOCATION is often just "Microsoft Teams Meeting") | ✅ Read all of them (plus `CONFERENCE`/`URL`) via `interop/onlinemeeting`; the invite card, reminder notification and Month-view peek show a provider-labelled "Join" button, and the editor names the provider. Teams-link creation via Graph is 5.1. |
| Meeting forward | `METHOD:REQUEST` sent by a non-organizer with the new attendee added, plus a forward notification to the organizer | ✅ CAL-18: the invite card and the event editor forward (REQUEST to the new recipients with the recipient added to the calendar part, plus a plain notification to the organizer; the local copy is untouched). |

### 4.4 Microsoft Graph surface (for `lookout-graph`)

| Feature | Endpoint / property | Scope |
|---|---|---|
| Category master list | `GET/POST /me/outlook/masterCategories`; colour `preset0`…`preset24` | `MailboxSettings.ReadWrite` |
| Message categories | `PATCH /me/messages/{id}` `{categories:[…]}` | `Mail.ReadWrite` (already granted) |
| Follow-up flag | The message's `flag` property, a `followupFlag { flagStatus: notFlagged\|flagged\|complete, startDateTime, dueDateTime, completedDateTime }`; Outlook wants a start date with a due date. Read back with the other properties through the `/me/messages/delta` feed (5.4) | `Mail.ReadWrite` |
| Importance | `importance: low\|normal\|high` | `Mail.ReadWrite` |
| Focused Inbox | `inferenceClassification: focused\|other`; overrides at `/me/inferenceClassification/overrides` | `Mail.ReadWrite` |
| Automatic replies | `/me/mailboxSettings` → `automaticRepliesSetting { status, scheduledStartDateTime, scheduledEndDateTime, internalReplyMessage, externalReplyMessage, externalAudience }` | `MailboxSettings.ReadWrite` |
| Server rules | `/me/mailFolders/inbox/messageRules` | `MailboxSettings.ReadWrite` |
| Schedule send (server-side) | Extended property `PidTagDeferredSendTime` (`SystemTime 0x3FEF`) on the draft, then `send` | `Mail.Send` |
| Calendar | `/me/calendars`, `/me/calendarView`, `/me/events` (`showAs`, `isOnlineMeeting`, `onlineMeetingProvider`, `responseStatus`, `attendees[].status`) | `Calendars.ReadWrite` |
| RSVP / propose | `/events/{id}/accept\|tentativelyAccept\|decline` (`sendResponse`, `comment`, `proposedNewTime`) | `Calendars.ReadWrite` |
| Free/busy | `POST /me/calendar/getSchedule`; `POST /me/findMeetingTimes` | `Calendars.Read.Shared` |
| Rooms | `GET /places/microsoft.graph.room` | `Place.Read.All` |
| Shared / delegate calendars | `/users/{id}/calendars`, `calendarPermissions` | `Calendars.ReadWrite.Shared` |
| Contacts | `/me/contactFolders`, `/me/contacts` | `Contacts.ReadWrite` |
| Directory / GAL | `/users?$search=`, `/me/people` | `User.ReadBasic.All`, `People.Read` |
| Profile photo | `/users/{id}/photo/$value` | `User.ReadBasic.All` |
| To Do | `/me/todo/lists`, `/tasks`, `/checklistItems` (steps), `linkedResources` (flagged email) | `Tasks.ReadWrite` |
| Shared mailboxes | `/users/{shared}/mailFolders` | `Mail.ReadWrite.Shared` |

Widen `GRAPH_SCOPES` in `microsoft_oauth.rs` **incrementally per feature**. Every new scope means a fresh consent prompt, and some tenants block admin-only scopes.

### 4.5 Non-Microsoft equivalents

| Outlook feature | Standard used against non-Exchange servers |
|---|---|
| Server rules | Sieve (RFC 5228) managed via ManageSieve (RFC 5804, port 4190); capability discovered through IMAP `SIEVE=` / server config. Gmail has neither, so client-side rules only. |
| Automatic replies | Sieve `vacation` extension (RFC 5230) |
| Free/busy | CalDAV Scheduling (RFC 6638): `POST` VFREEBUSY to the schedule-outbox |
| Server-side invitation delivery | RFC 6638 implicit scheduling (`SCHEDULE-AGENT`), with client iMIP as fallback when the server doesn't schedule |
| Directory | LDAP (`ldap3` crate); CardDAV directory gateways where offered |
| Calendar sharing | CalendarServer sharing (`CS:share`) where Nextcloud/SOGo support it |
| Event attachments | CalDAV Managed Attachments (RFC 8607) |

### 4.6 Outlook file formats

| Format | Spec | Plan |
|---|---|---|
| `winmail.dat` (TNEF) | [MS-OXTNEF] | ✅ (1.21) Decoder in `core/src/interop/tnef.rs`: attachments (`attAttachData`/`attAttachTitle`, plus `attMAPIProps` long names, MIME tags and Content-IDs), the bodies (`PR_BODY_HTML`, else RTF decompressed per [MS-OXRTFCP] and de-encapsulated per [MS-OXRTFEX]), and an embedded iCalendar (an `.ics` attachment or a VCALENDAR body). Hooked into the mail body assembly (`mail/src/body.rs`) and the `FetchAttachment` handler (`mail/src/session.rs`), so TNEF parts become normal parts, fetched on demand as `tnef:<source>:<n>`. A MAPI-native meeting reconstruction remains out of scope. |
| `.msg` | [MS-OXMSG] on a CFB container | Read via the `cfb` crate plus a property-stream mapper (or evaluate the `msg_parser` crate). Open when dropped or double-clicked; "Save as .msg" is lower priority. |
| PST | [MS-PST] | Import wizard. Evaluate Microsoft's open-source Rust PST crate (`outlook-pst`; **verify** status and licence) before falling back to `libpff`. |
| Outlook CSV contacts | Outlook's "Comma Separated Values" export columns (`First Name`, `Last Name`, `Company`, `Job Title`, `E-mail Address`, `E-mail 2 Address`, `Business Phone`, `Mobile Phone`, `Home Street`, …) | ✅ Header-driven tolerant reading and full-column export in `core/src/interop/contacts_csv.rs`, wired into `contacts_view.rs` (P-08, 4.5). |
| `.ics` | RFC 5545 | ✅ Import present; add "Save event as .ics" and "Export calendar". |

### 4.7 Keyboard maps

New Outlook offers keyboard **presets** (*Settings → General → Accessibility → Keyboard shortcuts*: Outlook, Outlook.com, Gmail, Yahoo Mail, Off). Lookout should ship an **"Outlook (desktop)" preset** alongside today's defaults, selectable in Settings → Keyboard shortcuts. The registry in `shortcuts.rs` already supports remapping, so this is a defaults table plus a picker (UI-04).

| Action | Outlook | Lookout default today | Notes |
|---|---|---|---|
| Mail / Calendar / People / Tasks | Ctrl+1 / 2 / 3 / 4 | Ctrl+1 / 2 / 3 / 4 | ✅ |
| Notes / Folders / Shortcuts | Ctrl+5 / 6 / 7 | Ctrl+5 Lookout dashboard, Ctrl+6 Settings | Conflict. In the Outlook preset, move the dashboard to Ctrl+7 and Settings to Ctrl+, |
| New item in current module | Ctrl+N | Ctrl+N (always mail) | Make it module-aware |
| New message / appointment / meeting / task / contact / contact group | Ctrl+Shift+M / A / Q / K / C / L | — | ❌ |
| Reply / Reply all | Ctrl+R / Ctrl+Shift+R | Same | ✅ |
| Forward / Forward as attachment | Ctrl+F / Ctrl+Alt+F | Ctrl+Shift+F / — | Conflict (Lookout Ctrl+F = Find) |
| Search | Ctrl+E, F3 | Ctrl+F | Conflict (Lookout Ctrl+E = Archive) |
| Archive | Backspace | Ctrl+E | |
| Mark read / unread | Ctrl+Q / Ctrl+U | Ctrl+Q (toggle) / — | Add separate unread |
| Toggle flag / custom flag | Insert / Ctrl+Shift+G | Insert = Flag / clear flag (1.16); Ctrl+Shift+G = Pin | The Outlook preset (1.18) moves Pin off Ctrl+Shift+G and opens the Flag menu there |
| Move to folder / go to folder | Ctrl+Shift+V / Ctrl+Y | — | ❌ |
| Delete | Delete, Ctrl+D | Delete | Add Ctrl+D |
| Next / previous item | Ctrl+. / Ctrl+, | — | ❌ |
| Send | Ctrl+Enter, Alt+S | **verify** | |
| Check names / address book | Ctrl+K / Ctrl+Shift+B | — | ❌ |
| Spelling | F7 | — | With C-02 |
| Sync all (Send/Receive) | F9 | — | Maps to `AccountCommand::Refresh` on every account |
| Calendar Day / Work week / Week / Month / Schedule | Ctrl+Alt+1…5 | — | ❌ |
| Go to date | Ctrl+G | — | ❌ |
| Inbox / Outbox | Ctrl+Shift+I / Ctrl+Shift+O | — | ❌ |
| Undo | Ctrl+Z | Ctrl+Z | ✅ (M-21) |

---

## 5. Gap analysis by area

"Today" evidence paths are relative to `source/crates/`.

### 5.1 Mail

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| M-01 | Multi-account, IDLE push, offline cache | `mail/src/session.rs` actor, dedicated IDLE connection, CONDSTORE/QRESYNC | ✅ | — |
| M-02 | Unified inbox | `TreeItem::Unified` / `MailView::UnifiedInbox` | ✅ | — |
| M-03 | Focused / Other inbox | Focused/Other tabs over a Microsoft 365 Inbox (`message_list.rs` `FocusedView`, applied with the filters in `compute_layout`), fed by Graph `inferenceClassification`; "Move to Focused/Other", "Always move…" (per-sender override) and "Stop moving…" in the message menu (5.4) | ✅ | Non-Microsoft accounts keep the tabs hidden until 2.12's local heuristic |
| M-04 | Conversation view | `message_list.rs` `group_threads` (list grouping, selectable conversation headers); `conversation_view.rs` stacked conversation in the reading pane; `mail-conversation-order` newest on top/bottom; your replies from Sent Items included (`Cache::conversation_members_in`) (1.7) | ✅ | Other folders means Sent Items only; Outlook also pulls in Deleted Items and Archive |
| M-05 | Reading pane right / bottom / off | `layout-reading-pane` boolean plus `layout-reading-pane-position`; View ribbon dropdown and Settings row | ✅ | — |
| M-06 | Density and message preview lines | `layout-vertical-spacing` tight/medium/loose; `layout-message-preview-lines` 0–3; `mail-show-sender-images` (vCard photo, else initials) | ✅ | Density still uses Lookout's names (UX-83) |
| M-07 | Sort / arrange by | `message_list.rs` `SortKey { Date, Sender, Subject, Size, Importance, Flag, Category, To, Attachments }`; group headers derived from every sort key (`GroupKey`) | 🟡 | No per-column sortable headers yet (U.4) |
| M-08 | Filter menu | `FilterSet`: Unread, Flagged (open follow-up flags), Pinned, Has files, High importance, Categorized, To me, Mentions me (combinable check items) | ✅ | — |
| M-09 | Categories (named, coloured, multiple) | `app/src/categories.rs` shared master list (`$Lookout-tag-*` keywords), Categorize menu with preset-colour dots, drop-on-category, editor (name, one of Outlook's 25 presets, classic shortcut); on Microsoft 365 synced with Graph `masterCategories` and per-message `categories` (5.4); stored in `settings.json` (2.1) | ✅ | — |
| M-10 | Flag for follow-up with dates | Message-level flag state (keywords, local fallback, Graph `flag` on Microsoft 365 via the messages delta and a cached Message-ID ↔ Graph-id map, §4.2); Flag ▾ (Today, Tomorrow, This week, Next week, No date, Custom date…, Mark complete, Clear flag) following the calendar's week start and work days; Insert toggles; row flag icon and due label (red once overdue); Flag sort and groups by due (Overdue … Later, No date, Completed); "Follow up. Due by …" bar; Undo; Pin stays separate | ✅ | Incoming `X-Message-Flag`/`Reply-By` not shown yet (UX-33) |
| M-11 | Snooze | Snooze ▾ presets (Later today, Tomorrow, This weekend, Next week, Choose a date…); synced via the server "Scheduled" folder plus a wake-time keyword, local fallback where keywords aren't stored (§4.2); wakes on time back to the Inbox, unread; "Snoozed until …" bar with Unsnooze; Undo | ✅ | Outlook's own snooze isn't visible over IMAP (⛔) |
| M-12 | Importance display | `!`/`↓` list glyph from parsed headers; on Microsoft 365 read from Graph `importance` and set from the message menu's Importance ▸ (5.4) | ✅ | — |
| M-13 | Pin | Pinned section, Graph mirror (`graph_pin.rs`) | ✅ | — |
| M-14 | Rules | Three backends (2.5): **client** (`core/src/rules.rs` model + matcher, `app/src/rules.rs` runner on arriving Inbox mail: move/copy/delete/flag/categorize/mark-read/forward/redirect, StopProcessing, body/header deferral); **Microsoft 365** (Graph `messageRules`, `app/src/graph_rules.rs`, per-account, synced on connect/F5/consent); **Sieve** (`mail/src/sieve.rs` compiler + `mail/src/managesieve.rs` RFC 5804 client, uploaded as Lookout's active script). Manager and editor (Config → Mail → Rules, per-rule "runs on" + account); message menu Rules ▸ Create rule; folder "Run rules now"; a rule Sieve/Graph can't express stays client-side | ✅ | — |
| M-15 | Quick Steps | `app/src/quick_steps.rs` runs the chains, `app/src/quick_steps_view.rs` manages them; the model is `lookout_core::QuickStep`, stored in `settings.json` beside the rules. The Mail ribbon's **Quick steps** menu and the `Ctrl+Shift+1`…`9` slots (gallery order) run a step on the selection, Config → Mail → "Quick Steps…" edits them, three role-based defaults ship, and one Undo entry reverses the whole step (2.6) | ✅ | Local only: not synced to Microsoft 365, unlike Outlook's mailbox-side Quick Steps |
| M-16 | Sweep | Mail Home ribbon **Sweep** opens a dialog for the selected message's sender: act on every message in the open folder - all, all except the latest, or older than N days - moving them to Deleted Items or Archive, with an optional standing client rule. The matches come from the account's local cache (`search_query("from:…")` scoped to the folder) | ✅ | — |
| M-17 | Junk: safe / blocked senders | "Report" moves to Junk and sets RFC 5788 `$Junk` (1.24); "Not junk" moves back to the Inbox and sets `$NotJunk`; Empty Junk. Safe/blocked sender lists (2.9): a per-account safe list (the S-01 trusted senders) and blocked list in `ui_state_db`; Settings → Mail → Junk email; the message menu's Block ▸ \| Block sender / Block domain \| Unblock sender moves mail to Junk and drops a conflicting safe entry; new mail from a blocked sender is junked on arrival by `rules::enforce_blocked_senders` (independent of the rules switch) | ✅ | — |
| M-18 | Search syntax and refiners | `mail/src/search_query.rs` query language (`from:`/`to:`/`cc:`/`subject:`/`body:`/`has:attachment`/`is:`/`category:`/`received:`/`size:`/`folder:`) compiled to local FTS5 + JSON predicates (`Cache::search_query`) and IMAP `SEARCH` keys; title-bar scope dropdown, refiner chips, advanced-search form and recent-search history (2.2–2.3) | ✅ | — |
| M-19 | Search Folders (saved searches) | Saved searches (`AppConfig::SearchFolder`, name + query) appear as a **Search Folders** group in the folder tree; selecting one runs its query across all mailboxes. Seeded once with Unread mail, Flagged, Large mail and Has attachments; Config → Mail → "Search Folders…" adds/removes them (2.4) | ✅ | — |
| M-20 | Ignore conversation / Clean up conversation | **Ignore** in the message menu remembers a conversation's thread key (`ignored_threads` in the UI-state database) and moves it to Deleted; new mail in it is moved to Deleted on arrival by `rules::enforce_ignored_threads`. **Stop ignoring** undoes it, and Config → Mail → "Ignored conversations…" manages the list. **Clean up conversation** deletes the conversation's older, wholly-quoted messages (`wholly_quoted`, conservative and Undo-able) (2.8) | ✅ | Cleanup uses the cached preview, not the full body |
| M-21 | Undo (move, delete, flag, archive) | "… / Undo" toast and Ctrl+Z for delete, archive, junk, drag-move, pin, read/unread, categorize and snooze; moves are reverted by `RevertMove` via the COPYUID map (or Message-ID) | ✅ | Follow-up flags are undone too (1.16) |
| M-22 | Mark-as-read options | Setting: when selection changes (default), when opened, after N seconds, never | ✅ | — |
| M-23 | Read receipts | Request toggle, banner, RFC 8098 MDN (`mail/src/send.rs`) | ✅ | Add "always / never / ask" policy |
| M-24 | Automatic replies (OOF) | Settings → Mail → "Automatic replies…": per-account on/off, an optional time period and the messages. Microsoft 365 via Graph `automaticRepliesSetting` (internal/external messages + audience); ManageSieve servers via an RFC 5230 `vacation` block merged into the account's **lookout** script (`:days 1`; scheduled with RFC 5260 `currentdate`), with a capability probe so unsupported servers or schedules degrade in the form; Sieve settings in `settings.json`; "Automatic replies are on (until …)" bar above the message list with Turn off | ✅ | — |
| M-25 | @mentions | A message that mentions one of the user's identities (an `mailto:`/bare address token, or `@Name`) is flagged from its body snippet and marked with an `@` glyph in the list; the **Mentions me** filter and `is:mentions` search match it. Detection runs where the body is already parsed (the preview pass, new-message fetch, on-demand open) and the account's identity aliases count as "me" (`core/src/interop/mentions.rs`, `EmailSummary.mentions`, 2.13) | ✅ | Over IMAP, best-effort: unflagged until a body snippet is fetched (same window as previews) |
| M-26 | Message recall | — | ⛔ | Exchange-internal only |
| M-27 | Delivery receipts (DSN) | — | ❌ | §4.1 |
| M-28 | Print | `print_visible_message` | ✅ | — |
| M-29 | Save as `.eml` / `.msg` / PDF | `.eml` via More menu and drag-out | 🟡 | `.msg`, "Save as PDF" via print-to-file |
| M-30 | View source / message headers | "View message details…" dialog (`window.rs`): parsed key fields, full header block, raw source via `FetchRawMessage`; debug `.eml` viewer retained | ✅ | — |
| M-31 | Open message in new window | Double-click/Enter or ⋯ "Open in new window": standalone read-only `adw::Window` (`message_window.rs`), raw-based render, theme toggle, attachment strip, remote-content banner | ✅ | — |
| M-32 | Attachment preview | Inline `cid:` images; Open / Open with / Save | 🟡 | In-app preview for PDF, images, text (Office via LibreOffice conversion, optional) |
| M-33 | Download / save all attachments, zip | Per-attachment save; "Save all" (folder) and "Download as zip" on the strip header and the conversation card menu (1.23) | ✅ | — |
| M-34 | Remote content / trusted senders | `core/src/trust.rs`, `app/src/trusted_senders.rs` | ✅ | — |
| M-35 | Phishing / unverified-sender / external banners | `EmailBody.auth_results` exists, never filled | ❌ | §4.1 |
| M-36 | Translate | — | ❌ | Pluggable provider (LibreTranslate, or the assistant endpoint) |
| M-37 | Immersive Reader / zoom | Dark message theme toggle; Ctrl+scroll and Ctrl+± zoom on the reading pane, popped-out message window and composer (1.20) | 🟡 | Reader mode (7.5) |
| M-38 | Message context menu | Right-click (or Shift+F10 / Menu) on a row or conversation header: Reply, Reply all, Forward · Delete, Archive, Move ▸, Copy ▸ (recent folders, Other folder…) · Mark as read/unread, Flag ▸, Pin, Snooze ▸, Categorize ▸ · Report ▸ · View ▸ details / source · Open in new window, Print (`message_menu.rs`, 1.17) | ✅ | Ignore (2.8), Block ▸ (2.9), Rules ▸ Create rule (2.5) and Report phishing / Not junk (1.24) join it as those land |
| M-39 | Notifications | `mail_notifications.rs`, tray, badge | ✅ | Add per-folder / VIP-only options |
| M-40 | Work offline / outbox | Cache-first reads; NetworkMonitor-aware errors; sends queue in the local outbox, and a banner above the message list lists waiting, scheduled and failed ones (Send now / Retry / Edit) | 🟡 | "Work offline" toggle; an Outbox folder in the folder pane |
| M-41 | List-unsubscribe | One-click POST plus mailto | ✅ | (Better than classic Outlook) |
| M-42 | Shared mailboxes / delegate access | A shared mailbox can be added by address (Config → Accounts → "Add shared mailbox…") against a Microsoft 365 account; it connects as its own mail account borrowing the parent's OAuth token (`connect_shared_mailboxes`, SASL user = the shared address), so its folders, messages and sends work through the existing session. The Graph transport (`users::resolve_user`, `MailClient::list_mail_folders`/`send_mail_as`, `Mail.ReadWrite.Shared`/`Mail.Send.Shared`) is in place for the folder/send-as surfaces. The exact Exchange Online IMAP login form is unverified against a live tenant | 🟡 | Verify the shared-mailbox IMAP login; wire the Graph folder/send-as surfaces; IMAP `NAMESPACE` shared folders |
| M-43 | Archive folder | SPECIAL-USE `\Archive`, Ctrl+E | ✅ | — |
| M-44 | Drag and drop | Rows → folders/tags; out as `.eml` | ✅ | Drop `.eml`/`.msg` *into* a folder (IMAP APPEND) |
| M-45 | TNEF / winmail.dat | Decoded (`core/src/interop/tnef.rs`, 1.21): inner attachments become normal parts fetched by the existing `FetchAttachment` path (`tnef:<source>:<n>`), the HTML/plain body and RTF (MS-OXRTFCP + MS-OXRTFEX) are read, an embedded iCalendar surfaces as an invite, inline images keep their `cid:`, and a malformed payload stays as `winmail.dat` | ✅ | — |
| M-46 | Voting buttons | — | ⛔ | Decode and display incoming votes only (from TNEF); the payload is now readable (1.21), the display is not built |

### 5.2 Compose

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| C-01 | Rich formatting | `app/src/compose.rs`: B/I/U/S, lists, 4 fonts, 4 sizes, colour, align, indent, HR, link, image, table | 🟡 | Styles/headings, highlight, any installed font, point sizes, format painter, superscript/subscript, paste options |
| C-02 | Spell check | WebKit spellchecking on the composer's own `WebContext`; language picker; F7 | ✅ | — |
| C-03 | Plain text vs HTML | Rich-text toggle; `multipart/alternative` | ✅ | — |
| C-04 | From / Bcc / identities | `app/src/identities.rs`, From dropdown | ✅ | "Send as" for Microsoft 365 shared mailboxes (Phase 5) |
| C-05 | Importance (High/Low) | `ComposedMessage.importance`; High/Low toggles; three headers written | ✅ | — |
| C-06 | Sensitivity | — | ❌ | §4.1 |
| C-07 | Request read / delivery receipt | Read only | 🟡 | DSN (M-27) |
| C-08 | Schedule send / delay delivery | Send ▾ → Tomorrow morning / Tomorrow afternoon / Monday morning / Custom time…; outbox `not_before`; the message waits in Drafts with a "Scheduled to send …" bar (Cancel send); "N scheduled messages" banner and dialog. On Microsoft 365 with Graph access the send is scheduled server-side through Exchange (`sendMail` + `PidTagDeferredSendTime`), so it goes out even with Lookout closed; the local outbox is the fallback elsewhere | ✅ | — |
| C-09 | Undo send | Outbox `not_before` delay (Settings → Mail → "Undo send": Off / 5 s / 10 s, default 5); Undo on the "Sending" toast cancels and reopens the composer | ✅ | — |
| C-10 | Signatures | Rich, per-account defaults for new and reply (`app_config.rs` `signature_defaults`) | ✅ | — |
| C-11 | Drafts autosave | 5 s autosave to IMAP Drafts | ✅ | — |
| C-12 | Reopen a draft | "Edit" banner on a Drafts preview (or Enter/double-click) opens the composer prefilled from the parsed MIME: identity, To/Cc/Bcc, subject, HTML/text, inline images, attachments, importance, receipt, threading; saving replaces the stored draft (UID on first, Message-ID after) | ✅ | — |
| C-13 | Forward with attachments / as attachment | Forward quotes the original (HTML preserved) and re-attaches its file attachments and inline `cid:` images; "Forward as attachment" (ribbon Forward ▾, reading-pane More menu) sends the whole message as a `message/rfc822` part (1.11) | ✅ | — |
| C-14 | Emoji picker | `gtk::EmojiChooser` in the format toolbar (1.20) | ✅ | — |
| C-15 | Paste / drop images and files | Paste images works; file drop onto the composer attaches (1.20) | ✅ | — |
| C-16 | Attachment reminder | Body/subject scan on send → "Send without an attachment?" dialog, toggle in Settings → Mail (1.19) | ✅ | — |
| C-17 | My Templates | `app/src/templates.rs` (2.10): `AppConfig.templates` (`MailTemplate { id, name, html, text }`, mirroring `Signature`); Settings → Mail → My Templates (new/edit/delete via the rich editor); the composer's **My Templates** menu inserts the HTML (rich) or text (plain) at the cursor, with "Manage Templates…" | ✅ | — |
| C-18 | Check names / resolve recipients | Autocomplete plus picker (`recipient_entry.rs`, `contact_picker.rs`); contact lists expand to their members (4.3) | 🟡 | GAL lookup (P-11) |
| C-19 | @mention insertion | — | ❌ | `@` → contact picker → adds to To |
| C-20 | Reply with meeting | Message menu and reading-pane More menu action opens a new event prefilled with the thread's participants (From/To/Cc, the user's own addresses removed) and its subject (3.19) | ✅ | — |
| C-21 | Encrypt / sign | — | ❌ | Phase 6 |
| C-22 | Voting buttons, expiration | — | ⛔ | Exchange-only |
| C-23 | Mail merge | — | ❌ | Phase 7 |

### 5.3 Calendar

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| CAL-01 | Day / Work week / Week / Month | `app/src/calendar_view.rs` `build_main` | ✅ | — |
| CAL-02 | Split / agenda / list view | Split (`build_split_view`) plus a standalone **List** view (`AgendaView` exposed as the `"list"` stack page, 3.11), a chronological day-grouped list over a rolling 30-day window from the anchor | ✅ | — |
| CAL-03 | Schedule (timeline) view | Calendars-as-rows horizontal timeline (`ScheduleView`, `calendar_view.rs`): one lane per checked calendar, event blocks at their exact times over a shared hour axis, show-as styling, working-hours shading, a now-line and click-to-open (3.11) | ✅ | — |
| CAL-04 | Multiple calendars: overlay / side-by-side | Overlay plus checklist | 🟡 | Side-by-side columns |
| CAL-05 | Show as: Free/Tentative/Busy/OOF/Working elsewhere | Full five-state `BusyStatus` on `CalendarEvent`/`EventOccurrence` (`show_as`); reads `X-MICROSOFT-CDO-BUSYSTATUS`/`-INTENDEDSTATUS` with a `TRANSP` fallback and writes both forms; editor dropdown and per-state chip styles (3.3) | ✅ | — |
| CAL-06 | Private / sensitivity | `EventSensitivity` | ✅ | — |
| CAL-07 | Categories with colours | The shared master list (M-09) as a toggle picker; the preset colour dot is shown per row (2.1) | ✅ | — |
| CAL-08 | Week start | Settings → Calendar "Week starts on", applied live to the month grids, mini calendars, Week view, recurrence weekday order and the mail list's "This week / Last week" groups | ✅ | GTK date-picker popovers follow the locale's first weekday |
| CAL-09 | Working hours and days | "Working days" (Work week view's columns) and "Working hours" (the business-hours band, drawn on work days only), applied live | ✅ | — |
| CAL-10 | Second time zone in the grid | "Second time zone" picker adds an outer hour gutter to Day/Week/Work week, labelled with both zones; "Time zone" sets the zone the whole calendar is shown in (`app/src/calendar_tz.rs`) | ✅ | — |
| CAL-11 | Per-event time zone | The event model carries an IANA `time_zone`; the editor's searchable "Time zone" picker enters/edits times in it (new events default to the display zone), `build_vevent` emits `TZID` + a generated `VTIMEZONE` (explicit transitions, event year ±2) and RRULE expansion is zone-anchored so local wall time survives DST; Windows TZIDs still read via `core/src/tzmap.rs` (3.2) | ✅ | — |
| CAL-12 | Reminders | `app/src/reminders.rs` notifications (Open / Snooze 5 min / Dismiss) | 🟡 | Reminders window (Dismiss all, snooze choices), multiple alarms, default reminder setting |
| CAL-13 | Recurrence | Full expansion (`dav/src/recurrence.rs`); builder in `app/src/recurrence.rs` limited to daily/weekly/monthly/yearly | 🟡 | "2nd Tuesday", "last weekday", "day 15", yearly by month/day |
| CAL-14 | Send meeting invitations (organizer) | Saving an event with attendees sends iMIP REQUEST (client-side, or defers to a server advertising RFC 6638 `schedule-outbox-URL`); edits prompt "Send update" and bump `SEQUENCE`; deletes prompt a CANCEL, `RECURRENCE-ID`-anchored for a series instance; the editor's footer reads Send / Send update (3.4) | ✅ | — |
| CAL-15 | RSVP | The invite card's Accept/Tentative/Decline split buttons: Send response now, Edit response before sending (a full-window composer prefilled with the REPLY's `text/calendar` part, so a comment goes out with the answer), Don't send a response (calendar answer only); `respond_to_imip_invitation` gained the send/comment options (3.5) | ✅ | — |
| CAL-16 | Response tracking | An opened REPLY merges its `PARTSTAT`s onto the stored organizer event and PUTs it (skipped when the server owns scheduling; deduped per message); the event editor has a Tracking view with per-attendee status, roles and counts (3.6) | ✅ | — |
| CAL-17 | Propose new time | The invite card's **Propose new time…** dialog (date plus start/end times, valid span required) sends an iTIP `COUNTER` to the organizer, hidden when the invitation carries `X-MICROSOFT-DISALLOW-COUNTER:TRUE`; a counter message's card offers **Accept proposal** (applies the time, `SEQUENCE++`, PUTs, and re-sends the update to all attendees) and **Decline proposal** (`DECLINECOUNTER` with the original times) (3.7) | ✅ | — |
| CAL-18 | Forward meeting | **Forward** on the invite card and in the event editor for a meeting someone else organizes: a recipient dialog whose REQUEST carries the new recipients on the attendee list, plus a plain "Your meeting was forwarded" notification to the organizer; the local copy is untouched (3.7) | ✅ | — |
| CAL-19 | Scheduling Assistant / free/busy / room finder | Event editor **Scheduling** tab: attendee rows × a reusable horizontal time grid (also the shape CAL-03 wants), fed by an RFC 6638 `VFREEBUSY` POST to the account's schedule-outbox; the organizer's own row comes from the cached calendars, and **Suggested times** scans the fetched availability across working hours (the whole day is queried). Where the server doesn't schedule, the tab still shows the organizer and flags attendees as unknown | 🟡 | Graph `getSchedule` landed (5.1); the editor's room suggestions from Graph `places` remain |
| CAL-20 | Teams / online meeting | `interop/onlinemeeting` classifies Teams/Zoom/Meet/Webex/Jitsi link hosts and detects the join URL from `CONFERENCE`, `URL`, the `X-MICROSOFT-SKYPE`/`-ONLINEMEETING` properties or the description; the invite card, reminder notification and Month-view peek each offer a provider-labelled **Join** button, and the editor's video-call row names the provider (3.17) | 🟡 | Teams-link creation / Graph `isOnlineMeeting` toggle (5.1) |
| CAL-21 | Event attachments | — | ❌ | RFC 8607 / Graph |
| CAL-22 | Event search | Calendar "Filter" button disabled | ❌ | Search over `dav` cache |
| CAL-23 | Print calendar | `print_calendar_month`; toolbar button live | ✅ | Day/Week/Month/Agenda print styles still pending (3.20) |
| CAL-24 | Share / publish calendar | The calendar Home ribbon's **Share** action opens a "Share calendar" dialog for a Microsoft 365 calendar: the people it's shared with (roles) and an add form (address + one of Outlook's four roles + a send-invitation switch), backed by Graph `calendarPermissions` (`CalendarClient::list_permissions`/`create_permission`/`update_permission`/`delete_permission`, `Calendars.ReadWrite.Shared`) | ✅ | CalDAV sharing (`CS:share`) stays a follow-up |
| CAL-25 | Open someone else's calendar | "Add calendar" now has an **Open another user's calendar** section: enter an address, Lookout resolves it (`/users/{address}`), lists that user's calendars (`/users/{id}/calendars`) and adds the chosen one, which syncs through `CalendarCommand::SetSharedCalendars` and renders in the sidebar and views (`CalendarInfo.owner`) (5.6) | ✅ | CalDAV principal browse stays a follow-up |
| CAL-26 | Internet calendar subscriptions | `dav/src/subscription.rs` | ✅ | — |
| CAL-27 | Birthdays | `dav/src/birthdays.rs` | ✅ | — |
| CAL-28 | Holidays | "Add calendar" dialog country picker subscribes to a bundled public holiday feed (`core/src/holidays.rs`, `data/holidays.json`, ~50 countries) via CAL-26 (3.15) | ✅ | — |
| CAL-29 | Weather in calendar header | — | ❌ | Optional (Open-Meteo); low priority |
| CAL-30 | Drag to move / resize | `DragMode`, `attach_month_drag`; Ctrl-drag copies the occurrence to the dropped time (3.12) | ✅ | — |
| CAL-31 | Event context menu (copy, duplicate, delete, categorise, show as, private) | Right-click in any view (`event_menu.rs`, `connect_event_context_menu`, 3.12): Open, Delete, Duplicate, Copy to calendar…, Save as .ics…, Categorize, Show as, Private; Duplicate/Copy clone under a fresh UID and open the editor once the server confirms it; inline edits ask the recurring scope | ✅ | — |
| CAL-32 | Calendar colour picker | Server colour or palette (`app/src/calendar_colors.rs`) | 🟡 | Picker; write back `calendar-color` |
| CAL-33 | Calendar groups | Grouped by account | 🟡 | User-defined groups |
| CAL-34 | My Day / calendar peek in Mail | `refresh_mail_overview_day_list` | ✅ | — |
| CAL-35 | Incoming invitation card | `render_invite_card`, plus a conflict indicator ("Conflicts with …") and a mini day timeline of the invited day (`invite_conflicts.rs`; `schedule_grid.rs` `build_day_timeline`) (3.18) | ✅ | — |

### 5.4 People

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| P-01 | Contact CRUD, multi email/phone/address | `core/src/vcard.rs`, `app/src/contacts_editor.rs` | ✅ | — |
| P-02 | Anniversary, IM, website, custom fields | Editor rows for anniversary (`ANNIVERSARY`), IM (`IMPP`), website (`URL`) and custom properties, each property's original parameters preserved (4.1) | ✅ | — |
| P-03 | Photo | Shown in the People list avatar (over the initials) and previewed/set/removed in the editor; Mail sender avatars already use it (4.1); Graph contact photos are fetched and cached as the card's `PHOTO` (5.2) | ✅ | — |
| P-04 | Groups / favourites | Groups, starred contacts | ✅ | — |
| P-05 | Contact lists in compose | `KIND:group` cards (with `X-ADDRESSBOOKSERVER-MEMBER`) appear in autocomplete and the address-book picker; picking one inserts a list chip with a `+` expander (4.3) | ✅ | — |
| P-06 | Search in People | `contacts_view.rs` search box (name/email/org) plus a module-aware title-bar search: searching while People is active filters the contacts list in place (UX-12) | ✅ | — |
| P-07 | Sort / File as | A global "File contacts as" preference (Last, First / First Last / Company) sorts and labels the list; a per-contact `file_as` override (`X-MS-FILEAS` / `SORT-AS`) is edited in the contact editor (4.4) | ✅ | — |
| P-08 | Import/export CSV | `.vcf` plus Outlook CSV import and export (§4.6), format auto-detected on import and chosen from the Export split button | ✅ | — |
| P-09 | Link / merge duplicates | Exact-dupe skip on import | ❌ | Merge UI |
| P-10 | Profile card | `app/src/profile_card.rs` (4.2): a popover on hover/click of a person reference (reading-pane header, message-list row, conversation card, attendee chip) showing photo/initials, name, title·org, phones, address, recent mail (FTS by address), upcoming shared events, and Send email / Schedule meeting / Add-or-Edit contact | ✅ | Invitation-card organizer and People-row triggers not yet wired; the UX-67 "Files" tab waits on attachment preview (M-32) |
| P-11 | Directory (GAL / LDAP) | The Graph half landed with 5.2: `ContactsClient::search_people` (`/me/people`) and `search_directory` (`/users?$search=`, `ConsistencyLevel: eventual`) return directory entries and user photos (`/users/{id}/photo/$value`). The merge seam landed with 4.7.1 (`merge_contact_suggestions` takes a `directory: &[DirectoryPerson]` slice and merges a `Suggestion::Directory` after stored contacts, deduped and `limit`-bounded) and the shared dispatcher with 4.7.2 (`app/src/directory.rs`'s `DirectorySource`: per-account `DirectoryBackend`s, ~250 ms keystroke debounce, a 3-character minimum and a prefix→results cache). The Graph backend followed with 4.7.3 (`app/src/graph_directory.rs`: `/users?$search=` with `/me/people` as fallback, gated by `AppConfig::directory_accounts` plus Graph consent, consent ensured only from the opt-in action) and the LDAP config model with 4.7.4 (`AppConfig::ldap_directories`, bind password in the keyring's `PROTOCOL_LDAP` slot) | 🟡 | The LDAP backend (4.7.5), wiring both sources into the composer, recipient picker and People search (4.7.6–4.7.8) and the Settings opt-in and LDAP dialogs (4.7.9) remain |
| P-12 | Recently deleted | Local Deleted bucket | ✅ | — |

### 5.5 Tasks (To Do)

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| T-01 | Task lists sidebar | `tasks_view.rs` navigation pane: smart lists **My Day**, **Flagged email** and **Tasks** (all), then one row per source (CalDAV calendar, Google Tasks list, To Do list, local list) with its colour, the row filtering the list. Create/rename/delete lists from **+ New list** and each row's ⋯ menu: CalDAV MKCALENDAR (VTODO-only) / PROPPATCH / DELETE, Google Tasks `tasklists` insert/patch/delete, Graph `/me/todo/lists` CRUD (well-known lists guarded), and local lists in `settings.json` — each backend degrades to a toast where it can't (4.8) | ✅ | — |
| T-02 | My Day | A per-day pin set in `ui_state_db` (`my_day_tasks`), the **My Day** smart list showing the pinned tasks plus **Suggestions** (open, due today or overdue), and a task row's right-click **Add to / Remove from My Day**; a date rollover reloads (4.9) | ✅ | Graph To Do has no My Day API, so the pin set stays local (§9) |
| T-03 | Flagged email as tasks | Tasks view "Flagged email" section listing open M-10 flags across accounts (check = mark complete, click = open), also on the dashboard and Mail overview; "Create task from message…" in More | ✅ | Lists cached messages only; Graph To Do's `linkedResources` list is not read separately, since M-10 already syncs the message flags directly (5.3) |
| T-04 | Due, importance, % complete, notes | `app/src/task_editor.rs` | ✅ | — |
| T-05 | Reminders | — | ❌ | `VALARM` in VTODO; Graph `reminderDateTime` |
| T-06 | Recurrence | — | ❌ | `RRULE` in VTODO; Graph `recurrence` |
| T-07 | Steps / subtasks | — | ❌ | `RELATED-TO;RELTYPE=PARENT`; Graph `checklistItems` |
| T-08 | Categories | The shared master list (M-09) as a toggle picker in the task editor (2.1) | ✅ | — |
| T-09 | Sort / group options | The Tasks view's "Sort and group" menu: sort by due/importance/title/created, group by due bucket/list/category/none, plus "Show completed"; persisted in GSettings, with pure tested functions (4.12) | ✅ | — |
| T-10 | Assign / task requests | — | ⛔ | Exchange-only (classic) |
| T-11 | Google Tasks, CalDAV VTODO, local list | `app/src/google_tasks.rs`, `dav` VTODO, `ui_state_db` | ✅ | — |
| T-12 | Microsoft To Do | `lookout_graph::todo` (item 5.3 / A-05): `/me/todo/lists` and `/me/todo/lists/{id}/tasks` are a separate Graph backend, mapped into the same `CalendarTask` and synthetic `todo:<list id>` calendar ids as CalDAV VTODO and Google Tasks, so the Tasks view and the task editor's picker treat the lists like calendars; create/update/delete round-trip (guarded by `If-Match`), while `checklistItems`/`recurrence`/`reminderDateTime` map to their own gaps (T-07/T-06/T-05) | ✅ | — |

### 5.6 Accounts and Microsoft 365

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| A-01 | Accounts from GOA | `goa/src/discovery.rs` (skips accounts without IMAP/SMTP) | ✅ | — |
| A-02 | Manual IMAP/SMTP | `app/src/other_accounts.rs` | ✅ | OAuth for manual Gmail/Microsoft 365 |
| A-03 | Microsoft 365 calendar | `lookout-graph`'s `calendar::session` actor (`calendarView` month sync, event CRUD, RRULE/Teams/RSVP/free-busy mapping) drives a Microsoft 365 account through the same contract as CalDAV; Graph access is opted into from Config (5.1) | ✅ | — |
| A-04 | Microsoft 365 contacts | `lookout-graph`'s `contacts` module drives the People screen through the same `ContactsCache`, snapshot and `ContactCommand` channel as CardDAV: contact folders are the address books, contacts delta-sync both ways (with a full-read fallback), photos are cached as the card's `PHOTO`, and Graph v1.0's lack of a contact-group endpoint leaves `KIND:group` lists local (5.2) | ✅ | — |
| A-05 | Microsoft To Do | `lookout_graph`'s `todo` module (`TodoClient` + `run_todo_session`) drives the Tasks view through synthetic `todo:<list id>` sources: `/me/todo/lists` and per-list `/tasks` sync on connect and the poll cadence, create/update/delete round-trip, and the account connects with the same "Sync with Outlook" Graph consent as the calendar (item 5.3 / T-12) | ✅ | — |
| A-06 | Microsoft 365 mail | IMAP/SMTP with Lookout's own XOAUTH2 (`microsoft_oauth.rs`); per-message properties - categories, follow-up flag, importance and Focused/Other - sync both ways through Graph (5.4) | ✅ | — |
| A-07 | Exchange on-premises (EWS) | — | ❌ | Deferred (§9) |
| A-08 | POP3 | — | ❌ | Low priority; IMAP covers modern hosts |

### 5.7 Security and compliance

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| S-01 | Block remote content | `mail-load-remote-images`, trusted senders | ✅ | — |
| S-02 | Sandboxed rendering | WebKit sandbox | ✅ | — |
| S-03 | Sender authentication banners | `auth_results_from_headers` (RFC 8601) fills `EmailBody.auth_results`; the reading pane and popped-out window show a failure/unverified info bar, plus a `From` display-name ↔ address mismatch banner (6.1) | ✅ | — |
| S-04 | S/MIME sign/encrypt/verify | — | ❌ | GPGME (gpgsm) |
| S-05 | OpenPGP | — | ❌ | GPGME (gpg) |
| S-06 | External-sender tag | `is_external_sender` plus a `mail-internal-domains` list; the reading pane and popped-out window tag an outside sender as external (6.2) | ✅ | — |
| S-07 | Sensitivity labels (Purview) | ⛔ | Tenant-specific; show the header-based sensitivity only |
| S-08 | Link safety (hover shows real URL, warn on mismatch) | Hover status strip with the real URL; a pure scanner warns (and confirms before opening) when an anchor's text names a different host (6.3) | ✅ | — |
| S-09 | Report phishing | Report phishing in the Report menu and split button: forwards the message as a `message/rfc822` attachment to a configured address, then moves it to Junk (6.7) | ✅ | — |

### 5.8 Desktop, UI and extras

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| UI-01 | Ribbon (simplified / classic) | Ribbon-style toolbar | 🟡 | Add a View tab: layout, reading pane, conversations, density, preview lines, zoom |
| UI-02 | Themes / dark mode | System / Flat Dark / Flat Light, accent | ✅ | — |
| UI-03 | Remappable shortcuts | `shortcuts.rs` (19 actions) | ✅ | — |
| UI-04 | Outlook keyboard preset | — | ❌ | §4.7 |
| UI-05 | Tray / badge / notifications | `tray.rs`, `launcher_entry.rs` | ✅ | — |
| UI-06 | Settings breadth | `app/src/config_view.rs` ("Privacy" is a placeholder) | 🟡 | Calendar, People, Tasks, Reading and Compose sections |
| X-01 | Notes (sticky) | — | ❌ | *classic-only*; optional |
| X-02 | PST import | — | ❌ | §4.6 |
| X-03 | Attachment preview | See M-32 | 🟡 | |
| X-04 | Translate / Immersive Reader | See M-36/M-37 | ❌ | |
| X-05 | Copilot (summarise, draft, coach) | `assistant.rs` chat with 6 read-only tools, dashboard only | 🟡 | Summarise thread in the reading pane; draft reply; rewrite selection in compose; action tools with confirmation |
| X-06 | Lookout dashboard | `app/src/lookout_view.rs` | ✅ | No Outlook equivalent (keep) |

---

## 6. UI/UX parity

Feature parity on its own isn't enough. An Outlook user should find every user-facing function **where Outlook puts it, under Outlook's name, behaving the way Outlook behaves by default**. This section is the UX specification. Every user-facing roadmap item in §7 must meet the matching rows here, as well as its functional gap row in §5.

**Basis of comparison.** For Lookout: the current UI code (`source/crates/app/src/window.rs` and its `window/` submodules, `calendar_view.rs`, `compose.rs`, `folder_tree.rs`, `recipient_entry.rs`) and the screenshots in `assets/screenshots/`. Screenshots can lag the code, so recheck screenshot-derived rows (marked **verify**) against a current build. For Outlook: new Outlook for Windows / OWA as of 2026. Ribbon contents and orders are approximate and should be re-captured from a live build before implementation.

### 6.1 Principles

1. **Place and name things as Outlook does.** Command placement (which ribbon tab, menu or pane), labels, menu order and default behaviour follow Outlook. Muscle memory is the product.
2. **Draw them as GNOME does.** Widgets, focus rings, dialogs, accessibility and theming stay libadwaita. Lookout matches Outlook's *information architecture and interaction model*, not its pixels or branding (§9).
3. **When they conflict,** Outlook wins on placement, wording, defaults and shortcuts (with the Outlook keymap preset, §4.7). GNOME HIG wins on widget choice, spacing, accessibility and anything platform-level (notifications, file choosers, portals).
4. **Same surface, same verbs.** One action has one name and one icon everywhere it appears: ribbon, hover actions, context menu, keyboard. Outlook uses the same consistency.
5. **Every destructive or bulk action gets an Undo toast** (M-21), as in Outlook. Prefer undo to "Are you sure?" dialogs, except for permanent deletes.

### 6.2 Terminology

Use Outlook's words in the UI (code identifiers can stay as they are).

| ID | Outlook term | Lookout today | Action |
|---|---|---|---|
| UX-01 | **Categorize** / Categories | UI says "Categorize"; docs and code say "color tags" | Keep the UI label; rename "tags" to "categories" in user-facing docs, settings and tooltips |
| UX-02 | **Flag** (follow-up) vs **Pin** | Both icons in the ribbon; Flag is the follow-up flag (1.16) | ✅ Flag = follow-up flag (M-10); Pin = pin. Never use one word for the other |
| UX-03 | **Junk Email** | Server folder name shown verbatim (e.g. "Junk Mail") | Show the well-known display name for SPECIAL-USE roles (Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive), with an option to "Use server folder names" |
| UX-04 | **New mail** / **New event** / **New contact** / **New task** | Tray says "Compose…"; ribbon icon has no label | "New mail" everywhere (tray, ribbon, menus) |
| UX-05 | **Report** ▸ Report junk / Report phishing / Not junk | "Report" (junk only) | Split button with the three verbs (M-17, S-09) |
| UX-06 | **Move to** ▸ (recent folders, Other folder…) | Drag-and-drop; context menu Move ▸ / Copy ▸ with recent folders and Other folder… (1.17) | Ribbon action (UX-21), reusing the context menu's recent folders |
| UX-07 | **Read / Unread**, **Mark all as read** | "Mark read/unread" toggle | Label shows the action that will happen ("Mark as unread" on a read message) |
| UX-08 | **Discard** (compose) | Trash icon | Tooltip and accessible name "Discard" |
| UX-09 | **Automatic replies**, **Focused / Other**, **Sweep**, **Quick steps**, **Rules**, **Reading pane**, **My Day**, **To Do** | Mostly absent (features missing) | Use these exact names when the features land |
| UX-10 | **Tasks** vs **To Do** | "Tasks" | Keep "Tasks" (classic Outlook's name) but call the smart lists "My Day", "Important", "Planned", "Flagged email" (UX-71) |

### 6.3 Application shell

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-11 | Title bar search | Centred search box; scope dropdown at its left edge ("Current folder / Current mailbox / All mailboxes"); filter (sliders) button at its right edge opening an advanced-search form (From, To, Subject, Keywords, Date range, Has attachments) | Left-aligned "Search mail" box (`window.rs` ~3226); centred "Lookout" window title (`adw::WindowTitle`) | Centre the search box in the title bar and drop the redundant title text. Add a scope dropdown and an advanced-search popover (pairs with M-18, 2.2–2.3) | M |
| UX-12 | Module-aware search | Placeholder and scope follow the module: Mail searches mail, Calendar events, People contacts | Always "Search mail"; searching from Calendar/People/Tasks switches to Mail | Module-aware placeholder and results (CAL-22, P-06) | M |
| UX-13 | Title-bar right cluster | My Day toggle, notifications/activity bell, **Settings gear**, Tips | Two pane-toggle icons; Settings gear at the **bottom of the nav rail** | Move Settings to the title bar right (keep Ctrl+, shortcut). Make the My Day toggle explicit, with a tooltip. Bell optional (reminders, scheduled-send results) | S |
| UX-14 | App bar (nav rail) | Mail, Calendar, People, (Groups), To Do, then More apps; selected app has an accent pill and filled icon; tooltips | Dashboard logo, Mail, Calendar, People, Tasks | Order already matches. Add tooltips with shortcuts ("Calendar (Ctrl+2)") and a filled/accent selected state. Move the Lookout dashboard below the core apps (where Outlook's More apps sits) | S |
| UX-15 | Ribbon tabs | File, Home, View, Help (Simplified ribbon); contextual tabs while composing (Message, Insert, Format text, Draw, Options) | File, Home, View, Help ✅; View greyed out outside Mail (calendar/people/tasks screenshots) | Add a View tab to every module (UX-16, UX-54, UX-66, UX-74). Add compose contextual tabs (UX-40) | M |
| UX-16 | Ribbon style | Simplified ribbon: one row of **icon + label** buttons in separated groups, split buttons (▾) for variants. As the window narrows, labels drop, then buttons move into a trailing "…" overflow. A toggle switches Simplified / Classic (two rows, large icons, group captions). Ctrl+F1 collapses | Mail Home: **icon-only** buttons, no labels or groups. People: text-only buttons. Calendar: segmented control. Overflow "⋮" exists | One ribbon component with Outlook's adaptive behaviour (labels → icons → overflow), used by every module. Optional Classic layout and collapse toggle. Fixes today's three different ribbon styles | M |
| UX-17 | Toasts | Bottom-left toast: action text plus **Undo** (and "View" for sends) | `adw::ToastOverlay` used for errors and some actions | Standardise wording ("Moved to Archive", "1 item deleted", "Message sent") and always offer Undo when an inverse exists (0.4) | S |
| UX-18 | My Day pane | Right-hand pane with **Calendar** and **To Do** tabs; agenda for today and next days; drag a message onto it to create an event or task | Right pane with mini month, "Today" and "Outstanding tasks" stacked (mail screenshot) | Tabbed Calendar / To Do layout. Drop targets on the pane create an event (prefilled from subject and body) or a task (flagged-email link) | M |
| UX-19 | Pane navigation | F6 / Shift+F6 cycle folder pane → list → reading pane → ribbon; Ctrl+Shift+Tab within | **verify** | F6 pane cycling and visible focus | S |
| UX-20 | Settings window | Modal dialog; left categories **Accounts, General, Mail, Calendar, People, (Copilot), Privacy and data**, each with Outlook's sub-pages | Modal dialog (`config_view.rs`): Accounts, Signatures, Identities, Appearance, Mail, Calendar, Google Tasks, Assistant, Keyboard shortcuts, General, Advanced, Privacy (placeholder) | Regroup to Outlook's structure. Mail: Layout, Compose and reply (signatures, identities), Attachments, Rules, Sweep, Junk email, **Customize actions**, Message handling, Forwarding, Automatic replies, S/MIME. Calendar: View, Events and invitations, Weather, Shared calendars. General: Appearance, Language and time, Notifications, Keyboard shortcuts, Startup. Remove the Privacy placeholder until it has content | M |

### 6.4 Mail module

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-21 | Mail Home ribbon order | **New mail ▾** · Delete ▾ · Archive · Report ▾ · Sweep · Move to ▾ · Reply · Reply all · Forward ▾ · Read/Unread · Categorize ▾ · Flag ▾ · Pin · Snooze ▾ · Quick steps ▾ · Print · … | New, Reply, Reply all, Forward, Delete, Archive, Report, Pin(★), Flag ▾ (1.16), Read/Unread, Categorize, Snooze ▾ (presets, 1.15), Quick steps ▾ (2.6), ⋮ (icon-only) | Reorder into Outlook's groups. Add **New mail ▾** (New event, New contact, New task), Sweep, Move to ▾ (recent folders, "Other folder…"), Forward ▾ (Forward as attachment), Snooze ▾ presets. Labels per UX-16 | S |
| UX-22 | Mail View ribbon | View settings · Messages ▾ (Show as conversations / individual messages) · Layout: Folder pane ▾ (show / collapsed / hide), Reading pane ▾ (right / bottom / hidden), Density ▾ (Full / Medium / Compact), Message preview ▾ · Zoom · Sender image · Immersive reader · My Day | "Layout" group (`window.rs`, `view_toolbar`): Folder pane, Reading pane ▾, Calendar overview, Messages ▾ (conversations / individual messages, newest message on top / bottom; 1.7) | Fill out to Outlook's set: M-05, M-06, M-37, X-04 | M |
| UX-23 | Folder pane order | Favorites, then each account (by email address) with **Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive**, Notes, Conversation History, then user folders A–Z; "Create new folder" at the end of each account | All Inboxes, Favorites, accounts; each account sorts **Inbox first, then everything alphabetical** (`folder_tree.rs` `sort_paths`), so Deleted Items lands before Drafts | Sort SPECIAL-USE roles in Outlook's fixed order before user folders. Add a "Create new folder" row. Keep All Inboxes (Outlook for Mac has it; harmless) | S |
| UX-24 | Folder counts | Unread count for most folders; **Drafts shows its total item count**; Junk shows unread; Deleted shows nothing by default | Unread count everywhere; folder ⋯ menu has a count option (`folder_menu.rs`) | Default Drafts to total count, matching Outlook | S |
| UX-25 | Collapse folder pane | ☰ button collapses the pane to a narrow strip (Favorites icons) | Pane is resizable only | Collapse toggle (☰) and View ▸ Folder pane ▸ Collapsed | S |
| UX-26 | List header | Folder name + ☆ (favorite) · **Focused \| Other** tabs · Select (checkbox) · **Filter** ▾ (one menu: Unread, Flagged, To me, Has files, Mentions me, then **Sort by** and order) | Folder name + account subtitle · ☆ · select checkbox · refresh · "All ▾" filter · sort-direction button · "By Date ▾" | Merge filter and sort into one Filter menu with Outlook's items (M-07, M-08). Focused/Other tabs when enabled (M-03). Keep refresh only if Outlook keymap F9 isn't enough (optional) | S |
| UX-27 | Column header row | **Only in the single-line (wide / reading pane off or bottom) layout.** Sortable columns: Importance, Icon, Attachment, From, Subject, Received, Size, Categories, Flag status; right-click a header to add or remove columns | Fixed "Sender / Subject / Date" header shown **above the two-line rows** (screenshots) | Hide headers in the multi-line layout. Add the single-line columnar layout with clickable, configurable headers, used automatically when the reading pane is bottom/off or the list is wide (pairs with 1.4) | M |
| UX-28 | Message row (multi-line) | Line 1: **sender** (bold if unread); hover actions on the right (Delete, Flag, Pin, Read/Unread). Line 2: **subject** (accent colour and bold if unread) + received time on the right. Line 3: preview. Status icons: attachment, importance (!), flag, meeting, replied/forwarded arrow, reminder bell, **category chips** under the preview. Unread = accent bar on the left edge | Line 1: sender + 📎 + **subject squeezed onto the same line** (often truncated to "Te…") + date. Line 2: preview. Avatar; unread accent bar ✅; hover quick menu (`window.rs` "hover-quickmenu"); flag icon and due label ✅ | Move subject to its own line (Outlook's three-line default), with "Message preview" lines from UX-22. Add the status-icon set (importance, flag, replied/forwarded, meeting) and category chips. Bold sender plus accent subject for unread | M |
| UX-29 | Selection affordance | Avatar becomes a round checkbox on hover / in Select mode; Ctrl and Shift click; reading pane shows "N items selected" with bulk actions | Ctrl/Shift multi-select; "multi" reading-pane page ✅ | Avatar → checkbox on hover; bulk-action buttons on the multi page | S |
| UX-30 | Group headers and dates | Today, Yesterday, Last week, Two weeks ago, Three weeks ago, Last month, Older; today shows time, this week "Mon 10:42", older a short date | Today, Yesterday, Last Week, This Month, month names, Older; similar date formats ✅; groups for every sort key, flag due included ✅ | Align bucket names to Outlook. Group headers for every sort key (by sender A–Z, by size range, by category, by flag due) | S |
| UX-31 | Draft marker | Conversation with an unsent reply shows a red **[Draft]** prefix in the list | — | Show [Draft] when Drafts holds a message whose `In-Reply-To` matches (pairs with C-12) | S |
| UX-32 | Reading pane header | Subject on top; avatar; sender name (click → **profile card**); To/Cc expandable; date; action bar: (reaction), Reply, Reply all, Forward, **⋯** (Reply all by meeting, Forward as attachment, Print, Translate, Immersive reader, View ▸ message details / source, Report, Block, Create rule, Open in new window) | Subject, avatar, name, address, To, date, Reply / Reply all / Forward, contact button, ⋮ ✅ (screenshot) | Make the ⋯ menu Outlook's list (M-30, M-31, M-36, M-37). Clicking the name opens the profile card (P-10). Reactions ⛔ (Exchange-only) | S |
| UX-33 | Info bars | Stacked bars above the body: "You replied to this message on …" / "You forwarded …", "Some content has been blocked — Show blocked content", "This sender is external", "This message was identified as junk — It's not junk", "Follow up. Due by …", "This item will be deleted in N days", importance/sensitivity notes | Remote-content banner ✅; read-receipt banner ✅; invitation banner ✅; "Follow up. Due by …" ✅ | One info-bar stack component with Outlook's wording and order. Add replied/forwarded (`\Answered`/`$Forwarded`), external (S-06), junk ("It's not junk"), follow-up (M-10), importance/sensitivity (M-12, C-06) | M |
| UX-34 | Attachments in reading pane | Tiles (file-type icon, name, size) under the header; each tile ▾: Preview, Download, Save to…; "Download all" / "Save all" link | Attachment rows with a ☰ menu (Open / Open with / Save) ✅ | Tile layout, Preview as the default click (M-32, 7.3), "Download all" (M-33) | S |
| UX-35 | Conversation in reading pane | Stacked message cards, newest on top (option): older collapsed to sender + one-line preview, click to expand; inline "Reply" at the top | Stacked cards in one WebView (1.7) ✅: the newest, unread and selected messages open, others collapsed to sender + preview; Reply / Reply all / Forward / ⋯ on each card | — | — |
| UX-36 | Message context menu | Reply, Reply all, Forward · Delete, Archive, Move ▸, Copy ▸ · Mark as unread, Flag ▸, Pin, Snooze ▸, Categorize ▸ · Ignore, Report ▸, Block ▸ · Rules ▸ Create rule · View ▸ · Open in new window, Print | This list and order ✅ (1.17), less Ignore; Report ▸ (junk / phishing / not junk, 1.24 + 6.7), Block ▸ (2.9) and Rules ▸ Create rule (2.5) are present | Add Ignore (2.8) in its slot | S |
| UX-37 | Default open / navigation | Single-click selects (marks read per setting); **double-click or Enter opens a separate window**; after delete/move: "open next item" (setting: next / previous / return to list); Esc closes a popped-out item | Single-click opens in the pane; double-click/Enter open a standalone window (1.9); Esc closes that window, and **hides** the reading pane in the main window | "After moving or deleting" setting. Esc should close popped-out items in the main window too, not hide the reading pane (remap Lookout's close-pane action, or keep it only in the Lookout keymap) | S |
| UX-38 | Drag and drop | Drag to a folder = move; **Ctrl+drag = copy**; drag onto the Calendar app icon / My Day calendar = new event; onto To Do = task; onto a category is not supported | Rows → folders/tags; out as `.eml` ✅ | Add Ctrl=copy, drop on nav-rail Calendar/Tasks icons and My Day (UX-18). Keep drop-on-category (a Lookout extra) | S |
| UX-39 | Sender avatars | Contact photo if known, else coloured initials; setting "Show sender images" | Coloured initials ✅ | Photos from vCard / Graph (P-03); on/off setting | S |

### 6.5 Compose

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-40 | Compose ribbon | Contextual tabs replace Home: **Message** (Attach file ▾, Signature ▾, Importance, Dictate), **Insert** (File, Picture, Table, Link, Emoji, Signature, My Templates, Meeting), **Format text** (font, size, B I U S, highlight, colour, bullets, numbering, indent, align, styles, clear), **Draw**, **Options** (Importance, Sensitivity, Receipts, Delay delivery, Encrypt, Switch to plain text, Spelling language) | Formatting toolbar takes over the action bar (one strip); it now carries a **My Templates** menu beside Signature (2.10) | Split into Message / Insert / Format text / Options tabs on the shared ribbon component (UX-16). Draw is out of scope | M |
| UX-41 | Field order | Top bar: **Send ▾** (Schedule send), Discard, Pop out. Then **From** (shown with multiple identities), **To** (Cc / Bcc links at the right end), Cc, Bcc, **Subject**, attachments strip, body. Tab order To → Cc → Subject → body | Send, From dropdown, then To / Cc / Bcc / Subject (`compose.rs` `fields_group`) ✅. The older `mail_compose_pop_in_1.png` screenshot shows Subject above To and should be refreshed | Keep the order; check Tab order To → Cc → Bcc → Subject → body. Send is a split button with Schedule send ✅ (C-08) | S |
| UX-42 | Recipient pills | Resolved recipients are **pills** (name, presence/avatar, ✕), external recipients flagged, contact lists show a "+" to expand, invalid addresses underlined in red; autocomplete rows show name, address and an ✕ to forget the suggestion | Compose uses `recipient_entry.rs` chip mode (4.3), with a list chip that expands to members; external recipients aren't flagged yet and there's no "Remove from suggestions" | Flag external recipients; add "Remove from suggestions" (deletes from the `addresses` table) | M |
| UX-43 | Attachments while composing | Tiles under the subject line; attach via Insert ▸ File ▸ *Browse this computer* / *Recent attachments*; drag-in; tile ▾: Open, Remove, (Share as link) | Attach button; file drop blocked (`compose.rs:722`) | Tiles, a recent-attachments list, drag-in (C-15) | S |
| UX-44 | Missing icons | — | Compose toolbar shows red "missing image" placeholders on several buttons (`assets/screenshots/mail_compose_pop_in_1.png`) | **verify** on a current build; bundle or rename any missing symbolic icons | S |
| UX-45 | Send feedback | Toast "Sending…" with **Undo** for N seconds, then sent; schedule send shows a "Scheduled for …" info bar on the draft | "Sending: <subject>" toast with Undo for the undo-send delay, then "Message sent"; failure toast; schedule send shows "Scheduled for …" with Undo, and the draft's bar reads "Scheduled to send …" with Cancel send ✅ | — | S |
| UX-46 | Inline vs pop-out | Reply opens **inline** at the top of the conversation in the reading pane, with a Pop out button; New mail opens in the reading pane (or a window by setting) | Same ✅ | — | — |

### 6.6 Calendar

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-50 | Calendar Home ribbon | **New event ▾** · Day ▾ · Work week · Week · Month · Split view · Filter ▾ · Share ▾ · Print | New-event icon · Day · Work week · Week · Month · Split view ✅ · Filter/Share/Print **disabled** | Labels match. Make New event a split button (Event, Teams meeting). Enable Print (CAL-23 bug), Filter (categories, show as, recurring), Share (CAL-24) | S |
| UX-51 | Date navigation | **Today** button and **‹ ›** arrows on the **left**, then the range title, which is a dropdown that opens a month picker | Range title on the left; Today and arrows on the **right** | Move Today/arrows left of the title; title becomes a picker. Ctrl+G Go to date (§4.7) | S |
| UX-52 | Left pane | Mini month (optional week numbers) · Add calendar · My calendars (coloured checkbox per calendar; hover ⋯: Colour, Rename, Share, Remove) · other groups · Show all | Mini month · Add calendar · My calendars checklist ✅ | Coloured checkboxes in the calendar's colour; per-calendar ⋯ menu (CAL-32); user groups (CAL-33) | S |
| UX-53 | Time grid | Working hours lighter ✅; **current-time line** (red line plus dot) across today; scrolls to around the current time ✅; 30-minute slots, single-click selects a slot, **drag selects a range** ✅ | Business-hours band on work days, scroll-to-now (`calendar_view.rs` `scroll_time_grid_to_now`) and a second time-zone gutter (CAL-10) ✅; no current-time line found in the paint code (**verify**) | Add the now-line. Week numbers in the gutter | S |
| UX-54 | Calendar View ribbon | Time scale (5/6/10/15/30/60 min) · Colour (calendar vs category) · Show week numbers · Split view orientation · Weather · Overlay vs side-by-side | View tab disabled | Add a View tab with these toggles (CAL-04) | M |
| UX-55 | Quick create | Single-click or drag on empty time → **quick-create popover** (title, time, all-day, invite attendees, location, Teams toggle, Save, **More options** → full form). Double-click → full form | Activating a slot opens the full editor | Quick-create popover as the default; full editor via More options / double-click | M |
| UX-56 | Event peek | **Single-click an event → peek card** (title, time, recurrence, location, Join button, organizer, RSVP buttons for invites, Edit, Delete, ⋯). Double-click → full form | Click opens the full editor (`open_event_editor_for` from `connect_event_activated`) | Peek popover on click; editor on double-click / Edit | M |
| UX-57 | Event chips | Calendar-colour fill with a darker **left stripe**; title, location; icons for recurring ↻, private 🔒, online meeting, reminder; **show-as styling**: tentative = hatched, free = outline only, OOF = purple, working elsewhere = dotted | Coloured chips at 92% opacity; the show-as styling landed with 3.3 (CAL-05): free hollow, tentative hatched, away purple-tinted, working elsewhere dotted | Left stripe and the icon set (Track U.7) | S |
| UX-58 | Month view overflow | "+N more" opens a day popover listing everything | "+N more" label, no popover (`calendar_view.rs` comment ~439) | Day popover | S |
| UX-59 | Event form | Title · Attendees (Required / Optional; "Response options" ▾) · date/time row with All day, **time zone**, **Repeat** · Location (room suggestions) · **Teams meeting** toggle · Reminder ▾ · Show as ▾ · Categorize ▾ · Private · **Scheduling assistant** · **rich-text** description · footer button reads **Save** with no attendees and **Send** with attendees | Editor has most fields, the date/time row's **time zone** picker (CAL-11, 3.2), the Save/Send footer switch (3.4) and a **Scheduling assistant** tab (3.8); plain-text Notes | Field order per Outlook; rich-text notes (reuse the composer's WebKit editor) | M |
| UX-60 | Invitation card in mail | Accept ▾ / Tentative ▾ / Decline ▾ (each: *Edit response before sending* / *Send response now* / *Don't send a response*), **Propose new time ▾**, and a **mini day timeline** showing the slot against your other events with conflicts highlighted | The card's Accept ▾ / Tentative ▾ / Decline ▾ split buttons offer Send response now / Edit response before sending / Don't send a response (`render_invite_card`, 3.5); **Propose new time…** (hidden by `X-MICROSOFT-DISALLOW-COUNTER`) and **Forward** follow them (3.7), a counter's card swaps in Accept proposal / Decline proposal, and the banner dialog also says Tentative. A **Conflicts with …** line and a mini day timeline (`invite_conflicts.rs`, `schedule_grid.rs`) show the slot against the cached events (3.18) | — | M |
| UX-61 | Reminders window | Separate always-on-top window: list of due items, "Dismiss all", Snooze ▾ duration, Join (online meetings), Open | Desktop notifications only | 3.9 (CAL-12) | M |

### 6.7 People

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-65 | Layout | Left: **Favorites, Your contacts, Contact lists, Deleted, Directory/Groups**. Middle: alphabetical list with **letter headers**, avatars, Filter/Sort ▾. Right: **details pane** (photo, name, title/company, action buttons *Send email*, *Schedule meeting*, *Call*, tabs *Overview / Contact info / Files / Emails*) | Left bucket pane; list; edit in a dialog (`contacts_editor.rs`) | Letter headers; details pane with Outlook's actions and an Emails tab (recent mail via FTS, P-10); edit in place in the details pane, with dialog fallback for pop-out | M |
| UX-66 | People ribbon | New contact ▾ (Contact, Contact list) · Edit · Delete · Add to favorites · Add to list · Import contacts · Export contacts · View ▸ sort / file-as | Text buttons: New contact, Import…, Export…, Manage groups… | Icon+label ribbon (UX-16), Outlook verbs, View tab (P-07) | S |
| UX-67 | Profile card | Hover or click a person anywhere (reading pane, attendee list, chip) → card with contact info, recent emails, files, org, "Schedule meeting" | Header contact button opens the editor | 4.2 (P-10) ✅: the card opens on hover/click of the reading-pane header, message-list row, conversation card and attendee chip, with contact info, recent emails, org, shared events and "Schedule meeting". "Files" waits on M-32; the invitation-card organizer and People-row triggers still to wire | M |

### 6.8 Tasks

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-71 | Navigation | Left: **My Day, Important, Planned, Assigned to me, Flagged email, Tasks**, then user lists (+ New list) | A navigation pane with **My Day, Flagged email, Tasks** then one row per source list + **New list** (4.8); the due buckets (Overdue / Today / This week / Later / Completed) group the Tasks list (T-09) | Add the remaining smart lists (Important, Planned, Assigned to me) as their data lands; keep the buckets as the grouping inside "Planned" | M |
| UX-72 | Quick add | "**Add a task**" entry at the top of the list, with inline icons for due date, reminder and repeat; Enter adds | "New Task" button opens the editor dialog | Quick-add row; dialog only for details | S |
| UX-73 | Task details | **Right-hand details pane**: title + ☐, Steps ("+ Add step"), Add to My Day, Remind me, Add due date, Repeat, Pick a category, Add file, Add note; created/edited footer | Modal editor (`task_editor.rs`) | Details pane (T-05…T-08). ☆ Important toggle on rows (maps to priority High) | M |
| UX-74 | Tasks View ribbon | Sort by (Importance, Due date, Added to My Day, Alphabetically, Creation date) · Group · Show completed | View tab disabled | Add (T-09) | S |

### 6.9 Visual language

| ID | Topic | Outlook | Lookout plan |
|---|---|---|---|
| UX-80 | Icons | Fluent System Icons | Microsoft publishes Fluent System Icons under the **MIT licence** (`microsoft/fluentui-system-icons`). Evaluate bundling the Outlook-meaning glyphs (flag, pin, categorize/tag, snooze, sweep, report, move-to, read/unread envelope) as symbolic SVGs, so actions are recognisable at a glance. Otherwise map each action to the closest Adwaita symbolic icon and keep the mapping in one table. **verify** the current licence before bundling |
| UX-81 | Accent | Outlook blue `#0F6CBD` (Fluent 2 brand) | Offer it as an accent preset ("Outlook blue") next to System accent. Don't make it the default (GNOME accent wins, §6.1) |
| UX-82 | Typography | Segoe UI (proprietary) | System UI font. Don't bundle Segoe. Match sizes and weights: subject 14–16 px semibold in the reading pane, list sender 14 px, preview 12–13 px |
| UX-83 | Density | Full / Medium / Compact | Map `layout-vertical-spacing` loose/medium/tight to Outlook's names in the UI (M-06) |
| UX-84 | Unread styling | Bold sender, accent-coloured bold subject, left accent bar | Partly ✅ (bar, bold). Add accent subject (UX-28) |
| UX-85 | Category colours | 25 presets: Red, Orange, Brown, Yellow, Green, Teal, Olive, Blue, Purple, Cranberry, Steel, DarkSteel, Gray, DarkGray, Black, then Dark variants of Red…Cranberry (Graph `preset0`–`preset24`) | Use exactly this palette and these names in the category editor (2.1), so the colours match an Outlook user's existing categories |
| UX-86 | Dark mode | Dark theme with optional "dark message background" toggle per message (sun/moon button in the reading pane) | Dark message theme setting ✅. Add the per-message toggle button in the reading-pane header |

### 6.10 Accessibility and input

| ID | Outlook behaviour | Lookout plan |
|---|---|---|
| UX-90 | Every icon-only control has a name and tooltip with its shortcut | Audit icon-only buttons for an accessible label and a "Name (Shortcut)" tooltip, generated from the `shortcuts.rs` registry so remaps stay in sync |
| UX-91 | F6 pane cycling, Alt key tips on the ribbon (Alt → H → R = Reply) | F6 (UX-19). Key tips optional (GTK mnemonics give a subset) |
| UX-92 | Full keyboard operation of list, calendar grid and pickers | Arrow-key navigation in the calendar grid (move selection by slot/day; Enter = quick create, UX-55) |
| UX-93 | High contrast | Respect the GNOME high-contrast setting in the custom Cairo calendar painting (`paint_time_grid` already takes the foreground colour; check contrast ratios) |

### 6.11 UX acceptance checklist (every user-facing item)

A roadmap item is done only when all of these hold, alongside its functional tests:

1. **Placement:** reachable from the same surfaces as in Outlook (ribbon tab and group, context menu, hover action, keyboard), per §6.3–6.8.
2. **Wording:** labels, tooltips, toasts and info bars use §6.2 terms and Outlook's phrasing.
3. **Defaults:** default values match Outlook's (e.g. reading pane right, conversations on, mark read on selection, 30-minute calendar slots).
4. **Shortcut:** bound in the Outlook keymap preset (§4.7), and in Lookout defaults if there's no conflict.
5. **Undo:** reversible actions show an Undo toast.
6. **Empty/error states:** an Outlook-style message (e.g. "Nothing in folder", "Select an item to read") rather than a blank pane.
7. **Accessibility:** UX-90 holds for any new controls.
8. **Screenshot:** if the feature is visible in `assets/screenshots/`, refresh the screenshot.

---

## 7. Phased roadmap

Each item lists the IDs it covers, the approach, what it touches, effort and dependencies. Phases are ordered by dependency and impact. Items inside a phase are independent unless stated. User-facing items must also pass the §6.11 UX checklist; **Track U** below covers the shell-level UX work that doesn't belong to a single feature.

### Phase 0: Foundations (no visible features; unblocks the rest)

| # | Item | Approach | Touches | Effort |
|---|---|---|---|---|
| 0.1 | ✅ ~~Outlook interop module~~ | `core/src/interop/{headers,cdo,categories,flags}.rs`: importance/sensitivity parse and emit, busy-status map, category colour presets (`preset0–24` ↔ RGB ↔ Lookout tag palette) | `core` | S |
| 0.2 | ✅ ~~Header-aware envelope~~ | Fetch `BODY.PEEK[HEADER.FIELDS (IMPORTANCE X-PRIORITY X-MSMAIL-PRIORITY PRIORITY SENSITIVITY LIST-ID AUTHENTICATION-RESULTS)]` with the envelope; add `importance`, `sensitivity` to `EmailSummary`; bump the `messages` `user_version` | `mail/src/session.rs`, `cache.rs`, `core/src/email.rs` | M |
| 0.3 | ✅ ~~Outbox and scheduler~~ | New cache table `outbox(id, account_id, not_before, attempts, payload BLOB, state)`; `SendMessage` enqueues; the session drains due rows on its loop tick and on connect; new events `OutboxQueued/Sent/Failed`; drafts stay in Drafts until sent | `mail/src/session.rs`, `cache.rs`, `send.rs` | M |
| 0.4 | ✅ ~~Undo stack~~ | `UndoEntry { description, inverse: Vec<AccountCommand> }` in `UiState`; toast with Undo; Ctrl+Z | `window.rs` | S |
| 0.5 | ✅ ~~Context-menu framework~~ | Generalise the `folder_menu.rs` pattern into a reusable builder (action groups plus `gio::Menu` models) for messages, events, contacts, tasks | `app/src/context_menu.rs` (new) | S |
| 0.6 | ✅ ~~Calendar preferences~~ | GSettings keys `calendar-week-start`, `calendar-work-days`, `calendar-work-start`, `calendar-work-end`, `calendar-primary-tz`, `calendar-secondary-tz`, `calendar-default-reminder`; replace `BUSINESS_HOURS_START/END`, `WORK_WEEK_DAYS`, `WEEK_DAYS`/`first_grid_day` constants | gschema, `settings.rs`, `calendar_view.rs`, `config_view.rs` | M |
| 0.7 | ✅ ~~`lookout-graph` crate skeleton~~ | `reqwest` client, token supplier from `microsoft_oauth.rs` (`TokenAudience::Graph`), paging (`@odata.nextLink`), delta queries (`/delta`), throttling (`Retry-After`), error model; per-feature scope negotiation | `source/crates/graph` (new), `microsoft_oauth.rs` | M |
| 0.8 | ✅ ~~Fixture corpus~~ | Capture real Outlook artefacts: invite, update, cancel, counter, TNEF, `.msg`, importance/sensitivity mail, Teams invite | `source/test-fixtures/outlook/` | S |

### Phase 1: Mail daily-driver parity (mostly S/M; highest value per hour)

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 1.1 | ✅ ~~Spell check~~ | C-02 | Enable WebKit spellcheck on the compose view; language from locale plus a setting; F7 shortcut | S |
| 1.2 | ✅ ~~Importance in list and compose~~ | M-12, C-05 | Uses 0.1/0.2; `!`/`↓` list glyph; compose toolbar toggle; write the three headers in `send.rs` | S |
| 1.3 | ✅ ~~Fix Calendar print button~~ | CAL-23 | Make it sensitive (`window.rs:4400`) | S |
| 1.4 | ✅ ~~Reading pane bottom~~ | M-05 | `layout-reading-pane` → enum `right\|bottom\|off`; swap `gtk::Paned` orientation | M |
| 1.5 | ✅ ~~Preview lines and sender images~~ | M-06 | Setting 0–3 lines; avatar (initials, or vCard photo) column | S |
| 1.6 | ✅ ~~More sort keys and filters~~ | M-07, M-08 | Extend `SortKey` (Size, Importance, Flag, Category, To, Attachments) and the filter set; group headers per key | M |
| 1.7 | ✅ ~~Conversation options~~ | M-04 | Newest-on-top/bottom setting; stacked conversation rendering in the reading pane (one WebKit view with collapsible messages) | L |
| 1.8 | ✅ ~~Mark-as-read options~~ | M-22 | Setting: on open, after N seconds, on selection change, never | S |
| 1.9 | ✅ ~~Message in new window~~ | M-31 | Reuse the reading-pane builder in an `adw::Window`; double-click and Enter open it | M |
| 1.10 | ✅ ~~View message details / source~~ | M-30 | Dialog with parsed headers plus raw source (via `FetchRawMessage`) | S |
| 1.11 | ✅ ~~Forward with attachments; forward as attachment~~ | C-13 | Fetch original parts (`FetchAttachment`) into the composer, resolving inline `cid:` images; `message/rfc822` part | M |
| 1.12 | ✅ ~~Reopen drafts~~ | C-12 | Open a Drafts message → parse MIME → composer prefill (identity, recipients, HTML, attachments); replace on save | M |
| 1.13 | ✅ ~~Undo send / undo actions~~ | M-21, C-09 | 0.3 delay (setting 0/5/10 s) plus 0.4 for move/delete/flag/archive | M |
| 1.14 | ✅ ~~Schedule send~~ | C-08 | Send menu → "Schedule send" date-time; 0.3 `not_before`; banner listing scheduled items; Graph deferred send on Microsoft 365 later (5.x) | M |
| 1.15 | ✅ ~~Snooze presets and sync~~ | M-11 | Preset menu plus custom; Scheduled folder plus keyword (§4.2) | M |
| 1.16 | ✅ ~~Follow-up flags~~ | M-10, T-03 | Message-level flag state (keyword `$Lookout-flag-due-<date>` / `$Lookout-flag-done`; Graph `followupFlag` on Microsoft 365); presets; due column; tasks view shows flagged mail; Pin stays separate | L |
| 1.17 | ✅ ~~Message context menu~~ | M-38 | 0.5; full Outlook item menu | M |
| 1.18 | Outlook keyboard preset | UI-04 | §4.7 table as a second defaults set; picker in Settings; new actions (move to folder, next/previous, mark unread, new event/task/contact, F9) | M |
| 1.19 | ✅ ~~Attachment reminder~~ | C-16 | Regex over body (localisable word list) on send → dialog | S |
| 1.20 | ✅ ~~Emoji, file drop, zoom~~ | C-14, C-15, M-37 | `gtk::EmojiChooser`; drop target on composer; WebKit zoom level plus Ctrl+±/scroll | S |
| 1.21 | ✅ ~~TNEF decode~~ | M-45 | §4.6 in `core/src/interop/tnef.rs`, hooked into the body assembly (`mail/src/body.rs`) and the on-demand `FetchAttachment` handler (`mail/src/session.rs`); synthetic, generator-pinned fixtures | M |
| 1.22 | Open `.msg` | M-29 | `cfb` plus a property mapper → `EmailBody` for the viewer; drag-in to APPEND | M |
| 1.23 | ✅ ~~Save all / zip~~ | M-33 | Folder chooser plus `zip` crate | S |
| 1.24 | ✅ ~~Junk keywords and Not Junk~~ | M-17 | `$Junk`/`$NotJunk`; "Not junk" in the Junk folder, also under the context menu's Report ▸ (`message_menu.rs`) | S |

### Phase 2: Organise and search

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 2.1 | ✅ ~~Category master list~~ | M-09, CAL-07, T-08 | Editor (name, preset colour, shortcut key Ctrl+F2…F12 *classic*); stored in `settings.json`; synced with Graph `masterCategories` on Microsoft 365; shared by mail, events and tasks | M |
| 2.2 | ✅ ~~Search query language~~ | M-18 | Parser in `mail/src/search_query.rs`: `from: to: cc: subject: body: has:attachment is:unread is:read is:flagged is:pinned category: received:>=2026-01-01 received:lastweek size:>1MB folder:`; compile to FTS5 `MATCH` plus SQL predicates locally and IMAP `SEARCH` keys remotely; keep `sanitize_fts_query` for free text | M |
| 2.3 | ✅ ~~Search UI: refiners, scope, history~~ | M-18 | Scope dropdown (current folder / subfolders / account / all); refiner chips; recent searches in `ui_state_db`; advanced-search form; contextual Search ribbon row | M |
| 2.4 | ✅ ~~Search Folders~~ | M-19 | `AppConfig::SearchFolder` (name + query) seeded once from `default_search_folders` (Unread mail, Flagged, Large mail, Has attachments); the folder tree renders a **Search Folders** group (`TreeItem::SearchFolders`/`SearchFolder`, in the pane signature) and selecting a row runs its query across all mailboxes; a Config → Mail manager adds/removes them and repaints the pane via `UiState::folder_tree_refresh` | M |
| 2.5 | ✅ ~~Rules engine~~ | M-14 | One model, `Rule { conditions: [From/To/Subject/Body/Header/HasAttachment/Importance/Size/SentOnlyToMe/List-Id], exceptions, actions: [Move/Copy/Delete/Flag/Categorise/MarkRead/Forward/Redirect/StopProcessing] }`, executed by three backends: (a) **client**, run on `NewMessages` in the session; (b) **Sieve** via ManageSieve (§4.5) when the server advertises it; (c) **Graph** `messageRules`. Rules UI shows where each rule runs; "Run rules now" on a folder; "Create rule…" from a message (and the context menu's Rules ▸, `message_menu.rs`). | L |
| 2.6 | ✅ ~~Quick Steps~~ | M-15 | Named action chains (reuses the rules engine's actions) in the Mail ribbon's **Quick steps** menu and the `Ctrl+Shift+1`…`9` slots, managed from Config → Mail → "Quick Steps…"; one Undo entry reverses a whole step | M |
| 2.7 | ✅ ~~Sweep~~ | M-16 | Mail Home ribbon **Sweep** → dialog for the selected sender: all / all except the latest / older than N days, move to Deleted Items or Archive, optional standing rule; matches gathered from the local cache by `search_query("from:…")` scoped to the folder | M |
| 2.8 | ✅ ~~Ignore / Clean up conversation~~ | M-20 | Ignore: an `ignored_threads` table + `UiState` mirror, `rules::enforce_ignored_threads` moves new arrivals to Deleted on receipt, **Ignore** / **Stop ignoring** in the message menu, and a manage dialog; Clean up: `wholly_quoted` deletes the conversation's older fully-quoted messages (conservative, Undo-able) | M |
| 2.9 | ✅ ~~Safe / blocked senders~~ | M-17 | Lists in `ui_state_db` (a `blocked_senders` table; the S-01 trusted senders are the safe list); the message menu's Block ▸ (Block sender / Block domain / Unblock sender) moves mail to Junk with `$Junk`; new arrivals from a blocked sender are junked by `rules::enforce_blocked_senders`, independent of the rules switch; Settings → Mail → Junk email manages both lists | S |
| 2.10 | ✅ ~~My Templates~~ | C-17 | Stored HTML snippets (`AppConfig.templates`, `lookout_core::MailTemplate`); managed in Settings → Mail and inserted from the composer's **My Templates** menu | S |
| 2.11 | ✅ ~~Automatic replies~~ | M-24 | Per-account Settings dialog (on/off, optional time period, messages): Microsoft 365 through Graph `automaticRepliesSetting` (internal/external + audience), other servers through an RFC 5230 `vacation` block merged into the account's Sieve script (capability-probed); "Automatic replies are on" banner in Mail with Turn off | M |
| 2.12 | Focused Inbox | M-03 | Microsoft 365: read `inferenceClassification`, "Move to Focused/Other" writes overrides. Elsewhere, optional local classifier (sender seen in Sent/contacts = Focused; bulk/List-Id = Other). Tabs above the Inbox list | L |
| 2.13 | ✅ ~~@mentions~~ | M-25 | Detect the user's address in body `mailto:` anchors / `@Name`; list glyph plus filter | S |

### Phase 3: Calendar and meetings

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 3.1 | ✅ ~~Apply calendar preferences~~ | CAL-08, CAL-09, CAL-10 | 0.6 everywhere: grid start day, business-hours band, work-week days, second TZ gutter | M |
| 3.2 | ✅ ~~Per-event time zone~~ | CAL-11 | TZ dropdown in `event_editor.rs`; `build_vevent` emits `TZID` plus `VTIMEZONE` (IANA); keep reading Windows names | M |
| 3.3 | ✅ ~~Full show-as~~ | CAL-05 | Dropdown Free/Working elsewhere/Tentative/Busy/Away; §4.3 CDO properties; hatched/striped rendering per state as Outlook does | S |
| 3.4 | ✅ ~~Organizer invitations~~ | CAL-14 | Detect RFC 6638 server scheduling (`schedule-outbox-URL`, `SCHEDULE-AGENT`); else on save send iMIP REQUEST to attendees (`build_imip_vcalendar` plus `SendMessage`); "Send update" on change (`SEQUENCE++`); CANCEL on delete; "Send / Don't send" prompt as Outlook does | L |
| 3.5 | ✅ ~~RSVP options~~ | CAL-15 | The invite card's split buttons (Send response now / Edit response before sending / Don't send); "Edit" opens the full composer with the `METHOD:REPLY` calendar part, applies the local answer on Send, and a comment rides in the text body; `respond_to_imip_invitation` gained the send/comment options | S |
| 3.6 | ✅ ~~Response tracking~~ | CAL-16 | An opened REPLY merges its `PARTSTAT`s onto the stored organizer event and PUTs it (skipped when the server schedules; deduped per message); the editor's Tracking view lists each attendee's status, roles and response counts | M |
| 3.7 | ✅ ~~Propose new time / forward~~ | CAL-17, CAL-18 | COUNTER and DECLINECOUNTER builders plus organizer accept-proposal flow; forward = REQUEST to new attendee plus notify organizer; honours `X-MICROSOFT-DISALLOW-COUNTER` | M |
| 3.8 | 🟡 ~~Scheduling Assistant~~ | CAL-19 | Editor tab with a reusable attendee-rows × time grid (`app/src/schedule_grid.rs`); free/busy via RFC 6638 `VFREEBUSY` POST to the schedule-outbox (`dav/src/freebusy.rs`, session `FetchFreeBusy`), the organizer row from the cache, and "Suggested times" (pure scan). Graph `getSchedule` and `places` rooms land with 5.1 | L |
| 3.9 | Reminders window | CAL-12, T-05 | `adw::Window` listing due reminders (events and tasks): Dismiss / Dismiss all / Snooze [5 min…1 week] / Open; multiple `VALARM`s; default reminder; catch-up on launch for missed reminders | M |
| 3.10 | Advanced recurrence builder | CAL-13 | Monthly "day N" / "the Nth weekday"; yearly "month day" / "Nth weekday of month"; BYSETPOS; end by/after; preview string | M |
| 3.11 | ✅ ~~List and Schedule views~~ | CAL-02, CAL-03 | Standalone **List** view (reuses `build_agenda_view` as the `"list"` stack page); **Schedule** view (calendars as rows, horizontal time) in `calendar_view.rs`, both wired to the calendar view switcher | L |
| 3.12 | ✅ ~~Event context menu, copy, duplicate~~ | CAL-30, CAL-31 | 0.5; the event menu (`app/src/event_menu.rs` + `connect_event_context_menu`) with Open/Delete/Duplicate/Copy to calendar…/Save as .ics…/Categorize/Show as/Private; Ctrl-drag copies; Duplicate and Copy create then open the editor | S |
| 3.13 | Calendar colour picker and groups | CAL-32, CAL-33 | `gtk::ColorDialog`; PROPPATCH `calendar-color` when writable; user groups persisted in `settings.json` | M |
| 3.14 | Event search | CAL-22 | Search over the `dav` cache (title, location, notes, attendees); results list; global search scope "Calendar" | M |
| 3.15 | ✅ ~~Holidays~~ | CAL-28 | Country picker → subscribe to a bundled public `.ics` URL via CAL-26 | S |
| 3.16 | Event attachments | CAL-21 | RFC 8607 managed attachments (`ATTACH;MANAGED-ID`) where supported; inline `ATTACH;VALUE=BINARY` fallback for iMIP; Graph attachments | M |
| 3.17 | 🟡 ~~Online meetings~~ | CAL-20 | Detect Teams/Zoom/Meet URLs → "Join" button in card, reminder and peek (`interop/onlinemeeting` + parser fills `conference_url`); Microsoft 365 "Teams meeting" toggle → Graph `isOnlineMeeting` lands with the Graph calendar backend (5.1) | M |
| 3.18 | ✅ ~~Conflict indicator on invites~~ | CAL-35 | Overlap check against the cache in `render_invite_card` | S |
| 3.19 | ✅ ~~Reply with meeting~~ | C-20 | Context action: new event with thread participants plus subject | S |
| 3.20 | Print styles | CAL-23 | Day / Week / Month grid / Agenda HTML templates through the existing WebKit print path | M |

### Phase 4: People and tasks

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 4.1 | ✅ ~~Full contact field editing~~ | P-02, P-03 | Editor rows for anniversary, IM (`IMPP`), URL, custom; photo view and set (`PHOTO`) | M |
| 4.2 | ✅ ~~Profile card~~ | P-10 | `app/src/profile_card.rs`: one reusable popover (`attach_person_trigger` + `show_person_card_at`), opened on hover/click of the reading-pane header sender, a message-list row's sender/avatar, a conversation-WebView card's sender, and an event-editor attendee chip (`RecipientEntry::set_person_card_hook`). Shows photo/initials, name, title·org, phones, address, recent mail (`Cache::search_query` by `from:`), upcoming shared events (`shared_upcoming_events` over the in-memory calendar), and Send email / Schedule meeting / Add-or-Edit contact. `EventEditorPrefill.suggested_attendees` lands "Schedule meeting" | M |
| 4.3 | ✅ ~~Contact lists in compose~~ | P-05, C-18 | `KIND:group`/`X-ADDRESSBOOKSERVER-MEMBER` parsed (`core/vcard.rs`); snapshot resolves members (`contacts_view.rs`); `Suggestion::List` offered in autocomplete and the picker; a list inserts one chip with a `+` expander (compose switched to chip mode, UX-42) | S |
| 4.4 | ✅ ~~People search and sort~~ | P-06, P-07 | Search box in the People pane (name/email/org) plus a module-aware title-bar search; a global "File contacts as" preference and a per-contact `file_as` override (`X-MS-FILEAS`/`SORT-AS`) | S |
| 4.5 | ✅ ~~Outlook CSV import/export~~ | P-08 | Column map per §4.6; tolerant header matching | M |
| 4.6 | Merge duplicates | P-09 | Match by email/phone/name; merge UI with field-level choice | M |
| 4.7 | Directory lookup | P-11 | Graph `/users` (Microsoft 365) and LDAP (`ldap3`, rustls, configured per account) as extra directory providers in autocomplete, the recipient picker and People search. Ten substeps in **4.7 Directory lookup — substeps** below | M–L |
| 4.8 | ✅ ~~Task lists sidebar~~ | T-01 | Navigation pane in `tasks_view.rs`: smart lists (My Day, Flagged email, Tasks) then one row per source, with the row filtering the list. Create/rename/delete lists route to the owning backend — CalDAV MKCALENDAR (VTODO-only) / PROPPATCH / DELETE (`dav`), Google Tasks `tasklists` CRUD, Graph `/me/todo/lists` CRUD (well-known lists guarded), local lists in `settings.json` (`app_config`, `task_lists.rs`) — degrading to a toast where unsupported | M |
| 4.9 | ✅ ~~My Day~~ | T-02 | Local per-day pin set (`ui_state_db`'s `my_day_tasks`); the **My Day** smart list shows the pins plus due-today/overdue **Suggestions**; a task row's right-click toggles membership (`my_day.rs`, pure selection logic) | S |
| 4.10 | Task reminders and recurrence | T-05, T-06 | `VALARM`/`RRULE` in `build_vtodo`; roll forward on complete; reminders via 3.9 | M |
| 4.11 | Steps / subtasks | T-07 | `RELATED-TO;RELTYPE=PARENT` children (CalDAV); Google Tasks `parent`; Graph `checklistItems` | M |
| 4.12 | ✅ ~~Task sort/group and categories~~ | T-08, T-09 | "Sort and group" toolbar menu (due/importance/title/created; group by due/list/category/none; show completed) persisted in GSettings; `CalendarTask.created` (RFC 5545 `CREATED`); category chips from the 2.1 master list | S |

#### 4.7 Directory lookup — substeps

Graph `/users` (Microsoft 365) and LDAP (`ldap3`, rustls, configured per account) join autocomplete, the recipient picker and People search as directory providers feeding `merge_contact_suggestions`. An unreachable or unauthorised directory degrades to no hits, never an error (§10).

| # | Substep | Effort |
|---|---|---|
| 4.7.1 | ✅ ~~Add a `directory: &[DirectoryPerson]` argument to `merge_contact_suggestions`, merged after CardDAV contacts, deduped and `limit`-bounded (pure, unit-tested).~~ Landed as a `Suggestion::Directory` variant so the picker and People search can render read-only rows (4.7.7/4.7.8); the three call sites pass `&[]` until 4.7.6. | S |
| 4.7.2 | ✅ ~~New `app/src/directory.rs`: one shared async `DirectorySource` dispatching per account over the Graph and LDAP backends, with keystroke debounce (~250 ms), a 2–3 character minimum and a small prefix→results cache (autocomplete fires on every keystroke).~~ `DirectoryBackend` trait (returns hits, never an error) plus a `Clone`-able `DirectorySource` (`search` takes a multi-shot callback, ready for 4.7.6); the Graph/LDAP backends are 4.7.3/4.7.5, so the module is `#[allow(dead_code)]` until it is wired. | M |
| 4.7.3 | ✅ ~~**Graph provider.** Build `ContactsClient` from the account's `MicrosoftOAuth`, run `ensure_directory_consent` on first use, search `search_directory` (`/users?$search=`) with `search_people` (`/me/people`) as fallback. Gated on `graph_consented()` plus a new per-account opt-in toggle, since `User.ReadBasic.All`/`People.Read` sit outside the default "Sync with Outlook" consent.~~ Landed as `app/src/graph_directory.rs`'s `GraphDirectoryBackend` (impl `DirectoryBackend`): `/users?$search=` first, `/me/people` when it errors or finds nobody, every failure degrading to no hits; gated by `AppConfig::directory_accounts` plus `graph_consented()` (`enabled(&AppConfig)`), and `ensure_consent()` runs the interactive directory-scope flow only from the opt-in action (4.7.9) - a search never launches a browser. `#[allow(dead_code)]` until 4.7.6 wires it in. | M |
| 4.7.4 | ✅ ~~**LDAP config model.** `LdapDirectory { id, account_id, display_name, host, port, tls, bind_dn, base_dn, filter, mail_attr, name_attr, enabled }` in `settings.json` (`AppConfig`, `#[serde(default)]`), with the bind password in the keyring under a new `PROTOCOL_LDAP` slot.~~ `AppConfig::ldap_directories` (serde-defaulted) with `ldap:<uuid>` ids, `upsert`/`remove` helpers and the optional attributes serde-defaulted for hand-written entries; the bind password keys the new `other_accounts::PROTOCOL_LDAP` slot by the directory id. | S |
| 4.7.5 | **LDAP backend.** `ldap3` (`default-features = false, features = ["tls-rustls-aws-lc-rs"]`, matching Lookout's existing rustls/aws-lc-rs stack; async `LdapConnAsync`) behind a `DirectorySearch` trait, with a pure filter builder/escaping and an entry→`EmailAddress` mapping so tests use a fake (no live server). | M |
| 4.7.6 | Wire both sources into the composer (`compose_host.rs`) and the calendar attendee field (`contacts_view.rs`) — account-scoped for compose, unioned across accounts for attendees. | S |
| 4.7.7 | Add a **Directory** section to the recipient picker (`contact_picker.rs`), queried once the search box has text, with read-only rows. | S |
| 4.7.8 | Add a transient **Directory** group to the People list while the filter is active, with Send email / Add to contacts row actions. | M |
| 4.7.9 | **Settings → People UI.** A per-Microsoft-365 "Search the company directory (GAL)" consent switch, plus add/edit/remove LDAP directory entries (dialog modelled on `show_add_account_dialog`). | M |
| 4.7.10 | **Close-out.** CHANGELOG entry (P-11), flip P-11 to ✅ with the §2.1/§7 rows, `cargo test`/`clippy` clean. | S |

**Order:** 4.7.1 → 4.7.2 → (4.7.3 ∥ 4.7.4 → 4.7.5) → 4.7.6 → 4.7.7 → 4.7.8 → 4.7.9 → 4.7.10. (4.7.1–4.7.4 landed.)

### Phase 5: Microsoft 365 native (Graph)

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 5.1 | ✅ ~~Graph calendar backend~~ | A-03, CAL-14…19 | `lookout_graph::calendar::{client,session}` implements the same command/event contract as `lookout-dav`: `calendarView` month sync, event create/update/delete, and the RRULE/Teams/RSVP/free-busy/rooms mappings; Microsoft 365 accounts connect to it and Graph schedules iTIP itself. Follow-ups: the editor's room suggestions, the Graph COUNTER write, and the editor's Teams toggle | L |
| 5.2 | ✅ ~~Graph contacts backend~~ | A-04, P-11 | `lookout-graph::contacts`: contact folders, per-folder `/delta`, create/update/delete (`If-Match`), `/photo/$value`, and the GAL (`/me/people`, `/users?$search=`). The People screen's poll loop syncs M365 accounts through the Graph backend (consent-gated like the calendar), and a fallback synthesises the account when GOA reports no `Contacts` interface | M |
| 5.3 | ✅ ~~Microsoft To Do backend~~ | A-05, T-12 | `lookout_graph::todo` (`TodoClient` + pure mapper + `run_todo_session`): `/me/todo/lists` and per-list `/tasks` mapped into `CalendarTask` and synthetic `todo:<list id>` ids, CRUD guarded by `If-Match`; the app routes saves/deletes, offers the lists in the editor picker and merges them into the Tasks view; `Tasks.ReadWrite` joins the combined consent. Checklist items map to T-07 (4.11); the flagged-email list is already served by M-10's per-message `followupFlag` sync | M |
| 5.4 | ✅ ~~Mail property sync~~ | A-06, M-09, M-10, M-12, M-03 | Per-message Graph lookup by `internetMessageId` with the Message-ID ↔ Graph-id map cached in the mail cache; the `/me/messages/delta` feed syncs categories, followupFlag, importance and inferenceClassification both ways, with the categories/importance/Focused-Other overlays stored locally where Exchange IMAP can't keep keywords. Shape: `lookout_graph::mail` transport, cache overlays + delta apply, session commands/events, app mirrors, and the Focused/Other tabs + Move to / Always / Stop + Importance menu actions | L |
| 5.5 | ✅ ~~Server-side schedule send~~ | C-08 | `PidTagDeferredSendTime` (`SystemTime 0x3FEF`) on a Graph `sendMail`, so Exchange holds the message in Drafts and sends it while Lookout is closed; the composer uses it for a scheduled send on a Microsoft 365 account with Graph access, falling back to the local outbox otherwise | S |
| 5.6 | 🟡 ~~Shared mailboxes and delegate calendars~~ | M-42, CAL-24, CAL-25 | Delegate calendars landed: `lookout_graph` gains owner-aware calendar paths (`/users/{owner}/calendars`), `users::resolve_user`, and `calendarPermissions` (list/create/update/delete) plus the `Calendars.ReadWrite.Shared` consent; `CalendarCommand::SetSharedCalendars` makes the Graph actor merge another user's calendars in, the Add-calendar dialog offers **Open another user's calendar**, and the Home ribbon's **Share** opens a permissions dialog (CAL-24, CAL-25). Shared **mailboxes** (M-42) are now addable by address from Config → Accounts: `connect_shared_mailboxes` connects each as its own account reusing the parent Microsoft 365 account's OAuth token, with the Graph folder/send-as transport ready; the exact Exchange Online IMAP login form still needs verification on a live tenant | L |
| 5.7 | Optional Graph mail transport | A-06 | Only if 5.4 proves too costly: full Graph mail sync as an alternative to IMAP for Microsoft 365 accounts | L |

### Phase 6: Security and compliance

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 6.1 | ✅ ~~Authentication-Results banners~~ | S-03, M-35 | Parse RFC 8601 into `EmailBody.auth_results`; banners for fail/none and for display-name ↔ address mismatches | S |
| 6.2 | ✅ ~~External-sender tag~~ | S-06 | Global internal domains (`mail-internal-domains`); "External" info bar | S |
| 6.3 | ✅ ~~Link safety~~ | S-08 | Hover status strip with the real URL; confirm before opening when text and href domains differ | S |
| 6.4 | S/MIME and OpenPGP | S-04, S-05, C-21 | GPGME (`gpgme` crate). `gpgsm` covers S/MIME and `gpg` covers OpenPGP, one dependency and key storage in the user's existing keyrings. Verify/decrypt in `mail/src/body.rs`; sign/encrypt in `send.rs`; status bar in the reading pane; per-message toggles in compose; certificate import in Settings | L |
| 6.5 | ✅ ~~Sensitivity~~ | C-06 | Compose option plus banner (§4.1) | S |
| 6.6 | DSN | M-27, C-07 | SMTP `NOTIFY`/`RET`/`ENVID` (**verify** `lettre` support, else extend `send.rs`); DSN report card | M |
| 6.7 | ✅ ~~Report phishing~~ | S-09 | Forward-as-attachment to a configurable address, then move to Junk | S |

### Phase 7: Migration and extras

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 7.1 | PST import | X-02 | Wizard: pick `.pst` → map folders → IMAP APPEND into the chosen account (mail), CalDAV/CardDAV PUT (calendar/contacts), task backend (tasks); progress and resume | L |
| 7.2 | Save as `.msg` / PDF | M-29 | `.msg` writer (CFB); PDF via the print-to-file path | M |
| 7.3 | Attachment preview | M-32, X-03 | Preview pane: images (`gtk::Picture`), PDF (Poppler/`papers` rendering), text/code; Office via optional `libreoffice --convert-to pdf` | M |
| 7.4 | Translate | M-36 | Provider setting (LibreTranslate URL, or the assistant endpoint); "Translate message" banner on language mismatch | M |
| 7.5 | Immersive Reader | M-37 | Reader stylesheet (font, spacing, width) injected into the WebKit view; read-aloud via Speech Dispatcher | S |
| 7.6 | Copilot-style assistant actions | X-05 | Tools/actions: summarise thread (reading pane button), draft reply (to composer, never auto-send), rewrite/shorten selection, extract tasks/events (confirm before writing); reuse `assistant.rs` `chat_with_tools` | M |
| 7.7 | Mail merge | C-23 | Template plus CSV/contact list → individual drafts or scheduled sends via the outbox | M |
| 7.8 | Notes | X-01 | *classic-only*; optional sticky notes, stored as IMAP `Notes` folder messages (Apple Notes convention) | M |

### Track U: UI/UX alignment (runs alongside Phases 1–4)

Shell-level work from §6 that isn't owned by a single feature. Feature-specific UX rows (e.g. UX-35 stacked conversation, UX-59 event form) ship with their feature's roadmap item.

| # | Item | UX IDs | Approach | Effort | Lands with |
|---|---|---|---|---|---|
| U.1 | Shared ribbon component | UX-15, UX-16, UX-21, UX-50, UX-66 | One ribbon builder in a new `app/src/ribbon.rs`: groups, icon+label buttons, split buttons, adaptive collapse (label → icon → overflow) using `adw::Breakpoint`, optional Classic layout and collapse toggle. Migrate the Mail, Calendar, People and Tasks command bars to it; reorder Mail Home to UX-21 | M | Phase 1 |
| U.2 | Terminology pass | UX-01…UX-10 | Rename user-visible strings (tray, tooltips, menus, settings) and add well-known folder display names | S | Phase 1 |
| U.3 | Title bar and shell | UX-11, UX-13, UX-14, UX-19, UX-25 | Centred search with scope dropdown; Settings gear to the title bar; rail tooltips and selected state; F6 pane cycling; folder-pane collapse | M | Phase 1 (scope and advanced search finish in 2.3) |
| U.4 | Message list redesign | UX-26…UX-31, UX-39, UX-84 | Three-line row; status icons and category chips; merged Filter/Sort menu; single-line columnar layout with configurable headers; Outlook folder order and counts (UX-23, UX-24); [Draft] marker; hover checkbox | L | Phase 1 (1.4–1.6) |
| U.5 | Reading pane polish | UX-32…UX-34, UX-37, UX-86 | Info-bar stack component; ⋯ menu contents; attachment tiles; double-click/Enter opens a window; per-message dark toggle; "After moving or deleting" setting | M | Phase 1 |
| U.6 | Compose layout | UX-40…UX-45 | Compose contextual ribbon tabs on U.1; chip-mode recipients in compose; attachment tiles and drag-in; icon audit | M | Phase 1 (chip-mode recipients landed with 4.3) |
| U.7 | Calendar interaction model | UX-51…UX-58 | Today/arrows on the left plus title picker; coloured calendar checkboxes; now-line; quick-create popover; event peek popover; chip styling; month "+N more" popover; Calendar View tab | M | Phase 3 |
| U.8 | My Day pane | UX-18 | Tabbed Calendar/To Do pane with drop targets | M | Phase 4 |
| U.9 | People and Tasks layouts | UX-65, UX-67, UX-71…UX-74 | People details pane with letter headers; Tasks smart lists, quick-add row and details pane | M | Phase 4 |
| U.10 | Settings regroup | UX-20 | Reorganise `config_view.rs` pages into Outlook's categories; add "Customize actions" (choose which actions appear in the ribbon, hover actions and reading-pane bar; stored in `settings.json`) | M | Phase 2 |
| U.11 | Visual language and accessibility | UX-80…UX-83, UX-85, UX-90…UX-93 | Icon decision (Fluent System Icons vs Adwaita mapping table); accent preset; density labels; category palette; accessible names and tooltips generated from `shortcuts.rs`; calendar keyboard navigation; contrast check | M | Phase 1 (icons, a11y), Phase 2 (palette with 2.1) |

---

## 8. Milestones

| Version | Scope | Exit criteria |
|---|---|---|
| v0.10 | Phase 0, Phase 1, Track U.1–U.6 and U.11 (icons, a11y) | Daily-driver mail parity: spell check, importance, undo/schedule send, flags, drafts, forward, Outlook keymap. Mail looks and is laid out like Outlook (ribbon, three-line list, reading pane, compose) |
| v0.11 | Phase 2, U.10 | Rules (client plus Sieve), search syntax, categories master list, automatic replies; Settings organised like Outlook's |
| v0.12 | Phase 3, U.7 | Lookout can organise a meeting end to end with an Outlook attendee: invite, tracking, proposal, update, cancel. The calendar responds to click, double-click and drag the way Outlook does |
| v0.13 | Phase 4, U.8, U.9 | People and Tasks parity for non-Microsoft accounts, including Outlook's layouts |
| v1.0 | Phases 0–4 and Track U complete, plus Phase 6 items 6.1–6.3 | "1:1 Outlook" for IMAP/CalDAV/CardDAV accounts: every §5 row ✅ or ⛔, and every user-facing §6 row met |
| v1.x | Phases 5–7 | Microsoft 365 native, encryption, migration |

**Dependency graph (short):**

- 0.1 → 0.2 → 1.2, 1.6, 2.2
- 0.3 → 1.13, 1.14, 7.7
- 0.4 → 1.13
- 0.5 → 1.17, 2.7, 3.12
- 0.6 → 3.1, 3.2
- 0.7 → 2.1 (Microsoft 365 sync), 2.11 (Microsoft 365), 2.12, 3.8 (Microsoft 365), 4.7, Phase 5
- 2.1 → 2.5, 2.6, 4.12
- 2.5 → 2.6, 2.7, 2.8
- 3.4 → 3.6, 3.7
- 3.9 → 4.10
- U.1 → U.6, U.10 (Customize actions); 0.4 → U.5 info-bar/undo toasts; 1.4 → U.4 columnar layout; 2.1 → U.11 palette; CAL-05 (3.3) → UX-57 show-as styling

---

## 9. Explicit non-goals

- Binary reverse engineering, decompilation or protocol sniffing of Outlook itself (published specs and captured message fixtures are enough).
- COM / VSTO / Office add-ins, VBA macros, Outlook forms.
- MAPI profiles, OST files, Cached Exchange Mode semantics.
- Exchange-internal features over IMAP: message recall, voting, task assignment, Purview labels (⛔ rows).
- Exchange on-premises via EWS/ActiveSync. Revisit only after Phase 5 if there is demand; Graph covers Exchange Online.
- Pixel-level UI cloning, Microsoft branding or the Segoe UI font. Parity means placement, wording, defaults and interaction (§6), presented in GNOME/libadwaita idiom.
- Teams presence and chat.

---

## 10. Cross-cutting rules for implementers

- **Protocol seam first.** New server operations are new `AccountCommand`/`AccountEvent` variants (or DAV/Graph equivalents). The UI never talks to a server directly (SCAFFOLD §4.1).
- **Cache migrations** follow the `PRAGMA user_version` pattern in `mail/src/cache.rs` and `dav/src/cache.rs`. Note any format change in the CHANGELOG ("no data-format change" otherwise).
- **Settings placement:** scalar preferences go in GSettings (the gschema file); lists and structured config go in `settings.json` (`app_config.rs`); per-item UI state goes in `ui_state_db.rs`.
- **Interop strings** (header names, `X-MICROSOFT-*`, Graph property names) live in `core/src/interop/`, never inline in feature code.
- **UX acceptance:** every user-facing change passes the §6.11 checklist. Commands go through the shared ribbon (U.1) and context-menu (0.5) builders, so placement and wording stay consistent across surfaces.
- **Degrade softly:** every server capability (Sieve, RFC 6638, Graph scope, DSN) is detected and optional, mirroring how the IMAP session treats `MOVE`/`QRESYNC`/`COMPRESS`. Missing capability means the client-side fallback or a hidden UI element, never an error.
- **Tests:** unit tests next to each module; new fixtures under `source/test-fixtures/outlook/` for every Outlook artefact (§0.8). Parser work (TNEF, `.msg`, CDO, COUNTER) needs a fixture-driven round-trip test.
- **CHANGELOG:** one entry per shipped item, in the existing style, citing IDs from this document.
- **Keep this file current:** flip status marks and strike completed roadmap rows as features land.

[MS-OXMSG]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxmsg
[MS-OXOCAL]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxocal
[MS-OXCICAL]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxcical
[MS-OXPROPS]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxprops
[MS-OXTNEF]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxtnef
[MS-OXRTFCP]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxrtfcp
[MS-OXRTFEX]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxrtfex
[MS-OXOFLAG]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxoflag
[MS-OXOMSG]: https://learn.microsoft.com/openspecs/exchange_server_protocols/ms-oxomsg
[MS-PST]: https://learn.microsoft.com/openspecs/office_file_formats/ms-pst
