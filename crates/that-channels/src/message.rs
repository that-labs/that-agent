//! Rich outbound message model.
//!
//! Adapters translate [`OutboundMessage`] into their native API representation
//! (e.g. Telegram `sendMessage` with `reply_markup`). The `send_raw` escape
//! hatch on [`Channel`][crate::Channel] covers platform-specific edge cases
//! that this model doesn't express.

use serde::{Deserialize, Serialize};

/// A structured outbound message with optional rich UI elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<ReplyMarkup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

/// Text parsing mode for outbound messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParseMode {
    MarkdownV2,
    HTML,
    Plain,
}

/// Interactive reply markup attached to an outbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplyMarkup {
    InlineKeyboard(Vec<Vec<InlineButton>>),
    ReplyKeyboard {
        keyboard: Vec<Vec<KeyboardButton>>,
        resize: bool,
        one_time: bool,
    },
    RemoveKeyboard,
}

/// A button in an inline keyboard row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineButton {
    pub text: String,
    pub callback_data: String,
}

/// A button in a reply keyboard row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardButton {
    pub text: String,
}
