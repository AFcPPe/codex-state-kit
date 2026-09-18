use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMethod {
    Device,
    #[default]
    Browser,
}

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_AGENT: &str = "codex-state-kit";
const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_EXPIRES_IN: u64 = 900;
const DEFAULT_INTERVAL: u64 = 5;

#[derive(Clone, Debug)]
pub struct LoginEndpoints {
    pub usercode_url: String,
    pub poll_url: String,
    pub oauth_token_url: String,
}

impl Default for LoginEndpoints {
    fn default() -> Self {
        Self {
            usercode_url: "https://auth.openai.com/api/accounts/deviceauth/usercode".into(),
            poll_url: "https://auth.openai.com/api/accounts/deviceauth/token".into(),
            oauth_token_url: "https://auth.openai.com/oauth/token".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    pub logged_in: bool,
    pub auth_mode: Option<String>,
    pub email: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStart {
    pub method: LoginMethod,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Clone, Debug)]
pub struct PendingLogin {
    cancelled: Arc<Mutex<bool>>,
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub expires_at: Instant,
    pub home: PathBuf,
}

impl PendingLogin {
    pub fn cancel(&self) {
        *self.cancelled.lock().expect("login cancellation") = true;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollStatus {
    Pending,
    Denied,
    Expired,
    Failed,
    Ok,
}

impl PollStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Failed => "error",
            Self::Ok => "ok",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PollResult {
    pub status: PollStatus,
    pub message: Option<String>,
    pub login: Option<LoginStatus>,
}

#[derive(Clone, Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct DevicePollSuccess {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedAuthMode {
    ApiKey,
    Chatgpt,
    Other,
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("build login http client")
}

pub fn login_status(home: &Path) -> LoginStatus {
    match read_auth(home) {
        Ok(Some(auth)) => status_from_auth(&auth),
        _ => LoginStatus {
            logged_in: false,
            auth_mode: None,
            email: None,
            account_id: None,
        },
    }
}

pub fn has_chatgpt_login(home: &Path) -> bool {
    login_status(home).logged_in
}

#[derive(Clone, Debug)]
pub(crate) struct ChatGptCredentials {
    pub access_token: String,
    pub account_id: String,
}

pub(crate) fn chatgpt_credentials(home: &Path) -> Result<ChatGptCredentials> {
    if !has_chatgpt_login(home) {
        bail!("尚未登录 ChatGPT。请先在本应用完成 ChatGPT 登录。");
    }
    let auth = read_auth(home)?.ok_or_else(|| anyhow::anyhow!("缺少 auth.json"))?;
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("auth.json 缺少 tokens"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("auth.json 缺少 access_token"))?
        .to_string();
    let stored_account = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let id_token = tokens.get("id_token").and_then(Value::as_str).unwrap_or("");
    let (jwt_account, _) = extract_account_metadata(id_token, &access_token);
    let account_id = jwt_account
        .or(stored_account)
        .ok_or_else(|| anyhow::anyhow!("无法从 auth.json 提取 chatgpt_account_id"))?;
    Ok(ChatGptCredentials {
        access_token,
        account_id,
    })
}

pub async fn start_device_login(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    home: PathBuf,
) -> Result<(LoginStart, PendingLogin)> {
    let response = client
        .post(&endpoints.usercode_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("start ChatGPT device login")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Device Code 请求失败: {status} - {text}");
    }
    let device: DeviceCodeResponse = response.json().await.context("parse device code")?;
    let interval = parse_interval(device.interval.as_ref());
    let expires_in = device.expires_in.unwrap_or(DEFAULT_EXPIRES_IN).max(1);
    let start = LoginStart {
        method: LoginMethod::Device,
        user_code: device.user_code.clone(),
        verification_uri: VERIFICATION_URI.to_string(),
        expires_in,
        interval,
    };
    let pending = PendingLogin {
        cancelled: Arc::new(Mutex::new(false)),
        device_auth_id: device.device_auth_id,
        user_code: device.user_code,
        verification_uri: VERIFICATION_URI.to_string(),
        expires_in,
        interval,
        expires_at: Instant::now() + Duration::from_secs(expires_in),
        home,
    };
    Ok((start, pending))
}

pub async fn poll_device_login(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    pending: &PendingLogin,
) -> Result<PollResult> {
    if *pending.cancelled.lock().expect("login cancellation") {
        bail!("登录已取消");
    }
    if Instant::now() >= pending.expires_at {
        return Ok(PollResult {
            status: PollStatus::Expired,
            message: Some("Device Code 已过期，请重新登录".into()),
            login: None,
        });
    }

    let response = client
        .post(&endpoints.poll_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&json!({
            "device_auth_id": pending.device_auth_id,
            "user_code": pending.user_code,
        }))
        .send()
        .await
        .context("poll ChatGPT device login")?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Ok(PollResult {
            status: PollStatus::Pending,
            message: None,
            login: None,
        });
    }
    if status == StatusCode::GONE {
        return Ok(PollResult {
            status: PollStatus::Expired,
            message: Some("Device Code 已过期，请重新登录".into()),
            login: None,
        });
    }
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if is_access_denied(&text) {
            return Ok(PollResult {
                status: PollStatus::Denied,
                message: Some("用户拒绝授权".into()),
                login: None,
            });
        }
        return Ok(PollResult {
            status: PollStatus::Failed,
            message: Some(format!("{status} - {text}")),
            login: None,
        });
    }

    let success: DevicePollSuccess = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            return Ok(PollResult {
                status: PollStatus::Failed,
                message: Some(format!("解析授权响应失败: {err}")),
                login: None,
            });
        }
    };

    match exchange_and_write(client, endpoints, &success, pending).await {
        Ok(login) => Ok(PollResult {
            status: PollStatus::Ok,
            message: Some("已写入 Codex auth.json".into()),
            login: Some(login),
        }),
        Err(err) => Ok(PollResult {
            status: PollStatus::Failed,
            message: Some(err.to_string()),
            login: None,
        }),
    }
}

async fn exchange_and_write(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    success: &DevicePollSuccess,
    pending: &PendingLogin,
) -> Result<LoginStatus> {
    let tokens = exchange_tokens(
        client,
        &endpoints.oauth_token_url,
        &success.authorization_code,
        &success.code_verifier,
        REDIRECT_URI,
    )
    .await?;
    let cancelled = pending.cancelled.lock().expect("login cancellation");
    if *cancelled {
        bail!("登录已取消");
    }
    persist_tokens(&pending.home, &tokens)
}

pub(crate) async fn exchange_tokens(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokenResponse> {
    let body = serde_urlencoded::to_string([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .context("encode oauth form")?;
    let response = client
        .post(token_url)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("无法连接授权服务，请稍后重新登录"))?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("Token 交换失败: {status}，请重新登录");
    }
    response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("授权服务返回了无效的 Token 响应"))
}

pub(crate) fn persist_tokens(home: &Path, tokens: &OAuthTokenResponse) -> Result<LoginStatus> {
    if tokens.access_token.trim().is_empty() {
        bail!("登录响应缺少 access_token");
    }
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录响应缺少 refresh_token"))?;
    let id_token = tokens
        .id_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录响应缺少 id_token"))?;
    let (account_id, email) = extract_account_metadata(id_token, &tokens.access_token);
    let account_id =
        account_id.ok_or_else(|| anyhow::anyhow!("无法从 token 中提取 chatgpt_account_id"))?;
    write_auth_json(
        home,
        id_token,
        &tokens.access_token,
        refresh_token,
        &account_id,
    )?;
    Ok(LoginStatus {
        logged_in: true,
        auth_mode: Some("chatgpt".into()),
        email,
        account_id: Some(account_id),
    })
}

fn write_auth_json(
    home: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": account_id,
        },
        "last_refresh": Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
    });
    atomic_write(&auth_path(home), &serde_json::to_vec_pretty(&auth)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth.json")
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

fn auth_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

fn read_auth(home: &Path) -> Result<Option<Value>> {
    let path = auth_path(home);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("parse auth.json")?;
    Ok(Some(value))
}

fn status_from_auth(auth: &Value) -> LoginStatus {
    let tokens = auth.get("tokens").and_then(Value::as_object);
    let access = tokens
        .and_then(|map| map.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let refresh = tokens
        .and_then(|map| map.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let id_token = tokens
        .and_then(|map| map.get("id_token"))
        .and_then(Value::as_str);
    let account_id = tokens
        .and_then(|map| map.get("account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let (jwt_account, email) = id_token
        .map(|token| extract_account_metadata(token, access.unwrap_or("")))
        .unwrap_or((None, None));
    let auth_mode = auth
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_string);
    let logged_in = resolved_auth_mode(auth) == ResolvedAuthMode::Chatgpt
        && access.is_some()
        && refresh.is_some();
    LoginStatus {
        logged_in,
        auth_mode: auth_mode.or_else(|| logged_in.then(|| "chatgpt".into())),
        email,
        account_id: jwt_account.or(account_id),
    }
}

fn resolved_auth_mode(auth: &Value) -> ResolvedAuthMode {
    let Some(obj) = auth.as_object() else {
        return ResolvedAuthMode::Other;
    };
    let present = |key: &str| obj.get(key).is_some_and(|value| !value.is_null());
    if let Some(mode) = obj.get("auth_mode").and_then(Value::as_str) {
        return match mode {
            "chatgpt" | "chatgptAuthTokens" => ResolvedAuthMode::Chatgpt,
            "apikey" => ResolvedAuthMode::ApiKey,
            _ => ResolvedAuthMode::Other,
        };
    }
    if present("personal_access_token")
        || present("bedrock_api_key")
        || present("bedrock_access_keys")
        || present("OPENAI_API_KEY")
    {
        return ResolvedAuthMode::ApiKey;
    }
    ResolvedAuthMode::Chatgpt
}

fn extract_account_metadata(
    id_token: &str,
    access_token: &str,
) -> (Option<String>, Option<String>) {
    let mut account_id = None;
    let mut email = None;
    if let Some(claims) = parse_jwt_claims(id_token) {
        account_id = chatgpt_account_id(&claims);
        email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    if account_id.is_none() {
        if let Some(claims) = parse_jwt_claims(access_token) {
            account_id = chatgpt_account_id(&claims);
            if email.is_none() {
                email = claims
                    .get("email")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
            }
        }
    }
    (account_id, email)
}

fn chatgpt_account_id(claims: &Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(Value::as_object)
                .and_then(|map| map.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

fn parse_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn parse_interval(value: Option<&Value>) -> u64 {
    let parsed = match value {
        Some(Value::Number(number)) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|float| float as u64)),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    };
    parsed.unwrap_or(DEFAULT_INTERVAL).max(1)
}

fn is_access_denied(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("access_denied") || lower.contains("access denied")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[derive(Clone, Copy)]
    enum MockMode {
        Success,
        Pending,
        MissingIdToken,
    }

    #[derive(Clone)]
    struct MockState {
        mode: MockMode,
        polls: Arc<AtomicU32>,
    }

    fn test_jwt(account: &str, email: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            json!({
                "chatgpt_account_id": account,
                "email": email,
            })
            .to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    async fn usercode() -> impl IntoResponse {
        Json(json!({
            "device_auth_id": "device-1",
            "user_code": "ABCD-EFGH",
            "expires_in": 900,
            "interval": 1,
        }))
    }

    async fn poll_token(State(state): State<MockState>) -> impl IntoResponse {
        let count = state.polls.fetch_add(1, Ordering::SeqCst);
        match state.mode {
            MockMode::Pending => (StatusCode::FORBIDDEN, "pending").into_response(),
            MockMode::Success | MockMode::MissingIdToken if count == 0 => {
                (StatusCode::FORBIDDEN, "pending").into_response()
            }
            _ => Json(json!({
                "authorization_code": "code-1",
                "code_verifier": "verifier-1",
            }))
            .into_response(),
        }
    }

    async fn oauth_token(State(state): State<MockState>) -> impl IntoResponse {
        let id_token = match state.mode {
            MockMode::MissingIdToken => None,
            _ => Some(test_jwt("acct-1", "user@example.com")),
        };
        Json(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "id_token": id_token,
        }))
    }

    async fn serve(mode: MockMode) -> (SocketAddr, LoginEndpoints) {
        let state = MockState {
            mode,
            polls: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/usercode", post(usercode))
            .route("/token", post(poll_token))
            .route("/oauth/token", post(oauth_token))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        (
            addr,
            LoginEndpoints {
                usercode_url: format!("{base}/usercode"),
                poll_url: format!("{base}/token"),
                oauth_token_url: format!("{base}/oauth/token"),
            },
        )
    }

    #[tokio::test]
    async fn cancelled_device_session_cannot_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Success).await;
        let client = http_client().unwrap();
        let (_, pending) = start_device_login(&client, &endpoints, home.path().into()).await.unwrap();
        pending.cancel();
        assert!(poll_device_login(&client, &endpoints, &pending).await.is_err());
        assert!(!home.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn pending_403_does_not_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Pending).await;
        let client = http_client().unwrap();
        let (_start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        let poll = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(poll.status, PollStatus::Pending);
        assert!(!home.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn success_writes_native_auth_json() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Success).await;
        let client = http_client().unwrap();
        let (start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(start.user_code, "ABCD-EFGH");
        let first = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(first.status, PollStatus::Pending);
        let second = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(second.status, PollStatus::Ok);
        let raw = std::fs::read_to_string(home.path().join("auth.json")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["auth_mode"], "chatgpt");
        assert!(value.get("OPENAI_API_KEY").unwrap().is_null());
        assert_eq!(value["tokens"]["access_token"], "access-1");
        assert_eq!(value["tokens"]["refresh_token"], "refresh-1");
        assert_eq!(value["tokens"]["account_id"], "acct-1");
        assert!(value["tokens"]["id_token"].as_str().unwrap().contains('.'));
        assert!(value["last_refresh"].as_str().unwrap().ends_with('Z'));
        let status = login_status(home.path());
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("user@example.com"));
        assert_eq!(status.account_id.as_deref(), Some("acct-1"));
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(encoded.get("accessToken").is_none());
        assert!(encoded.get("refreshToken").is_none());
        assert!(encoded.get("idToken").is_none());
        assert!(encoded.get("tokens").is_none());
    }

    #[tokio::test]
    async fn missing_id_token_does_not_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::MissingIdToken).await;
        let client = http_client().unwrap();
        let (_start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        let _ = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        let poll = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(poll.status, PollStatus::Failed);
        assert!(!home.path().join("auth.json").exists());
    }

    #[test]
    fn credentials_come_from_native_auth() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{
  "auth_mode": "chatgpt",
  "tokens": {
    "id_token": "a.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0In0.sig",
    "access_token": "access",
    "refresh_token": "refresh",
    "account_id": "acct"
  }
}"#,
        )
        .unwrap();
        let creds = chatgpt_credentials(home.path()).unwrap();
        assert_eq!(creds.access_token, "access");
        assert_eq!(creds.account_id, "acct");
    }

    #[test]
    fn api_key_only_is_not_chatgpt_login() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        assert!(!has_chatgpt_login(home.path()));
        let status = login_status(home.path());
        assert!(!status.logged_in);
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(encoded.get("OPENAI_API_KEY").is_none());
    }
}
