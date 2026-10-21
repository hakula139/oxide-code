//! Slash-command input shared by the TUI's slash commands and the bare REPL: the line parser, the
//! command aliases, and the typed forms parsed into the [`Op`] each one runs. The TUI adds the
//! pickers and modals behind the bare forms.

pub mod init;
mod parser;

pub use parser::{Parsed, parse_slash, split_slash, unescape_literal};

use std::path::Path;

use indoc::formatdoc;

use crate::config::Effort;
use crate::model::{MODELS, ModelInfo, ResolvedModelId, replacement_for};
use crate::permission::Mode;
use crate::protocol::{ConfigPick, Op, ResumeTarget};
use crate::session::display::fit_title;

/// Canonical names of the commands [`typed_op`] parses, without their leading `/`.
pub const NAMES: [&str; 8] = [
    "clear", "compact", "effort", "init", "mode", "model", "rename", "resume",
];

/// Each command's aliases, keyed by its canonical name.
const ALIASES: [(&str, &[&str]); 2] = [("clear", &["new", "reset"]), ("resume", &["continue"])];

/// The op the typed form of `parsed` runs at `project_root`, with an alias resolved to its command
/// and an error prefixed by the name the user typed. `None` when the command has no typed form.
pub fn typed_op(parsed: &Parsed, project_root: &Path) -> Option<Result<Op, String>> {
    let op = parse(canonical(&parsed.name), &parsed.args, project_root)?;
    Some(op.map_err(|msg| parsed.named_error(&msg)))
}

/// The canonical name `name` stands for: the command it is an alias of, or `name` itself.
fn canonical(name: &str) -> &str {
    ALIASES
        .iter()
        .find(|(_, aliases)| aliases.contains(&name))
        .map_or(name, |&(canonical, _)| canonical)
}

/// The aliases of the command whose canonical name is `name`.
pub fn aliases(name: &str) -> &'static [&'static str] {
    ALIASES
        .iter()
        .find(|(canonical, _)| *canonical == name)
        .map_or(&[], |&(_, aliases)| aliases)
}

/// Parses the typed form of the command `name`, canonical and without its `/`, into the op it
/// runs, where `/init` reads the instruction files at `project_root`. `None` when `name` is not
/// one of [`NAMES`]. `Err` carries the message for a malformed form, including a bare form only a
/// picker can complete.
fn parse(name: &str, args: &str, project_root: &Path) -> Option<Result<Op, String>> {
    Some(match name {
        "clear" => Ok(Op::Clear),
        "compact" => Ok(compact(args)),
        "effort" => effort(args),
        "init" => init::parse(args, project_root),
        "mode" => mode(args),
        "model" => model(args),
        "rename" => rename(args),
        "resume" => resume(args),
        _ => return None,
    })
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
    let arg = required(arg, "level", "/effort <level>")?;
    let pick = arg
        .to_ascii_lowercase()
        .parse()
        .map_err(|_| format!("Unknown effort: `{arg}`. Valid: {}.", Effort::VALID_VALUES))?;
    Ok(Op::SetModel(ConfigPick::Effort(pick)))
}

/// `/mode <mode>`, case-insensitive.
pub fn mode(arg: &str) -> Result<Op, String> {
    let arg = required(arg, "mode", "/mode <mode>")?;
    let mode = arg
        .to_ascii_lowercase()
        .parse::<Mode>()
        .map_err(|_| format!("Unknown mode: `{arg}`. Valid: {}.", Mode::VALID_VALUES))?;
    Ok(Op::SetMode(mode))
}

/// `/model <id>`. Matches an exact catalog ID, then a unique substring of one, so family names
/// such as `opus` work as aliases.
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
        id: ResolvedModelId::new(info.id.to_owned()),
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

/// The error for a form typed without its `what` argument, pointing at its `usage` and, when
/// given, an `alternative` that needs no argument.
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
    use super::ALIASES;

    /// The canonical names of the commands that have aliases.
    pub fn aliased_names() -> impl Iterator<Item = &'static str> {
        ALIASES.iter().map(|&(name, _)| name)
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::session::display::MAX_TITLE_WIDTH;

    fn set_model(id: &str) -> Op {
        Op::SetModel(ConfigPick::Model {
            id: ResolvedModelId::new(id.to_owned()),
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
    }

    mod canonical {
        use super::*;

        #[test]
        fn resolves_each_alias_to_its_command() {
            for (alias, name) in [("new", "clear"), ("reset", "clear"), ("continue", "resume")] {
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

    mod parse {
        use super::*;

        #[test]
        fn routes_each_name_to_its_form() {
            let root = tempfile::tempdir().unwrap();
            let cases = [
                ("clear", "", Op::Clear),
                ("compact", "auth", compact("auth")),
                ("effort", "low", set_effort(Effort::Low)),
                (
                    "init",
                    "AGENTS.md",
                    Op::Prompt(init::create_prompt(root.path(), "AGENTS.md")),
                ),
                ("mode", "plan", Op::SetMode(Mode::Plan)),
                ("model", "haiku", set_model("claude-haiku-5-5")),
                ("rename", "Fix login", rename("Fix login").unwrap()),
                ("resume", "1a2b", resume("1a2b").unwrap()),
            ];

            assert_eq!(cases.each_ref().map(|(name, ..)| *name), NAMES);
            for (name, args, op) in cases {
                assert_eq!(parse(name, args, root.path()), Some(Ok(op)), "{name}");
            }
        }

        #[test]
        fn knows_no_other_name() {
            for name in ["help", "new", "theme", "compact-all", ""] {
                assert_eq!(parse(name, "x", Path::new("/repo")), None, "{name}");
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
                    parse(name, "  ", Path::new("/repo")),
                    Some(Err(usage.to_owned())),
                    "{name}",
                );
            }
        }
    }

    mod compact {
        use super::*;

        #[test]
        fn trims_the_instructions_and_drops_blank_ones() {
            assert_eq!(compact("   \n\t  "), Op::Compact { instructions: None });
            assert_eq!(
                compact("  focus on the build error  "),
                Op::Compact {
                    instructions: Some("focus on the build error".to_owned()),
                },
            );
        }
    }

    mod effort {
        use super::*;

        #[test]
        fn accepts_each_level_in_any_case() {
            for (arg, level) in [
                ("low", Effort::Low),
                ("Medium", Effort::Medium),
                ("high", Effort::High),
                ("XHIGH", Effort::Xhigh),
                (" MaX ", Effort::Max),
            ] {
                assert_eq!(effort(arg), Ok(set_effort(level)), "{arg}");
            }
        }

        #[test]
        fn unknown_level_lists_the_valid_ones() {
            for arg in ["turbo", "auto"] {
                assert_eq!(
                    effort(arg),
                    Err(format!(
                        "Unknown effort: `{arg}`. Valid: low, medium, high, xhigh, max."
                    )),
                );
            }
        }
    }

    mod mode {
        use super::*;

        #[test]
        fn accepts_each_mode_in_any_case() {
            for (arg, expected) in [
                ("manual", Mode::Manual),
                (" Plan ", Mode::Plan),
                ("YOLO", Mode::Yolo),
            ] {
                assert_eq!(mode(arg), Ok(Op::SetMode(expected)), "{arg}");
            }
        }

        #[test]
        fn unknown_mode_lists_the_valid_ones() {
            assert_eq!(
                mode("Turbo"),
                Err("Unknown mode: `Turbo`. Valid: manual, plan, yolo.".to_owned()),
            );
        }
    }

    mod model {
        use super::*;

        #[test]
        fn round_trips_every_catalog_id() {
            for info in MODELS {
                assert_eq!(model(info.id), Ok(set_model(info.id)));
            }
        }

        #[test]
        fn resolves_family_aliases_ignoring_case_and_padding() {
            for (arg, id) in [
                ("fable", "claude-fable-5-1"),
                ("opus", "claude-opus-5-5"),
                ("sonnet", "claude-sonnet-5-5"),
                ("haiku", "claude-haiku-5-5"),
                ("  Claude-Sonnet-5-5  ", "claude-sonnet-5-5"),
            ] {
                assert_eq!(model(arg), Ok(set_model(id)), "{arg}");
            }
        }

        #[test]
        fn ambiguous_substring_lists_only_the_matching_rows() {
            assert_eq!(
                model("o").expect_err("ambiguous arg must error"),
                indoc! {"
                    Model `o` is ambiguous. Pick one or use a short alias (`fable`, `opus`, `sonnet`, `haiku`):

                    - `claude-opus-5-5` — Claude Opus 5.5
                    - `claude-sonnet-5-5` — Claude Sonnet 5.5"
                },
            );
        }

        #[test]
        fn unknown_id_suggests_a_replacement_and_lists_the_catalog() {
            // IDs naming a family and a `[1m]` suffix also get the matching model as a suggestion.
            for (arg, expected) in [
                (
                    "gpt-4",
                    indoc! {"
                        Unknown model: `gpt-4`. Valid:

                        - `claude-fable-5-1` — Claude Fable 5.1
                        - `claude-opus-5-5` — Claude Opus 5.5
                        - `claude-sonnet-5-5` — Claude Sonnet 5.5
                        - `claude-haiku-5-5` — Claude Haiku 5.5"
                    },
                ),
                (
                    "claude-opus-5-5-20261001",
                    indoc! {"
                        Unknown model: `claude-opus-5-5-20261001`. Did you mean `claude-opus-5-5`? Valid:

                        - `claude-fable-5-1` — Claude Fable 5.1
                        - `claude-opus-5-5` — Claude Opus 5.5
                        - `claude-sonnet-5-5` — Claude Sonnet 5.5
                        - `claude-haiku-5-5` — Claude Haiku 5.5"
                    },
                ),
                (
                    "sonnet[1m]",
                    indoc! {"
                        Unknown model: `sonnet[1m]`. Did you mean `claude-sonnet-5-5`? Valid:

                        - `claude-fable-5-1` — Claude Fable 5.1
                        - `claude-opus-5-5` — Claude Opus 5.5
                        - `claude-sonnet-5-5` — Claude Sonnet 5.5
                        - `claude-haiku-5-5` — Claude Haiku 5.5"
                    },
                ),
            ] {
                assert_eq!(model(arg), Err(expected.to_owned()), "{arg}");
            }
        }
    }

    mod rename {
        use super::*;

        #[test]
        fn trims_the_title() {
            assert_eq!(
                rename("  Fix auth bug  "),
                Ok(Op::Rename {
                    title: "Fix auth bug".to_owned(),
                }),
            );
        }

        #[test]
        fn fits_a_long_title_to_the_session_cap() {
            let pasted = format!("{}\nSecond line", "a".repeat(MAX_TITLE_WIDTH + 1));

            assert_eq!(
                rename(&pasted),
                Ok(Op::Rename {
                    title: format!("{}...", "a".repeat(MAX_TITLE_WIDTH - 3)),
                }),
            );
            assert_eq!(
                rename("Fix auth flow\nDetails below"),
                Ok(Op::Rename {
                    title: "Fix auth flow".to_owned(),
                }),
            );
        }
    }

    mod resume {
        use super::*;

        #[test]
        fn targets_the_trimmed_prefix() {
            assert_eq!(
                resume(" 1a2b "),
                Ok(Op::Resume {
                    target: ResumeTarget::Prefix("1a2b".to_owned()),
                }),
            );
        }
    }
}
