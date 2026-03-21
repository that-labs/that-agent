use std::sync::LazyLock;
use tiktoken_rs::CoreBPE;

/// Cached tokenizer instance — initialized once, used everywhere.
pub(crate) static TOKENIZER: LazyLock<CoreBPE> =
    LazyLock::new(|| tiktoken_rs::cl100k_base().expect("tokenizer initialization should not fail"));

/// Approximate token count for a string using cl100k_base (GPT-4/Claude tokenizer).
pub fn count_tokens(text: &str) -> usize {
    TOKENIZER.encode_ordinary(text).len()
}
