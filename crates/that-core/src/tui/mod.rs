mod channel_adapter;
mod formatting;
mod hook;
mod modal;
mod palette;
mod stats;
mod terminal;

pub use channel_adapter::TuiChannel;
pub use hook::TuiHook;
pub use modal::{Modal, ModalAction, ModalItem, ModalKind, MODEL_OPTIONS};
pub use palette::{CommandEntry, CommandPalette};
pub use stats::UsageStats;
pub use terminal::{
    install_panic_hook, next_crossterm_event, restore_terminal, setup_terminal, Tui,
};

use std::io;
use std::path::{Path, PathBuf};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::{mpsc, oneshot};
use tui_textarea::TextArea;

use formatting::{extract_skill_name, format_agent_text, sanitize, truncate_str};

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
        if key.code == KeyCode::Esc {
            if self.command_palette.is_some() {
                self.command_palette = None;
                return KeyAction::None;
            }
            return KeyAction::Quit;
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
        if let Some(idx) = self.history_cursor {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

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
