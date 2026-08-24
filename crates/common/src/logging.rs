//! `tracing` init + the shared `<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>`
//! formatter (docs/specs/Observability_Spec.md), plus optional
//! OpenTelemetry-compatible distributed tracing export (#49) layered
//! onto the exact same `tracing` spans/events every crate already emits
//! for logging — one instrumentation API (`tracing::instrument`,
//! `tracing::info!`/`warn!`/etc.), two consumers, not two separate
//! tracing systems to keep in sync.

use std::fmt;
use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::Visit;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

struct LineFormatter;

impl<S, N> FormatEvent<S, N> for LineFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| fmt::Error)?;
        let level = event.metadata().level();
        let source = event.metadata().target();

        write!(writer, "{timestamp} {level} {source} ")?;

        let mut message = MessageVisitor::default();
        event.record(&mut message);
        writer.write_str(&message.0)?;
        writeln!(writer)
    }
}

// message field written verbatim; any other fields appended as name=value
#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        use fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            if !self.0.is_empty() {
                let _ = write!(self.0, " ");
            }
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }
}

/// Unset means distributed tracing export is disabled entirely — unlike
/// `chat`/`metrics` (`common::config::ServicesConfig`, default-enabled
/// flags), there's no separate `WZ_SERVICE_TRACING_ENABLED` toggle: a
/// self-hoster who hasn't stood up an OTel collector has nothing useful
/// to point this at, so the presence of a real endpoint *is* the enable
/// signal — same "a config value's presence gates the behavior, no
/// redundant boolean" pattern `gateway::tls`'s `WZ_TLS_CERT_PATH` already
/// uses.
const OTEL_ENDPOINT_VAR: &str = "WZ_OTEL_ENDPOINT";

/// Tags every exported span's `service.name` resource attribute — what a
/// trace viewer (Jaeger, Tempo, ...) groups/colors traces by. Defaults to
/// `"worldzero"`: today every crate runs inside one combined `server`
/// process (Phase 1/2), so there is exactly one meaningful service name,
/// not one per crate — `tracing`'s own `target` field (the same one the
/// fixed log line format's `SOURCE` column resolves from) is still what
/// identifies *which crate* emitted a given span within that one
/// service. Override via `WZ_OTEL_SERVICE_NAME` once/if a deployment
/// actually splits services across processes.
const OTEL_SERVICE_NAME_VAR: &str = "WZ_OTEL_SERVICE_NAME";
const DEFAULT_OTEL_SERVICE_NAME: &str = "worldzero";

/// Kept alive for the process's lifetime once built — dropping an
/// `SdkTracerProvider` stops span export. No explicit shutdown/flush
/// hook exists anywhere else in this codebase either (metrics/DB pools
/// aren't drained on exit), so this matches that same "best-effort,
/// not gracefully drained on abrupt exit" posture; the batch exporter's
/// own periodic flush interval means an ordinary process exit still
/// gets most spans out.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Sets the global `tracing` subscriber. Call once, as early as possible
/// in any binary's `main.rs` — every existing call site (`server`,
/// `common::bin::migrate`, `chat`'s demo bins, ...) already does this
/// with no arguments and needs no change: whether OpenTelemetry export
/// is also active is decided entirely by `WZ_OTEL_ENDPOINT`, not by a
/// different function or a different code path (#49's acceptance
/// criteria). Level filtering via `RUST_LOG`, defaulting to `info`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .event_format(LineFormatter)
        .with_ansi(false);

    let otel_layer = std::env::var(OTEL_ENDPOINT_VAR).ok().map(build_otel_layer);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    match std::env::var(OTEL_ENDPOINT_VAR) {
        Ok(endpoint) => tracing::info!(endpoint, "distributed tracing export enabled"),
        Err(_) => {
            tracing::info!("distributed tracing export disabled ({OTEL_ENDPOINT_VAR} not set)")
        }
    }
}

fn build_otel_layer<S>(endpoint: String) -> impl tracing_subscriber::Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let service_name = std::env::var(OTEL_SERVICE_NAME_VAR)
        .unwrap_or_else(|_| DEFAULT_OTEL_SERVICE_NAME.to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()
        .expect("failed to build the OTLP span exporter");

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attribute(KeyValue::new("service.name", service_name.clone()))
                .build(),
        )
        .build();

    let tracer = provider.tracer(service_name);
    // Only the first `init()` call's provider is kept — matches
    // `tracing_subscriber::registry().init()`'s own "global subscriber
    // set once" contract this function is already bound by.
    let _ = TRACER_PROVIDER.set(provider);

    tracing_opentelemetry::layer().with_tracer(tracer)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::prelude::*;

    use super::LineFormatter;

    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(emit: impl FnOnce()) -> String {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .event_format(LineFormatter)
                .with_ansi(false)
                .with_writer(buf.clone()),
        );
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn matches_fixed_line_shape() {
        let output = capture(|| tracing::info!("hello world"));

        let mut parts = output.trim_end().splitn(4, ' ');
        let timestamp = parts.next().unwrap();
        let level = parts.next().unwrap();
        let source = parts.next().unwrap();
        let message = parts.next().unwrap();

        assert!(
            time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
                .is_ok(),
            "timestamp {timestamp:?} is not RFC 3339"
        );
        assert_eq!(level, "INFO");
        assert_eq!(source, "common::logging::tests");
        assert_eq!(message, "hello world");
    }

    #[test]
    fn all_levels_are_distinguishable() {
        let output = capture(|| {
            tracing::trace!("t");
            tracing::debug!("d");
            tracing::info!("i");
            tracing::warn!("w");
            tracing::error!("e");
        });

        for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
            assert!(output.contains(level), "missing {level} in:\n{output}");
        }
    }

    #[tokio::test]
    async fn build_otel_layer_does_not_panic_for_a_syntactically_valid_endpoint() {
        // Exporter construction is lazy (doesn't require a reachable
        // collector) but needs a Tokio runtime context to set up its
        // gRPC channel — just proves the `WZ_OTEL_ENDPOINT`-set branch
        // of `init()` wires itself up without panicking.
        let _layer = super::build_otel_layer::<tracing_subscriber::Registry>(
            "http://127.0.0.1:4317".to_string(),
        );
    }

    #[test]
    fn source_reflects_emitting_target_across_crate_boundaries() {
        let output = capture(|| {
            tracing::info!(target: "auth::session", "logged in");
        });

        assert!(
            output.contains("auth::session"),
            "expected target auth::session in:\n{output}"
        );
        assert!(
            !output.contains("common::"),
            "should not fall back to common's own path:\n{output}"
        );
    }
}
