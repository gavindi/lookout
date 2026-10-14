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

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::BTreeSet;

use lookout_core::{
    sanitize_tag_key, tag_keyword, FlagStatus, Importance, Rule, RuleAction, RuleBackend, RuleCondition, RuleTarget, SizeComparison, FLAG_DONE_KEYWORD, FLAG_KEYWORD,
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

/// Compiles the enabled, `Sieve`-backend rules into one script. Rules Sieve
/// can't express are skipped and named in a comment; the rest still apply.
pub fn rules_to_sieve(rules: &[Rule]) -> String {
    let mut requirements: BTreeSet<&'static str> = BTreeSet::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

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

    let mut script = String::from("# Rules generated by Lookout - do not edit by hand.\n");
    if !requirements.is_empty() {
        let list = requirements.iter().map(|name| quoted(name)).collect::<Vec<_>>().join(", ");
        script.push_str(&format!("require [{list}];\n"));
    }
    if blocks.is_empty() {
        script.push_str("# No rules are set to run on the server.\n");
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
}
