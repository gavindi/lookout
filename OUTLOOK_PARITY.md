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
| Mail (5.1) | 10 | 17 | 17 | 2 | Solid IMAP core (sync, IDLE, FTS search, unified inbox, pin, tags, drag-and-drop). Missing Outlook's organising layer: rules, Focused Inbox, flags with dates, importance, Sweep, Quick Steps, undo. No winmail.dat or `.msg`. |
| Compose (5.2) | 4 | 5 | 13 | 1 | Good rich editor and identities. No spell check, schedule send, importance, emoji, draft reopen, or forwarding with attachments. |
| Calendar (5.3) | 7 | 14 | 14 | 0 | Views and drag-to-edit are good. Meetings as organizer are missing: invitations aren't sent, there is no response tracking and no free/busy. No week-start, working-hours or time-zone settings. |
| People (5.4) | 3 | 6 | 3 | 0 | CRUD and groups are fine. No profile card, CSV, directory, or contact lists in compose. |
| Tasks (5.5) | 3 | 3 | 5 | 1 | VTODO and Google Tasks CRUD. No reminders, recurrence, subtasks or My Day. |
| Accounts / Microsoft 365 (5.6) | 2 | 1 | 5 | 0 | Microsoft 365 is **mail only** (IMAP/SMTP). No Graph calendar, contacts, To Do, rules, auto-reply or categories. |
| Security (5.7) | 2 | 1 | 5 | 1 | Remote-image blocking and a sandbox exist. No S/MIME/PGP, no authentication-results banners. |
| Desktop / extras (5.8) | 4 | 4 | 4 | 0 | Themes, tray and remappable shortcuts are good. No Outlook keymap preset, PST import or Copilot-style actions. |
| **Total** | **35** | **51** | **66** | **5** | ~22% at parity, ~33% partial |

### 2.2 Top 10 gaps (ranked by user impact)

1. **Microsoft 365 calendar, contacts and To Do** (A-03…A-05). Accounts users "add from Outlook" show no calendar.
2. **Sending meeting invitations as organizer** (CAL-14). `build_imip_vcalendar` already supports REQUEST and CANCEL but is only called for REPLY.
3. **Rules** (M-14), both client-side and server-side (Sieve / Graph).
4. **Undo send and undo actions** (M-21, C-09).
5. **Spell check** (C-02). WebKit can do this; it's switched off.
6. **Flags with due dates / follow-up**, and **importance** (M-10, M-12, C-05).
7. **Calendar preferences**: week start, working hours, time zones (CAL-08…CAL-11).
8. **Scheduling Assistant / free/busy** (CAL-19).
9. **Reopen drafts; forward with attachments** (C-12, C-13).
10. **Automatic replies (Out of Office)** (M-24).

### 2.3 UI/UX at a glance

Lookout's shell already follows new Outlook's layout: app rail, File/Home/View/Help ribbon, folder / list / reading panes, a My Day-style right pane, and Outlook's calendar view names. The gaps are in the details an Outlook user notices within minutes (full spec in §6):

- An **icon-only ribbon**, styled differently in each module; View is greyed out outside Mail.
- The subject **squeezed onto the sender line** in the message list, and column headers shown over multi-line rows.
- **Folder order** that's alphabetical rather than Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive.
- **Calendar clicks open the full editor** instead of Outlook's quick-create and peek popovers; Today/arrows sit on the wrong side.
- **Plain-text recipient fields** in compose, where Outlook uses pills.
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
| Local UI state DB | `source/crates/app/src/ui_state_db.rs` | Safe/blocked senders, Quick Steps, templates, reminder dismissals |
| JSON config | `source/crates/app/src/app_config.rs` (`settings.json`) | Rules (client-side), category master list, signature defaults |
| GSettings | `source/data/gschema/io.github.gavindi.Lookout.gschema.xml` | Scalar preferences (week start, reading-pane position, mark-as-read delay) |
| UI state and dispatch | `source/crates/app/src/window.rs` (`UiState`, `CalendarUiState`) | All UI |
| Context menus (pattern) | `source/crates/app/src/folder_menu.rs` | Message, event, contact and task context menus |
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
| Sensitivity | `Sensitivity: Personal\|Private\|Company-Confidential` (RFC 2156) | Same | Compose option and reading-pane banner. |
| Read receipt | `Disposition-Notification-To:` | Same | ✅ already (`send.rs`, RFC 8098). |
| Delivery receipt | SMTP DSN `NOTIFY=SUCCESS,FAILURE` (RFC 3461) | DSN reports (`multipart/report; report-type=delivery-status`) | Needs DSN support in the SMTP layer (**verify** `lettre`); render DSNs as a status card. |
| Follow-up flag (internet) | `X-Message-Flag: Follow up` (classic, to internet recipients); `Reply-By:` (RFC 2156) | Same | Display "Follow up / Reply by" banner. |
| TNEF | `application/ms-tnef` part named `winmail.dat` | — | Decode (§4.6). |
| Auth results | n/a (server adds `Authentication-Results:`, RFC 8601) | Drives the "unverified sender" banner | Fill the existing but unused `EmailBody.auth_results` (`source/crates/core/src/email.rs:334`). |

### 4.2 IMAP keywords: cross-client conventions

Exchange does **not** expose Outlook categories or rich flags over IMAP. On Microsoft 365 those exist only through Graph. On other servers Lookout picks the convention:

| Concept | Convention | Lookout today | Plan |
|---|---|---|---|
| Flagged | `\Flagged` | Used for **Pin** (`graph_pin.rs` mirrors it to Outlook's pin MAPI property on Microsoft 365) | Keep. Flags with due dates are separate (M-10). |
| Categories | No standard. Thunderbird uses `$label1…$label5`; Lookout uses `$Lookout-tag-<key>` (`tags.rs`) | 🟡 Lookout-only | Keep the keyword store. On Microsoft 365 also sync to Graph `categories` (§4.4). Optionally read Thunderbird `$label*`. |
| Junk | `$Junk` / `$NotJunk` (RFC 5788 registry) | Not set | Set on Report Junk / Not Junk so server filters learn. |
| Forwarded / answered | `$Forwarded`, `\Answered` | **verify** whether set on send | Set both; show the reply/forward indicator icons in the list. |
| Snooze | No standard. New Outlook moves the item to a server **"Scheduled"** folder and keeps the wake time in a MAPI property | Local-only (`cache.rs` `snoozed` table) | Move to a "Scheduled"-role folder plus `$Lookout-snooze-<unix>` keyword, so any Lookout install restores it (M-11). Can't interoperate with Outlook's own snooze over IMAP (⛔). |

### 4.3 iCalendar / iTIP

| Concept | Outlook encoding | Lookout plan |
|---|---|---|
| Show-as (busy status) | `TRANSP` plus `X-MICROSOFT-CDO-BUSYSTATUS: FREE\|TENTATIVE\|BUSY\|OOF\|WORKINGELSEWHERE`; `X-MICROSOFT-CDO-INTENDEDSTATUS` on invites | Read and write both. Map Busy/Free to `TRANSP` for non-Outlook clients (CAL-05). |
| All-day | `DTSTART;VALUE=DATE` plus `X-MICROSOFT-CDO-ALLDAYEVENT:TRUE` | Write both. |
| Importance | `X-MICROSOFT-CDO-IMPORTANCE: 0\|1\|2`, `PRIORITY` | Round-trip. |
| Time zones | `TZID` with **Windows** zone names (e.g. "W. Europe Standard Time") plus embedded `VTIMEZONE` | Reading ✅ (`dav/src/tzmap.rs` `windows_to_iana`). Writing must emit IANA `TZID` plus `VTIMEZONE` (CAL-11). |
| Invitation | `METHOD:REQUEST`, `ORGANIZER`, `ATTENDEE;ROLE=REQ-PARTICIPANT\|OPT-PARTICIPANT;RSVP=TRUE` (RFC 5546 / iMIP RFC 6047) | Use the existing `build_imip_vcalendar(…, ImipMethod::Request)` (CAL-14). |
| Cancel | `METHOD:CANCEL`, `STATUS:CANCELLED`, incremented `SEQUENCE` | Same builder, `ImipMethod::Cancel`. |
| Propose new time | iTIP `COUNTER` / `DECLINECOUNTER` (**verify** Outlook's internet form against [MS-OXCICAL] with a captured sample) | Add `ImipMethod::Counter` / `DeclineCounter` (CAL-17). |
| Disallow proposals | `X-MICROSOFT-DISALLOW-COUNTER:TRUE` | Respect it: hide "Propose new time". |
| Online meeting | `X-MICROSOFT-SKYPETEAMSMEETINGURL`, `X-MICROSOFT-ONLINEMEETINGCONFLINK` (**verify**), plus the link in DESCRIPTION | Detect and show a "Join" button. Lookout already writes `CONFERENCE`/`URL`. |
| Meeting forward | `METHOD:REQUEST` sent by a non-organizer with the new attendee added, plus a forward notification to the organizer | CAL-18. |

### 4.4 Microsoft Graph surface (for `lookout-graph`)

| Feature | Endpoint / property | Scope |
|---|---|---|
| Category master list | `GET/POST /me/outlook/masterCategories`; colour `preset0`…`preset24` | `MailboxSettings.ReadWrite` |
| Message categories | `PATCH /me/messages/{id}` `{categories:[…]}` | `Mail.ReadWrite` (already granted) |
| Follow-up flag | `followupFlag { flagStatus: notFlagged\|flagged\|complete, startDateTime, dueDateTime, completedDateTime }` | `Mail.ReadWrite` |
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
| `winmail.dat` (TNEF) | [MS-OXTNEF] | Decoder in `core/src/interop/tnef.rs`: extract attachments (`attAttachData`/`attAttachTitle`), the RTF body (decompress per [MS-OXRTFCP]; de-encapsulate HTML per [MS-OXRTFEX]), and the meeting request if present. Hook it into `mail/src/structure.rs` so TNEF parts become normal attachments. |
| `.msg` | [MS-OXMSG] on a CFB container | Read via the `cfb` crate plus a property-stream mapper (or evaluate the `msg_parser` crate). Open when dropped or double-clicked; "Save as .msg" is lower priority. |
| PST | [MS-PST] | Import wizard. Evaluate Microsoft's open-source Rust PST crate (`outlook-pst`; **verify** status and licence) before falling back to `libpff`. |
| Outlook CSV contacts | Outlook's "Comma Separated Values" export columns (`First Name`, `Last Name`, `Company`, `Job Title`, `E-mail Address`, `E-mail 2 Address`, `Business Phone`, `Mobile Phone`, `Home Street`, …) | Import and export mapping in `contacts_view.rs` (P-08). |
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
| Toggle flag / custom flag | Insert / Ctrl+Shift+G | Ctrl+Shift+G = Pin | Remap when flags exist (M-10) |
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
| Undo | Ctrl+Z | — | With M-21 |

---

## 5. Gap analysis by area

"Today" evidence paths are relative to `source/crates/`.

### 5.1 Mail

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| M-01 | Multi-account, IDLE push, offline cache | `mail/src/session.rs` actor, dedicated IDLE connection, CONDSTORE/QRESYNC | ✅ | — |
| M-02 | Unified inbox | `TreeItem::Unified` / `MailView::UnifiedInbox` | ✅ | — |
| M-03 | Focused / Other inbox | — | ❌ | Graph `inferenceClassification` on Microsoft 365; local heuristic elsewhere |
| M-04 | Conversation view | `app/src/message_list.rs` `group_threads` (list grouping only) | 🟡 | No newest-on-top/bottom setting; no stacked conversation in the reading pane; no "show messages from other folders" |
| M-05 | Reading pane right / bottom / off | `layout-reading-pane` boolean | 🟡 | No bottom position |
| M-06 | Density and message preview lines | `layout-vertical-spacing` tight/medium/loose | 🟡 | No 0/1/2/3-line preview setting; no "show sender image" toggle |
| M-07 | Sort / arrange by | `message_list.rs` `SortKey { Date, Sender, Subject }`; filter All/Unread/Flagged | 🟡 | Missing size, importance, flag/due, category, to, attachments; group headers only for Date |
| M-08 | Filter menu | All/Unread/Flagged | 🟡 | Add: has attachments, @mentions me, to me, categorised, high importance |
| M-09 | Categories (named, coloured, multiple) | `app/src/tags.rs` (`$Lookout-tag-*` keywords), Categorize menu, drop-on-tag | 🟡 | No master-list editor with Outlook's 25 colour presets; no Microsoft 365 sync |
| M-10 | Flag for follow-up with dates | Pin = `\Flagged`; "Follow up" creates a linked task with `due: None` (`window.rs` `build_task_seed_for_email`) | 🟡 | No Today/Tomorrow/This week/Next week/No date/Custom/Mark complete; no flag state on the message itself; no due-date list column |
| M-11 | Snooze | Fixed "tomorrow 9 AM"; local-only `snoozed` table (`mail/src/cache.rs`) | 🟡 | Presets (later today, tomorrow, this weekend, next week, custom); syncing across installs (§4.2) |
| M-12 | Importance display | — | ❌ | Not parsed or shown (§4.1) |
| M-13 | Pin | Pinned section, Graph mirror (`graph_pin.rs`) | ✅ | — |
| M-14 | Rules | — | ❌ | Client engine plus Sieve plus Graph `messageRules` |
| M-15 | Quick Steps | — | ❌ | User-defined action chains |
| M-16 | Sweep | — | ❌ | Delete/move all from sender, keep latest, older than N days |
| M-17 | Junk: safe / blocked senders | "Report" moves to Junk; Empty Junk | 🟡 | No lists; no `$Junk`/`$NotJunk`; no "Not junk" action |
| M-18 | Search syntax and refiners | Local FTS5 (`mail/src/cache.rs` `sanitize_fts_query`) plus server `UID SEARCH TEXT` | 🟡 | No `from:`/`to:`/`subject:`/`has:attachment`/`received:`/`is:`/`category:`; no refiners, scope picker or search history |
| M-19 | Search Folders (saved searches) | — | ❌ | |
| M-20 | Ignore conversation / Clean up conversation | — | ❌ | |
| M-21 | Undo (move, delete, flag, archive) | Optimistic rollback only on server failure | ❌ | Toast "Undo" plus inverse command |
| M-22 | Mark-as-read options | Marks read on open | 🟡 | "After N seconds", "when selection changes", "never" |
| M-23 | Read receipts | Request toggle, banner, RFC 8098 MDN (`mail/src/send.rs`) | ✅ | Add "always / never / ask" policy |
| M-24 | Automatic replies (OOF) | — | ❌ | Graph or Sieve vacation |
| M-25 | @mentions | — | ❌ | Highlight "@Me" in the list; filter |
| M-26 | Message recall | — | ⛔ | Exchange-internal only |
| M-27 | Delivery receipts (DSN) | — | ❌ | §4.1 |
| M-28 | Print | `print_visible_message` | ✅ | — |
| M-29 | Save as `.eml` / `.msg` / PDF | `.eml` via More menu and drag-out | 🟡 | `.msg`, "Save as PDF" via print-to-file |
| M-30 | View source / message headers | Debug `.eml` viewer only | ❌ | "View message details" dialog (headers, raw source) |
| M-31 | Open message in new window | Pop-out only for compose and People | ❌ | |
| M-32 | Attachment preview | Inline `cid:` images; Open / Open with / Save | 🟡 | In-app preview for PDF, images, text (Office via LibreOffice conversion, optional) |
| M-33 | Download / save all attachments, zip | Per-attachment save | 🟡 | "Save all", "Download as zip" (the retired `TODO.md` noted the zip variant was missing) |
| M-34 | Remote content / trusted senders | `core/src/trust.rs`, `app/src/trusted_senders.rs` | ✅ | — |
| M-35 | Phishing / unverified-sender / external banners | `EmailBody.auth_results` exists, never filled | ❌ | §4.1 |
| M-36 | Translate | — | ❌ | Pluggable provider (LibreTranslate, or the assistant endpoint) |
| M-37 | Immersive Reader / zoom | Dark message theme toggle | 🟡 | Ctrl+scroll / Ctrl+± zoom on the WebKit view; reader mode |
| M-38 | Message context menu | Row right-click shows only category toggles (`window.rs`) | 🟡 | Full Outlook menu: reply, forward, mark, flag, categorise, move, rules, junk, ignore, delete, view source |
| M-39 | Notifications | `mail_notifications.rs`, tray, badge | ✅ | Add per-folder / VIP-only options |
| M-40 | Work offline / outbox | Cache-first reads; NetworkMonitor-aware errors | 🟡 | "Work offline" toggle; queued sends (outbox) |
| M-41 | List-unsubscribe | One-click POST plus mailto | ✅ | (Better than classic Outlook) |
| M-42 | Shared mailboxes / delegate access | — | ❌ | Graph (Phase 5); IMAP `NAMESPACE` shared folders |
| M-43 | Archive folder | SPECIAL-USE `\Archive`, Ctrl+E | ✅ | — |
| M-44 | Drag and drop | Rows → folders/tags; out as `.eml` | ✅ | Drop `.eml`/`.msg` *into* a folder (IMAP APPEND) |
| M-45 | TNEF / winmail.dat | — | ❌ | §4.6 |
| M-46 | Voting buttons | — | ⛔ | Decode and display incoming votes only (from TNEF) |

### 5.2 Compose

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| C-01 | Rich formatting | `app/src/compose.rs`: B/I/U/S, lists, 4 fonts, 4 sizes, colour, align, indent, HR, link, image, table | 🟡 | Styles/headings, highlight, any installed font, point sizes, format painter, superscript/subscript, paste options |
| C-02 | Spell check | WebKit spellchecking not enabled | ❌ | `WebContext::set_spell_checking_enabled` plus language picker; F7 |
| C-03 | Plain text vs HTML | Rich-text toggle; `multipart/alternative` | ✅ | — |
| C-04 | From / Bcc / identities | `app/src/identities.rs`, From dropdown | ✅ | "Send as" for Microsoft 365 shared mailboxes (Phase 5) |
| C-05 | Importance (High/Low) | `ComposedMessage` has no field | ❌ | §4.1 |
| C-06 | Sensitivity | — | ❌ | §4.1 |
| C-07 | Request read / delivery receipt | Read only | 🟡 | DSN (M-27) |
| C-08 | Schedule send / delay delivery | — | ❌ | Outbox `not_before` (Phase 0); server-side via Graph on Microsoft 365 |
| C-09 | Undo send | — | ❌ | Outbox delay 5/10 s |
| C-10 | Signatures | Rich, per-account defaults for new and reply (`app_config.rs` `signature_defaults`) | ✅ | — |
| C-11 | Drafts autosave | 5 s autosave to IMAP Drafts | ✅ | — |
| C-12 | Reopen a draft | No way to edit a saved draft | ❌ | Open from Drafts into the composer (parse MIME back) |
| C-13 | Forward with attachments / as attachment | Forward quotes text and drops attachments | 🟡 | Re-attach original parts; "Forward as attachment" (`message/rfc822`) |
| C-14 | Emoji picker | — | ❌ | `gtk::EmojiChooser` |
| C-15 | Paste / drop images and files | Paste images works; file drop blocked (`compose.rs:722`) | 🟡 | Drop files → attachments |
| C-16 | Attachment reminder | — | ❌ | Scan body for "attached / attachment" on send |
| C-17 | My Templates | — | ❌ | |
| C-18 | Check names / resolve recipients | Autocomplete plus picker (`recipient_entry.rs`, `contact_picker.rs`) | 🟡 | Expand contact lists (P-05); GAL lookup (P-11) |
| C-19 | @mention insertion | — | ❌ | `@` → contact picker → adds to To |
| C-20 | Reply with meeting | — | ❌ | Prefill event editor with thread participants |
| C-21 | Encrypt / sign | — | ❌ | Phase 6 |
| C-22 | Voting buttons, expiration | — | ⛔ | Exchange-only |
| C-23 | Mail merge | — | ❌ | Phase 7 |

### 5.3 Calendar

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| CAL-01 | Day / Work week / Week / Month | `app/src/calendar_view.rs` `build_main` | ✅ | — |
| CAL-02 | Split / agenda / list view | Split (`build_split_view`); agenda only inside Split | 🟡 | Standalone list view |
| CAL-03 | Schedule (timeline) view | — | ❌ | Calendars as rows, horizontal time |
| CAL-04 | Multiple calendars: overlay / side-by-side | Overlay plus checklist | 🟡 | Side-by-side columns |
| CAL-05 | Show as: Free/Tentative/Busy/OOF/Working elsewhere | Busy/Free only (`EventTransparency`) | 🟡 | §4.3 |
| CAL-06 | Private / sensitivity | `EventSensitivity` | ✅ | — |
| CAL-07 | Categories with colours | Free-text field | 🟡 | Share the mail category master list (M-09) |
| CAL-08 | Week start | Hard-coded Sunday | ❌ | Setting |
| CAL-09 | Working hours and days | Hard-coded 08–18, Mon–Fri | ❌ | Setting |
| CAL-10 | Second time zone in the grid | — | ❌ | |
| CAL-11 | Per-event time zone | Reads TZIDs (`dav/src/tzmap.rs`); writes UTC only | 🟡 | TZ picker; emit `TZID` plus `VTIMEZONE` |
| CAL-12 | Reminders | `app/src/reminders.rs` notifications (Open / Snooze 5 min / Dismiss) | 🟡 | Reminders window (Dismiss all, snooze choices), multiple alarms, default reminder setting |
| CAL-13 | Recurrence | Full expansion (`dav/src/recurrence.rs`); builder in `app/src/recurrence.rs` limited to daily/weekly/monthly/yearly | 🟡 | "2nd Tuesday", "last weekday", "day 15", yearly by month/day |
| CAL-14 | Send meeting invitations (organizer) | Attendees saved to CalDAV only; `build_imip_vcalendar(Request/Cancel)` is never called from the app | 🟡 | iMIP REQUEST on save, update on change, CANCEL on delete, unless the server does RFC 6638 implicit scheduling |
| CAL-15 | RSVP | `respond_to_imip_invitation` (Accept/Maybe/Decline) | 🟡 | "Edit response before sending", "Don't send a response", comment |
| CAL-16 | Response tracking | `AttendeeStatus` parsed, never shown; incoming REPLY only dismisses banner | ❌ | Apply REPLY to the stored event; tracking list with counts |
| CAL-17 | Propose new time | — | ❌ | §4.3 COUNTER |
| CAL-18 | Forward meeting | — | ❌ | §4.3 |
| CAL-19 | Scheduling Assistant / free/busy / room finder | Editor only previews your own day | ❌ | RFC 6638 VFREEBUSY / Graph `getSchedule`; rooms via Graph |
| CAL-20 | Teams / online meeting | Video-call switch writes `CONFERENCE`/`URL` | 🟡 | Teams via Graph `isOnlineMeeting`; "Join" button on detected links |
| CAL-21 | Event attachments | — | ❌ | RFC 8607 / Graph |
| CAL-22 | Event search | Calendar "Filter" button disabled | ❌ | Search over `dav` cache |
| CAL-23 | Print calendar | `print_calendar_month` exists, but the Calendar toolbar print button is created insensitive (`window.rs:4400`) and never re-enabled | 🟡 | **Bug:** enable the button. Add Day/Week/Month/Agenda print styles |
| CAL-24 | Share / publish calendar | Share button disabled | ❌ | Graph `calendarPermissions`; CalDAV sharing where supported |
| CAL-25 | Open someone else's calendar | Only calendars already in the account | 🟡 | Graph `/users/{id}/calendars`; CalDAV principal browse |
| CAL-26 | Internet calendar subscriptions | `dav/src/subscription.rs` | ✅ | — |
| CAL-27 | Birthdays | `dav/src/birthdays.rs` | ✅ | — |
| CAL-28 | Holidays | — | ❌ | Bundled country `.ics` list, subscribed via CAL-26 |
| CAL-29 | Weather in calendar header | — | ❌ | Optional (Open-Meteo); low priority |
| CAL-30 | Drag to move / resize | `DragMode`, `attach_month_drag` | ✅ | Ctrl-drag to copy |
| CAL-31 | Event context menu (copy, duplicate, delete, categorise, show as, private) | — | ❌ | |
| CAL-32 | Calendar colour picker | Server colour or palette (`app/src/calendar_colors.rs`) | 🟡 | Picker; write back `calendar-color` |
| CAL-33 | Calendar groups | Grouped by account | 🟡 | User-defined groups |
| CAL-34 | My Day / calendar peek in Mail | `refresh_mail_overview_day_list` | ✅ | — |
| CAL-35 | Incoming invitation card | `render_invite_card` | ✅ | Add conflict indicator ("Conflicts with …") |

### 5.4 People

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| P-01 | Contact CRUD, multi email/phone/address | `core/src/vcard.rs`, `app/src/contacts_editor.rs` | ✅ | — |
| P-02 | Anniversary, IM, website, custom fields | Preserved, not editable | 🟡 | Editor rows |
| P-03 | Photo | Not displayed | 🟡 | Show vCard `PHOTO`; Graph photo; sender avatar in list |
| P-04 | Groups / favourites | Groups, starred contacts | ✅ | — |
| P-05 | Contact lists in compose | Lists shown, not expandable in picker/autocomplete | 🟡 | Expand on insert (Outlook "+" expansion) |
| P-06 | Search in People | Only in picker/autocomplete | 🟡 | Search entry in People tab; global search scope |
| P-07 | Sort / File as | Display name only | ❌ | First/Last, Last/First, Company |
| P-08 | Import/export CSV | `.vcf` only | 🟡 | Outlook CSV (§4.6) |
| P-09 | Link / merge duplicates | Exact-dupe skip on import | ❌ | Merge UI |
| P-10 | Profile card | Header button opens editor | 🟡 | Hover/click card: email, phone, org, recent mail, shared files/events, "Schedule meeting" |
| P-11 | Directory (GAL / LDAP) | — | ❌ | Graph `/users`; LDAP |
| P-12 | Recently deleted | Local Deleted bucket | ✅ | — |

### 5.5 Tasks (To Do)

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| T-01 | Task lists sidebar | Lists merged (`merged_tasks`); chosen only in editor | 🟡 | List navigation pane, create/rename lists |
| T-02 | My Day | — | ❌ | Per-day pin set (local; Graph To Do has no My Day API; **verify**) |
| T-03 | Flagged email as tasks | `task_for_email` linked via notes marker | ✅ | Unify with M-10 flags; Graph `linkedResources` |
| T-04 | Due, importance, % complete, notes | `app/src/task_editor.rs` | ✅ | — |
| T-05 | Reminders | — | ❌ | `VALARM` in VTODO; Graph `reminderDateTime` |
| T-06 | Recurrence | — | ❌ | `RRULE` in VTODO; Graph `recurrence` |
| T-07 | Steps / subtasks | — | ❌ | `RELATED-TO;RELTYPE=PARENT`; Graph `checklistItems` |
| T-08 | Categories | Stored, not editable | 🟡 | Share master list |
| T-09 | Sort / group options | Fixed buckets (`group_tasks`) | 🟡 | Sort by due/importance/title/created; group by list/category |
| T-10 | Assign / task requests | — | ⛔ | Exchange-only (classic) |
| T-11 | Google Tasks, CalDAV VTODO, local list | `app/src/google_tasks.rs`, `dav` VTODO, `ui_state_db` | ✅ | — |
| T-12 | Microsoft To Do | — | ❌ | Graph (A-05) |

### 5.6 Accounts and Microsoft 365

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| A-01 | Accounts from GOA | `goa/src/discovery.rs` (skips accounts without IMAP/SMTP) | ✅ | — |
| A-02 | Manual IMAP/SMTP | `app/src/other_accounts.rs` | ✅ | OAuth for manual Gmail/Microsoft 365 |
| A-03 | Microsoft 365 calendar | — | ❌ | Graph backend |
| A-04 | Microsoft 365 contacts | — | ❌ | Graph backend |
| A-05 | Microsoft To Do | — | ❌ | Graph backend |
| A-06 | Microsoft 365 mail | IMAP/SMTP with Lookout's own XOAUTH2 (`microsoft_oauth.rs`) | 🟡 | Graph-only mail features (categories, flags, Focused, rules, OOF) |
| A-07 | Exchange on-premises (EWS) | — | ❌ | Deferred (§9) |
| A-08 | POP3 | — | ❌ | Low priority; IMAP covers modern hosts |

### 5.7 Security and compliance

| ID | Outlook feature | Lookout today | Status | Gap |
|---|---|---|---|---|
| S-01 | Block remote content | `mail-load-remote-images`, trusted senders | ✅ | — |
| S-02 | Sandboxed rendering | WebKit sandbox | ✅ | — |
| S-03 | Sender authentication banners | Types present, unpopulated | 🟡 | §4.1 |
| S-04 | S/MIME sign/encrypt/verify | — | ❌ | GPGME (gpgsm) |
| S-05 | OpenPGP | — | ❌ | GPGME (gpg) |
| S-06 | External-sender tag | — | ❌ | Configurable internal domains |
| S-07 | Sensitivity labels (Purview) | — | ⛔ | Tenant-specific; show the header-based sensitivity only |
| S-08 | Link safety (hover shows real URL, warn on mismatch) | — | ❌ | WebKit `mouse-target-changed` → status bar; text/href mismatch warning |
| S-09 | Report phishing | Report junk only | ❌ | Report-as-phishing (forward-as-attachment to configured address) |

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

**Basis of comparison.** For Lookout: the current UI code (`source/crates/app/src/window.rs`, `calendar_view.rs`, `compose.rs`, `folder_tree.rs`, `recipient_entry.rs`) and the screenshots in `assets/screenshots/`. Screenshots can lag the code, so recheck screenshot-derived rows (marked **verify**) against a current build. For Outlook: new Outlook for Windows / OWA as of 2026. Ribbon contents and orders are approximate and should be re-captured from a live build before implementation.

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
| UX-02 | **Flag** (follow-up) vs **Pin** | Both icons in the ribbon; flag today means "create a linked task" | Flag = follow-up flag (M-10); Pin = pin. Never use one word for the other |
| UX-03 | **Junk Email** | Server folder name shown verbatim (e.g. "Junk Mail") | Show the well-known display name for SPECIAL-USE roles (Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive), with an option to "Use server folder names" |
| UX-04 | **New mail** / **New event** / **New contact** / **New task** | Tray says "Compose…"; ribbon icon has no label | "New mail" everywhere (tray, ribbon, menus) |
| UX-05 | **Report** ▸ Report junk / Report phishing / Not junk | "Report" (junk only) | Split button with the three verbs (M-17, S-09) |
| UX-06 | **Move to** ▸ (recent folders, Other folder…) | Drag-and-drop only | Ribbon and context-menu action (UX-21) |
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
| UX-21 | Mail Home ribbon order | **New mail ▾** · Delete ▾ · Archive · Report ▾ · Sweep · Move to ▾ · Reply · Reply all · Forward ▾ · Read/Unread · Categorize ▾ · Flag ▾ · Pin · Snooze ▾ · Quick steps ▾ · Print · … | New, Reply, Reply all, Forward, Delete, Archive, Report, Pin(★), Flag, Read/Unread, Categorize, Snooze, ⋮ (icon-only) | Reorder into Outlook's groups. Add **New mail ▾** (New event, New contact, New task), Sweep, Move to ▾ (recent folders, "Other folder…"), Forward ▾ (Forward as attachment), Snooze ▾ presets, Quick steps ▾. Labels per UX-16 | S |
| UX-22 | Mail View ribbon | View settings · Messages ▾ (Show as conversations / individual messages) · Layout: Folder pane ▾ (show / collapsed / hide), Reading pane ▾ (right / bottom / hidden), Density ▾ (Full / Medium / Compact), Message preview ▾ · Zoom · Sender image · Immersive reader · My Day | "Layout" group (`window.rs` ~4422) | Fill out to Outlook's set: M-04, M-05, M-06, M-37, X-04 | M |
| UX-23 | Folder pane order | Favorites, then each account (by email address) with **Inbox, Drafts, Sent Items, Deleted Items, Junk Email, Archive**, Notes, Conversation History, then user folders A–Z; "Create new folder" at the end of each account | All Inboxes, Favorites, accounts; each account sorts **Inbox first, then everything alphabetical** (`folder_tree.rs` `sort_paths`), so Deleted Items lands before Drafts | Sort SPECIAL-USE roles in Outlook's fixed order before user folders. Add a "Create new folder" row. Keep All Inboxes (Outlook for Mac has it; harmless) | S |
| UX-24 | Folder counts | Unread count for most folders; **Drafts shows its total item count**; Junk shows unread; Deleted shows nothing by default | Unread count everywhere; folder ⋯ menu has a count option (`folder_menu.rs`) | Default Drafts to total count, matching Outlook | S |
| UX-25 | Collapse folder pane | ☰ button collapses the pane to a narrow strip (Favorites icons) | Pane is resizable only | Collapse toggle (☰) and View ▸ Folder pane ▸ Collapsed | S |
| UX-26 | List header | Folder name + ☆ (favorite) · **Focused \| Other** tabs · Select (checkbox) · **Filter** ▾ (one menu: Unread, Flagged, To me, Has files, Mentions me, then **Sort by** and order) | Folder name + account subtitle · ☆ · select checkbox · refresh · "All ▾" filter · sort-direction button · "By Date ▾" | Merge filter and sort into one Filter menu with Outlook's items (M-07, M-08). Focused/Other tabs when enabled (M-03). Keep refresh only if Outlook keymap F9 isn't enough (optional) | S |
| UX-27 | Column header row | **Only in the single-line (wide / reading pane off or bottom) layout.** Sortable columns: Importance, Icon, Attachment, From, Subject, Received, Size, Categories, Flag status; right-click a header to add or remove columns | Fixed "Sender / Subject / Date" header shown **above the two-line rows** (screenshots) | Hide headers in the multi-line layout. Add the single-line columnar layout with clickable, configurable headers, used automatically when the reading pane is bottom/off or the list is wide (pairs with 1.4) | M |
| UX-28 | Message row (multi-line) | Line 1: **sender** (bold if unread); hover actions on the right (Delete, Flag, Pin, Read/Unread). Line 2: **subject** (accent colour and bold if unread) + received time on the right. Line 3: preview. Status icons: attachment, importance (!), flag, meeting, replied/forwarded arrow, reminder bell, **category chips** under the preview. Unread = accent bar on the left edge | Line 1: sender + 📎 + **subject squeezed onto the same line** (often truncated to "Te…") + date. Line 2: preview. Avatar; unread accent bar ✅; hover quick menu (`window.rs` "hover-quickmenu") | Move subject to its own line (Outlook's three-line default), with "Message preview" lines from UX-22. Add the status-icon set (importance, flag, replied/forwarded, meeting) and category chips. Bold sender plus accent subject for unread | M |
| UX-29 | Selection affordance | Avatar becomes a round checkbox on hover / in Select mode; Ctrl and Shift click; reading pane shows "N items selected" with bulk actions | Ctrl/Shift multi-select; "multi" reading-pane page ✅ | Avatar → checkbox on hover; bulk-action buttons on the multi page | S |
| UX-30 | Group headers and dates | Today, Yesterday, Last week, Two weeks ago, Three weeks ago, Last month, Older; today shows time, this week "Mon 10:42", older a short date | Today, Yesterday, Last Week, This Month, month names, Older; similar date formats ✅ | Align bucket names to Outlook. Group headers for every sort key (by sender A–Z, by size range, by category, by flag due) | S |
| UX-31 | Draft marker | Conversation with an unsent reply shows a red **[Draft]** prefix in the list | — | Show [Draft] when Drafts holds a message whose `In-Reply-To` matches (pairs with C-12) | S |
| UX-32 | Reading pane header | Subject on top; avatar; sender name (click → **profile card**); To/Cc expandable; date; action bar: (reaction), Reply, Reply all, Forward, **⋯** (Reply all by meeting, Forward as attachment, Print, Translate, Immersive reader, View ▸ message details / source, Report, Block, Create rule, Open in new window) | Subject, avatar, name, address, To, date, Reply / Reply all / Forward, contact button, ⋮ ✅ (screenshot) | Make the ⋯ menu Outlook's list (M-30, M-31, M-36, M-37). Clicking the name opens the profile card (P-10). Reactions ⛔ (Exchange-only) | S |
| UX-33 | Info bars | Stacked bars above the body: "You replied to this message on …" / "You forwarded …", "Some content has been blocked — Show blocked content", "This sender is external", "This message was identified as junk — It's not junk", "Follow up. Due by …", "This item will be deleted in N days", importance/sensitivity notes | Remote-content banner ✅; read-receipt banner ✅; invitation banner ✅ | One info-bar stack component with Outlook's wording and order. Add replied/forwarded (`\Answered`/`$Forwarded`), external (S-06), junk ("It's not junk"), follow-up (M-10), importance/sensitivity (M-12, C-06) | M |
| UX-34 | Attachments in reading pane | Tiles (file-type icon, name, size) under the header; each tile ▾: Preview, Download, Save to…; "Download all" / "Save all" link | Attachment rows with a ☰ menu (Open / Open with / Save) ✅ | Tile layout, Preview as the default click (M-32, 7.3), "Download all" (M-33) | S |
| UX-35 | Conversation in reading pane | Stacked message cards, newest on top (option): older collapsed to sender + one-line preview, click to expand; inline "Reply" at the top | One message at a time | Implement with 1.7 (M-04) | L |
| UX-36 | Message context menu | Reply, Reply all, Forward · Delete, Archive, Move ▸, Copy ▸ · Mark as unread, Flag ▸, Pin, Snooze ▸, Categorize ▸ · Ignore, Report ▸, Block ▸ · Rules ▸ Create rule · View ▸ · Open in new window, Print | Category toggles only | Exactly this list and order (M-38, 1.17) | M |
| UX-37 | Default open / navigation | Single-click selects (marks read per setting); **double-click or Enter opens a separate window**; after delete/move: "open next item" (setting: next / previous / return to list); Esc closes a popped-out item | Single-click opens in the pane; double-click/Enter do nothing extra; Esc **closes the reading pane** | Double-click/Enter → new window (M-31). "After moving or deleting" setting. Esc should close popped windows, not hide the reading pane (remap Lookout's close-pane action, or keep it only in the Lookout keymap) | S |
| UX-38 | Drag and drop | Drag to a folder = move; **Ctrl+drag = copy**; drag onto the Calendar app icon / My Day calendar = new event; onto To Do = task; onto a category is not supported | Rows → folders/tags; out as `.eml` ✅ | Add Ctrl=copy, drop on nav-rail Calendar/Tasks icons and My Day (UX-18). Keep drop-on-category (a Lookout extra) | S |
| UX-39 | Sender avatars | Contact photo if known, else coloured initials; setting "Show sender images" | Coloured initials ✅ | Photos from vCard / Graph (P-03); on/off setting | S |

### 6.5 Compose

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-40 | Compose ribbon | Contextual tabs replace Home: **Message** (Attach file ▾, Signature ▾, Importance, Dictate), **Insert** (File, Picture, Table, Link, Emoji, Signature, My Templates, Meeting), **Format text** (font, size, B I U S, highlight, colour, bullets, numbering, indent, align, styles, clear), **Draw**, **Options** (Importance, Sensitivity, Receipts, Delay delivery, Encrypt, Switch to plain text, Spelling language) | Formatting toolbar takes over the action bar (one strip) | Split into Message / Insert / Format text / Options tabs on the shared ribbon component (UX-16). Draw is out of scope | M |
| UX-41 | Field order | Top bar: **Send ▾** (Schedule send), Discard, Pop out. Then **From** (shown with multiple identities), **To** (Cc / Bcc links at the right end), Cc, Bcc, **Subject**, attachments strip, body. Tab order To → Cc → Subject → body | Send, From dropdown, then To / Cc / Bcc / Subject (`compose.rs` `fields_group`) ✅. The older `mail_compose_pop_in_1.png` screenshot shows Subject above To and should be refreshed | Keep the order; check Tab order To → Cc → Bcc → Subject → body. Send becomes a split button (C-08) | S |
| UX-42 | Recipient pills | Resolved recipients are **pills** (name, presence/avatar, ✕), external recipients flagged, contact lists show a "+" to expand, invalid addresses underlined in red; autocomplete rows show name, address and an ✕ to forget the suggestion | Compose uses `recipient_entry.rs` **plain** comma-separated mode; chip mode exists only in the event editor | Switch compose to chip mode with Outlook's pill behaviour. Expand lists (P-05). Add "Remove from suggestions" (deletes from the `addresses` table) | M |
| UX-43 | Attachments while composing | Tiles under the subject line; attach via Insert ▸ File ▸ *Browse this computer* / *Recent attachments*; drag-in; tile ▾: Open, Remove, (Share as link) | Attach button; file drop blocked (`compose.rs:722`) | Tiles, a recent-attachments list, drag-in (C-15) | S |
| UX-44 | Missing icons | — | Compose toolbar shows red "missing image" placeholders on several buttons (`assets/screenshots/mail_compose_pop_in_1.png`) | **verify** on a current build; bundle or rename any missing symbolic icons | S |
| UX-45 | Send feedback | Toast "Sending…" with **Undo** for N seconds, then sent; schedule send shows a "Scheduled for …" info bar on the draft | Send completes synchronously; failure toast | With 0.3 / 1.13 / 1.14 | S |
| UX-46 | Inline vs pop-out | Reply opens **inline** at the top of the conversation in the reading pane, with a Pop out button; New mail opens in the reading pane (or a window by setting) | Same ✅ | — | — |

### 6.6 Calendar

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-50 | Calendar Home ribbon | **New event ▾** · Day ▾ · Work week · Week · Month · Split view · Filter ▾ · Share ▾ · Print | New-event icon · Day · Work week · Week · Month · Split view ✅ · Filter/Share/Print **disabled** | Labels match. Make New event a split button (Event, Teams meeting). Enable Print (CAL-23 bug), Filter (categories, show as, recurring), Share (CAL-24) | S |
| UX-51 | Date navigation | **Today** button and **‹ ›** arrows on the **left**, then the range title, which is a dropdown that opens a month picker | Range title on the left; Today and arrows on the **right** | Move Today/arrows left of the title; title becomes a picker. Ctrl+G Go to date (§4.7) | S |
| UX-52 | Left pane | Mini month (optional week numbers) · Add calendar · My calendars (coloured checkbox per calendar; hover ⋯: Colour, Rename, Share, Remove) · other groups · Show all | Mini month · Add calendar · My calendars checklist ✅ | Coloured checkboxes in the calendar's colour; per-calendar ⋯ menu (CAL-32); user groups (CAL-33) | S |
| UX-53 | Time grid | Working hours lighter ✅; **current-time line** (red line plus dot) across today; scrolls to around the current time ✅; 30-minute slots, single-click selects a slot, **drag selects a range** ✅ | Business-hours band and scroll-to-now (`calendar_view.rs` `scroll_time_grid_to_now`) ✅; no current-time line found in the paint code (**verify**) | Add the now-line. Week numbers and second time zone in the gutter (CAL-10) | S |
| UX-54 | Calendar View ribbon | Time scale (5/6/10/15/30/60 min) · Colour (calendar vs category) · Show week numbers · Split view orientation · Weather · Overlay vs side-by-side | View tab disabled | Add a View tab with these toggles (CAL-04) | M |
| UX-55 | Quick create | Single-click or drag on empty time → **quick-create popover** (title, time, all-day, invite attendees, location, Teams toggle, Save, **More options** → full form). Double-click → full form | Activating a slot opens the full editor | Quick-create popover as the default; full editor via More options / double-click | M |
| UX-56 | Event peek | **Single-click an event → peek card** (title, time, recurrence, location, Join button, organizer, RSVP buttons for invites, Edit, Delete, ⋯). Double-click → full form | Click opens the full editor (`open_event_editor_for` from `connect_event_activated`) | Peek popover on click; editor on double-click / Edit | M |
| UX-57 | Event chips | Calendar-colour fill with a darker **left stripe**; title, location; icons for recurring ↻, private 🔒, online meeting, reminder; **show-as styling**: tentative = hatched, free = outline only, OOF = purple, working elsewhere = dotted | Coloured chips at 92% opacity | Left stripe, icon set, show-as styling (needs CAL-05) | S |
| UX-58 | Month view overflow | "+N more" opens a day popover listing everything | "+N more" label, no popover (`calendar_view.rs` comment ~439) | Day popover | S |
| UX-59 | Event form | Title · Attendees (Required / Optional; "Response options" ▾) · date/time row with All day, **time zone**, **Repeat** · Location (room suggestions) · **Teams meeting** toggle · Reminder ▾ · Show as ▾ · Categorize ▾ · Private · **Scheduling assistant** · **rich-text** description · footer button reads **Save** with no attendees and **Send** with attendees | Editor has most fields; plain-text Notes; no TZ, scheduling assistant or Send/Save switch (CAL-11, CAL-19) | Field order per Outlook; Save ↔ Send label; rich-text notes (reuse the composer's WebKit editor); Scheduling assistant tab (3.8) | M |
| UX-60 | Invitation card in mail | Accept ▾ / Tentative ▾ / Decline ▾ (each: *Edit response before sending* / *Send response now* / *Don't send a response*), **Propose new time ▾**, and a **mini day timeline** showing the slot against your other events with conflicts highlighted | Accept / Maybe / Decline card ✅ (`render_invite_card`) | Split buttons (CAL-15), Propose new time (CAL-17), mini timeline with conflict highlight (CAL-35). Rename "Maybe" to "Tentative" | M |
| UX-61 | Reminders window | Separate always-on-top window: list of due items, "Dismiss all", Snooze ▾ duration, Join (online meetings), Open | Desktop notifications only | 3.9 (CAL-12) | M |

### 6.7 People

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-65 | Layout | Left: **Favorites, Your contacts, Contact lists, Deleted, Directory/Groups**. Middle: alphabetical list with **letter headers**, avatars, Filter/Sort ▾. Right: **details pane** (photo, name, title/company, action buttons *Send email*, *Schedule meeting*, *Call*, tabs *Overview / Contact info / Files / Emails*) | Left bucket pane; list; edit in a dialog (`contacts_editor.rs`) | Letter headers; details pane with Outlook's actions and an Emails tab (recent mail via FTS, P-10); edit in place in the details pane, with dialog fallback for pop-out | M |
| UX-66 | People ribbon | New contact ▾ (Contact, Contact list) · Edit · Delete · Add to favorites · Add to list · Import contacts · Export contacts · View ▸ sort / file-as | Text buttons: New contact, Import…, Export…, Manage groups… | Icon+label ribbon (UX-16), Outlook verbs, View tab (P-07) | S |
| UX-67 | Profile card | Hover or click a person anywhere (reading pane, attendee list, chip) → card with contact info, recent emails, files, org, "Schedule meeting" | Header contact button opens the editor | 4.2 (P-10), triggered from every person reference | M |

### 6.8 Tasks

| ID | Element | Outlook | Lookout today | Change | Effort |
|---|---|---|---|---|---|
| UX-71 | Navigation | Left: **My Day, Important, Planned, Assigned to me, Flagged email, Tasks**, then user lists (+ New list) | One list, fixed buckets (Overdue / Today / This week / Later / Completed) | Smart lists plus user lists (T-01, T-02). Keep the buckets as the grouping inside "Planned" | M |
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
| 0.7 | `lookout-graph` crate skeleton | `reqwest` client, token supplier from `microsoft_oauth.rs` (`TokenAudience::Graph`), paging (`@odata.nextLink`), delta queries (`/delta`), throttling (`Retry-After`), error model; per-feature scope negotiation | `source/crates/graph` (new), `microsoft_oauth.rs` | M |
| 0.8 | Fixture corpus | Capture real Outlook artefacts: invite, update, cancel, counter, TNEF, `.msg`, importance/sensitivity mail, Teams invite | `source/test-fixtures/outlook/` | S |

### Phase 1: Mail daily-driver parity (mostly S/M; highest value per hour)

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 1.1 | Spell check | C-02 | Enable WebKit spellcheck on the compose view; language from locale plus a setting; F7 shortcut | S |
| 1.2 | Importance in list and compose | M-12, C-05 | Uses 0.1/0.2; `!`/`↓` list glyph; compose toolbar toggle; write the three headers in `send.rs` | S |
| 1.3 | Fix Calendar print button | CAL-23 | Make it sensitive (`window.rs:4400`) | S |
| 1.4 | Reading pane bottom | M-05 | `layout-reading-pane` → enum `right\|bottom\|off`; swap `gtk::Paned` orientation | M |
| 1.5 | Preview lines and sender images | M-06 | Setting 0–3 lines; avatar (initials, or vCard photo) column | S |
| 1.6 | More sort keys and filters | M-07, M-08 | Extend `SortKey` (Size, Importance, Flag, Category, To, Attachments) and the filter enum; group headers per key | M |
| 1.7 | Conversation options | M-04 | Newest-on-top/bottom setting; stacked conversation rendering in the reading pane (one WebKit view with collapsible messages) | L |
| 1.8 | Mark-as-read options | M-22 | Setting: on open, after N seconds, on selection change, never | S |
| 1.9 | Message in new window | M-31 | Reuse the reading-pane builder in an `adw::Window`; double-click and Enter open it | M |
| 1.10 | View message details / source | M-30 | Dialog with parsed headers plus raw source (via `FetchRawMessage`) | S |
| 1.11 | Forward with attachments; forward as attachment | C-13 | Fetch original parts (`FetchAttachment`) into the composer; `message/rfc822` part | M |
| 1.12 | Reopen drafts | C-12 | Open a Drafts message → parse MIME → composer prefill (identity, recipients, HTML, attachments); replace on save | M |
| 1.13 | Undo send / undo actions | M-21, C-09 | 0.3 delay (setting 0/5/10 s) plus 0.4 for move/delete/flag/archive | M |
| 1.14 | Schedule send | C-08 | Send menu → "Schedule send" date-time; 0.3 `not_before`; banner listing scheduled items; Graph deferred send on Microsoft 365 later (5.x) | M |
| 1.15 | Snooze presets and sync | M-11 | Preset menu plus custom; Scheduled folder plus keyword (§4.2) | M |
| 1.16 | Follow-up flags | M-10, T-03 | Message-level flag state (keyword `$Lookout-flag-due-<date>` / `$Lookout-flag-done`; Graph `followupFlag` on Microsoft 365); presets; due column; tasks view shows flagged mail; Pin stays separate | L |
| 1.17 | Message context menu | M-38 | 0.5; full Outlook item menu | M |
| 1.18 | Outlook keyboard preset | UI-04 | §4.7 table as a second defaults set; picker in Settings; new actions (move to folder, next/previous, mark unread, new event/task/contact, F9) | M |
| 1.19 | Attachment reminder | C-16 | Regex over body (localisable word list) on send → dialog | S |
| 1.20 | Emoji, file drop, zoom | C-14, C-15, M-37 | `gtk::EmojiChooser`; drop target on composer; WebKit zoom level plus Ctrl+±/scroll | S |
| 1.21 | TNEF decode | M-45 | §4.6 in `core/src/interop/tnef.rs`, hooked into `mail/src/structure.rs` | M |
| 1.22 | Open `.msg` | M-29 | `cfb` plus a property mapper → `EmailBody` for the viewer; drag-in to APPEND | M |
| 1.23 | Save all / zip | M-33 | Folder chooser plus `zip` crate | S |
| 1.24 | Junk keywords and Not Junk | M-17 | `$Junk`/`$NotJunk`; "Not junk" in the Junk folder | S |

### Phase 2: Organise and search

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 2.1 | Category master list | M-09, CAL-07, T-08 | Editor (name, preset colour, shortcut key Ctrl+F2…F12 *classic*); stored in `settings.json`; synced with Graph `masterCategories` on Microsoft 365; shared by mail, events and tasks | M |
| 2.2 | Search query language | M-18 | Parser in `mail/src/search_query.rs`: `from: to: cc: subject: body: has:attachment is:unread is:read is:flagged is:pinned category: received:>=2026-01-01 received:lastweek size:>1MB folder:`; compile to FTS5 `MATCH` plus SQL predicates locally and IMAP `SEARCH` keys remotely; keep `sanitize_fts_query` for free text | M |
| 2.3 | Search UI: refiners, scope, history | M-18 | Scope dropdown (current folder / subfolders / account / all); refiner chips; recent searches in `ui_state_db` | M |
| 2.4 | Search Folders | M-19 | Saved queries as virtual folders in the tree (unread mail, flagged, from VIPs, large mail, custom) | M |
| 2.5 | Rules engine | M-14 | One model, `Rule { conditions: [From/To/Subject/Body/Header/HasAttachment/Importance/Size/SentOnlyToMe/List-Id], exceptions, actions: [Move/Copy/Delete/Flag/Categorise/MarkRead/Forward/Redirect/StopProcessing] }`, executed by three backends: (a) **client**, run on `NewMessages` in the session; (b) **Sieve** via ManageSieve (§4.5) when the server advertises it; (c) **Graph** `messageRules`. Rules UI shows where each rule runs; "Run rules now" on a folder; "Create rule…" from a message. | L |
| 2.6 | Quick Steps | M-15 | Named action chains (reuses rule actions) in the ribbon and a shortcut slot | M |
| 2.7 | Sweep | M-16 | Dialog from a message: delete/move all from sender, keep latest, delete older than 10 days; optional standing rule | M |
| 2.8 | Ignore / Clean up conversation | M-20 | Ignore: thread-ID list, client rule moves new arrivals to Deleted. Clean up: delete messages whose text is wholly quoted in a later message | M |
| 2.9 | Safe / blocked senders | M-17 | Lists in `ui_state_db`, enforced by the client rule backend; "Block sender" in the context menu; trusted senders (S-01) reuse the safe list | S |
| 2.10 | My Templates | C-17 | Stored HTML snippets; insert from the composer | S |
| 2.11 | Automatic replies | M-24 | Settings page: on/off, schedule, internal/external messages; Graph `automaticRepliesSetting` or Sieve `vacation`; banner in Mail while active | M |
| 2.12 | Focused Inbox | M-03 | Microsoft 365: read `inferenceClassification`, "Move to Focused/Other" writes overrides. Elsewhere, optional local classifier (sender seen in Sent/contacts = Focused; bulk/List-Id = Other). Tabs above the Inbox list | L |
| 2.13 | @mentions | M-25 | Detect the user's address in body `mailto:` anchors / `@Name`; list glyph plus filter | S |

### Phase 3: Calendar and meetings

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 3.1 | Apply calendar preferences | CAL-08, CAL-09, CAL-10 | 0.6 everywhere: grid start day, business-hours band, work-week days, second TZ gutter | M |
| 3.2 | Per-event time zone | CAL-11 | TZ dropdown in `event_editor.rs`; `build_vevent` emits `TZID` plus `VTIMEZONE` (IANA); keep reading Windows names | M |
| 3.3 | Full show-as | CAL-05 | Dropdown Free/Working elsewhere/Tentative/Busy/Away; §4.3 CDO properties; hatched/striped rendering per state as Outlook does | S |
| 3.4 | Organizer invitations | CAL-14 | Detect RFC 6638 server scheduling (`schedule-outbox-URL`, `SCHEDULE-AGENT`); else on save send iMIP REQUEST to attendees (`build_imip_vcalendar` plus `SendMessage`); "Send update" on change (`SEQUENCE++`); CANCEL on delete; "Send / Don't send" prompt as Outlook does | L |
| 3.5 | RSVP options | CAL-15 | Split buttons: "Send response now / Edit response / Don't send"; comment field; `respond_to_imip_invitation` gains params | S |
| 3.6 | Response tracking | CAL-16 | Apply incoming REPLY `PARTSTAT` to the stored event (organizer copy) and PUT; "Tracking" tab in the editor with per-attendee status and counts | M |
| 3.7 | Propose new time / forward | CAL-17, CAL-18 | COUNTER and DECLINECOUNTER builders plus organizer accept-proposal flow; forward = REQUEST to new attendee plus notify organizer; honour `X-MICROSOFT-DISALLOW-COUNTER` | M |
| 3.8 | Scheduling Assistant | CAL-19 | Editor tab: attendee rows × time grid; free/busy via RFC 6638 VFREEBUSY POST, or Graph `getSchedule`; "Suggested times"; room list via Graph `places` | L |
| 3.9 | Reminders window | CAL-12, T-05 | `adw::Window` listing due reminders (events and tasks): Dismiss / Dismiss all / Snooze [5 min…1 week] / Open; multiple `VALARM`s; default reminder; catch-up on launch for missed reminders | M |
| 3.10 | Advanced recurrence builder | CAL-13 | Monthly "day N" / "the Nth weekday"; yearly "month day" / "Nth weekday of month"; BYSETPOS; end by/after; preview string | M |
| 3.11 | List and Schedule views | CAL-02, CAL-03 | Standalone list view (reuse `build_agenda_view`); Schedule view (calendars as rows) | L |
| 3.12 | Event context menu, copy, duplicate | CAL-30, CAL-31 | 0.5; Ctrl-drag copies; "Duplicate", "Copy to calendar", "Save as .ics" | S |
| 3.13 | Calendar colour picker and groups | CAL-32, CAL-33 | `gtk::ColorDialog`; PROPPATCH `calendar-color` when writable; user groups persisted in `settings.json` | M |
| 3.14 | Event search | CAL-22 | Search over the `dav` cache (title, location, notes, attendees); results list; global search scope "Calendar" | M |
| 3.15 | Holidays | CAL-28 | Country picker → subscribe to a bundled public `.ics` URL via CAL-26 | S |
| 3.16 | Event attachments | CAL-21 | RFC 8607 managed attachments (`ATTACH;MANAGED-ID`) where supported; inline `ATTACH;VALUE=BINARY` fallback for iMIP; Graph attachments | M |
| 3.17 | Online meetings | CAL-20 | Detect Teams/Zoom/Meet URLs → "Join" button in card, reminder and peek; Microsoft 365: "Teams meeting" toggle → Graph `isOnlineMeeting` | M |
| 3.18 | Conflict indicator on invites | CAL-35 | Overlap check against the cache in `render_invite_card` | S |
| 3.19 | Reply with meeting | C-20 | Context action: new event with thread participants plus subject | S |
| 3.20 | Print styles | CAL-23 | Day / Week / Month grid / Agenda HTML templates through the existing WebKit print path | M |

### Phase 4: People and tasks

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 4.1 | Full contact field editing | P-02, P-03 | Editor rows for anniversary, IM (`IMPP`), URL, custom; photo view and set (`PHOTO`) | M |
| 4.2 | Profile card | P-10 | Popover on sender name/avatar click in reading pane and list: contact info, recent mail (FTS query by address), upcoming shared events, actions (email, schedule, add to contacts) | M |
| 4.3 | Contact lists in compose | P-05, C-18 | Lists appear in autocomplete/picker; expand to members on insert; optional `+` expander chip | S |
| 4.4 | People search and sort | P-06, P-07 | Search entry in the People tab; File-as setting; global search scope "People" | S |
| 4.5 | Outlook CSV import/export | P-08 | Column map per §4.6; tolerant header matching | M |
| 4.6 | Merge duplicates | P-09 | Match by email/phone/name; merge UI with field-level choice | M |
| 4.7 | Directory lookup | P-11 | Graph `/users` (Microsoft 365) and LDAP (`ldap3`, server configured per account) as an extra provider in `merge_contact_suggestions` | M |
| 4.8 | Task lists sidebar | T-01 | Navigation list in `tasks_view.rs`: My Day, Flagged email, per-list; create/rename/delete lists (CalDAV MKCALENDAR VTODO / Google / local) | M |
| 4.9 | My Day | T-02 | Local pin set per date in `ui_state_db`; suggestions (due today/overdue) | S |
| 4.10 | Task reminders and recurrence | T-05, T-06 | `VALARM`/`RRULE` in `build_vtodo`; roll forward on complete; reminders via 3.9 | M |
| 4.11 | Steps / subtasks | T-07 | `RELATED-TO;RELTYPE=PARENT` children (CalDAV); Google Tasks `parent`; Graph `checklistItems` | M |
| 4.12 | Task sort/group and categories | T-08, T-09 | Sort and group options; category chips from 2.1 | S |

### Phase 5: Microsoft 365 native (Graph)

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 5.1 | Graph calendar backend | A-03, CAL-14…19 | `graph::CalendarSession` implementing the same command/event contract as `dav::CalendarSession`; delta sync of `calendarView`; invites/RSVP/proposals/free-busy/Teams natively (server sends iTIP) | L |
| 5.2 | Graph contacts backend | A-04, P-11 | `contactFolders` plus delta; GAL search; photos | M |
| 5.3 | Microsoft To Do backend | A-05, T-12 | Lists/tasks/checklistItems/linkedResources; flagged-email list maps to M-10 | M |
| 5.4 | Mail property sync | A-06, M-09, M-10, M-12, M-03 | Per-message Graph lookup by `internetMessageId` (cache the IMAP UID ↔ Graph ID map); sync categories, followupFlag, importance, inferenceClassification both ways; extends today's `graph_pin.rs` approach | L |
| 5.5 | Server-side schedule send | C-08 | `PidTagDeferredSendTime` on a Graph draft, so it sends while Lookout is closed | S |
| 5.6 | Shared mailboxes and delegate calendars | M-42, CAL-24, CAL-25 | Add shared mailbox by address (IMAP `user@tenant/shared` login or Graph); open another user's calendar; share mine via `calendarPermissions` | L |
| 5.7 | Optional Graph mail transport | A-06 | Only if 5.4 proves too costly: full Graph mail sync as an alternative to IMAP for Microsoft 365 accounts | L |

### Phase 6: Security and compliance

| # | Item | IDs | Approach | Effort |
|---|---|---|---|---|
| 6.1 | Authentication-Results banners | S-03, M-35 | Parse RFC 8601 into `EmailBody.auth_results`; banners for fail/none and for display-name ↔ address mismatches | S |
| 6.2 | External-sender tag | S-06 | Per-account internal domains; "External" chip | S |
| 6.3 | Link safety | S-08 | Hover status bar with the real URL; warn when text and href domains differ | S |
| 6.4 | S/MIME and OpenPGP | S-04, S-05, C-21 | GPGME (`gpgme` crate). `gpgsm` covers S/MIME and `gpg` covers OpenPGP, one dependency and key storage in the user's existing keyrings. Verify/decrypt in `mail/src/body.rs`; sign/encrypt in `send.rs`; status bar in the reading pane; per-message toggles in compose; certificate import in Settings | L |
| 6.5 | Sensitivity | C-06 | Compose option plus banner (§4.1) | S |
| 6.6 | DSN | M-27, C-07 | SMTP `NOTIFY`/`RET`/`ENVID` (**verify** `lettre` support, else extend `send.rs`); DSN report card | M |
| 6.7 | Report phishing | S-09 | Forward-as-attachment to a configurable address, then move to Junk | S |

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
| U.6 | Compose layout | UX-40…UX-45 | Compose contextual ribbon tabs on U.1; chip-mode recipients in compose; attachment tiles and drag-in; icon audit | M | Phase 1 |
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
- 2.5 → 2.6, 2.7, 2.8, 2.9
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
