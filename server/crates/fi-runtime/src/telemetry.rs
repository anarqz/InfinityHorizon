//! Structured logs in the shape Cloud Logging understands (docs/server/OBSERVABILITY.md §2).
//!
//! Cloud Run forwards each stdout line that is a JSON object to Cloud Logging, which lifts a few
//! special keys: `severity`, `message`, `timestamp`, `logging.googleapis.com/trace`,
//! `logging.googleapis.com/spanId` and `logging.googleapis.com/trace_sampled`
//! (cloud.google.com/logging/docs/structured-logging). tracing-subscriber's own JSON format writes
//! `level` instead of `severity`, so every line would land as DEFAULT; this formatter writes the
//! special keys and flattens the fields of the enclosing spans (the request context of
//! [`crate::http`]) into each line.
//!
//! Never log a secret, a token, a site key or a save's bytes: fields are written verbatim.

use crate::config::{self, ServiceInfo};
use serde_json::{Map, Value};
use std::fmt;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

/// Installs the global subscriber. `LOG_LEVEL` (or `RUST_LOG`) sets the filter, `info` by
/// default; `LOG_FORMAT=pretty` switches to human-readable lines for local development.
pub fn init(info: &ServiceInfo) {
    let filter = config::optional("RUST_LOG")
        .or_else(|| config::optional("LOG_LEVEL"))
        .unwrap_or_else(|| "info".to_owned());
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout);
    let installed = if config::optional("LOG_FORMAT").as_deref() == Some("pretty") {
        builder.try_init()
    } else {
        builder
            .fmt_fields(JsonFields::new())
            .event_format(CloudLogging::new(info.project.clone()))
            .try_init()
    };
    // A second init (tests, or a library that installed one) keeps the first subscriber.
    if installed.is_ok() {
        tracing::info!(service = info.service, version = info.version, revision = %info.revision, "starting");
    }
}

/// The Cloud Logging JSON event format.
pub struct CloudLogging {
    project: String,
}

impl CloudLogging {
    /// Trace ids are written as `projects/<project>/traces/<id>`, which is what links a log line
    /// to its trace in the console.
    pub fn new(project: String) -> Self {
        Self { project }
    }
}

fn severity(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.0
            .insert(field.name().to_owned(), Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), Value::from(format!("{value:?}")));
    }
}

impl<S, N> FormatEvent<S, N> for CloudLogging
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut line = Map::new();
        // Outer spans first, so the innermost value of a repeated field wins.
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                if let Some(fields) = extensions.get::<FormattedFields<N>>()
                    && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(fields)
                {
                    line.extend(map);
                }
            }
        }
        let mut fields = Map::new();
        event.record(&mut JsonVisitor(&mut fields));
        line.extend(fields);

        if let Some(Value::String(trace)) = line.remove("trace_id") {
            line.insert(
                "logging.googleapis.com/trace".to_owned(),
                Value::from(format!("projects/{}/traces/{trace}", self.project)),
            );
        }
        if let Some(span_id) = line.remove("span_id") {
            line.insert("logging.googleapis.com/spanId".to_owned(), span_id);
        }
        if let Some(sampled) = line.remove("trace_sampled") {
            line.insert("logging.googleapis.com/trace_sampled".to_owned(), sampled);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        line.insert("severity".to_owned(), Value::from(severity(meta.level())));
        line.insert(
            "timestamp".to_owned(),
            serde_json::json!({ "seconds": now.as_secs(), "nanos": now.subsec_nanos() }),
        );
        line.insert("target".to_owned(), Value::from(meta.target()));
        if !line.contains_key("message") {
            line.insert("message".to_owned(), Value::from(meta.name()));
        }
        let text = serde_json::to_string(&Value::Object(line)).map_err(|_| fmt::Error)?;
        writeln!(writer, "{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[test]
    fn lines_carry_cloud_logging_keys_and_span_fields() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .fmt_fields(JsonFields::new())
            .event_format(CloudLogging::new("demo-local".into()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "request",
                request_id = "r-1",
                trace_id = "abc123",
                span_id = "00f067aa0ba902b7"
            );
            let _guard = span.enter();
            tracing::warn!(profile = "p-1", "save refused");
        });
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let line: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["severity"], "WARNING");
        assert_eq!(line["message"], "save refused");
        assert_eq!(line["profile"], "p-1");
        assert_eq!(line["request_id"], "r-1");
        assert_eq!(
            line["logging.googleapis.com/trace"],
            "projects/demo-local/traces/abc123"
        );
        assert_eq!(line["logging.googleapis.com/spanId"], "00f067aa0ba902b7");
        assert!(line["timestamp"]["seconds"].as_u64().unwrap() > 1_700_000_000);
        assert!(line.get("trace_id").is_none());
    }
}
