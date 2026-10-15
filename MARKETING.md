# Lookout

**Your email, calendar, people, and tasks — finally at home on GNOME.**

Email is still how the world gets things done. But while the rest of our lives moved on — personal inboxes and work accounts side by side, calendars, contacts, and to-do lists juggled without a second thought — mail clients on Linux mostly stayed stuck a decade in the past. Lookout closes that gap: a native GNOME client that brings the familiar, powerful Outlook-style experience to your desktop, built for the way we actually consume email today.

Every account, every message, every meeting, every contact and task — together in one place, presented the way you already know. Fast, native, private, and right at home on GNOME.

## Why Lookout

- **Familiar by design.** Three panes: folders on the left, messages in the middle, reading pane on the right — with a ribbon-style toolbar for the actions you reach for constantly. Used Outlook at work, or in a previous life? You already know how to use Lookout. Nothing to relearn.
- **Native GNOME, through and through.** Written in Rust on GTK 4 and libadwaita, Lookout looks, feels, and behaves like part of your desktop — because it is. Quick to start, quick to use, gentle on your battery.
- **Zero sign-in friction.** Accounts come from **GNOME Online Accounts**: add a Google, Microsoft 365, or Nextcloud account in your system Settings and Lookout picks it up automatically. It never touches your passwords and never makes you sign in twice.
- **Or add any account yourself.** Running your own mail server, or using a provider GNOME Online Accounts doesn't know about? Add it directly in Lookout with your IMAP/SMTP settings — your password goes straight into the GNOME keyring, and nowhere else.
- **One workspace, not five apps.** Mail, calendar, contacts, and tasks live in one application with one consistent interface. A meeting invite is right there in the email. A contact's number is a click away. Your tasks sit next to your schedule.
- **Instant, even offline.** Everything is cached locally. Folders you've visited paint instantly, messages you've read open instantly, and contacts and calendars are there even when your connection isn't. Lookout also warms up your mail in the background, so much of what you're about to open is already downloaded.
- **A good citizen of your mailbox.** Lookout speaks standard IMAP, SMTP, CalDAV, and CardDAV, and syncs with Outlook itself on Microsoft 365. Your mail, categories, flags, and rules live in your mailbox wherever your server supports them — not locked inside one app.
- **Private by default.** Messages render in a sandboxed web view, remote images stay blocked unless you say otherwise, and you can trust individual senders one at a time. Your mail is your business.

## Mail

- **All your accounts, live.** Multi-account IMAP sync keeps every folder current in real time — new mail simply appears, and if you're already sitting at the top of your inbox it arrives right where you're looking instead of pushing your place down the list. A unified **All Inboxes** view merges every account into one list when you want it.
- **Conversations.** Replies group under collapsible thread headers, so a busy discussion reads as one conversation, not a wall of messages — and the reading pane can show the whole thread as stacked cards.
- **Instant search that understands you.** Full-text search across every account, answered from a local index in milliseconds. Type plain words, or refine with `from:`, `to:`, `subject:`, `has:attachment`, `is:unread`, `category:`, `received:` and more — with clickable refiners, saved recent searches, and an advanced-search form when you'd rather point and click.
- **The full action set.** Delete to Trash, archive, report as junk, pin, mark read or unread, and snooze — applied to one message or a whole selection at once. Pinned messages float to their own section at the top of the list, always in reach, and snoozed mail waits in a **Scheduled** folder until you need it again.
- **Follow-up flags with dates.** Flag a message with Today, Tomorrow, This week, Next week, or any date — the flag turns red when it's overdue, sorts and groups by due date, and appears in the Tasks view. On Microsoft 365 your flags sync with Outlook's own.
- **Undo and schedule send.** Clicked send too soon? Undo it for a few seconds and your draft comes right back. Want it out tomorrow morning? Schedule the send and it waits as a draft until the moment you picked.
- **Stay organized your way.** Categories with Outlook's own colour palette (and two-way sync on Microsoft 365), a Favorites section for the folders you live in, smart per-role folder icons, and date-grouped lists that make old mail findable.
- **Mentions of you, surfaced.** When a message names you, it gets an `@` badge in the list — plus a **Mentions me** filter and a matching search phrase.
- **Tame the noise.** Report junk and not-junk with proper `$Junk` keywords so your server learns, block a sender or a whole domain, and let Lookout keep blocked mail out of your way automatically.
- **Know what's real.** Suspicious mail gets a warning banner when the sender fails authentication checks or hides behind a mismatched display name, mail from outside your organisation can be tagged as external, hovering a link shows where it really goes, and Report Phishing forwards the message and cleans it up for you.
- **Drag and drop.** Move messages onto any folder, drop them on a category to organize, or drag them right out of the window to save as `.eml`.
- **Papers, please.** Save every attachment at once — as individual files or one tidy zip. `winmail.dat` attachments from Outlook open like any other file. A polite reminder pops up if you mention an attachment and forget to add it.
- **Open it in its own window.** Double-click (or press Enter) to read a message in a separate window, view its full source and headers, and print it — a draft even reopens straight into the composer where you left off.
- **Polite, practical touches.** One-click unsubscribe from mailing lists, request and send read receipts, and export raw `.eml` files on demand.

## Compose

- **A composer that keeps up with you.** Rich HTML editing with formatted text, tables, links, and inline images — and a plain-text copy rides along automatically for recipients on older clients. Reply and forward drop your cursor right above the quoted message, ready for you to start typing, while forwarding can carry the original's attachments along or resend the whole message as an attachment.
- **The tools where your hands already are.** Compose in the reading pane and the formatting toolbar takes over the main action bar — bold, italic, lists, font sizes and colours, alignment, links, tables, images, emoji, and drag-and-drop files. Pop the composer out into its own window and the controls come with it.
- **Signatures and templates that fit the moment.** Rich signatures per account, with separate defaults for new mail and replies — and **My Templates** to drop in paragraphs you write over and over, from quick acknowledgements to full boilerplate.
- **Recipients, fast.** Type into To, Cc, or Bcc and get autocomplete from your address books and recent mail history as you go — or click To to open a searchable picker and check off everyone you need at once. Contact lists expand to their members with one click.
- **Send as you.** Multiple identities per account, with display names, Reply-To, and per-identity blind copies — ideal for work accounts, personal aliases, and mailing-list personas.
- **Say it the right way.** Mark a message high or low importance, set its sensitivity (Personal, Private, or Company-Confidential) with a matching banner on the receiving end, request a read receipt, and let the built-in spell checker follow your system language or one you choose.
- **Pop it out, keep working.** Pop the composer into its own window and carry on — it pops right back in whenever you like, and drafts autosave the whole time, so nothing is ever lost.
- **Turn it into a meeting.** Reply to an email as an event: Lookout opens a new meeting with the whole thread's participants already invited.

## Calendar

- **See your schedule your way.** Day, work week, week, month, **List**, and **Schedule** views, a split view, a mini-calendar sidebar, and an at-a-glance overview right on the Mail screen — including what's coming up over the next three months.
- **Schedule at the speed of thought.** Click any slot to start a new event, drag events to reschedule (hold Ctrl to copy instead), and drag an event's edge to resize it. Right-click for open, duplicate, copy to another calendar, save as `.ics`, categories, show-as, and privacy.
- **Your week, your rules.** Choose the first day of the week, your working days and hours, a second time zone in the grid, and even a different time zone per event — the calendar follows.
- **Recurrence done right.** Edit a whole series, just this occurrence, or this and everything after — recurring events can be moved, edited, and deleted individually.
- **Invitations, handled end to end.** Accept, tentative, or decline right from the email — sending your response with a comment, editing it first, or not replying at all — and Lookout updates your calendar for you. You can also propose a new time, forward the meeting to a colleague, and organize a meeting yourself: Lookout sends the invitations, tracks who accepted in a **Tracking** view, and sends updates and cancellations as things change. When a new invite lands, a conflict line and a mini day timeline show whether it clashes with your day.
- **Find a time that works.** The event editor's **Scheduling** tab pulls busy/free time from your server (or Microsoft 365) and suggests times that fit inside your working hours.
- **Join with one click.** Teams, Zoom, Google Meet, Webex, and Jitsi links in invitations are recognized and offered as a **Join** button — in the invite, the reminder, and the event peek.
- **Feeds, files, and holidays.** Subscribe to webcal calendars, import and export `.ics` files, add a holiday calendar for your country in one click, enjoy a synthesized **Birthdays** calendar built from your contacts, and print your month when you need it on paper.
- **Never miss a meeting.** Event alerts arrive as desktop notifications with Open, Snooze, and Dismiss actions — even for events that came due while the app was closed.

## People

- **Your contacts, fully manageable.** Create, edit, and delete contacts across all your CardDAV address books — synced, cached, and always current — including anniversaries, IM handles, websites, photos, and your own custom fields.
- **Organized.** Favourites, contact groups, per-account buckets, a Deleted list that survives restarts, and a **File as** preference (global or per contact) so names sort the way you think of them.
- **A card, wherever a person appears.** Hover or click any name — in a message, an invitation, or the contacts list — and a profile card shows their details, your recent conversations, upcoming shared events, and shortcuts to email or schedule a meeting.
- **Portable.** Import and export `.vcf` files and Outlook's own CSV format, with duplicate detection that updates instead of cluttering.
- **Everywhere you need them.** Contact autocomplete powers the composer, event invitations, and the People tab itself — one shared address book across the whole app, including contact lists that expand into their members.

## Tasks

- **Everything you owe the world, in one list.** Tasks sync from every calendar account, grouped into Overdue, Today, This week, Later, and Completed — and the same list appears on your dashboard and on the Mail screen.
- **Flagged email is a task list too.** Every message you've flagged for follow-up shows up in Tasks; tick it off there and the flag clears in your mail.
- **Built for capture.** One-click completion, a full editor for details, due dates, priorities, categories, and notes, plus a **Create task from message** action when an email becomes a to-do. Google Tasks accounts are supported alongside CalDAV and local lists.
- **Sorted your way.** Sort by due date, importance, title, or creation date; group by due bucket, list, or category; hide completed tasks when you want the clear picture.

## Rules and automatic replies

- **Rules that keep working when the app doesn't.** Build rules that move, copy, delete, flag, categorize, forward, or redirect mail — then let them run wherever they make the most sense: in Lookout, in your Microsoft 365 mailbox, or on your server (any Sieve-capable mail host). Create one from any message, run a folder's rules on demand, and see at a glance where each rule lives.
- **Out of office, handled.** Set automatic replies per account with an optional date range, and on Microsoft 365 even separate messages for inside and outside your organisation. Microsoft 365 sends them through Outlook's own settings; Sieve-capable servers send them through your server — so replies go out even while Lookout is closed. A banner in Mail reminds you when they're on, with one click to turn them off.

## Microsoft 365, all the way

Opt in to Outlook sync and your Microsoft 365 account gets a native calendar in Lookout — and keeps more of itself in step with Outlook: your **categories**, **follow-up flags**, **rules**, and **automatic replies** stay in sync through Microsoft Graph, and pins mirror across, with access you grant explicitly and can revoke any time.

## The Lookout dashboard

- **Your day at a glance.** A dedicated dashboard shows who you contact most, your email volume by time of day, outstanding tasks, and upcoming events — all live from your local data, updating as you work.
- **An assistant that knows your mailbox.** An optional AI chat is built in: point it at any OpenAI-compatible endpoint (your own server, or a hosted service), and ask questions about your mail, people, tasks, and events. The assistant only sees your data through a small set of read-only lookups — it never sends, deletes, or changes anything — and its API token lives in your keyring, never in your settings files.

## Privacy and security

- **Sandboxed by design.** Messages render in an isolated web view; remote images and content stay blocked until you choose to load them, with per-sender trust you manage one at a time.
- **Suspicious mail, flagged.** Sender-authentication results (SPF, DKIM, DMARC) are checked on arrival, display-name/address mismatches are called out, and mail from outside your organisation can be tagged as external.
- **Safe links.** Hovering a link shows its real destination, and Lookout asks before opening anything whose visible text and real address disagree.
- **Report phishing** in one action: the message is forwarded to the address you choose and moved out of your inbox.
- **Your credentials stay yours.** Passwords and tokens live in the GNOME keyring; mail, contacts, and calendars are cached locally for speed, never used for anything else.

## At home on your desktop

- **An eye on your inbox, even with the window closed.** An optional tray icon carries your total unread count and offers Open Lookout, Compose…, and Quit — and the app icon in your dock shows the same badge. Both are yours to switch off.

## Make it yours

- **Themes.** System, Flat Dark, or Flat Light — or match your GNOME colour scheme with a custom accent colour.
- **Your background.** Choose your own window background artwork.
- **The layout you like.** Reading pane on the right, at the bottom, or off; loose, medium, or tight spacing; up to three lines of message preview.
- **Your shortcuts.** Every action has a keyboard shortcut, and every shortcut is remappable — click to record a new key, with conflict checking and one-click restore to defaults.

## Availability

Lookout is packaged for modern Linux desktops — Flatpak, snap, `.deb`, and `.rpm` — with a CI pipeline that builds every release. Free and open source, under the GPL-3.0.

---

**Your email, your calendar, your people, your tasks — finally at home on GNOME.**
