//! The prompts the TUI holds: the one running as the active turn and the FIFO typed mid-turn.

use std::collections::VecDeque;

use tracing::warn;

use ox_core::command;

/// A prompt the TUI holds: the line the user typed and the text the engine receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueuedPrompt {
    /// The line as typed, or the `/init <file>` a picker choice stands for, which Esc and a failed
    /// turn hand back. A typed line unescapes to the text the chat shows.
    line: String,
    /// The prompt a slash command built from `line`, which the engine receives in its place.
    built: Option<String>,
    needs_echo: bool,
}

impl QueuedPrompt {
    /// A prompt typed into the input, sent with its `//` escape removed.
    fn typed(line: String) -> Self {
        Self {
            line,
            built: None,
            needs_echo: true,
        }
    }

    /// A prompt built from `line`, whose slash command has already echoed in chat.
    pub(super) fn slash(line: String, prompt: String) -> Self {
        Self {
            line,
            built: Some(prompt),
            needs_echo: false,
        }
    }

    pub(super) fn line(&self) -> &str {
        &self.line
    }

    /// The text the engine receives: the unescaped line, or the prompt a slash command built.
    pub(super) fn prompt(&self) -> &str {
        self.built.as_deref().unwrap_or_else(|| self.display())
    }

    /// Returns whether a slash command built the prompt from its line.
    pub(super) fn is_slash(&self) -> bool {
        self.built.is_some()
    }

    /// The session title for a prompt a slash command built: the line that ran it.
    pub(super) fn title(&self) -> Option<&str> {
        self.is_slash().then_some(self.line.as_str())
    }

    /// The text the chat shows for the prompt.
    pub(super) fn display(&self) -> &str {
        command::unescape_literal(&self.line)
    }

    /// Takes the chat echo for an accepted delivery, once per attempt.
    pub(super) fn take_echo(&mut self) -> Option<String> {
        std::mem::take(&mut self.needs_echo).then(|| self.display().to_owned())
    }
}

#[cfg(test)]
impl PartialEq<&str> for QueuedPrompt {
    fn eq(&self, line: &&str) -> bool {
        self.line == *line
    }
}

/// The active prompt and the prompts queued behind it.
#[derive(Default)]
pub(super) struct PromptQueue {
    /// Prompt already painted for the in-flight turn. Replayed if auto-compaction clears chat,
    /// handed back if the turn fails before it is sent or is rolled back, and dropped once a
    /// mid-turn compaction summarizes it.
    active: Option<QueuedPrompt>,
    /// What a failure of the in-flight operation hands back to the user.
    hand_back: HandBack,
    pending: VecDeque<QueuedPrompt>,
    /// Whether the queued prompts wait for the user's next submit, since an operation ended
    /// without starting them.
    held: bool,
    /// How many leading `pending` prompts the in-flight turn already holds.
    forwarded: usize,
    /// Queued prompts the in-flight turn took in, which a rolled-back turn hands back.
    drained: Vec<QueuedPrompt>,
    /// How many leading `pending` prompts were queued before a requested session switch. They
    /// belong to the session it leaves, and the queue only grows while the switch runs.
    before_switch: Option<usize>,
}

/// What a failure of the in-flight operation hands back to the user.
#[derive(Clone, Copy, Default)]
enum HandBack {
    /// Nothing, since the operation ran and the queue goes on.
    #[default]
    Nothing,
    /// Nothing, but the queue waits for the user, since a compaction or a session switch is
    /// running. After a failed compaction the next prompt would start another summary request, and
    /// after a failed switch the prompts typed for the new session would run in the old one.
    HeldQueue,
    /// The prompt, since the automatic compaction ahead of it failed, so the engine never sent it.
    UnsentPrompt,
    /// The prompt and the queued prompts its turn took in, since the engine rolled the turn back.
    RolledBackTurn,
}

/// How [`PromptQueue::fail_turn`] settled a failed turn.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailedTurn {
    /// The operation ran, so nothing comes back and the queue goes on.
    Ran,
    /// The operation ran, but the queue waits for the user.
    Held,
    /// The prompts came back to the queue, apart from the one returned for the empty input.
    HandedBack(Option<QueuedPrompt>),
}

impl PromptQueue {
    pub(super) fn pending(&self) -> &VecDeque<QueuedPrompt> {
        &self.pending
    }

    /// The prompt the in-flight turn runs.
    pub(super) fn active(&self) -> Option<&QueuedPrompt> {
        self.active.as_ref()
    }

    /// Marks `prompt` as the one the in-flight turn runs.
    pub(super) fn set_active(&mut self, prompt: QueuedPrompt) {
        self.active = Some(prompt);
    }

    pub(super) fn clear_active(&mut self) {
        self.active = None;
    }

    /// Notes that a manual or automatic compaction is running, so a failure holds the queue.
    pub(super) fn mark_compacting(&mut self) {
        self.hand_back = HandBack::HeldQueue;
    }

    /// Notes that the engine will not send the active prompt.
    pub(super) fn mark_unsent(&mut self) {
        self.hand_back = HandBack::UnsentPrompt;
    }

    /// Returns whether the engine will not send the active prompt, so a cancel hands it back.
    pub(super) fn unsent(&self) -> bool {
        matches!(self.hand_back, HandBack::UnsentPrompt)
    }

    /// Notes that compaction cleared chat, so pending prompts need an echo and a later failure is
    /// the turn's own.
    pub(super) fn mark_compacted(&mut self) {
        self.hand_back = HandBack::Nothing;
        for prompt in &mut self.pending {
            prompt.needs_echo = true;
        }
    }

    /// Forgets the active prompt and the prompts the turn took in, once a mid-turn compaction
    /// summarizes them, so a rollback hands back only the prompts drained after it.
    pub(super) fn mark_summarized(&mut self) {
        self.active = None;
        self.drained.clear();
    }

    /// Notes that the engine rolled the in-flight turn back.
    pub(super) fn mark_rolled_back(&mut self) {
        self.hand_back = HandBack::RolledBackTurn;
    }

    /// Queues a line typed into the input, which releases a held queue.
    pub(super) fn push(&mut self, line: String) {
        self.pending.push_back(QueuedPrompt::typed(line));
        self.held = false;
    }

    /// Holds the queued prompts until the user next submits one.
    pub(super) fn hold(&mut self) {
        self.held = true;
    }

    pub(super) fn held(&self) -> bool {
        self.held
    }

    /// Returns a prompt whose turn could not start to the head of the queue.
    pub(super) fn push_front(&mut self, prompt: QueuedPrompt) {
        self.pending.push_front(prompt);
    }

    /// Takes the most recently queued line back, for editing.
    pub(super) fn pop_last(&mut self) -> Option<String> {
        self.pending.pop_back().map(|queued| queued.line)
    }

    /// Takes the oldest queued prompt to run as a turn of its own.
    pub(super) fn pop_next(&mut self) -> Option<QueuedPrompt> {
        self.pending.pop_front()
    }

    /// The text of the oldest queued prompt the in-flight turn lacks, or `None` while held.
    pub(super) fn next_to_forward(&self) -> Option<String> {
        if self.held {
            return None;
        }
        self.pending
            .get(self.forwarded)
            .map(|queued| queued.prompt().to_owned())
    }

    /// Records that the in-flight turn now holds [`Self::next_to_forward`].
    pub(super) fn mark_forwarded(&mut self) {
        self.forwarded += 1;
    }

    #[cfg(test)]
    pub(super) fn forwarded(&self) -> usize {
        self.forwarded
    }

    /// Moves the queue head into the in-flight turn once the engine confirms it drained `text`,
    /// returning any pending chat echo. Only a forwarded head matches, since the engine drains
    /// prompts in the order it took them.
    pub(super) fn confirm_drained(&mut self, text: String) -> Option<String> {
        if self.forwarded > 0
            && self
                .pending
                .front()
                .is_some_and(|queued| queued.prompt() == text)
            && let Some(mut queued) = self.pending.pop_front()
        {
            let display = queued.take_echo();
            self.drained.push(queued);
            self.forwarded -= 1;
            display
        } else {
            warn!(
                prompt_len = text.len(),
                "drained prompt does not lead the forwarded queue"
            );
            Some(text)
        }
    }

    /// Settles a failed turn by what it marked. A failed compaction or session switch holds the
    /// queue. An unsent prompt returns for the input when the input and the queue are empty, so
    /// Enter sends it again, and otherwise heads the queue so it still runs first. A rolled-back
    /// turn returns the queued prompts it took in to the queue head, and its own prompt for the
    /// input when `input_empty`, or else ahead of them.
    pub(super) fn fail_turn(&mut self, input_empty: bool) -> FailedTurn {
        match self.hand_back {
            HandBack::Nothing => FailedTurn::Ran,
            HandBack::HeldQueue => FailedTurn::Held,
            HandBack::UnsentPrompt => {
                let into_input = input_empty && self.pending.is_empty();
                FailedTurn::HandedBack(self.restore_active(into_input))
            }
            HandBack::RolledBackTurn => {
                for mut drained in self.drained.drain(..).rev() {
                    drained.needs_echo = true;
                    self.pending.push_front(drained);
                }
                FailedTurn::HandedBack(self.restore_active(input_empty))
            }
        }
    }

    fn restore_active(&mut self, into_input: bool) -> Option<QueuedPrompt> {
        let mut prompt = self.active.take()?;
        prompt.needs_echo = true;
        if into_input {
            return Some(prompt);
        }
        self.pending.push_front(prompt);
        None
    }

    /// Forgets what the ended turn held, leaving the queued prompts queued.
    pub(super) fn end_turn(&mut self) {
        self.active = None;
        self.hand_back = HandBack::Nothing;
        self.forwarded = 0;
        self.drained.clear();
        self.before_switch = None;
    }

    /// Notes that a session switch starts. The prompts queued so far belong to the session it
    /// leaves, and later ones to the session it reaches, so a failed switch holds them.
    pub(super) fn mark_switch(&mut self) {
        self.before_switch = Some(self.pending.len());
        self.hand_back = HandBack::HeldQueue;
    }

    /// Drops the prompts that belong to the session just left, returning how many there were.
    /// Without a [`Self::mark_switch`], that is every queued prompt.
    pub(super) fn discard_left_behind(&mut self) -> usize {
        let count = self.before_switch.take().unwrap_or(self.pending.len());
        self.pending.drain(..count).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue whose turn runs `active` and took in `drained`, with `queued` still waiting.
    fn queue_with(active: &str, drained: &[&str], queued: &[&str]) -> PromptQueue {
        let mut queue = PromptQueue::default();
        for prompt in drained.iter().chain(queued) {
            queue.push((*prompt).to_owned());
        }
        queue.set_active(QueuedPrompt::typed(active.to_owned()));
        for prompt in drained {
            queue.mark_forwarded();
            queue.confirm_drained((*prompt).to_owned());
        }
        queue
    }

    mod hold {
        use super::*;

        #[test]
        fn keeps_the_queue_from_the_turn_until_the_user_submits() {
            let mut queue = queue_with("running", &[], &["held"]);

            queue.hold();
            let while_held = queue.next_to_forward();
            queue.end_turn();
            let after_the_turn = queue.held();
            queue.push("next".to_owned());

            assert_eq!(while_held, None);
            assert!(after_the_turn, "only a submit releases the queue");
            assert!(!queue.held());
            assert_eq!(queue.next_to_forward().as_deref(), Some("held"));
        }
    }

    mod confirm_drained {
        use super::*;

        #[test]
        fn echoes_a_slash_prompt_only_after_compaction_clears_chat() {
            for compacted in [false, true] {
                let mut queue = PromptQueue::default();
                queue.pending.push_back(QueuedPrompt::slash(
                    "/init".to_owned(),
                    "Create AGENTS.md.".to_owned(),
                ));
                if compacted {
                    queue.mark_compacted();
                }
                queue.mark_forwarded();

                let display = queue.confirm_drained("Create AGENTS.md.".to_owned());

                assert_eq!(display.as_deref(), compacted.then_some("/init"));
                assert_eq!(*queue.pending(), [] as [&str; 0]);
            }
        }
    }

    mod fail_turn {
        use super::*;

        #[test]
        fn a_rolled_back_turn_returns_its_prompts_ahead_of_the_queue_in_order() {
            let mut queue = queue_with("active", &["d1", "d2"], &["queued"]);
            queue.mark_rolled_back();

            let failed = queue.fail_turn(false);

            assert_eq!(failed, FailedTurn::HandedBack(None));
            assert_eq!(*queue.pending(), ["active", "d1", "d2", "queued"]);
        }

        #[test]
        fn a_rolled_back_turn_keeps_each_slash_prompt_as_built() {
            let init = |prompt: &str| QueuedPrompt::slash("/init".to_owned(), prompt.to_owned());
            let mut queue = PromptQueue::default();
            queue.pending.push_back(init("Update AGENTS.md."));
            queue.set_active(init("Create AGENTS.md."));
            queue.mark_forwarded();
            queue.confirm_drained("Update AGENTS.md.".to_owned());
            queue.mark_rolled_back();

            let failed = queue.fail_turn(false);

            assert_eq!(failed, FailedTurn::HandedBack(None));
            assert_eq!(
                queue
                    .pending()
                    .iter()
                    .map(QueuedPrompt::prompt)
                    .collect::<Vec<_>>(),
                ["Create AGENTS.md.", "Update AGENTS.md."]
            );
            for mut prompt in queue.pending {
                assert_eq!(prompt.title(), Some("/init"));
                assert_eq!(prompt.take_echo().as_deref(), Some("/init"));
                assert_eq!(prompt.take_echo(), None);
            }
        }

        #[test]
        fn a_rolled_back_turn_returns_its_prompt_for_an_empty_input() {
            let mut queue = queue_with("active", &["d1"], &[]);
            queue.mark_rolled_back();

            let failed = queue.fail_turn(true);

            assert_eq!(
                failed,
                FailedTurn::HandedBack(Some(QueuedPrompt::typed("active".to_owned())))
            );
            assert_eq!(*queue.pending(), ["d1"]);
        }

        #[test]
        fn a_failed_automatic_compaction_holds_the_queue() {
            let mut queue = queue_with("active", &["d1"], &["queued"]);
            queue.mark_compacting();

            let failed = queue.fail_turn(true);

            assert_eq!(failed, FailedTurn::Held);
            assert_eq!(*queue.pending(), ["queued"]);
        }

        #[test]
        fn a_mid_turn_summary_leaves_only_later_prompts_to_a_rollback() {
            let mut queue = queue_with("active", &["d1"], &["d2"]);
            queue.mark_summarized();
            queue.mark_forwarded();
            queue.confirm_drained("d2".to_owned());
            queue.mark_rolled_back();

            let failed = queue.fail_turn(true);

            assert_eq!(failed, FailedTurn::HandedBack(None));
            assert_eq!(*queue.pending(), ["d2"]);
        }

        #[test]
        fn an_unsent_prompt_returns_for_the_input_only_when_nothing_else_waits() {
            for (label, waiting, input_empty, into_input, pending) in [
                ("a lone prompt", &[][..], true, Some("active"), &[][..]),
                ("a draft in the input", &[], false, None, &["active"][..]),
                (
                    "a prompt queued behind it",
                    &["next"],
                    true,
                    None,
                    &["active", "next"][..],
                ),
            ] {
                let mut queue = queue_with("active", &[], waiting);
                queue.mark_unsent();

                let failed = queue.fail_turn(input_empty);

                assert_eq!(
                    failed,
                    FailedTurn::HandedBack(
                        into_input.map(|line| QueuedPrompt::typed(line.to_owned()))
                    ),
                    "{label}"
                );
                assert_eq!(*queue.pending(), pending, "{label}");
            }
        }

        #[test]
        fn a_turn_past_its_compaction_hands_nothing_back() {
            let mut queue = queue_with("active", &["d1"], &["queued"]);
            queue.mark_compacting();
            queue.mark_compacted();

            let failed = queue.fail_turn(true);

            assert_eq!(failed, FailedTurn::Ran);
            assert_eq!(*queue.pending(), ["queued"]);
        }
    }

    mod end_turn {
        use super::*;

        #[test]
        fn forgets_the_ended_turn_so_a_later_failure_hands_back_only_its_own_prompts() {
            let mut queue = queue_with("active", &["d1"], &["queued"]);
            queue.mark_rolled_back();

            queue.end_turn();

            assert_eq!(queue.active(), None);

            let after_end = queue.fail_turn(true);

            assert_eq!(after_end, FailedTurn::Ran, "the hand-back resets");

            queue.set_active(QueuedPrompt::typed("next".to_owned()));
            queue.mark_rolled_back();

            let next_failed = queue.fail_turn(false);

            assert_eq!(next_failed, FailedTurn::HandedBack(None));
            assert_eq!(*queue.pending(), ["next", "queued"]);
        }
    }

    mod discard_left_behind {
        use super::*;

        #[test]
        fn keeps_the_prompts_queued_after_the_switch_started() {
            let mut queue = PromptQueue::default();
            queue.push("before".to_owned());
            queue.mark_switch();
            queue.push("after".to_owned());

            let dropped = queue.discard_left_behind();

            assert_eq!(dropped, 1);
            assert_eq!(*queue.pending(), ["after"]);
        }

        #[test]
        fn drops_every_prompt_once_an_ended_turn_cleared_the_mark() {
            let mut queue = PromptQueue::default();
            queue.push("before".to_owned());
            queue.mark_switch();
            queue.push("after".to_owned());
            queue.end_turn();

            let dropped = queue.discard_left_behind();

            assert_eq!(dropped, 2);
            assert_eq!(*queue.pending(), [] as [&str; 0]);
        }
    }
}
