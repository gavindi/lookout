use imap_proto::types::UidSetMember;

/// A `COPYUID` response code (RFC 4315 UIDPLUS, section 3), returned by a
/// `UID COPY` or `UID MOVE`: the UIDs the copied messages were given in the
/// destination mailbox, paired with their source UIDs.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CopyUid {
    /// The destination mailbox's `UIDVALIDITY`.
    pub uid_validity: u32,
    /// `(source UID, destination UID)` pairs, in the order the server listed
    /// them (the two uid-sets correspond member by member).
    pub pairs: Vec<(u32, u32)>,
}

impl CopyUid {
    /// Builds the pairing from the response code's two uid-sets. `None` when
    /// they don't expand to the same number of UIDs (a malformed response).
    pub fn from_sets(uid_validity: u32, source: &[UidSetMember], destination: &[UidSetMember]) -> Option<Self> {
        let source = expand_uid_set(source);
        let destination = expand_uid_set(destination);
        if source.len() != destination.len() {
            return None;
        }
        Some(CopyUid {
            uid_validity,
            pairs: source.into_iter().zip(destination).collect(),
        })
    }
}

/// Flattens a uid-set into its UIDs, ranges ascending (the parser already
/// normalises a reversed `4:2` range to `2..=4`).
pub fn expand_uid_set(set: &[UidSetMember]) -> Vec<u32> {
    let mut uids = Vec::new();
    for member in set {
        match member {
            UidSetMember::Uid(uid) => uids.push(*uid),
            UidSetMember::UidRange(range) => uids.extend(range.clone()),
        }
    }
    uids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_ranges_and_singles_member_by_member() {
        let source = [UidSetMember::Uid(142), UidSetMember::Uid(399)];
        let destination = [UidSetMember::UidRange(41..=42)];
        let copy = CopyUid::from_sets(7, &source, &destination).unwrap();
        assert_eq!(copy.uid_validity, 7);
        assert_eq!(copy.pairs, vec![(142, 41), (399, 42)]);
    }

    #[test]
    fn mismatched_set_sizes_are_rejected() {
        let source = [UidSetMember::UidRange(1..=3)];
        let destination = [UidSetMember::Uid(9)];
        assert_eq!(CopyUid::from_sets(1, &source, &destination), None);
    }
}
