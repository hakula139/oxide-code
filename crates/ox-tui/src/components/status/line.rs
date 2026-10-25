//! Ordered segment rendering for the status line.

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
        Some(RenderedSegment::new(segment, span, hyperlink))
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
        segments.remove(lowest_priority_index(segments));
    }

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

/// Index of the segment to drop first. `segments` must not be empty.
fn lowest_priority_index(segments: &[RenderedSegment]) -> usize {
    let (index, _) = segments
        .iter()
        .enumerate()
        .min_by_key(|(_, segment)| segment_utility(segment.segment))
        .expect("fit_segments drops only while two or more segments remain");
    index
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
        StatusLineSegment::PermissionMode => 17,
        StatusLineSegment::RunState => 18,
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
    match mode {
        Mode::Manual => ("Mode: manual", SegmentStyle::Muted),
        Mode::Plan => ("Mode: plan", SegmentStyle::Accent),
        Mode::Yolo => ("Mode: yolo", SegmentStyle::Error),
    }
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
                    current_minute: Some(9 * 60 + 5),
                    ..bare_state()
                },
                width,
            );
            text_of(&rendered)
        }

        fn pr_state(number: u64) -> PullRequest {
            PullRequest {
                number,
                url: format!("https://github.com/o/r/pull/{number}"),
                checks: None,
                review: None,
            }
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
        fn pull_request_metadata_hides_without_checks_or_review() {
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
        fn permission_mode_flags_plan_and_yolo_and_hides_manual() {
            let rendered = |permission_mode| {
                render(
                    vec![
                        StatusLineSegment::PermissionMode,
                        StatusLineSegment::RunState,
                    ],
                    &StatusLineState {
                        permission_mode,
                        ..bare_state()
                    },
                )
            };
            let theme = Theme::default();

            let plan = rendered(Mode::Plan);
            assert_eq!(text_of(&plan), "  Mode: plan │ Ready");
            assert_eq!(plan.line.spans[1].style, theme.accent());

            let yolo = rendered(Mode::Yolo);
            assert_eq!(text_of(&yolo), "  Mode: yolo │ Ready");
            assert_eq!(yolo.line.spans[1].style, theme.error());

            assert_eq!(text_of(&rendered(Mode::Manual)), "  Ready");
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
        fn truncates_single_oversized_segment_to_width() {
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

    mod segment_utility {
        use super::*;

        /// The drop order the configuration guide documents.
        #[test]
        fn follows_the_documented_drop_order() {
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
                StatusLineSegment::PermissionMode,
                StatusLineSegment::RunState,
            ];
            for (earlier, later) in drop_order.iter().zip(&drop_order[1..]) {
                assert!(
                    segment_utility(*earlier) < segment_utility(*later),
                    "{earlier:?} must drop before {later:?}",
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
