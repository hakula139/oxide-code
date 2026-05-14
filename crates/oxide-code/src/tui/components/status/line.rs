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
            spans.push(segment.span);
            first = false;
        }
        Line::from(spans)
    }

    fn render_segment(
        segment: StatusLineSegment,
        theme: &Theme,
        state: &StatusLineState<'_>,
    ) -> Option<RenderedSegment> {
        let span = match segment {
            StatusLineSegment::CurrentDir => non_empty_span(
                truncate_to_width(state.cwd, MAX_CURRENT_DIR_WIDTH),
                Self::segment_style(theme, SegmentStyle::Dim),
            ),
            StatusLineSegment::GitBranch => state.git_branch.map(|branch| {
                Span::styled(
                    truncate_to_width(branch, MAX_GIT_BRANCH_WIDTH),
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
        Some(RenderedSegment::new(segment, span))
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
    /// Display label for the active model.
    pub(super) model: &'a str,
    /// Resolved effort tier for model-with-effort.
    pub(super) effort: Option<Effort>,
    /// Optional session title.
    pub(super) title: Option<&'a str>,
    /// Latest usage snapshot from the agent loop.
    pub(super) usage: Option<UsageSnapshot>,
    /// Tildified working directory.
    pub(super) cwd: &'a str,
    /// Branch captured at TUI startup.
    pub(super) git_branch: Option<&'a str>,
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
}

impl RenderedSegment {
    fn new(segment: StatusLineSegment, span: Span<'static>) -> Self {
        Self { segment, span }
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

fn segment_utility(segment: StatusLineSegment) -> u8 {
    match segment {
        StatusLineSegment::ThreadTitle => 0,
        StatusLineSegment::CurrentTime => 1,
        StatusLineSegment::GitBranch => 2,
        StatusLineSegment::CurrentDir => 3,
        StatusLineSegment::SessionCost => 4,
        StatusLineSegment::ContextUsed => 5,
        StatusLineSegment::Model | StatusLineSegment::ModelWithEffort => 6,
        StatusLineSegment::RunState => 7,
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
    if cost >= 0.995 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
