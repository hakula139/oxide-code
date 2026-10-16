//! Ordered segment rendering for the status line.

use std::path::Path;

use ratatui::text::{Line, Span};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

use crate::client::{RateLimitWindow, RateLimits};
use crate::config::{Effort, StatusLineSegment};
use crate::protocol::UsageSnapshot;
use crate::session::display::id_prefix;
use crate::tui::theme::Theme;
use crate::util::git::{Checks, PullRequest, ReviewDecision};
use crate::util::text::truncate_to_width;
use crate::util::time::local_offset;

const MAX_CURRENT_DIR_WIDTH: usize = 40;
const MAX_GIT_BRANCH_WIDTH: usize = 32;
const MAX_TITLE_WIDTH: usize = 40;
/// Leading margin (cells) inside the status bar that lines up content with the chat block.
const STATUS_LINE_MARGIN: u16 = 2;
const STATUS_LINE_MARGIN_STR: &str = "  ";
const _: () = assert!(STATUS_LINE_MARGIN_STR.len() == STATUS_LINE_MARGIN as usize);

/// Ordered segment roster for one status-line render.
#[derive(Debug, Clone)]
pub(super) struct StatusLine {
    segments: Vec<StatusLineSegment>,
}

/// A rendered status line plus the column ranges of hyperlinkable segments.
#[derive(Debug)]
pub(super) struct RenderedStatusLine {
    pub(super) line: Line<'static>,
    pub(super) hyperlinks: Vec<RenderedHyperlink>,
}

/// Cell range of a hyperlinked segment, measured from the start of the line.
#[derive(Debug, Clone)]
pub(super) struct RenderedHyperlink {
    pub(super) col: u16,
    pub(super) width: u16,
    pub(super) url: String,
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
    ) -> RenderedStatusLine {
        let sep = theme.separator_span();
        let sep_width = UnicodeWidthStr::width(sep.content.as_ref());
        let mut rendered = self
            .segments
            .iter()
            .filter_map(|segment| Self::render_segment(*segment, theme, state))
            .collect::<Vec<_>>();
        fit_segments(&mut rendered, usize::from(width), sep_width);

        // Leading margin lines up content with the chat block underneath.
        let mut spans = vec![Span::raw(STATUS_LINE_MARGIN_STR)];
        let mut hyperlinks = Vec::new();
        let mut col: u16 = STATUS_LINE_MARGIN;
        let sep_w = u16::try_from(sep_width).unwrap_or(0);
        for (index, segment) in rendered.iter().enumerate() {
            if index > 0 {
                spans.push(sep.clone());
                col = col.saturating_add(sep_w);
            }
            let span_width = u16::try_from(segment.width()).unwrap_or(0);
            if let Some(url) = &segment.hyperlink
                && span_width > 0
            {
                hyperlinks.push(RenderedHyperlink {
                    col,
                    width: span_width,
                    url: url.clone(),
                });
            }
            spans.push(segment.span.clone());
            col = col.saturating_add(span_width);
        }
        RenderedStatusLine {
            line: Line::from(spans),
            hyperlinks,
        }
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
            StatusLineSegment::PullRequestChecks => state.pull_request.and_then(|pr| {
                let (label, style) = checks_label(pr.checks?);
                hyperlink = Some(format!("{}/checks", pr.url));
                Some(Span::styled(label, Self::segment_style(theme, style)))
            }),
            StatusLineSegment::PullRequestReview => state.pull_request.and_then(|pr| {
                let (label, style) = review_label(pr.review?);
                hyperlink = Some(format!("{}/files", pr.url));
                Some(Span::styled(label, Self::segment_style(theme, style)))
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
                .and_then(context_label)
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::SessionCost => state
                .usage
                .and_then(session_cost_label)
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::FiveHourLimit => state
                .rate_limits
                .and_then(|limits| rate_limit_label("5h", limits.five_hour))
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::WeeklyLimit => state
                .rate_limits
                .and_then(|limits| rate_limit_label("7d", limits.seven_day))
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Dim))),
            StatusLineSegment::RunState => Some(state.status_span.clone()),
            StatusLineSegment::QueuedPrompts => (state.queued_prompts > 0).then(|| {
                Span::styled(
                    format!("Queued: {}", state.queued_prompts),
                    Self::segment_style(theme, SegmentStyle::Dim),
                )
            }),
            StatusLineSegment::TaskProgress => None,
            StatusLineSegment::ThreadTitle => state.title.map(|title| {
                Span::styled(
                    truncate_to_width(title, MAX_TITLE_WIDTH),
                    Self::segment_style(theme, SegmentStyle::Muted),
                )
            }),
            StatusLineSegment::SessionId => (!state.session_id.is_empty()).then(|| {
                Span::styled(
                    format!("ID: {}", id_prefix(state.session_id)),
                    Self::segment_style(theme, SegmentStyle::Muted),
                )
            }),
            StatusLineSegment::Theme => theme_label(state.theme_name)
                .map(|label| Span::styled(label, Self::segment_style(theme, SegmentStyle::Muted))),
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
            SegmentStyle::Success => theme.success(),
            SegmentStyle::Warning => theme.warning(),
            SegmentStyle::Error => theme.error(),
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
    pub(super) pull_request: Option<&'a PullRequest>,
    pub(super) rate_limits: Option<RateLimits>,
    /// Pre-rendered run-state segment from the parent component.
    pub(super) status_span: Span<'static>,
    pub(super) queued_prompts: usize,
    pub(super) session_id: &'a str,
    /// Built-in theme name or theme file path.
    pub(super) theme_name: &'a str,
}

#[derive(Debug, Clone, Copy)]
enum SegmentStyle {
    Text,
    Muted,
    Dim,
    Accent,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
struct RenderedSegment {
    segment: StatusLineSegment,
    span: Span<'static>,
    /// OSC 8 target for the visible span.
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

/// Per-segment narrow-width rank. Lower numbers drop earlier.
fn segment_utility(segment: StatusLineSegment) -> u8 {
    match segment {
        StatusLineSegment::Theme => 0,
        StatusLineSegment::SessionId => 1,
        StatusLineSegment::ThreadTitle => 2,
        StatusLineSegment::CurrentTime => 3,
        StatusLineSegment::PullRequestReview => 4,
        StatusLineSegment::PullRequestChecks => 5,
        StatusLineSegment::PullRequest => 6,
        StatusLineSegment::GitBranch => 7,
        StatusLineSegment::CurrentDir => 8,
        StatusLineSegment::TaskProgress => 9,
        StatusLineSegment::QueuedPrompts => 10,
        StatusLineSegment::SessionCost => 11,
        StatusLineSegment::WeeklyLimit => 12,
        StatusLineSegment::FiveHourLimit => 13,
        StatusLineSegment::ContextUsed => 14,
        StatusLineSegment::Model => 15,
        StatusLineSegment::ModelWithEffort => 16,
        StatusLineSegment::RunState => 17,
    }
}

fn non_empty_span(label: String, style: ratatui::style::Style) -> Option<Span<'static>> {
    (!label.is_empty()).then(|| Span::styled(label, style))
}

fn model_with_effort(model: &str, effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("{model} ({effort})"),
        None => model.to_owned(),
    }
}

fn context_label(usage: UsageSnapshot) -> Option<String> {
    let tokens = usage.context_tokens?;
    Some(match usage.context_window {
        Some(window) if window > 0 => {
            let percent = tokens.saturating_mul(100) / window;
            format!(
                "Ctx: {percent}% ({}/{})",
                compact_tokens(tokens),
                compact_tokens(window),
            )
        }
        _ => format!("Ctx: {}", compact_tokens(tokens)),
    })
}

fn session_cost_label(usage: UsageSnapshot) -> Option<String> {
    usage
        .estimated_cost_usd
        .map(|cost| format!("Sess: {}", format_cost(cost)))
}

fn rate_limit_label(name: &str, window: Option<RateLimitWindow>) -> Option<String> {
    // Rounds to basis points before flooring, since `0.29 * 100.0` is `28.999...` in binary64.
    let percent = ((window?.utilization * 10_000.0).round() / 100.0).floor();
    Some(format!("{name}: {percent:.0}%"))
}

fn checks_label(checks: Checks) -> (String, SegmentStyle) {
    match checks {
        Checks::Failing(count) => (format!("Checks: {count} failing"), SegmentStyle::Error),
        Checks::Pending(count) => (format!("Checks: {count} pending"), SegmentStyle::Warning),
        Checks::Cancelled(count) => (format!("Checks: {count} cancelled"), SegmentStyle::Muted),
        Checks::Passing => ("Checks: passing".to_owned(), SegmentStyle::Success),
    }
}

fn review_label(review: ReviewDecision) -> (&'static str, SegmentStyle) {
    match review {
        ReviewDecision::Approved => ("Review: approved", SegmentStyle::Success),
        ReviewDecision::ChangesRequested => ("Review: changes requested", SegmentStyle::Error),
        ReviewDecision::ReviewRequired => ("Review: required", SegmentStyle::Warning),
    }
}

fn theme_label(theme_name: &str) -> Option<String> {
    let name = Path::new(theme_name).file_stem()?.to_str()?;
    Some(format!("Theme: {name}"))
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

    /// Resets in 2100, so a window built with it stays current.
    const FUTURE_RESET: i64 = 4_102_444_800;

    /// No optional data, idle, with model `m`.
    fn bare_state() -> StatusLineState<'static> {
        StatusLineState {
            model: "m",
            effort: None,
            title: None,
            usage: None,
            cwd: "~/repo",
            git_branch: None,
            pull_request: None,
            rate_limits: None,
            status_span: Span::raw("Ready"),
            queued_prompts: 0,
            session_id: "",
            theme_name: "",
        }
    }

    fn render(segments: Vec<StatusLineSegment>, state: &StatusLineState<'_>) -> RenderedStatusLine {
        StatusLine::new(segments).render(&Theme::default(), state, 80)
    }

    fn text_of(rendered: &RenderedStatusLine) -> String {
        rendered
            .line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn render_text(segments: Vec<StatusLineSegment>, width: u16) -> String {
        let pr = pr_state(86);
        let rendered = StatusLine::new(segments).render(
            &Theme::default(),
            &StatusLineState {
                effort: Some(Effort::High),
                title: Some("title"),
                usage: Some(UsageSnapshot {
                    context_tokens: Some(100_000),
                    context_window: Some(200_000),
                    estimated_cost_usd: Some(0.1234),
                }),
                git_branch: Some("main"),
                pull_request: Some(&pr),
                ..bare_state()
            },
            width,
        );
        text_of(&rendered)
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

    fn pr_state(number: u64) -> PullRequest {
        PullRequest {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
            checks: None,
            review: None,
        }
    }

    // ── StatusLine::render ──

    #[test]
    fn render_current_time_uses_clock_label() {
        let text = render_text(vec![StatusLineSegment::CurrentTime], 20);
        assert!(is_hh_mm(text.trim()), "expected HH:MM label: {text:?}");
    }

    #[test]
    fn render_truncates_single_oversized_segment_to_width() {
        let rendered = StatusLine::new(vec![StatusLineSegment::RunState]).render(
            &Theme::default(),
            &StatusLineState {
                cwd: "",
                status_span: Span::raw("Running a very long command name"),
                ..bare_state()
            },
            12,
        );

        assert_eq!(text_of(&rendered), "  Running...");
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
        assert_eq!(render_text(segments.clone(), 22), "  main │ #86 │ Ready");
        assert_eq!(render_text(segments, 14), "  main │ Ready");
    }

    #[test]
    fn render_pull_request_reports_hyperlink_range() {
        let pr = pr_state(86);
        let rendered = render(
            vec![StatusLineSegment::PullRequest],
            &StatusLineState {
                pull_request: Some(&pr),
                ..bare_state()
            },
        );
        assert_eq!(rendered.hyperlinks.len(), 1);
        assert_eq!(rendered.hyperlinks[0].col, 2);
        assert_eq!(rendered.hyperlinks[0].width, 3);
        assert_eq!(rendered.hyperlinks[0].url, pr.url);
    }

    #[test]
    fn render_pull_request_reports_no_hyperlink_when_absent() {
        let rendered = render(vec![StatusLineSegment::PullRequest], &bare_state());
        assert!(rendered.hyperlinks.is_empty(), "no PR → no hyperlink range");
        assert!(
            !text_of(&rendered).contains('#'),
            "no PR number rendered when absent"
        );
    }

    #[test]
    fn render_pull_request_checks_label_their_worst_state_and_link_the_checks_page() {
        let theme = Theme::default();
        for (checks, label, style) in [
            (Checks::Failing(2), "Checks: 2 failing", theme.error()),
            (Checks::Pending(1), "Checks: 1 pending", theme.warning()),
            (Checks::Cancelled(3), "Checks: 3 cancelled", theme.muted()),
            (Checks::Passing, "Checks: passing", theme.success()),
        ] {
            let pr = PullRequest {
                checks: Some(checks),
                ..pr_state(86)
            };
            let rendered = render(
                vec![StatusLineSegment::PullRequestChecks],
                &StatusLineState {
                    pull_request: Some(&pr),
                    ..bare_state()
                },
            );

            assert_eq!(text_of(&rendered), format!("  {label}"));
            assert_eq!(rendered.line.spans[1].style, style, "{label}");
            assert_eq!(
                rendered.hyperlinks[0].url,
                "https://github.com/o/r/pull/86/checks"
            );
        }
    }

    #[test]
    fn render_pull_request_review_labels_the_decision_and_links_the_review_page() {
        let theme = Theme::default();
        for (review, label, style) in [
            (
                ReviewDecision::Approved,
                "Review: approved",
                theme.success(),
            ),
            (
                ReviewDecision::ChangesRequested,
                "Review: changes requested",
                theme.error(),
            ),
            (
                ReviewDecision::ReviewRequired,
                "Review: required",
                theme.warning(),
            ),
        ] {
            let pr = PullRequest {
                review: Some(review),
                ..pr_state(86)
            };
            let rendered = render(
                vec![StatusLineSegment::PullRequestReview],
                &StatusLineState {
                    pull_request: Some(&pr),
                    ..bare_state()
                },
            );

            assert_eq!(text_of(&rendered), format!("  {label}"));
            assert_eq!(rendered.line.spans[1].style, style, "{label}");
            assert_eq!(
                rendered.hyperlinks[0].url,
                "https://github.com/o/r/pull/86/files"
            );
        }
    }

    #[test]
    fn render_pull_request_metadata_hides_without_checks_or_review() {
        let pr = pr_state(86);
        let rendered = render(
            vec![
                StatusLineSegment::PullRequestChecks,
                StatusLineSegment::PullRequestReview,
                StatusLineSegment::RunState,
            ],
            &StatusLineState {
                pull_request: Some(&pr),
                ..bare_state()
            },
        );

        assert_eq!(text_of(&rendered), "  Ready");
        assert!(rendered.hyperlinks.is_empty());
    }

    #[test]
    fn render_rate_limits_floor_the_spent_share_to_a_whole_percent() {
        let window = |utilization| {
            Some(RateLimitWindow {
                utilization,
                resets_at: FUTURE_RESET,
            })
        };
        let segments = vec![
            StatusLineSegment::FiveHourLimit,
            StatusLineSegment::WeeklyLimit,
            StatusLineSegment::RunState,
        ];
        let text = |five_hour, seven_day| {
            text_of(&render(
                segments.clone(),
                &StatusLineState {
                    rate_limits: Some(RateLimits {
                        five_hour,
                        seven_day,
                    }),
                    ..bare_state()
                },
            ))
        };

        assert_eq!(
            text(window(0.429), window(0.995)),
            "  5h: 42% │ 7d: 99% │ Ready",
        );
        assert_eq!(
            text(window(0.29), window(0.57)),
            "  5h: 29% │ 7d: 57% │ Ready",
            "a two-decimal share keeps its percent despite binary rounding",
        );
        assert_eq!(text(None, window(0.13)), "  7d: 13% │ Ready");
        assert_eq!(text(None, None), "  Ready");
        assert_eq!(
            text_of(&render(segments, &bare_state())),
            "  Ready",
            "no limits before the first response reports them",
        );
    }

    #[test]
    fn render_queued_prompts_counts_the_queue_and_hides_an_empty_one() {
        let text = |queued_prompts| {
            text_of(&render(
                vec![
                    StatusLineSegment::QueuedPrompts,
                    StatusLineSegment::RunState,
                ],
                &StatusLineState {
                    queued_prompts,
                    ..bare_state()
                },
            ))
        };

        assert_eq!(text(2), "  Queued: 2 │ Ready");
        assert_eq!(text(0), "  Ready");
    }

    #[test]
    fn render_session_id_shows_the_resumable_prefix() {
        let text = |session_id| {
            text_of(&render(
                vec![StatusLineSegment::SessionId, StatusLineSegment::RunState],
                &StatusLineState {
                    session_id,
                    ..bare_state()
                },
            ))
        };

        assert_eq!(
            text("3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b"),
            "  ID: 3f2a9c1e │ Ready"
        );
        assert_eq!(text(""), "  Ready");
    }

    #[test]
    fn render_theme_names_a_built_in_or_a_theme_file_by_its_stem() {
        let text = |theme_name| {
            text_of(&render(
                vec![StatusLineSegment::Theme, StatusLineSegment::RunState],
                &StatusLineState {
                    theme_name,
                    ..bare_state()
                },
            ))
        };

        assert_eq!(text("latte"), "  Theme: latte │ Ready");
        assert_eq!(text("/home/me/themes/dusk.toml"), "  Theme: dusk │ Ready");
        assert_eq!(text(""), "  Ready");
    }

    // ── segment_utility ──

    /// The drop order the configuration guide documents, plus `task-progress`, which renders
    /// nothing yet.
    #[test]
    fn segment_utility_follows_the_documented_drop_order() {
        let drop_order = [
            StatusLineSegment::Theme,
            StatusLineSegment::SessionId,
            StatusLineSegment::ThreadTitle,
            StatusLineSegment::CurrentTime,
            StatusLineSegment::PullRequestReview,
            StatusLineSegment::PullRequestChecks,
            StatusLineSegment::PullRequest,
            StatusLineSegment::GitBranch,
            StatusLineSegment::CurrentDir,
            StatusLineSegment::TaskProgress,
            StatusLineSegment::QueuedPrompts,
            StatusLineSegment::SessionCost,
            StatusLineSegment::WeeklyLimit,
            StatusLineSegment::FiveHourLimit,
            StatusLineSegment::ContextUsed,
            StatusLineSegment::Model,
            StatusLineSegment::ModelWithEffort,
            StatusLineSegment::RunState,
        ];
        for (earlier, later) in drop_order.iter().zip(&drop_order[1..]) {
            assert!(
                segment_utility(*earlier) < segment_utility(*later),
                "{earlier:?} must drop before {later:?}",
            );
        }
    }

    // ── context_label ──

    #[test]
    fn context_label_omits_unknown_context_window() {
        assert_eq!(
            context_label(UsageSnapshot {
                context_tokens: Some(987),
                context_window: None,
                estimated_cost_usd: None,
            })
            .as_deref(),
            Some("Ctx: 987"),
        );
    }

    #[test]
    fn context_label_hides_context_a_compaction_cleared() {
        assert_eq!(
            context_label(UsageSnapshot {
                context_tokens: None,
                context_window: Some(200_000),
                estimated_cost_usd: Some(1.0),
            }),
            None,
        );
    }

    // ── format_cost ──

    #[test]
    fn format_cost_uses_cents_for_larger_totals() {
        assert_eq!(format_cost(1.234), "$1.23");
        assert_eq!(format_cost(0.12345), "$0.1235");
    }
}
