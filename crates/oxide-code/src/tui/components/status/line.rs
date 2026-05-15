use ratatui::text::{Line, Span};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

use crate::agent::event::UsageSnapshot;
use crate::config::{Effort, StatusLineSegment};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;
use crate::util::time::local_offset;

const MAX_CURRENT_DIR_WIDTH: usize = 40;
const MAX_GIT_BRANCH_WIDTH: usize = 32;
const MAX_TITLE_WIDTH: usize = 40;

/// Ordered segment roster for one status-line render.
#[derive(Debug, Clone)]
pub(super) struct StatusLine {
    segments: Vec<StatusLineSegment>,
}

impl StatusLine {
    pub(super) fn new(segments: Vec<StatusLineSegment>) -> Self {
        Self { segments }
    }

    pub(super) fn render(
        &self,
        theme: &Theme,
        state: &StatusLineState<'_>,
        width: u16,
    ) -> Line<'static> {
        let sep = theme.separator_span();
        let sep_width = UnicodeWidthStr::width(sep.content.as_ref());
        let mut rendered = self
            .segments
            .iter()
            .filter_map(|segment| Self::render_segment(*segment, theme, state))
            .collect::<Vec<_>>();
        fit_segments(&mut rendered, usize::from(width), sep_width);

        let mut spans = vec![Span::raw("  ")];
        let mut first = true;
        for segment in rendered {
            if !first {
                spans.push(sep.clone());
            }
            if let Some(url) = segment.hyperlink {
                spans.push(osc8_open(&url));
                spans.push(segment.span);
                spans.push(osc8_close());
            } else {
                spans.push(segment.span);
            }
            first = false;
        }
        Line::from(spans)
    }

    fn render_segment(
        segment: StatusLineSegment,
        theme: &Theme,
        state: &StatusLineState<'_>,
    ) -> Option<RenderedSegment> {
        let mut hyperlink: Option<String> = None;
        let span = match segment {
            StatusLineSegment::CurrentDir => non_empty_span(
                truncate_to_width(state.cwd, MAX_CURRENT_DIR_WIDTH),
                Self::segment_style(theme, SegmentStyle::Muted),
            ),
            StatusLineSegment::GitBranch => state.git_branch.map(|branch| {
                Span::styled(
                    truncate_to_width(branch, MAX_GIT_BRANCH_WIDTH),
                    Self::segment_style(theme, SegmentStyle::Accent),
                )
            }),
            StatusLineSegment::PullRequest => state.pull_request.map(|pr| {
                hyperlink = Some(pr.url.clone());
                Span::styled(
                    format!("#{}", pr.number),
                    Self::segment_style(theme, SegmentStyle::Accent),
                )
            }),
            StatusLineSegment::Model => Some(Span::styled(
                state.model.to_owned(),
                Self::segment_style(theme, SegmentStyle::Text),
            )),
            StatusLineSegment::ModelWithEffort => Some(Span::styled(
                model_with_effort(state.model, state.effort),
                Self::segment_style(theme, SegmentStyle::Text),
            )),
            StatusLineSegment::ContextUsed => state
                .usage
                .map(context_label)
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::SessionCost => state
                .usage
                .and_then(session_cost_label)
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::RunState => Some(state.status_span.clone()),
            StatusLineSegment::ThreadTitle => state.title.map(|title| {
                Span::styled(
                    truncate_to_width(title, MAX_TITLE_WIDTH),
                    Self::segment_style(theme, SegmentStyle::Muted),
                )
            }),
            StatusLineSegment::CurrentTime => Some(Span::styled(
                current_time_label(),
                Self::segment_style(theme, SegmentStyle::Dim),
            )),
        }?;
        Some(RenderedSegment::new(segment, span, hyperlink))
    }

    fn segment_style(theme: &Theme, style: SegmentStyle) -> ratatui::style::Style {
        match style {
            SegmentStyle::Text => theme.text(),
            SegmentStyle::Muted => theme.muted(),
            SegmentStyle::Dim => theme.dim(),
            SegmentStyle::Accent => theme.accent(),
        }
    }
}

pub(super) struct StatusLineState<'a> {
    pub(super) model: &'a str,
    pub(super) effort: Option<Effort>,
    pub(super) title: Option<&'a str>,
    pub(super) usage: Option<UsageSnapshot>,
    /// Already tilde-expanded, so the renderer must not substitute `~` again.
    pub(super) cwd: &'a str,
    pub(super) git_branch: Option<&'a str>,
    pub(super) pull_request: Option<&'a crate::util::git::PullRequest>,
    /// Pre-rendered run-state segment from the parent component.
    pub(super) status_span: Span<'static>,
}

#[derive(Debug, Clone, Copy)]
enum SegmentStyle {
    Text,
    Muted,
    Dim,
    Accent,
}

#[derive(Debug, Clone)]
struct RenderedSegment {
    segment: StatusLineSegment,
    span: Span<'static>,
    /// URL to wrap the visible span in an OSC 8 hyperlink. Empty when the segment is plain text.
    hyperlink: Option<String>,
}

impl RenderedSegment {
    fn new(segment: StatusLineSegment, span: Span<'static>, hyperlink: Option<String>) -> Self {
        Self {
            segment,
            span,
            hyperlink,
        }
    }

    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.span.content.as_ref())
    }
}

fn fit_segments(segments: &mut Vec<RenderedSegment>, max_width: usize, sep_width: usize) {
    while segments.len() > 1 && total_width(segments, sep_width) > max_width {
        let Some(index) = lowest_priority_index(segments) else {
            break;
        };
        segments.remove(index);
    }

    if total_width(segments, sep_width) <= max_width {
        return;
    }
    // The drop loop only stops with `segments.len() <= 1`, so truncating the widest is the
    // single-survivor truncation path even though the iterator wording suggests otherwise.
    let content_width = max_width
        .saturating_sub(2)
        .saturating_sub(sep_width.saturating_mul(segments.len().saturating_sub(1)));
    if let Some(segment) = segments.iter_mut().max_by_key(|segment| segment.width()) {
        let label = truncate_to_width(segment.span.content.as_ref(), content_width);
        segment.span = Span::styled(label, segment.span.style);
    }
}

fn total_width(segments: &[RenderedSegment], sep_width: usize) -> usize {
    2 + segments.iter().map(RenderedSegment::width).sum::<usize>()
        + sep_width.saturating_mul(segments.len().saturating_sub(1))
}

fn lowest_priority_index(segments: &[RenderedSegment]) -> Option<usize> {
    segments
        .iter()
        .enumerate()
        .min_by_key(|(_, segment)| segment_utility(segment.segment))
        .map(|(index, _)| index)
}

/// Per-segment "drop me first when narrow" rank. Lower numbers drop earlier, so run state and
/// model sit at the top because the bar is useless without them.
fn segment_utility(segment: StatusLineSegment) -> u8 {
    match segment {
        StatusLineSegment::ThreadTitle => 0,
        StatusLineSegment::CurrentTime => 1,
        StatusLineSegment::PullRequest => 2,
        StatusLineSegment::GitBranch => 3,
        StatusLineSegment::CurrentDir => 4,
        StatusLineSegment::SessionCost => 5,
        StatusLineSegment::ContextUsed => 6,
        StatusLineSegment::Model => 7,
        StatusLineSegment::ModelWithEffort => 8,
        StatusLineSegment::RunState => 9,
    }
}

fn non_empty_span(label: String, style: ratatui::style::Style) -> Option<Span<'static>> {
    (!label.is_empty()).then(|| Span::styled(label, style))
}

/// OSC 8 hyperlink opener (`ESC ] 8 ; ; URL ST`). Emitted as a `Span::raw` because the bytes
/// carry no visible glyph; `unicode_width` reports 0 for control codes, so ratatui's column
/// accounting passes the escape through without disturbing layout. Control chars in `url` are
/// stripped so a malformed value cannot break out of the hyperlink envelope.
fn osc8_open(url: &str) -> Span<'static> {
    let sanitized: String = url.chars().filter(|c| !c.is_control()).collect();
    Span::raw(format!("\x1b]8;;{sanitized}\x1b\\"))
}

fn osc8_close() -> Span<'static> {
    Span::raw("\x1b]8;;\x1b\\")
}

fn model_with_effort(model: &str, effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("{model} ({effort})"),
        None => model.to_owned(),
    }
}

fn context_label(usage: UsageSnapshot) -> String {
    match usage.context_window {
        Some(window) if window > 0 => {
            let percent = usage.context_tokens.saturating_mul(100) / window;
            format!(
                "Ctx: {percent}% ({}/{})",
                compact_tokens(usage.context_tokens),
                compact_tokens(window),
            )
        }
        _ => format!("Ctx: {}", compact_tokens(usage.context_tokens)),
    }
}

fn session_cost_label(usage: UsageSnapshot) -> Option<String> {
    usage
        .estimated_cost_usd
        .map(|cost| format!("Sess: {}", format_cost(cost)))
}

fn current_time_label() -> String {
    let now = OffsetDateTime::now_utc().to_offset(local_offset());
    format!("{:02}:{:02}", now.hour(), now.minute())
}

fn compact_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn format_cost(cost: f64) -> String {
    // Switch to two-decimal display once `{cost:.2}` would round up to `$1.00` so the bar reads
    // `$1.00` instead of `$0.9999` at the boundary.
    if cost >= 0.995 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_text(segments: Vec<StatusLineSegment>, width: u16) -> String {
        let pr = crate::util::git::PullRequest {
            number: 86,
            url: "https://github.com/o/r/pull/86".to_owned(),
        };
        let line = StatusLine::new(segments).render(
            &Theme::default(),
            &StatusLineState {
                model: "m",
                effort: Some(Effort::High),
                title: Some("title"),
                usage: Some(UsageSnapshot {
                    context_tokens: 100_000,
                    context_window: Some(200_000),
                    estimated_cost_usd: Some(0.1234),
                }),
                cwd: "~/repo",
                git_branch: Some("main"),
                pull_request: Some(&pr),
                status_span: Span::raw("Ready"),
            },
            width,
        );
        line.spans
            .into_iter()
            .map(|span| span.content)
            .collect::<String>()
    }

    fn is_hh_mm(label: &str) -> bool {
        let bytes = label.as_bytes();
        bytes.len() == 5
            && bytes[0].is_ascii_digit()
            && bytes[1].is_ascii_digit()
            && bytes[2] == b':'
            && bytes[3].is_ascii_digit()
            && bytes[4].is_ascii_digit()
    }

    // ── StatusLine::render ──

    #[test]
    fn render_current_time_uses_clock_label() {
        let text = render_text(vec![StatusLineSegment::CurrentTime], 20);
        assert!(is_hh_mm(text.trim()), "expected HH:MM label: {text:?}");
    }

    #[test]
    fn render_truncates_single_oversized_segment_to_width() {
        let line = StatusLine::new(vec![StatusLineSegment::RunState]).render(
            &Theme::default(),
            &StatusLineState {
                model: "m",
                effort: None,
                title: None,
                usage: None,
                cwd: "",
                git_branch: None,
                pull_request: None,
                status_span: Span::raw("Running a very long command name"),
            },
            12,
        );
        let text = line
            .spans
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();

        assert_eq!(text, "  Running...");
    }

    #[test]
    fn render_drops_low_utility_segments_before_usage_model_and_state() {
        let segments = vec![
            StatusLineSegment::CurrentTime,
            StatusLineSegment::SessionCost,
            StatusLineSegment::ContextUsed,
            StatusLineSegment::Model,
            StatusLineSegment::RunState,
        ];

        assert_eq!(
            render_text(segments.clone(), 34),
            "  Ctx: 50% (100k/200k) │ m │ Ready",
        );
        assert_eq!(render_text(segments.clone(), 11), "  m │ Ready");
        assert_eq!(render_text(segments, 10), "  Ready");
    }

    #[test]
    fn render_drops_plain_model_before_model_with_effort() {
        // The compact `model-with-effort` label carries strictly more information than `model`,
        // so a user who configures both keeps the more useful variant under width pressure.
        let segments = vec![
            StatusLineSegment::Model,
            StatusLineSegment::ModelWithEffort,
            StatusLineSegment::RunState,
        ];

        assert_eq!(render_text(segments.clone(), 80), "  m │ m (high) │ Ready");
        assert_eq!(render_text(segments, 18), "  m (high) │ Ready");
    }

    #[test]
    fn render_pull_request_renders_hash_prefix_and_drops_before_git_branch() {
        let segments = vec![
            StatusLineSegment::CurrentTime,
            StatusLineSegment::GitBranch,
            StatusLineSegment::PullRequest,
            StatusLineSegment::RunState,
        ];

        let full = render_text(segments.clone(), 80);
        assert!(
            full.contains("#86") && full.contains("main") && full.ends_with("Ready"),
            "wide width keeps every segment: {full}",
        );
        // Width 22 forces time (utility 1) to drop before PR (2) and branch (3). Width 14
        // narrows further until only branch and run state remain. The PR span is wrapped in
        // OSC 8 hyperlink bytes that `unicode_width` measures as zero, so column math is
        // unaffected even though the printed string carries the escape sequence.
        let pr_open = "\x1b]8;;https://github.com/o/r/pull/86\x1b\\";
        let pr_close = "\x1b]8;;\x1b\\";
        assert_eq!(
            render_text(segments.clone(), 22),
            format!("  main │ {pr_open}#86{pr_close} │ Ready"),
        );
        assert_eq!(render_text(segments, 14), "  main │ Ready");
    }

    #[test]
    fn pull_request_segment_emits_osc8_open_and_close_around_visible_text() {
        let rendered = render_text(vec![StatusLineSegment::PullRequest], 80);
        let url = "https://github.com/o/r/pull/86";
        assert!(rendered.contains(&format!("\x1b]8;;{url}\x1b\\#86\x1b]8;;\x1b\\")));
    }

    #[test]
    fn pull_request_segment_skips_osc8_when_absent() {
        let rendered = StatusLine::new(vec![StatusLineSegment::PullRequest]).render(
            &Theme::default(),
            &StatusLineState {
                model: "m",
                effort: None,
                title: None,
                usage: None,
                cwd: "~/repo",
                git_branch: None,
                pull_request: None,
                status_span: Span::raw("Ready"),
            },
            80,
        );
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("\x1b]8;;"), "no OSC 8 bytes when PR absent");
        assert!(!text.contains('#'), "no PR number rendered when absent");
    }

    // ── osc8_open ──

    #[test]
    fn osc8_open_strips_control_chars_from_url() {
        let span = osc8_open("https://example.com/\x1b\x07\x00ok");
        assert_eq!(
            span.content.as_ref(),
            "\x1b]8;;https://example.com/ok\x1b\\"
        );
    }

    // ── context_label ──

    #[test]
    fn context_label_omits_unknown_context_window() {
        assert_eq!(
            context_label(UsageSnapshot {
                context_tokens: 987,
                context_window: None,
                estimated_cost_usd: None,
            }),
            "Ctx: 987",
        );
    }

    // ── format_cost ──

    #[test]
    fn format_cost_uses_cents_for_larger_totals() {
        assert_eq!(format_cost(1.234), "$1.23");
        assert_eq!(format_cost(0.12345), "$0.1235");
    }
}
