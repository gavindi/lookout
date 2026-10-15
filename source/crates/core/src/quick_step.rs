//! The Quick Steps domain model (OUTLOOK_PARITY.md §5.1 M-15, roadmap item
//! 2.6).
//!
//! A [`QuickStep`] is a name plus an ordered chain of [`RuleAction`]s - the
//! same actions the rules engine (item 2.5) understands, so a Quick Step runs
//! exactly what a rule would. Unlike a rule it has no conditions and no
//! backend: it is applied by hand to the messages the user has selected, from
//! the Mail ribbon's **Quick steps** gallery or the `Ctrl+Shift+1`…`9` slots
//! (gallery order). Quick Steps are a local client-side convenience (Outlook
//! keeps them in the mailbox; there is no IMAP equivalent), stored in
//! `settings.json` beside the user's rules.
//!
//! This module holds only the shared shape and the shipped defaults - the
//! execution lives in `lookout-app` (`app/src/quick_steps.rs`), matching the
//! way `rules.rs` holds the pure matcher and the app holds the dispatch.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use crate::interop::{FlagStatus, FollowUpFlag};
use crate::mailbox::MailboxRole;
use crate::rules::{RuleAction, RuleTarget};

/// A named chain of actions a user runs on the selected messages.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuickStep {
    /// Stable identity, used by the manager's edit/delete and by reordering.
    pub id: String,
    pub name: String,
    /// Executed in order. An empty chain is a no-op and is never offered.
    pub actions: Vec<RuleAction>,
}

impl QuickStep {
    /// A new, empty quick step.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        QuickStep {
            id: id.into(),
            name: name.into(),
            actions: Vec::new(),
        }
    }

    /// Whether the step has anything to run. An empty chain is dropped from
    /// the gallery and from the slots.
    pub fn runnable(&self) -> bool {
        !self.actions.is_empty()
    }

    /// A one-line human summary of the chain, for the manager and the
    /// gallery tooltip: "Mark as read, Move to Archive folder".
    pub fn summary(&self) -> String {
        if self.actions.is_empty() {
            return "No actions".to_string();
        }
        self.actions.iter().map(RuleAction::label).collect::<Vec<_>>().join(", ")
    }
}

/// The Quick Steps a fresh install starts with. Deliberately built from
/// well-known mailbox roles (not folder names or categories), so each resolves
/// on any account and is skipped cleanly where the folder is absent - the same
/// "degrade softly" rule the client rules engine follows (§10). Seeding is
/// guarded by a one-shot flag (see `AppConfig`), so deleting them all stays
/// deleted.
pub fn default_quick_steps() -> Vec<QuickStep> {
    let archive = || RuleAction::Move(RuleTarget::Role(MailboxRole::Archive));
    vec![
        QuickStep {
            id: "quick-step-archive-read".to_string(),
            name: "Archive and mark read".to_string(),
            actions: vec![RuleAction::MarkRead(true), archive()],
        },
        QuickStep {
            id: "quick-step-mark-done".to_string(),
            name: "Mark done".to_string(),
            actions: vec![
                RuleAction::Flag(FollowUpFlag {
                    status: FlagStatus::Complete,
                    ..FollowUpFlag::default()
                }),
                archive(),
            ],
        },
        QuickStep {
            id: "quick-step-flag-follow-up".to_string(),
            name: "Flag for follow-up".to_string(),
            actions: vec![RuleAction::Flag(FollowUpFlag {
                status: FlagStatus::Flagged,
                ..FollowUpFlag::default()
            })],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_step_is_not_runnable_and_says_so() {
        let step = QuickStep::new("s1", "Untitled");
        assert!(!step.runnable());
        assert_eq!(step.summary(), "No actions");
    }

    #[test]
    fn summary_lists_actions_in_order() {
        let mut step = QuickStep::new("s1", "Archive and read");
        step.actions = vec![RuleAction::MarkRead(true), RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))];
        assert!(step.runnable());
        let summary = step.summary();
        assert!(summary.contains("Mark as read"));
        assert!(summary.contains("Move to Archive folder"));
        assert!(summary.find("Mark as read") < summary.find("Move to Archive"), "order is preserved");
    }

    #[test]
    fn serializes_round_trip() {
        let mut step = QuickStep::new("s1", "Newsletters");
        step.actions = vec![
            RuleAction::Categorize(vec!["Newsletters".to_string()]),
            RuleAction::MarkRead(true),
            RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)),
        ];
        let json = serde_json::to_string(&step).unwrap();
        let back: QuickStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn defaults_are_runnable_unique_and_role_based() {
        let steps = default_quick_steps();
        assert!(!steps.is_empty());
        let mut ids = std::collections::HashSet::new();
        for step in &steps {
            assert!(step.runnable(), "{} has actions", step.name);
            assert!(!step.name.trim().is_empty());
            assert!(ids.insert(step.id.clone()), "ids are unique");
            // No "stop processing" - meaningless in a hand-run chain.
            assert!(!step.actions.iter().any(|a| matches!(a, RuleAction::StopProcessing)), "{} has no stop action", step.name);
        }
    }
}
