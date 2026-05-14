//! Welcome surface painted into the chat region while the chat is empty.

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

// ── Content pools ──

type Starter = (&'static str, &'static str);

const STARTER_POOL: &[Starter] = &[
    ("/clear", "reset conversation"),
    ("/diff", "show git changes"),
    ("/effort", "tune Speed ↔ Intelligence"),
    ("/help", "list commands"),
    ("/init", "author or update AGENTS.md"),
    ("/model", "switch model"),
    ("/resume", "switch to another session"),
    ("/status", "session at a glance"),
    ("/theme", "switch theme"),
];

const TIP_POOL: &[&str] = &[
    "ox --continue resumes your last session",
    "ox --list shows recent sessions",
    "press / to browse all commands",
    "press Ctrl+C twice to exit",
    "press Ctrl+D in /resume to delete a session",
    "press Enter to send, Shift+Enter for newline",
];

const STARTER_PICK: usize = 3;

// ── Layout constants ──

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

/// Minimum height for the starters / tip block. Below this we drop them so the env / cwd column
/// centers on its own width rather than reserving space for clipped content.
const STARTERS_MIN_HEIGHT: u16 = 13;

// ── Snapshot ──

/// Projection of [`LiveSessionInfo`] consumed by [`paint`]; randomized fields are stable per
/// construction.
pub(crate) struct WelcomeSnapshot {
    pub(crate) version: &'static str,
    pub(crate) model_label: String,
    pub(crate) effort_label: String,
    pub(crate) auth_label: &'static str,
    pub(crate) cwd: String,
    pub(crate) starters: [Starter; STARTER_PICK],
    pub(crate) tip: &'static str,
}

impl WelcomeSnapshot {
    pub(crate) fn from_live(info: &LiveSessionInfo) -> Self {
        // Derive seed from session id: the same welcome paints across re-renders within a session
        // but `/clear` (which rolls the session) and fresh launches get a new pick.
        Self::from_live_with_seed(info, hash_seed(&info.session_id))
    }

    fn from_live_with_seed(info: &LiveSessionInfo, seed: u64) -> Self {
        Self {
            version: info.version,
            model_label: info.display_name().into_owned(),
            effort_label: display_effort(info.config.effort),
            auth_label: info.config.auth_label,
            cwd: info.cwd.clone(),
            starters: pick_starters(seed),
            tip: pick_tip(seed),
        }
    }
}

// ── Paint ──

/// No-op when `area.width < NARROW_MIN` or `area.height == 0`.
pub(crate) fn paint(frame: &mut Frame<'_>, area: Rect, theme: &Theme, snap: &WelcomeSnapshot) {
    if area.width < NARROW_MIN || area.height == 0 {
        return;
    }
    let lines = build_lines(area.width, area.height, theme, snap);
    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(theme.surface());
    frame.render_widget(paragraph, area);
}

// ── Line builders ──

fn build_lines(
    width: u16,
    height: u16,
    theme: &Theme,
    snap: &WelcomeSnapshot,
) -> Vec<Line<'static>> {
    let max_body = usize::from(width);
    let full = width >= FULL_MIN;
    let with_starters = width >= COLLAPSED_MIN;
    let render_starters_block = with_starters && height >= STARTERS_MIN_HEIGHT;
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::raw(""));
    push_identity(&mut lines, theme, snap, full);
    lines.push(Line::raw(""));

    // Shared column width keeps centered rows on one left edge instead of each row floating.
    // Starter / tip widths only contribute when the block actually renders, so a height-clipped
    // area doesn't reserve space for content `Paragraph` will drop.
    let env = truncate_to_width(&environment_text(snap, full, with_starters), max_body);
    let cwd = cwd_text(max_body, snap, with_starters);
    let starter_rows = render_starters_block.then(|| starter_rows(&snap.starters));
    let tip_text = render_starters_block.then(|| format!("{TIP_LABEL}{TIP_SEP}{}", snap.tip));
    // Clamp to area.width — wider columns clip on the right under center alignment.
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
        push_tip(&mut lines, theme, snap.tip, column_width);
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
    // Drop the " effort" suffix below COLLAPSED_MIN so the line fits at NARROW_MIN.
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

fn starter_rows(picked: &[Starter; STARTER_PICK]) -> Vec<(String, String)> {
    let name_col_width = picked
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or(0);
    picked
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

// ── Random picks ──

// Knuth's MMIX LCG constants — not cryptographic, just enough spread per session.
const LCG_MULT: u64 = 6_364_136_223_846_793_005;
const LCG_INC: u64 = 1_442_695_040_888_963_407;

fn lcg_step(state: u64) -> u64 {
    state.wrapping_mul(LCG_MULT).wrapping_add(LCG_INC)
}

fn pick_starters(seed: u64) -> [Starter; STARTER_PICK] {
    let n = STARTER_POOL.len();
    debug_assert!(n >= STARTER_PICK);
    // Partial Fisher-Yates: STARTER_PICK swaps from a length-n deck draws k distinct indices.
    let mut state = seed | 1;
    let mut deck: Vec<usize> = (0..n).collect();
    for i in 0..STARTER_PICK {
        state = lcg_step(state);
        let span = u64::try_from(n - i).unwrap_or(u64::MAX);
        let j = i + usize::try_from((state >> 33) % span).unwrap_or(0);
        deck.swap(i, j);
    }
    [
        STARTER_POOL[deck[0]],
        STARTER_POOL[deck[1]],
        STARTER_POOL[deck[2]],
    ]
}

fn pick_tip(seed: u64) -> &'static str {
    let state = lcg_step(seed | 1);
    let n = u64::try_from(TIP_POOL.len()).unwrap_or(1);
    let idx = usize::try_from((state >> 33) % n).unwrap_or(0);
    TIP_POOL[idx]
}

fn hash_seed(s: &str) -> u64 {
    // FNV-1a 64-bit — cheap and dependency-free.
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::{
        AutoCompactionConfig, CompactionConfig, ConfigSnapshot, Effort, PromptCacheTtl,
        test_thresholds,
    };
    use crate::slash::LiveSessionInfo;

    const TEST_SEED: u64 = 0x00C0_FFEE;

    fn fixture() -> LiveSessionInfo {
        LiveSessionInfo {
            cwd: "~/github/oxide-code".to_owned(),
            git_branch: Some("main".to_owned()),
            version: "0.1.0",
            session_id: "test-session".to_owned(),
            config: ConfigSnapshot {
                auth_label: "OAuth",
                base_url: "https://api.test.invalid".to_owned(),
                extra_ca_certs: None,
                model_id: "claude-opus-4-7".to_owned(),
                effort: Some(Effort::Xhigh),
                max_tokens: 64_000,
                max_tool_rounds: None,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(test_thresholds::WINDOW_1M),
                }),
                show_thinking: false,
                show_welcome: true,
                status_line: crate::config::StatusLineSegment::DEFAULT.to_vec(),
                theme_name: "mocha".to_owned(),
            },
        }
    }

    fn snap_for(info: &LiveSessionInfo) -> WelcomeSnapshot {
        WelcomeSnapshot::from_live_with_seed(info, TEST_SEED)
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
        let snap = snap_for(&info);
        assert_eq!(snap.version, "0.1.0");
        assert_eq!(snap.model_label, "Claude Opus 4.7");
        assert_eq!(snap.effort_label, "xhigh");
        assert_eq!(snap.auth_label, "OAuth");
        assert_eq!(snap.cwd, "~/github/oxide-code");
    }

    #[test]
    fn from_live_picks_distinct_starters_from_the_pool() {
        let info = fixture();
        let snap = snap_for(&info);
        let names: Vec<&str> = snap.starters.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), STARTER_PICK);
        // No duplicates: shuffle gives distinct picks.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            STARTER_PICK,
            "picked starters must be distinct"
        );
        // Each picked entry is from STARTER_POOL.
        for picked in &snap.starters {
            assert!(STARTER_POOL.contains(picked));
        }
    }

    #[test]
    fn from_live_picks_tip_from_the_pool() {
        let info = fixture();
        let snap = snap_for(&info);
        assert!(TIP_POOL.contains(&snap.tip));
    }

    #[test]
    fn from_live_with_seed_is_deterministic() {
        let info = fixture();
        let a = WelcomeSnapshot::from_live_with_seed(&info, 7);
        let b = WelcomeSnapshot::from_live_with_seed(&info, 7);
        assert_eq!(a.starters, b.starters);
        assert_eq!(a.tip, b.tip);
    }

    // ── paint ──

    #[test]
    fn paint_below_narrow_min_is_a_noop() {
        let snap = snap_for(&fixture());
        let backend = render(NARROW_MIN - 1, 12, &snap);
        for cell in &backend.buffer().content {
            assert_eq!(cell.symbol(), " ", "every cell must remain blank");
        }
    }

    #[test]
    fn paint_full_width_renders_box_environment_starters_and_trailer() {
        let snap = snap_for(&fixture());
        insta::assert_snapshot!(render(80, 14, &snap));
    }

    #[test]
    fn paint_60_col_minimum_full_layout_still_includes_starters() {
        let snap = snap_for(&fixture());
        insta::assert_snapshot!(render(60, 14, &snap));
    }

    #[test]
    fn paint_collapsed_drops_box_but_keeps_starters() {
        let snap = snap_for(&fixture());
        insta::assert_snapshot!(render(50, 14, &snap));
    }

    #[test]
    fn paint_narrow_drops_starters_and_truncates_cwd_in_the_middle() {
        let mut info = fixture();
        info.cwd = "~/very/long/working/directory/path/oxide-code".to_owned();
        let snap = snap_for(&info);
        insta::assert_snapshot!(render(30, 8, &snap));
    }

    #[test]
    fn paint_at_narrow_min_does_not_clip_environment_text() {
        let snap = snap_for(&fixture());
        let backend = render(NARROW_MIN, 8, &snap);
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
    fn paint_below_starters_min_height_centers_env_on_its_own_width() {
        let snap = snap_for(&fixture());
        let backend = render(80, STARTERS_MIN_HEIGHT - 1, &snap);
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
    fn paint_just_below_collapsed_min_drops_starters() {
        let snap = snap_for(&fixture());
        let backend = render(COLLAPSED_MIN - 1, 12, &snap);
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
    fn paint_just_below_full_min_drops_ribbon() {
        let snap = snap_for(&fixture());
        let backend = render(FULL_MIN - 1, 14, &snap);
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
    fn paint_zero_height_is_a_noop() {
        let snap = snap_for(&fixture());
        let backend = render(80, 0, &snap);
        assert!(
            backend.buffer().content.is_empty(),
            "zero-height area paints nothing",
        );
    }
}
