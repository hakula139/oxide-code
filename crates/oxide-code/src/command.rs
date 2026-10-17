//! Typed slash-command forms parsed into the [`Op`] each one runs, shared by the TUI's slash
//! commands and the bare REPL. The TUI adds the pickers and modals behind the bare forms.

use indoc::formatdoc;

use crate::config::Effort;
use crate::model::{MODELS, ModelInfo, ResolvedModelId, replacement_for};
use crate::permission::Mode;
use crate::protocol::{Op, ResumeTarget};

/// Canonical names of the commands [`parse`] knows, without their leading `/`.
pub(crate) const NAMES: [&str; 7] = [
    "clear", "compact", "effort", "mode", "model", "rename", "resume",
];

/// Parses the typed form of the command `name`, canonical and without its `/`, into the op it
/// runs. `None` when `name` is not one of [`NAMES`]. `Err` carries the message for a malformed
/// form, including a bare form only a picker can complete.
pub(crate) fn parse(name: &str, args: &str) -> Option<Result<Op, String>> {
    Some(match name {
        "clear" => Ok(Op::Clear),
        "compact" => Ok(compact(args)),
        "effort" => effort(args),
        "mode" => mode(args),
        "model" => model(args),
        "rename" => rename(args),
        "resume" => resume(args),
        _ => return None,
    })
}

/// `/compact [<instructions>]`, where trailing text focuses the summary.
pub(crate) fn compact(args: &str) -> Op {
    let instructions = args.trim();
    Op::Compact {
        instructions: (!instructions.is_empty()).then(|| instructions.to_owned()),
    }
}

/// `/effort <level>`, case-insensitive.
pub(crate) fn effort(arg: &str) -> Result<Op, String> {
    let arg = required(arg, "level", "/effort <level>")?;
    let pick = arg
        .to_ascii_lowercase()
        .parse()
        .map_err(|_| format!("Unknown effort: `{arg}`. Valid: {}.", Effort::VALID_VALUES))?;
    Ok(Op::SetModel {
        model: None,
        effort: Some(pick),
    })
}

/// `/mode <mode>`, case-insensitive.
pub(crate) fn mode(arg: &str) -> Result<Op, String> {
    let arg = required(arg, "mode", "/mode <mode>")?;
    let mode = arg
        .to_ascii_lowercase()
        .parse::<Mode>()
        .map_err(|_| format!("Unknown mode: `{arg}`. Valid: {}.", Mode::VALID_VALUES))?;
    Ok(Op::SetMode(mode))
}

/// `/model <id>`. Matches an exact catalog id, then a unique substring of one, so family names
/// such as `opus` work as aliases.
pub(crate) fn model(arg: &str) -> Result<Op, String> {
    let arg = required(arg, "model id", "/model <id>")?.to_ascii_lowercase();
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
                Unknown model: `{arg}`.{hint} Supported models:

                {models}"
            });
        }
    };
    Ok(Op::SetModel {
        model: Some(ResolvedModelId::new(info.id.to_owned())),
        effort: None,
    })
}

/// `/rename <title>`.
pub(crate) fn rename(title: &str) -> Result<Op, String> {
    let title = required(title, "title", "/rename <title>")?;
    Ok(Op::Rename {
        title: title.to_owned(),
    })
}

/// `/resume <id-prefix>`. The engine resolves the prefix against the saved sessions.
pub(crate) fn resume(prefix: &str) -> Result<Op, String> {
    let prefix = required(prefix, "session id", "/resume <id-prefix>")?;
    Ok(Op::Resume {
        target: ResumeTarget::Prefix(prefix.to_owned()),
    })
}

/// The trimmed `arg`, or an error naming the missing `what` and the form's `usage`.
fn required<'a>(arg: &'a str, what: &str, usage: &str) -> Result<&'a str, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(format!("Missing {what}. Usage: `{usage}`."));
    }
    Ok(arg)
}

/// Markdown bullet list of `id — display name` rows.
fn format_models<'a>(models: impl IntoIterator<Item = &'a ModelInfo>) -> String {
    models
        .into_iter()
        .map(|info| format!("- `{}` — {}", info.id, info.display_name))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    fn set_model(id: &str) -> Op {
        Op::SetModel {
            model: Some(ResolvedModelId::new(id.to_owned())),
            effort: None,
        }
    }

    fn set_effort(level: Effort) -> Op {
        Op::SetModel {
            model: None,
            effort: Some(level),
        }
    }

    // ── parse ──

    #[test]
    fn parse_routes_each_name_to_its_form() {
        let cases = [
            ("clear", "", Op::Clear),
            ("compact", "auth", compact("auth")),
            ("effort", "low", set_effort(Effort::Low)),
            ("mode", "plan", Op::SetMode(Mode::Plan)),
            ("model", "haiku", set_model("claude-haiku-5-5")),
            ("rename", "Fix login", rename("Fix login").unwrap()),
            ("resume", "1a2b", resume("1a2b").unwrap()),
        ];

        assert_eq!(cases.each_ref().map(|(name, ..)| *name), NAMES);
        for (name, args, op) in cases {
            assert_eq!(parse(name, args), Some(Ok(op)), "{name}");
        }
    }

    #[test]
    fn parse_knows_no_other_name() {
        for name in ["help", "new", "theme", "compact-all", ""] {
            assert_eq!(parse(name, "x"), None, "{name}");
        }
    }

    #[test]
    fn parse_bare_forms_that_need_an_argument_name_the_usage() {
        for (name, usage) in [
            ("effort", "Missing level. Usage: `/effort <level>`."),
            ("mode", "Missing mode. Usage: `/mode <mode>`."),
            ("model", "Missing model id. Usage: `/model <id>`."),
            ("rename", "Missing title. Usage: `/rename <title>`."),
            (
                "resume",
                "Missing session id. Usage: `/resume <id-prefix>`.",
            ),
        ] {
            assert_eq!(parse(name, "  "), Some(Err(usage.to_owned())), "{name}");
        }
    }

    // ── compact ──

    #[test]
    fn compact_trims_the_instructions_and_drops_blank_ones() {
        assert_eq!(compact("   \n\t  "), Op::Compact { instructions: None });
        assert_eq!(
            compact("  focus on the build error  "),
            Op::Compact {
                instructions: Some("focus on the build error".to_owned()),
            },
        );
    }

    // ── effort ──

    #[test]
    fn effort_accepts_each_level_in_any_case() {
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
    fn effort_unknown_level_lists_the_valid_ones() {
        for arg in ["turbo", "auto"] {
            assert_eq!(
                effort(arg),
                Err(format!(
                    "Unknown effort: `{arg}`. Valid: low, medium, high, xhigh, max."
                )),
            );
        }
    }

    // ── mode ──

    #[test]
    fn mode_accepts_each_mode_in_any_case() {
        for (arg, expected) in [
            ("manual", Mode::Manual),
            (" Plan ", Mode::Plan),
            ("YOLO", Mode::Yolo),
        ] {
            assert_eq!(mode(arg), Ok(Op::SetMode(expected)), "{arg}");
        }
    }

    #[test]
    fn mode_unknown_mode_lists_the_valid_ones() {
        assert_eq!(
            mode("Turbo"),
            Err("Unknown mode: `Turbo`. Valid: manual, plan, yolo.".to_owned()),
        );
    }

    // ── model ──

    #[test]
    fn model_round_trips_every_catalog_id() {
        for info in MODELS {
            assert_eq!(model(info.id), Ok(set_model(info.id)));
        }
    }

    #[test]
    fn model_resolves_family_aliases_ignoring_case_and_padding() {
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
    fn model_unknown_id_suggests_a_replacement_and_lists_the_catalog() {
        // Ids naming a family and a `[1m]` suffix also get the matching model as a suggestion.
        for (arg, expected) in [
            (
                "gpt-4",
                indoc! {"
                    Unknown model: `gpt-4`. Supported models:

                    - `claude-fable-5-1` — Claude Fable 5.1
                    - `claude-opus-5-5` — Claude Opus 5.5
                    - `claude-sonnet-5-5` — Claude Sonnet 5.5
                    - `claude-haiku-5-5` — Claude Haiku 5.5"
                },
            ),
            (
                "claude-opus-5-5-20261001",
                indoc! {"
                    Unknown model: `claude-opus-5-5-20261001`. Did you mean `claude-opus-5-5`? Supported models:

                    - `claude-fable-5-1` — Claude Fable 5.1
                    - `claude-opus-5-5` — Claude Opus 5.5
                    - `claude-sonnet-5-5` — Claude Sonnet 5.5
                    - `claude-haiku-5-5` — Claude Haiku 5.5"
                },
            ),
            (
                "sonnet[1m]",
                indoc! {"
                    Unknown model: `sonnet[1m]`. Did you mean `claude-sonnet-5-5`? Supported models:

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

    #[test]
    fn model_ambiguous_substring_lists_only_the_matching_rows() {
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
    fn model_ambiguous_suggests_every_catalog_alias() {
        let err = model("o").expect_err("ambiguous arg must error");

        for info in MODELS {
            let alias = format!("`{}`", info.family.id_token());
            assert!(err.contains(&alias), "missing {alias}: {err}");
        }
    }

    // ── rename ──

    #[test]
    fn rename_trims_the_title() {
        assert_eq!(
            rename("  Fix auth bug  "),
            Ok(Op::Rename {
                title: "Fix auth bug".to_owned(),
            }),
        );
    }

    // ── resume ──

    #[test]
    fn resume_targets_the_trimmed_prefix() {
        assert_eq!(
            resume(" 1a2b "),
            Ok(Op::Resume {
                target: ResumeTarget::Prefix("1a2b".to_owned()),
            }),
        );
    }
}
