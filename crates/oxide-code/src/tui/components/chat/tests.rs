use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use indoc::indoc;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::message::{ContentBlock, Role};

// ── Fixtures ──

fn test_chat() -> ChatView {
    ChatView::new(Theme::default(), true)
}

fn test_tools() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(crate::tool::bash::BashTool),
        Box::new(crate::tool::read::ReadTool),
        Box::new(crate::tool::write::WriteTool),
        Box::new(crate::tool::edit::EditTool),
        Box::new(crate::tool::glob::GlobTool),
        Box::new(crate::tool::grep::GrepTool),
    ])
}

/// Render `build_text` at default width and join all span content
/// into a single string for substring assertions.
fn all_text(chat: &ChatView) -> String {
    chat.build_text(80)
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_count(chat: &ChatView) -> usize {
    chat.build_text(80).lines.len()
}

fn key_event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl_key_event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

fn mouse_scroll(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}

fn render_chat(chat: &mut ChatView, width: u16, height: u16) -> TestBackend {
    chat.update_layout(Rect::new(0, 0, width, height));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            chat.render(frame, frame.area());
        })
        .unwrap();
    terminal.backend().clone()
}

// ── load_history ──

#[test]
fn load_history_populates_user_and_assistant_entries() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message::user("hello"), Message::assistant("hi there")],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 2);
    let text = all_text(&chat);
    assert!(text.contains("hello"));
    assert!(text.contains("hi there"));
}

#[test]
fn load_history_multi_tool_turn_pairs_inline_with_orphan_fallback() {
    // Live rendering pairs Call → Result inline. The resumed walk
    // must preserve that order regardless of JSONL batching; an
    // orphan result ("ghost", no matching call) surfaces at its
    // original position with the "(result)" fallback label.
    let mut chat = test_chat();
    chat.load_history(
        &[
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "read".to_owned(),
                        input: serde_json::json!({"file_path": "a.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".to_owned(),
                        name: "grep".to_owned(),
                        input: serde_json::json!({"pattern": "TODO"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "file a".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "ghost".to_owned(),
                        content: "stale output".to_owned(),
                        is_error: true,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "t2".to_owned(),
                        content: "3 matches".to_owned(),
                        is_error: false,
                    },
                ],
            },
        ],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 5);
    let text = all_text(&chat);
    // Order: call(a.rs), result(a.rs)=file a, call(TODO), result(TODO)=3 matches, orphan=stale
    let a_call = text.find("a.rs").unwrap();
    let file_a = text.find("file a").unwrap();
    let todo_call = text.find("TODO").unwrap();
    let matches = text.find("3 matches").unwrap();
    let stale = text.find("stale output").unwrap();
    let result_label = text.find("(result)").unwrap();
    assert!(a_call < file_a);
    assert!(file_a < todo_call);
    assert!(todo_call < matches);
    assert!(matches < stale);
    assert!(result_label < stale);
}

#[test]
fn load_history_renders_tool_result_after_paired_tool_use() {
    let mut chat = test_chat();
    chat.load_history(
        &[
            Message::user("ask"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "output".to_owned(),
                    is_error: false,
                }],
            },
            Message::assistant("reply"),
        ],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 4);
    let text = all_text(&chat);
    assert!(
        text.find("ask")
            < text
                .find("ls")
                .and_then(|i| text[i..].find("output").map(|j| i + j))
    );
    assert!(text.contains("ask"));
    assert!(text.contains("ls"));
    assert!(text.contains("output"));
    assert!(text.contains("reply"));
}

#[test]
fn load_history_tool_result_without_matching_tool_use_uses_fallback_label() {
    // Orphan tool_result — possible after crash sanitization. Render
    // with a generic fallback rather than dropping.
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "missing".to_owned(),
                content: "stderr".to_owned(),
                is_error: true,
            }],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains("(result)"));
    assert!(text.contains("stderr"));
    assert!(text.contains('✗'));
}

#[test]
fn load_history_joins_multiple_text_blocks() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "first".to_owned(),
                },
                ContentBlock::Text {
                    text: "second".to_owned(),
                },
            ],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains("first"));
    assert!(text.contains("second"));
}

#[test]
fn load_history_skips_whitespace_only_text() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "  \n  ".to_owned(),
            }],
        }],
        &test_tools(),
    );
    assert!(chat.blocks.is_empty());
}

#[test]
fn load_history_empty_slice_is_noop() {
    let mut chat = test_chat();
    chat.load_history(&[], &test_tools());
    assert!(chat.blocks.is_empty());
}

#[test]
fn load_history_restores_tool_call_after_assistant_text() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check that.".to_owned(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls -la"}),
                },
            ],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 2);
    let text = all_text(&chat);
    assert!(text.find("Let me check that.") < text.find("ls -la"));
    assert!(text.contains('$')); // bash icon
}

#[test]
fn load_history_unknown_tool_falls_back_to_tool_name_as_label() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "custom_tool".to_owned(),
                input: serde_json::json!({"arg": "value"}),
            }],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains('⟡'));
    assert!(text.contains("custom_tool"));
}

#[test]
fn load_history_server_tool_use_renders_like_local_tool_call() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ServerToolUse {
                id: "srv1".to_owned(),
                name: "web_search".to_owned(),
                input: serde_json::json!({"query": "rust"}),
            }],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains('⟡'));
    assert!(text.contains("web_search"));
}

#[test]
fn load_history_redacted_thinking_is_dropped() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::RedactedThinking {
                    data: "opaque-ciphertext".to_owned(),
                },
                ContentBlock::Text {
                    text: "fine".to_owned(),
                },
            ],
        }],
        &test_tools(),
    );
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains("fine"));
    assert!(!text.contains("opaque-ciphertext"));
}

// ── append_stream_token / commit_streaming ──

#[test]
fn append_stream_token_clears_thinking() {
    let mut chat = test_chat();
    chat.append_thinking_token("thinking...");
    assert!(!chat.thinking_buffer.is_empty());

    chat.append_stream_token("text");
    assert!(chat.thinking_buffer.is_empty());
}

#[test]
fn commit_streaming_moves_buffer_to_block() {
    let mut chat = test_chat();
    chat.append_stream_token("hello world");
    assert!(chat.blocks.is_empty());

    chat.commit_streaming();
    assert_eq!(chat.blocks.len(), 1);
    let text = all_text(&chat);
    assert!(text.contains("hello world"));
    assert!(chat.streaming.is_none());
}

#[test]
fn commit_streaming_empty_buffer_no_block() {
    let mut chat = test_chat();
    chat.commit_streaming();
    assert!(chat.blocks.is_empty());
}

#[test]
fn commit_streaming_clears_state() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("line1\nline2\npartial");
    assert!(chat.streaming.is_some());

    chat.commit_streaming();
    assert!(chat.streaming.is_none());
    assert!(chat.thinking_buffer.is_empty());
}

// ── update_layout ──

#[test]
fn update_layout_sets_viewport_height() {
    let mut chat = test_chat();
    chat.update_layout(Rect::new(0, 0, 80, 30));
    assert_eq!(chat.viewport_height, 30);
}

#[test]
fn update_layout_auto_scrolls_when_enabled() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.auto_scroll = true;

    chat.update_layout(Rect::new(0, 0, 80, 20));
    assert_eq!(chat.scroll_offset, 80);
}

#[test]
fn update_layout_invalidates_streaming_cache_on_width_change() {
    let mut chat = test_chat();
    chat.update_layout(Rect::new(0, 0, 80, 24));
    chat.append_stream_token("a complete line\n");
    let s = chat.streaming.as_ref().unwrap();
    assert_ne!(s.rendered_len(), 0);
    assert_eq!(s.cached_width(), 80);

    chat.update_layout(Rect::new(0, 0, 40, 24));
    let s = chat.streaming.as_ref().unwrap();
    assert_eq!(s.rendered_len(), 0);
    assert_eq!(s.rendered_boundary(), 0);
    assert_eq!(s.cached_width(), 0);
}

// ── handle_event ──

#[test]
fn handle_event_arrow_up_scrolls_up() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 10;

    let action = chat.handle_event(&key_event(KeyCode::Up));
    assert!(action.is_none());
    assert_eq!(chat.scroll_offset, 9);
    assert!(!chat.auto_scroll);
}

#[test]
fn handle_event_arrow_down_scrolls_down() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 10;
    chat.auto_scroll = false;

    let action = chat.handle_event(&key_event(KeyCode::Down));
    assert!(action.is_none());
    assert_eq!(chat.scroll_offset, 11);
}

#[test]
fn handle_event_mouse_scroll_up() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 10;

    let action = chat.handle_event(&mouse_scroll(MouseEventKind::ScrollUp));
    assert!(action.is_none());
    assert_eq!(chat.scroll_offset, 9);
}

#[test]
fn handle_event_mouse_scroll_down() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 10;
    chat.auto_scroll = false;

    let action = chat.handle_event(&mouse_scroll(MouseEventKind::ScrollDown));
    assert!(action.is_none());
    assert_eq!(chat.scroll_offset, 11);
}

#[test]
fn handle_event_page_up() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 30;

    chat.handle_event(&key_event(KeyCode::PageUp));
    assert_eq!(chat.scroll_offset, 12);
}

#[test]
fn handle_event_page_down() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 30;
    chat.auto_scroll = false;

    chat.handle_event(&key_event(KeyCode::PageDown));
    assert_eq!(chat.scroll_offset, 48);
}

#[test]
fn handle_event_ctrl_home_scrolls_to_top() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 50;

    chat.handle_event(&ctrl_key_event(KeyCode::Home));
    assert_eq!(chat.scroll_offset, 0);
    assert!(!chat.auto_scroll);
}

#[test]
fn handle_event_ctrl_end_scrolls_to_bottom() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 10;
    chat.auto_scroll = false;

    chat.handle_event(&ctrl_key_event(KeyCode::End));
    assert_eq!(chat.scroll_offset, 80);
    assert!(chat.auto_scroll);
}

#[test]
fn handle_event_unhandled_key_returns_none() {
    let mut chat = test_chat();
    let action = chat.handle_event(&key_event(KeyCode::Char('a')));
    assert!(action.is_none());
}

// ── Scroll helpers ──

#[test]
fn scroll_to_bottom_sets_offset_correctly() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;

    chat.scroll_to_bottom();
    assert_eq!(chat.scroll_offset, 80);
}

#[test]
fn scroll_to_bottom_zero_when_content_fits() {
    let mut chat = test_chat();
    chat.content_height.set(10);
    chat.viewport_height = 20;

    chat.scroll_to_bottom();
    assert_eq!(chat.scroll_offset, 0);
}

#[test]
fn scroll_up_decreases_offset_and_disables_auto_scroll() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 50;
    chat.auto_scroll = true;

    chat.scroll_up(5);
    assert_eq!(chat.scroll_offset, 45);
    assert!(!chat.auto_scroll);
}

#[test]
fn scroll_up_saturates_at_zero() {
    let mut chat = test_chat();
    chat.scroll_offset = 3;

    chat.scroll_up(10);
    assert_eq!(chat.scroll_offset, 0);
}

#[test]
fn scroll_down_increases_offset() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 50;
    chat.auto_scroll = false;

    chat.scroll_down(5);
    assert_eq!(chat.scroll_offset, 55);
    assert!(!chat.auto_scroll);
}

#[test]
fn scroll_down_clamps_to_max_and_enables_auto_scroll() {
    let mut chat = test_chat();
    chat.content_height.set(100);
    chat.viewport_height = 20;
    chat.scroll_offset = 75;

    chat.scroll_down(10);
    assert_eq!(chat.scroll_offset, 80);
    assert!(chat.auto_scroll);
}

// ── build_text behavior ──

#[test]
fn build_text_empty_shows_welcome() {
    let chat = test_chat();
    let text = all_text(&chat);
    assert!(text.contains("Welcome to ox"));
    assert!(text.contains("Ask anything to begin."));
}

#[test]
fn build_text_full_conversation() {
    let mut chat = test_chat();
    chat.push_user_message("What is 2+2?".to_owned());
    chat.blocks
        .push(Box::new(AssistantText::new("The answer is 4.")));
    chat.push_tool_call("$", "python -c 'print(2+2)'");
    chat.push_tool_result("4", "4", false);
    chat.push_user_message("Thanks!".to_owned());
    chat.append_stream_token("You're welcome");

    let text = all_text(&chat);
    assert!(text.contains("What is 2+2?"));
    assert!(text.contains("The answer is 4."));
    assert!(text.contains("python -c 'print(2+2)'"));
    assert!(text.contains("You're welcome"));
    // Two user messages → two user-icon prefixes.
    assert_eq!(text.matches('❯').count(), 2);
}

// ── welcome ──

#[test]
fn welcome_centered_for_width() {
    let chat = test_chat();

    let narrow = chat.build_text(30);
    let wide = chat.build_text(120);

    let narrow_pad = narrow.lines[2].spans.first().map_or(0, |s| s.content.len());
    let wide_pad = wide.lines[2].spans.first().map_or(0, |s| s.content.len());
    assert!(wide_pad > narrow_pad);
}

// ── User messages ──

#[test]
fn user_message_has_icon_and_content() {
    let mut chat = test_chat();
    chat.push_user_message("hello world".to_owned());
    let text = all_text(&chat);
    assert!(text.contains('❯'));
    assert!(text.contains("hello world"));
}

#[test]
fn user_message_has_trailing_blank_before_tool_call() {
    let mut chat = test_chat();
    chat.push_user_message("hello".to_owned());
    chat.push_tool_call("$", "ls");
    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let user = lines.iter().rposition(|l| l.contains("hello")).unwrap();
    let tool = lines.iter().position(|l| l.contains("ls")).unwrap();
    assert!(
        (user + 1..tool).any(|i| lines[i].trim().is_empty()),
        "expected blank line after user message"
    );
}

#[test]
fn user_followed_by_assistant_has_no_double_blank() {
    let mut chat = test_chat();
    chat.push_user_message("hello".to_owned());
    chat.blocks.push(Box::new(AssistantText::new("reply")));
    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let max_consecutive_blanks = lines
        .windows(2)
        .filter(|w| w[0].trim().is_empty() && w[1].trim().is_empty())
        .count();
    assert_eq!(
        max_consecutive_blanks, 0,
        "no double blank lines between user and assistant: {lines:?}"
    );
}

#[test]
fn user_message_enables_auto_scroll() {
    let mut chat = test_chat();
    chat.auto_scroll = false;
    chat.push_user_message("hello".to_owned());
    assert!(chat.auto_scroll);
}

#[test]
fn user_message_multiline_renders_every_line() {
    let mut chat = test_chat();
    chat.push_user_message(
        indoc! {"
            line1
            line2
            line3
        "}
        .to_owned(),
    );
    let text = all_text(&chat);
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
    assert!(text.contains("line3"));
}

// ── Assistant text ──

#[test]
fn assistant_text_has_icon_and_content() {
    let mut chat = test_chat();
    chat.blocks.push(Box::new(AssistantText::new("response")));
    let text = all_text(&chat);
    assert!(text.contains('⟡'));
    assert!(text.contains("response"));
}

// ── Tool calls ──

#[test]
fn tool_call_shows_icon_and_label() {
    let mut chat = test_chat();
    chat.push_tool_call("$", "ls -la");
    let text = all_text(&chat);
    assert!(text.contains('$'));
    assert!(text.contains("ls -la"));
}

#[test]
fn tool_call_after_assistant_has_blank_separator() {
    let mut chat = test_chat();
    chat.blocks.push(Box::new(AssistantText::new("some text")));
    chat.push_tool_call("$", "ls");
    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let assistant = lines.iter().rposition(|l| l.contains("some text")).unwrap();
    let tool = lines.iter().position(|l| l.contains("ls")).unwrap();
    assert!(
        (assistant + 1..tool).any(|i| lines[i].trim().is_empty()),
        "expected blank separator between assistant text and tool call"
    );
}

#[test]
fn consecutive_tool_calls_have_no_gap() {
    let mut chat = test_chat();
    chat.push_tool_call("$", "ls");
    chat.push_tool_call("$", "cat foo");
    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let ls_line = lines.iter().position(|l| l.contains("ls")).unwrap();
    let cat_line = lines.iter().position(|l| l.contains("cat foo")).unwrap();
    assert_eq!(
        cat_line,
        ls_line + 1,
        "consecutive tool calls should have no blank gap"
    );
}

#[test]
fn tool_call_wraps_long_label() {
    let mut chat = test_chat();
    let long_cmd =
        "cd /home/user/projects/example-app && ls ${XDG_DATA_HOME:-$HOME/.local/share}/ox";
    chat.push_tool_call("$", long_cmd);
    let text = chat.build_text(60);
    assert!(
        text.lines.len() > 1,
        "long tool call label should wrap across multiple lines: {}",
        text.lines.len(),
    );
    for line in &text.lines {
        let width: usize = line.spans.iter().map(|s| s.content.as_ref().len()).sum();
        assert!(
            width <= 60,
            "wrapped tool call line must fit the width budget (got {width}): {line:?}",
        );
    }
}

// ── Tool results ──

#[test]
fn tool_result_success() {
    let mut chat = test_chat();
    chat.push_tool_result("done", "output text", false);
    let text = all_text(&chat);
    assert!(text.contains("✓"));
    assert!(text.contains("done"));
    assert!(text.contains("output text"));
}

#[test]
fn tool_result_error() {
    let mut chat = test_chat();
    chat.push_tool_result("failed", "error details", true);
    let text = all_text(&chat);
    assert!(text.contains("✗"));
    assert!(text.contains("failed"));
    assert!(text.contains("error details"));
}

#[test]
fn tool_result_wraps_long_label() {
    let mut chat = test_chat();
    let long_label = "some-very-long-file-path-that-exceeds.the.width.budget/and/then/more/path";
    chat.push_tool_result(long_label, "", false);
    let text = chat.build_text(50);
    assert!(
        text.lines.len() > 1,
        "long tool result label should wrap: {}",
        text.lines.len(),
    );
    for line in &text.lines {
        let width: usize = line.spans.iter().map(|s| s.content.as_ref().len()).sum();
        assert!(
            width <= 50,
            "wrapped tool result line must fit width (got {width}): {line:?}",
        );
    }
}

#[test]
fn tool_result_truncation() {
    let mut chat = test_chat();
    let long_output = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>();
    chat.push_tool_result("result", &long_output.join("\n"), false);
    let text = all_text(&chat);

    assert!(text.contains("line 0"));
    assert!(text.contains("line 4"));
    assert!(!text.contains("line 5"));
    assert!(text.contains("... 5 more lines"));
}

#[test]
fn tool_result_empty_content_adds_nothing() {
    let mut chat = test_chat();
    chat.push_tool_result("result", "  \n  ", false);
    let before = line_count(&chat);

    let mut chat2 = test_chat();
    chat2.push_tool_result("result", "", false);
    let after = line_count(&chat2);

    assert_eq!(before, after);
}

#[test]
fn tool_result_exactly_max_no_truncation() {
    const MAX: usize = 5; // matches MAX_TOOL_OUTPUT_LINES in tool.rs
    let mut chat = test_chat();
    let output: Vec<_> = (0..MAX).map(|i| format!("line {i}")).collect();
    chat.push_tool_result("result", &output.join("\n"), false);
    let text = all_text(&chat);
    assert!(!text.contains("more lines"));
}

#[test]
fn tool_result_one_over_max_shows_singular_line() {
    const MAX: usize = 5;
    let mut chat = test_chat();
    let output: Vec<_> = (0..=MAX).map(|i| format!("line {i}")).collect();
    chat.push_tool_result("result", &output.join("\n"), false);
    let text = all_text(&chat);
    assert!(text.contains("... 1 more line"));
    assert!(!text.contains("lines"), "singular 'line' expected: {text}");
}

#[test]
fn tool_result_long_line_is_truncated() {
    const MAX_CHARS: usize = 512;
    let mut chat = test_chat();
    let long_line = "x".repeat(MAX_CHARS + 100);
    chat.push_tool_result("result", &long_line, false);
    let text = all_text(&chat);
    assert!(text.contains("..."), "long line should be truncated");
    assert!(
        !text.contains(&long_line),
        "full long line should not appear"
    );
}

// ── Error ──

#[test]
fn error_block_shows_error_indicator() {
    let mut chat = test_chat();
    chat.push_error("something broke");
    let text = all_text(&chat);
    assert!(text.contains("✗"));
    assert!(text.contains("something broke"));
}

// ── last_is_error ──

#[test]
fn last_is_error_true_after_push_error() {
    let mut chat = test_chat();
    chat.push_error("boom");
    assert!(chat.last_is_error());
}

#[test]
fn last_is_error_false_for_non_error_blocks() {
    // Exercises the `ChatBlock::is_error_marker` default impl on every
    // non-error variant. A failed tool result also renders a ✗ but
    // `is_error_marker` stays `false` — the predicate is about block
    // identity, not rendered glyphs.
    let mut chat = test_chat();
    chat.push_user_message("hello".into());
    assert!(!chat.last_is_error());

    chat.push_tool_call("$", "ls");
    assert!(!chat.last_is_error());

    chat.push_tool_result("failed", "boom", true);
    assert!(
        !chat.last_is_error(),
        "failed tool result is not an error marker"
    );
}

#[test]
fn last_is_error_false_when_no_blocks() {
    let chat = test_chat();
    assert!(!chat.last_is_error());
}

// ── Thinking ──

#[test]
fn live_thinking_visible_when_enabled() {
    let mut chat = test_chat();
    chat.append_thinking_token("pondering...");
    let text = all_text(&chat);
    assert!(text.contains("Thinking..."));
    assert!(text.contains("pondering..."));
}

#[test]
fn live_thinking_hidden_when_disabled() {
    let mut chat = ChatView::new(Theme::default(), false);
    chat.append_thinking_token("pondering...");
    let text = all_text(&chat);
    assert!(!text.contains("Thinking..."));
    assert!(!text.contains("pondering..."));
}

#[test]
fn live_thinking_after_user_has_separator() {
    let mut chat = test_chat();
    chat.push_user_message("hello".to_owned());
    chat.append_thinking_token("deep thought");
    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let last_user = lines.iter().rposition(|l| l.contains("hello")).unwrap();
    let thinking = lines.iter().position(|l| l.contains("Thinking")).unwrap();
    assert!(
        (last_user + 1..thinking).any(|i| lines[i].trim().is_empty()),
        "expected blank separator between user message and thinking block"
    );
}

#[test]
fn resumed_thinking_renders_when_show_thinking_enabled() {
    let mut chat = test_chat();
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "resumed reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "reply".to_owned(),
                },
            ],
        }],
        &test_tools(),
    );
    let text = all_text(&chat);
    assert!(text.contains("Thinking..."));
    assert!(text.contains("resumed reasoning"));
}

#[test]
fn resumed_thinking_hidden_when_show_thinking_disabled() {
    let mut chat = ChatView::new(Theme::default(), false);
    chat.load_history(
        &[Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "private reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "reply".to_owned(),
                },
            ],
        }],
        &test_tools(),
    );
    let text = all_text(&chat);
    assert!(!text.contains("Thinking..."));
    assert!(!text.contains("private reasoning"));
    assert!(text.contains("reply"));
}

// ── Streaming ──

#[test]
fn streaming_shows_partial_text() {
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.append_stream_token("partial response");
    let text = all_text(&chat);
    assert!(text.contains('⟡'), "should show assistant icon");
    assert!(text.contains("partial response"));
}

#[test]
fn streaming_cached_and_tail_both_visible() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("cached line\n");
    chat.append_stream_token("tail text");

    let text = all_text(&chat);
    assert!(text.contains("cached line"));
    assert!(text.contains("tail text"));
}

#[test]
fn streaming_uncommitted_newlines_all_render() {
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.viewport_width = 80;
    chat.append_stream_token("line1\nline2\npartial");

    let text = all_text(&chat);
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
    assert!(text.contains("partial"));
}

#[test]
fn streaming_without_prior_assistant_shows_icon() {
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.append_stream_token("response");

    let text = all_text(&chat);
    assert!(text.contains('⟡'), "new turn should show assistant icon");
}

#[test]
fn streaming_after_committed_assistant_omits_duplicate_icon() {
    let mut chat = test_chat();
    chat.blocks.push(Box::new(AssistantText::new("committed")));
    // Push streaming directly — simulates a continued turn.
    let mut s = StreamingAssistant::new();
    s.append("streaming");
    chat.streaming = Some(s);

    let text = all_text(&chat);
    let count = text.matches('⟡').count();
    assert_eq!(count, 1, "icon should appear once, not duplicated");
}

#[test]
fn streaming_inserts_blank_separator_after_tool_output() {
    // When streaming tokens arrive after a non-standalone block (tool
    // call / tool result / error — no trailing blank of its own), the
    // streaming block must insert its own leading blank so the icon
    // doesn't sit flush against the preceding line.
    let mut chat = test_chat();
    chat.push_tool_call("$", "ls");
    chat.append_stream_token("response");

    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let tool_pos = lines.iter().position(|l| l.contains("ls")).unwrap();
    let stream_pos = lines.iter().position(|l| l.contains("response")).unwrap();
    assert!(
        (tool_pos + 1..stream_pos).any(|i| lines[i].trim().is_empty()),
        "expected blank separator between tool call and streaming: {lines:?}"
    );
}

#[test]
fn streaming_renders_committed_and_trailing_before_cache_advance() {
    // With viewport_width = 0, advance_cache no-ops, so the streaming
    // buffer accumulates newlines that rfind('\n') inside render_into
    // then splits on first paint. Covers the Some(nl) match arm plus
    // the `!committed.is_empty()` branch that an advance-cache-first
    // flow skips.
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.append_stream_token("cached line\ntail text");
    // Pre-check the invariant that makes this test meaningful: cache
    // deferred because viewport wasn't measured.
    assert_eq!(
        chat.streaming.as_ref().unwrap().rendered_boundary(),
        0,
        "advance_cache must defer when viewport_width is 0"
    );

    let text = all_text(&chat);
    assert!(text.contains("cached line"));
    assert!(text.contains("tail text"));
}

#[test]
fn streaming_renders_buffer_ending_in_newline_before_cache_advance() {
    // Trailing newline with viewport_width = 0: `advance_cache` defers,
    // so `render_into` sees a tail that ends in `\n`. The rfind split
    // gives committed = "line1\nline2" and trailing = "" — this is the
    // fall-through where `if !trailing.is_empty()` is false.
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.append_stream_token("line1\nline2\n");
    assert_eq!(
        chat.streaming.as_ref().unwrap().rendered_boundary(),
        0,
        "advance_cache must defer when viewport_width is 0"
    );

    let text = all_text(&chat);
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
}

#[test]
fn live_thinking_after_tool_call_has_separator() {
    // Live thinking pushes a leading blank when the tail block has no
    // trailing blank of its own. Tool call is the natural example —
    // standalone = false, no trail blank, so the thinking header needs
    // its own separator.
    let mut chat = test_chat();
    chat.push_tool_call("$", "ls");
    chat.append_thinking_token("deep thought");

    let text = all_text(&chat);
    let lines: Vec<&str> = text.lines().collect();
    let tool_pos = lines.iter().position(|l| l.contains("ls")).unwrap();
    let thinking_pos = lines.iter().position(|l| l.contains("Thinking")).unwrap();
    assert!(
        (tool_pos + 1..thinking_pos).any(|i| lines[i].trim().is_empty()),
        "expected blank separator between tool call and thinking: {lines:?}"
    );
}

#[test]
fn streaming_trailing_newline_with_empty_tail() {
    let mut chat = test_chat();
    chat.push_user_message("hi".to_owned());
    chat.viewport_width = 80;
    chat.append_stream_token("line1\nline2\n");

    let text = all_text(&chat);
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
}

#[test]
fn streaming_advance_cache_no_newline_keeps_boundary_zero() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("no newline here");
    let s = chat.streaming.as_ref().unwrap();
    assert_eq!(s.rendered_boundary(), 0);
    assert_eq!(s.rendered_len(), 0);
}

#[test]
fn streaming_advance_cache_single_newline() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("first line\nincomplete");
    let s = chat.streaming.as_ref().unwrap();
    assert_eq!(s.rendered_boundary(), "first line\n".len());
    assert_eq!(s.rendered_len(), 1);
}

#[test]
fn streaming_advance_cache_multiple_newlines() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("line1\nline2\nline3\npartial");
    let s = chat.streaming.as_ref().unwrap();
    assert_eq!(s.rendered_boundary(), "line1\nline2\nline3\n".len());
}

#[test]
fn streaming_advance_cache_incremental() {
    let mut chat = test_chat();
    chat.viewport_width = 80;

    chat.append_stream_token("first\n");
    {
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 6);
        assert_eq!(s.rendered_len(), 1);
    }

    chat.append_stream_token("second\n");
    {
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 13);
        assert_eq!(s.rendered_len(), 2);
    }
}

#[test]
fn streaming_advance_cache_trailing_newline_only() {
    let mut chat = test_chat();
    chat.viewport_width = 80;
    chat.append_stream_token("\n");
    let s = chat.streaming.as_ref().unwrap();
    assert_eq!(s.rendered_boundary(), 1);
}

#[test]
fn streaming_advance_cache_defers_until_viewport_measured() {
    // Streaming before update_layout runs must not bake unwrapped
    // markdown into the cache. The cache stays empty until the
    // viewport width is supplied.
    let mut chat = test_chat();
    chat.append_stream_token("first complete line\n");
    {
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_len(), 0);
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.cached_width(), 0);
    }

    chat.update_layout(Rect::new(0, 0, 80, 24));
    chat.append_stream_token("second complete line\n");
    {
        let s = chat.streaming.as_ref().unwrap();
        assert_ne!(s.rendered_len(), 0);
        assert_eq!(s.cached_width(), 80);
    }
}

// ── render ──

#[test]
fn render_updates_content_height() {
    let mut chat = test_chat();
    render_chat(&mut chat, 80, 24);
    // Welcome screen: 2 blank lines + title + subtitle = 4 lines.
    assert_eq!(chat.content_height.get(), 4);
}

#[test]
fn render_empty_shows_welcome_screen() {
    let mut chat = test_chat();
    insta::assert_snapshot!(render_chat(&mut chat, 60, 8));
}

#[test]
fn render_user_and_assistant_interleaved() {
    let mut chat = test_chat();
    chat.push_user_message("what is 2 + 2?".into());
    chat.append_stream_token("The answer is 4.");
    chat.commit_streaming();
    insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
}

#[test]
fn render_tool_call_followed_by_result() {
    let mut chat = test_chat();
    chat.push_tool_call("$", "echo hi");
    chat.push_tool_result("ran echo", "hi", false);
    insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
}

#[test]
fn render_tool_result_overflow_shows_line_count() {
    let mut chat = test_chat();
    chat.push_tool_call("$", "ls");
    let long = (0..5 + 3)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    chat.push_tool_result("ls out", &long, false);
    insta::assert_snapshot!(render_chat(&mut chat, 60, 14));
}

#[test]
fn render_error_entry_is_styled_distinctly() {
    let mut chat = test_chat();
    chat.push_error("API error (HTTP 503): overloaded");
    insta::assert_snapshot!(render_chat(&mut chat, 60, 4));
}

#[test]
fn render_history_with_resumed_thinking_block() {
    let mut chat = ChatView::new(Theme::default(), true);
    let tools = test_tools();
    let history = vec![
        Message::user("hello"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "pondering...".into(),
                    signature: "sig".into(),
                },
                ContentBlock::Text { text: "Hi!".into() },
            ],
        },
    ];
    chat.load_history(&history, &tools);
    insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
}
