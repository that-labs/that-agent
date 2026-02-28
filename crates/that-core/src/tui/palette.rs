use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

/// A single entry in the command palette.
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub is_skill: bool,
}

/// Inline autocomplete popup when the user types `/`.
pub struct CommandPalette {
    commands: Vec<CommandEntry>,
    pub(super) filtered: Vec<usize>,
    pub(super) cursor: usize,
    /// Index of the first visible entry in the filtered list.
    pub(super) view_offset: usize,
}

pub(super) const COMMAND_PALETTE_MAX_ROWS: usize = 10;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
