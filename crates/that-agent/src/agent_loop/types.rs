//! Core types for the agentic loop: messages, tool definitions, usage.

use serde::Serialize;

// ─── Conversation history ─────────────────────────────────────────────────────

/// A single entry in the LLM conversation history.
#[derive(Debug, Clone)]
pub enum Message {
    User {
        content: String,
        images: Vec<(Vec<u8>, String)>, // (data, mime_type)
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        images: Vec<(Vec<u8>, String)>, // (data, mime_type) — vision blocks for image_read
    },
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            images: vec![],
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }
}

// ─── Tool definitions (sent to the LLM) ──────────────────────────────────────

/// A tool definition serialized into the LLM API request.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's parameters.
    pub parameters: serde_json::Value,
}

// ─── Tool calls (returned by the LLM) ────────────────────────────────────────

/// A single tool invocation requested by the LLM in one turn.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args_json: String,
}

// ─── Token usage ─────────────────────────────────────────────────────────────

/// Token counts returned by the provider at the end of a turn.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens served from the prompt cache (Anthropic cache_read_input_tokens).
    pub cache_read_tokens: u32,
    /// Tokens written into the prompt cache (Anthropic cache_creation_input_tokens).
    pub cache_write_tokens: u32,
}

impl Usage {
    /// Combine two Usage values (e.g. across multiple turns).
    pub fn add(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: self.cache_read_tokens + other.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens + other.cache_write_tokens,
        }
    }
}
