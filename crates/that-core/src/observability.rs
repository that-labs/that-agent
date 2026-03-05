//! Tracing initialisation with optional Phoenix / OpenTelemetry export.
//!
//! # Toggle
//!
//! Set `PHOENIX_TRACING=true` (or `1` / `yes`) to enable structured trace export.
//!
//! | Env var                               | Default                              | Purpose                                  |
//! |---------------------------------------|--------------------------------------|------------------------------------------|
//! | `PHOENIX_TRACING`                     | `false`                              | Master toggle (`true`/`1`/`yes`)         |
//! | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`  | —                                    | Full traces endpoint (preferred)         |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT`         | `http://127.0.0.1:6006`              | OTLP base URL (auto-appends `/v1/traces`) |
//! | `PHOENIX_LOG_LEVEL`                   | `trace`                              | Min level forwarded to Phoenix           |
//! | `PHOENIX_EXPORTER_LOG_ERRORS`         | `false`                              | Show OTel exporter internal error logs   |
//!
//! When the toggle is off, or when exporter init fails, the function falls back
//! to a plain `tracing-subscriber` fmt layer writing to stderr.
//!
//! # Load order
//!
//! Always call `dotenvy::dotenv()` **before** `init_tracing` so that env vars
//! from `.env` are visible.
//!
//! # Shutdown
//!
//! Call `shutdown_tracing()` at the end of `main()` to flush any buffered spans
//! before the process exits. It is a no-op when tracing export is disabled.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use opentelemetry::trace::TraceContextExt as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::{
    span_processor_with_async_runtime::BatchSpanProcessor, SdkTracerProvider,
};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// When true, the fmt (stderr) layer suppresses all output to avoid
/// corrupting the TUI alternate screen.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Suppress stderr tracing output (call when entering TUI mode).
pub fn suppress_fmt_output() {
    TUI_ACTIVE.store(true, Ordering::Relaxed);
}

/// Resume stderr tracing output (call when leaving TUI mode).
pub fn resume_fmt_output() {
    TUI_ACTIVE.store(false, Ordering::Relaxed);
}

/// A writer that emits to stderr unless the TUI is active, in which case
/// it silently discards output.
struct TuiAwareStderr;

impl std::io::Write for TuiAwareStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if TUI_ACTIVE.load(Ordering::Relaxed) {
            Ok(buf.len()) // discard
        } else {
            std::io::stderr().write(buf)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if TUI_ACTIVE.load(Ordering::Relaxed) {
            Ok(())
        } else {
            std::io::stderr().flush()
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TuiAwareStderr {
    type Writer = TuiAwareStderr;

    fn make_writer(&'a self) -> Self::Writer {
        TuiAwareStderr
    }
}

/// Fields to suppress from the fmt (stderr) layer — they contain huge LLM
/// payloads that make compact log output unreadable. The OTel layer still
/// receives them via span extensions, so Phoenix/Jaeger traces are unaffected.
const FILTERED_FIELD_NAMES: &[&str] = &[
    "input.value",
    "input.mime_type",
    "output.value",
    "output.mime_type",
    "gen_ai.prompt",
    "gen_ai.completion",
];

/// Formats span fields for the fmt layer, skipping blocklisted verbose fields.
/// Delegates to `DefaultFields` for the actual formatting — the filtering
/// happens by recording into an intermediate `String` per field and only
/// emitting allowed ones.
struct FilteredFields;

impl FilteredFields {
    fn new() -> Self {
        Self
    }
}

impl<'writer> FormatFields<'writer> for FilteredFields {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(
        &self,
        writer: tracing_subscriber::fmt::format::Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut v = FilteredFmtVisitor::new(writer);
        fields.record(&mut v);
        Ok(())
    }
}

/// Visitor that writes field=value pairs to a `Writer`, skipping blocklisted names.
/// Mimics `DefaultFields` output format: `field=value field2=value2 ...`
struct FilteredFmtVisitor<'writer> {
    writer: tracing_subscriber::fmt::format::Writer<'writer>,
    needs_sep: bool,
}

impl<'writer> FilteredFmtVisitor<'writer> {
    fn new(writer: tracing_subscriber::fmt::format::Writer<'writer>) -> Self {
        Self {
            writer,
            needs_sep: false,
        }
    }

    fn skip(field: &tracing::field::Field) -> bool {
        FILTERED_FIELD_NAMES.contains(&field.name())
    }

    fn sep(&mut self) {
        if self.needs_sep {
            let _ = write!(self.writer, " ");
        }
        self.needs_sep = true;
    }
}

/// Collapse the identical skip-check + sep + write pattern for scalar types.
macro_rules! record_scalar {
    ($name:ident, $ty:ty) => {
        fn $name(&mut self, field: &tracing::field::Field, value: $ty) {
            if Self::skip(field) {
                return;
            }
            self.sep();
            let _ = write!(self.writer, "{}={value}", field.name());
        }
    };
}

impl tracing::field::Visit for FilteredFmtVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if Self::skip(field) {
            return;
        }
        self.sep();
        let _ = write!(self.writer, "{}={:?}", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if Self::skip(field) {
            return;
        }
        self.sep();
        if field.name() == "message" {
            let _ = write!(self.writer, "{value}");
        } else {
            let _ = write!(self.writer, "{}={value}", field.name());
        }
    }

    record_scalar!(record_i64, i64);
    record_scalar!(record_u64, u64);
    record_scalar!(record_f64, f64);
    record_scalar!(record_bool, bool);
}

/// Initialise the global tracing subscriber.
///
/// Must be called once, after `.env` loading, from within a tokio runtime context
/// (i.e. inside `#[tokio::main]`).
pub fn init_tracing(default_filter: &str) {
    let enabled = std::env::var("PHOENIX_TRACING")
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);

    if enabled {
        // Default to `trace` so GenAI spans (emitted at info level) flow through.
        let phoenix_level = std::env::var("PHOENIX_LOG_LEVEL").unwrap_or_else(|_| "trace".into());
        let traces_endpoint = resolve_traces_endpoint();
        let exporter_result = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(traces_endpoint.clone())
            .build()
            .map_err(|e| e.to_string());

        match exporter_result {
            Ok(exporter) => {
                let provider = SdkTracerProvider::builder()
                    .with_span_processor(BatchSpanProcessor::builder(exporter, Tokio).build())
                    .build();

                // Store in global so the provider (and its processors) stay alive
                // for the entire process lifetime.
                let _ = TRACER_PROVIDER.set(provider);
                let tracer = TRACER_PROVIDER.get().unwrap().tracer("that");

                tracing_subscriber::registry()
                    .with(
                        tracing_opentelemetry::layer()
                            .with_tracer(tracer)
                            .with_filter(tracing_subscriber::EnvFilter::new(&phoenix_level)),
                    )
                    .with(
                        tracing_subscriber::fmt::layer()
                            .fmt_fields(FilteredFields::new())
                            .compact()
                            .with_target(false)
                            .with_writer(TuiAwareStderr)
                            .with_filter(build_fmt_env_filter(default_filter)),
                    )
                    .init();
                eprintln!(
                    "[observability] Phoenix tracing active → {traces_endpoint} (level: {phoenix_level})"
                );
                return;
            }
            Err(e) => {
                eprintln!(
                    "[observability] Phoenix exporter init failed, \
                     falling back to plain logging: {e}"
                );
            }
        }
    }

    // Plain stderr fallback (default path).
    tracing_subscriber::fmt()
        .fmt_fields(FilteredFields::new())
        .compact()
        .with_env_filter(build_fmt_env_filter(default_filter))
        .with_target(false)
        .with_writer(TuiAwareStderr)
        .init();
}

/// Best-effort immediate flush of queued spans without shutting down the tracer.
///
/// Useful for long-running interactive sessions (for example TUI) where we want
/// the just-completed run to appear in the backend immediately.
pub fn flush_tracing() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| {
                    let _ = provider.force_flush();
                });
            }
            _ => {
                let _ = provider.force_flush();
            }
        }
    }
}

/// Flush and shut down the OTel tracer provider.
///
/// Call this at the very end of `main()` to ensure all buffered spans are
/// exported before the process exits. No-op when exporter is disabled.
///
/// Uses `tokio::task::block_in_place` so the flush can block without
/// preventing the tokio reactor from making progress on other tasks
/// (in particular the background export HTTP request).
pub fn shutdown_tracing() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        eprintln!("[observability] Flushing Phoenix traces…");
        flush_tracing();
        // `provider.shutdown()` internally blocks waiting for the batch processor
        // background task to finish exporting.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| {
                    let _ = provider.shutdown();
                });
            }
            _ => {
                let _ = provider.shutdown();
            }
        }
        eprintln!("[observability] Phoenix flush complete.");
    }
}

fn resolve_traces_endpoint() -> String {
    if let Ok(traces) = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        let trimmed = traces.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:6006".to_string());
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    }
}

fn build_fmt_env_filter(default_filter: &str) -> tracing_subscriber::EnvFilter {
    let mut filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));

    // Prevent OTEL exporter transport failures from polluting interactive TUI frames.
    if !exporter_error_logs_enabled() {
        for directive in [
            "opentelemetry=off",
            "opentelemetry_sdk=off",
            "opentelemetry_otlp=off",
        ] {
            if let Ok(parsed) = directive.parse() {
                filter = filter.add_directive(parsed);
            }
        }
    }

    filter
}

fn exporter_error_logs_enabled() -> bool {
    std::env::var("PHOENIX_EXPORTER_LOG_ERRORS")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

/// Best-effort extraction of the current OpenTelemetry trace ID from the active
/// tracing span context. Returns `None` when no OTel context is attached.
pub fn current_trace_id() -> Option<String> {
    let cx = tracing::Span::current().context();
    let span = cx.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        Some(span_context.trace_id().to_string())
    } else {
        None
    }
}

/// Best-effort extraction of the current OpenTelemetry span ID from the active
/// tracing span context. Returns `None` when no OTel context is attached.
pub fn current_span_id() -> Option<String> {
    let cx = tracing::Span::current().context();
    let span = cx.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        Some(span_context.span_id().to_string())
    } else {
        None
    }
}
