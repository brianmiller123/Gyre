//! # agent-telemetry
//!
//! OpenTelemetry 遥测桥：初始化 OTLP/控制台 span exporter，将 `tracing` span 导出。
//!
//! 保持解耦：本 crate 依赖 OTel，而 `agent`/`core` 仅用 `tracing`（不依赖 OTel），
//! 满足依赖洁癖守卫（核心 crate 不引入 OTel）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::TracerProvider;

/// 初始化遥测：若配置了 `otlp_endpoint` 则启用 batch exporter 导出到该 endpoint；
/// 否则仅本地控制台日志。
///
/// 返回的 [`TelemetryGuard`] 在 drop 时 flush span（应在程序退出前保持存活）。
///
/// # Errors
/// exporter 初始化失败时返回错误。
pub fn init(endpoint: Option<&str>) -> Result<TelemetryGuard, String> {
    let provider = if let Some(endpoint) = endpoint {
        // 使用 OTLP exporter（连接 collector）
        let exporter = build_otlp_exporter(endpoint)?;
        let p = TracerProvider::builder()
            .with_batch_exporter(exporter, Tokio)
            .build();
        register_subscriber(p.tracer("agent-project"));
        tracing::info!(endpoint, "已启用 OTLP span 导出");
        Some(p)
    } else {
        use tracing_subscriber::EnvFilter;
        // 日志必须走 stderr：stdout 在 ACP/LSP stdio 模式下是 JSON-RPC 协议通道，
        // 任何非协议行（如 tracing 日志）会破坏握手（客户端报 Parse error）。
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with_target(false)
            .try_init()
            .ok();
        None
    };
    Ok(TelemetryGuard { provider })
}

fn build_otlp_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::SpanExporter, String> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("OTLP exporter 构建失败: {e:?}"))
}

fn register_subscriber(tracer: opentelemetry_sdk::trace::Tracer) {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(otel_layer)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .try_init()
        .ok();
}

/// 遥测守卫：drop 时 flush 并关闭 exporter。
pub struct TelemetryGuard {
    provider: Option<TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            if let Err(e) = provider.shutdown() {
                eprintln!("OTLP provider 关闭警告: {e:?}");
            }
        }
    }
}
