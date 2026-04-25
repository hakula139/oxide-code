//! UUID-DAG message-chain reconstruction.
//!
//! Session files are append-only JSONL where each
//! [`Entry::Message`](crate::session::entry::Entry) carries an optional
//! `parent_uuid` pointing at its predecessor. Concurrent resumes form
//! forks in the resulting DAG; this module reconstructs the newest
//! non-sidechain linear chain by walking parent edges from the latest
//! leaf back to the root.
//!
//! [`ChainBuilder`] is the loader-facing type: feed it one
//! `(uuid, parent_uuid, message, timestamp)` tuple per
//! [`Entry::Message`](crate::session::entry::Entry) as the file is read,
//! then call [`ChainBuilder::resolve`] to get back the linear chain plus
//! the UUID of its tip. Last-append-wins on duplicate UUIDs matches the
//! retry / partial-write recovery semantics the UUID is supposed to
//! dedupe on.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::message::Message;

/// Accumulator for [`Entry::Message`](crate::session::entry::Entry)
/// records as a session file is read. After every message has been
/// fed through [`Self::insert`], call [`Self::resolve`] to pick the
/// newest leaf and walk its parent chain.
pub(super) struct ChainBuilder {
    /// All messages seen so far, keyed by UUID. Last-append-wins on
    /// duplicates so a partial-write retry collapses to the latest
    /// representation.
    nodes: HashMap<Uuid, ChainNode>,
    /// UUIDs that some other message claims as its `parent_uuid`. The
    /// leaves of the DAG are `nodes.keys() - referenced`; subtracting
    /// the referenced set is what lets [`Self::resolve`] pick a tip
    /// without reading every node twice.
    referenced: HashSet<Uuid>,
}

/// Internal node stored in [`ChainBuilder::nodes`].
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

    /// Records one message entry. Last-append-wins on duplicate UUIDs —
    /// a retry or partial-write recovery could replay an entry, and we
    /// prefer the most recent representation.
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

    /// Walks back from the newest leaf via `parent_uuid` to produce a
    /// linear chain. Returns `(chain, Some(tip))` on success, or
    /// `(vec![], None)` when no messages were inserted.
    ///
    /// A cycle (e.g., from on-disk corruption where a UUID points at
    /// one of its descendants) is treated as a terminated chain: the
    /// walker detects the repeat and stops, preserving the prefix it
    /// has already collected rather than looping forever. A
    /// `parent_uuid` missing from the inserted set (orphan) is also
    /// treated as a chain terminator.
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
                // Cycle or repeated visit — bail out with what we have.
                warn!(
                    "session chain walk hit a cycle at {uuid}; truncating to the prefix collected so far"
                );
                break;
            }
            let Some(node) = self.nodes.remove(&uuid) else {
                // Missing ancestor — chain reaches an orphan. Stop here;
                // everything we collected so far stays in `chain`.
                break;
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

    fn text_of(message: &Message) -> &str {
        match &message.content[0] {
            ContentBlock::Text { text } => text.as_str(),
            other => panic!("expected text block, got {other:?}"),
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
    fn resolve_single_message_returns_self_as_tip() {
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
        assert_eq!(text_of(&chain[0]), "solo");
    }

    #[test]
    fn resolve_linear_chain_returns_root_to_tip_order() {
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
        let texts: Vec<&str> = chain.iter().map(text_of).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn resolve_fork_picks_newest_leaf() {
        // Two children of the same root; the later one wins as tip.
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
        let texts: Vec<&str> = chain.iter().map(text_of).collect();
        assert_eq!(texts, vec!["root", "newer"]);
    }

    #[test]
    fn resolve_fork_breaks_timestamp_tie_by_uuid_order() {
        // Two leaves at the same timestamp — uuid ordering breaks the
        // tie deterministically so the chosen tip is reproducible.
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
        // The single leaf points at a UUID we never inserted. The walk
        // halts at that orphan rather than erroring; the leaf still
        // appears in the returned chain.
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
        assert_eq!(text_of(&chain[0]), "dangling");
    }

    #[test]
    fn resolve_walk_breaks_when_leaf_chain_re_enters_cycle() {
        // Leaf → mid → tail → mid (cycle below the leaf). The leaf is
        // its own non-referenced node so it survives as the tip; the
        // walker visits leaf → mid → tail → mid again, where the
        // seen-set check trips and the chain truncates with whatever
        // it has so far. Exercises the cycle-break branch that the
        // pure mutual-cycle case (no leaf at all) doesn't reach.
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
        // Walker visits leaf, mid, tail, then mid again → break. The
        // collected prefix is reversed before return.
        let texts: Vec<&str> = chain.iter().map(text_of).collect();
        assert_eq!(texts, vec!["tail", "mid", "leaf"]);
    }

    #[test]
    fn resolve_cycle_truncates_chain_without_looping() {
        // a → b → a. Both nodes reference each other so neither is a
        // leaf — `referenced` covers both, leaving no tip candidate
        // and the chain comes back empty without spinning forever.
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
        // A leaf that points at itself: it is its own `referenced` so
        // it is excluded from the tip candidates. Same outcome as the
        // mutual-cycle case — empty chain, no infinite loop.
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
        // Same UUID inserted twice — the second wins, matching the
        // retry / partial-write recovery semantics the loader documents.
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
        assert_eq!(chain.len(), 1);
        assert_eq!(text_of(&chain[0]), "second", "latest duplicate should win");
    }
}
