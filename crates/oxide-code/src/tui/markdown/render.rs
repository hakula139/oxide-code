use pulldown_cmark::{CodeBlockKind, CowStr, Event, HeadingLevel, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tracing::{debug, warn};

use super::highlight::{CODE_FG, highlight_code};

// ── Style Constants ──

const HEADING_H1: Style = Style::new()
    .fg(Color::White)
    .add_modifier(Modifier::BOLD)
    .add_modifier(Modifier::UNDERLINED);
const HEADING_H2: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const HEADING_H3: Style = Style::new()
    .fg(Color::White)
    .add_modifier(Modifier::BOLD)
    .add_modifier(Modifier::ITALIC);
const HEADING_H456: Style = Style::new().fg(Color::White).add_modifier(Modifier::ITALIC);

const CODE_STYLE: Style = Style::new().fg(CODE_FG);
const LINK_STYLE: Style = Style::new()
    .fg(Color::Rgb(137, 180, 250)) // Catppuccin Blue
    .add_modifier(Modifier::UNDERLINED);
const BLOCKQUOTE_STYLE: Style = Style::new().fg(Color::Rgb(166, 227, 161)); // Catppuccin Green
const LIST_MARKER_STYLE: Style = Style::new().fg(Color::Rgb(137, 180, 250)); // Catppuccin Blue
const RULE_STYLE: Style = Style::new().fg(Color::Rgb(88, 91, 112)); // Catppuccin Overlay0

// ── Renderer ──

pub(super) struct MarkdownRenderer<I> {
    iter: I,
    pub(super) lines: Vec<Line<'static>>,

    /// Nested inline style stack (bold, italic, strikethrough).
    inline_styles: Vec<Style>,

    /// Ordered (`Some(index)`) or unordered (`None`) per nesting level.
    list_stack: Vec<Option<u64>>,

    /// Deferred list marker spans, emitted on the next `push_line`.
    pending_marker: Option<Vec<Span<'static>>>,

    /// Indent prefix per nesting level (list continuation or blockquote).
    indent_stack: Vec<Vec<Span<'static>>>,

    /// Insert a blank line before the next block element.
    needs_newline: bool,

    // Code block state
    in_code_block: bool,
    code_lang: Option<String>,
    code_buf: String,

    /// Stored link destination, appended at `End(Link)`.
    link_url: Option<String>,
}

impl<'a, I> MarkdownRenderer<I>
where
    I: Iterator<Item = Event<'a>>,
{
    pub(super) fn new(iter: I) -> Self {
        Self {
            iter,
            lines: Vec::new(),
            inline_styles: Vec::new(),
            list_stack: Vec::new(),
            pending_marker: None,
            indent_stack: Vec::new(),
            needs_newline: false,
            in_code_block: false,
            code_lang: None,
            code_buf: String::new(),
            link_url: None,
        }
    }

    pub(super) fn run(&mut self) {
        while let Some(event) = self.iter.next() {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(ref text) => self.text(text),
            Event::Code(code) => self.code(code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::Html(ref html) => self.html(html),
            Event::InlineHtml(ref html) => self.inline_html(html),
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {
                debug!(?event, "unsupported markdown event");
            }
        }
    }

    // ── Tag Dispatch ──

    fn start_tag(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.start_paragraph(),
            Tag::Heading { level, .. } => self.start_heading(level),
            Tag::BlockQuote(_) => self.start_blockquote(),
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::List(start) => self.start_list(start),
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.push_inline_style(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_inline_style(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_inline_style(Style::new().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
            }
            Tag::HtmlBlock
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Image { .. }
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Subscript
            | Tag::Superscript => {
                warn!(?tag, "unsupported markdown tag");
            }
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_blockquote(),
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => {
                self.indent_stack.pop();
                self.pending_marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_inline_style();
            }
            TagEnd::Link => self.pop_link(),
            _ => {}
        }
    }

    // ── Block Elements ──

    fn start_paragraph(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::default());
        self.needs_newline = false;
    }

    fn end_paragraph(&mut self) {
        self.needs_newline = true;
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        if self.needs_newline {
            self.push_blank_line();
        }
        let style = match level {
            HeadingLevel::H1 => HEADING_H1,
            HeadingLevel::H2 => HEADING_H2,
            HeadingLevel::H3 => HEADING_H3,
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => HEADING_H456,
        };
        let prefix = format!("{} ", "#".repeat(level as usize));
        self.push_line(Line::from(vec![Span::styled(prefix, style)]));
        self.push_inline_style(style);
        self.needs_newline = false;
    }

    fn end_heading(&mut self) {
        self.pop_inline_style();
        self.needs_newline = true;
    }

    fn start_blockquote(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
        self.indent_stack
            .push(vec![Span::styled("> ", BLOCKQUOTE_STYLE)]);
    }

    fn end_blockquote(&mut self) {
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    fn rule(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::styled("───", RULE_STYLE));
        self.needs_newline = true;
    }

    // ── Lists ──

    fn start_list(&mut self, start_index: Option<u64>) {
        if self.list_stack.is_empty() && self.needs_newline {
            self.push_blank_line();
        }
        self.list_stack.push(start_index);
    }

    fn end_list(&mut self) {
        self.list_stack.pop();
        self.needs_newline = true;
    }

    fn start_item(&mut self) {
        let depth = self.list_stack.len();
        let indent_width = depth * 4 - 3;

        let marker = if let Some(last) = self.list_stack.last_mut() {
            match last {
                None => " ".repeat(indent_width.saturating_sub(1)) + "- ",
                Some(index) => {
                    let m = format!("{:indent_width$}. ", *index);
                    *index += 1;
                    m
                }
            }
        } else {
            "- ".to_owned()
        };

        let continuation = vec![Span::raw(" ".repeat(marker.len()))];
        self.pending_marker = Some(vec![Span::styled(marker, LIST_MARKER_STYLE)]);
        self.indent_stack.push(continuation);
        self.needs_newline = false;
    }

    // ── Code Blocks ──

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        if !self.lines.is_empty() {
            self.push_blank_line();
        }
        self.in_code_block = true;
        self.code_lang = match kind {
            CodeBlockKind::Fenced(lang) => {
                let l = lang.to_string();
                if l.is_empty() { None } else { Some(l) }
            }
            CodeBlockKind::Indented => None,
        };
        self.code_buf.clear();
    }

    fn end_code_block(&mut self) {
        self.in_code_block = false;
        let code = std::mem::take(&mut self.code_buf);
        let lang = self.code_lang.take();

        let highlighted = highlight_code(lang.as_deref().unwrap_or(""), &code);
        for line in highlighted {
            self.lines.push(line);
        }
        self.needs_newline = true;
    }

    // ── Inline Content ──

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }

        if self.pending_marker.is_some() {
            self.push_line(Line::default());
        }

        let style = self.current_inline_style();
        for (i, line) in text.lines().enumerate() {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if i > 0 {
                self.push_line(Line::default());
            }
            self.push_span(Span::styled(line.to_owned(), style));
        }
        self.needs_newline = false;
    }

    fn code(&mut self, code: CowStr<'a>) {
        if self.pending_marker.is_some() {
            self.push_line(Line::default());
        }
        self.push_span(Span::styled(code.into_string(), CODE_STYLE));
    }

    fn html(&mut self, html: &str) {
        let style = self.current_inline_style();
        for (i, line) in html.lines().enumerate() {
            if i > 0 || self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            self.push_span(Span::styled(line.to_owned(), style));
        }
    }

    fn inline_html(&mut self, html: &str) {
        self.push_span(Span::raw(html.to_owned()));
    }

    fn soft_break(&mut self) {
        self.push_span(Span::raw(" "));
    }

    fn hard_break(&mut self) {
        self.push_line(Line::default());
    }

    // ── Links ──

    fn pop_link(&mut self) {
        if let Some(url) = self.link_url.take() {
            self.push_span(Span::raw(" ("));
            self.push_span(Span::styled(url, LINK_STYLE));
            self.push_span(Span::raw(")"));
        }
    }

    // ── Inline Style Stack ──

    fn push_inline_style(&mut self, style: Style) {
        let merged = self
            .inline_styles
            .last()
            .copied()
            .unwrap_or_default()
            .patch(style);
        self.inline_styles.push(merged);
    }

    fn pop_inline_style(&mut self) {
        self.inline_styles.pop();
    }

    fn current_inline_style(&self) -> Style {
        self.inline_styles.last().copied().unwrap_or_default()
    }

    // ── Line Building ──

    /// Start a new output line with indent prefixes and any pending list marker.
    fn push_line(&mut self, line: Line<'static>) {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // Emit indent prefixes. If a pending marker exists, use it for the
        // deepest list level instead of the continuation indent.
        let marker = self.pending_marker.take();
        let marker_depth = if marker.is_some() {
            self.indent_stack.len().saturating_sub(1)
        } else {
            usize::MAX
        };

        for (i, prefix) in self.indent_stack.iter().enumerate() {
            if i == marker_depth
                && let Some(ref m) = marker
            {
                spans.extend(m.iter().cloned());
                continue;
            }
            spans.extend(prefix.iter().cloned());
        }

        spans.extend(line.spans);
        self.lines.push(Line::from(spans));
    }

    /// Append a span to the last line, creating one if needed.
    fn push_span(&mut self, span: Span<'static>) {
        if let Some(line) = self.lines.last_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    fn push_blank_line(&mut self) {
        self.lines.push(Line::default());
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    use super::super::render_markdown;
    use super::*;

    fn rendered_text(input: &str) -> Vec<String> {
        render_markdown(input)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    // ── render_markdown ──

    #[test]
    fn plain_text() {
        let lines = rendered_text("Hello, world!");
        assert_eq!(lines, vec!["Hello, world!"]);
    }

    // ── Paragraphs ──

    #[test]
    fn paragraph_separation() {
        let lines = rendered_text(indoc! {"
            First paragraph

            Second paragraph
        "});
        let first_pos = lines.iter().position(|l| l.contains("First")).unwrap();
        let blank_pos = lines.iter().position(String::is_empty).unwrap();
        let second_pos = lines.iter().position(|l| l.contains("Second")).unwrap();
        assert!(
            first_pos < blank_pos && blank_pos < second_pos,
            "expected First < blank < Second, got {first_pos} < {blank_pos} < {second_pos}"
        );
    }

    #[test]
    fn paragraph_soft_break_joins() {
        let lines = rendered_text(indoc! {"
            Hello
            World
        "});
        assert_eq!(lines, vec!["Hello World"]);
    }

    // ── Headings ──

    #[test]
    fn heading_levels() {
        let lines = rendered_text(indoc! {"
            # H1
            ## H2
            ### H3
        "});
        assert!(
            lines.iter().any(|l| l.starts_with("# H1")),
            "H1 prefix: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("## H2")),
            "H2 prefix: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("### H3")),
            "H3 prefix: {lines:?}"
        );
    }

    // ── Inline Styles ──

    #[test]
    fn bold_and_italic() {
        let text = render_markdown("**bold** and *italic*");
        let bold_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .unwrap();
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        let italic_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("italic"))
            .unwrap();
        assert!(italic_span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn bold_italic_combined() {
        let text = render_markdown("***bold italic***");
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold italic"))
            .unwrap();
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn strikethrough() {
        let text = render_markdown("~~struck~~");
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("struck"))
            .unwrap();
        assert!(span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn inline_code() {
        let text = render_markdown("Use `foo()` here");
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("foo()"))
            .unwrap();
        assert_eq!(span.style.fg, Some(CODE_FG));
    }

    // ── Links ──

    #[test]
    fn link_appends_url() {
        let lines = rendered_text("[Click](https://example.com)");
        assert_eq!(lines, vec!["Click (https://example.com)"]);
    }

    #[test]
    fn link_url_has_underline_style() {
        let text = render_markdown("[text](https://example.com)");
        let url_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("https://example.com"))
            .unwrap();
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    // ── Code Blocks ──

    #[test]
    fn fenced_code_block_plain() {
        let lines = rendered_text(indoc! {"
            ```
            fn main() {}
            ```
        "});
        assert!(lines.iter().any(|l| l.contains("fn main()")));
    }

    #[test]
    fn fenced_code_block_with_lang_highlights() {
        let text = render_markdown(indoc! {"
            ```rust
            fn main() {}
            ```
        "});
        let has_rgb = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        });
        assert!(has_rgb, "syntax highlighting should produce RGB colors");
    }

    // ── Ordered Lists ──

    #[test]
    fn tight_ordered_list() {
        let lines = rendered_text(indoc! {"
            1. First
            2. Second
            3. Third
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("1.") && l.contains("First"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2.") && l.contains("Second"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("3.") && l.contains("Third"))
        );
    }

    #[test]
    fn loose_ordered_list() {
        let lines = rendered_text(indoc! {"
            1. First

            2. Second

            3. Third
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("1.") && l.contains("First")),
            "loose list marker and content should be on the same line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2.") && l.contains("Second")),
            "second item: {lines:?}"
        );
    }

    #[test]
    fn ordered_list_double_digit_alignment() {
        let mut input = String::new();
        for i in 1..=12 {
            use std::fmt::Write;
            writeln!(input, "{i}. Item {i}").unwrap();
        }
        let lines = rendered_text(&input);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("10.") && l.contains("Item 10")),
            "marker and content on same line for item 10: {lines:?}"
        );
    }

    // ── Unordered Lists ──

    #[test]
    fn tight_unordered_list() {
        let lines = rendered_text(indoc! {"
            - Alpha
            - Beta
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("- ") && l.contains("Alpha"))
        );
        assert!(lines.iter().any(|l| l.contains("- ") && l.contains("Beta")));
    }

    #[test]
    fn loose_unordered_list() {
        let lines = rendered_text(indoc! {"
            - Alpha

            - Beta
        "});
        assert!(
            lines.iter().any(|l| l.contains('-') && l.contains("Alpha")),
            "loose unordered: {lines:?}"
        );
    }

    // ── Nested Lists ──

    #[test]
    fn nested_list() {
        let lines = rendered_text(indoc! {"
            - Outer
              - Inner
        "});
        assert!(lines.iter().any(|l| l.contains("Outer")));
        assert!(lines.iter().any(|l| l.contains("Inner")));
        let inner = lines.iter().find(|l| l.contains("Inner")).unwrap();
        let outer = lines.iter().find(|l| l.contains("Outer")).unwrap();
        assert!(
            inner.len() > outer.len(),
            "inner should have more indent than outer"
        );
    }

    // ── Inline Elements in Lists ──

    #[test]
    fn inline_code_in_list_item() {
        let text = render_markdown("- Use `foo()` here");
        let has_code_span = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("foo()") && s.style.fg == Some(CODE_FG))
        });
        assert!(has_code_span, "inline code should be styled in list items");
    }

    #[test]
    fn link_in_list_item() {
        let lines = rendered_text("- [Click](https://example.com)");
        assert!(lines.iter().any(|l| l.contains("Click")));
        assert!(lines.iter().any(|l| l.contains("https://example.com")));
    }

    // ── Blockquotes ──

    #[test]
    fn blockquote() {
        let lines = rendered_text("> Quoted text");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("> ") && l.contains("Quoted"))
        );
    }

    #[test]
    fn nested_blockquote() {
        let lines = rendered_text(indoc! {"
            > Outer
            > > Inner
        "});
        assert!(lines.iter().any(|l| l.contains("Outer")));
        let inner = lines.iter().find(|l| l.contains("Inner")).unwrap();
        assert!(
            inner.matches("> ").count() >= 2,
            "inner blockquote should have nested > markers: {inner:?}"
        );
    }

    // ── Horizontal Rule ──

    #[test]
    fn horizontal_rule() {
        let lines = rendered_text(indoc! {"
            Above

            ---

            Below
        "});
        assert!(lines.iter().any(|l| l.contains('─')));
    }

    // ── HTML ──

    #[test]
    fn html_block_rendered_as_text() {
        let lines = rendered_text("<div>hello</div>\n");
        assert!(lines.iter().any(|l| l.contains("<div>hello</div>")));
    }

    #[test]
    fn inline_html_preserved() {
        let lines = rendered_text("text <br> more");
        let joined: String = lines.join("");
        assert!(joined.contains("<br>"));
    }
}
