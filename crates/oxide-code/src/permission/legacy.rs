//! The flat `class(specifier)` rule lists that `permission.allow` and `permission.deny` held before
//! the tier tables, converted for the load error that names their replacement.

use std::fmt::Write as _;

use toml::Value;

use crate::permission::PermissionClass;
use crate::permission::rule::Rule;

/// The load error for `permission.<tier>` set to a list of old rules: the table that replaces it,
/// with each rule the patterns cannot express left as a comment that says why.
pub(crate) fn migration_error(tier: &str, rules: &[Value]) -> String {
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
    format!(
        "permission.{tier} is a list of `class(specifier)` rules, which per-class tables replaced. \
         Write it as:\n\n{}",
        table.trim_end()
    )
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
    Ok((class, pattern))
}

/// Whether `spec` starts with a Windows drive, as `C:/...` does.
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
            "allow",
            &rules(&[
                "bash(cargo test:*)",
                "edit(src/**)",
                "Bash(git status)",
                "read",
                "edit(~/notes/**)",
                "bash(git *)",
                "edit(./docs/*.md)",
            ]),
        );

        assert_eq!(
            message,
            indoc! {r#"
                permission.allow is a list of `class(specifier)` rules, which per-class tables replaced. Write it as:

                [permission.allow]
                read = ["*"]
                edit = ["./src/**", "~/notes/**", "./docs/*.md"]
                exec = ["cargo test *", "git status", "git *"]"#}
        );
    }

    #[test]
    fn migration_error_comments_on_a_rule_the_patterns_cannot_express() {
        let mut old = rules(&["bash(mkfs*)", "bash(* | sh)", "write(x)", "bash(rm -rf:*"]);
        old.push(Value::from(1));

        let message = migration_error("deny", &old);

        let lines: Vec<&str> = message.lines().skip(3).collect();
        assert_eq!(
            lines,
            [
                "# bash(mkfs*) has no equivalent: the word \"mkfs*\" holds a glob, but only a lone `*` is a wildcard. Quote it to match it as written.",
                "# bash(* | sh) has no equivalent: it must be one simple command, such as `cargo test *`",
                "# write(x) has no equivalent: \"write\" is not a permission class",
                "# bash(rm -rf:* has no equivalent: it has an unbalanced parenthesis",
                "# 1 is not a rule string",
            ]
        );
    }

    #[test]
    fn migration_error_quotes_a_pattern_as_a_toml_string() {
        let message = migration_error(
            "deny",
            &rules(&[r#"bash(echo "hi")"#, "read(C:/secrets/**)"]),
        );

        assert_eq!(
            message,
            indoc! {r#"
                permission.deny is a list of `class(specifier)` rules, which per-class tables replaced. Write it as:

                [permission.deny]
                read = ["C:/secrets/**"]
                exec = ['echo "hi"']"#}
        );
    }
}
