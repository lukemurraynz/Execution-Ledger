use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::{Span as _, Tracer as _};
use serde_json::Value;

static TELEMETRY_INIT: OnceLock<()> = OnceLock::new();

/// Initializes tracing and the current OpenTelemetry export path once.
pub fn init_runtime_telemetry(service_name: &'static str) {
    let () = TELEMETRY_INIT.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .compact()
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);

        if matches!(
            std::env::var("EXECUTION_LEDGER_OTEL_STDOUT")
                .ok()
                .as_deref(),
            Some("1" | "true" | "yes")
        ) {
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_simple_exporter(opentelemetry_stdout::SpanExporter::default())
                .build();
            global::set_tracer_provider(provider);
            tracing::info!(service_name, "execution-ledger telemetry initialized");
        } else {
            tracing::info!(service_name, "execution-ledger tracing initialized");
        }
    });
}

/// Emits one structured runtime event in a stable envelope.
pub fn emit_runtime_event(event_name: &'static str, payload: &Value) {
    tracing::info!(
        target: "execution_ledger.runtime",
        event_name,
        payload = %payload
    );

    let tracer = global::tracer("execution-ledger-runtime");
    let mut span = tracer.start(event_name);
    span.set_attribute(KeyValue::new("execution.event", event_name));
    span.set_attribute(KeyValue::new("execution.payload", payload.to_string()));
    span.end();
}

#[allow(clippy::cast_precision_loss)] // u64 counter → f64 for metric emission; lossless up to 2^53
pub fn log_counter(metric_name: &'static str, value: u64, labels: &Value) {
    log_metric("counter", metric_name, value as f64, labels);
}

pub fn log_histogram(metric_name: &'static str, value: f64, labels: &Value) {
    log_metric("histogram", metric_name, value, labels);
}

pub fn log_gauge(metric_name: &'static str, value: f64, labels: &Value) {
    log_metric("gauge", metric_name, value, labels);
}

fn log_metric(kind: &'static str, metric_name: &'static str, value: f64, labels: &Value) {
    tracing::info!(
        target: "execution_ledger.metric",
        kind,
        metric_name,
        value,
        labels = %labels
    );
}
