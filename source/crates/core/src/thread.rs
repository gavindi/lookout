use std::collections::{BTreeSet, HashMap};

use crate::email::EmailSummary;
use crate::ids::Uid;

/// Root Message-ID of a JWZ-connected component, or a `subject:`-prefixed
/// normalized-subject fallback key when a message has no usable Message-ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct ThreadKey(pub String);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadGroup {
    pub key: ThreadKey,
    /// Sorted oldest to newest (natural reading order for a conversation).
    pub emails: Vec<EmailSummary>,
    /// Index into `emails` of the most recent message.
    pub latest: usize,
    pub participants: Vec<String>,
    pub has_unread: bool,
    pub has_starred: bool,
    pub has_attachment: bool,
    pub has_answered: bool,
}

/// A minimal union-find (disjoint-set) structure over normalized Message-ID
/// strings. Connecting every message to the identifiers in its
/// References/In-Reply-To chain has the same grouping effect as JWZ's
/// container-linking step, without materializing an explicit tree — Phase 1's
/// UI only needs a flat grouping key per message, not a navigable tree.
struct UnionFind {
    parent: HashMap<String, String>,
}

impl UnionFind {
    fn new() -> Self {
        UnionFind { parent: HashMap::new() }
    }

    fn find(&mut self, id: &str) -> String {
        if !self.parent.contains_key(id) {
            self.parent.insert(id.to_string(), id.to_string());
            return id.to_string();
        }
        let mut root = id.to_string();
        while self.parent[&root] != root {
            root = self.parent[&root].clone();
        }
        let mut cur = id.to_string();
        while self.parent[&cur] != root {
            let next = self.parent[&cur].clone();
            self.parent.insert(cur, root.clone());
            cur = next;
        }
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }
}

fn normalize_message_id(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').to_string()
}

/// The ancestor chain for a message, oldest-to-newest, built from
/// `References` when present (per RFC 5322 §3.6.4, ordered oldest-first) or
/// falling back to a single-element `In-Reply-To` chain.
fn ancestor_chain(msg: &EmailSummary) -> Vec<String> {
    let mut chain: Vec<String> = msg.references.iter().map(|r| normalize_message_id(r)).collect();
    if chain.is_empty() {
        if let Some(irt) = &msg.in_reply_to {
            chain.push(normalize_message_id(irt));
        }
    }
    chain
}

/// Strips common reply/forward subject prefixes (`Re:`, `Fwd:`, `Fw:`)
/// recursively, then lowercases and trims, for use as a fallback
/// thread-grouping key when no Message-ID chain connects messages (older or
/// broken clients that omit References/In-Reply-To).
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let prefix_len = ["re:", "fwd:", "fw:"].iter().find(|p| lower.starts_with(**p)).map(|p| p.len());
        match prefix_len {
            Some(len) => s = s[len..].trim(),
            None => break,
        }
    }
    s.to_ascii_lowercase()
}

/// Computes a [`ThreadKey`] for every message in `messages` by connecting
/// messages that share a Message-ID / In-Reply-To / References relationship.
/// Within each connected component, the canonical key is the identifier that
/// is referenced by another message but itself has no ancestor chain (i.e.
/// the true thread root); if no such identifier is present in the fetched
/// set (the root message itself wasn't fetched, e.g. it's in another
/// mailbox), the earliest-dated message's own id is used instead, so the key
/// is still stable across re-fetches of the same message set.
pub fn compute_thread_keys(messages: &[EmailSummary]) -> HashMap<Uid, ThreadKey> {
    let mut uf = UnionFind::new();
    let mut chains: HashMap<Uid, (String, Vec<String>)> = HashMap::new();

    for msg in messages {
        let Some(mid) = msg.message_id.as_deref().map(normalize_message_id) else {
            continue;
        };
        uf.find(&mid); // ensure a singleton set exists even with no relations
        let chain = ancestor_chain(msg);
        for pair in chain.windows(2) {
            uf.union(&pair[0], &pair[1]);
        }
        if let Some(last) = chain.last() {
            uf.union(last, &mid);
        }
        chains.insert(msg.uid, (mid, chain));
    }

    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for (_, chain) in chains.values() {
        referenced.extend(chain.iter().cloned());
    }

    let mut components: HashMap<String, Vec<(String, bool, chrono::DateTime<chrono::Utc>)>> = HashMap::new();
    for msg in messages {
        let Some((mid, chain)) = chains.get(&msg.uid) else { continue };
        let root = uf.find(mid);
        components.entry(root).or_default().push((mid.clone(), !chain.is_empty(), msg.date));
    }

    let mut canonical: HashMap<String, String> = HashMap::new();
    for (root, members) in &components {
        let true_root = members
            .iter()
            .find(|(id, has_parent, _)| referenced.contains(id) && !has_parent)
            .map(|(id, _, _)| id.clone());
        let key = true_root.unwrap_or_else(|| members.iter().min_by_key(|(_, _, date)| *date).unwrap().0.clone());
        canonical.insert(root.clone(), key);
    }

    let mut result = HashMap::with_capacity(messages.len());
    for msg in messages {
        let key = match chains.get(&msg.uid) {
            Some((mid, _)) => {
                let root = uf.find(mid);
                canonical.get(&root).cloned().unwrap_or_else(|| mid.clone())
            }
            None => format!("subject:{}", normalize_subject(msg.subject.as_deref().unwrap_or(""))),
        };
        result.insert(msg.uid, ThreadKey(key));
    }
    result
}

/// Groups `messages` into [`ThreadGroup`]s using [`compute_thread_keys`],
/// sorted oldest-to-newest within each group. Groups are returned in
/// descending order of their most recent message's date, matching a typical
/// mailbox list ordering.
pub fn group_into_threads(mut messages: Vec<EmailSummary>) -> Vec<ThreadGroup> {
    let keys = compute_thread_keys(&messages);
    messages.sort_by_key(|m| m.date);

    let mut by_key: HashMap<ThreadKey, Vec<EmailSummary>> = HashMap::new();
    for msg in messages {
        let key = keys
            .get(&msg.uid)
            .cloned()
            .unwrap_or_else(|| ThreadKey(format!("subject:{}", normalize_subject(msg.subject.as_deref().unwrap_or("")))));
        by_key.entry(key).or_default().push(msg);
    }

    let mut groups: Vec<ThreadGroup> = by_key
        .into_iter()
        .map(|(key, emails)| {
            let has_unread = emails.iter().any(|e| e.is_unread());
            let has_starred = emails.iter().any(|e| e.is_starred());
            let has_attachment = emails.iter().any(|e| e.has_attachment);
            let has_answered = emails.iter().any(|e| e.flags.contains(&crate::email::SystemFlagBit::Answered));
            let mut participants: Vec<String> = emails.iter().flat_map(|e| e.from.iter().map(|a| a.display_label().to_string())).collect();
            participants.dedup();
            let latest = emails.len() - 1;
            ThreadGroup {
                key,
                emails,
                latest,
                participants,
                has_unread,
                has_starred,
                has_attachment,
                has_answered,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        let a_date = a.emails[a.latest].date;
        let b_date = b.emails[b.latest].date;
        b_date.cmp(&a_date)
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::EmailAddress;
    use chrono::{TimeZone, Utc};

    fn msg(uid: u32, message_id: &str, in_reply_to: Option<&str>, references: &[&str], subject: &str, hours_offset: i64) -> EmailSummary {
        EmailSummary {
            uid: Uid(uid),
            mailbox: crate::ids::MailboxId("acct:INBOX".into()),
            message_id: Some(message_id.to_string()),
            in_reply_to: in_reply_to.map(|s| s.to_string()),
            references: references.iter().map(|s| s.to_string()).collect(),
            thread_key: ThreadKey(String::new()),
            subject: Some(subject.to_string()),
            from: vec![EmailAddress::new("someone@example.com")],
            to: vec![],
            cc: vec![],
            date: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::hours(hours_offset),
            flags: Default::default(),
            keywords: Default::default(),
            size: 100,
            has_attachment: false,
            preview: None,
        }
    }

    #[test]
    fn groups_a_reply_chain_under_the_true_root() {
        let root = msg(1, "<root@x>", None, &[], "Project kickoff", 0);
        let reply1 = msg(2, "<r1@x>", Some("<root@x>"), &["<root@x>"], "Re: Project kickoff", 1);
        let reply2 = msg(3, "<r2@x>", Some("<r1@x>"), &["<root@x>", "<r1@x>"], "Re: Project kickoff", 2);

        let keys = compute_thread_keys(&[root.clone(), reply1.clone(), reply2.clone()]);
        assert_eq!(keys[&Uid(1)], ThreadKey("root@x".into()));
        assert_eq!(keys[&Uid(2)], ThreadKey("root@x".into()));
        assert_eq!(keys[&Uid(3)], ThreadKey("root@x".into()));
    }

    #[test]
    fn unrelated_messages_get_distinct_keys() {
        let a = msg(1, "<a@x>", None, &[], "Hello", 0);
        let b = msg(2, "<b@x>", None, &[], "Unrelated", 1);
        let keys = compute_thread_keys(&[a, b]);
        assert_ne!(keys[&Uid(1)], keys[&Uid(2)]);
    }

    #[test]
    fn root_message_not_in_fetched_set_falls_back_to_earliest_dated() {
        // Simulates the thread root living in a different mailbox: only the
        // two replies are in this fetch, and neither is "referenced by
        // another message but has no parent itself" - the true-root search
        // has no candidate. The key should stay stable and be the earliest
        // of the two.
        let reply1 = msg(2, "<r1@x>", Some("<root@x>"), &["<root@x>"], "Re: Kickoff", 1);
        let reply2 = msg(3, "<r2@x>", Some("<r1@x>"), &["<root@x>", "<r1@x>"], "Re: Kickoff", 2);
        let keys = compute_thread_keys(&[reply1.clone(), reply2.clone()]);
        assert_eq!(keys[&Uid(2)], keys[&Uid(3)]);
        assert_eq!(keys[&Uid(2)], ThreadKey("r1@x".into()));
    }

    #[test]
    fn messages_without_message_id_fall_back_to_normalized_subject() {
        let mut a = msg(1, "<a@x>", None, &[], "Weekly sync", 0);
        a.message_id = None;
        let mut b = msg(2, "<b@x>", None, &[], "Re: Weekly sync", 1);
        b.message_id = None;
        let keys = compute_thread_keys(&[a, b]);
        assert_eq!(keys[&Uid(1)], keys[&Uid(2)]);
        assert_eq!(keys[&Uid(1)], ThreadKey("subject:weekly sync".into()));
    }

    #[test]
    fn normalize_subject_strips_nested_prefixes() {
        assert_eq!(normalize_subject("Re: Fwd: Re: Budget"), "budget");
        assert_eq!(normalize_subject("  Re:Budget"), "budget");
        assert_eq!(normalize_subject("Budget"), "budget");
    }

    #[test]
    fn group_into_threads_sorts_oldest_first_within_group_and_newest_group_first() {
        let root = msg(1, "<root@x>", None, &[], "Kickoff", 0);
        let reply = msg(2, "<r1@x>", Some("<root@x>"), &["<root@x>"], "Re: Kickoff", 5);
        let other = msg(3, "<other@x>", None, &[], "Later unrelated", 10);

        let groups = group_into_threads(vec![reply, root, other]);
        assert_eq!(groups.len(), 2);
        // The unrelated, more recent single message sorts first.
        assert_eq!(groups[0].emails.len(), 1);
        assert_eq!(groups[0].emails[0].uid, Uid(3));
        // The two-message thread is sorted oldest-to-newest internally.
        assert_eq!(groups[1].emails[0].uid, Uid(1));
        assert_eq!(groups[1].emails[1].uid, Uid(2));
        assert_eq!(groups[1].latest, 1);
    }
}
