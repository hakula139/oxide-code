//! Welcome surface painted into the chat region while the chat is empty.

use std::fmt::Write as _;

use fastrand::Rng;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;
use xxhash_rust::xxh64::xxh64;

use ox_core::config::{NOT_SIGNED_IN, StatusLineSegment};
use ox_core::util::text::{center_truncate_to_width, pad_to_width, truncate_to_width};

use crate::glyphs::INLINE_SEPARATOR;
use crate::slash::{LiveSessionInfo, MatchedCommand, filter_built_ins};
use crate::theme::Theme;

// ── Content Pools ──

/// Built-ins a new session cannot run bare: `/delete` and `/unarchive` need an ID, `/archive` has
/// nothing to archive yet, `/exit` ends the session, and `/logout` only undoes a `/login`.
const NOT_STARTERS: &[&str] = &["archive", "delete", "exit", "logout", "unarchive"];

/// Offered only when the configured status line includes the `pull-request` segment.
const PR_TIP: &str = "click the PR number in the status line to open it";

const TIP_POOL: &[&str] = &[
    PR_TIP,
    "drag chat text to select it",
    "ox --continue resumes your last session",
    "ox --list shows recent sessions",
    "ox -p \"<prompt>\" runs a one-shot prompt headless",
    "press / to browse all commands",
    "press Ctrl+C twice to exit",
    "press Ctrl+D in /resume to delete a session",
    "press Ctrl+End to jump chat back to bottom",
    "press Enter to send, Shift+Enter for newline",
    "press Esc to cancel an in-flight response",
    "press Shift+Tab to switch to plan mode and back",
];

const STARTER_PICK: usize = 3;

// ── Layout Constants ──

const WORDMARK: &str = "oxide-code";
const RIBBON_FLANK: &str = "━━━━";
const STARTER_HEADER: &str = "Try one of:";
const STARTER_GAP: &str = "    ";
const TIP_LABEL: &str = "Tip";
const TIP_SEP: &str = " — ";

/// Width thresholds for the layout ladder. Below `NARROW_MIN` the welcome paints nothing.
const FULL_MIN: u16 = 60;
const COLLAPSED_MIN: u16 = 40;
const NARROW_MIN: u16 = 25;

/// Minimum height for the starters and tip block. Shorter areas drop the block, so the env and
/// cwd column centers on its own width.
const STARTERS_MIN_HEIGHT: u16 = 13;

// ── Picks ──

/// The starters and tip a session shows, drawn from a seed of its ID so every frame of a session
/// paints the same picks while `/clear` and a new launch draw again.
struct Picks {
    starters: Vec<MatchedCommand>,
    tip: &'static str,
}

impl Picks {
    fn for_session(session_id: &str, status_line: &[StatusLineSegment]) -> Self {
        let mut rng = Rng::with_seed(xxh64(session_id.as_bytes(), 0));
        let pool = filter_built_ins("")
            .into_iter()
            .filter(|cmd| !NOT_STARTERS.contains(&cmd.name));
        let pr_tip = status_line.contains(&StatusLineSegment::PullRequest);
        let tips: Vec<&'static str> = TIP_POOL
            .iter()
            .copied()
            .filter(|&tip| pr_tip || tip != PR_TIP)
            .collect();
        Self {
            starters: rng.choose_multiple(pool, STARTER_PICK),
            tip: tips[rng.usize(..tips.len())],
        }
    }
}

// ── Paint ──

/// No-op when `area.width < NARROW_MIN` or `area.height == 0`.
pub(crate) fn paint(frame: &mut Frame<'_>, area: Rect, theme: &Theme, info: &LiveSessionInfo) {
    if area.width < NARROW_MIN || area.height == 0 {
        return;
    }
    let picks = Picks::for_session(&info.session_id, &info.config.status_line);
    let lines = build_lines(area.width, area.height, theme, info, &picks);
    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(theme.surface());
    frame.render_widget(paragraph, area);
}

// ── Line Builders ──

fn build_lines(
    width: u16,
    height: u16,
    theme: &Theme,
    info: &LiveSessionInfo,
    picks: &Picks,
) -> Vec<Line<'static>> {
    let max_body = usize::from(width);
    let full = width >= FULL_MIN;
    let with_starters = width >= COLLAPSED_MIN;
    let render_starters_block = with_starters && height >= STARTERS_MIN_HEIGHT;
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::raw(""));
    push_identity(&mut lines, theme, info.version, full);
    lines.push(Line::raw(""));

    // A shared column width keeps the centered rows on one left edge. Starter and tip widths count
    // only when that block renders, so a short area reserves no space for rows it drops.
    let env = truncate_to_width(&environment_text(info, full, with_starters), max_body);
    let cwd = cwd_text(max_body, &info.cwd, with_starters);
    let starter_rows = render_starters_block.then(|| starter_rows(&picks.starters));
    let tip_text = render_starters_block.then(|| format!("{TIP_LABEL}{TIP_SEP}{}", picks.tip));
    // Clamp to area.width, since wider columns clip on the right under center alignment.
    let column_width =
        column_width(&env, &cwd, starter_rows.as_deref(), tip_text.as_deref()).min(max_body);

    push_padded(&mut lines, &env, theme.text(), column_width);
    push_padded(&mut lines, &cwd, theme.dim(), column_width);

    if let Some(rows) = starter_rows {
        lines.push(Line::raw(""));
        push_padded(&mut lines, STARTER_HEADER, theme.dim(), column_width);
        lines.push(Line::raw(""));
        for (name, desc) in &rows {
            push_starter_row(&mut lines, name, desc, theme, column_width);
        }
        lines.push(Line::raw(""));
        push_tip(&mut lines, theme, picks.tip, column_width);
    }
    lines
}

fn push_identity(lines: &mut Vec<Line<'static>>, theme: &Theme, version: &str, full: bool) {
    let dim = theme.dim();
    let accent_bold = theme.accent().add_modifier(Modifier::BOLD);
    let version = format!("v{version}");
    if full {
        lines.push(Line::from(vec![
            Span::styled(RIBBON_FLANK, dim),
            Span::raw(" "),
            Span::styled(WORDMARK, accent_bold),
            Span::raw(" "),
            Span::styled(version, dim),
            Span::raw(" "),
            Span::styled(RIBBON_FLANK, dim),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(WORDMARK, accent_bold),
            Span::raw(" "),
            Span::styled(version, dim),
        ]));
    }
}

fn environment_text(info: &LiveSessionInfo, full: bool, with_starters: bool) -> String {
    let config = &info.config;
    let mut text = config.model.display_name().to_owned();
    if let Some(effort) = config.effort {
        // Drop the " effort" suffix below COLLAPSED_MIN so the line fits at NARROW_MIN.
        let suffix = if with_starters { " effort" } else { "" };
        _ = write!(text, "{INLINE_SEPARATOR}{effort}{suffix}");
    }
    if full {
        text.push_str(INLINE_SEPARATOR);
        text.push_str(
            config
                .auth
                .map_or(NOT_SIGNED_IN, |source| source.kind().label()),
        );
    }
    text
}

fn cwd_text(max_body: usize, cwd: &str, with_starters: bool) -> String {
    if with_starters {
        truncate_to_width(cwd, max_body)
    } else {
        center_truncate_to_width(cwd, max_body)
    }
}

fn starter_rows(picked: &[MatchedCommand]) -> Vec<(String, String)> {
    let name_col_width = picked
        .iter()
        .map(|cmd| cmd.name.width() + 1)
        .max()
        .unwrap_or(0);
    picked
        .iter()
        .map(|cmd| {
            let name = pad_to_width(&format!("/{}", cmd.name), name_col_width);
            (format!("  {name}"), cmd.description.to_owned())
        })
        .collect()
}

fn starter_row_width(rows: &[(String, String)]) -> usize {
    rows.iter()
        .map(|(name, desc)| name.width() + STARTER_GAP.width() + desc.width())
        .max()
        .unwrap_or(0)
}

fn column_width(
    env: &str,
    cwd: &str,
    starter_rows: Option<&[(String, String)]>,
    tip_text: Option<&str>,
) -> usize {
    let mut max_width = env.width().max(cwd.width());
    if let Some(rows) = starter_rows {
        max_width = max_width
            .max(STARTER_HEADER.width())
            .max(starter_row_width(rows));
    }
    if let Some(tip) = tip_text {
        max_width = max_width.max(tip.width());
    }
    max_width
}

fn push_padded(lines: &mut Vec<Line<'static>>, body: &str, style: Style, column_width: usize) {
    let pad = column_width.saturating_sub(body.width());
    lines.push(Line::from(vec![
        Span::styled(body.to_owned(), style),
        Span::raw(" ".repeat(pad)),
    ]));
}

fn push_starter_row(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    desc: &str,
    theme: &Theme,
    column_width: usize,
) {
    let prefix_width = name.width() + STARTER_GAP.width();
    let desc_budget = column_width.saturating_sub(prefix_width);
    let desc = truncate_to_width(desc, desc_budget);
    let pad = column_width.saturating_sub(prefix_width + desc.width());
    lines.push(Line::from(vec![
        Span::styled(name.to_owned(), theme.accent()),
        Span::raw(STARTER_GAP),
        Span::styled(desc, theme.dim()),
        Span::raw(" ".repeat(pad)),
    ]));
}

fn push_tip(lines: &mut Vec<Line<'static>>, theme: &Theme, tip: &'static str, column_width: usize) {
    let prefix_width = TIP_LABEL.width() + TIP_SEP.width();
    let body_budget = column_width.saturating_sub(prefix_width);
    let body = truncate_to_width(tip, body_budget);
    let pad = column_width.saturating_sub(prefix_width + body.width());
    lines.push(Line::from(vec![
        Span::styled(TIP_LABEL, theme.accent()),
        Span::styled(TIP_SEP, theme.dim()),
        Span::styled(body, theme.text()),
        Span::raw(" ".repeat(pad)),
    ]));
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use ox_core::config::Effort;
    use ox_core::config::auth::AuthSource;

    use super::*;
    use crate::slash::testing::test_session_info;

    fn fixture() -> LiveSessionInfo {
        let mut info = test_session_info();
        info.cwd = "~/github/oxide-code".to_owned();
        info.version = "0.1.0";
        info.config.auth = Some(AuthSource::ClaudeCodeLogin);
        info.config.effort = Some(Effort::Xhigh);
        info
    }

    mod picks {
        use super::*;

        const SESSION_IDS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

        fn starter_names(picks: &Picks) -> Vec<&'static str> {
            picks.starters.iter().map(|cmd| cmd.name).collect()
        }

        #[test]
        fn draws_the_same_picks_on_every_frame_of_a_session() {
            let first = Picks::for_session("session", &[]);

            let again = Picks::for_session("session", &[]);

            assert_eq!(starter_names(&again), starter_names(&first));
            assert_eq!(again.tip, first.tip);
        }

        #[test]
        fn draws_distinct_starters_a_new_session_can_run() {
            for id in SESSION_IDS {
                let picks = Picks::for_session(id, &[]);

                let mut names = starter_names(&picks);

                names.sort_unstable();
                names.dedup();
                assert_eq!(names.len(), STARTER_PICK, "{id}: {names:?}");
                assert!(
                    names.iter().all(|name| !NOT_STARTERS.contains(name)),
                    "{id}: {names:?}"
                );
            }
        }

        #[test]
        fn draws_again_for_another_session() {
            let draws: Vec<(Vec<&str>, &str)> = SESSION_IDS
                .iter()
                .map(|id| Picks::for_session(id, &[]))
                .map(|picks| (starter_names(&picks), picks.tip))
                .collect();

            assert!(draws.iter().any(|draw| *draw != draws[0]), "{draws:?}");
        }

        #[test]
        fn offers_the_pr_tip_only_with_the_pull_request_segment() {
            let tips = |status_line: &[StatusLineSegment]| -> Vec<&str> {
                (0..64)
                    .map(|n| Picks::for_session(&n.to_string(), status_line).tip)
                    .collect()
            };

            let with_segment = tips(&[StatusLineSegment::PullRequest]);
            let without_segment = tips(&[]);

            assert!(with_segment.contains(&PR_TIP), "{with_segment:?}");
            assert!(!without_segment.contains(&PR_TIP), "{without_segment:?}");
        }
    }

    mod not_starters {
        use super::*;

        #[test]
        fn names_only_built_ins() {
            let built_ins: Vec<&str> = filter_built_ins("").iter().map(|cmd| cmd.name).collect();

            assert!(
                NOT_STARTERS.iter().all(|name| built_ins.contains(name)),
                "{built_ins:?}"
            );
        }
    }

    mod environment_text {
        use super::*;

        #[test]
        fn labels_a_signed_out_session() {
            let mut info = fixture();
            info.config.auth = None;

            let text = environment_text(&info, true, true);

            assert_eq!(text, "Claude Opus 5.5 · xhigh effort · Not signed in");
        }

        #[test]
        fn leaves_out_the_effort_of_a_model_without_levels() {
            let mut info = fixture();
            info.config.effort = None;

            let text = environment_text(&info, true, true);

            assert_eq!(text, "Claude Opus 5.5 · OAuth");
        }
    }

    mod paint {
        use super::*;

        fn render(width: u16, height: u16, info: &LiveSessionInfo) -> TestBackend {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let theme = Theme::default();
            terminal
                .draw(|frame| {
                    paint(frame, Rect::new(0, 0, width, height), &theme, info);
                })
                .unwrap();
            terminal.backend().clone()
        }

        #[test]
        fn below_narrow_min_is_a_noop() {
            let info = fixture();
            let backend = render(NARROW_MIN - 1, 12, &info);
            for cell in &backend.buffer().content {
                assert_eq!(cell.symbol(), " ", "every cell must remain blank");
            }
        }

        #[test]
        fn full_width_flanks_the_wordmark_and_indents_the_body() {
            let info = fixture();
            insta::assert_snapshot!(render(80, 14, &info));
        }

        #[test]
        fn at_60_col_minimum_full_layout_still_includes_starters() {
            let info = fixture();
            insta::assert_snapshot!(render(60, 14, &info));
        }

        #[test]
        fn collapsed_drops_the_ribbon_and_keeps_starters() {
            let info = fixture();
            insta::assert_snapshot!(render(50, 14, &info));
        }

        #[test]
        fn narrow_drops_starters_and_truncates_cwd_in_the_middle() {
            let mut info = fixture();
            info.cwd = "~/very/long/working/directory/path/oxide-code".to_owned();
            insta::assert_snapshot!(render(30, 8, &info));
        }

        #[test]
        fn at_narrow_min_does_not_clip_environment_text() {
            let info = fixture();
            let backend = render(NARROW_MIN, 8, &info);
            let row: String = (0..NARROW_MIN)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, 3))
                        .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            assert!(row.contains("xhigh"), "narrow env must not clip: {row:?}");
        }

        #[test]
        fn below_starters_min_height_centers_env_on_its_own_width() {
            let info = fixture();
            let backend = render(80, STARTERS_MIN_HEIGHT - 1, &info);
            let env_row: String = (0..80)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, 5))
                        .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            let leading = env_row.chars().take_while(|c| *c == ' ').count();
            let trailing = env_row.chars().rev().take_while(|c| *c == ' ').count();
            assert!(
                leading.abs_diff(trailing) <= 1,
                "env row must be centered, got leading={leading} trailing={trailing}: {env_row:?}",
            );
        }

        #[test]
        fn just_below_collapsed_min_drops_starters() {
            let info = fixture();
            let backend = render(COLLAPSED_MIN - 1, 12, &info);
            let body: String = backend
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(
                !body.contains(STARTER_HEADER),
                "starter header must disappear"
            );
            assert!(!body.contains(TIP_LABEL), "tip row must disappear");
        }

        #[test]
        fn just_below_full_min_drops_ribbon() {
            let info = fixture();
            let backend = render(FULL_MIN - 1, 14, &info);
            let body: String = backend
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(body.contains(WORDMARK), "wordmark must still render");
            assert!(!body.contains(RIBBON_FLANK), "ribbon flanks must disappear");
        }

        #[test]
        fn paints_nothing_into_a_zero_height_area() {
            let info = fixture();
            let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();

            terminal
                .draw(|frame| paint(frame, Rect::new(0, 2, 80, 0), &Theme::default(), &info))
                .unwrap();

            let buffer = terminal.backend().buffer();
            assert!(buffer.content.iter().all(|cell| cell.symbol() == " "));
        }
    }
}
