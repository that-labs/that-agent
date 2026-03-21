use super::tokenizer::count_tokens;

/// Keeps the first ~60% and last ~40% of the usable budget, with an elision marker.
/// Reserves tokens for the marker, then redistributes unused head tokens to tail.
pub(crate) fn compact_head_tail(text: &str, max_tokens: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 3 {
        return compact_head_only(text, max_tokens);
    }

    // Reserve ~5 tokens for the elision marker before splitting
    let marker_reserve = 5;
    let usable = max_tokens.saturating_sub(marker_reserve);
    let head_budget = (usable as f64 * 0.6) as usize;

    let mut head_lines = Vec::new();
    let mut head_tokens = 0;
    for line in &lines {
        let line_tokens = count_tokens(line);
        if head_tokens + line_tokens > head_budget {
            break;
        }
        head_lines.push(*line);
        head_tokens += line_tokens;
    }

    // Redistribute unused head tokens to the tail budget
    let unused_head = head_budget.saturating_sub(head_tokens);
    let base_tail_budget = (usable as f64 * 0.4) as usize;
    let tail_budget = base_tail_budget + unused_head;

    let mut tail_lines = Vec::new();
    let mut tail_tokens = 0;
    for line in lines.iter().rev() {
        let line_tokens = count_tokens(line);
        if tail_tokens + line_tokens > tail_budget {
            break;
        }
        tail_lines.push(*line);
        tail_tokens += line_tokens;
    }
    tail_lines.reverse();

    let omitted = lines
        .len()
        .saturating_sub(head_lines.len() + tail_lines.len());
    let mut result = head_lines.join("\n");
    if omitted > 0 {
        result.push_str(&format!("\n... ({} lines omitted) ...\n", omitted));
    }
    result.push_str(&tail_lines.join("\n"));
    result
}

/// Keeps only the beginning of the text up to the budget.
pub(crate) fn compact_head_only(text: &str, max_tokens: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines = Vec::new();
    let mut token_count = 0;

    for line in &lines {
        let line_tokens = count_tokens(line);
        if token_count + line_tokens > max_tokens.saturating_sub(5) {
            break;
        }
        result_lines.push(*line);
        token_count += line_tokens;
    }

    let omitted = lines.len().saturating_sub(result_lines.len());
    let mut result = result_lines.join("\n");
    if omitted > 0 {
        result.push_str(&format!("\n... ({} more lines truncated)", omitted));
    }
    result
}

/// Rule-based compaction: extract key patterns (errors, headings, summaries).
pub(crate) fn compact_rule_based(text: &str, max_tokens: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();

    let priority_patterns = [
        "error", "Error", "ERROR", "warn", "Warn", "WARN", "panic", "fail", "FAIL",
    ];
    let mut priority_lines: Vec<&str> = Vec::new();
    let mut other_lines: Vec<&str> = Vec::new();

    for line in &lines {
        if priority_patterns.iter().any(|p| line.contains(p)) {
            priority_lines.push(line);
        } else {
            other_lines.push(line);
        }
    }

    let mut result_lines = Vec::new();
    let mut token_count = 0;
    let budget = max_tokens.saturating_sub(5);

    for line in priority_lines.iter().chain(other_lines.iter()) {
        let line_tokens = count_tokens(line);
        if token_count + line_tokens > budget {
            break;
        }
        result_lines.push(*line);
        token_count += line_tokens;
    }

    let total = lines.len();
    let included = result_lines.len();
    let mut result = result_lines.join("\n");
    if included < total {
        result.push_str(&format!("\n... ({} of {} lines shown)", included, total));
    }
    result
}
