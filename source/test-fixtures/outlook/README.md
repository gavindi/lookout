# Outlook fixture corpus (OUTLOOK_PARITY.md item 0.8)

Real artefacts Microsoft Outlook puts on the wire, used to pin the interop
code (`core/src/interop/`, the `text/calendar`/header readers) to observed
behaviour rather than an interpretation of the specs.

## Checked in

| File | Exercises |
|---|---|
| `invite.eml` | iMIP `METHOD:REQUEST`, `X-MICROSOFT-CDO-*` busy/intended status, a Windows `TZID` + `VTIMEZONE`, `SEQUENCE:0`. |
| `invite-iana-tz.ics` | The IANA-`TZID` invitation form other clients (Thunderbird, Google) send, next to Outlook's Windows IDs (CAL-11). |
| `invite-update.eml` | An updated invitation: `SEQUENCE:1` / `X-MICROSOFT-CDO-APPT-SEQUENCE:1`, moved time and location. |
| `cancel.eml` | iMIP `METHOD:CANCEL` with `STATUS:CANCELLED` and an incremented `SEQUENCE`. |
| `counter.eml` | An attendee's iTIP `METHOD:COUNTER` proposing a new time (CAL-17). |
| `disallow-counter.eml` | An invitation with `X-MICROSOFT-DISALLOW-COUNTER:TRUE`, which must hide "Propose new time" (CAL-17). |
| `teams-invite.eml` | A Teams meeting: `X-MICROSOFT-SKYPETEAMSMEETINGURL`, `X-MICROSOFT-ONLINEMEETINGCONFLINK`, and the join link in `DESCRIPTION`/`LOCATION`. |
| `importance-high.eml` | `Importance: High` + `X-Priority: 1 (Highest)` + `X-MSMail-Priority: High`, a `Sensitivity: Private`, and a `List-Id`. |
| `sensitivity-private.eml` | RFC 2156 `Sensitivity: Company-Confidential`. |
| `auth-results.eml` | RFC 8601 `Authentication-Results` (SPF softfail, DKIM fail, DMARC fail, plus a second header), with a `From` display name impersonating a different address — drives the unverified-sender banner (S-03, M-35). |
| `winmail.dat` | A synthetic TNEF stream (`[MS-OXTNEF]`): plain/RTF/HTML bodies, a MAPI property stream, two attachments and an inline image. |
| `tnef-message.eml` | The `winmail.dat` above wrapped in the MIME envelope Outlook sends: `text/plain` plus `application/ms-tnef; name="winmail.dat"`. |
| `freebusy-request.ics` | The RFC 6638 §B.5 `VFREEBUSY` `METHOD:REQUEST` body a client POSTs to a CalDAV schedule-outbox (CAL-19). |
| `freebusy-response.xml` | The RFC 6638 §B.5 `CALDAV:schedule-response` answer: three recipients (two busy, one an invalid calendar user with no `calendar-data`). |
| `automatic-replies.json` | A Graph `GET /me/mailboxSettings` response with `automaticRepliesSetting`: a scheduled range in a Windows time zone, HTML internal/external messages, a `contactsOnly` audience — pins the automatic-replies mapping (M-24, item 2.11). |
| `contacts.json` | A Graph `GET /me/contacts` response: two contacts exercising the name parts, company/department, job title, phones, the three address blocks, IM addresses, categories, a full and a yearless birthday, and the scalar fields with no vCard home — pins the Graph contacts mapping (A-04, item 5.2). |
| `contact-folders.json` | A Graph `GET /me/contactFolders` response, the People screen's address-book rows (A-04, item 5.2). |
| `calendar-permissions.json` | A Graph `GET /me/calendars/{id}/calendarPermissions` response: the owner's own grant plus two others at different roles — pins the sharing mapping (CAL-24, item 5.6). |
| `delegated-calendars.json` | A Graph `GET /users/{id}/calendars` response, the calendars under another user's mailbox — pins the delegated-calendar mapping and its owner tag (CAL-25, item 5.6). |
| `shared-mail-folders.json` | A Graph `GET /users/{id}/mailFolders` response, a shared mailbox's folders with their item counts — pins the shared-mailbox mapping (M-42, item 5.6). |
| `messages-delta.json` | A Graph folder `GET /me/mailFolders/{id}/messages/delta` page: three live records (one carrying categories, importance, a flag and Focused/Other; one properties-only; one importance + Other) and a tombstone — pins the delta page's upsert/removal split and the value ↔ property mapping (A-06, item 5.7.1). |
| `mail-folders-delta.json` | A Graph `GET /me/mailFolders/delta` page: the folder hierarchy (a nested child, Sent Items, Deleted Items) and a tombstone — pins the Graph mail transport's folder mapping (A-06, item 5.7.2). Synthetic, authored to the Graph reference. |
| `message-summary-delta.json` | A Graph folder messages delta at list-row weight: an unread flagged message with categories and an attachment, a read reply in the same conversation, a draft, a meeting request (`eventMessage`) and a tombstone — pins the message-to-row mapping (A-06, item 5.7.2). Synthetic. |
| `message-full.json` | A Graph `GET /me/messages/{id}` read with `body` and `internetMessageHeaders`, including `Authentication-Results` and `List-Id` — pins the reading-pane body mapping (A-06, item 5.7.2). Synthetic. |
| `message-attachments.json` | A Graph `GET /me/messages/{id}/attachments` page: a file attachment and an inline image with a `contentId` (A-06, item 5.7.2). Synthetic; whether Graph returns `contentId` under the transport's `$select` still needs checking against a tenant. |
| `recurrence-rules.ics` | The six recurrence shapes Outlook writes: monthly day-of-month, monthly "second Tuesday" (`BYDAY=2TU`), monthly last weekday (`BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1`), yearly month/day, yearly fourth Thursday, and the Graph round-trip form with `WKST` — pins the Series builder (CAL-13, item 3.10). |
| `event-attachments.ics` | The three `ATTACH` forms an event can carry: an RFC 8607 managed attachment (`MANAGED-ID`/`FILENAME`/`FMTTYPE`/`SIZE` + URI), an inline `VALUE=BINARY;ENCODING=BASE64` file folded across lines, and a plain URI attachment — pins the parser, the stored round trip and the iMIP managed-id strip (CAL-21, item 3.16). Authored to RFC 5545/8607. |

These are authored to the documented wire formats (`[MS-OXCICAL]`, `[MS-OXOMSG]`,
`[MS-OXTNEF]`, RFC 6047, the Microsoft Graph reference) and verified by
`crates/mail/tests/outlook_fixtures.rs`, the decoder's own tests and the Graph
mapping's tests (`graph_automatic_replies`), which parse every one through the
same code the app uses. The optional Graph mail transport (A-06) also
round-trips `invite.eml` and `auth-results.eml` through its `/$value` MIME path
in `crates/graph/tests/graph_mail_session.rs` (item 5.7.9).

## Synthetic binaries

`winmail.dat` and `tnef-message.eml` are **synthesized**, not captured: they are
generated by `crates/core/src/interop/tnef.rs`'s test-only builder, which
follows `[MS-OXTNEF]`/`[MS-OXRTFCP]` attribute by attribute (including the LZFu
compression). A unit test asserts the checked-in bytes match the generator, so
regenerate them after changing the builder:

```sh
cargo test -p lookout-core --lib interop::tnef::tests::regenerate_fixtures -- --ignored
```

Captured real-world samples are still welcome - a formatting-only
`winmail.dat`, a meeting-request one, a MacBinary attachment - and should be
added with a one-line table entry above. Keep anything that could identify a
real person or organisation out of the file, or redact the
addresses/subjects first.

## Still to capture from a real Outlook install

- **`*.msg` (`[MS-OXMSG]` over a CFB container)** — in Outlook, *File → Save As*
  a message as `.msg`.

To capture: send the relevant message, then either save the raw `.eml` (so the
whole MIME envelope is preserved) or extract the binary part, redact anything
identifying, and add it here with a one-line table entry above.
