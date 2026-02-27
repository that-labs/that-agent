use std::io::{self, Stdout};
use std::path::{Path, PathBuf};

use crate::agent_loop::hook::{HookAction, LoopHook};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::{mpsc, oneshot};
use tui_textarea::TextArea;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Style for code block content (subtle dark background + lighter text).
const CODE_BG: Color = Color::Rgb(30, 30, 46);
const CODE_FG: Color = Color::Rgb(180, 190, 210);

/// Convert agent text (which may contain fenced code blocks) into styled Lines.
///
/// The first line gets the "Agent: " prefix; subsequent lines are indented to align.
/// Lines inside ``` fences are styled with a code background.
fn format_agent_text(text: &str) -> Vec<Line<'static>> {
    let prefix_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let code_style = Style::default().fg(CODE_FG).bg(CODE_BG);
    let fence_style = Style::default().fg(Color::DarkGray);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;

    for (i, raw_line) in text.lines().enumerate() {
        let trimmed = raw_line.trim();

        // Detect fenced code block toggles
        if trimmed.starts_with("```") {
            let opening = !in_code;
            in_code = !in_code;
            let label = if opening {
                let lang = trimmed.trim_start_matches('`').trim();
                if lang.is_empty() {
                    " ── code ──".to_string()
                } else {
                    format!(" ── {lang} ──")
                }
            } else {
                " ──────".to_string()
            };
            if i == 0 {
                out.push(Line::from(vec![
                    Span::styled("Agent: ", prefix_style),
                    Span::styled(label, fence_style),
                ]));
            } else {
                out.push(Line::from(Span::styled(
                    format!("       {label}"),
                    fence_style,
                )));
            }
            continue;
        }

        if i == 0 {
            if in_code {
                out.push(Line::from(vec![
                    Span::styled("Agent: ", prefix_style),
                    Span::styled(raw_line.to_string(), code_style),
                ]));
            } else {
                out.push(Line::from(vec![
                    Span::styled("Agent: ", prefix_style),
                    Span::raw(raw_line.to_string()),
                ]));
            }
        } else if in_code {
            out.push(Line::from(Span::styled(
                format!("       {raw_line}"),
                code_style,
            )));
        } else {
            out.push(Line::from(Span::raw(format!("       {raw_line}"))));
        }
    }

    out
}

/// Strip ANSI escape sequences and control characters that could confuse rendering.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC + any CSI sequence (ESC [ ... final_byte)
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                              // consume until we hit a letter (the final byte of a CSI sequence)
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() || c2 == '~' {
                        break;
                    }
                }
            }
            // Also skip ESC + single char (e.g. ESC ( B)
            continue;
        }
        // Allow newlines, tabs, and printable chars; drop other control chars
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Truncate a string to `max` bytes, appending "..." if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a char boundary at or before `max`
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

/// Extract the skill name from `read_skill` tool args JSON.
fn extract_skill_name(args_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| v["name"].as_str().map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------
// Spinner — animated "thinking" indicator
// ---------------------------------------------------------------------------

/// Each frame is 3 lines of ASCII art. The cycle: form → pulse → break → scatter → reform.
const SPINNER_FRAMES: &[&[&str]] = &[
    // Void — particles gathering
    &[
        "          ·          ",
        "         · ·         ",
        "          ·          ",
    ],
    // Forming
    &[
        "         ·•·         ",
        "        · ◯ ·        ",
        "         ·•·         ",
    ],
    // Coalescing
    &[
        "        ╭─•─╮        ",
        "        │ ◉ │        ",
        "        ╰─•─╯        ",
    ],
    // Solid
    &[
        "        ╭─●─╮        ",
        "        │ ◉ │        ",
        "        ╰─●─╯        ",
    ],
    // Pulse out
    &[
        "        ╭━◉━╮        ",
        "        ◉ ◉ ◉        ",
        "        ╰━◉━╯        ",
    ],
    // Pulse in
    &[
        "        ╭─●─╮        ",
        "        │ ◉ │        ",
        "        ╰─●─╯        ",
    ],
    // Cracking
    &[
        "        ╭ ● ╮        ",
        "          ◉          ",
        "        ╰ ● ╯        ",
    ],
    // Breaking
    &[
        "       · ◎  ·        ",
        "      ·      ·       ",
        "       · ◎  ·        ",
    ],
    // Fragmenting
    &[
        "      ·  ○   ·       ",
        "     ·        ·      ",
        "      ·  ○   ·       ",
    ],
    // Scattering
    &[
        "     ·   ·    ·      ",
        "    ·    ·     ·     ",
        "     ·   ·    ·      ",
    ],
    // Scattered
    &[
        "    ·    ·     ·     ",
        "          ·          ",
        "    ·    ·     ·     ",
    ],
    // Regathering
    &[
        "       · · ·         ",
        "        · ·          ",
        "       · · ·         ",
    ],
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Accumulated token usage and cost tracking for a TUI session.
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub tool_calls: u64,
    pub turns_success: u64,
    pub turns_error: u64,
}

impl UsageStats {
    pub fn new() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            turns_success: 0,
            turns_error: 0,
        }
    }

    pub fn add_usage(&mut self, input: u64, output: u64, cached: u64, cache_write: u64) {
        self.input_tokens += input;
        self.output_tokens += output;
        self.cached_input_tokens += cached;
        self.cache_write_tokens += cache_write;
    }

    pub fn add_tool_call(&mut self) {
        self.tool_calls += 1;
    }

    pub fn record_success(&mut self) {
        self.turns_success += 1;
    }

    pub fn record_error(&mut self) {
        self.turns_error += 1;
    }

    /// Estimate session cost based on common model pricing (per 1M tokens).
    pub fn estimated_cost(&self, _provider: &str, model: &str) -> f64 {
        let (input_rate, output_rate) = match model {
            // Anthropic — Feb 2026
            m if m.starts_with("claude-opus-4") => (5.0, 25.0),
            m if m.starts_with("claude-sonnet-4") => (3.0, 15.0),
            m if m.starts_with("claude-haiku-4") => (1.0, 5.0),
            // OpenAI — Feb 2026
            m if m.starts_with("gpt-5.2") => (1.75, 14.0),
            m if m.starts_with("gpt-5.1") => (0.25, 2.0),
            m if m.starts_with("gpt-4o") => (2.50, 10.0),
            m if m.starts_with("gpt-4.1") => (2.0, 8.0),
            m if m.starts_with("o4-mini") => (1.10, 4.40),
            m if m.starts_with("o3") => (0.40, 1.60),
            _ => (3.0, 15.0),
        };
        let input_cost = (self.input_tokens as f64) * input_rate / 1_000_000.0;
        let output_cost = (self.output_tokens as f64) * output_rate / 1_000_000.0;
        input_cost + output_cost
    }
}

// ---------------------------------------------------------------------------
// Modal — reusable overlay component
// ---------------------------------------------------------------------------

pub const MODEL_OPTIONS: &[(&str, &str)] = &[
    ("anthropic", "claude-opus-4-6"),
    ("anthropic", "claude-sonnet-4-5-20250929"),
    ("anthropic", "claude-haiku-4-5-20251001"),
    ("openai", "gpt-5.2-codex"),
    ("openai", "gpt-5.1-codex-mini"),
    ("openai", "gpt-4o"),
    ("openai", "gpt-4.1"),
    ("openai", "o3"),
    ("openai", "o4-mini"),
];

// ---------------------------------------------------------------------------
// Command palette
// ---------------------------------------------------------------------------

/// A single entry in the command palette.
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub is_skill: bool,
}

/// Inline autocomplete popup when the user types `/`.
pub struct CommandPalette {
    commands: Vec<CommandEntry>,
    filtered: Vec<usize>,
    cursor: usize,
    /// Index of the first visible entry in the filtered list.
    view_offset: usize,
}

const COMMAND_PALETTE_MAX_ROWS: usize = 10;

impl CommandPalette {
    pub fn new(commands: Vec<CommandEntry>) -> Self {
        let filtered: Vec<usize> = (0..commands.len()).collect();
        Self {
            commands,
            filtered,
            cursor: 0,
            view_offset: 0,
        }
    }

    /// Re-filter entries by prefix match on name (case-insensitive).
    /// `text` is what the user typed after `/`.
    pub fn update_filter(&mut self, text: &str) {
        let lower = text.to_lowercase();
        self.filtered = self
            .commands
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let name_without_slash = e.name.strip_prefix('/').unwrap_or(&e.name);
                name_without_slash.to_lowercase().starts_with(&lower)
            })
            .map(|(i, _)| i)
            .collect();

        if self.filtered.is_empty() {
            self.cursor = 0;
            self.view_offset = 0;
            return;
        }

        // Keep cursor in range after filtering.
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len() - 1;
        }

        // Keep window bounds valid and ensure the selected row stays visible.
        let max_offset = self.filtered.len().saturating_sub(1);
        self.view_offset = self.view_offset.min(max_offset);
        if self.cursor < self.view_offset {
            self.view_offset = self.cursor;
        }
        if self.cursor >= self.view_offset + COMMAND_PALETTE_MAX_ROWS {
            self.view_offset = self.cursor + 1 - COMMAND_PALETTE_MAX_ROWS;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            // Scroll view up if cursor moved above the visible window
            if self.cursor < self.view_offset {
                self.view_offset = self.cursor;
            }
        }
    }

    pub fn move_down(&mut self) {
        if !self.filtered.is_empty() && self.cursor + 1 < self.filtered.len() {
            self.cursor += 1;
            // Scroll view down if cursor moved below the visible window
            if self.cursor >= self.view_offset + COMMAND_PALETTE_MAX_ROWS {
                self.view_offset = self.cursor + 1 - COMMAND_PALETTE_MAX_ROWS;
            }
        }
    }

    /// Get the currently selected entry (if any).
    pub fn selected(&self) -> Option<&CommandEntry> {
        self.filtered
            .get(self.cursor)
            .and_then(|&i| self.commands.get(i))
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// Render the palette above the input area.
    pub fn render(&self, frame: &mut Frame, input_area: Rect) {
        if self.filtered.is_empty() {
            return;
        }

        let max_rows = COMMAND_PALETTE_MAX_ROWS;
        let visible = (self.filtered.len().min(max_rows)) as u16;
        let width = input_area.width;
        let y = input_area.y.saturating_sub(visible);
        let rect = Rect::new(input_area.x, y, width, visible);

        // Be defensive: stale offsets can happen when the list shrinks sharply
        // after filtering, and must never panic at render time.
        let start = self.view_offset.min(self.filtered.len().saturating_sub(1));
        let end = (start + max_rows).min(self.filtered.len());
        let selected = self.cursor.min(self.filtered.len().saturating_sub(1));
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (vi, &ci) in self.filtered[start..end].iter().enumerate() {
            let entry = &self.commands[ci];
            let is_selected = start + vi == selected;

            let name_style = if is_selected {
                Style::default().fg(Color::White).bg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };

            let desc_style = if is_selected {
                Style::default().fg(Color::Gray).bg(Color::DarkGray)
            } else if entry.is_skill {
                Style::default().fg(Color::Rgb(80, 80, 80))
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let bg_style = if is_selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", entry.name), name_style),
                Span::styled(format!("  {}", entry.description), desc_style),
                // Fill rest with bg
                Span::styled("", bg_style),
            ]));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(Clear, rect);
        frame.render_widget(paragraph, rect);
    }
}

// ---------------------------------------------------------------------------
// Modal — reusable overlay component
// ---------------------------------------------------------------------------

/// Identifies the purpose of a modal so the main loop can dispatch selections.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    ModelSelect,
    SessionResume,
    Info,
    SkillsList,
    SkillView,
}

/// A single item in a modal overlay.
pub enum ModalItem {
    /// Section header (non-selectable, styled bold).
    Header(String),
    /// Informational text line (non-selectable).
    Text(String),
    /// Selectable option: label shown left, detail right, active marked with ✓.
    Option {
        label: String,
        detail: String,
        active: bool,
    },
    /// Blank separator line.
    Separator,
}

/// Result of a modal key press.
pub enum ModalAction {
    None,
    Close,
    Select { label: String, detail: String },
    Delete { detail: String },
}

/// A centered overlay with dynamic content.
pub struct Modal {
    title: String,
    pub kind: ModalKind,
    items: Vec<ModalItem>,
    selectable: bool,
    cursor: usize,
    footer: String,
    scroll_offset: u16,
    /// Inner height (rows inside borders) recorded during the last render pass.
    /// Used by handle_key to keep the cursor within the visible window.
    last_inner_height: u16,
}

impl Modal {
    pub fn new(title: String, kind: ModalKind, items: Vec<ModalItem>, selectable: bool) -> Self {
        let footer = match kind {
            ModalKind::SkillView => " ↑↓ scroll  D delete  Esc ".to_string(),
            _ if selectable => " ↑↓ Enter Esc ".to_string(),
            _ => " ↑↓ scroll  Esc ".to_string(),
        };

        // Find initial cursor: first active Option, or first Option if none active
        let mut cursor = 0;
        let mut first_option = None;
        for (i, item) in items.iter().enumerate() {
            if let ModalItem::Option { active, .. } = item {
                if first_option.is_none() {
                    first_option = Some(i);
                }
                if *active {
                    cursor = i;
                    break;
                }
            }
        }
        if cursor == 0 {
            if let Some(idx) = first_option {
                cursor = idx;
            }
        }

        Self {
            title,
            kind,
            items,
            selectable,
            cursor,
            footer,
            scroll_offset: 0,
            last_inner_height: 0,
        }
    }

    /// Adjust scroll_offset so the cursor line is within the visible window.
    fn ensure_cursor_visible(&mut self) {
        let h = self.last_inner_height;
        if h == 0 {
            return;
        }
        let cursor = self.cursor as u16;
        if cursor < self.scroll_offset {
            self.scroll_offset = cursor;
        } else if cursor >= self.scroll_offset + h {
            self.scroll_offset = cursor.saturating_sub(h - 1);
        }
    }

    /// Handle a key event within the modal.
    pub fn handle_key(&mut self, key: KeyEvent) -> ModalAction {
        match key.code {
            KeyCode::Esc => ModalAction::Close,
            // SkillView: scroll and delete
            KeyCode::Up if self.kind == ModalKind::SkillView => {
                self.scroll_offset = self.scroll_offset.saturating_sub(2);
                ModalAction::None
            }
            KeyCode::Down if self.kind == ModalKind::SkillView => {
                self.scroll_offset = self.scroll_offset.saturating_add(2);
                ModalAction::None
            }
            KeyCode::Char('d') | KeyCode::Char('D') if self.kind == ModalKind::SkillView => {
                // detail field holds skill name (stored in title: "Skill: <name>")
                // Extract skill name from title
                let name = self.title.strip_prefix("Skill: ").unwrap_or(&self.title);
                ModalAction::Delete {
                    detail: name.to_string(),
                }
            }
            // Non-selectable, non-SkillView modals (e.g. Info): manual scroll
            KeyCode::Up if !self.selectable => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                ModalAction::None
            }
            KeyCode::Down if !self.selectable => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                ModalAction::None
            }
            KeyCode::Up if self.selectable => {
                // Move cursor to previous Option item
                let mut pos = self.cursor;
                loop {
                    if pos == 0 {
                        break;
                    }
                    pos -= 1;
                    if matches!(self.items[pos], ModalItem::Option { .. }) {
                        self.cursor = pos;
                        break;
                    }
                }
                self.ensure_cursor_visible();
                ModalAction::None
            }
            KeyCode::Down if self.selectable => {
                // Move cursor to next Option item
                let mut pos = self.cursor;
                loop {
                    pos += 1;
                    if pos >= self.items.len() {
                        break;
                    }
                    if matches!(self.items[pos], ModalItem::Option { .. }) {
                        self.cursor = pos;
                        break;
                    }
                }
                self.ensure_cursor_visible();
                ModalAction::None
            }
            KeyCode::Enter if self.selectable => {
                if let Some(ModalItem::Option { label, detail, .. }) = self.items.get(self.cursor) {
                    ModalAction::Select {
                        label: label.clone(),
                        detail: detail.clone(),
                    }
                } else {
                    ModalAction::None
                }
            }
            _ => ModalAction::None,
        }
    }

    /// Render the modal as a centered overlay.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Compute content dimensions
        let content_width: u16 = self
            .items
            .iter()
            .map(|item| match item {
                ModalItem::Header(t) => t.len() as u16 + 2,
                ModalItem::Text(t) => t.len() as u16 + 2,
                ModalItem::Option { label, detail, .. } => (label.len() + detail.len() + 8) as u16, // "  label  (detail) ✓"
                ModalItem::Separator => 0,
            })
            .max()
            .unwrap_or(20)
            .max(self.title.len() as u16 + 4)
            .max(self.footer.len() as u16 + 4);

        let width = (content_width + 4).min(area.width.saturating_sub(4)); // +4 for borders+padding
        let height = (self.items.len() as u16 + 2).min(area.height.saturating_sub(4)); // +2 for borders

        // Record inner height for cursor-tracking in handle_key
        self.last_inner_height = height.saturating_sub(2);

        let rect = centered_rect(width, height, area);

        // Build styled lines
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let is_cursor = self.selectable && i == self.cursor;
            match item {
                ModalItem::Header(text) => {
                    lines.push(Line::from(Span::styled(
                        format!(" {text}"),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                ModalItem::Text(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
                ModalItem::Option {
                    label,
                    detail,
                    active,
                } => {
                    let marker = if *active { " ✓" } else { "  " };
                    let style = if is_cursor {
                        Style::default().fg(Color::White).bg(Color::DarkGray)
                    } else if *active {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {label}  ({detail}){marker}"),
                        style,
                    )));
                }
                ModalItem::Separator => {
                    lines.push(Line::from(""));
                }
            }
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title))
            .title_bottom(Line::from(self.footer.clone()).centered())
            .border_style(Style::default().fg(Color::Cyan));

        // Apply scroll offset for all modal kinds
        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0));

        frame.render_widget(Clear, rect);
        frame.render_widget(paragraph, rect);
    }
}

/// Return a centered rectangle of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

/// Events sent from the agent task to the TUI render loop.
pub enum TuiEvent {
    /// A text token from the assistant's streaming response.
    Token(String),
    /// A thinking/reasoning delta from the assistant.
    #[allow(dead_code)]
    ThinkingDelta(String),
    /// The agent is calling a tool.
    ToolCall {
        call_id: String,
        name: String,
        args: String,
    },
    /// A tool returned a result (shown in debug mode).
    ToolResult {
        call_id: String,
        name: String,
        result: String,
    },
    /// The agent turn completed successfully with final text and usage data.
    Done {
        text: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
    },
    /// The agent turn failed.
    Error(String),
    /// The agent is asking the human a question via the human_ask tool.
    HumanAsk {
        message: String,
        response_tx: oneshot::Sender<String>,
    },
    /// A transient network error occurred; the agent is retrying with exponential backoff.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
    },
    /// Onboarding LLM call completed — contains the generated Soul.md and Identity.md content.
    OnboardingDone {
        soul_md: String,
        identity_md: String,
    },
    /// Onboarding LLM call failed.
    OnboardingError(String),
    /// Session compaction completed successfully.
    /// Fields: (mem_compact result message, LLM-generated conversation summary).
    CompactDone { message: String, summary: String },
    /// Session compaction failed.
    CompactError(String),
}

/// Which mode the TUI input area is currently in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMode {
    /// Normal input — user is composing a message.
    Input,
    /// Agent is streaming — input is disabled.
    Streaming,
    /// Agent asked a question via human_ask — user is typing a response.
    HumanAsk,
    /// First-run onboarding — collecting the agent's identity description.
    Onboarding,
    /// Graceful shutdown in progress — waiting for session compaction before exit.
    ShuttingDown,
}

/// Which pane currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Chat,
    Input,
}

/// A single line/entry in the chat history pane.
enum ChatLine {
    User(String),
    Agent(String),
    Thinking(String),
    ToolCall(String),
    ToolResult(String),
    Error(String),
    System(String),
}

/// What the key handler wants the main loop to do.
pub enum KeyAction {
    Submit(String),
    SubmitHumanAsk(String),
    ModalSelect {
        kind: ModalKind,
        label: String,
        detail: String,
    },
    ModalDelete {
        kind: ModalKind,
        detail: String,
    },
    InterruptRun,
    Quit,
    None,
}

// ---------------------------------------------------------------------------
// TuiHook — StreamingPromptHook that forwards events to the TUI
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TuiHook {
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl TuiHook {
    pub fn new(tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl LoopHook for TuiHook {
    async fn on_text_delta(&self, delta: &str) {
        let _ = self.tx.send(TuiEvent::Token(delta.to_string()));
    }

    async fn on_reasoning_delta(&self, delta: &str) {
        let _ = self.tx.send(TuiEvent::ThinkingDelta(delta.to_string()));
    }

    async fn on_tool_call(&self, name: &str, call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let message = serde_json::from_str::<serde_json::Value>(args_json)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(String::from))
                .unwrap_or_else(|| "Agent is asking for input:".into());

            let (response_tx, response_rx) = oneshot::channel();
            let _ = self.tx.send(TuiEvent::HumanAsk {
                message,
                response_tx,
            });

            let result_json = match response_rx.await {
                Ok(response) => {
                    let approved = {
                        let lower = response.to_lowercase();
                        lower != "no" && lower != "n" && lower != "deny"
                    };
                    serde_json::json!({
                        "response": response,
                        "approved": approved,
                        "method": "tui_hook",
                        "elapsed_ms": 0
                    })
                    .to_string()
                }
                Err(_) => serde_json::json!({
                    "response": "User quit",
                    "approved": false,
                    "method": "tui_hook",
                    "elapsed_ms": 0
                })
                .to_string(),
            };
            HookAction::Skip { result_json }
        } else {
            let _ = self.tx.send(TuiEvent::ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                args: args_json.to_string(),
            });
            HookAction::Continue
        }
    }

    async fn on_tool_result(&self, name: &str, call_id: &str, result_json: &str) {
        let _ = self.tx.send(TuiEvent::ToolResult {
            call_id: call_id.to_string(),
            name: name.to_string(),
            result: result_json.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// ChatApp — TUI state and rendering
// ---------------------------------------------------------------------------

pub struct ChatApp<'a> {
    messages: Vec<ChatLine>,
    textarea: TextArea<'a>,
    scroll_offset: u16,
    mode: AppMode,
    focused_pane: Pane,
    agent_rx: mpsc::UnboundedReceiver<TuiEvent>,
    current_streaming: String,
    current_thinking: String,
    debug: bool,
    #[allow(dead_code)]
    sandbox: bool,
    human_ask_tx: Option<oneshot::Sender<String>>,
    human_ask_message: String,
    spinner_frame: usize,
    /// Total lines in the rendered messages (for scroll clamping).
    last_messages_height: u16,
    /// Visible area height for messages.
    last_viewport_height: u16,
    /// True when the user has manually scrolled (Shift+Up/Down). Prevents auto-scroll.
    user_scrolled: bool,
    /// Active modal overlay (if any).
    modal: Option<Modal>,
    /// Command palette (shown when typing `/`).
    command_palette: Option<CommandPalette>,
    /// Available commands for the palette.
    available_commands: Vec<CommandEntry>,
    /// Input history (oldest first).
    input_history: Vec<String>,
    /// Current position in input history (None = not browsing).
    history_cursor: Option<usize>,
    /// Saves in-progress text when entering history navigation.
    input_stash: String,
    /// Path to the input history file for persistence.
    history_file: Option<PathBuf>,
    /// Tool calls in the current agent turn (reset on set_streaming).
    turn_tool_calls: u32,
    /// Max turns budget (for detecting exhaustion).
    max_turns: usize,
    /// Consecutive tool calls in the current collapse block (non-debug).
    tool_collapse_count: u32,
    /// Name of the currently executing tool (shown in spinner area).
    current_tool_name: String,
    /// Timestamp of the last Ctrl+C press — double Ctrl+C within 500ms quits.
    last_ctrl_c: Option<std::time::Instant>,
    /// Whether the previous key handled in the input pane was a bare Down arrow.
    /// Two consecutive Down presses in single-line mode clear the input.
    last_key_was_down: bool,
    /// When true the TUI has released mouse capture so the terminal can handle
    /// native text selection. Re-pressing `s` or Esc restores capture.
    selection_mode: bool,
}

impl<'a> ChatApp<'a> {
    pub fn new(
        rx: mpsc::UnboundedReceiver<TuiEvent>,
        debug: bool,
        sandbox: bool,
        state_dir: Option<&Path>,
    ) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("Type or paste... (Enter send, Ctrl+J newline)");
        textarea.set_cursor_line_style(Style::default());

        let mode_label = if sandbox { "SANDBOX" } else { "local" };
        let mut messages = Vec::new();
        messages.push(ChatLine::System(format!(
            "that-agent interactive chat ({mode_label}). Enter send, Ctrl+J newline, Tab switch panes, Shift+Up/Down scroll, Esc quit."
        )));

        // Load input history from disk
        let history_file = state_dir.map(|d| d.join("input_history"));
        let input_history = history_file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|content| {
                content
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.replace("\\n", "\n").replace("\\\\", "\\"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Self {
            messages,
            textarea,
            scroll_offset: 0,
            mode: AppMode::Input,
            focused_pane: Pane::Input,
            agent_rx: rx,
            current_streaming: String::new(),
            current_thinking: String::new(),
            debug,
            sandbox,
            human_ask_tx: None,
            human_ask_message: String::new(),
            spinner_frame: 0,
            last_messages_height: 0,
            last_viewport_height: 0,
            user_scrolled: false,
            modal: None,
            command_palette: None,
            available_commands: Vec::new(),
            input_history,
            turn_tool_calls: 0,
            max_turns: 75,
            tool_collapse_count: 0,
            current_tool_name: String::new(),
            history_cursor: None,
            input_stash: String::new(),
            history_file,
            last_ctrl_c: None,
            last_key_was_down: false,
            selection_mode: false,
        }
    }

    /// Render the TUI frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(3),    // messages
            Constraint::Length(5), // input
        ])
        .split(frame.area());

        // --- Messages pane ---
        let pane_width = chunks[0].width.saturating_sub(2); // inside borders
        let lines = self.build_message_lines(pane_width);
        let viewport_h = chunks[0].height.saturating_sub(2); // inside borders

        // Use Ratatui's own word-wrap calculation to get the accurate visual line count.
        // The old manual ceiling-division approach underestimated word-wrapped heights,
        // causing the scroll offset to fall short and the bottom content to be clipped.
        let total_visual = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(pane_width) as u16;

        self.last_messages_height = total_visual;
        self.last_viewport_height = viewport_h;

        // Auto-scroll to bottom unless user has manually scrolled
        if !self.user_scrolled && total_visual > viewport_h {
            self.scroll_offset = total_visual.saturating_sub(viewport_h);
        }

        // Clamp scroll
        let max_scroll = total_visual.saturating_sub(viewport_h);
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }

        let chat_border = if self.selection_mode {
            Style::default().fg(Color::Yellow)
        } else if self.mode == AppMode::Onboarding {
            Style::default().fg(Color::Magenta)
        } else if self.focused_pane == Pane::Chat {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let chat_title = if self.selection_mode {
            " Chat — SELECTION  select with mouse · s or Esc to exit "
        } else if self.mode == AppMode::Onboarding {
            " Identity Setup "
        } else if self.focused_pane == Pane::Chat {
            " Chat (Tab \u{00b7} Shift+\u{2191}\u{2193} scroll \u{00b7} s select \u{00b7} y copy) "
        } else {
            " Chat "
        };

        let messages_widget = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(chat_title)
                    .title_bottom(
                        Line::from(Span::styled(
                            " Shift+↑↓ scroll ",
                            Style::default().fg(Color::DarkGray),
                        ))
                        .right_aligned(),
                    )
                    .border_style(chat_border),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0));

        frame.render_widget(messages_widget, chunks[0]);

        // --- Input pane ---
        let input_title = match self.mode {
            AppMode::ShuttingDown => " Compacting session… (Esc to skip) ",
            AppMode::Onboarding if self.focused_pane == Pane::Input => {
                " Describe your agent (Enter to generate identity) "
            }
            AppMode::Onboarding => " Describe your agent ",
            AppMode::Input | AppMode::HumanAsk if self.focused_pane == Pane::Input => {
                if self.mode == AppMode::HumanAsk {
                    " Agent asks — type response (Enter to send) "
                } else {
                    " Input (Enter to send) "
                }
            }
            AppMode::Streaming => " Agent is responding... (Ctrl+C or /stop interrupt) ",
            _ => " Input ",
        };

        let input_border = if self.focused_pane == Pane::Input {
            match self.mode {
                AppMode::Input => Style::default().fg(Color::White),
                AppMode::Streaming => Style::default().fg(Color::DarkGray),
                AppMode::HumanAsk => Style::default().fg(Color::Yellow),
                AppMode::Onboarding => Style::default().fg(Color::Magenta),
                AppMode::ShuttingDown => Style::default().fg(Color::DarkGray),
            }
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(input_title)
            .border_style(input_border);

        self.textarea.set_block(input_block);

        frame.render_widget(&self.textarea, chunks[1]);

        // --- Command palette (drawn above input, below modal) ---
        if let Some(palette) = &self.command_palette {
            palette.render(frame, chunks[1]);
        }

        // --- Modal overlay (drawn last, on top of everything) ---
        let full_area = frame.area();
        if let Some(modal) = &mut self.modal {
            modal.render(frame, full_area);
        }
    }

    /// Build styled lines for the messages pane.
    fn build_message_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        for msg in &self.messages {
            match msg {
                ChatLine::User(text) => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "You: ",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(text.clone()),
                    ]));
                }
                ChatLine::Agent(text) => {
                    lines.extend(format_agent_text(text));
                }
                ChatLine::Thinking(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("[thinking] {text}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                ChatLine::ToolCall(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("[tool] {text}"),
                        Style::default().fg(Color::Magenta),
                    )));
                }
                ChatLine::ToolResult(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("[result] {text}"),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                ChatLine::Error(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("[error] {text}"),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )));
                }
                ChatLine::System(text) => {
                    lines.push(Line::from(Span::styled(
                        text.clone(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            lines.push(Line::from(""));
        }

        // Show current streaming text if any
        if !self.current_streaming.is_empty() {
            lines.extend(format_agent_text(&self.current_streaming));
        }

        // Show current thinking text if any (truncate to keep layout sane)
        if !self.current_thinking.is_empty() {
            // Only show the last 200 chars to avoid a mega-line that breaks scroll
            let thinking = &self.current_thinking;
            let display = if thinking.len() > 200 {
                let start = thinking.floor_char_boundary(thinking.len() - 200);
                format!("[thinking] ...{}", &thinking[start..])
            } else {
                format!("[thinking] {}", thinking)
            };
            lines.push(Line::from(Span::styled(
                display,
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Show human_ask prompt if in that mode
        if self.mode == AppMode::HumanAsk && !self.human_ask_message.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("[human_ask] {}", self.human_ask_message),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        // Show animated spinner when agent is working (streaming, waiting for human_ask, or compacting)
        if self.mode == AppMode::Streaming
            || self.mode == AppMode::HumanAsk
            || self.mode == AppMode::ShuttingDown
        {
            lines.push(Line::from(""));
            // Show current tool activity above the spinner
            if !self.current_tool_name.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "       \u{2699} {} (turn {}/{})",
                        self.current_tool_name, self.turn_tool_calls, self.max_turns
                    ),
                    Style::default().fg(Color::Magenta),
                )));
            }
            let frame = &SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
            let dim = Style::default().fg(Color::DarkGray);
            for line in *frame {
                lines.push(Line::from(Span::styled(line.to_string(), dim)));
            }
            // Padding so auto-scroll overshoots and the spinner is fully visible
            lines.push(Line::from(""));
            lines.push(Line::from(""));
        }

        lines
    }

    /// Advance the spinner animation by one frame. Called on periodic tick.
    pub fn tick(&mut self) {
        if self.mode != AppMode::Input && self.modal.is_none() {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        }
    }

    /// Handle a key event, returning what action the main loop should take.
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // While in selection mode the terminal owns the mouse; we only handle
        // scroll keys and the exit bindings — everything else exits the mode.
        if self.selection_mode {
            match key.code {
                KeyCode::Char('s') | KeyCode::Esc => self.exit_selection_mode(),
                KeyCode::Up if shift => {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
                KeyCode::Down if shift => {
                    self.scroll_offset = self.scroll_offset.saturating_add(3);
                    let max_scroll = self
                        .last_messages_height
                        .saturating_sub(self.last_viewport_height);
                    if self.scroll_offset >= max_scroll {
                        self.user_scrolled = false;
                    }
                }
                _ => self.exit_selection_mode(),
            }
            return KeyAction::None;
        }

        // Reset the double-Down tracker unless this key IS a bare Down in input.
        // The actual check + conditional re-set happens inside the input branch below.
        let prev_last_key_was_down = self.last_key_was_down;
        self.last_key_was_down = false;

        // Ctrl+C: single = clear input, double (≤500 ms) = quit.
        if ctrl && key.code == KeyCode::Char('c') {
            let now = std::time::Instant::now();
            let is_double = self
                .last_ctrl_c
                .map(|t| now.duration_since(t) <= std::time::Duration::from_millis(500))
                .unwrap_or(false);
            if is_double {
                return KeyAction::Quit;
            }
            self.last_ctrl_c = Some(now);
            // During active runs, single Ctrl+C interrupts the current turn.
            if self.mode == AppMode::Streaming {
                return KeyAction::InterruptRun;
            }
            // Single Ctrl+C — clear the textarea and close any palette
            self.command_palette = None;
            self.history_cursor = None;
            self.input_stash.clear();
            self.set_textarea_content("");
            return KeyAction::None;
        }

        // Route keys to modal when one is open
        if let Some(modal) = &mut self.modal {
            match modal.handle_key(key) {
                ModalAction::Close => {
                    self.modal = None;
                    return KeyAction::None;
                }
                ModalAction::Select { label, detail } => {
                    let kind = modal.kind;
                    self.modal = None;
                    return KeyAction::ModalSelect {
                        kind,
                        label,
                        detail,
                    };
                }
                ModalAction::Delete { detail } => {
                    let kind = modal.kind;
                    self.modal = None;
                    return KeyAction::ModalDelete { kind, detail };
                }
                ModalAction::None => return KeyAction::None,
            }
        }

        // Global keys — always active regardless of pane/mode (no modal)
        // If palette is open, Esc closes palette instead of quitting
        match key.code {
            KeyCode::Esc => {
                if self.command_palette.is_some() {
                    self.command_palette = None;
                    return KeyAction::None;
                }
                return KeyAction::Quit;
            }
            _ => {}
        }

        // Shift+Up/Down always scrolls chat, regardless of focused pane or mode
        match key.code {
            KeyCode::Up if shift => {
                self.user_scrolled = true;
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                return KeyAction::None;
            }
            KeyCode::Down if shift => {
                self.user_scrolled = true;
                self.scroll_offset = self.scroll_offset.saturating_add(3);
                // If scrolled to bottom, re-enable auto-scroll
                let max_scroll = self
                    .last_messages_height
                    .saturating_sub(self.last_viewport_height);
                if self.scroll_offset >= max_scroll {
                    self.user_scrolled = false;
                }
                return KeyAction::None;
            }
            _ => {}
        }

        // Tab always switches panes (unless modal is open, handled above)
        if key.code == KeyCode::Tab {
            self.focused_pane = match self.focused_pane {
                Pane::Chat => Pane::Input,
                Pane::Input => Pane::Chat,
            };
            return KeyAction::None;
        }

        match self.mode {
            AppMode::Streaming => match self.focused_pane {
                Pane::Chat => KeyAction::None,
                Pane::Input => match key.code {
                    KeyCode::Enter => {
                        let text: String = self.textarea.lines().join("\n");
                        let text = text.trim().to_string();
                        if text == "/stop" {
                            self.command_palette = None;
                            self.set_textarea_content("");
                            return KeyAction::InterruptRun;
                        }
                        KeyAction::None
                    }
                    _ => {
                        // Allow composing text while streaming so `/stop` can be typed.
                        self.textarea.input(key);
                        KeyAction::None
                    }
                },
            },
            AppMode::ShuttingDown => {
                // During shutdown: no input, Tab already handled above
                KeyAction::None
            }
            AppMode::Input | AppMode::HumanAsk | AppMode::Onboarding => {
                match self.focused_pane {
                    Pane::Chat => {
                        if self.mode != AppMode::Onboarding {
                            match key.code {
                                KeyCode::Char('s') => self.enter_selection_mode(),
                                KeyCode::Char('y') => self.copy_last_turn(),
                                _ => {}
                            }
                        }
                        KeyAction::None
                    }
                    Pane::Input => {
                        let is_single_line = self.textarea.lines().len() <= 1;

                        // When the command palette is open, intercept navigation keys
                        if self.command_palette.is_some() {
                            match key.code {
                                KeyCode::Up => {
                                    if let Some(palette) = &mut self.command_palette {
                                        palette.move_up();
                                    }
                                    return KeyAction::None;
                                }
                                KeyCode::Down => {
                                    if let Some(palette) = &mut self.command_palette {
                                        palette.move_down();
                                    }
                                    return KeyAction::None;
                                }
                                KeyCode::Tab | KeyCode::Enter => {
                                    // Fill selected command into textarea + close palette
                                    if let Some(palette) = &self.command_palette {
                                        if let Some(entry) = palette.selected() {
                                            let cmd_text = format!("{} ", entry.name);
                                            self.set_textarea_content(&cmd_text);
                                        }
                                    }
                                    self.command_palette = None;
                                    return KeyAction::None;
                                }
                                _ => {
                                    // Let other keys fall through to normal handling
                                }
                            }
                        }

                        match key.code {
                            // Up arrow in single-line → navigate history (not during onboarding)
                            KeyCode::Up if is_single_line && self.mode != AppMode::Onboarding => {
                                self.navigate_history_up();
                            }
                            // Down arrow in single-line:
                            //   • browsing history → navigate forward
                            //   • second consecutive Down with nothing to navigate → clear input
                            //   (skipped during onboarding)
                            KeyCode::Down if is_single_line && self.mode != AppMode::Onboarding => {
                                if self.history_cursor.is_some() {
                                    self.navigate_history_down();
                                } else if prev_last_key_was_down {
                                    // Double Down — clear input
                                    self.command_palette = None;
                                    self.set_textarea_content("");
                                } else {
                                    // First Down at end of history — arm the clear
                                    self.last_key_was_down = true;
                                }
                            }
                            // Up/Down in multi-line → pass to TextArea for cursor movement
                            KeyCode::Up | KeyCode::Down => {
                                self.textarea.input(key);
                            }
                            // Ctrl+U → kill line (like readline)
                            KeyCode::Char('u') if ctrl => {
                                self.textarea.move_cursor(tui_textarea::CursorMove::Head);
                                self.textarea.delete_line_by_end();
                            }
                            // Ctrl+J → insert newline (reliable on all terminals)
                            // Shift+Enter → insert newline (terminals that support it)
                            KeyCode::Char('j') if ctrl => {
                                self.textarea.insert_newline();
                            }
                            KeyCode::Enter if shift => {
                                self.textarea.insert_newline();
                            }
                            // Plain Enter → submit
                            KeyCode::Enter => {
                                let text: String = self.textarea.lines().join("\n");
                                let text = text.trim().to_string();
                                if text.is_empty() {
                                    return KeyAction::None;
                                }
                                // Reset history browsing
                                self.history_cursor = None;
                                self.input_stash.clear();
                                // Close palette
                                self.command_palette = None;
                                // Clear textarea
                                self.textarea = TextArea::default();
                                self.textarea.set_placeholder_text(
                                    "Type or paste... (Enter send, Ctrl+J newline)",
                                );
                                self.textarea.set_cursor_line_style(Style::default());

                                if self.mode == AppMode::HumanAsk {
                                    return KeyAction::SubmitHumanAsk(text);
                                }
                                return KeyAction::Submit(text);
                            }
                            // Everything else → textarea
                            _ => {
                                self.textarea.input(key);
                            }
                        }

                        // After handling the key, check if we should open/update/close palette
                        // (not during onboarding — no commands available yet)
                        if self.mode != AppMode::Onboarding {
                            self.update_palette_state();
                        }

                        KeyAction::None
                    }
                }
            }
        }
    }

    /// Handle a paste event (multi-line paste arrives as a single string).
    pub fn handle_paste(&mut self, text: &str) {
        if self.mode == AppMode::Input
            || self.mode == AppMode::HumanAsk
            || self.mode == AppMode::Streaming
        {
            self.textarea.insert_str(text);
        }
    }

    /// Copy the latest turn (from the last User message to the end of history)
    /// to the system clipboard. In debug mode, tool call and result lines are
    /// included; otherwise only User and Agent lines are copied.
    fn copy_last_turn(&mut self) {
        let start = self
            .messages
            .iter()
            .rposition(|m| matches!(m, ChatLine::User(_)));

        let Some(idx) = start else {
            return;
        };

        let mut text = String::new();
        for line in &self.messages[idx..] {
            match line {
                ChatLine::User(t) => {
                    text.push_str("You: ");
                    text.push_str(t);
                    text.push('\n');
                }
                ChatLine::Agent(t) => {
                    text.push_str("Agent: ");
                    text.push_str(t);
                    text.push('\n');
                }
                ChatLine::ToolCall(t) if self.debug => {
                    text.push_str("[tool] ");
                    text.push_str(t);
                    text.push('\n');
                }
                ChatLine::ToolResult(t) if self.debug => {
                    text.push_str("[result] ");
                    text.push_str(t);
                    text.push('\n');
                }
                _ => {}
            }
        }

        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }

        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(_) => self
                .messages
                .push(ChatLine::System("Copied last turn to clipboard.".into())),
            Err(e) => self
                .messages
                .push(ChatLine::Error(format!("Clipboard error: {e}"))),
        }
    }

    /// Release mouse capture so the terminal can handle native text selection.
    fn enter_selection_mode(&mut self) {
        self.selection_mode = true;
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    }

    /// Restore mouse capture and return to normal TUI operation.
    fn exit_selection_mode(&mut self) {
        self.selection_mode = false;
        let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    }

    /// Handle a mouse event. Scroll wheel always scrolls the chat pane regardless
    /// of which pane is focused — the input pane is never scrolled by the mouse.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.user_scrolled = true;
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.user_scrolled = true;
                self.scroll_offset = self.scroll_offset.saturating_add(3);
                let max_scroll = self
                    .last_messages_height
                    .saturating_sub(self.last_viewport_height);
                if self.scroll_offset >= max_scroll {
                    self.user_scrolled = false;
                }
            }
            _ => {}
        }
    }

    /// Handle an event from the agent task.
    pub fn handle_agent_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::Token(text) => {
                self.current_streaming.push_str(&sanitize(&text));
            }
            TuiEvent::ThinkingDelta(text) => {
                self.current_thinking.push_str(&sanitize(&text));
            }
            TuiEvent::ToolCall { name, args, .. } => {
                self.turn_tool_calls += 1;
                self.current_tool_name = name.clone();

                // Flush any accumulated streaming text as a committed agent message
                if !self.current_streaming.is_empty() {
                    let text = std::mem::take(&mut self.current_streaming);
                    self.messages.push(ChatLine::Agent(text));
                    // Reset collapse — we had non-tool content in between
                    self.tool_collapse_count = 0;
                }

                // Flush any accumulated thinking
                if !self.current_thinking.is_empty() {
                    let thinking = std::mem::take(&mut self.current_thinking);
                    self.messages.push(ChatLine::Thinking(thinking));
                }

                // Detect read_skill calls — show distinctly and break the collapse run
                if name == "read_skill" {
                    let label = extract_skill_name(&args).unwrap_or_else(|| "?".to_string());
                    self.messages
                        .push(ChatLine::System(format!("\u{25ce} skill: {label}")));
                    self.tool_collapse_count = 0;
                }

                if self.debug {
                    let truncated_args = truncate_str(&sanitize(&args), 200);
                    self.messages
                        .push(ChatLine::ToolCall(format!("{name}({truncated_args})")));
                } else {
                    // Collapse consecutive tool calls into a single updating line
                    self.tool_collapse_count += 1;
                    if self.tool_collapse_count > 1 {
                        // Replace the previous collapse line
                        if let Some(ChatLine::ToolCall(_)) = self.messages.last() {
                            self.messages.pop();
                        }
                    }
                    self.messages.push(ChatLine::ToolCall(format!(
                        "{} tool calls \u{2014} {}",
                        self.tool_collapse_count, name,
                    )));
                }
            }
            TuiEvent::ToolResult { name, result, .. } => {
                if self.debug {
                    let truncated = truncate_str(&sanitize(&result), 300);
                    self.messages
                        .push(ChatLine::ToolResult(format!("{name} -> {truncated}")));
                }
            }
            TuiEvent::Done {
                text: final_text, ..
            } => {
                // Flush any remaining streaming buffer
                if !self.current_streaming.is_empty() {
                    let text = std::mem::take(&mut self.current_streaming);
                    self.messages.push(ChatLine::Agent(text));
                } else if !final_text.is_empty() {
                    self.messages.push(ChatLine::Agent(sanitize(&final_text)));
                }
                // Flush any remaining thinking
                if !self.current_thinking.is_empty() {
                    let thinking = std::mem::take(&mut self.current_thinking);
                    self.messages.push(ChatLine::Thinking(thinking));
                }
                // Check if ANY agent text was produced during this turn
                // (either from streaming or from the final response)
                let has_agent_text = self
                    .messages
                    .iter()
                    .rev()
                    .take_while(|m| !matches!(m, ChatLine::User(_)))
                    .any(|m| matches!(m, ChatLine::Agent(_)));

                if !has_agent_text && self.turn_tool_calls == 0 {
                    self.messages
                        .push(ChatLine::System("(agent finished without response)".into()));
                }
                // Add breathing room so the answer isn't flush against the bottom
                self.messages.push(ChatLine::System(String::new()));

                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
                self.current_tool_name.clear();
                self.tool_collapse_count = 0;
            }
            TuiEvent::Error(err) => {
                // Flush any partial streaming
                if !self.current_streaming.is_empty() {
                    let text = std::mem::take(&mut self.current_streaming);
                    self.messages.push(ChatLine::Agent(text));
                }
                self.current_thinking.clear();
                self.messages.push(ChatLine::Error(sanitize(&err)));
                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
                self.current_tool_name.clear();
                self.tool_collapse_count = 0;
            }
            TuiEvent::HumanAsk {
                message,
                response_tx,
            } => {
                self.human_ask_message = message;
                self.human_ask_tx = Some(response_tx);
                self.mode = AppMode::HumanAsk;
            }
            TuiEvent::Retrying {
                attempt,
                max_attempts,
                delay_secs,
            } => {
                // Clear any partial streaming state from the failed attempt so the
                // retry starts with a clean slate in the TUI.
                self.current_streaming.clear();
                self.current_thinking.clear();
                self.messages.push(ChatLine::System(format!(
                    "Network error — retrying ({attempt}/{max_attempts}) in {delay_secs}s…"
                )));
            }
            TuiEvent::OnboardingDone { .. } => {
                // State-only updates — saving to disk and rebuilding preamble happen in main.rs.
                self.current_streaming.clear();
                self.current_thinking.clear();
                self.current_tool_name.clear();
                self.tool_collapse_count = 0;
                self.messages.push(ChatLine::System(
                    "Identity captured. Edit Soul.md or Identity.md anytime to evolve who you are."
                        .into(),
                ));
                self.messages.push(ChatLine::System(String::new()));
                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
            }
            TuiEvent::OnboardingError(err) => {
                self.current_streaming.clear();
                self.current_thinking.clear();
                self.current_tool_name.clear();
                self.tool_collapse_count = 0;
                self.messages.push(ChatLine::Error(format!(
                    "Failed to generate identity: {err}"
                )));
                self.messages.push(ChatLine::System(
                    "Continuing with default templates. Create Soul.md and Identity.md manually to define your identity.".into(),
                ));
                self.messages.push(ChatLine::System(String::new()));
                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
            }
            TuiEvent::CompactDone { message, .. } => {
                let display = if message.trim().is_empty() {
                    "Session compacted.".to_string()
                } else {
                    format!("Compacted: {}", message.trim())
                };
                self.messages.push(ChatLine::System(display));
                self.messages.push(ChatLine::System(String::new()));
                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
            }
            TuiEvent::CompactError(err) => {
                self.messages
                    .push(ChatLine::Error(format!("Compact failed: {err}")));
                self.messages.push(ChatLine::System(String::new()));
                self.mode = AppMode::Input;
                self.focused_pane = Pane::Input;
                self.user_scrolled = false;
            }
        }
    }

    /// Switch the TUI into onboarding mode for first-run identity setup.
    pub fn start_onboarding(&mut self) {
        self.messages.clear();
        self.messages.push(ChatLine::System(
            "This agent has no identity yet. Let's create one.".into(),
        ));
        self.messages.push(ChatLine::System(String::new()));
        self.messages.push(ChatLine::System(
            "Describe how this agent should think, feel, and behave.".into(),
        ));
        self.messages.push(ChatLine::System(
            "Be as brief or as detailed as you like — the AI will distill it into an identity file.".into(),
        ));
        self.messages.push(ChatLine::System(
            "Ctrl+J or Shift+Enter for newlines. Enter to generate.".into(),
        ));
        self.messages.push(ChatLine::System(String::new()));
        self.mode = AppMode::Onboarding;
        self.focused_pane = Pane::Input;
        self.textarea = TextArea::default();
        self.textarea
            .set_placeholder_text("Describe the agent's personality, values, style…");
        self.textarea.set_cursor_line_style(Style::default());
    }

    /// Returns true when the TUI is in onboarding mode.
    pub fn is_onboarding(&self) -> bool {
        self.mode == AppMode::Onboarding
    }

    /// Enter shutdown mode — shows a compacting indicator and blocks input.
    pub fn start_compaction_shutdown(&mut self) {
        self.mode = AppMode::ShuttingDown;
        self.messages.push(ChatLine::System(
            "Compacting session before exit… (press Esc to skip)".into(),
        ));
        self.user_scrolled = false;
    }

    /// Set mode to streaming (called when user submits a prompt).
    pub fn set_streaming(&mut self) {
        self.mode = AppMode::Streaming;
        self.current_streaming.clear();
        self.current_thinking.clear();
        self.user_scrolled = false;
        self.turn_tool_calls = 0;
        self.tool_collapse_count = 0;
        self.current_tool_name.clear();
    }

    /// Set the max turns budget (for detecting exhaustion).
    pub fn set_max_turns(&mut self, max_turns: usize) {
        self.max_turns = max_turns;
    }

    /// Add a user message to the chat history.
    pub fn push_user_message(&mut self, text: &str) {
        self.messages.push(ChatLine::User(text.to_string()));
    }

    /// Add a system message to the chat history.
    pub fn push_system_message(&mut self, text: &str) {
        self.messages.push(ChatLine::System(text.to_string()));
    }

    /// Interrupt the current run and return to normal input mode.
    pub fn interrupt_run(&mut self, text: &str) {
        if !self.current_streaming.is_empty() {
            let partial = std::mem::take(&mut self.current_streaming);
            self.messages.push(ChatLine::Agent(partial));
        }
        self.current_thinking.clear();
        self.current_tool_name.clear();
        self.tool_collapse_count = 0;
        self.user_scrolled = false;
        self.mode = AppMode::Input;
        self.focused_pane = Pane::Input;
        self.messages.push(ChatLine::System(text.to_string()));
        self.messages.push(ChatLine::System(String::new()));
    }

    /// Open a modal overlay.
    pub fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Set the available commands for the command palette.
    pub fn set_available_commands(&mut self, commands: Vec<CommandEntry>) {
        self.available_commands = commands;
    }

    /// Send the human_ask response and return to streaming mode.
    pub fn send_human_ask_response(&mut self, response: String) {
        if let Some(tx) = self.human_ask_tx.take() {
            let _ = tx.send(response);
        }
        self.human_ask_message.clear();
        self.mode = AppMode::Streaming;
    }

    /// Record a submitted input into history and persist to file.
    pub fn record_input(&mut self, text: &str) {
        // Skip if duplicate of the most recent entry
        if self.input_history.last().map(|s| s.as_str()) == Some(text) {
            return;
        }
        self.input_history.push(text.to_string());
        // Persist to file
        if let Some(path) = &self.history_file {
            let encoded = text.replace('\\', "\\\\").replace('\n', "\\n");
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "{encoded}")
                });
        }
    }

    /// Navigate to the previous history entry (Up arrow).
    fn navigate_history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_cursor {
            None => {
                // Save current input and start browsing from the end
                self.input_stash = self.textarea.lines().join("\n");
                let idx = self.input_history.len() - 1;
                self.history_cursor = Some(idx);
                self.set_textarea_content(&self.input_history[idx].clone());
            }
            Some(idx) if idx > 0 => {
                let new_idx = idx - 1;
                self.history_cursor = Some(new_idx);
                self.set_textarea_content(&self.input_history[new_idx].clone());
            }
            _ => {} // Already at oldest entry
        }
    }

    /// Navigate to the next history entry (Down arrow).
    fn navigate_history_down(&mut self) {
        match self.history_cursor {
            Some(idx) => {
                if idx + 1 < self.input_history.len() {
                    let new_idx = idx + 1;
                    self.history_cursor = Some(new_idx);
                    self.set_textarea_content(&self.input_history[new_idx].clone());
                } else {
                    // Past the end → restore stash
                    self.history_cursor = None;
                    let stash = self.input_stash.clone();
                    self.set_textarea_content(&stash);
                    self.input_stash.clear();
                }
            }
            None => {}
        }
    }

    /// Check current textarea text and open/update/close the command palette.
    fn update_palette_state(&mut self) {
        let text = self.textarea.lines().join("\n");
        let text = text.trim_start();

        if text.starts_with('/') && self.textarea.lines().len() <= 1 {
            let after_slash = &text[1..];
            // Build a new palette from available_commands if we don't have one
            if self.command_palette.is_none() && !self.available_commands.is_empty() {
                let entries: Vec<CommandEntry> = self
                    .available_commands
                    .iter()
                    .map(|c| CommandEntry {
                        name: c.name.clone(),
                        description: c.description.clone(),
                        is_skill: c.is_skill,
                    })
                    .collect();
                self.command_palette = Some(CommandPalette::new(entries));
            }
            if let Some(palette) = &mut self.command_palette {
                // If there's a space, the user is typing args — close palette
                if after_slash.contains(' ') {
                    self.command_palette = None;
                } else {
                    palette.update_filter(after_slash);
                    if palette.is_empty() {
                        self.command_palette = None;
                    }
                }
            }
        } else {
            self.command_palette = None;
        }
    }

    /// Replace the textarea content with the given text.
    fn set_textarea_content(&mut self, text: &str) {
        self.textarea = TextArea::default();
        self.textarea
            .set_placeholder_text("Type or paste... (Enter send, Ctrl+J newline)");
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea.insert_str(text);
    }

    /// Clear all messages and reset scroll (used when resuming a session).
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.user_scrolled = false;
    }

    /// Add an agent message to the chat history.
    pub fn push_agent_message(&mut self, text: &str) {
        self.messages.push(ChatLine::Agent(text.to_string()));
    }

    /// Async receive from the agent channel.
    pub async fn recv_agent_event(&mut self) -> Option<TuiEvent> {
        self.agent_rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// Terminal setup / cleanup
// ---------------------------------------------------------------------------

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Install a panic hook that restores the terminal before printing the panic.
///
/// The hook only restores the terminal when the panic occurs on the **main
/// thread**.  Panics in spawned Tokio worker tasks must NOT restore the
/// terminal — the main TUI task is still running and doing so would corrupt
/// the alternate screen, leaving the terminal in an unusable state.
pub fn install_panic_hook() {
    let main_thread_id = std::thread::current().id();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        if std::thread::current().id() == main_thread_id {
            // Best-effort terminal restoration (main-thread panics only)
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                LeaveAlternateScreen,
                DisableBracketedPaste,
                DisableMouseCapture
            );
        }
        original_hook(panic_info);
    }));
}

// ---------------------------------------------------------------------------
// EventStream helper — async crossterm event reading
// ---------------------------------------------------------------------------

/// Read the next crossterm event asynchronously.
pub async fn next_crossterm_event(reader: &mut EventStream) -> Option<io::Result<Event>> {
    reader.next().await
}

#[cfg(test)]
mod tests {
    use super::{ChatApp, CommandEntry, CommandPalette, KeyAction};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    fn make_palette() -> CommandPalette {
        let commands: Vec<CommandEntry> = (0..20)
            .map(|i| CommandEntry {
                name: format!("/cmd{i}"),
                description: format!("command {i}"),
                is_skill: false,
            })
            .collect();
        CommandPalette::new(commands)
    }

    #[test]
    fn palette_filter_clears_state_when_empty() {
        let mut palette = make_palette();
        palette.cursor = 7;
        palette.view_offset = 5;
        palette.update_filter("zzzz-no-match");

        assert!(palette.filtered.is_empty());
        assert_eq!(palette.cursor, 0);
        assert_eq!(palette.view_offset, 0);
    }

    #[test]
    fn palette_filter_clamps_stale_window_after_shrink() {
        let mut palette = make_palette();
        palette.cursor = 15;
        palette.view_offset = 12;
        palette.update_filter("cmd1");

        assert!(!palette.filtered.is_empty());
        assert!(palette.cursor < palette.filtered.len());
        assert!(palette.view_offset < palette.filtered.len());
        assert!(palette.cursor >= palette.view_offset);
    }

    #[test]
    fn ctrl_c_interrupts_when_streaming() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut app = ChatApp::new(rx, false, false, None);
        app.set_streaming();

        let action = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, KeyAction::InterruptRun));
    }

    #[test]
    fn slash_stop_interrupts_when_streaming() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut app = ChatApp::new(rx, false, false, None);
        app.set_streaming();

        for ch in "/stop".chars() {
            let action = app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            assert!(matches!(action, KeyAction::None));
        }
        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, KeyAction::InterruptRun));
    }
}

// ---------------------------------------------------------------------------
// TuiChannel — that_channels::Channel impl for the TUI
// ---------------------------------------------------------------------------

/// A [`that_channels::Channel`] implementation backed by the TUI's mpsc event channel.
///
/// Wraps `mpsc::UnboundedSender<TuiEvent>` and maps [`that_channels::ChannelEvent`]
/// to the appropriate [`TuiEvent`] variants. Lives in `that-core` (not in
/// `that-channels`) to avoid a circular dependency between the two crates.
pub struct TuiChannel {
    id: String,
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl TuiChannel {
    /// Create a new TUI channel backed by the given sender.
    pub fn new(id: impl Into<String>, tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { id: id.into(), tx }
    }
}

#[async_trait::async_trait]
impl that_channels::Channel for TuiChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn format_instructions(&self) -> Option<String> {
        // TUI renders markdown natively; no special instructions needed.
        None
    }

    async fn send_event(
        &self,
        event: &that_channels::ChannelEvent,
        _target: Option<&that_channels::OutboundTarget>,
    ) -> anyhow::Result<that_channels::MessageHandle> {
        use that_channels::ChannelEvent;
        let _ = match event {
            ChannelEvent::StreamToken(t) => self.tx.send(TuiEvent::Token(t.clone())),
            ChannelEvent::ThinkingDelta(t) => self.tx.send(TuiEvent::ThinkingDelta(t.clone())),
            ChannelEvent::ToolCall {
                call_id,
                name,
                args,
            } => self.tx.send(TuiEvent::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            ChannelEvent::ToolResult {
                call_id,
                name,
                result,
            } => self.tx.send(TuiEvent::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
            }),
            ChannelEvent::Done {
                text,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_write_tokens,
            } => self.tx.send(TuiEvent::Done {
                text: text.clone(),
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cached_input_tokens: *cached_input_tokens,
                cache_write_tokens: *cache_write_tokens,
            }),
            ChannelEvent::Error(e) => self.tx.send(TuiEvent::Error(e.clone())),
            ChannelEvent::Retrying {
                attempt,
                max_attempts,
                delay_secs,
            } => self.tx.send(TuiEvent::Retrying {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_secs: *delay_secs,
            }),
            ChannelEvent::Notify(msg) => self.tx.send(TuiEvent::Token(format!("\n📢 {msg}\n"))),
            ChannelEvent::Attachment {
                filename,
                data,
                caption,
                ..
            } => {
                let size_kb = data.len() as f64 / 1024.0;
                let line = if let Some(cap) = caption.as_deref().filter(|s| !s.is_empty()) {
                    format!("\n📎 {filename} ({size_kb:.1} KB) — {cap}\n")
                } else {
                    format!("\n📎 {filename} ({size_kb:.1} KB)\n")
                };
                self.tx.send(TuiEvent::Token(line))
            }
            // TUI has its own visual rendering — typing indicators are not needed.
            ChannelEvent::TypingIndicator => return Ok(that_channels::MessageHandle::default()),
        };
        Ok(that_channels::MessageHandle::default())
    }

    async fn ask_human(
        &self,
        message: &str,
        _timeout: Option<u64>,
        _target: Option<&that_channels::OutboundTarget>,
    ) -> anyhow::Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        let _ = self.tx.send(TuiEvent::HumanAsk {
            message: message.to_string(),
            response_tx,
        });
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("TUI ask_human: response channel closed"))
    }

    async fn start_listener(
        &self,
        _tx: tokio::sync::mpsc::UnboundedSender<that_channels::InboundMessage>,
    ) -> anyhow::Result<()> {
        // TUI handles input via its own crossterm event loop.
        // No external inbound listener is needed.
        Ok(())
    }
}
