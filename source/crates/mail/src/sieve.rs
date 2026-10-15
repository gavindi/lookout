//! Rule → Sieve script compilation (OUTLOOK_PARITY.md §4.5, roadmap item 2.5,
//! the Sieve backend).
//!
//! A Sieve script (RFC 5228) is the non-Microsoft equivalent of a client rule:
//! uploaded to a ManageSieve server (RFC 5804), it filters mail in the mailbox
//! whether or not Lookout is running. This module is the pure half of the
//! backend - it turns the shared [`lookout_core::Rule`]s into a script and
//! reports which rules Sieve can't express - and is exercised without any
//! network. The transport lives in [`crate::managesieve`].
//!
//! Only `Sieve`-backend, enabled rules are compiled. A rule whose conditions
//! or actions have no Sieve form (`[`crate::sieve::is_sieve_expressible`] is
//! false) is skipped, with a comment in the script, so the rest still apply -
//! the same "leave it as a client rule" behaviour the Graph backend uses.
//!
//! The same script also carries the account's automatic replies (roadmap item
//! 2.11) as an RFC 5230 `vacation` block, since ManageSieve activates exactly
//! one script: [`build_sieve_script`] merges the two, and the vacation block
//! runs before the rules so a `stop;` rule can't suppress it.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::BTreeSet;

use lookout_core::{
    sanitize_tag_key, tag_keyword, AutomaticReplies, AutomaticReplyStatus, FlagStatus, Importance, Rule, RuleAction, RuleBackend, RuleCondition, RuleTarget, SizeComparison,
    FLAG_DONE_KEYWORD, FLAG_KEYWORD,
};

/// The script name Lookout owns on the server. Its entries are wholly managed
/// by Lookout; a user's other scripts are never touched.
pub const SCRIPT_NAME: &str = "lookout";

/// Whether Sieve can express a whole rule. A rule with any unexpressible
/// condition or action is left to the client backend instead.
pub fn is_sieve_expressible(rule: &Rule) -> bool {
    rule.conditions.iter().all(condition_expressible) && rule.exceptions.iter().all(condition_expressible) && rule.actions.iter().all(|action| action_to_sieve(action).is_some())
}

/// Whether one condition has a Sieve test.
fn condition_expressible(condition: &RuleCondition) -> bool {
    condition_to_sieve(condition).is_some()
}

/// The Sieve quotation of a string value, escaping `"` and `\`.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// A `header :contains` test.
fn header_test(name: &str, value: &str) -> String {
    format!("header :contains {} {}", quoted(name), quoted(value))
}

/// The word the importance condition writes.
fn importance_word(importance: Importance) -> &'static str {
    match importance {
        Importance::High => "High",
        Importance::Low => "Low",
        Importance::Normal => "Normal",
    }
}

/// The `fileinto`/`redirect` target text for a move/copy action. Roles map to
/// their conventional folder names; a named folder is used verbatim.
fn target_folder(target: &RuleTarget) -> String {
    match target {
        RuleTarget::Role(role) => match role {
            lookout_core::MailboxRole::Inbox => "INBOX",
            lookout_core::MailboxRole::Sent => "Sent",
            lookout_core::MailboxRole::Drafts => "Drafts",
            lookout_core::MailboxRole::Trash => "Trash",
            lookout_core::MailboxRole::Junk => "Junk",
            lookout_core::MailboxRole::Archive => "Archive",
            lookout_core::MailboxRole::Custom => "Archive",
        }
        .to_string(),
        RuleTarget::Folder(name) => name.clone(),
    }
}

/// The Sieve test for a condition, or `None` when it has no Sieve form.
fn condition_to_sieve(condition: &RuleCondition) -> Option<String> {
    Some(match condition {
        RuleCondition::From(text) => header_test("From", text),
        RuleCondition::To(text) => header_test("To", text),
        RuleCondition::Cc(text) => header_test("Cc", text),
        RuleCondition::Subject(text) => header_test("Subject", text),
        // The `body` extension's substring test.
        RuleCondition::Body(text) => format!("body :contains {}", quoted(text)),
        // A named header maps directly; Graph's nameless `headerContains` has
        // no Sieve form.
        RuleCondition::Header { name, value } if !name.trim().is_empty() => header_test(name, value),
        RuleCondition::Header { .. } => return None,
        RuleCondition::ListId(text) => header_test("List-Id", text),
        // Sieve has no attachment test.
        RuleCondition::HasAttachment(_) => return None,
        RuleCondition::Importance(importance) => header_test("Importance", importance_word(*importance)),
        RuleCondition::Size { comparison, bytes } => match comparison {
            SizeComparison::GreaterThan => format!("size :over {}", bytes.saturating_add(1)),
            SizeComparison::GreaterOrEqual => format!("size :over {}", bytes),
            SizeComparison::LessThan => format!("size :under {}", bytes),
            SizeComparison::LessOrEqual => format!("size :under {}", bytes.saturating_add(1)),
            SizeComparison::Equal => format!("allof (size :over {}, size :under {})", bytes.saturating_sub(1), bytes.saturating_add(1)),
        },
        // "Sent only to me" needs the owner's address, which compilation
        // doesn't carry; left to the client.
        RuleCondition::SentOnlyToMe => return None,
    })
}

/// The Sieve statements and required extensions an action needs, or `None`
/// when Sieve has no form for it.
fn action_to_sieve(action: &RuleAction) -> Option<(Vec<String>, Vec<&'static str>)> {
    Some(match action {
        RuleAction::Move(target) => (vec![format!("fileinto {};", quoted(&target_folder(target)))], vec!["fileinto"]),
        RuleAction::Copy(target) => (vec![format!("fileinto :copy {};", quoted(&target_folder(target)))], vec!["fileinto", "copy"]),
        RuleAction::Delete => (vec!["discard;".to_string()], vec![]),
        RuleAction::Flag(flag) => {
            let mut statements = Vec::new();
            match flag.status {
                FlagStatus::Flagged => {
                    statements.push(format!("addflag {};", quoted(FLAG_KEYWORD)));
                    if let Some(due) = flag.due {
                        statements.push(format!("addflag {};", quoted(&lookout_core::flag_due_keyword(due))));
                    }
                }
                FlagStatus::Complete => statements.push(format!("addflag {};", quoted(FLAG_DONE_KEYWORD))),
                FlagStatus::NotFlagged => {
                    statements.push(format!("removeflag {};", quoted(FLAG_KEYWORD)));
                    statements.push(format!("removeflag {};", quoted(FLAG_DONE_KEYWORD)));
                }
            }
            (statements, vec!["imap4flags"])
        }
        RuleAction::Categorize(names) => {
            let statements = names.iter().map(|name| format!("addflag {};", quoted(&tag_keyword(&sanitize_tag_key(name))))).collect();
            (statements, vec!["imap4flags"])
        }
        RuleAction::MarkRead(true) => (vec!["addflag \"\\\\Seen\";".to_string()], vec!["imap4flags"]),
        RuleAction::MarkRead(false) => (vec!["removeflag \"\\\\Seen\";".to_string()], vec!["imap4flags"]),
        // Sieve `redirect` resends without changing the sender, which serves
        // both forward and redirect.
        RuleAction::Forward(to) | RuleAction::Redirect(to) => {
            let statements = to.iter().map(|address| format!("redirect {};", quoted(&address.address))).collect();
            (statements, vec![])
        }
        RuleAction::StopProcessing => (vec!["stop;".to_string()], vec![]),
    })
}

/// Combines tests with `allof`/`anyof`, collapsing a single test and using
/// `true` for none.
fn combine(tests: &[String], any: bool) -> String {
    match tests.len() {
        0 => "true".to_string(),
        1 => tests[0].clone(),
        _ => format!("{} ({})", if any { "anyof" } else { "allof" }, tests.join(", ")),
    }
}

/// The `if` test for a rule: every condition, and no exception.
fn rule_test(rule: &Rule) -> Option<String> {
    let mut conditions: Vec<String> = rule.conditions.iter().map(condition_to_sieve).collect::<Option<Vec<_>>>()?;
    // With no positive conditions, an `allof` would be vacuous; `true` keeps
    // the exception check meaningful.
    if conditions.is_empty() {
        conditions.push("true".to_string());
    }
    if !rule.exceptions.is_empty() {
        let exceptions: Vec<String> = rule.exceptions.iter().map(condition_to_sieve).collect::<Option<Vec<_>>>()?;
        conditions.push(format!("not {}", combine(&exceptions, true)));
    }
    Some(combine(&conditions, false))
}

/// The Sieve string for arbitrary text: a quoted string when it fits on one
/// line, else an RFC 5228 multi-line `text:` literal (a quoted string can't
/// contain a raw newline). `.`-stuffing is applied and CRLF endings emitted,
/// and the content lines start at column 0 - leading whitespace would become
/// part of the message.
fn text_literal(value: &str) -> String {
    if !value.contains(['\r', '\n']) {
        return quoted(value);
    }
    let mut out = String::from("text:");
    for line in value.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        out.push_str("\r\n");
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }
    out.push_str("\r\n.\r\n");
    out
}

/// The Sieve block and required extensions for an account's automatic replies,
/// or `None` when they're off (or a scheduled range is incomplete). The
/// `vacation` action brings RFC 5230's own loop protection - it skips
/// auto-submitted and bulk mail - and `:days 1` throttles replies to one per
/// sender per day. No `:subject` is emitted: a fixed subject would lose the
/// original one, and prefixing it needs the optional `variables` extension,
/// so the server's own reply subject is used instead.
fn vacation_to_sieve(vacation: &AutomaticReplies) -> Option<(String, Vec<&'static str>)> {
    match vacation.status {
        AutomaticReplyStatus::Disabled => None,
        AutomaticReplyStatus::AlwaysEnabled => {
            let message = text_literal(&vacation.internal_message);
            Some((format!("# Automatic replies\nvacation :days 1 {message};"), vec!["vacation"]))
        }
        AutomaticReplyStatus::Scheduled => {
            let (start, end) = (vacation.start?, vacation.end?);
            if end <= start {
                return None;
            }
            // The `date` extension's `currentdate` test; the `:value` form's
            // relational matches need the `relational` extension.
            let start = start.format("%Y-%m-%dT%H:%M:%SZ");
            let end = end.format("%Y-%m-%dT%H:%M:%SZ");
            let message = text_literal(&vacation.internal_message);
            let block = format!(
                "# Automatic replies until {end}\nif allof (currentdate :value \"ge\" \"iso8601\" \"{start}\",\n    currentdate :value \"le\" \"iso8601\" \"{end}\") {{\n    vacation :days 1 {message};\n}}"
            );
            Some((block, vec!["vacation", "date", "relational"]))
        }
    }
}

/// Compiles the enabled, `Sieve`-backend rules - plus the account's automatic
/// replies, when configured - into one script. Rules Sieve can't express are
/// skipped and named in a comment; the rest still apply.
pub fn build_sieve_script(rules: &[Rule], vacation: Option<&AutomaticReplies>) -> String {
    let mut requirements: BTreeSet<&'static str> = BTreeSet::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    let vacation = vacation.and_then(vacation_to_sieve);
    if let Some((_, vacation_requires)) = &vacation {
        requirements.extend(vacation_requires.iter().copied());
    }

    for rule in rules.iter().filter(|rule| rule.enabled && rule.backend == RuleBackend::Sieve) {
        if !is_sieve_expressible(rule) {
            skipped.push(rule.name.clone());
            continue;
        }
        let Some(test) = rule_test(rule) else {
            skipped.push(rule.name.clone());
            continue;
        };
        let mut statements: Vec<String> = Vec::new();
        for action in &rule.actions {
            let Some((mut action_statements, action_requires)) = action_to_sieve(action) else {
                statements.clear();
                break;
            };
            for requirement in action_requires {
                requirements.insert(requirement);
            }
            statements.append(&mut action_statements);
        }
        if statements.is_empty() {
            skipped.push(rule.name.clone());
            continue;
        }
        // The `body` test and `fileinto`/`copy` actions carry their own
        // requirements; collect the condition side too.
        if rule
            .conditions
            .iter()
            .chain(rule.exceptions.iter())
            .any(|condition| matches!(condition, RuleCondition::Body(_)))
        {
            requirements.insert("body");
        }
        let body = statements.join("\n    ");
        blocks.push(format!("# {}\nif {} {{\n    {}\n}}", sanitize_comment(&rule.name), test, body));
    }

    let mut script = String::from("# Script generated by Lookout - do not edit by hand.\n");
    if !requirements.is_empty() {
        let list = requirements.iter().map(|name| quoted(name)).collect::<Vec<_>>().join(", ");
        script.push_str(&format!("require [{list}];\n"));
    }
    if let Some((block, _)) = &vacation {
        script.push('\n');
        script.push_str(block);
        script.push('\n');
    }
    if blocks.is_empty() {
        if vacation.is_none() {
            script.push_str("# No rules are set to run on the server.\n");
        }
    } else {
        script.push('\n');
        script.push_str(&blocks.join("\n\n"));
        script.push('\n');
    }
    if !skipped.is_empty() {
        script.push_str("\n# Left to the client (Sieve can't express them): ");
        script.push_str(&skipped.join(", "));
        script.push('\n');
    }
    script
}

/// [`build_sieve_script`] with no automatic replies (rule compilation alone).
pub fn rules_to_sieve(rules: &[Rule]) -> String {
    build_sieve_script(rules, None)
}

/// Strips anything that would break a Sieve comment (newlines).
fn sanitize_comment(name: &str) -> String {
    name.replace(['\r', '\n'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{EmailAddress, FollowUpFlag, MailboxRole, RuleTarget};

    fn sieve_rule(name: &str) -> Rule {
        let mut rule = Rule::new("r", name);
        rule.backend = RuleBackend::Sieve;
        rule
    }

    #[test]
    fn compiles_a_move_rule() {
        let mut rule = sieve_rule("Archive newsletters");
        rule.conditions = vec![RuleCondition::From("news@example.com".into())];
        rule.actions = vec![RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))];
        let script = rules_to_sieve(&[rule]);
        assert!(script.contains("require [\"fileinto\"];"));
        assert!(script.contains("if header :contains \"From\" \"news@example.com\" {"));
        assert!(script.contains("fileinto \"Archive\";"));
    }

    #[test]
    fn exceptions_become_not_anyof_and_stop_becomes_stop() {
        let mut rule = sieve_rule("File project");
        rule.conditions = vec![RuleCondition::Subject("Project".into()), RuleCondition::To("me@example.com".into())];
        rule.exceptions = vec![RuleCondition::From("vip@example.com".into())];
        rule.actions = vec![RuleAction::MarkRead(true), RuleAction::StopProcessing];
        let script = rules_to_sieve(&[rule]);
        assert!(script.contains("allof (header :contains \"Subject\" \"Project\", header :contains \"To\" \"me@example.com\", not header :contains \"From\" \"vip@example.com\")"));
        assert!(script.contains("require [\"imap4flags\"];"));
        assert!(script.contains("addflag \"\\\\Seen\";"));
        assert!(script.contains("stop;"));
    }

    #[test]
    fn flags_categories_and_size() {
        let mut rule = sieve_rule("Flag big");
        rule.conditions = vec![RuleCondition::Size {
            comparison: SizeComparison::GreaterThan,
            bytes: 1024 * 1024,
        }];
        rule.actions = vec![
            RuleAction::Flag(FollowUpFlag {
                status: FlagStatus::Flagged,
                ..FollowUpFlag::default()
            }),
            RuleAction::Categorize(vec!["Newsletters".into()]),
        ];
        let script = rules_to_sieve(&[rule]);
        assert!(script.contains("size :over 1048577"));
        assert!(script.contains("addflag \"$Lookout-flagged\";"));
        assert!(script.contains("addflag \"$Lookout-tag-newsletters\";"));
    }

    #[test]
    fn unexpressible_rules_are_skipped_with_a_note() {
        let mut good = sieve_rule("Good");
        good.conditions = vec![RuleCondition::From("a@example.com".into())];
        good.actions = vec![RuleAction::Delete];
        let mut bad = sieve_rule("Has attachment");
        bad.conditions = vec![RuleCondition::HasAttachment(true)];
        bad.actions = vec![RuleAction::Delete];
        let mut client = Rule::new("c", "Client only");
        client.backend = RuleBackend::Client;
        client.conditions = vec![RuleCondition::From("b@example.com".into())];
        client.actions = vec![RuleAction::Delete];

        let script = rules_to_sieve(&[good, bad, client]);
        assert!(script.contains("# Good"));
        assert!(script.contains("discard;"));
        assert!(!script.contains("# Has attachment"));
        assert!(script.contains("Left to the client (Sieve can't express them): Has attachment"));
        assert!(!script.contains("# Client only"));
    }

    #[test]
    fn empty_rule_set_is_a_valid_script() {
        let script = rules_to_sieve(&[]);
        assert!(script.contains("do not edit by hand"));
        assert!(script.contains("No rules are set to run on the server"));
    }

    #[test]
    fn redirect_and_copy() {
        let mut rule = sieve_rule("Copy and forward");
        rule.conditions = vec![RuleCondition::ListId("list.example.com".into())];
        rule.actions = vec![
            RuleAction::Copy(RuleTarget::Folder("Projects".into())),
            RuleAction::Forward(vec![EmailAddress::new("team@example.com")]),
        ];
        let script = rules_to_sieve(&[rule]);
        assert!(script.contains("require [\"copy\", \"fileinto\"];"));
        assert!(script.contains("fileinto :copy \"Projects\";"));
        assert!(script.contains("redirect \"team@example.com\";"));
        assert!(script.contains("header :contains \"List-Id\" \"list.example.com\""));
    }

    #[test]
    fn always_on_vacation_compiles_to_a_vacation_action() {
        let vacation = AutomaticReplies {
            status: AutomaticReplyStatus::AlwaysEnabled,
            internal_message: "Away until Friday.".into(),
            ..AutomaticReplies::default()
        };
        let script = build_sieve_script(&[], Some(&vacation));
        assert!(script.contains("require [\"vacation\"];"));
        assert!(script.contains("vacation :days 1 \"Away until Friday.\";"));
        assert!(script.contains("# Automatic replies"));
        assert!(!script.contains("No rules are set to run on the server"));
    }

    #[test]
    fn scheduled_vacation_guards_on_the_current_date() {
        use chrono::{TimeZone, Utc};
        let vacation = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            start: Some(Utc.with_ymd_and_hms(2026, 10, 15, 9, 0, 0).unwrap()),
            end: Some(Utc.with_ymd_and_hms(2026, 10, 20, 17, 0, 0).unwrap()),
            internal_message: "Back next week.".into(),
            ..AutomaticReplies::default()
        };
        let script = build_sieve_script(&[], Some(&vacation));
        assert!(script.contains("require [\"date\", \"relational\", \"vacation\"];"));
        assert!(script.contains("currentdate :value \"ge\" \"iso8601\" \"2026-10-15T09:00:00Z\""));
        assert!(script.contains("currentdate :value \"le\" \"iso8601\" \"2026-10-20T17:00:00Z\""));
        assert!(script.contains("vacation :days 1 \"Back next week.\";"));
    }

    #[test]
    fn disabled_or_incomplete_vacation_emits_nothing() {
        let disabled = AutomaticReplies::default();
        let script = build_sieve_script(&[], Some(&disabled));
        assert!(!script.contains("vacation"));
        assert!(script.contains("No rules are set to run on the server"));

        let incomplete = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            ..AutomaticReplies::default()
        };
        assert!(!build_sieve_script(&[], Some(&incomplete)).contains("vacation"));
    }

    #[test]
    fn multiline_vacation_message_becomes_a_text_literal() {
        let vacation = AutomaticReplies {
            status: AutomaticReplyStatus::AlwaysEnabled,
            internal_message: "Out of office.\n\n--\nGavin\n.leading dot".into(),
            ..AutomaticReplies::default()
        };
        let script = build_sieve_script(&[], Some(&vacation));
        assert!(
            script.contains("vacation :days 1 text:\r\nOut of office.\r\n\r\n--\r\nGavin\r\n..leading dot\r\n.\r\n;"),
            "script:\n{script}"
        );
    }

    #[test]
    fn vacation_runs_before_the_rules() {
        let mut rule = sieve_rule("Archive");
        rule.conditions = vec![RuleCondition::From("news@example.com".into())];
        rule.actions = vec![RuleAction::Move(RuleTarget::Folder("News".into())), RuleAction::StopProcessing];
        let vacation = AutomaticReplies {
            status: AutomaticReplyStatus::AlwaysEnabled,
            internal_message: "Away".into(),
            ..AutomaticReplies::default()
        };
        let script = build_sieve_script(&[rule], Some(&vacation));
        let vacation_at = script.find("vacation :days 1").expect("vacation block");
        let rule_at = script.find("# Archive").expect("rule block");
        assert!(vacation_at < rule_at, "script:\n{script}");
    }
}
