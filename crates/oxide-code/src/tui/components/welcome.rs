//! First-paint surface for an empty chat. Stateless [`paint`] over a [`WelcomeSnapshot`] derived
//! from `&LiveSessionInfo`; width-ladder between full / collapsed / suppressed.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::config::display_effort;
use crate::slash::LiveSessionInfo;
use crate::tui::theme::Theme;
use crate::util::text::{center_truncate_to_width, truncate_to_width};

const STARTERS: &[(&str, &str)] = &[
    ("/help", "list commands"),
    ("/init", "author or update AGENTS.md"),
    ("/diff", "show staged changes"),
];

const WORDMARK: &str = "oxide-code";
const RIBBON_FLANK: &str = "━━━━";
const STARTER_HEADER: &str = "Try one of:";
const STARTER_GAP: &str = "    ";
const TRAILER: &str = "tip — press / to browse all commands.";

const FULL_MIN: u16 = 60;
const COLLAPSED_MIN: u16 = 40;
const NARROW_MIN: u16 = 25;

/// Projection of [`LiveSessionInfo`] consumed by [`paint`].
pub(crate) struct WelcomeSnapshot {
    pub(crate) version: &'static str,
    pub(crate) model_label: String,
    pub(crate) effort_label: String,
    pub(crate) auth_label: &'static str,
    pub(crate) cwd: String,
}

impl WelcomeSnapshot {
    pub(crate) fn from_live(info: &LiveSessionInfo) -> Self {
        Self {
            version: info.version,
            model_label: info.display_name().into_owned(),
            effort_label: display_effort(info.config.effort),
            auth_label: info.config.auth_label,
            cwd: info.cwd.clone(),
        }
    }
}

/// No-op when `area.width < NARROW_MIN` or `area.height == 0` — too narrow to read cleanly.
pub(crate) fn paint(frame: &mut Frame<'_>, area: Rect, theme: &Theme, snap: &WelcomeSnapshot) {
    if area.width < NARROW_MIN || area.height == 0 {
        return;
    }
    let lines = build_lines(area.width, theme, snap);
    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(theme.surface());
    frame.render_widget(paragraph, area);
}

fn build_lines(width: u16, theme: &Theme, snap: &WelcomeSnapshot) -> Vec<Line<'static>> {
    let max_body = usize::from(width);
    let full = width >= FULL_MIN;
    let with_starters = width >= COLLAPSED_MIN;
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::raw(""));
    push_identity(&mut lines, theme, snap, full);
    lines.push(Line::raw(""));

    // Pad body rows to a shared column width so they share one left edge under
    // `Paragraph::alignment(Center)`. Without the pad, each row centers on its own width and
    // floats independently ("ransom note" stack).
    let env = truncate_to_width(&environment_text(snap, full, with_starters), max_body);
    let cwd = cwd_text(max_body, snap, with_starters);
    let starter_rows = with_starters.then(starter_rows);
    // Clamp column_width to area.width — a wider column would overflow centering and clip on the
    // right edge of the pane.
    let column_width = column_width(&env, &cwd, starter_rows.as_deref()).min(max_body);

    push_padded(&mut lines, &env, theme.text(), column_width);
    push_padded(&mut lines, &cwd, theme.dim(), column_width);

    if let Some(rows) = starter_rows {
        lines.push(Line::raw(""));
        push_padded(&mut lines, STARTER_HEADER, theme.dim(), column_width);
        for (name, desc) in &rows {
            push_starter_row(&mut lines, name, desc, theme, column_width);
        }
        lines.push(Line::raw(""));
        push_padded(&mut lines, TRAILER, theme.dim(), column_width);
    }
    lines
}

fn push_identity(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    snap: &WelcomeSnapshot,
    full: bool,
) {
    let dim = theme.dim();
    let accent_bold = theme.accent().add_modifier(Modifier::BOLD);
    let version = format!("v{}", snap.version);
    if full {
        // Editorial ribbon: heavy rules flanking the wordmark form a single-line identity that
        // anchors the welcome without the chrome of a four-side box.
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

fn environment_text(snap: &WelcomeSnapshot, full: bool, with_starters: bool) -> String {
    // Below COLLAPSED_MIN (i.e., narrow tier) drops the " effort" suffix so the line fits at
    // 25 cols; the suffix is verbose redundancy once the value is visible.
    let suffix = if with_starters { " effort" } else { "" };
    let mut text = format!("{} · {}{}", snap.model_label, snap.effort_label, suffix);
    if full {
        text.push_str(" · ");
        text.push_str(snap.auth_label);
    }
    text
}

fn cwd_text(max_body: usize, snap: &WelcomeSnapshot, with_starters: bool) -> String {
    if with_starters {
        truncate_to_width(&snap.cwd, max_body)
    } else {
        center_truncate_to_width(&snap.cwd, max_body)
    }
}

fn starter_rows() -> Vec<(String, String)> {
    let name_col_width = STARTERS
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or(0);
    STARTERS
        .iter()
        .map(|(name, desc)| {
            let pad = " ".repeat(name_col_width.saturating_sub(name.width()));
            (format!("  {name}{pad}"), (*desc).to_owned())
        })
        .collect()
}

fn starter_row_width(rows: &[(String, String)]) -> usize {
    rows.iter()
        .map(|(name, desc)| name.width() + STARTER_GAP.len() + desc.width())
        .max()
        .unwrap_or(0)
}

fn column_width(env: &str, cwd: &str, starter_rows: Option<&[(String, String)]>) -> usize {
    let mut max_width = env.width().max(cwd.width());
    if let Some(rows) = starter_rows {
        max_width = max_width
            .max(STARTER_HEADER.width())
            .max(starter_row_width(rows))
            .max(TRAILER.width());
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
    let prefix_width = name.width() + STARTER_GAP.len();
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};
    use crate::slash::LiveSessionInfo;

    fn fixture() -> LiveSessionInfo {
        LiveSessionInfo {
            cwd: "~/github/oxide-code".to_owned(),
            version: "0.1.0",
            session_id: "test-session".to_owned(),
            config: ConfigSnapshot {
                auth_label: "OAuth",
                base_url: "https://api.test.invalid".to_owned(),
                model_id: "claude-opus-4-7".to_owned(),
                effort: Some(Effort::Xhigh),
                max_tokens: 64_000,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                show_thinking: false,
                show_welcome: true,
                theme_name: "mocha".to_owned(),
            },
        }
    }

    fn render(width: u16, height: u16, snap: &WelcomeSnapshot) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                paint(frame, Rect::new(0, 0, width, height), &theme, snap);
            })
            .unwrap();
        terminal.backend().clone()
    }

    // ── WelcomeSnapshot::from_live ──

    #[test]
    fn from_live_projects_display_name_and_effort() {
        let info = fixture();
        let snap = WelcomeSnapshot::from_live(&info);
        assert_eq!(snap.version, "0.1.0");
        assert_eq!(snap.model_label, "Claude Opus 4.7");
        assert_eq!(snap.effort_label, "xhigh");
        assert_eq!(snap.auth_label, "OAuth");
        assert_eq!(snap.cwd, "~/github/oxide-code");
    }

    // ── paint / width ladder ──

    #[test]
    fn paint_below_narrow_min_is_a_no_op() {
        // Anything narrower than NARROW_MIN should leave the buffer untouched.
        let snap = WelcomeSnapshot::from_live(&fixture());
        let backend = render(NARROW_MIN - 1, 12, &snap);
        let buf = backend.buffer();
        for cell in &buf.content {
            assert_eq!(cell.symbol(), " ", "every cell must remain blank");
        }
    }

    #[test]
    fn paint_full_width_renders_box_environment_starters_and_trailer() {
        let snap = WelcomeSnapshot::from_live(&fixture());
        insta::assert_snapshot!(render(80, 14, &snap));
    }

    #[test]
    fn paint_60_col_minimum_full_layout_still_includes_starters() {
        let snap = WelcomeSnapshot::from_live(&fixture());
        insta::assert_snapshot!(render(60, 14, &snap));
    }

    #[test]
    fn paint_collapsed_drops_box_but_keeps_starters() {
        let snap = WelcomeSnapshot::from_live(&fixture());
        insta::assert_snapshot!(render(50, 12, &snap));
    }

    #[test]
    fn paint_narrow_drops_starters_and_truncates_cwd_in_the_middle() {
        let mut info = fixture();
        info.cwd = "~/very/long/working/directory/path/oxide-code".to_owned();
        let snap = WelcomeSnapshot::from_live(&info);
        insta::assert_snapshot!(render(30, 8, &snap));
    }

    #[test]
    fn paint_at_narrow_min_does_not_clip_environment_text() {
        // env at the narrow tier is "Claude Opus 4.7 · xhigh" (23 cols), fitting NARROW_MIN.
        let snap = WelcomeSnapshot::from_live(&fixture());
        let backend = render(NARROW_MIN, 8, &snap);
        let row: String = (0..NARROW_MIN)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 3))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect();
        assert!(
            row.contains("xhigh"),
            "narrow env must end without clipping: {row:?}"
        );
    }

    #[test]
    fn paint_just_below_collapsed_min_drops_starters() {
        // 39 cols (COLLAPSED_MIN - 1) must hide the starter list and trailer.
        let snap = WelcomeSnapshot::from_live(&fixture());
        let backend = render(COLLAPSED_MIN - 1, 12, &snap);
        let body: String = backend
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            !body.contains(STARTER_HEADER),
            "starter header must disappear at narrow tier"
        );
        assert!(
            !body.contains("press / to browse"),
            "trailer must disappear at narrow tier"
        );
    }

    #[test]
    fn paint_just_below_full_min_drops_ribbon() {
        // 59 cols (FULL_MIN - 1) must show the wordmark only — no ribbon flanks.
        let snap = WelcomeSnapshot::from_live(&fixture());
        let backend = render(FULL_MIN - 1, 14, &snap);
        let body: String = backend
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(body.contains(WORDMARK), "wordmark must still render");
        assert!(
            !body.contains(RIBBON_FLANK),
            "ribbon flanks must disappear below FULL_MIN"
        );
    }

    #[test]
    fn paint_zero_height_is_a_no_op() {
        let snap = WelcomeSnapshot::from_live(&fixture());
        let backend = render(80, 0, &snap);
        assert!(
            backend.buffer().content.is_empty(),
            "zero-height area paints nothing"
        );
    }
}
