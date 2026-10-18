//! UUID-DAG message chain reconstruction.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::message::Message;
use crate::prompt::SessionContext;

/// Accumulator of the messages a loader reads, which [`Self::resolve`] flattens into one chain. The
/// UUID DAG forks when two `ox -c` processes on one session each append onto the same parent.
#[derive(Default)]
pub(super) struct ChainBuilder {
    nodes: HashMap<Uuid, ChainNode>,
    /// UUIDs some message names as its `parent_uuid`.
    referenced: HashSet<Uuid>,
}

struct ChainNode {
    parent_uuid: Option<Uuid>,
    message: Message,
    timestamp: OffsetDateTime,
    context: Option<Box<SessionContext>>,
}

/// One branch of the DAG, flattened root → tip.
#[derive(Debug, Default)]
pub(super) struct Chain {
    pub(super) messages: Vec<Message>,
    pub(super) tip: Option<Uuid>,
    /// Newest prompt context recorded on this branch, ignoring other branches in the file.
    pub(super) context: Option<Box<SessionContext>>,
}

impl ChainBuilder {
    /// Adds a message. A duplicate UUID, as a retried or replayed write leaves, keeps the last one.
    pub(super) fn insert(
        &mut self,
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        message: Message,
        timestamp: OffsetDateTime,
        context: Option<Box<SessionContext>>,
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
                context,
            },
        );
    }

    /// Flattens the branch ending at the newest leaf, with UUID order breaking timestamp ties so
    /// every load picks the same branch. The walk takes each node out of the map, so a cycle or an
    /// orphan parent ends it at a missing node. Returns an empty chain when no leaf exists, as in a
    /// pure cycle.
    pub(super) fn resolve(mut self) -> Chain {
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
            return Chain::default();
        };

        let mut messages: Vec<Message> = Vec::new();
        let mut context = None;
        let mut cursor = Some(tip_uuid);
        while let Some(uuid) = cursor {
            let Some(node) = self.nodes.remove(&uuid) else {
                warn!("session chain walk stopped at missing or revisited message {uuid}");
                break;
            };
            messages.push(node.message);
            context = context.or(node.context);
            cursor = node.parent_uuid;
        }
        messages.reverse();
        Chain {
            messages,
            tip: Some(tip_uuid),
            context,
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::message::ContentBlock;
    use crate::prompt::PromptParts;

    fn user_message(text: &str) -> Message {
        Message::user(text)
    }

    /// Asserts the messages open with Text blocks holding `want`, in order.
    fn assert_chain_texts(chain: &[Message], want: &[&str]) {
        assert_eq!(chain.len(), want.len());
        for (msg, want_text) in chain.iter().zip(want) {
            assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == want_text));
        }
    }

    // ── ChainBuilder::resolve ──

    #[test]
    fn resolve_empty_builder_yields_no_chain() {
        let chain = ChainBuilder::default().resolve();
        assert_eq!(chain.messages, []);
        assert!(chain.tip.is_none());
    }

    #[test]
    fn resolve_single_message_is_self_as_tip() {
        let mut b = ChainBuilder::default();
        let u = Uuid::new_v4();
        b.insert(
            u,
            None,
            user_message("solo"),
            datetime!(2026-04-16 12:00:00 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(chain.tip, Some(u));
        assert_eq!(chain.messages.len(), 1);
        assert_chain_texts(&chain.messages, &["solo"]);
    }

    #[test]
    fn resolve_linear_chain_preserves_root_to_tip_order() {
        let mut b = ChainBuilder::default();
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let u3 = Uuid::new_v4();
        b.insert(
            u1,
            None,
            user_message("a"),
            datetime!(2026-04-16 12:00:00 UTC),
            None,
        );
        b.insert(
            u2,
            Some(u1),
            user_message("b"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );
        b.insert(
            u3,
            Some(u2),
            user_message("c"),
            datetime!(2026-04-16 12:00:02 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(chain.tip, Some(u3));
        assert_chain_texts(&chain.messages, &["a", "b", "c"]);
    }

    #[test]
    fn resolve_fork_picks_newest_leaf() {
        let mut b = ChainBuilder::default();
        let root = Uuid::new_v4();
        let older = Uuid::new_v4();
        let newer = Uuid::new_v4();
        b.insert(
            root,
            None,
            user_message("root"),
            datetime!(2026-04-16 12:00:00 UTC),
            None,
        );
        b.insert(
            older,
            Some(root),
            user_message("older"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );
        b.insert(
            newer,
            Some(root),
            user_message("newer"),
            datetime!(2026-04-16 12:00:02 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(chain.tip, Some(newer), "tip should be the newer leaf");
        assert_chain_texts(&chain.messages, &["root", "newer"]);
    }

    #[test]
    fn resolve_fork_takes_the_newest_context_on_the_tip_branch() {
        let context = |date: &str| {
            let mut prompt = PromptParts::default();
            date.clone_into(&mut prompt.seen.date);
            Some(Box::new(prompt.session_context()))
        };
        let mut b = ChainBuilder::default();
        let root = Uuid::new_v4();
        b.insert(
            root,
            None,
            user_message("root"),
            datetime!(2026-04-16 12:00:00 UTC),
            context("2026-04-16"),
        );
        b.insert(
            Uuid::new_v4(),
            Some(root),
            user_message("sibling"),
            datetime!(2026-04-17 12:00:00 UTC),
            context("2026-04-17"),
        );
        b.insert(
            Uuid::new_v4(),
            Some(root),
            user_message("tip"),
            datetime!(2026-04-18 12:00:00 UTC),
            None,
        );

        let chain = b.resolve();
        assert_chain_texts(&chain.messages, &["root", "tip"]);
        assert_eq!(chain.context, context("2026-04-16"));
    }

    #[test]
    fn resolve_fork_breaks_timestamp_tie_by_uuid_order() {
        let mut b = ChainBuilder::default();
        let root = Uuid::new_v4();
        let lo = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let hi = Uuid::parse_str("99999999-9999-9999-9999-999999999999").unwrap();
        let same = datetime!(2026-04-16 12:00:01 UTC);
        b.insert(
            root,
            None,
            user_message("root"),
            datetime!(2026-04-16 12:00:00 UTC),
            None,
        );
        b.insert(lo, Some(root), user_message("lo"), same, None);
        b.insert(hi, Some(root), user_message("hi"), same, None);

        let tip = b.resolve().tip;
        assert_eq!(
            tip,
            Some(hi),
            "uuid::Ord breaks the timestamp tie deterministically"
        );
    }

    #[test]
    fn resolve_orphan_parent_terminates_chain_at_dangling_node() {
        let mut b = ChainBuilder::default();
        let only = Uuid::new_v4();
        b.insert(
            only,
            Some(Uuid::new_v4()),
            user_message("dangling"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(chain.tip, Some(only));
        assert_eq!(chain.messages.len(), 1);
        assert_chain_texts(&chain.messages, &["dangling"]);
    }

    #[test]
    fn resolve_walk_breaks_when_leaf_chain_re_enters_cycle() {
        // leaf → mid → tail → mid: leaf is the tip, and the walk stops when it returns to mid.
        let mut b = ChainBuilder::default();
        let leaf = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let tail = Uuid::new_v4();
        b.insert(
            leaf,
            Some(mid),
            user_message("leaf"),
            datetime!(2026-04-16 12:00:03 UTC),
            None,
        );
        b.insert(
            mid,
            Some(tail),
            user_message("mid"),
            datetime!(2026-04-16 12:00:02 UTC),
            None,
        );
        b.insert(
            tail,
            Some(mid),
            user_message("tail"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(
            chain.tip,
            Some(leaf),
            "leaf is the only non-referenced node"
        );
        assert_chain_texts(&chain.messages, &["tail", "mid", "leaf"]);
    }

    #[test]
    fn resolve_cycle_truncates_chain_without_looping() {
        let mut b = ChainBuilder::default();
        let a = Uuid::new_v4();
        let bb = Uuid::new_v4();
        b.insert(
            a,
            Some(bb),
            user_message("A"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );
        b.insert(
            bb,
            Some(a),
            user_message("B"),
            datetime!(2026-04-16 12:00:02 UTC),
            None,
        );

        let chain = b.resolve();
        assert!(chain.tip.is_none(), "no leaf means no tip");
        assert_eq!(chain.messages, []);
    }

    #[test]
    fn resolve_duplicate_uuid_keeps_latest_insertion() {
        let mut b = ChainBuilder::default();
        let u = Uuid::new_v4();
        b.insert(
            u,
            None,
            user_message("first"),
            datetime!(2026-04-16 12:00:01 UTC),
            None,
        );
        b.insert(
            u,
            None,
            user_message("second"),
            datetime!(2026-04-16 12:00:05 UTC),
            None,
        );

        let chain = b.resolve();
        assert_eq!(chain.tip, Some(u));
        assert_chain_texts(&chain.messages, &["second"]);
    }
}
