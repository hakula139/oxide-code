//! Slash-popup match-and-rank. Filters a registry slice against a typed query (leading `/`
//! stripped) and returns ranked rows for the popup renderer.
//!
//! Ranking: name-prefix → alias-prefix → name-substring → alias-substring, alphabetical within
//! each tier. Empty query returns the registry in declared order.

use super::registry::SlashCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchedCommand {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    /// `None` when the match landed on the canonical name or query is empty.
    pub(crate) matched_alias: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    NamePrefix,
    AliasPrefix,
    NameSubstring,
    AliasSubstring,
}

/// Empty `query` returns the slice in declared order; non-empty matches by prefix or substring,
/// sorted by tier.
pub(crate) fn filter_and_rank(query: &str, commands: &[&dyn SlashCommand]) -> Vec<MatchedCommand> {
    if query.is_empty() {
        return commands
            .iter()
            .map(|cmd| MatchedCommand {
                name: cmd.name(),
                description: cmd.description(),
                matched_alias: None,
            })
            .collect();
    }
    let q = query.to_ascii_lowercase();
    let mut hits: Vec<(Tier, MatchedCommand)> = commands
        .iter()
        .filter_map(|cmd| best_match(&q, *cmd))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(b.1.name)));
    hits.into_iter().map(|(_, m)| m).collect()
}

/// `None` when neither name nor alias match.
fn best_match(query: &str, cmd: &dyn SlashCommand) -> Option<(Tier, MatchedCommand)> {
    let name = cmd.name();
    let name_lower = name.to_ascii_lowercase();
    if name_lower.starts_with(query) {
        return Some((Tier::NamePrefix, on_name(cmd)));
    }
    if let Some(alias) = matching_alias(cmd, |a| a.starts_with(query)) {
        return Some((Tier::AliasPrefix, on_alias(cmd, alias)));
    }
    if name_lower.contains(query) {
        return Some((Tier::NameSubstring, on_name(cmd)));
    }
    if let Some(alias) = matching_alias(cmd, |a| a.contains(query)) {
        return Some((Tier::AliasSubstring, on_alias(cmd, alias)));
    }
    None
}

fn matching_alias(cmd: &dyn SlashCommand, pred: impl Fn(&str) -> bool) -> Option<&'static str> {
    cmd.aliases()
        .iter()
        .copied()
        .find(|alias| pred(&alias.to_ascii_lowercase()))
}

/// Two-tier prefix-then-substring rank over a curated roster, preserving declared order within
/// each tier. Empty `query` returns every item in declared order. Lower-cased internally.
pub(crate) fn rank_by_prefix<'a, T>(
    items: &'a [T],
    query: &str,
    key: impl Fn(&T) -> &str,
) -> Vec<&'a T> {
    if query.is_empty() {
        return items.iter().collect();
    }
    let q = query.to_ascii_lowercase();
    let mut prefix_hits: Vec<&'a T> = Vec::new();
    let mut substring_hits: Vec<&'a T> = Vec::new();
    for item in items {
        let label = key(item).to_ascii_lowercase();
        if label.starts_with(&q) {
            prefix_hits.push(item);
        } else if label.contains(&q) {
            substring_hits.push(item);
        }
    }
    prefix_hits.into_iter().chain(substring_hits).collect()
}

fn on_name(cmd: &dyn SlashCommand) -> MatchedCommand {
    MatchedCommand {
        name: cmd.name(),
        description: cmd.description(),
        matched_alias: None,
    }
}

fn on_alias(cmd: &dyn SlashCommand, alias: &'static str) -> MatchedCommand {
    MatchedCommand {
        name: cmd.name(),
        description: cmd.description(),
        matched_alias: Some(alias),
    }
}

#[cfg(test)]
mod tests {
    use super::super::context::SlashContext;
    use super::super::registry::SlashOutcome;
    use super::*;

    struct Fake {
        name: &'static str,
        aliases: &'static [&'static str],
        description: &'static str,
    }

    impl SlashCommand for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }
        fn description(&self) -> &'static str {
            self.description
        }
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
            Ok(SlashOutcome::Done)
        }
    }

    const HELP: Fake = Fake {
        name: "help",
        aliases: &[],
        description: "list",
    };
    const CLEAR: Fake = Fake {
        name: "clear",
        aliases: &["new", "reset"],
        description: "wipe",
    };
    const STATUS: Fake = Fake {
        name: "status",
        aliases: &[],
        description: "show",
    };
    const COMPACT: Fake = Fake {
        name: "compact",
        aliases: &[],
        description: "fold",
    };

    fn registry() -> Vec<&'static dyn SlashCommand> {
        vec![&HELP, &CLEAR, &STATUS, &COMPACT]
    }

    fn names(rows: &[MatchedCommand]) -> Vec<&'static str> {
        rows.iter().map(|m| m.name).collect()
    }

    // ── filter_and_rank ──

    #[test]
    fn filter_and_rank_empty_query_yields_full_registry_in_declared_order() {
        let rows = filter_and_rank("", &registry());
        assert_eq!(names(&rows), vec!["help", "clear", "status", "compact"]);
        assert!(rows.iter().all(|m| m.matched_alias.is_none()));
    }

    #[test]
    fn filter_and_rank_name_prefix_beats_substring_within_other_command() {
        const FORCONFIG: Fake = Fake {
            name: "forconfig",
            aliases: &[],
            description: "fake",
        };
        let with_substring: Vec<&dyn SlashCommand> = vec![&COMPACT, &FORCONFIG];
        let rows = filter_and_rank("co", &with_substring);
        assert_eq!(names(&rows), vec!["compact", "forconfig"]);
    }

    #[test]
    fn filter_and_rank_alias_prefix_beats_name_substring() {
        const RENEW: Fake = Fake {
            name: "renew",
            aliases: &[],
            description: "fake",
        };
        let with_substring: Vec<&dyn SlashCommand> = vec![&CLEAR, &RENEW];
        let rows = filter_and_rank("new", &with_substring);
        assert_eq!(names(&rows), vec!["clear", "renew"]);
        assert_eq!(rows[0].matched_alias, Some("new"));
        assert_eq!(rows[1].matched_alias, None);
    }

    #[test]
    fn filter_and_rank_within_tier_sorts_alphabetically_on_canonical_name() {
        const SELECT: Fake = Fake {
            name: "select",
            aliases: &[],
            description: "fake",
        };
        let two_prefix: Vec<&dyn SlashCommand> = vec![&STATUS, &SELECT];
        let rows = filter_and_rank("s", &two_prefix);
        assert_eq!(names(&rows), vec!["select", "status"]);
    }

    #[test]
    fn filter_and_rank_alias_match_surfaces_only_typed_alias() {
        let rows = filter_and_rank("res", &registry());
        assert_eq!(names(&rows), vec!["clear"]);
        assert_eq!(rows[0].matched_alias, Some("reset"));
    }

    #[test]
    fn filter_and_rank_alias_substring_lands_below_other_tiers() {
        let rows = filter_and_rank("se", &registry());
        assert_eq!(names(&rows), vec!["clear"]);
        assert_eq!(rows[0].matched_alias, Some("reset"));
    }

    #[test]
    fn filter_and_rank_query_is_case_insensitive() {
        let prefix = filter_and_rank("HE", &registry());
        assert_eq!(names(&prefix), vec!["help"]);

        let substring = filter_and_rank("EAR", &registry());
        assert_eq!(names(&substring), vec!["clear"]);
    }

    #[test]
    fn filter_and_rank_unmatched_query_is_empty() {
        let rows = filter_and_rank("zzz", &registry());
        assert!(rows.is_empty());
    }

    // ── best_match ──

    #[test]
    fn best_match_prefers_name_prefix_over_any_alias_branch() {
        const ALSO: Fake = Fake {
            name: "clearview",
            aliases: &["cl"],
            description: "fake",
        };
        let (tier, m) = best_match("cl", &ALSO).unwrap();
        assert_eq!(tier, Tier::NamePrefix);
        assert_eq!(m.matched_alias, None);
    }

    #[test]
    fn best_match_is_none_when_neither_name_nor_alias_match() {
        assert!(best_match("zzz", &CLEAR).is_none());
    }

    // ── rank_by_prefix ──

    fn levels() -> Vec<&'static str> {
        vec!["low", "medium", "high", "xhigh", "max"]
    }

    fn ranked(query: &str) -> Vec<&'static str> {
        let xs = levels();
        rank_by_prefix(&xs, query, |s| *s)
            .into_iter()
            .copied()
            .collect()
    }

    #[test]
    fn rank_by_prefix_empty_query_yields_full_roster_in_declared_order() {
        assert_eq!(ranked(""), levels());
    }

    #[test]
    fn rank_by_prefix_promotes_prefix_matches_above_substring_within_declared_order() {
        // `h` prefixes `high`; substring-matches `xhigh`. Prefix wins; declared order otherwise.
        assert_eq!(ranked("h"), vec!["high", "xhigh"]);
    }

    #[test]
    fn rank_by_prefix_is_case_insensitive() {
        assert_eq!(ranked("HI"), vec!["high", "xhigh"]);
    }

    #[test]
    fn rank_by_prefix_unmatched_query_is_empty() {
        assert!(ranked("zzz").is_empty());
    }

    // ── Fake fixture ──

    #[test]
    fn fake_execute_is_a_no_op_ok() {
        // Stub so registries containing Fake don't panic during dispatch.
        let mut chat = crate::tui::components::chat::ChatView::new(
            &crate::tui::theme::Theme::default(),
            false,
        );
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        assert!(HELP.execute("anything", &mut ctx).is_ok());
    }
}
