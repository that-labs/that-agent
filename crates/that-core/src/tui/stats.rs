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

impl Default for UsageStats {
    fn default() -> Self {
        Self::new()
    }
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
