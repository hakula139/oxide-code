//! UUID-DAG message-chain reconstruction.
//!
//! Concurrent resumes append in parallel and form forks in the on-disk
//! DAG. [`ChainBuilder`] reconstructs the newest non-sidechain linear
//! chain: feed it every `Entry::Message` tuple as the file is read,
//! then call [`ChainBuilder::resolve`] for the chain plus its tip.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::message::Message;

pub(super) struct ChainBuilder {
    nodes: HashMap<Uuid, ChainNode>,
    /// UUIDs claimed as some other message's `parent_uuid`. Leaves = keys − referenced.
    referenced: HashSet<Uuid>,
}

struct ChainNode {
    parent_uuid: Option<Uuid>,
    message: Message,
    timestamp: OffsetDateTime,
}

impl ChainBuilder {
    pub(super) fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            referenced: HashSet::new(),
        }
    }

    /// Last-append-wins on duplicate UUIDs (retry / partial-write replay).
    pub(super) fn insert(
        &mut self,
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        message: Message,
        timestamp: OffsetDateTime,
    ) {
        if let Some(p) = parent_uuid {
            self.referenced.insert(p);
        }
        self.nodes.insert(
            uuid,
            ChainNode {
                parent_uuid,
                message,
                timestamp,
            },
        );
    }

    /// Picks the newest leaf and walks back via `parent_uuid`. Cycles and orphan parents
    /// terminate the walk so corrupted on-disk state cannot hang.
    pub(super) fn resolve(mut self) -> (Vec<Message>, Option<Uuid>) {
        let tip = self
            .nodes
            .iter()
            .filter(|(uuid, _)| !self.referenced.contains(uuid))
            .max_by(|(a_uuid, a), (b_uuid, b)| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then_with(|| a_uuid.cmp(b_uuid))
            })
            .map(|(uuid, _)| *uuid);
        let Some(tip_uuid) = tip else {
            return (Vec::new(), None);
        };

        let mut chain: Vec<Message> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut cursor = Some(tip_uuid);
        while let Some(uuid) = cursor {
            if !seen.insert(uuid) {
                warn!("session chain walk hit a cycle at {uuid}; truncating");
                break;
            }
            let Some(node) = self.nodes.remove(&uuid) else {
                break; // orphan parent
            };
            chain.push(node.message);
            cursor = node.parent_uuid;
        }
        chain.reverse();
        (chain, Some(tip_uuid))
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::message::ContentBlock;

    fn user_message(text: &str) -> Message {
        Message::user(text)
    }

    /// Asserts the chain holds single-Text messages with these texts,
    /// in order. `matches!` returns `bool` — no panic arm to uncover.
    fn assert_chain_texts(chain: &[Message], want: &[&str]) {
        assert_eq!(chain.len(), want.len());
        for (msg, want_text) in chain.iter().zip(want) {
            assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == want_text));
        }
    }

    // ── ChainBuilder::resolve ──

    #[test]
    fn resolve_empty_builder_yields_no_chain() {
        let (chain, tip) = ChainBuilder::new().resolve();
        assert!(chain.is_empty());
        assert!(tip.is_none());
    }

    #[test]
    fn resolve_single_message_is_self_as_tip() {
        let mut b = ChainBuilder::new();
        let u = Uuid::new_v4();
        b.insert(
            u,
            None,
            user_message("solo"),
            datetime!(2026-04-16 12:00:00 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(u));
        assert_eq!(chain.len(), 1);
        assert_chain_texts(&chain, &["solo"]);
    }

    #[test]
    fn resolve_linear_chain_preserves_root_to_tip_order() {
        let mut b = ChainBuilder::new();
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let u3 = Uuid::new_v4();
        b.insert(
            u1,
            None,
            user_message("a"),
            datetime!(2026-04-16 12:00:00 UTC),
        );
        b.insert(
            u2,
            Some(u1),
            user_message("b"),
            datetime!(2026-04-16 12:00:01 UTC),
        );
        b.insert(
            u3,
            Some(u2),
            user_message("c"),
            datetime!(2026-04-16 12:00:02 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(u3));
        assert_chain_texts(&chain, &["a", "b", "c"]);
    }

    #[test]
    fn resolve_fork_picks_newest_leaf() {
        let mut b = ChainBuilder::new();
        let root = Uuid::new_v4();
        let older = Uuid::new_v4();
        let newer = Uuid::new_v4();
        b.insert(
            root,
            None,
            user_message("root"),
            datetime!(2026-04-16 12:00:00 UTC),
        );
        b.insert(
            older,
            Some(root),
            user_message("older"),
            datetime!(2026-04-16 12:00:01 UTC),
        );
        b.insert(
            newer,
            Some(root),
            user_message("newer"),
            datetime!(2026-04-16 12:00:02 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(newer), "tip should be the newer leaf");
        assert_chain_texts(&chain, &["root", "newer"]);
    }

    #[test]
    fn resolve_fork_breaks_timestamp_tie_by_uuid_order() {
        let mut b = ChainBuilder::new();
        let root = Uuid::new_v4();
        let lo = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let hi = Uuid::parse_str("99999999-9999-9999-9999-999999999999").unwrap();
        let same = datetime!(2026-04-16 12:00:01 UTC);
        b.insert(
            root,
            None,
            user_message("root"),
            datetime!(2026-04-16 12:00:00 UTC),
        );
        b.insert(lo, Some(root), user_message("lo"), same);
        b.insert(hi, Some(root), user_message("hi"), same);

        let (_, tip) = b.resolve();
        assert_eq!(
            tip,
            Some(hi),
            "uuid::Ord breaks the timestamp tie deterministically"
        );
    }

    #[test]
    fn resolve_orphan_parent_terminates_chain_at_dangling_node() {
        let mut b = ChainBuilder::new();
        let only = Uuid::new_v4();
        b.insert(
            only,
            Some(Uuid::new_v4()),
            user_message("dangling"),
            datetime!(2026-04-16 12:00:01 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(only));
        assert_eq!(chain.len(), 1);
        assert_chain_texts(&chain, &["dangling"]);
    }

    #[test]
    fn resolve_walk_breaks_when_leaf_chain_re_enters_cycle() {
        // leaf → mid → tail → mid: leaf survives as tip, walker
        // re-enters mid and the seen-set trips. Covers the
        // cycle-break branch the no-leaf case doesn't reach.
        let mut b = ChainBuilder::new();
        let leaf = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let tail = Uuid::new_v4();
        b.insert(
            leaf,
            Some(mid),
            user_message("leaf"),
            datetime!(2026-04-16 12:00:03 UTC),
        );
        b.insert(
            mid,
            Some(tail),
            user_message("mid"),
            datetime!(2026-04-16 12:00:02 UTC),
        );
        b.insert(
            tail,
            Some(mid),
            user_message("tail"),
            datetime!(2026-04-16 12:00:01 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(leaf), "leaf is the only non-referenced node");
        assert_chain_texts(&chain, &["tail", "mid", "leaf"]);
    }

    #[test]
    fn resolve_cycle_truncates_chain_without_looping() {
        // Mutual cycle: every node is referenced, so no leaf and no
        // tip — chain comes back empty without spinning forever.
        let mut b = ChainBuilder::new();
        let a = Uuid::new_v4();
        let bb = Uuid::new_v4();
        b.insert(
            a,
            Some(bb),
            user_message("A"),
            datetime!(2026-04-16 12:00:01 UTC),
        );
        b.insert(
            bb,
            Some(a),
            user_message("B"),
            datetime!(2026-04-16 12:00:02 UTC),
        );

        let (chain, tip) = b.resolve();
        assert!(tip.is_none(), "no leaf means no tip");
        assert!(chain.is_empty());
    }

    #[test]
    fn resolve_self_loop_breaks_at_repeat_visit() {
        let mut b = ChainBuilder::new();
        let only = Uuid::new_v4();
        b.insert(
            only,
            Some(only),
            user_message("ouroboros"),
            datetime!(2026-04-16 12:00:01 UTC),
        );

        let (chain, tip) = b.resolve();
        assert!(tip.is_none());
        assert!(chain.is_empty());
    }

    #[test]
    fn resolve_duplicate_uuid_keeps_latest_insertion() {
        let mut b = ChainBuilder::new();
        let u = Uuid::new_v4();
        b.insert(
            u,
            None,
            user_message("first"),
            datetime!(2026-04-16 12:00:01 UTC),
        );
        b.insert(
            u,
            None,
            user_message("second"),
            datetime!(2026-04-16 12:00:05 UTC),
        );

        let (chain, tip) = b.resolve();
        assert_eq!(tip, Some(u));
        assert_chain_texts(&chain, &["second"]);
    }
}
