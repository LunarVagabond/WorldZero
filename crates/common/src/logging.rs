//! `tracing` init + the shared `<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>`
//! formatter (docs/specs/Observability_Spec.md).

use std::fmt;

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

/// Sets the global `tracing` subscriber. Call once, as early as possible in
/// `server`'s `main.rs`. Level filtering via `RUST_LOG`, defaulting to `info`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .event_format(LineFormatter)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
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
