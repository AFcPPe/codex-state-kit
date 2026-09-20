use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use url::Url;

pub const ROUTE_DEFAULT_SYSTEM: &str = "default_system";
pub const ROUTE_EXPLICIT_PROXY: &str = "explicit_proxy";
pub const ROUTE_EMBEDDED_WARP: &str = "embedded_warp";
pub const ROUTE_MANUAL_PROXY: &str = "manual_proxy";
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_LOGS: usize = 80;
const MAX_TOKEN_FETCH_LOGS: usize = 30;

#[derive(Clone, Debug, Default)]
pub struct NetworkLogDetails {
    pub account_id: Option<String>,
    pub account_email: Option<String>,
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
    pub response_status: Option<u16>,
    pub response_header_ms: Option<u128>,
    pub response_content_encoding: Option<String>,
    pub first_token_ms: Option<u128>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub in_progress: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub account_id: Option<String>,
    pub account_email: Option<String>,
    pub id: u64,
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u128,
    pub response_header_ms: Option<u128>,
    pub response_content_encoding: Option<String>,
    pub first_token_ms: Option<u128>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub in_progress: bool,
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
}

impl LogEntry {
    pub fn new(
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) -> Self {
        let ms = details
            .response_header_ms
            .unwrap_or_else(|| started.elapsed().as_millis());
        Self {
            id: LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            account_id: details.account_id,
            account_email: details.account_email,
            ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            method: method.to_string(),
            path: path.to_string(),
            status,
            ms,
            response_header_ms: details.response_header_ms,
            response_content_encoding: details.response_content_encoding,
            first_token_ms: details.first_token_ms,
            output_tokens: details.output_tokens,
            tokens_per_second: details.tokens_per_second,
            in_progress: details.in_progress,
            flow: details.flow,
            transport: details.transport,
            target_origin: details.target_origin,
            final_origin: details.final_origin,
            route_kind: details.route_kind,
            proxy_endpoint: details.proxy_endpoint,
            peer_addr: details.peer_addr,
            http_version: details.http_version,
            model: details.model,
            content_encoding: details.content_encoding,
            body_bytes: details.body_bytes,
            turn_state_action: details.turn_state_action,
            turn_state_len: details.turn_state_len,
            returned_turn_state_len: details.returned_turn_state_len,
            error_kind: details.error_kind,
        }
    }
}

pub fn safe_text(raw: &str, max_chars: usize) -> String {
    raw.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

pub fn endpoint_origin(raw: &str) -> String {
    let Ok(url) = Url::parse(raw.trim()) else {
        return "invalid-endpoint".into();
    };
    let Some(host) = url.host_str() else {
        return "invalid-endpoint".into();
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match url.port_or_known_default() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    }
}

pub fn network_details(upstream: &str, proxy: &str) -> NetworkLogDetails {
    let proxy = proxy.trim();
    NetworkLogDetails {
        flow: "business".into(),
        transport: "http".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if proxy.is_empty() {
            ROUTE_DEFAULT_SYSTEM.into()
        } else {
            ROUTE_EXPLICIT_PROXY.into()
        },
        proxy_endpoint: (!proxy.is_empty()).then(|| endpoint_origin(proxy)),
        content_encoding: "none".into(),
        turn_state_action: "not_applicable".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn token_network_details(
    upstream: &str,
    proxy: &str,
    embedded_warp: bool,
    model: &str,
) -> NetworkLogDetails {
    NetworkLogDetails {
        flow: "token_fetch".into(),
        transport: "http_sse".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if embedded_warp {
            ROUTE_EMBEDDED_WARP.into()
        } else {
            ROUTE_MANUAL_PROXY.into()
        },
        proxy_endpoint: Some(endpoint_origin(proxy)),
        model: Some(safe_text(model, 80)).filter(|value| !value.is_empty()),
        content_encoding: "json".into(),
        turn_state_action: "awaiting_response".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn safe_content_encoding(raw: Option<&str>) -> String {
    let value = raw.unwrap_or("none").trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "none" | "identity" => "none".into(),
        "gzip" | "x-gzip" | "br" | "deflate" | "zstd" => value,
        _ => "other".into(),
    }
}

pub fn request_error_kind(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "connect"
    } else if error.is_timeout() {
        "timeout"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "upstream"
    }
    .into()
}

/// Tracks response-level timing without retaining or logging response content.
/// For SSE, first-token timing follows the visible-output event (`delta`, text,
/// tool arguments, or image content) rather than response headers/preamble.
#[derive(Clone, Debug, Default)]
pub struct ResponseMetrics {
    first_token_ms: Option<u128>,
    output_tokens: Option<u64>,
    line: Vec<u8>,
    data: Vec<u8>,
    skip_event: bool,
    after_cr: bool,
    is_sse: bool,
    completed: bool,
    pub error_kind: Option<&'static str>,
}

// Decode only the statistics side channel. The proxy forwards original bytes.
// The writer feeds the bounded event parser directly, never collecting a whole
// decompressed response in memory.
#[derive(Default)]
struct MetricsSink {
    metrics: ResponseMetrics,
    elapsed_ms: u128,
    is_sse: bool,
}

impl std::io::Write for MetricsSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.metrics.observe(bytes, self.elapsed_ms, self.is_sse);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum MetricsDecoder {
    Plain(MetricsSink),
    Zstd(zstd::stream::write::Decoder<'static, MetricsSink>),
    Gzip(flate2::write::GzDecoder<MetricsSink>),
    Deflate(flate2::write::ZlibDecoder<MetricsSink>),
}

pub struct ResponseBodyMetrics {
    decoder: MetricsDecoder,
    disabled: bool,
}

impl ResponseBodyMetrics {
    pub fn new(encoding: &str) -> Self {
        let decoder = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(MetricsDecoder::Plain(MetricsSink::default())),
            "zstd" => zstd::stream::write::Decoder::new(MetricsSink::default())
                .and_then(|mut decoder| {
                    decoder.window_log_max(23)?;
                    Ok(MetricsDecoder::Zstd(decoder))
                })
                .ok(),
            "gzip" | "x-gzip" => Some(MetricsDecoder::Gzip(flate2::write::GzDecoder::new(
                MetricsSink::default(),
            ))),
            "deflate" => Some(MetricsDecoder::Deflate(flate2::write::ZlibDecoder::new(
                MetricsSink::default(),
            ))),
            _ => None,
        };
        let disabled = decoder.is_none();
        Self {
            decoder: decoder.unwrap_or_else(|| MetricsDecoder::Plain(MetricsSink::default())),
            disabled,
        }
    }

    fn sink_mut(&mut self) -> &mut MetricsSink {
        match &mut self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder.get_mut(),
            MetricsDecoder::Gzip(decoder) => decoder.get_mut(),
            MetricsDecoder::Deflate(decoder) => decoder.get_mut(),
        }
    }

    pub fn observe(&mut self, bytes: &[u8], elapsed_ms: u128, is_sse: bool) {
        use std::io::Write;
        if self.disabled {
            return;
        }
        let sink = self.sink_mut();
        sink.elapsed_ms = elapsed_ms;
        sink.is_sse = is_sse;
        let writer: &mut dyn Write = match &mut self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder,
            MetricsDecoder::Gzip(decoder) => decoder,
            MetricsDecoder::Deflate(decoder) => decoder,
        };
        if writer.write_all(bytes).and_then(|()| writer.flush()).is_err() {
            // A statistics decoding failure must not interrupt forwarding or
            // turn a successful HTTP request into a network error.
            self.disabled = true;
            self.sink_mut().metrics = ResponseMetrics::default();
        }
    }

    pub fn finish(&mut self, elapsed_ms: u128) {
        if !self.disabled {
            self.sink_mut().metrics.finish(elapsed_ms);
        }
    }
}

impl std::ops::Deref for ResponseBodyMetrics {
    type Target = ResponseMetrics;

    fn deref(&self) -> &Self::Target {
        let sink = match &self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder.get_ref(),
            MetricsDecoder::Gzip(decoder) => decoder.get_ref(),
            MetricsDecoder::Deflate(decoder) => decoder.get_ref(),
        };
        &sink.metrics
    }
}

impl ResponseMetrics {
    pub fn observe(&mut self, chunk: &[u8], elapsed_ms: u128, is_sse: bool) {
        self.is_sse = is_sse;
        if !is_sse {
            if !self.skip_event && self.data.len() + chunk.len() <= 128 * 1024 {
                self.data.extend_from_slice(chunk);
            } else {
                self.data.clear();
                self.skip_event = true;
            }
            return;
        }
        // Handle LF, CRLF, CR and arbitrary network/UTF-8 chunk boundaries. Bound
        // inspection memory per event, and recover after oversized events.
        for &byte in chunk {
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                if self.line.is_empty() {
                    if !self.skip_event {
                        let data = std::mem::take(&mut self.data);
                        self.observe_event(&data, elapsed_ms);
                    }
                    self.data.clear();
                    self.skip_event = false;
                } else {
                    if let Some(value) = self.line.strip_prefix(b"data:") {
                        let value = value.strip_prefix(b" ").unwrap_or(value);
                        if self.data.len() + value.len() + 1 <= 128 * 1024 {
                            self.data.extend_from_slice(value);
                            self.data.push(b'\n');
                        } else {
                            self.skip_event = true;
                        }
                    }
                    self.line.clear();
                }
            } else if self.line.len() < 128 * 1024 {
                self.line.push(byte);
            } else {
                self.skip_event = true;
            }
        }
    }

    pub fn finish(&mut self, elapsed_ms: u128) {
        if self.is_sse {
            self.observe(b"\n\n", elapsed_ms, true);
        } else if !self.skip_event {
            let data = std::mem::take(&mut self.data);
            self.observe_event(&data, elapsed_ms);
        }
    }

    pub fn first_token_ms(&self) -> Option<u128> {
        self.first_token_ms
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    fn observe_event(&mut self, event: &[u8], elapsed_ms: u128) {
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(event) else {
            return;
        };
        if self.is_sse && json.get("type").and_then(serde_json::Value::as_str) == Some("response.completed") {
            self.completed = true;
        }
        self.error_kind = match json.get("type").and_then(serde_json::Value::as_str) {
            Some("response.failed" | "error") => Some("response_failed"),
            Some("response.incomplete") => Some("response_incomplete"),
            _ => self.error_kind,
        };
        if self.is_sse && self.first_token_ms.is_none() && has_visible_output(&json) {
            self.first_token_ms = Some(elapsed_ms);
        }
        if let Some(tokens) = find_output_tokens(&json) {
            self.output_tokens = Some(tokens);
        }
    }
}

fn has_visible_output(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let event_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let visible_event = matches!(
        event_type,
        "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.audio_transcript.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
            | "response.image_generation_call.partial_image"
    );
    if visible_event {
        for key in ["delta", "partial_image_b64"] {
            if object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                return true;
            }
        }
    }
    let field = match event_type {
        "response.output_text.done"
        | "response.reasoning_summary_text.done"
        | "response.reasoning_text.done"
        | "response.audio_transcript.done" => Some("text"),
        "response.function_call_arguments.done" => Some("arguments"),
        "response.custom_tool_call_input.done" => Some("input"),
        _ => None,
    };
    if field.is_some_and(|key| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty())
    }) {
        return true;
    }
    if matches!(
        event_type,
        "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
    ) {
        return ["/part/text", "/part/transcript"].iter().any(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty())
        });
    }
    false
}

fn find_output_tokens(value: &serde_json::Value) -> Option<u64> {
    // Only trust provider usage metadata, never token-shaped fields in output/tool content.
    let usage = value
        .pointer("/response/usage")
        .filter(|usage| usage.is_object())
        .or_else(|| value.get("usage"))?;
    usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))?
        .as_u64()
}

pub fn push(logs: &mut VecDeque<LogEntry>, entry: LogEntry) {
    if entry.flow == "token_fetch" {
        let token_count = logs
            .iter()
            .filter(|existing| existing.flow == "token_fetch")
            .count();
        if token_count >= MAX_TOKEN_FETCH_LOGS {
            if let Some(index) = logs
                .iter()
                .position(|existing| existing.flow == "token_fetch")
            {
                logs.remove(index);
            }
        } else if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
    } else if logs.len() >= MAX_LOGS {
        logs.pop_front();
    }
    logs.push_back(entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_metrics_parse_fragmented_streams() {
        use std::io::Write;
        let events = [
            b"data: {\"type\":\"response.created\"}\n\n".as_slice(),
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n".as_slice(),
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"output_tokens\":120}}}\n\n".as_slice(),
        ];
        for encoding in ["zstd", "gzip", "deflate"] {
            let mut chunks = Vec::new();
            macro_rules! encode {
                ($encoder:expr) => {{
                    let mut encoder = $encoder;
                    for event in &events[..2] {
                        encoder.write_all(event).unwrap();
                        encoder.flush().unwrap();
                        chunks.push(std::mem::take(encoder.get_mut()));
                    }
                    encoder.write_all(events[2]).unwrap();
                    chunks.push(encoder.finish().unwrap());
                }};
            }
            match encoding {
                "zstd" => encode!(zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap()),
                "gzip" => encode!(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast())),
                _ => encode!(flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast())),
            }
            let mut metrics = ResponseBodyMetrics::new(encoding);
            for (index, chunk) in chunks.iter().enumerate() {
                for byte in chunk {
                    metrics.observe(&[*byte], (index as u128 + 1) * 50, true);
                }
                assert_eq!(metrics.first_token_ms(), (index > 0).then_some(100), "{encoding}");
            }
            metrics.finish(200);
            assert_eq!(metrics.output_tokens(), Some(120), "{encoding}");
            assert!(!metrics.disabled, "{encoding}");
        }
    }

    #[test]
    fn unsupported_or_invalid_compression_does_not_invent_metrics() {
        for encoding in ["br", "gzip, zstd", "zstd", "gzip", "deflate"] {
            let mut metrics = ResponseBodyMetrics::new(encoding);
            metrics.observe(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n", 25, true);
            metrics.finish(50);
            assert!(metrics.disabled, "{encoding}");
            assert_eq!(metrics.first_token_ms(), None);
            assert_eq!(metrics.output_tokens(), None);
            assert_eq!(metrics.error_kind, None);
        }
    }

    #[test]
    fn sse_metrics_ignore_preamble_and_parse_split_utf8_crlf_and_usage() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            b": keepalive\r\ndata: {\"type\":\"response.created\"}\r\n\r\n",
            10,
            true,
        );
        metrics.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n",
            20,
            true,
        );
        assert_eq!(metrics.first_token_ms(), None);
        let text = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\r\n\r\n";
        for byte in text.as_bytes() {
            metrics.observe(&[*byte], 50, true);
        }
        metrics.observe(b"data: {\"type\":\"response.completed\",\n", 60, true);
        metrics.observe(
            b"data: \"response\":{\"usage\":{\"output_tokens\":120}}}\n\ndata: [DONE]\n\n",
            70,
            true,
        );
        metrics.finish(80);
        assert_eq!(metrics.first_token_ms(), Some(50));
        assert_eq!(metrics.output_tokens(), Some(120));
    }

    #[test]
    fn sse_metrics_track_tools_images_and_never_use_output_as_usage() {
        for event in [
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.content_part.added","part":{"text":"hi"}}"#,
            r#"{"type":"response.output_text.done","text":"hi"}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#,
            r#"{"type":"response.custom_tool_call_input.delta","delta":"ls"}"#,
            r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"abc"}"#,
        ] {
            let mut metrics = ResponseMetrics::default();
            metrics.observe(format!("data: {event}\n\n").as_bytes(), 25, true);
            assert_eq!(metrics.first_token_ms(), Some(25));
        }
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"data: {\"output\":{\"output_tokens\":999}}\n\n", 30, true);
        assert_eq!(metrics.output_tokens(), None);
    }

    #[test]
    fn metrics_recover_from_oversized_events_and_keep_memory_bounded() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(&vec![b'x'; 512 * 1024], 10, true);
        assert!(metrics.line.len() <= 128 * 1024);
        metrics.observe(b"\n\ndata: {\"usage\":{\"output_tokens\":0}}\n\n", 20, true);
        assert_eq!(metrics.output_tokens(), Some(0));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn metrics_read_json_usage_without_inventing_nonstream_ttft() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"{\"usage\":{\"completion_tokens\":42}}", 10, false);
        metrics.finish(20);
        assert_eq!(metrics.output_tokens(), Some(42));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn metrics_capture_sse_error_class_without_error_message() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            b"data: {\"type\":\"response.failed\",\"error\":{\"message\":\"private\"}}\n\n",
            10,
            true,
        );
        assert_eq!(metrics.error_kind, Some("response_failed"));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn endpoint_origin_keeps_route_and_drops_credentials() {
        assert_eq!(
            endpoint_origin("socks5h://user:secret@127.0.0.1:1080/path?token=hidden"),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            endpoint_origin("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com:443"
        );
    }

    #[test]
    fn network_details_distinguishes_explicit_and_default_routes() {
        let direct = network_details("https://chatgpt.com/backend-api/codex", "");
        assert_eq!(direct.route_kind, ROUTE_DEFAULT_SYSTEM);
        assert!(direct.proxy_endpoint.is_none());

        let proxied = network_details(
            "https://chatgpt.com/backend-api/codex",
            "http://user:secret@localhost:7897",
        );
        assert_eq!(proxied.route_kind, ROUTE_EXPLICIT_PROXY);
        assert_eq!(
            proxied.proxy_endpoint.as_deref(),
            Some("http://localhost:7897")
        );
    }

    #[test]
    fn token_details_identify_warp_without_exposing_credentials() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://statekit:private@127.0.0.1:1080",
            true,
            "gpt-6-astra",
        );
        assert_eq!(details.flow, "token_fetch");
        assert_eq!(details.route_kind, ROUTE_EMBEDDED_WARP);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
        assert_eq!(details.model.as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn token_details_identify_manual_proxy() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://proxy.example.test:44445",
            false,
            "gpt-6-astra",
        );
        assert_eq!(details.route_kind, ROUTE_MANUAL_PROXY);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://proxy.example.test:44445")
        );
    }

    #[test]
    fn safe_text_removes_log_controls_and_bounds_input() {
        assert_eq!(safe_text("  gpt-6\nastra\t-extra", 12), "gpt-6astra-e");
    }

    #[test]
    fn token_fetch_bursts_do_not_evict_all_business_logs() {
        let mut entries = VecDeque::new();
        for _ in 0..60 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    200,
                    Instant::now(),
                    network_details("https://chatgpt.com", ""),
                ),
            );
        }
        for _ in 0..100 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    502,
                    Instant::now(),
                    token_network_details(
                        "https://chatgpt.com",
                        "socks5h://127.0.0.1:1080",
                        true,
                        "gpt-6-astra",
                    ),
                ),
            );
        }

        assert_eq!(entries.len(), MAX_LOGS);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "token_fetch")
                .count(),
            MAX_TOKEN_FETCH_LOGS
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "business")
                .count(),
            MAX_LOGS - MAX_TOKEN_FETCH_LOGS
        );
    }
}
