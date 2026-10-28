//! Ordered segment rendering for the status line.

use std::cmp::Reverse;
use std::path::Path;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use ox_core::client::{RateLimitWindow, RateLimits};
use ox_core::config::StatusLineSegment;
use ox_core::model::Effort;
use ox_core::permission::Mode;
use ox_core::protocol::UsageSnapshot;
use ox_core::session::display::id_prefix;
use ox_core::tool::tasks::Progress;
use ox_core::util::git::{Checks, PullRequest, ReviewDecision};
use ox_core::util::text::{compact_count, truncate_to_width};

use crate::components::LEFT_MARGIN;
use crate::saturating_u16;
use crate::theme::Theme;

const MAX_CURRENT_DIR_WIDTH: usize = 40;
const MAX_GIT_BRANCH_WIDTH: usize = 32;
const MAX_TITLE_WIDTH: usize = 40;

/// Shown percent from which the context and usage-limit segments take the warning style.
const USAGE_WARNING_PERCENT: f64 = 70.0;
/// Shown percent from which the context and usage-limit segments take the error style.
const USAGE_ERROR_PERCENT: f64 = 90.0;

// ── StatusLine ──

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

        let mut spans = vec![Span::raw(LEFT_MARGIN)];
        let mut hyperlinks = Vec::new();
        let mut col = saturating_u16(LEFT_MARGIN.width());
        let sep_w = saturating_u16(sep_width);
        for (index, segment) in rendered.iter().enumerate() {
            if index > 0 {
                spans.push(sep.clone());
                col = col.saturating_add(sep_w);
            }
            let span_width = saturating_u16(segment.width());
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
                SegmentStyle::Muted.style(theme),
            ),
            StatusLineSegment::GitBranch => state.git_branch.map(|branch| {
                Span::styled(
                    truncate_to_width(branch, MAX_GIT_BRANCH_WIDTH),
                    SegmentStyle::Accent.style(theme),
                )
            }),
            StatusLineSegment::PullRequest => state.pull_request.map(|pr| {
                hyperlink = Some(pr.url.clone());
                Span::styled(format!("#{}", pr.number), SegmentStyle::Accent.style(theme))
            }),
            StatusLineSegment::PullRequestChecks => state.pull_request.and_then(|pr| {
                let (label, style) = checks_label(pr.checks?);
                hyperlink = Some(format!("{}/checks", pr.url));
                Some(Span::styled(label, style.style(theme)))
            }),
            StatusLineSegment::PullRequestReview => state.pull_request.and_then(|pr| {
                let (label, style) = review_label(pr.review?);
                hyperlink = Some(format!("{}/files", pr.url));
                Some(Span::styled(label, style.style(theme)))
            }),
            StatusLineSegment::Model => Some(Span::styled(
                state.model.to_owned(),
                SegmentStyle::Text.style(theme),
            )),
            StatusLineSegment::ModelWithEffort => Some(Span::styled(
                model_with_effort(state.model, state.effort),
                SegmentStyle::Text.style(theme),
            )),
            StatusLineSegment::ContextUsed => state
                .usage
                .and_then(context_label)
                .map(|(label, style)| Span::styled(label, style.style(theme))),
            StatusLineSegment::SessionCost => state
                .usage
                .and_then(session_cost_label)
                .map(|label| Span::styled(label, SegmentStyle::Muted.style(theme))),
            StatusLineSegment::FiveHourLimit => state
                .rate_limits
                .and_then(|limits| rate_limit_label("5h", limits.five_hour))
                .map(|(label, style)| Span::styled(label, style.style(theme))),
            StatusLineSegment::WeeklyLimit => state
                .rate_limits
                .and_then(|limits| rate_limit_label("7d", limits.seven_day))
                .map(|(label, style)| Span::styled(label, style.style(theme))),
            StatusLineSegment::PermissionMode => {
                let (label, style) = permission_mode_label(state.permission_mode);
                Some(Span::styled(label, style.style(theme)))
            }
            StatusLineSegment::RunState => Some(state.status_span.clone()),
            StatusLineSegment::QueuedPrompts => (state.queued_prompts > 0).then(|| {
                Span::styled(
                    format!("Queued: {}", state.queued_prompts),
                    SegmentStyle::Muted.style(theme),
                )
            }),
            StatusLineSegment::TaskProgress => state.task_progress.map(|progress| {
                Span::styled(
                    format!("Tasks: {progress}"),
                    SegmentStyle::Muted.style(theme),
                )
            }),
            StatusLineSegment::ThreadTitle => state.title.map(|title| {
                Span::styled(
                    truncate_to_width(title, MAX_TITLE_WIDTH),
                    SegmentStyle::Muted.style(theme),
                )
            }),
            StatusLineSegment::SessionId => (!state.session_id.is_empty()).then(|| {
                Span::styled(
                    format!("ID: {}", id_prefix(state.session_id)),
                    SegmentStyle::Muted.style(theme),
                )
            }),
            StatusLineSegment::Theme => theme_label(state.theme_name)
                .map(|label| Span::styled(label, SegmentStyle::Muted.style(theme))),
            StatusLineSegment::CurrentTime => state.current_minute.map(|minute| {
                Span::styled(
                    format!("{:02}:{:02}", minute / 60, minute % 60),
                    SegmentStyle::Dim.style(theme),
                )
            }),
        }?;
        Some(RenderedSegment::new(
            segment_utility(segment, state.permission_mode),
            span,
            hyperlink,
        ))
    }
}

pub(super) struct StatusLineState<'a> {
    pub(super) model: &'a str,
    pub(super) effort: Option<Effort>,
    pub(super) title: Option<&'a str>,
    pub(super) usage: Option<UsageSnapshot>,
    /// Already shortened with `~` by `tildify`, so the renderer must not substitute it again.
    pub(super) cwd: &'a str,
    pub(super) git_branch: Option<&'a str>,
    pub(super) pull_request: Option<&'a PullRequest>,
    pub(super) rate_limits: Option<RateLimits>,
    pub(super) permission_mode: Mode,
    /// Pre-rendered run-state segment from the parent component.
    pub(super) status_span: Span<'static>,
    pub(super) queued_prompts: usize,
    /// `None` while the session keeps no task list.
    pub(super) task_progress: Option<Progress>,
    pub(super) session_id: &'a str,
    /// Built-in theme name or theme file path.
    pub(super) theme_name: &'a str,
    /// Minute of the local day, or `None` unless the line shows the current time.
    pub(super) current_minute: Option<u16>,
}

// ── Segments ──

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

impl SegmentStyle {
    fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Text => theme.text(),
            Self::Muted => theme.muted(),
            Self::Dim => theme.dim(),
            Self::Accent => theme.accent(),
            Self::Success => theme.success(),
            Self::Warning => theme.warning(),
            Self::Error => theme.error(),
        }
    }
}

#[derive(Debug, Clone)]
struct RenderedSegment {
    utility: u8,
    span: Span<'static>,
    /// OSC 8 target for the visible span.
    hyperlink: Option<String>,
}

impl RenderedSegment {
    fn new(utility: u8, span: Span<'static>, hyperlink: Option<String>) -> Self {
        Self {
            utility,
            span,
            hyperlink,
        }
    }

    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.span.content.as_ref())
    }
}

fn fit_segments(segments: &mut Vec<RenderedSegment>, max_width: usize, sep_width: usize) {
    let mut ranked: Vec<_> = std::mem::take(segments).into_iter().enumerate().collect();
    ranked.sort_by_key(|(index, segment)| (Reverse(segment.utility), Reverse(*index)));

    let mut width = LEFT_MARGIN.width();
    let mut fitting = Vec::with_capacity(ranked.len());
    for (index, segment) in ranked {
        let separator = if fitting.is_empty() { 0 } else { sep_width };
        let next_width = width + separator + segment.width();
        if fitting.is_empty() || next_width <= max_width {
            width = next_width;
            fitting.push((index, segment));
        }
    }
    fitting.sort_by_key(|(index, _)| *index);
    segments.extend(fitting.into_iter().map(|(_, segment)| segment));

    if total_width(segments, sep_width) <= max_width {
        return;
    }
    if let [segment] = segments.as_mut_slice() {
        let content_width = max_width.saturating_sub(LEFT_MARGIN.width());
        let label = truncate_to_width(segment.span.content.as_ref(), content_width);
        segment.span = Span::styled(label, segment.span.style);
    }
}

fn total_width(segments: &[RenderedSegment], sep_width: usize) -> usize {
    LEFT_MARGIN.width()
        + segments.iter().map(RenderedSegment::width).sum::<usize>()
        + sep_width.saturating_mul(segments.len().saturating_sub(1))
}

/// Per-segment narrow-width rank. Higher numbers are considered first.
fn segment_utility(segment: StatusLineSegment, mode: Mode) -> u8 {
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
        StatusLineSegment::PermissionMode if mode == Mode::Manual => 15,
        StatusLineSegment::Model => 16,
        StatusLineSegment::ModelWithEffort => 17,
        StatusLineSegment::PermissionMode => 18,
        StatusLineSegment::RunState => 19,
    }
}

// ── Labels ──

fn non_empty_span(label: String, style: Style) -> Option<Span<'static>> {
    (!label.is_empty()).then(|| Span::styled(label, style))
}

fn model_with_effort(model: &str, effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("{model} ({effort})"),
        None => model.to_owned(),
    }
}

fn context_label(usage: UsageSnapshot) -> Option<(String, SegmentStyle)> {
    let tokens = usage.context_tokens?;
    Some(match usage.context_window {
        Some(window) if window > 0 => {
            let percent = tokens.saturating_mul(100) / window;
            let label = format!(
                "Ctx: {percent}% ({}/{})",
                compact_count(tokens),
                compact_count(window),
            );
            (label, usage_style(f64::from(percent)))
        }
        _ => (
            format!("Ctx: {}", compact_count(tokens)),
            SegmentStyle::Text,
        ),
    })
}

fn session_cost_label(usage: UsageSnapshot) -> Option<String> {
    usage
        .estimated_cost_usd
        .map(|cost| format!("Cost: {}", format_cost(cost)))
}

fn rate_limit_label(name: &str, window: Option<RateLimitWindow>) -> Option<(String, SegmentStyle)> {
    // Rounds to basis points before flooring, since `0.29 * 100.0` is `28.999...` in binary64.
    let percent = ((window?.utilization * 10_000.0).round() / 100.0).floor();
    Some((format!("{name}: {percent:.0}%"), usage_style(percent)))
}

/// The style for a shown spent share, which escalates as the context or a limit nears its end.
fn usage_style(percent: f64) -> SegmentStyle {
    if percent >= USAGE_ERROR_PERCENT {
        SegmentStyle::Error
    } else if percent >= USAGE_WARNING_PERCENT {
        SegmentStyle::Warning
    } else {
        SegmentStyle::Text
    }
}

fn permission_mode_label(mode: Mode) -> (&'static str, SegmentStyle) {
    let style = match mode {
        Mode::Manual => SegmentStyle::Muted,
        Mode::Plan => SegmentStyle::Accent,
        Mode::Yolo => SegmentStyle::Error,
    };
    (mode.as_str(), style)
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

fn format_cost(cost: f64) -> String {
    // Cents take over once `{cost:.4}` would round up to `$0.0100`, so `$0.00996` reads `$0.01`.
    if cost < 0.009_95 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

#[cfg(test)]
mod tests {
    use ox_core::util::git::testing::pull_request;

    use super::*;

    mod render {
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
                permission_mode: Mode::Manual,
                status_span: Span::raw("Ready"),
                queued_prompts: 0,
                task_progress: None,
                session_id: "",
                theme_name: "",
                current_minute: None,
            }
        }

        fn render(
            segments: Vec<StatusLineSegment>,
            state: &StatusLineState<'_>,
        ) -> RenderedStatusLine {
            StatusLine::new(segments).render(&Theme::default(), state, 80)
        }

        fn text_of(rendered: &RenderedStatusLine) -> String {
            rendered.line.to_string()
        }

        fn render_text(segments: Vec<StatusLineSegment>, width: u16) -> String {
            let pr = pull_request(86);
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
                    current_minute: Some(9 * 60 + 5),
                    ..bare_state()
                },
                width,
            );
            text_of(&rendered)
        }

        #[test]
        fn pull_request_renders_hash_prefix_and_drops_before_git_branch() {
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
        fn pull_request_reports_hyperlink_range() {
            let pr = pull_request(86);
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
        fn pull_request_reports_no_hyperlink_when_absent() {
            let rendered = render(vec![StatusLineSegment::PullRequest], &bare_state());
            assert!(rendered.hyperlinks.is_empty(), "no PR → no hyperlink range");
            assert!(
                !text_of(&rendered).contains('#'),
                "no PR number rendered when absent"
            );
        }

        #[test]
        fn pull_request_checks_label_their_worst_state_and_link_the_checks_page() {
            let theme = Theme::default();
            for (checks, label, style) in [
                (Checks::Failing(2), "Checks: 2 failing", theme.error()),
                (Checks::Pending(1), "Checks: 1 pending", theme.warning()),
                (Checks::Cancelled(3), "Checks: 3 cancelled", theme.muted()),
                (Checks::Passing, "Checks: passing", theme.success()),
            ] {
                let pr = PullRequest {
                    checks: Some(checks),
                    ..pull_request(86)
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
        fn pull_request_review_labels_the_decision_and_links_the_review_page() {
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
                    ..pull_request(86)
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
        fn pull_request_metadata_hides_without_checks_or_review() {
            let pr = pull_request(86);
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
        fn context_used_escalates_from_text_to_warning_and_error() {
            let theme = Theme::default();
            let render_context = |tokens| {
                render(
                    vec![StatusLineSegment::ContextUsed],
                    &StatusLineState {
                        usage: Some(UsageSnapshot {
                            context_tokens: Some(tokens),
                            context_window: Some(1_000_000),
                            estimated_cost_usd: None,
                        }),
                        ..bare_state()
                    },
                )
            };

            for (tokens, label, style) in [
                (23_000, "Ctx: 2% (23k/1M)", theme.text()),
                (699_999, "Ctx: 69% (699k/1M)", theme.text()),
                (700_000, "Ctx: 70% (700k/1M)", theme.warning()),
                (899_999, "Ctx: 89% (899k/1M)", theme.warning()),
                (900_000, "Ctx: 90% (900k/1M)", theme.error()),
            ] {
                let rendered = render_context(tokens);

                assert_eq!(text_of(&rendered), format!("  {label}"));
                assert_eq!(rendered.line.spans[1].style, style, "{label}");
            }
        }

        #[test]
        fn context_used_without_a_window_shows_tokens_in_the_text_style() {
            let rendered = render(
                vec![StatusLineSegment::ContextUsed],
                &StatusLineState {
                    usage: Some(UsageSnapshot {
                        context_tokens: Some(987),
                        context_window: None,
                        estimated_cost_usd: None,
                    }),
                    ..bare_state()
                },
            );

            assert_eq!(text_of(&rendered), "  Ctx: 987");
            assert_eq!(rendered.line.spans[1].style, Theme::default().text());
        }

        #[test]
        fn session_cost_reads_in_the_muted_style() {
            let rendered = render(
                vec![StatusLineSegment::SessionCost],
                &StatusLineState {
                    usage: Some(UsageSnapshot {
                        context_tokens: None,
                        context_window: None,
                        estimated_cost_usd: Some(0.5385),
                    }),
                    ..bare_state()
                },
            );

            assert_eq!(text_of(&rendered), "  Cost: $0.54");
            assert_eq!(rendered.line.spans[1].style, Theme::default().muted());
        }

        #[test]
        fn rate_limits_floor_the_spent_share_to_a_whole_percent() {
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
        fn rate_limits_escalate_from_text_to_warning_and_error_by_the_shown_percent() {
            let theme = Theme::default();
            for (utilization, percent, style) in [
                (0.05, "5%", theme.text()),
                (0.6999, "69%", theme.text()),
                (0.70, "70%", theme.warning()),
                (0.8999, "89%", theme.warning()),
                (0.90, "90%", theme.error()),
                (1.0, "100%", theme.error()),
            ] {
                let window = Some(RateLimitWindow {
                    utilization,
                    resets_at: FUTURE_RESET,
                });
                let rendered = render(
                    vec![
                        StatusLineSegment::FiveHourLimit,
                        StatusLineSegment::WeeklyLimit,
                    ],
                    &StatusLineState {
                        rate_limits: Some(RateLimits {
                            five_hour: window,
                            seven_day: window,
                        }),
                        ..bare_state()
                    },
                );

                assert_eq!(
                    text_of(&rendered),
                    format!("  5h: {percent} │ 7d: {percent}")
                );
                assert_eq!(rendered.line.spans[1].style, style, "5h at {percent}");
                assert_eq!(rendered.line.spans[3].style, style, "7d at {percent}");
            }
        }

        #[test]
        fn permission_mode_shows_each_mode_in_its_style() {
            let theme = Theme::default();

            for (permission_mode, label, style) in [
                (Mode::Manual, "manual", theme.muted()),
                (Mode::Plan, "plan", theme.accent()),
                (Mode::Yolo, "yolo", theme.error()),
            ] {
                let rendered = render(
                    vec![
                        StatusLineSegment::PermissionMode,
                        StatusLineSegment::RunState,
                    ],
                    &StatusLineState {
                        permission_mode,
                        ..bare_state()
                    },
                );

                assert_eq!(text_of(&rendered), format!("  {label} │ Ready"));
                assert_eq!(rendered.line.spans[1].style, style, "{permission_mode}");
            }
        }

        #[test]
        fn model_with_effort_shows_the_bare_model_without_an_effort() {
            let rendered = render(
                vec![
                    StatusLineSegment::ModelWithEffort,
                    StatusLineSegment::RunState,
                ],
                &bare_state(),
            );

            assert_eq!(text_of(&rendered), "  m │ Ready");
        }

        #[test]
        fn manual_mode_yields_to_the_model_while_plan_and_yolo_stay_visible() {
            for (model_segment, model, label) in [
                (StatusLineSegment::Model, "long-model", "long-model"),
                (StatusLineSegment::ModelWithEffort, "m", "m (high)"),
            ] {
                for (permission_mode, expected) in [
                    (Mode::Manual, format!("  {label} │ Ready")),
                    (Mode::Plan, "  plan │ Ready".to_owned()),
                    (Mode::Yolo, "  yolo │ Ready".to_owned()),
                ] {
                    let state = StatusLineState {
                        model,
                        effort: Some(Effort::High),
                        permission_mode,
                        ..bare_state()
                    };

                    let rendered = StatusLine::new(vec![
                        model_segment,
                        StatusLineSegment::PermissionMode,
                        StatusLineSegment::RunState,
                    ])
                    .render(&Theme::default(), &state, 22);

                    assert_eq!(
                        text_of(&rendered),
                        expected,
                        "{model_segment:?} / {permission_mode}"
                    );
                }
            }
        }

        #[test]
        fn an_oversized_manual_label_leaves_room_for_the_working_directory() {
            let state = StatusLineState {
                cwd: "~/r",
                model: "test-model",
                effort: Some(Effort::High),
                ..bare_state()
            };

            let rendered = StatusLine::new(StatusLineSegment::DEFAULT.to_vec()).render(
                &Theme::default(),
                &state,
                35,
            );

            assert_eq!(text_of(&rendered), "  ~/r │ test-model (high) │ Ready");
        }

        #[test]
        fn configured_roster_controls_every_fields_membership_and_order() {
            let roster = [
                (StatusLineSegment::RunState, "Ready"),
                (StatusLineSegment::PermissionMode, "yolo"),
                (StatusLineSegment::CurrentTime, "09:05"),
                (StatusLineSegment::Theme, "Theme: mocha"),
                (StatusLineSegment::SessionId, "ID: 3f2a9c1e"),
                (StatusLineSegment::ThreadTitle, "Session title"),
                (StatusLineSegment::TaskProgress, "Tasks: 2/5"),
                (StatusLineSegment::QueuedPrompts, "Queued: 2"),
                (StatusLineSegment::WeeklyLimit, "7d: 57%"),
                (StatusLineSegment::FiveHourLimit, "5h: 42%"),
                (StatusLineSegment::SessionCost, "Cost: $0.12"),
                (StatusLineSegment::ContextUsed, "Ctx: 50% (100k/200k)"),
                (StatusLineSegment::ModelWithEffort, "m (high)"),
                (StatusLineSegment::Model, "m"),
                (StatusLineSegment::PullRequestReview, "Review: approved"),
                (StatusLineSegment::PullRequestChecks, "Checks: passing"),
                (StatusLineSegment::PullRequest, "#86"),
                (StatusLineSegment::GitBranch, "main"),
                (StatusLineSegment::CurrentDir, "~/repo"),
            ];
            let pr = PullRequest {
                checks: Some(Checks::Passing),
                review: Some(ReviewDecision::Approved),
                ..pull_request(86)
            };
            let state = StatusLineState {
                effort: Some(Effort::High),
                title: Some("Session title"),
                usage: Some(UsageSnapshot {
                    context_tokens: Some(100_000),
                    context_window: Some(200_000),
                    estimated_cost_usd: Some(0.1234),
                }),
                git_branch: Some("main"),
                pull_request: Some(&pr),
                rate_limits: Some(RateLimits {
                    five_hour: Some(RateLimitWindow {
                        utilization: 0.42,
                        resets_at: FUTURE_RESET,
                    }),
                    seven_day: Some(RateLimitWindow {
                        utilization: 0.57,
                        resets_at: FUTURE_RESET,
                    }),
                }),
                permission_mode: Mode::Yolo,
                queued_prompts: 2,
                task_progress: Some(Progress {
                    completed: 2,
                    total: 5,
                }),
                session_id: "3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b",
                theme_name: "mocha",
                current_minute: Some(9 * 60 + 5),
                ..bare_state()
            };

            for reversed in [false, true] {
                for omitted in
                    std::iter::once(None).chain(roster.iter().map(|(segment, _)| Some(*segment)))
                {
                    let mut included: Vec<_> = roster
                        .iter()
                        .copied()
                        .filter(|(segment, _)| Some(*segment) != omitted)
                        .collect();
                    if reversed {
                        included.reverse();
                    }
                    let (segments, labels): (Vec<_>, Vec<_>) = included.into_iter().unzip();

                    let rendered = StatusLine::new(segments).render(&Theme::default(), &state, 512);

                    assert_eq!(
                        text_of(&rendered),
                        format!("  {}", labels.join(" │ ")),
                        "omitted={omitted:?}, reversed={reversed}",
                    );
                }
            }
        }

        #[test]
        fn width_fitting_keeps_the_configured_order_of_retained_fields() {
            for (reversed, expected) in
                [(false, "  Ready │ yolo │ m"), (true, "  m │ yolo │ Ready")]
            {
                let mut segments = vec![
                    StatusLineSegment::CurrentDir,
                    StatusLineSegment::RunState,
                    StatusLineSegment::PermissionMode,
                    StatusLineSegment::ThreadTitle,
                    StatusLineSegment::ModelWithEffort,
                    StatusLineSegment::Model,
                ];
                if reversed {
                    segments.reverse();
                }
                let state = StatusLineState {
                    effort: Some(Effort::High),
                    title: Some("Session title"),
                    permission_mode: Mode::Yolo,
                    ..bare_state()
                };

                let rendered = StatusLine::new(segments).render(&Theme::default(), &state, 22);

                assert_eq!(text_of(&rendered), expected, "reversed={reversed}");
            }
        }

        #[test]
        fn queued_prompts_counts_the_queue_in_the_muted_style_and_hides_an_empty_one() {
            let rendered = |queued_prompts| {
                render(
                    vec![
                        StatusLineSegment::QueuedPrompts,
                        StatusLineSegment::RunState,
                    ],
                    &StatusLineState {
                        queued_prompts,
                        ..bare_state()
                    },
                )
            };

            let queued = rendered(2);
            assert_eq!(text_of(&queued), "  Queued: 2 │ Ready");
            assert_eq!(queued.line.spans[1].style, Theme::default().muted());

            assert_eq!(text_of(&rendered(0)), "  Ready");
        }

        #[test]
        fn task_progress_counts_completed_tasks_in_the_muted_style_and_hides_without_a_list() {
            let rendered = |task_progress| {
                render(
                    vec![StatusLineSegment::TaskProgress, StatusLineSegment::RunState],
                    &StatusLineState {
                        task_progress,
                        ..bare_state()
                    },
                )
            };

            let progress = rendered(Some(Progress {
                completed: 2,
                total: 5,
            }));
            assert_eq!(text_of(&progress), "  Tasks: 2/5 │ Ready");
            assert_eq!(progress.line.spans[1].style, Theme::default().muted());

            assert_eq!(text_of(&rendered(None)), "  Ready");
        }

        #[test]
        fn session_id_shows_the_resumable_prefix() {
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
        fn theme_names_a_built_in_or_a_theme_file_by_its_stem() {
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

        #[test]
        fn current_time_shows_the_minute_as_hours_and_minutes() {
            let text = render_text(vec![StatusLineSegment::CurrentTime], 20);

            assert_eq!(text, "  09:05");
        }

        #[test]
        fn keeps_the_highest_utility_segment_when_it_needs_truncation() {
            for (segments, status, width, expected) in [
                (
                    vec![StatusLineSegment::RunState],
                    "Running a very long command name",
                    12,
                    "  Running...",
                ),
                (
                    vec![StatusLineSegment::Model, StatusLineSegment::RunState],
                    "Streaming · Esc to interrupt",
                    22,
                    "  Streaming · Esc t...",
                ),
            ] {
                let state = StatusLineState {
                    status_span: Span::raw(status),
                    ..bare_state()
                };

                let rendered = StatusLine::new(segments).render(&Theme::default(), &state, width);

                assert_eq!(text_of(&rendered), expected, "{status} at {width}");
            }
        }

        #[test]
        fn plan_and_yolo_stay_visible_when_the_mode_is_the_highest_configured_segment() {
            let theme = Theme::default();

            for (permission_mode, style) in
                [(Mode::Plan, theme.accent()), (Mode::Yolo, theme.error())]
            {
                let state = StatusLineState {
                    permission_mode,
                    ..bare_state()
                };

                let rendered = StatusLine::new(vec![
                    StatusLineSegment::Model,
                    StatusLineSegment::PermissionMode,
                ])
                .render(&theme, &state, 7);

                assert_eq!(text_of(&rendered), format!("  {permission_mode}"));
                assert_eq!(rendered.line.spans[1].style, style, "{permission_mode}");
            }
        }

        #[test]
        fn drops_low_utility_segments_before_usage_model_and_state() {
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
        fn drops_plain_model_before_model_with_effort() {
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
    }

    mod fit_segments {
        use super::*;

        #[test]
        fn equal_utility_keeps_the_later_configured_segment() {
            let mut segments = vec![
                RenderedSegment::new(7, Span::raw("first"), None),
                RenderedSegment::new(7, Span::raw("second"), None),
            ];

            fit_segments(&mut segments, 8, 3);

            let labels: Vec<&str> = segments
                .iter()
                .map(|segment| segment.span.content.as_ref())
                .collect();
            assert_eq!(labels, ["second"]);
        }
    }

    mod segment_utility {
        use super::*;

        #[test]
        fn follows_the_documented_utility_order() {
            let utility_order = [
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
                StatusLineSegment::PermissionMode,
                StatusLineSegment::RunState,
            ];

            for (earlier, later) in utility_order.iter().zip(&utility_order[1..]) {
                assert!(
                    segment_utility(*earlier, Mode::Plan) < segment_utility(*later, Mode::Plan),
                    "{earlier:?} must have lower utility than {later:?}",
                );
            }
        }
    }

    mod context_label {
        use super::*;

        #[test]
        fn hides_context_a_compaction_cleared() {
            let label = context_label(UsageSnapshot {
                context_tokens: None,
                context_window: Some(200_000),
                estimated_cost_usd: Some(1.0),
            });

            assert!(label.is_none(), "{label:?}");
        }
    }

    mod format_cost {
        use super::*;

        #[test]
        fn shows_cents_and_more_digits_only_under_a_cent() {
            for (cost, expected) in [
                (0.0042, "$0.0042"),
                (0.0099, "$0.0099"),
                (0.009_95, "$0.01"),
                (0.4321, "$0.43"),
                (1.234, "$1.23"),
            ] {
                assert_eq!(format_cost(cost), expected, "{cost}");
            }
        }
    }
}
