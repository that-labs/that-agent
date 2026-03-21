/// A tool event emitted by the agent loop for session transcript logging.
///
/// Sent to an optional `mpsc` channel so callers can record a complete
/// tool call/result log without routing every event to external channels.
#[derive(Debug, Clone)]
pub enum ToolLogEvent {
    /// A tool was called. `args` is the raw JSON argument string.
    Call { name: String, args: String },
    /// A tool returned a result.
    Result {
        name: String,
        result: String,
        is_error: bool,
    },
}
