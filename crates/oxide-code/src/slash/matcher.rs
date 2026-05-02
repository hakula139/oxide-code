//! Slash-popup match-and-rank.
//!
//! Filters a registry slice against a typed query (the buffer minus
//! the leading `/`) and returns ranked rows ready for the popup
//! renderer. Pure data — no ratatui types — so the popup component
//! can hand-roll fixtures in tests without dragging in `BUILT_INS`.
//!
//! Ranking ladder: name-prefix → alias-prefix → name-substring →
//! alias-substring, alphabetical (canonical name) within each tier.
//! Empty query bypasses ranking and returns the registry in
//! presentation order (`BUILT_INS`'s declared order).
//!
//! Aliases display conditionally: a match against the canonical name
//! leaves [`MatchedCommand::matched_alias`] as `None`; a match against
//! an alias surfaces that one alias only — the popup never paints a
//! full alias list.

use super::registry::SlashCommand;

/// One row of popup output. Renderer-agnostic so the popup can be
/// tested without depending on `BUILT_INS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchedCommand {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    /// The alias the user's query matched, if any. `None` when the
    /// match landed on the canonical name or when the query is empty.
    pub(crate) matched_alias: Option<&'static str>,
}

/// Rank tier — lower is shown first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    NamePrefix,
    AliasPrefix,
    NameSubstring,
    AliasSubstring,
}

/// Filter and rank `commands` against `query` (the buffer with the
/// leading `/` stripped). Empty `query` returns every command in the
/// slice's declared order; non-empty queries return only commands
/// that match by prefix or substring on the name or any alias,
/// sorted by tier then alphabetically on canonical name.
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

/// Score one command against a lowercase query. Returns the best
/// tier the command qualifies for, with the matching alias surfaced
/// when the match landed on an alias rather than the canonical name.
/// `None` ⇒ neither name nor any alias matched.
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

/// First alias whose lowercase form satisfies `pred`. Lowercases
/// once per alias rather than per match, since the predicate runs
/// against the lowercased copy.
fn matching_alias(cmd: &dyn SlashCommand, pred: impl Fn(&str) -> bool) -> Option<&'static str> {
    cmd.aliases()
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

#[cfg(test)]
mod tests {
    use super::super::context::SlashContext;
    use super::*;

    /// Synthetic registry fixture so tests pin the matcher's
    /// behaviour independent of which built-ins ship today.
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
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
            Ok(())
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
    fn filter_and_rank_empty_query_returns_full_registry_in_declared_order() {
        // Empty query is the popup's "just typed `/`" state. Order
        // mirrors `BUILT_INS`'s declared order so frequently-used
        // commands stay first.
        let rows = filter_and_rank("", &registry());
        assert_eq!(names(&rows), vec!["help", "clear", "status", "compact"]);
        // Empty query never surfaces an alias — the slot is always None.
        assert!(rows.iter().all(|m| m.matched_alias.is_none()));
    }

    #[test]
    fn filter_and_rank_name_prefix_beats_substring_within_other_command() {
        // Query "co" prefixes `compact` and substrings `status`-not-really
        // (no `co` in `status`), so just `compact` returns. Add a
        // synthetic substring case to make the tier ordering observable.
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
        // `new` prefixes the alias `new`, so /clear sits above any
        // command that only substring-matches `n`. Adding a synthetic
        // command containing `new` mid-name makes the tier ordering
        // observable rather than vacuous.
        const RENEW: Fake = Fake {
            name: "renew",
            aliases: &[],
            description: "fake",
        };
        let with_substring: Vec<&dyn SlashCommand> = vec![&CLEAR, &RENEW];
        let rows = filter_and_rank("new", &with_substring);
        // CLEAR matches via alias prefix; RENEW matches via name substring.
        assert_eq!(names(&rows), vec!["clear", "renew"]);
        assert_eq!(rows[0].matched_alias, Some("new"));
        assert_eq!(rows[1].matched_alias, None);
    }

    #[test]
    fn filter_and_rank_within_tier_sorts_alphabetically_on_canonical_name() {
        // Two commands both match by name prefix (`s` ⇒ `status` and
        // a synthetic `select`). Tier is equal; secondary sort is
        // alphabetical on canonical name.
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
        // Two aliases on /clear; query matches one. `matched_alias`
        // must surface the typed alias, not the full list.
        let rows = filter_and_rank("res", &registry());
        assert_eq!(names(&rows), vec!["clear"]);
        assert_eq!(rows[0].matched_alias, Some("reset"));
    }

    #[test]
    fn filter_and_rank_query_is_case_insensitive() {
        // Uppercase query must match lowercase canonical names. Pin
        // both prefix and substring cases since they take separate
        // code paths through `to_ascii_lowercase`.
        let prefix = filter_and_rank("HE", &registry());
        assert_eq!(names(&prefix), vec!["help"]);

        let substring = filter_and_rank("EAR", &registry());
        assert_eq!(names(&substring), vec!["clear"]);
    }

    #[test]
    fn filter_and_rank_unmatched_query_returns_empty() {
        let rows = filter_and_rank("zzz", &registry());
        assert!(rows.is_empty());
    }

    // ── best_match ──

    #[test]
    fn best_match_prefers_name_prefix_over_any_alias_branch() {
        // A query that prefixes both the canonical name and an alias
        // (synthetic `clearview` aliased to `cl`) must report
        // `Tier::NamePrefix` so the popup labels it on the canonical
        // name rather than the alias.
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
    fn best_match_returns_none_when_neither_name_nor_alias_match() {
        assert!(best_match("zzz", &CLEAR).is_none());
    }
}
