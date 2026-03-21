use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

pub use crate::model_catalog::MODEL_OPTIONS;

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
