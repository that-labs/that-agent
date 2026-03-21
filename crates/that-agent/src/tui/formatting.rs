use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Style for code block content (subtle dark background + lighter text).
pub(super) const CODE_BG: Color = Color::Rgb(30, 30, 46);
pub(super) const CODE_FG: Color = Color::Rgb(180, 190, 210);

/// Convert agent text (which may contain fenced code blocks) into styled Lines.
///
/// The first line gets the "Agent: " prefix; subsequent lines are indented to align.
/// Lines inside ``` fences are styled with a code background.
pub(super) fn format_agent_text(text: &str) -> Vec<Line<'static>> {
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
pub(super) fn sanitize(s: &str) -> String {
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
pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a char boundary at or before `max`
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

/// Extract the skill name from `read_skill` tool args JSON.
pub(super) fn extract_skill_name(args_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| v["name"].as_str().map(|s| s.to_string()))
}
