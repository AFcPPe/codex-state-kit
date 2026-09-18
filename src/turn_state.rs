use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const HEADER_NAME: &str = "x-codex-turn-state";
pub const MAX_AGE_SECS: i64 = 3600;

/// 正常 token 长度约 292
pub const QUALITY_TOKEN_LEN: usize = 292;
/// 降智 token 长度约 312
pub const DEGRADED_TOKEN_LEN: usize = 312;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnState {
    pub token: String,
    pub issued_unix: i64,
    pub len: usize,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub captured_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStateView {
    pub status: String,
    pub age_secs: Option<i64>,
    pub len: Option<usize>,
    pub source: Option<String>,
    pub captured_at: Option<String>,
}

/// 内存 + 磁盘持久化一个 292 token，重启不丢失。
pub struct TurnStateStore {
    current: Option<TurnState>,
}

fn token_path() -> std::path::PathBuf {
    use crate::settings::{home_dir, is_dev_mode};
    if is_dev_mode() {
        home_dir().join(".codex-state-kit-dev-token.json")
    } else {
        home_dir().join(".codex-state-kit-token.json")
    }
}

impl TurnStateStore {
    pub fn load() -> Self {
        let current = std::fs::read_to_string(token_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<TurnState>(&raw).ok())
            .filter(|ts| {
                // 只恢复 292 token，312 丢弃
                !is_degraded_token(&ts.token) && ts.token.starts_with("gAAAAA")
            });
        if let Some(ref ts) = current {
            eprintln!(
                "[token] 从磁盘恢复 292 token（{}字节，age={}s）",
                ts.len,
                now_unix() - ts.issued_unix
            );
        }
        Self { current }
    }

    fn persist(&self) {
        let path = token_path();
        match &self.current {
            Some(ts) => {
                if let Ok(raw) = serde_json::to_string_pretty(ts) {
                    let _ = std::fs::write(&path, raw);
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    pub fn view(&self) -> TurnStateView {
        match &self.current {
            Some(state) => {
                let age = now_unix() - state.issued_unix;
                TurnStateView {
                    status: "active".into(),
                    age_secs: Some(age),
                    len: Some(state.len),
                    source: Some(state.source.clone()).filter(|v| !v.is_empty()),
                    captured_at: Some(state.captured_at.clone()).filter(|v| !v.is_empty()),
                }
            }
            None => TurnStateView {
                status: "empty".into(),
                age_secs: None,
                len: None,
                source: None,
                captured_at: None,
            },
        }
    }

    /// 读取当前 292 token（不消费）
    pub fn peek_freshest(&self) -> Option<String> {
        self.current.as_ref().map(|s| s.token.clone())
    }

    /// 清除当前 token（服务端拒绝时调用）
    pub fn invalidate_all(&mut self) {
        self.current = None;
        self.persist();
    }

    /// 存入新 292 token，替换旧的。312 直接拒绝。
    pub fn capture(&mut self, token: &str, source: &str) -> bool {
        if is_degraded_token(token) {
            return false;
        }
        let Some(state) = TurnState::from_token(token, source) else {
            return false;
        };
        self.current = Some(state);
        self.persist();
        true
    }

    pub fn fresh_count(&self) -> usize {
        if self.current.is_some() { 1 } else { 0 }
    }

    pub fn stampable(&self) -> Option<String> {
        self.peek_freshest()
    }
}

impl TurnState {
    pub fn from_token(token: &str, source: &str) -> Option<Self> {
        let token = token.trim();
        let issued_unix = issued_unix(token)?;
        Some(Self {
            token: token.to_string(),
            issued_unix,
            len: token.len(),
            source: source.to_string(),
            captured_at: now_rfc3339(),
        })
    }
}

pub fn issued_unix(token: &str) -> Option<i64> {
    let token = token.trim();
    if !token.starts_with("gAAAAA") {
        return None;
    }
    let bytes = decode_fernet(token)?;
    if bytes.len() < 9 || bytes[0] != 0x80 {
        return None;
    }
    Some(i64::from_be_bytes(bytes[1..9].try_into().ok()?))
}

pub fn should_stamp_http(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path.contains("/responses")
}

pub fn header_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| value.starts_with("gAAAAA"))
        .map(str::to_string)
}

pub fn has_http_turn_state(headers: &HeaderMap) -> bool {
    headers
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

pub fn apply_http_header(headers: &mut HeaderMap, token: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(HEADER_NAME.as_bytes()),
        HeaderValue::from_str(token),
    ) {
        headers.insert(name, value);
    }
}

pub fn clear_http_header(headers: &mut HeaderMap) {
    headers.remove(HEADER_NAME);
}

pub fn ws_looks_json_object(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .is_some_and(|value| value.is_object())
}

pub fn ws_has_turn_state(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match value.pointer(&format!("/client_metadata/{HEADER_NAME}")) {
        Some(Value::String(token)) => !token.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

pub fn stamp_ws_json(text: &str, token: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object_mut()?;
    let metadata = object.entry("client_metadata").or_insert_with(|| json!({}));
    let metadata = metadata.as_object_mut()?;
    metadata.insert(HEADER_NAME.to_string(), json!(token));
    Some(value.to_string())
}

pub fn token_from_json(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    find_token(&value)
}

fn find_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key.eq_ignore_ascii_case(HEADER_NAME) {
                    if let Some(token) = child
                        .as_str()
                        .map(str::trim)
                        .filter(|value| value.starts_with("gAAAAA"))
                    {
                        return Some(token.to_string());
                    }
                }
                if let Some(token) = find_token(child) {
                    return Some(token);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_token),
        _ => None,
    }
}

fn decode_fernet(token: &str) -> Option<Vec<u8>> {
    URL_SAFE
        .decode(token)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(token).ok())
        .or_else(|| {
            let mut padded = token.to_string();
            while padded.len() % 4 != 0 {
                padded.push('=');
            }
            URL_SAFE.decode(padded).ok()
        })
}

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 判断 token 长度是否属于 312（降智）区间
pub fn is_degraded_token(token: &str) -> bool {
    let len = token.trim().len();
    (308..=316).contains(&len)
}

/// 判断 token 长度是否属于 292（正常）区间
pub fn is_quality_token(token: &str) -> bool {
    let len = token.trim().len();
    (288..=296).contains(&len)
}

/// 返回 token 的质量标签
pub fn token_quality_label(token: &str) -> &'static str {
    if is_quality_token(token) {
        "292/normal"
    } else if is_degraded_token(token) {
        "312/degraded"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_for(issued: i64) -> String {
        let mut raw = vec![0x80];
        raw.extend_from_slice(&issued.to_be_bytes());
        raw.extend_from_slice(&[0u8; 64]);
        URL_SAFE.encode(raw)
    }

    #[test]
    fn parses_fernet_timestamp() {
        let issued = 1_700_000_000;
        let token = token_for(issued);
        assert!(token.starts_with("gAAAAA"));
        assert_eq!(issued_unix(&token), Some(issued));
    }

    #[test]
    fn rejects_non_token() {
        assert!(issued_unix("not-a-token").is_none());
        assert!(issued_unix("").is_none());
    }

    #[test]
    fn capture_and_peek() {
        let mut store = TurnStateStore::load();
        assert!(store.peek_freshest().is_none());
        assert_eq!(store.fresh_count(), 0);

        let token = token_for(now_unix() - 30);
        assert!(store.capture(&token, "fetch"));
        assert_eq!(store.peek_freshest().as_deref(), Some(token.as_str()));
        assert_eq!(store.fresh_count(), 1);
        assert_eq!(store.view().status, "active");
    }

    #[test]
    fn capture_replaces_old() {
        let mut store = TurnStateStore::load();
        let old = token_for(now_unix() - 100);
        let new = token_for(now_unix() - 5);
        store.capture(&old, "fetch");
        store.capture(&new, "fetch");
        assert_eq!(store.peek_freshest().as_deref(), Some(new.as_str()));
        assert_eq!(store.fresh_count(), 1);
    }

    #[test]
    fn invalidate_clears() {
        let mut store = TurnStateStore::load();
        let token = token_for(now_unix() - 30);
        store.capture(&token, "fetch");
        assert!(store.peek_freshest().is_some());
        store.invalidate_all();
        assert!(store.peek_freshest().is_none());
        assert_eq!(store.view().status, "empty");
    }

    #[test]
    fn rejects_312_token() {
        let mut store = TurnStateStore::load();
        let degraded = "a".repeat(312);
        assert!(!store.capture(&degraded, "fetch"));
        assert!(store.peek_freshest().is_none());
    }

    #[test]
    fn detects_degraded_token_length() {
        let short = "a".repeat(292);
        assert!(is_quality_token(&short));
        assert!(!is_degraded_token(&short));
        let long = "a".repeat(312);
        assert!(is_degraded_token(&long));
        assert!(!is_quality_token(&long));
    }

    #[test]
    fn stamps_ws_client_metadata() {
        let token = token_for(now_unix());
        let out = stamp_ws_json(r#"{"type":"item","client_metadata":{}}"#, &token).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["client_metadata"][HEADER_NAME], json!(token));
    }

    #[test]
    fn inserts_ws_token_when_missing() {
        let token = token_for(now_unix());
        let out = stamp_ws_json(r#"{"foo":1}"#, &token).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["client_metadata"][HEADER_NAME], json!(token));
    }

    #[test]
    fn leaves_non_json_ws_alone() {
        assert!(stamp_ws_json("not-json", "gAAAAA").is_none());
    }

    #[test]
    fn finds_nested_token() {
        let token = token_for(now_unix());
        let payload = json!({
            "event": "x",
            "headers": { "X-Codex-Turn-State": token }
        });
        assert_eq!(
            token_from_json(&payload.to_string()).as_deref(),
            Some(token.as_str())
        );
    }

    #[test]
    fn http_follow_up_has_turn_state() {
        let mut headers = HeaderMap::new();
        assert!(!has_http_turn_state(&headers));
        headers.insert(
            HeaderName::from_static(HEADER_NAME),
            HeaderValue::from_static("ts-1"),
        );
        assert!(has_http_turn_state(&headers));
        clear_http_header(&mut headers);
        assert!(!has_http_turn_state(&headers));
    }

    #[test]
    fn http_stamp_overwrites_header() {
        let token = token_for(now_unix());
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(HEADER_NAME),
            HeaderValue::from_static("old"),
        );
        apply_http_header(&mut headers, &token);
        assert_eq!(header_token(&headers).as_deref(), Some(token.as_str()));
        assert!(should_stamp_http("POST", "/backend-api/codex/responses"));
        assert!(!should_stamp_http("GET", "/responses"));
    }
}
