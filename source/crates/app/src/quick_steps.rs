//! Quick Steps execution (OUTLOOK_PARITY.md §5.1 M-15, roadmap item 2.6).
//!
//! A Quick Step is a named chain of rule actions (see `lookout_core::QuickStep`)
//! run by hand on the selected messages. This module is the app-side glue: it
//! turns a chain into `AccountCommand`s with the *same* planner the rules
//! engine uses ([`crate::rules::plan_actions`]), shows the effect in the list
//! at once (the optimistic patches the click helpers would apply), sends it,
//! and - unlike a rule - records **one** Undo entry that reverses the whole
//! step.
//!
//! Scope: a Quick Step runs on whatever the user selected, in any folder (not
//! just the Inbox), matching Outlook. Nothing runs on arriving mail: that is
//! the rules engine's job. A forward/redirect action rides the same
//! raw-bytes queue the rules use (`crate::rules::queue_forward`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use lookout_core::{EmailSummary, QuickStep, SystemFlagBit};
use lookout_mail::session::AccountCommand;

use crate::message_list::MessageListModel;
use crate::rules::queue_forward;
use crate::window::{mailbox_account_id, optimistic_remove_messages, optimistic_set_follow_up, optimistic_toggle_read, record_undo, UiState};

/// Applies `step` to the selected messages. Resolves each message's account
/// (so a mixed selection from the unified inbox routes correctly), plans and
/// sends its commands, patches the list, and records a single Undo entry when
/// anything was done.
pub(crate) fn apply_quick_step(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: &[EmailSummary], step: &QuickStep) {
    if summaries.is_empty() || !step.runnable() {
        return;
    }
    let mut inverse: Vec<AccountCommand> = Vec::new();
    let mut applied = 0usize;
    for message in summaries {
        let Some(account_id) = mailbox_account_id(&message.mailbox) else { continue };
        let (folders, categories, cmd_tx, interactive_cmd_tx, email) = {
            let st = state.borrow();
            let Some(handle) = st.accounts.get(&account_id) else { continue };
            let folders = handle.folders.clone();
            let categories = st.categories.borrow().clone();
            let cmd_tx = handle.cmd_tx.clone();
            let interactive_cmd_tx = handle.interactive_cmd_tx.clone();
            let email = handle.email.clone();
            (folders, categories, cmd_tx, interactive_cmd_tx, email)
        };
        let plan = crate::rules::plan_actions(&folders, &categories, message, &step.actions);
        if plan.commands.is_empty() && plan.forwards.is_empty() {
            continue;
        }
        for patch in list_patches(&plan.commands) {
            apply_list_patch(state, message_list, message, &patch);
        }
        for command in plan.commands {
            let _ = cmd_tx.send_blocking(command);
        }
        for forward in plan.forwards {
            queue_forward(state, &interactive_cmd_tx, &email, message, &forward.to, forward.kind);
        }
        inverse.extend(plan.inverse);
        applied += 1;
    }
    if applied == 0 {
        return;
    }
    record_undo(state, &step_undo_description(step), inverse);
}

/// Runs the quick step in gallery position `slot` (0-based) on the current
/// selection - the `Ctrl+Shift+1`…`9` slots. A no-op when the slot is empty or
/// nothing is selected.
pub(crate) fn apply_quick_step_slot(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, slot: usize) {
    let step = state.borrow().app_config.borrow().quick_steps.get(slot).cloned();
    let Some(step) = step else { return };
    let summaries = message_list.selected_summaries();
    apply_quick_step(state, message_list, &summaries, &step);
}

/// The Undo-toast wording for a step. A named step reads as itself ("Archive
/// and mark read"); the fallback covers a nameless one.
fn step_undo_description(step: &QuickStep) -> String {
    if step.name.trim().is_empty() {
        "Quick step".to_string()
    } else {
        step.name.clone()
    }
}

/// The pre-confirmation change a plan makes to a message's list row - the
/// optimistic half of `move_messages_to_role`/`set_messages_read`/`set_follow_up`
/// (§6, "show it at once"). A move or delete takes the row out of the current
/// folder; otherwise the read/flag state is patched. Pure, so it is tested
/// without a running GTK.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListPatch {
    /// The message leaves the current folder (move or delete): remove the row.
    Remove,
    /// Set or clear `\Seen`.
    Read(bool),
    /// Set the follow-up flag.
    Flag(lookout_core::FollowUpFlag),
}

fn list_patches(commands: &[AccountCommand]) -> Vec<ListPatch> {
    // A move/delete wins over a read/flag patch: the row is gone either way.
    if commands
        .iter()
        .any(|command| matches!(command, AccountCommand::MoveMessagesTo { .. } | AccountCommand::MoveMessage { .. }))
    {
        return vec![ListPatch::Remove];
    }
    let mut patches = Vec::new();
    for command in commands {
        match command {
            AccountCommand::StoreFlagsMany { add, remove, .. } => {
                if add.contains(&SystemFlagBit::Seen) {
                    patches.push(ListPatch::Read(true));
                } else if remove.contains(&SystemFlagBit::Seen) {
                    patches.push(ListPatch::Read(false));
                }
            }
            AccountCommand::SetFollowUp { flag, .. } => patches.push(ListPatch::Flag(flag.clone())),
            _ => {}
        }
    }
    patches
}

/// Applies one optimistic patch to the list (and the unified snapshot).
fn apply_list_patch(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, message: &EmailSummary, patch: &ListPatch) {
    let mailbox = &message.mailbox;
    let uids = [message.uid];
    match patch {
        ListPatch::Remove => optimistic_remove_messages(state, message_list, mailbox, &uids),
        ListPatch::Read(mark_read) => optimistic_toggle_read(state, message_list, mailbox, &uids, *mark_read),
        ListPatch::Flag(flag) => optimistic_set_follow_up(state, message_list, mailbox, &uids, flag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{FollowUpFlag, MailboxId, Uid};

    fn mailbox(id: &str) -> MailboxId {
        MailboxId(format!("acct:{id}"))
    }

    #[test]
    fn an_empty_step_never_runs() {
        assert!(!QuickStep::new("s", "Empty").runnable());
    }

    #[test]
    fn a_move_patches_the_row_away() {
        let commands = vec![AccountCommand::MoveMessagesTo {
            mailbox: mailbox("INBOX"),
            uids: vec![Uid(1)],
            target: mailbox("Archive"),
        }];
        assert_eq!(list_patches(&commands), vec![ListPatch::Remove]);
    }

    #[test]
    fn read_and_flag_commands_become_read_and_flag_patches() {
        let flag = FollowUpFlag {
            status: lookout_core::FlagStatus::Flagged,
            ..FollowUpFlag::default()
        };
        let commands = vec![
            AccountCommand::StoreFlagsMany {
                mailbox: mailbox("INBOX"),
                uids: vec![Uid(1)],
                add: vec![SystemFlagBit::Seen],
                remove: Vec::new(),
            },
            AccountCommand::SetFollowUp {
                mailbox: mailbox("INBOX"),
                uids: vec![Uid(1)],
                flag: flag.clone(),
            },
        ];
        assert_eq!(list_patches(&commands), vec![ListPatch::Read(true), ListPatch::Flag(flag)]);
    }

    #[test]
    fn a_move_supersedes_a_read_patch() {
        let commands = vec![
            AccountCommand::StoreFlagsMany {
                mailbox: mailbox("INBOX"),
                uids: vec![Uid(1)],
                add: vec![SystemFlagBit::Seen],
                remove: Vec::new(),
            },
            AccountCommand::MoveMessagesTo {
                mailbox: mailbox("INBOX"),
                uids: vec![Uid(1)],
                target: mailbox("Archive"),
            },
        ];
        assert_eq!(list_patches(&commands), vec![ListPatch::Remove]);
    }

    #[test]
    fn undo_description_prefers_the_name() {
        assert_eq!(step_undo_description(&QuickStep::new("s", "Archive and read")), "Archive and read");
        assert_eq!(step_undo_description(&QuickStep::new("s", "   ")), "Quick step");
    }
}
