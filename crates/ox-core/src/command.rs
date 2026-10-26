//! Slash-command parsing, aliases, and typed [`Op`] forms shared by the TUI and bare REPL. The TUI
//! adds pickers and modals.

pub mod init;
mod parser;

pub use self::parser::{Parsed, parse_slash, split_slash, unescape_literal};

use std::path::Path;

use indoc::formatdoc;

use crate::model::{Effort, MODELS, ModelInfo, ResolvedModelId};
use crate::permission::Mode;
use crate::protocol::{ConfigPick, Op, ResumeTarget};
use crate::session::display::fit_title;
use crate::util::variant;

/// One slash command [`typed_op`] parses: its canonical name, aliases, and parser.
struct Command {
    name: &'static str,
    aliases: &'static [&'static str],
    parse: fn(&str, &Path) -> Result<Op, String>,
}

/// The slash commands [`typed_op`] parses, in completion order. `/init` reads instructions at the
/// project root.
const COMMANDS: [Command; 9] = [
    Command {
        name: "clear",
        aliases: &["new", "reset"],
        parse: |_, _| Ok(Op::Clear),
    },
    Command {
        name: "compact",
        aliases: &[],
        parse: |args, _| Ok(compact(args)),
    },
    Command {
        name: "effort",
        aliases: &[],
        parse: |args, _| effort(args),
    },
    Command {
        name: "exit",
        aliases: &["quit"],
        parse: |_, _| Ok(Op::Shutdown),
    },
    Command {
        name: "init",
        aliases: &[],
        parse: init::parse,
    },
    Command {
        name: "mode",
        aliases: &[],
        parse: |args, _| mode(args),
    },
    Command {
        name: "model",
        aliases: &[],
        parse: |args, _| model(args),
    },
    Command {
        name: "rename",
        aliases: &[],
        parse: |args, _| rename(args),
    },
    Command {
        name: "resume",
        aliases: &["continue"],
        parse: |args, _| resume(args),
    },
];

/// The canonical names of the commands [`typed_op`] parses, without their leading `/`.
pub fn names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|command| command.name)
}

/// Resolves aliases and parses `parsed` into an op at `project_root`. Errors use the typed command
/// name, and prompts use the command line as their title. `None` without a typed form.
pub fn typed_op(parsed: &Parsed, project_root: &Path) -> Option<Result<Op, String>> {
    let command = find(canonical(&parsed.name))?;
    let mut op = (command.parse)(&parsed.args, project_root);
    if let Ok(Op::Prompt { title, .. }) = &mut op {
        *title = Some(parsed.line());
    }
    Some(op.map_err(|msg| parsed.named_error(&msg)))
}

/// The command `name` runs, canonically or as one of its aliases.
fn find(name: &str) -> Option<&'static Command> {
    COMMANDS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

/// The canonical name `name` stands for: the command it is an alias of, or `name` itself.
fn canonical(name: &str) -> &str {
    find(name).map_or(name, |command| command.name)
}

/// The aliases of the command whose canonical name is `name`.
pub fn aliases(name: &str) -> &'static [&'static str] {
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map_or(&[], |command| command.aliases)
}

/// `/compact [<instructions>]`, where trailing text focuses the summary.
pub fn compact(args: &str) -> Op {
    let instructions = args.trim();
    Op::Compact {
        instructions: (!instructions.is_empty()).then(|| instructions.to_owned()),
    }
}

/// `/effort <level>`, case-insensitive.
pub fn effort(arg: &str) -> Result<Op, String> {
    let usage = format!("/effort <{}>", variant::names_joined::<Effort>("|"));
    let arg = required(arg, "level", &usage)?;
    let pick = arg.to_ascii_lowercase().parse().map_err(|_| {
        format!(
            "Unknown effort: `{arg}`. Valid: {}.",
            variant::names::<Effort>()
        )
    })?;
    Ok(Op::SetModel(ConfigPick::Effort(pick)))
}

/// `/mode <mode>`, case-insensitive.
pub fn mode(arg: &str) -> Result<Op, String> {
    let usage = format!("/mode <{}>", variant::names_joined::<Mode>("|"));
    let arg = required(arg, "mode", &usage)?;
    let mode = arg.to_ascii_lowercase().parse::<Mode>().map_err(|_| {
        format!(
            "Unknown mode: `{arg}`. Valid: {}.",
            variant::names::<Mode>()
        )
    })?;
    Ok(Op::SetMode(mode))
}

/// `/model <id>`. Matches an exact catalog ID, then a unique substring of one, so family names such
/// as `opus` work as aliases.
pub fn model(arg: &str) -> Result<Op, String> {
    let arg = required(arg, "model ID", "/model <id>")?.to_ascii_lowercase();
    let hits: Vec<&ModelInfo> = match MODELS.iter().find(|info| info.id == arg) {
        Some(exact) => vec![exact],
        None => MODELS
            .iter()
            .filter(|info| info.id.contains(&arg))
            .collect(),
    };
    let info = match hits.as_slice() {
        [info] => info,
        [_, ..] => {
            let aliases = MODELS
                .iter()
                .map(|info| format!("`{}`", info.family.id_token()))
                .collect::<Vec<_>>()
                .join(", ");
            let models = format_models(hits);
            return Err(formatdoc! {"
                Model `{arg}` is ambiguous. Pick one or use a short alias ({aliases}):

                {models}"
            });
        }
        [] => {
            let hint = replacement_for(&arg)
                .map_or_else(String::new, |info| format!(" Did you mean `{}`?", info.id));
            let models = format_models(MODELS);
            return Err(formatdoc! {"
                Unknown model: `{arg}`.{hint} Valid:

                {models}"
            });
        }
    };
    Ok(Op::SetModel(ConfigPick::Model {
        id: ResolvedModelId::new(info.id),
        effort: None,
    }))
}

/// `/rename <title>`, fitted to the session's title cap.
pub fn rename(title: &str) -> Result<Op, String> {
    let title = required(title, "title", "/rename <title>")?;
    Ok(Op::Rename {
        title: fit_title(title),
    })
}

/// `/resume <id-prefix>`. The engine resolves the prefix against the saved sessions.
pub fn resume(prefix: &str) -> Result<Op, String> {
    let prefix = required(prefix, "session ID", "/resume <id-prefix>")?;
    Ok(Op::Resume {
        target: ResumeTarget::Prefix(prefix.to_owned()),
    })
}

/// The trimmed `arg`, or the [`missing`] error for `what` and the form's `usage`.
fn required<'a>(arg: &'a str, what: &str, usage: &str) -> Result<&'a str, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(missing(what, usage, None));
    }
    Ok(arg)
}

/// The error for a form typed without its `what` argument, pointing at its `usage` and, when given,
/// an `alternative` that needs no argument.
pub fn missing(what: &str, usage: &str, alternative: Option<&str>) -> String {
    let alternative = alternative
        .map(|alt| format!(", or {alt}"))
        .unwrap_or_default();
    format!("Missing {what}. Usage: `{usage}`{alternative}.")
}

/// Markdown bullet list pairing each model's ID with its display name.
fn format_models<'a>(models: impl IntoIterator<Item = &'a ModelInfo>) -> String {
    models
        .into_iter()
        .map(|info| format!("- `{}` — {}", info.id, info.display_name))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::COMMANDS;

    /// The canonical names of the commands that have aliases.
    pub fn aliased_names() -> impl Iterator<Item = &'static str> {
        COMMANDS
            .iter()
            .filter(|command| !command.aliases.is_empty())
            .map(|command| command.name)
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::session::display::MAX_TITLE_WIDTH;

    fn set_model(id: &str) -> Op {
        Op::SetModel(ConfigPick::Model {
            id: ResolvedModelId::new(id),
            effort: None,
        })
    }

    fn set_effort(level: Effort) -> Op {
        Op::SetModel(ConfigPick::Effort(level))
    }

    mod typed_op {
        use super::*;

        /// The op the typed `line` runs at `/repo`.
        fn typed(line: &str) -> Option<Result<Op, String>> {
            typed_op(&parse_slash(line).unwrap(), Path::new("/repo"))
        }

        /// The op the canonical `name` with `args` runs at `root`.
        fn parsed(name: &str, args: &str, root: &Path) -> Option<Result<Op, String>> {
            typed_op(
                &Parsed {
                    name: name.to_owned(),
                    args: args.to_owned(),
                },
                root,
            )
        }

        #[test]
        fn routes_an_alias_to_its_command() {
            assert_eq!(typed("/new"), Some(Ok(Op::Clear)));
            assert_eq!(
                typed("/continue ab12"),
                Some(Ok(Op::Resume {
                    target: ResumeTarget::Prefix("ab12".to_owned()),
                })),
            );
        }

        #[test]
        fn titles_a_prompt_after_the_command_line() {
            assert_eq!(
                typed("/init AGENTS.md"),
                Some(Ok(Op::Prompt {
                    text: init::create_prompt(Path::new("/repo"), "AGENTS.md"),
                    title: Some("/init AGENTS.md".to_owned()),
                })),
            );
        }

        #[test]
        fn error_names_the_command_as_typed() {
            assert_eq!(
                typed("/continue"),
                Some(Err(
                    "/continue: Missing session ID. Usage: `/resume <id-prefix>`.".to_owned()
                )),
            );
        }

        #[test]
        fn command_without_a_typed_form_or_an_unknown_one_has_no_op() {
            assert_eq!(typed("/help"), None);
            assert_eq!(typed("/no-such-thing x"), None);
        }

        #[test]
        fn routes_each_name_to_its_form() {
            let root = tempfile::tempdir().unwrap();
            let cases = [
                ("clear", "", Op::Clear),
                ("compact", "auth", compact("auth")),
                ("effort", "low", set_effort(Effort::Low)),
                ("exit", "", Op::Shutdown),
                (
                    "init",
                    "AGENTS.md",
                    Op::Prompt {
                        text: init::create_prompt(root.path(), "AGENTS.md"),
                        title: Some("/init AGENTS.md".to_owned()),
                    },
                ),
                ("mode", "plan", Op::SetMode(Mode::Plan)),
                ("model", "haiku", set_model("claude-haiku-5-5")),
                ("rename", "Fix login", rename("Fix login").unwrap()),
                ("resume", "1a2b", resume("1a2b").unwrap()),
            ];

            assert_eq!(
                cases.each_ref().map(|(name, ..)| *name).to_vec(),
                names().collect::<Vec<_>>()
            );
            for (name, args, op) in cases {
                assert_eq!(parsed(name, args, root.path()), Some(Ok(op)), "{name}");
            }
        }

        #[test]
        fn knows_no_other_name() {
            for name in ["help", "theme", "compact-all", "no-such-thing"] {
                assert_eq!(parsed(name, "x", Path::new("/repo")), None, "{name}");
            }
        }

        #[test]
        fn bare_forms_that_need_an_argument_name_the_usage() {
            for (name, usage) in [
                ("effort", "Missing level. Usage: `/effort <level>`."),
                ("mode", "Missing mode. Usage: `/mode <mode>`."),
                ("model", "Missing model ID. Usage: `/model <id>`."),
                ("rename", "Missing title. Usage: `/rename <title>`."),
                (
                    "resume",
                    "Missing session ID. Usage: `/resume <id-prefix>`.",
                ),
            ] {
                assert_eq!(
                    parsed(name, "  ", Path::new("/repo")),
                    Some(Err(format!("/{name}: {usage}"))),
                    "{name}",
                );
            }
        }
    }

    mod canonical {
        use super::*;

        #[test]
        fn resolves_each_alias_to_its_command() {
            for (alias, name) in [
                ("new", "clear"),
                ("reset", "clear"),
                ("quit", "exit"),
                ("continue", "resume"),
            ] {
                assert_eq!(canonical(alias), name, "{alias}");
            }
        }

        #[test]
        fn keeps_a_canonical_or_unknown_name() {
            for name in ["clear", "resume", "help", "no-such-thing"] {
                assert_eq!(canonical(name), name);
            }
        }
    }

    mod aliases {
        use super::*;

        #[test]
        fn lists_the_aliases_of_a_canonical_name_only() {
            assert_eq!(aliases("clear"), ["new", "reset"]);
            assert_eq!(aliases("resume"), ["continue"]);
            for name in ["help", "new", "no-such-thing"] {
                assert_eq!(aliases(name), [] as [&str; 0], "{name}");
            }
        }
    }

