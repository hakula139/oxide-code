//! The flat `class(specifier)` rule lists that `permission.allow` and `permission.deny` held before
//! the tier tables, converted for the load error that names their replacement.

use std::fmt::Write as _;

use indoc::formatdoc;
use toml::Value;

use crate::permission::rule::Rule;
use crate::permission::shell::{PatternWord, pattern_words};
use crate::permission::{PermissionClass, Tier};

/// The load error for `permission.<tier>` set to a list of old rules: the table that replaces it,
/// with each rule the patterns cannot express left as a comment that says why.
pub(crate) fn migration_error(tier: Tier, rules: &[Value]) -> String {
    let mut entries: Vec<(PermissionClass, Vec<String>)> = Vec::new();
    let mut comments = Vec::new();
    for value in rules {
        let Some(raw) = value.as_str() else {
            comments.push(format!("# {value} is not a rule string"));
            continue;
        };
        match convert(raw) {
            Ok((class, pattern)) => match entries.iter_mut().find(|(c, _)| *c == class) {
                Some((_, patterns)) => patterns.push(pattern),
                None => entries.push((class, vec![pattern])),
            },
            Err(reason) => comments.push(format!("# {raw} has no equivalent: {reason}")),
        }
    }
    entries.sort_by_key(|(class, _)| *class);
    if tier == Tier::Allow
        && entries
            .iter()
            .any(|(class, patterns)| widens(*class, patterns))
    {
        comments.push(
            "# A path pattern now also covers everything below a directory it matches.".to_owned(),
        );
    }

    let mut table = format!("[permission.{tier}]\n");
    for (class, patterns) in entries {
        let quoted: Vec<String> = patterns
            .into_iter()
            .map(|pattern| Value::String(pattern).to_string())
            .collect();
        _ = writeln!(table, "{class} = [{}]", quoted.join(", "));
    }
    for comment in comments {
        _ = writeln!(table, "{comment}");
    }
    formatdoc! {"
        permission.{tier} is an old `class(specifier)` rule list, which oxide-code no longer reads. Remove it and add this table at the end of the file:

        {}",
        table.trim_end()
    }
}

/// The class and pattern an old rule converts to, or why it has none. `bash` becomes `exec`, a
/// `:*` prefix rule ends in a `*` word, and a relative path glob gains the `./` that keeps it
/// anchored at the cwd, where old relative globs applied.
fn convert(raw: &str) -> Result<(PermissionClass, String), String> {
    let raw = raw.trim();
    let (name, spec) = match (raw.find('('), raw.strip_suffix(')')) {
        (Some(open), Some(_)) => (&raw[..open], raw[open + 1..raw.len() - 1].trim()),
        (None, None) => (raw, ""),
        _ => return Err("it has an unbalanced parenthesis".to_owned()),
    };
    let class = match name.trim().to_lowercase().as_str() {
        "bash" => PermissionClass::Exec,
        "read" => PermissionClass::Read,
        "edit" => PermissionClass::Edit,
        _ => return Err(format!("{name:?} is not a permission class")),
    };
    let pattern = if spec.is_empty() || spec == "*" {
        "*".to_owned()
    } else if class == PermissionClass::Exec {
        match spec.strip_suffix(":*") {
            Some(prefix) => format!("{} *", prefix.trim_end()),
            None => spec.to_owned(),
        }
    } else if spec.starts_with(['/', '~']) || spec.starts_with("./") || is_drive_path(spec) {
        spec.to_owned()
    } else {
        format!("./{spec}")
    };
    Rule::parse(class, &pattern).map_err(|e| format!("{e:#}"))?;
    if class == PermissionClass::Exec && has_inner_star(&pattern) {
        return Err(
            "an old `*` matched any run of characters, and a `*` before the last word now \
             matches exactly one word"
                .to_owned(),
        );
    }
    Ok((class, pattern))
}

fn has_inner_star(pattern: &str) -> bool {
    pattern_words(pattern).is_ok_and(|words| {
        words
            .split_last()
            .is_some_and(|(_, rest)| rest.contains(&PatternWord::Any))
    })
}

/// Whether one of the converted path `patterns` may now cover more than the old rule, which matched
/// only the paths its glob named. A pattern ending in `**` already covered everything below.
fn widens(class: PermissionClass, patterns: &[String]) -> bool {
    matches!(class, PermissionClass::Read | PermissionClass::Edit)
        && patterns
            .iter()
            .any(|pattern| pattern != "*" && !pattern.ends_with("**"))
}

fn is_drive_path(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    fn rules(raw: &[&str]) -> Vec<Value> {
        raw.iter().map(|rule| Value::from(*rule)).collect()
    }

    // ── migration_error ──

    #[test]
    fn migration_error_shows_the_table_that_replaces_each_old_rule() {
        let message = migration_error(
            Tier::Allow,
            &rules(&[
                "bash(cargo test:*)",
                "edit(src/**)",
                "Bash(git status)",
                "read",
                "edit(~/notes/**)",
                "bash(git *)",
                "edit(./docs/**)",
            ]),
        );

        assert_eq!(
            message,
            indoc! {r#"
                permission.allow is an old `class(specifier)` rule list, which oxide-code no longer reads. Remove it and add this table at the end of the file:

                [permission.allow]
                read = ["*"]
                edit = ["./src/**", "~/notes/**", "./docs/**"]
                exec = ["cargo test *", "git status", "git *"]"#}
        );
    }

    #[test]
    fn migration_error_comments_on_a_rule_the_patterns_cannot_express() {
        let mut old = rules(&[
            "bash(mkfs*)",
            "bash(* | sh)",
            "bash(git * --force)",
            "write(x)",
            "bash(rm -rf:*",
        ]);
        old.push(Value::from(1));

        let message = migration_error(Tier::Deny, &old);

        let lines: Vec<&str> = message.lines().skip(3).collect();
        assert_eq!(
            lines,
            [
                "# bash(mkfs*) has no equivalent: the word \"mkfs*\" holds a glob, but only a `*` standing alone as a word is a wildcard. Write a pattern for each command it should cover, or quote the word to match it as written.",
                "# bash(* | sh) has no equivalent: it must be one simple command, such as `cargo test *`. Write a pattern for each command of a chain or pipeline.",
                "# bash(git * --force) has no equivalent: an old `*` matched any run of characters, and a `*` before the last word now matches exactly one word",
                "# write(x) has no equivalent: \"write\" is not a permission class",
                "# bash(rm -rf:* has no equivalent: it has an unbalanced parenthesis",
                "# 1 is not a rule string",
            ]
        );
    }

    #[test]
    fn migration_error_notes_that_a_converted_allow_path_now_covers_its_contents() {
        let notice = "# A path pattern now also covers everything below a directory it matches.";
        for (tier, rule, noted) in [
            (Tier::Allow, "edit(~/notes)", true),
            (Tier::Allow, "read(src/*.rs)", true),
            (Tier::Allow, "edit(src/**)", false),
            (Tier::Allow, "edit", false),
            (Tier::Allow, "bash(ls)", false),
            (Tier::Deny, "edit(~/notes)", false),
        ] {
            let message = migration_error(tier, &rules(&[rule]));

            assert_eq!(message.ends_with(notice), noted, "{tier} {rule}: {message}");
        }
    }

    #[test]
    fn migration_error_quotes_a_pattern_as_a_toml_string() {
        let message = migration_error(
            Tier::Deny,
            &rules(&[r#"bash(echo "hi")"#, "read(C:/secrets/**)"]),
        );

        assert_eq!(
            message,
            indoc! {r#"
                permission.deny is an old `class(specifier)` rule list, which oxide-code no longer reads. Remove it and add this table at the end of the file:

                [permission.deny]
                read = ["C:/secrets/**"]
                exec = ['echo "hi"']"#}
        );
    }
}
