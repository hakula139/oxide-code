//! Slash-popup matching, which ranks the commands a typed query matches by name prefix → alias
//! prefix → name substring → alias substring, alphabetically within each tier.

use ox_core::command;

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

/// Matches `query` by prefix or substring, sorted by tier. An empty `query` returns `commands` in
/// declared order.
pub(super) fn filter_and_rank(commands: &[&dyn SlashCommand], query: &str) -> Vec<MatchedCommand> {
    if query.is_empty() {
        return commands.iter().map(|cmd| on_name(*cmd)).collect();
    }
    let q = query.to_ascii_lowercase();
    let mut hits: Vec<(Tier, MatchedCommand)> = commands
        .iter()
        .filter_map(|cmd| best_match(&q, *cmd))
        .collect();
    hits.sort_by_key(|(tier, m)| (*tier, m.name));
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
    command::aliases(cmd.name())
        .iter()
        .copied()
        .find(|alias| pred(&alias.to_ascii_lowercase()))
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

/// Two-tier prefix-then-substring rank over a curated roster, case-insensitive, preserving
/// declared order within each tier. Empty `query` returns every item in declared order.
pub(super) fn rank_by_prefix<'a, T>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::registry::testing::FakeCmd;

    /// Answers to `/new` and `/reset` through the shared alias table.
    const CLEAR: FakeCmd = FakeCmd {
        name: "clear",
        description: "wipe",
        ..FakeCmd::DEFAULT
    };

    mod filter_and_rank {
        use super::*;

        const HELP: FakeCmd = FakeCmd {
            name: "help",
            description: "list",
            ..FakeCmd::DEFAULT
        };
        const STATUS: FakeCmd = FakeCmd {
            name: "status",
            description: "show",
            ..FakeCmd::DEFAULT
        };
        const COMPACT: FakeCmd = FakeCmd {
            name: "compact",
            description: "fold",
            ..FakeCmd::DEFAULT
        };

        fn registry() -> Vec<&'static dyn SlashCommand> {
            vec![&HELP, &CLEAR, &STATUS, &COMPACT]
        }

        fn names(rows: &[MatchedCommand]) -> Vec<&'static str> {
            rows.iter().map(|m| m.name).collect()
        }

        #[test]
        fn empty_query_is_full_registry_in_declared_order() {
            let rows = filter_and_rank(&registry(), "");
            assert_eq!(names(&rows), vec!["help", "clear", "status", "compact"]);
            assert!(rows.iter().all(|m| m.matched_alias.is_none()));
        }

        #[test]
        fn name_prefix_beats_substring_within_other_command() {
            const FORCONFIG: FakeCmd = FakeCmd {
                name: "forconfig",
                description: "fake",
                ..FakeCmd::DEFAULT
            };
            let with_substring: Vec<&dyn SlashCommand> = vec![&COMPACT, &FORCONFIG];
            let rows = filter_and_rank(&with_substring, "co");
            assert_eq!(names(&rows), vec!["compact", "forconfig"]);
        }

        #[test]
        fn alias_prefix_beats_name_substring() {
            const RENEW: FakeCmd = FakeCmd {
                name: "renew",
                description: "fake",
                ..FakeCmd::DEFAULT
            };
            let with_substring: Vec<&dyn SlashCommand> = vec![&CLEAR, &RENEW];
            let rows = filter_and_rank(&with_substring, "new");
            assert_eq!(names(&rows), vec!["clear", "renew"]);
            assert_eq!(rows[0].matched_alias, Some("new"));
            assert_eq!(rows[1].matched_alias, None);
        }

        #[test]
        fn within_tier_sorts_alphabetically_on_canonical_name() {
            const SELECT: FakeCmd = FakeCmd {
                name: "select",
                description: "fake",
                ..FakeCmd::DEFAULT
            };
            let two_prefix: Vec<&dyn SlashCommand> = vec![&STATUS, &SELECT];
            let rows = filter_and_rank(&two_prefix, "s");
            assert_eq!(names(&rows), vec!["select", "status"]);
        }

        #[test]
        fn alias_match_surfaces_only_typed_alias() {
            let rows = filter_and_rank(&registry(), "res");
            assert_eq!(names(&rows), vec!["clear"]);
            assert_eq!(rows[0].matched_alias, Some("reset"));
        }

        #[test]
        fn alias_substring_lands_below_other_tiers() {
            let rows = filter_and_rank(&registry(), "se");
            assert_eq!(names(&rows), vec!["clear"]);
            assert_eq!(rows[0].matched_alias, Some("reset"));
        }

        #[test]
        fn query_is_case_insensitive() {
            let prefix = filter_and_rank(&registry(), "HE");
            assert_eq!(names(&prefix), vec!["help"]);

            let substring = filter_and_rank(&registry(), "EAR");
            assert_eq!(names(&substring), vec!["clear"]);
        }

        #[test]
        fn unmatched_query_is_empty() {
            let rows = filter_and_rank(&registry(), "zzz");
            assert_eq!(rows, []);
        }
    }

    mod best_match {
        use super::*;

        #[test]
        fn finds_nothing_when_neither_name_nor_alias_match() {
            assert!(best_match("zzz", &CLEAR).is_none());
        }
    }

    mod rank_by_prefix {
        use super::*;

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
        fn empty_query_yields_full_roster_in_declared_order() {
            assert_eq!(ranked(""), levels());
        }

        #[test]
        fn promotes_prefix_above_substring() {
            assert_eq!(ranked("h"), vec!["high", "xhigh"]);
        }

        #[test]
        fn is_case_insensitive() {
            assert_eq!(ranked("HI"), vec!["high", "xhigh"]);
        }

        #[test]
        fn unmatched_query_is_empty() {
            assert_eq!(ranked("zzz"), [] as [&str; 0]);
        }
    }
}
