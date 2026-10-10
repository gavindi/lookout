# Outlook fixture corpus (OUTLOOK_PARITY.md item 0.8)

Real artefacts Microsoft Outlook puts on the wire, used to pin the interop
code (`core/src/interop/`, the `text/calendar`/header readers) to observed
behaviour rather than an interpretation of the specs.

## Checked in

| File | Exercises |
|---|---|
| `invite.eml` | iMIP `METHOD:REQUEST`, `X-MICROSOFT-CDO-*` busy/intended status, a Windows `TZID` + `VTIMEZONE`, `SEQUENCE:0`. |
| `invite-update.eml` | An updated invitation: `SEQUENCE:1` / `X-MICROSOFT-CDO-APPT-SEQUENCE:1`, moved time and location. |
| `cancel.eml` | iMIP `METHOD:CANCEL` with `STATUS:CANCELLED` and an incremented `SEQUENCE`. |
| `counter.eml` | An attendee's iTIP `METHOD:COUNTER` proposing a new time (CAL-17). |
| `teams-invite.eml` | A Teams meeting: `X-MICROSOFT-SKYPETEAMSMEETINGURL`, `X-MICROSOFT-ONLINEMEETINGCONFLINK`, and the join link in `DESCRIPTION`/`LOCATION`. |
| `importance-high.eml` | `Importance: High` + `X-Priority: 1 (Highest)` + `X-MSMail-Priority: High`, a `Sensitivity: Private`, and a `List-Id`. |
| `sensitivity-private.eml` | RFC 2156 `Sensitivity: Company-Confidential`. |

These are authored to the documented wire formats (`[MS-OXCICAL]`, `[MS-OXOMSG]`,
RFC 6047) and verified by `crates/mail/tests/outlook_fixtures.rs`, which parses
every one through the same code the app uses.

## Still to capture from a real Outlook install

Two formats are opaque binaries that can't be authored by hand faithfully and
must be captured and checked in:

- **`winmail.dat` (TNEF, `[MS-OXTNEF]`)** — receive a message sent from classic
  Outlook in Rich Text format to an internet recipient; save the
  `application/ms-tnef` part.
- **`*.msg` (`[MS-OXMSG]` over a CFB container)** — in Outlook, *File → Save As*
  a message as `.msg`.

To capture: send the relevant message, then either save the raw `.eml` (so the
whole MIME envelope, including the TNEF part, is preserved) or extract the
binary part, and add it here with a one-line table entry above. Keep anything
that could identify a real person or organisation out of the file, or redact
the addresses/subjects first.
