# MAIL_GRAPH_OVERVIEW — Phase 5, item 5.7 (Optional Graph mail transport)

**Status:** plan / assessment. 5.7.1 partial (offline only) and 5.7.2 landed as unwired library code; the rest is not implemented.
**Covers:** [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) §4.4, §5.1, §5.6, §7 item 5.7; ID **A-06** (Microsoft 365 mail).
**Companion docs:** [OUTLOOK_PARITY.md](OUTLOOK_PARITY.md) (parity plan), [SCAFFOLD.md](SCAFFOLD.md) (architecture), [CHANGELOG.md](CHANGELOG.md) (history).

> OUTLOOK_PARITY.md §7 item 5.7 points here. This file records the assessment of
> that item and, if the deferral is ever lifted, the task breakdown to implement it.

---

## 1. What item 5.7 is

> **5.7 Optional Graph mail transport** (A-06) — *"Only if 5.4 proves too
> costly: full Graph mail sync as an alternative to IMAP for Microsoft 365
> accounts."* Effort: **L**.

Today a Microsoft 365 account is **mail over IMAP/SMTP** with Lookout's own
XOAUTH2 (`source/crates/app/src/microsoft_oauth.rs`), and Graph is layered on top
for the things Exchange IMAP cannot carry or expose:

| Feature | How it works today | Where |
|---|---|---|
| Categories (M-09) | local `local_categories` overlay + Graph `categories` PATCH | item 5.4 |
| Follow-up flag (M-10) | `$Lookout-flag-*` keywords, or the `follow_up_flags` cache table on Exchange, mirrored to Graph `flag` | items 1.16, 5.4 |
| Importance (M-12) | `local_importance` overlay + Graph `importance` | item 5.4 |
| Focused / Other (M-03) | `local_inference` + `inference_overrides` + Graph `inferenceClassification` | items 2.12, 5.4 |
| Rules (M-14) | client / Sieve / Graph `messageRules` | item 2.5 |
| Automatic replies (M-24) | Graph `automaticRepliesSetting` / Sieve `vacation` | item 2.11 |
| Schedule send (C-08) | Graph `sendMail` + `PidTagDeferredSendTime`, local outbox fallback | items 1.14, 5.5 |
| Shared mailboxes (M-42) | add-by-address over IMAP (login form unverified) + Graph folder/send-as transport ready | item 5.6 |
| Calendar / contacts / To Do | native Graph actors (calendar, contacts, todo) | items 5.1–5.3 |

**5.7 is a conditional fallback.** The condition ("only if 5.4 proves too
costly") has **not fired**: 5.4 landed across five sprints (transport → cache
overlays → session seam → app wiring → Focused/Importance UI) and A-06 is
marked ✅. The layered IMAP + Graph design is working.

## 2. Recommendation

**Keep 5.7 deferred.** Do not implement a full Graph mail sync now.

Reasons:

1. The trigger in the item's own text has not fired — 5.4 was not too costly.
2. A Graph transport is **strictly worse for real-time delivery**: Graph change
   notifications require a **publicly accessible HTTPS `notificationUrl`**, which
   a desktop app cannot host (Microsoft's `/subscriptions` docs). So Graph mail is
   **delta polling only**, a latency regression against today's dedicated IDLE
   connection (`run_idle_supervisor`).
3. Graph's message query surface is far weaker than IMAP + local FTS5:
   a per-folder delta supports only `$filter=receivedDateTime ge|gt …`,
   `$orderby=receivedDateTime desc`, and **no `$search`**.
4. The identity model (`Uid`/`UidValidity`) is IMAP-shaped and threaded through
   every command; replacing it is the largest single source of churn for
   uncertain benefit.

**Revisit if a concrete trigger appears**, e.g.:

- The Exchange Online shared-mailbox IMAP login (M-42) proves unusable in
  practice; or
- Microsoft further restricts IMAP on Exchange Online (keywords, `MOVE`, IDLE);
  or
- Real-time sync is judged unnecessary and a polling transport is acceptable.

### 2.1 Spike result so far (item 5.7.1, offline only)

5.7.1 was run **offline** — no live Microsoft 365 tenant was available — so its
two evidence questions remain open:

- **(a) MIME fidelity** and **(b) delta paging cost** could **not** be measured
  against Graph. The spike landed the harness to measure them (§4, 5.7.1) and
  its wiremock proof, but the go/no-go **cannot** be settled from fixtures alone:
  the question is whether Graph's `/$value` bytes match the IMAP body, and only
  a real mailbox can answer it.
- **(c) Trigger review** — answered from Microsoft's published guidance rather
  than a live test, and it **confirms the trigger has not fired**: Microsoft is
  retiring **Basic authentication** for IMAP/POP/SMTP (final SMTP AUTH deadline
  **30 April 2026**), **not** the IMAP protocol; OAuth `XOAUTH2` — which Lookout
  already uses — stays the supported path, and no Exchange Online restriction of
  IMAP keywords, `MOVE` or IDLE is announced. The one unverified trigger
  candidate is unchanged: the shared-mailbox IMAP login form (M-42).

**Net: 5.7 stays deferred, and 5.7.1 is complete only as far as a tenant-less
spike can take it.** Re-running (a) and (b) against a tenant is what would move
the decision.

## 3. Architecture if greenlit

Mirror the existing Graph/DAV actors: a **second account-session actor** speaking
the **same `AccountCommand` / `AccountEvent` contract** as
`lookout_mail::session::run_account_session`, selected **per account** at connect
time, so the app's event loop, message list, reading pane, composer and outbox
are untouched.

```
source/crates/app/src/window/connect.rs
  └─ if account is Microsoft 365 AND graph mail opted in
        worker.spawn(lookout_graph::mail::run_mail_session(client, cmd_rx, interactive_cmd_rx, evt_tx))
     else
        worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, interactive_cmd_rx, evt_tx))
```

Key properties:

- One protocol seam (`AccountCommand`/`AccountEvent`), reused verbatim — §10
  "Protocol seam first".
- Graph is the transport of record for bodies **and** properties; the 5.4
  overlay tables (`local_categories`, `local_importance`, `local_inference`)
  become redundant on this transport.
- The local outbox / scheduler (item 0.3) still owns undo send, schedule send
  and the offline outbox; Graph `sendMail` + `PidTagDeferredSendTime` remains the
  server-side path (item 5.5).
- Native writes replace the overlay+mirror dance: `isRead`, `flag`,
  `categories`, `importance`, `inferenceClassification`, `move`, `copy`,
  `delete` are all direct Graph PATCH/POST.

## 4. Task breakdown

Effort: **S** ≤ 1 day · **M** ≤ 1 week · **L** multi-week.

| # | Task | Scope | Effort |
|---|---|---|---|
| 5.7.1 | **Spike + go/no-go** — 🟡 partial (offline only) | **Landed:** the harness to measure the two evidence questions — `MailClient::get_mime` (`GET /me/messages/{id}/$value`, `Prefer: IdType="ImmutableId"`), `MailClient::folder_delta` (`GET /me/mailFolders/{id}/messages/delta`) and `GraphClient::get_bytes_with_headers`, with wiremock tests and the `test-fixtures/outlook/messages-delta.json` page fixture. Question (c) is answered from Microsoft's published guidance (§2.1): Basic auth is going, IMAP is not. **Outstanding (needs a live tenant):** (a) `/$value` MIME fidelity for an iMIP invite, a TNEF message and an `Authentication-Results` message — diffed against the IMAP body; (b) delta paging cost on a large mailbox and folder. Capture those three `.eml` fixtures when a tenant is available. | S |
| 5.7.2 | **Graph mail transport** — ✅ transport only, not wired | **Landed** in `source/crates/graph/src/mail/` as dormant library code: nothing calls it yet. **Operations** (`MailClient`): `own_mail_folders_delta` (the whole hierarchy through `/me/mailFolders/delta`, since the plain list is top-level only), `well_known_folders`, `folder_messages_delta` at row weight, `get_message` (HTML body + headers), `move_message`/`copy_message`/`delete_message`, `update_message` (one `MessagePatch` for isRead / flag / categories / importance / inferenceClassification), `list_attachments` + `get_attachment_bytes`, `create_draft`/`update_draft`/`create_reply`/`create_reply_all`/`create_forward`/`send_draft`, `create_folder`/`rename_folder`/`delete_folder`, and `search_messages` (`$search`, unordered, capped). `GraphError::is_delta_reset` flags an expired cursor (410). **Pure mappers** (`mail/message.rs`): `MailFolder` (the 5.6 `SharedMailFolder` is now an alias of it) → `Mailbox`, `GraphMessage` → `EmailSummary`, attachments → `BodyPart`, the JSON body → `EmailBody`, with the `MailboxId`/`Uid` passed in so 5.7.3 owns the identity model. **Inputs for later tasks:** a part's `part_number` is the Graph attachment id (5.7.3/5.7.5); the thread key is `graph:{conversationId}` because Graph summaries carry no `References` (5.7.4 must keep it); `size` is 0 and Pin has no Graph counterpart (5.7.7); `calendar_ics` needs the MIME read (R3). Covered by wiremock tests per operation and four synthetic fixtures (`mail-folders-delta.json`, `message-summary-delta.json`, `message-full.json`, `message-attachments.json`). | M |
| 5.7.3 | **Identity model** | Define `MailboxId = "{account}:{graph folder id}"` and a **stable synthetic `Uid(u32)`** assigned from a cached Graph-immutable-id → u32 map (reuse the 5.4 `message_graph_ids` idea). Decide `UidValidity` semantics on a delta reset (bump to invalidate). Document in SCAFFOLD §3/§4. **Risk centre.** | M |
| 5.7.4 | **Cache strategy** | Either feed the existing `lookout_mail::Cache` synthetic summaries/bodies (keeps FTS5, conversation keys, overlays; may need a `PRAGMA user_version` migration), or add a separate Graph cache file. Prefer reuse to avoid duplicating search/conversation logic. | M |
| 5.7.5 | **Session actor** | `run_graph_mail_session`: initial folder + folder-delta sync, incremental per-folder delta (30–60 s poll), on-demand body/attachment fetch, native property writes, move/copy/revert, snooze, junk, drafts, outbox drain via `sendMail`, search. Reuse existing rules / OOF / schedule-send Graph glue. Emit the existing event variants. | L |
| 5.7.6 | **App wiring + opt-in** | Per-account **"Use Graph mail (experimental)"** switch in Config → Accounts (`settings.json` list + `AppConfig` helpers, like `directory_accounts`). `connect.rs` picks the session. Disable/hide IMAP-only surfaces: Sieve rules, keyword-driven snooze/`$Junk`/`$Forwarded`, capability probes (`SieveAdvertised`), IMAP account fields. | M |
| 5.7.7 | **Feature-gap reconciliation** | Walk §5.1 row by row: native / degraded / unavailable. Update status marks and the §2.1 scorecard. Confirm each stays a §6.11 UX-checklist pass (wording, defaults, undo, empty/error states). | M |
| 5.7.8 | **Refresh cadence** | Delta polling interval + `/me/messages/delta` cursor persisted in the cache. Document the no-IDLE latency trade-off in the doc/README. Keep the local outbox for the offline path. | S |
| 5.7.9 | **Tests** | Wiremock transport tests per endpoint; fixture round-trips (folders, delta page, message with attachments, MIME, invite); a fake-Graph session test; a `tests/` integration harness paralleling `source/crates/mail/tests/imap_integration.rs`. | M |
| 5.7.10 | **Close-out** | One CHANGELOG entry citing 5.7 / A-06; flip 5.7 to ✅ and update §2.1/§4.4/§5.1/§7; README; mark this file done. | S |

**Order:** 5.7.1 (gate) → 5.7.2 ∥ 5.7.3 → 5.7.4 → 5.7.5 → 5.7.6 → 5.7.7 → 5.7.8 → 5.7.9 → 5.7.10.

**Dependencies (all landed):** 0.3 outbox, 0.7 `GraphClient`, 5.4 delta + property
mapping + Message-ID map, 5.5 `sendMail`/deferred send, 5.6 shared-mailbox
folder/send-as transport. New consent must stay **incremental per feature**
(§4.4): Graph mail reuses `Mail.ReadWrite` / `Mail.Send` (shared variants only
for shared mailboxes).

## 5. Risks and hard blockers

| # | Risk / blocker | Impact | Mitigation / decision |
|---|---|---|---|
| R1 | **No push.** Change notifications need a public HTTPS `notificationUrl`; desktop apps cannot host one. | Sync is poll-only; new-mail latency regresses vs IDLE. | Accept as a known regression, or **don't do 5.7**. Interval polling only. |
| R2 | **Search.** Delta/`$filter` support a tiny subset; no `$search`. | Remote search can't match `search_query.rs`. | Rely on local FTS5 cache (as now); document the gap. |
| R3 | **MIME fidelity.** Invite card, TNEF, `Authentication-Results`, read receipts, inline `cid:` depend on exact MIME. | CAL/M-45/S-03/read-receipt features could break on Graph bodies. | Gate on the 5.7.1 spike with fixtures. If MIME differs materially, stay on IMAP. **Still unvalidated** — 5.7.1 ran without a tenant (§2.1). |
| R4 | **Identity model churn.** `Uid`/`UidValidity` are IMAP-shaped and used everywhere. | Large blast radius; undo/conversation/flagged/outbox correctness. | 5.7.3 as an isolated design step with tests before any feature wiring. |
| R5 | **Throttling / cost.** Per-message body and attachment round trips. | Large mailboxes hit Graph throttling; slower first paint. | Batch, `$select` narrowly, rely on the client's `Retry-After` handling (already present). **Cost still unmeasured** — question (b) needs a tenant (§2.1). |
| R6 | **Feature regressions.** Sieve, `$Junk`/`$NotJunk`, keyword snooze, IMAP capability probes have no Graph equivalent. | Some M-rows degrade on Microsoft 365. | 5.7.7 reconciliation; hide/disable clearly (§10 "degrade softly"). |
| R7 | **Spike can't reach go/no-go without a tenant.** 5.7.1's evidence questions (a) and (b) need a real Microsoft 365 mailbox; fixtures and wiremock pin the shape, not Graph's actual bytes or latency. | 5.7 stays deferred on an assumption rather than measured evidence. | Run the landed harness (`MailClient::get_mime`/`folder_delta`) against a tenant and capture the three `.eml` fixtures before any green-light. |

## 6. Non-goals

- Server-side push / notifications (blocked, R1).
- Remote `$search` and full IMAP `SEARCH` parity (blocked, R2).
- Sieve / ManageSieve over Graph.
- Reproducing IMAP keyword fidelity where Graph has no equivalent.
- Exchange on-premises (EWS) — §9 non-goal.

## 7. Verification checklist (per §6.11 / §10)

- [ ] Commands/events route through the existing `AccountCommand`/`AccountEvent`
      contract; no server call from the UI.
- [ ] Interop strings (Graph property names, OData filters) live in
      `core/src/interop/` or `lookout_graph`, never inline in feature code.
- [ ] Cache migrations note any format change in the CHANGELOG ("no data-format
      change" otherwise).
- [ ] Missing capability degrades to a hidden UI element or a client fallback,
      never an error.
- [ ] New fixtures under `source/test-fixtures/outlook/` for every artefact.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, workspace tests green.
- [ ] §6.11 UX checklist passes for any user-facing change.
