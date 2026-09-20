use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use url::Url;

use crate::attach::{self, is_attached};
use crate::fetch;
use crate::login::{self, has_chatgpt_login};
use crate::logs::{self, LogEntry, NetworkLogDetails};
use crate::traffic::{AccountTraffic, RequestActivity, TrafficTracker};
use crate::settings::{save_settings, OutboundMode, Settings, SettingsPatch};
use crate::turn_state::{self, TurnStateStore, TurnStateView};
use crate::warp::{WarpRuntime, WarpStatus};

fn debug_log(msg: &str) {
    eprintln!("{}", msg);
    let path = crate::settings::home_dir().join(if cfg!(debug_assertions) {
        ".codex-state-kit-dev-debug.log"
    } else {
        ".codex-state-kit-debug.log"
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchRetryClass {
    Normal,
    Backoff,
    Auth,
    Stale,
}

#[derive(Debug)]
struct FetchOnceError {
    message: String,
    retry: FetchRetryClass,
}

impl FetchOnceError {
    fn new(message: impl Into<String>, retry: FetchRetryClass) -> Self {
        Self {
            message: message.into(),
            retry,
        }
    }
}

impl std::fmt::Display for FetchOnceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FetchOnceError {}

fn fetch_retry_delay(class: FetchRetryClass) -> Duration {
    match class {
        FetchRetryClass::Normal => fetch::RETRY_INTERVAL,
        FetchRetryClass::Backoff => fetch::ERROR_BACKOFF,
        FetchRetryClass::Auth => fetch::AUTH_BACKOFF,
        FetchRetryClass::Stale => fetch::RETRY_INTERVAL,
    }
}

fn classify_fetch_failure(details: &NetworkLogDetails) -> FetchRetryClass {
    match details.response_status {
        Some(401 | 403) => FetchRetryClass::Auth,
        Some(429 | 503) => FetchRetryClass::Backoff,
        _ if details.error_kind.as_deref() == Some("connect") => FetchRetryClass::Backoff,
        _ => FetchRetryClass::Normal,
    }
}

fn model_for_fetch_round(models: &[String], round: u32) -> Option<&str> {
    if models.is_empty() {
        return None;
    }
    Some(models[(round.saturating_sub(1) as usize) % models.len()].as_str())
}

fn capture_fetched_ticket(store: &mut TurnStateStore, model: &str, token: &str) -> bool {
    if !store.capture(model, token, "fetch") {
        return false;
    }
    token.trim().len() == store.bound_len_for(model)
        && store.peek_for_model(model).as_deref() == Some(token.trim())
        && !store.needs_refresh(model)
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    pub proxy_ok: bool,
    pub attached: bool,
    pub proxy_error: Option<String>,
    pub attach_error: Option<String>,
    pub outbound_proxy: String,
    pub upstream_proxy: String,
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub warp: WarpStatus,
    pub fetch_error: Option<String>,
    pub fetch_ok_at: Option<String>,
    pub turn_state: TurnStateView,
    pub degraded: bool,
    pub degraded_at: Option<String>,
    pub logs: Vec<LogEntry>,
    pub account_traffic: AccountTraffic,
    pub current_account_id: Option<String>,
    pub current_account_email: Option<String>,
}

pub struct App {
    pub warp: WarpRuntime,
    pub settings: Mutex<Settings>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    traffic: TrafficTracker,
    pub proxy_ok: AtomicBool,
    pub login_http: reqwest::Client,
    leftover_restored: AtomicBool,
    proxy_error: Mutex<Option<String>>,
    fetch_error: Mutex<Option<String>>,
    fetch_ok_at: Mutex<Option<String>>,
    fetch_round: AtomicU32,
    fetch_gate: Mutex<()>,
    fetch_next_allowed_at: Mutex<Instant>,
    fetch_generation: AtomicU64,
    fetch_change_notify: Notify,
    fetch_transition: Mutex<()>,
    turn_state: Mutex<TurnStateStore>,
    http: Mutex<reqwest::Client>,
    degraded: AtomicBool,
    degraded_at: Mutex<Option<String>>,
    pub degrade_notify: Notify,
    pub warp_wake: Notify,
    /// 新模型被发现时通知 fetch 循环立即唤醒
    model_notify: Notify,
    /// 是否已注册 settings.models 中的种子模型
    seeds_registered: AtomicBool,
}

impl App {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_warp(settings, WarpRuntime::default())
    }

    pub fn with_warp(settings: Settings, warp: WarpRuntime) -> Result<Self> {
        let http = upstream_http_client(&settings.upstream_proxy)?;
        Ok(Self {
            warp,
            settings: Mutex::new(settings),
            logs: Mutex::new(VecDeque::with_capacity(80)),
            traffic: TrafficTracker::default(),
            proxy_ok: AtomicBool::new(false),
            login_http: crate::login::http_client()?,
            leftover_restored: AtomicBool::new(false),
            proxy_error: Mutex::new(None),
            fetch_error: Mutex::new(None),
            fetch_ok_at: Mutex::new(None),
            fetch_round: AtomicU32::new(0),
            fetch_gate: Mutex::new(()),
            fetch_next_allowed_at: Mutex::new(Instant::now()),
            fetch_generation: AtomicU64::new(0),
            fetch_change_notify: Notify::new(),
            fetch_transition: Mutex::new(()),
            turn_state: Mutex::new(TurnStateStore::load()),
            http: Mutex::new(http),
            degraded: AtomicBool::new(false),
            degraded_at: Mutex::new(None),
            degrade_notify: Notify::new(),
            warp_wake: Notify::new(),
            model_notify: Notify::new(),
            seeds_registered: AtomicBool::new(false),
        })
    }

    async fn sync_logged_in_account(&self) {
        let _transition = self.fetch_transition.lock().await;
        let home = self.settings.lock().await.codex_home.clone();
        let Ok(creds) = login::chatgpt_credentials(Path::new(&home)) else {
            return;
        };
        let needs_change = !self
            .turn_state
            .lock()
            .await
            .is_bound_to_account(&creds.account_id);
        if !needs_change {
            return;
        }
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.fetch_change_notify.notify_waiters();
        let _gate = self.fetch_gate.lock().await;
        self.turn_state.lock().await.bind_account(&creds.account_id);
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.seeds_registered.store(false, Ordering::Relaxed);
        *self.fetch_error.lock().await = None;
        *self.fetch_ok_at.lock().await = None;
        *self.fetch_next_allowed_at.lock().await = Instant::now();
        self.model_notify.notify_one();
    }

    pub async fn status(&self) -> Status {
        self.sync_logged_in_account().await;
        let settings = self.settings.lock().await.clone();
        let logs = self.logs.lock().await.iter().cloned().collect();
        let login_status = login::login_status(Path::new(&settings.codex_home));
        let account = login_status.account_id;
        let account_traffic = self.traffic.view(account.as_deref(), Instant::now());
        let attached = is_attached(
            Path::new(&settings.codex_home),
            &format!("http://{}", settings.proxy_listen),
        );
        Status {
            proxy_listen: settings.proxy_listen,
            upstream: settings.upstream,
            codex_home: settings.codex_home,
            proxy_ok: self.proxy_ok.load(Ordering::Relaxed),
            attached,
            proxy_error: self.proxy_error.lock().await.clone(),
            attach_error: None,
            outbound_proxy: settings.outbound_proxy,
            upstream_proxy: settings.upstream_proxy,
            outbound_mode: settings.outbound_mode,
            warp_http2: settings.warp_http2,
            warp: self.warp.status(),
            fetch_error: self.fetch_error.lock().await.clone(),
            fetch_ok_at: self.fetch_ok_at.lock().await.clone(),
            turn_state: self.turn_state.lock().await.view(),
            degraded: self.degraded.load(Ordering::Relaxed),
            degraded_at: self.degraded_at.lock().await.clone(),
            logs,
            account_traffic,
            current_account_id: account,
            current_account_email: login_status.email,
        }
    }

    pub async fn refresh_turn_state(&self) -> Result<Status> {
        self.sync_logged_in_account().await;
        let settings = self.settings.lock().await.clone();
        let mut models = settings.models.clone();
        if models.is_empty() {
            models = self.turn_state.lock().await.all_active_models();
        }
        if models.is_empty() {
            let model = fetch::preferred_model(Path::new(&settings.codex_home));
            self.turn_state.lock().await.register_model(&model);
            models.push(model);
        }
        for model in &models {
            if let Err(e) = self.fetch_once(model).await {
                eprintln!("[refresh] 模型 {} 获取失败: {e}", model);
                return Err(e.into());
            }
        }
        Ok(self.status().await)
    }

    /// 用户切换绑定的 token 长度（传 None 恢复账号自动识别的 292/332）
    pub async fn set_bound_token_len(&self, len: Option<usize>) -> Status {
        let _transition = self.fetch_transition.lock().await;
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.fetch_change_notify.notify_waiters();
        let _gate = self.fetch_gate.lock().await;
        {
            let mut store = self.turn_state.lock().await;
            store.set_bound_len(len);
        }
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        *self.fetch_next_allowed_at.lock().await = Instant::now();
        self.fetch_change_notify.notify_waiters();
        drop(_gate);
        drop(_transition);
        self.degrade_notify.notify_one();
        self.status().await
    }

    pub async fn set_model_bound_token_len(&self, model: &str, len: Option<usize>) -> Status {
        let _transition = self.fetch_transition.lock().await;
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.fetch_change_notify.notify_waiters();
        let _gate = self.fetch_gate.lock().await;
        {
            let mut store = self.turn_state.lock().await;
            store.set_model_bound_len(model, len);
        }
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        *self.fetch_next_allowed_at.lock().await = Instant::now();
        self.fetch_change_notify.notify_waiters();
        drop(_gate);
        drop(_transition);
        self.degrade_notify.notify_one();
        self.status().await
    }

    fn fetch_settings(&self, settings: &Settings) -> Result<Settings> {
        let mut effective = settings.clone();
        if effective.outbound_mode == OutboundMode::Warp {
            effective.outbound_proxy = self.warp.proxy_url()?;
        }
        if effective.outbound_proxy.trim().is_empty() {
            anyhow::bail!("尚未配置出站代理");
        }
        Ok(effective)
    }

    async fn wait_for_fetch_slot(&self, generation: u64) -> bool {
        loop {
            let notified = self.fetch_change_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.fetch_generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            let wait = {
                let next = *self.fetch_next_allowed_at.lock().await;
                next.saturating_duration_since(Instant::now())
            };
            if wait.is_zero() {
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = &mut notified => {},
            }
        }
    }

    async fn defer_next_fetch(&self, delay: Duration) {
        *self.fetch_next_allowed_at.lock().await = Instant::now() + delay;
    }

    async fn fetch_once(&self, model: &str) -> std::result::Result<String, FetchOnceError> {
        let _gate = self.fetch_gate.lock().await;
        let generation = self.fetch_generation.load(Ordering::SeqCst);
        if generation % 2 == 1 {
            return Err(FetchOnceError::new(
                format!("[{model}] 配置切换中，暂不获取票据"),
                FetchRetryClass::Normal,
            ));
        }
        let saved = self.settings.lock().await.clone();
        let settings = match self.fetch_settings(&saved) {
            Ok(settings) => settings,
            Err(err) => {
                let message = err.to_string();
                self.defer_next_fetch(fetch::ERROR_BACKOFF).await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Backoff));
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            let message = "尚未登录 ChatGPT".to_string();
            self.defer_next_fetch(fetch::AUTH_BACKOFF).await;
            *self.fetch_error.lock().await = Some(message.clone());
            return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
        }
        let creds = match login::chatgpt_credentials(Path::new(&settings.codex_home)) {
            Ok(creds) => creds,
            Err(err) => {
                let message = format!("{err:#}");
                self.defer_next_fetch(fetch::AUTH_BACKOFF).await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
            }
        };
        let (target_len, allow_auto_quality, account_matches) = {
            let store = self.turn_state.lock().await;
            (
                store.bound_len_for(model),
                store.allows_auto_quality_discovery(model),
                store.is_bound_to_account(&creds.account_id),
            )
        };
        if !account_matches {
            return Err(FetchOnceError::new(
                format!("[{model}] 登录账号与票据池绑定账号不一致"),
                FetchRetryClass::Stale,
            ));
        }

        for attempt in 1..=fetch::CONNECT_ATTEMPTS {
            if !self.wait_for_fetch_slot(generation).await {
                let message = format!("[{model}] 配置已变化，取消旧线路票据请求");
                return Err(FetchOnceError::new(message, FetchRetryClass::Stale));
            }
            if !fetch_account_is_current(&settings, &creds.account_id) {
                return Err(FetchOnceError::new(format!("[{model}] 登录账号已变化，取消旧账号票据请求"), FetchRetryClass::Stale));
            }
            let client = match fetch::http_client(&settings.outbound_proxy) {
                Ok(client) => client,
                Err(err) => {
                    let message = format!("{err:#}");
                    self.defer_next_fetch(fetch::ERROR_BACKOFF).await;
                    *self.fetch_error.lock().await = Some(message.clone());
                    return Err(FetchOnceError::new(message, FetchRetryClass::Backoff));
                }
            };
            let started = Instant::now();
            let mut details = NetworkLogDetails::default();
            let result = fetch::fetch_turn_state_with_log(
                &client,
                &settings,
                &creds,
                model,
                target_len,
                allow_auto_quality,
                &mut details,
            )
            .await;

            if !fetch_account_is_current(&settings, &creds.account_id) {
                details.turn_state_action = "discarded_stale_account".into();
                self.record_fetch(started, details).await;
                return Err(FetchOnceError::new(format!("[{model}] 登录账号已变化，丢弃旧账号票据结果"), FetchRetryClass::Stale));
            }
            match result {
                Ok(token) => {
                    if self.fetch_generation.load(Ordering::SeqCst) != generation {
                        details.turn_state_action = "discarded_stale_config".into();
                        self.record_fetch(started, details).await;
                        let message = format!("[{model}] 配置已变化，丢弃旧线路返回的票据");
                        return Err(FetchOnceError::new(message, FetchRetryClass::Stale));
                    }
                    let ready = {
                        let mut store = self.turn_state.lock().await;
                        if self.fetch_generation.load(Ordering::SeqCst) != generation
                            || !store.is_bound_to_account(&creds.account_id)
                        {
                            None
                        } else {
                            store.record_distribution(
                                model,
                                vec![turn_state::TokenLenCount {
                                    len: token.len(),
                                    count: 1,
                                }],
                            );
                            Some(capture_fetched_ticket(&mut store, model, &token))
                        }
                    };
                    let Some(ready) = ready else {
                        details.turn_state_action = "discarded_stale_config".into();
                        self.record_fetch(started, details).await;
                        let message = format!("[{model}] 配置已变化，丢弃旧线路返回的票据");
                        return Err(FetchOnceError::new(message, FetchRetryClass::Stale));
                    };
                    self.defer_next_fetch(fetch::RETRY_INTERVAL).await;
                    if !ready {
                        details.turn_state_action = "pooled_unmatched".into();
                        self.record_fetch(started, details).await;
                        let message = format!(
                            "[{model}] 采到 {} 字节票据，但未匹配请求开始时的目标长度 {target_len}",
                            token.len()
                        );
                        *self.fetch_error.lock().await = Some(message.clone());
                        return Err(FetchOnceError::new(message, FetchRetryClass::Normal));
                    }
                    details.turn_state_action = "captured".into();
                    self.record_fetch(started, details).await;
                    *self.fetch_error.lock().await = None;
                    *self.fetch_ok_at.lock().await = Some(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    );
                    return Ok(token);
                }
                Err(err) => {
                    if self.fetch_generation.load(Ordering::SeqCst) != generation {
                        details.turn_state_action = "discarded_stale_config".into();
                        self.record_fetch(started, details).await;
                        let message = format!("[{model}] 配置已变化，忽略旧线路请求错误");
                        return Err(FetchOnceError::new(message, FetchRetryClass::Stale));
                    }
                    let error_kind = details.error_kind.clone();
                    let retry_class = classify_fetch_failure(&details);
                    if let Some(len) = details.returned_turn_state_len {
                        self.turn_state.lock().await.record_distribution(
                            model,
                            vec![turn_state::TokenLenCount { len, count: 1 }],
                        );
                    }
                    self.record_fetch(started, details).await;
                    let message = format!("{err:#}");
                    eprintln!("[{model}] turn-state fetch failed: {message}");
                    *self.fetch_error.lock().await = Some(message.clone());

                    let connect = error_kind.as_deref() == Some("connect");
                    if connect && attempt < fetch::CONNECT_ATTEMPTS {
                        self.defer_next_fetch(fetch::CONNECT_RETRY_INTERVAL).await;
                        continue;
                    }
                    let final_class = if connect {
                        FetchRetryClass::Backoff
                    } else {
                        retry_class
                    };
                    self.defer_next_fetch(fetch_retry_delay(final_class)).await;
                    return Err(FetchOnceError::new(message, final_class));
                }
            }
        }
        unreachable!("connect retry loop always returns")
    }

    async fn refresh_if_needed(&self) -> Duration {
        let saved = self.settings.lock().await.clone();
        let settings = match self.fetch_settings(&saved) {
            Ok(settings) => settings,
            Err(err) => {
                *self.fetch_error.lock().await = Some(err.to_string());
                return Duration::from_secs(30);
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            *self.fetch_error.lock().await = Some("尚未登录 ChatGPT".into());
            return Duration::from_secs(30);
        }
        self.sync_logged_in_account().await;

        // 首次运行：注册 settings.models 中的种子模型
        if !self.seeds_registered.swap(true, Ordering::Relaxed) {
            let mut store = self.turn_state.lock().await;
            for model in &settings.models {
                if !model.is_empty() && store.register_model(model) {
                    eprintln!("[seed] 从设置注册种子模型: {}", model);
                }
            }
        }

        // 312 降智信号 → 清池（所有模型的 token，但保留追踪）
        if self.degraded.swap(false, Ordering::Relaxed) {
            eprintln!("312 降智 / 服务端拒绝信号，清池重打 292（所有模型）");
            self.turn_state.lock().await.invalidate_all();
            *self.degraded_at.lock().await = None;
        }

        // 获取所有活跃模型（最近 60 分钟内有请求的），检查哪些需要刷新
        let models_needing_refresh: Vec<String> = {
            let store = self.turn_state.lock().await;
            store
                .all_active_models()
                .into_iter()
                .filter(|m| store.needs_refresh(m))
                .collect()
        };

        if models_needing_refresh.is_empty() {
            return fetch::CHECK_INTERVAL;
        }

        let round = self.fetch_round.fetch_add(1, Ordering::Relaxed) + 1;
        let model = model_for_fetch_round(&models_needing_refresh, round)
            .expect("models_needing_refresh is not empty");
        let bound_len = self.turn_state.lock().await.bound_len_for(model);
        *self.fetch_error.lock().await = Some(format!(
            "正在单发获取 {} 的 {} Token（第 {} 轮）…",
            model, bound_len, round
        ));
        debug_log(&format!(
            "[{}] 需要新 token，第 {} 轮单发获取，目标长度 {}",
            model, round, bound_len
        ));

        match self.fetch_once(model).await {
            Ok(token) => {
                debug_log(&format!("✅ [{}] 单发命中 {} 字节票据", model, token.len()));
                let remaining = {
                    let store = self.turn_state.lock().await;
                    store
                        .all_active_models()
                        .into_iter()
                        .any(|active| store.needs_refresh(&active))
                };
                if remaining {
                    fetch::RETRY_INTERVAL
                } else {
                    fetch::CHECK_INTERVAL
                }
            }
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!("[{}] 单发未命中: {message}", model);
                if err.retry != FetchRetryClass::Stale {
                    *self.fetch_error.lock().await = Some(message.clone());
                }
                fetch_retry_delay(err.retry)
            }
        }
    }

    async fn record(
        &self,
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) {
        let entry = LogEntry::new(method, path, status, started, details);
        let mut logs = self.logs.lock().await;
        logs::push(&mut logs, entry);
    }

    async fn record_fetch(&self, started: Instant, details: NetworkLogDetails) {
        let status = details.response_status.unwrap_or(502);
        self.record("POST", "/responses", status, started, details)
            .await;
    }
}

#[derive(Clone)]
pub struct ProxyHandle {
    app: Arc<App>,
    stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    fetch_stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    fetch_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    settings_change: Arc<Mutex<()>>,
    managed_routes: Arc<std::sync::Mutex<Option<attach::ManagedRoutes>>>,
    attach_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl ProxyHandle {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            stop: Arc::new(Mutex::new(None)),
            task: Arc::new(Mutex::new(None)),
            fetch_stop: Arc::new(Mutex::new(None)),
            fetch_task: Arc::new(Mutex::new(None)),
            settings_change: Arc::new(Mutex::new(())),
            managed_routes: Arc::new(std::sync::Mutex::new(None)),
            attach_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn app(&self) -> Arc<App> {
        self.app.clone()
    }

    pub fn enable_auto_attach(&self) {
        *self.managed_routes.lock().expect("managed routes") = Some(attach::ManagedRoutes::new(attach::backup_path()));
    }

    pub fn restore_managed_routes(&self) -> Result<()> {
        if let Some(routes) = self.managed_routes.lock().expect("managed routes").as_mut() {
            routes.shutdown()?;
        }
        Ok(())
    }

    fn sync_routes_to(&self, settings: &Settings) -> Result<()> {
        let result = match self.managed_routes.lock().expect("managed routes").as_mut() {
            Some(routes) => routes.sync(settings, self.app.proxy_ok.load(Ordering::Relaxed)),
            None => Ok(()),
        };
        *self.attach_error.lock().expect("attach error") = result.as_ref().err().map(|err| format!("自动接入失败：{err:#}"));
        result
    }

    pub async fn managed_status(&self) -> Status {
        let mut status = self.app.status().await;
        status.attach_error = self.attach_error.lock().expect("attach error").clone();
        status
    }

    pub fn core(&self) -> &App {
        &self.app
    }

    pub async fn run_attachment_supervisor(&self) {
        loop {
            {
                let _change = self.settings_change.lock().await;
                let settings = self.app.settings.lock().await.clone();
                let _ = self.sync_routes_to(&settings);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn start_managed(&self) -> Result<()> {
        let _change = self.settings_change.lock().await;
        self.start().await
    }

    pub async fn start(&self) -> Result<()> {
        self.stop().await;
        let listen = self.app.settings.lock().await.proxy_listen.clone();
        let addr: SocketAddr = listen.parse().context("proxy_listen")?;
        let listener = match bind_listen(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                self.app.proxy_ok.store(false, Ordering::Relaxed);
                let in_use = err.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
                }) || format!("{err:#}").contains("Address already in use");
                let message = if in_use {
                    format!("{addr} 已被占用，无法启动代理。请先关掉旧的 Codex State Kit 再试。")
                } else {
                    format!("无法绑定 {addr}: {err:#}")
                };
                *self.app.proxy_error.lock().await = Some(message);
                self.start_fetch_loop().await;
                return Err(err);
            }
        };
        *self.app.proxy_error.lock().await = None;
        self.restore_leftover().await;
        let (tx, rx) = oneshot::channel();
        *self.stop.lock().await = Some(tx);
        let app = self.app.clone();
        app.proxy_ok.store(true, Ordering::Relaxed);
        println!("proxy  http://{addr}  (point Codex openai_base_url here)");
        let task_app = app.clone();
        let handle = tokio::spawn(async move {
            let router = axum::Router::new()
                .fallback(proxy)
                .with_state(task_app.clone());
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
            task_app.proxy_ok.store(false, Ordering::Relaxed);
            if let Err(err) = result {
                eprintln!("proxy stopped: {err}");
            }
        });
        *self.task.lock().await = Some(handle);
        let settings = self.app.settings.lock().await.clone();
        let _ = self.sync_routes_to(&settings);
        self.start_fetch_loop().await;
        Ok(())
    }

    async fn start_fetch_loop(&self) {
        self.stop_fetch_loop().await;
        let (tx, mut rx) = oneshot::channel();
        *self.fetch_stop.lock().await = Some(tx);
        let app = self.app.clone();
        let handle = tokio::spawn(async move {
            loop {
                let wait = app.refresh_if_needed().await;
                tokio::select! {
                    _ = &mut rx => break,
                    _ = tokio::time::sleep(wait) => {}
                    _ = app.degrade_notify.notified() => {
                        eprintln!("312 信号唤醒 fetch 循环，立即续期");
                    }
                    _ = app.model_notify.notified() => {
                        eprintln!("新模型发现，唤醒 fetch 循环");
                    }
                }
            }
        });
        *self.fetch_task.lock().await = Some(handle);
    }

    async fn stop_fetch_loop(&self) {
        if let Some(tx) = self.fetch_stop.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.fetch_task.lock().await.take() {
            // Cancel the old route's in-flight fetches before a mode switch.
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn restore_leftover(&self) {
        if self.app.leftover_restored.swap(true, Ordering::SeqCst) {
            return;
        }
        let home = self.app.settings.lock().await.codex_home.clone();
        match crate::attach::restore_codex_config(Path::new(&home)) {
            Ok(msg) if msg != "nothing to restore" => {
                println!("restored leftover Codex config: {msg}");
            }
            Err(err) => eprintln!("failed to restore leftover Codex config: {err:#}"),
            _ => {}
        }
    }

    pub async fn stop(&self) {
        self.stop_fetch_loop().await;
        if let Some(tx) = self.stop.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
        self.app.proxy_ok.store(false, Ordering::Relaxed);
    }

    pub async fn apply_settings(&self, patch: SettingsPatch) -> Result<Status> {
        let next = patch.into_settings()?;
        let _change = if next.outbound_mode == OutboundMode::Manual {
            loop {
                self.app.warp.cancel_connect();
                tokio::select! {
                    guard = self.settings_change.lock() => break guard,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
            }
        } else {
            self.settings_change.lock().await
        };
        let old = self.app.settings.lock().await.clone();
        let next_http = if old.upstream_proxy != next.upstream_proxy {
            Some(upstream_http_client(&next.upstream_proxy)?)
        } else {
            None
        };
        if old.codex_home != next.codex_home {
            attach::validate_codex_home(Path::new(&next.codex_home))?;
        }
        self.sync_routes_to(&next)?;
        if let Err(err) = save_settings(&next) {
            self.sync_routes_to(&old)?;
            return Err(err);
        }
        let route_changed = old.outbound_proxy != next.outbound_proxy
            || old.outbound_mode != next.outbound_mode
            || old.warp_http2 != next.warp_http2
            || old.upstream != next.upstream
            || old.codex_home != next.codex_home;
        let mut fetch_transition_guard = None;
        let mut fetch_change_guard = None;
        if route_changed {
            fetch_transition_guard = Some(self.app.fetch_transition.lock().await);
            self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
            self.app.fetch_change_notify.notify_waiters();
            self.stop_fetch_loop().await;
            fetch_change_guard = Some(self.app.fetch_gate.lock().await);
            self.app.turn_state.lock().await.invalidate_all();
            *self.app.fetch_error.lock().await = None;
            *self.app.fetch_ok_at.lock().await = None;
            self.app.degraded.store(false, Ordering::Relaxed);
            *self.app.degraded_at.lock().await = None;
            if next.outbound_mode == OutboundMode::Manual || old.warp_http2 != next.warp_http2 {
                self.app.warp.stop().await;
            }
        }
        {
            let mut settings = self.app.settings.lock().await;
            *settings = next.clone();
            if let Some(http) = next_http {
                *self.app.http.lock().await = http;
            }
        }
        if route_changed {
            self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
            *self.app.fetch_next_allowed_at.lock().await = Instant::now();
            self.app.fetch_change_notify.notify_waiters();
            drop(fetch_change_guard.take());
            drop(fetch_transition_guard.take());
        }
        if old.proxy_listen != next.proxy_listen {
            if let Err(err) = self.start().await {
                {
                    let mut settings = self.app.settings.lock().await;
                    settings.proxy_listen = old.proxy_listen.clone();
                    let _ = save_settings(&settings);
                }
                let _ = self.start().await;
                return Err(err);
            }
            // Managed routes are already synchronized by start(), under their exit lock.
            if self.managed_routes.lock().expect("managed routes").is_none() {
                attach::update_attached_base_url(&next)?;
            }
        } else if route_changed {
            self.start_fetch_loop().await;
        }
        self.app.warp_wake.notify_one();
        Ok(self.managed_status().await)
    }

    pub async fn run_warp_supervisor(&self) {
        loop {
            let mode = self.app.settings.lock().await.outbound_mode;
            let mut wait = Duration::from_secs(20);
            if mode == OutboundMode::Warp {
                let phase = self.app.warp.status().phase;
                if matches!(phase.as_str(), "stopped" | "error") {
                    if let Err(err) = self.connect_warp(true).await {
                        eprintln!("embedded WARP: {err:#}");
                        wait = Duration::from_secs(60);
                    }
                } else {
                    self.app.warp.check_health().await;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = self.app.warp_wake.notified() => {},
            }
        }
    }

    pub async fn connect_warp(&self, accept_terms: bool) -> Result<Status> {
        let _change = self.settings_change.lock().await;
        let settings = self.app.settings.lock().await.clone();
        if settings.outbound_mode != OutboundMode::Warp {
            anyhow::bail!("请先选择内置 WARP 模式");
        }
        let fetch_transition = self.app.fetch_transition.lock().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.fetch_change_notify.notify_waiters();
        self.stop_fetch_loop().await;
        let fetch_change = self.app.fetch_gate.lock().await;
        let result = self
            .app
            .warp
            .connect(accept_terms, settings.warp_http2)
            .await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        *self.app.fetch_next_allowed_at.lock().await = Instant::now();
        self.app.fetch_change_notify.notify_waiters();
        *self.app.fetch_error.lock().await = None;
        *self.app.fetch_ok_at.lock().await = None;
        drop(fetch_change);
        drop(fetch_transition);
        self.start_fetch_loop().await;
        result?;
        Ok(self.app.status().await)
    }

    pub async fn stop_warp(&self) -> Status {
        let _change = self.settings_change.lock().await;
        let fetch_transition = self.app.fetch_transition.lock().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.fetch_change_notify.notify_waiters();
        self.stop_fetch_loop().await;
        let fetch_change = self.app.fetch_gate.lock().await;
        self.app.warp.stop().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        *self.app.fetch_next_allowed_at.lock().await = Instant::now();
        self.app.fetch_change_notify.notify_waiters();
        *self.app.fetch_error.lock().await = None;
        *self.app.fetch_ok_at.lock().await = None;
        drop(fetch_change);
        drop(fetch_transition);
        self.start_fetch_loop().await;
        self.app.status().await
    }
}

async fn bind_listen(addr: SocketAddr) -> Result<TcpListener> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => return Err(err).with_context(|| format!("bind {addr}")),
        }
    }
}

async fn proxy(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    if is_websocket(&req) {
        // WebSocket 升级需要 Cloudflare cookie（由 Codex 客户端维护），
        // 代理自建的连接没有 cookie 会被 Cloudflare 403 拒绝。
        // 返回 426 Upgrade Required —— 官方 Codex 客户端检测到此状态码后
        // 会自动永久切换到 HTTP SSE 流式传输（见 client.rs FallbackToHttp 逻辑）。
        eprintln!("[ws] 拒绝 WS 升级（无 Cloudflare cookie），返回 426 触发客户端回退到 HTTP SSE");
        return (StatusCode::UPGRADE_REQUIRED, "WebSocket not supported by proxy, use HTTP SSE").into_response();
    }
    proxy_http(app, req).await
}

fn is_websocket(req: &Request<Body>) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

async fn proxy_http(app: Arc<App>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = logs::safe_text(req.uri().path(), 256);
    let mut details = NetworkLogDetails::default();
    let mut activity = None;
    match forward_http_tracked(&app, req, &mut details, &mut activity).await {
        Ok(resp) => {
            details.response_header_ms = Some(started.elapsed().as_millis());
            details.response_content_encoding = Some(logs::safe_content_encoding(
                resp.headers().get(header::CONTENT_ENCODING).and_then(|value| value.to_str().ok()),
            ));
            if method == http::Method::HEAD
                || matches!(resp.status().as_u16(), 204 | 304)
                || resp
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .is_some_and(|value| value == "0")
            {
                app.record(
                    method.as_str(),
                    &path,
                    resp.status().as_u16(),
                    started,
                    details,
                )
                .await;
                return resp;
            }
            details.in_progress = true;
            // Some Codex upstream responses omit Content-Type despite sending
            // SSE. Retain the request's explicit SSE negotiation in that case.
            let is_sse = details.transport == "http_sse" || resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                });
            if is_sse {
                details.transport = "http_sse".into();
            }
            let metrics = logs::ResponseBodyMetrics::new(resp
                .headers()
                .get(header::CONTENT_ENCODING)
                .map(|value| value.to_str().unwrap_or("unsupported"))
                .unwrap_or_default());
            let remaining_bytes = resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let entry = LogEntry::new(
                method.as_str(),
                &path,
                resp.status().as_u16(),
                started,
                details,
            );
            logs::push(&mut *app.logs.lock().await, entry.clone());
            let tracker = ResponseLogTracker {
                app,
                entry,
                started,
                metrics,
                finished: false,
                remaining_bytes,
                activity,
            };
            let (parts, body) = resp.into_parts();
            let stream = futures_util::stream::unfold(
                (body.into_data_stream(), tracker),
                move |(mut stream, mut tracker)| async move {
                    if tracker.finished {
                        return None;
                    }
                    let next = stream.next().await;
                    match &next {
                        Some(Ok(bytes)) => {
                            tracker.metrics.observe(
                                bytes,
                                tracker.started.elapsed().as_millis(),
                                is_sse,
                            );
                            // Hyper may stop polling immediately after Content-Length
                            // bytes. That is completion, not client cancellation.
                            if let Some(remaining) = &mut tracker.remaining_bytes {
                                *remaining = remaining.saturating_sub(bytes.len() as u64);
                                if *remaining == 0 {
                                    tracker
                                        .metrics
                                        .finish(tracker.started.elapsed().as_millis());
                                    tracker.finished = true;
                                }
                            }
                        }
                        Some(Err(_)) => {
                            tracker.entry.error_kind = Some("response_body".into());
                            tracker.finished = true;
                        }
                        None => {
                            tracker
                                .metrics
                                .finish(tracker.started.elapsed().as_millis());
                            tracker.finished = true;
                        }
                    }
                    if tracker.finished {
                        tracker.activity.take();
                    }
                    // No read-ahead or buffering for forwarding. Only publish metric changes
                    // and the final result; response bytes are passed through unchanged.
                    if tracker.finished
                        || tracker.entry.first_token_ms != tracker.metrics.first_token_ms()
                        || tracker.entry.output_tokens != tracker.metrics.output_tokens()
                        || tracker.entry.error_kind.as_deref() != tracker.metrics.error_kind
                    {
                        tracker.refresh();
                        tracker.publish().await;
                    }
                    next.map(|item| (item, (stream, tracker)))
                },
            );
            Response::from_parts(parts, Body::from_stream(stream))
        }
        Err(err) => {
            app.record(method.as_str(), &path, 502, started, details)
                .await;
            (StatusCode::BAD_GATEWAY, err.to_string()).into_response()
        }
    }
}

fn fetch_account_is_current(settings: &Settings, account: &str) -> bool {
    login::chatgpt_credentials(Path::new(&settings.codex_home))
        .is_ok_and(|creds| creds.account_id == account)
}

struct ResponseLogTracker {
    app: Arc<App>,
    entry: LogEntry,
    started: Instant,
    metrics: logs::ResponseBodyMetrics,
    finished: bool,
    remaining_bytes: Option<u64>,
    activity: Option<RequestActivity>,
}

impl ResponseLogTracker {
    fn refresh(&mut self) {
        self.entry.ms = self.started.elapsed().as_millis();
        self.entry.first_token_ms = self.metrics.first_token_ms();
        self.entry.output_tokens = self.metrics.output_tokens();
        self.entry.in_progress = !self.finished;
        if self.entry.error_kind.is_none() {
            self.entry.error_kind = self.metrics.error_kind.map(str::to_owned);
        }
        self.entry.tokens_per_second = if self.finished && self.entry.error_kind.is_none() {
            self.entry
                .output_tokens
                .zip(self.entry.first_token_ms)
                .and_then(|(tokens, first)| {
                    let generation_ms = self.entry.ms.saturating_sub(first);
                    (generation_ms > 0).then(|| tokens as f64 * 1000.0 / generation_ms as f64)
                })
        } else {
            None
        };
    }

    async fn publish(&self) {
        replace_network_log(&self.app, self.entry.clone()).await;
    }
}

async fn replace_network_log(app: &App, entry: LogEntry) {
    if let Some(existing) = app
        .logs
        .lock()
        .await
        .iter_mut()
        .find(|existing| existing.id == entry.id)
    {
        *existing = entry;
    }
}

impl Drop for ResponseLogTracker {
    fn drop(&mut self) {
        if !self.finished {
            if !self.metrics.completed() && self.metrics.error_kind.is_none() {
                self.entry.error_kind = Some("client_cancelled".into());
            }
            self.finished = true;
            self.refresh();
        }
        // A disconnected client drops the body without polling EOF. Preserve that
        // partial request too, without continuing to read the upstream response.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let app = self.app.clone();
            let entry = self.entry.clone();
            runtime.spawn(async move {
                replace_network_log(&app, entry).await;
            });
        }
    }
}

fn upstream_http_client(proxy: &str) -> Result<reqwest::Client> {
    let proxy = crate::settings::normalize_proxy(proxy, "上游转发代理")?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5));
    if !proxy.is_empty() {
        let proxy = reqwest::Proxy::all(fetch::outbound_proxy_for_client(&proxy))
            .map_err(|_| anyhow::anyhow!("上游转发代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建上游转发客户端"))
}

#[cfg(test)]
async fn forward_http(app: &App, req: Request<Body>) -> Result<Response> {
    let mut details = NetworkLogDetails::default();
    forward_http_with_log(app, req, &mut details).await
}

#[cfg(test)]
async fn forward_http_with_log(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
) -> Result<Response> {
    forward_http_tracked(app, req, details, &mut None).await
}

async fn forward_http_tracked(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
    activity: &mut Option<RequestActivity>,
) -> Result<Response> {
    let (upstream, home, upstream_proxy, http) = {
        let settings = app.settings.lock().await;
        (
            settings.upstream.clone(),
            settings.codex_home.clone(),
            settings.upstream_proxy.clone(),
            app.http.lock().await.clone(),
        )
    };
    let effective_proxy = fetch::outbound_proxy_for_client(&upstream_proxy);
    *details = logs::network_details(&upstream, &effective_proxy);
    let (mut parts, body) = req.into_parts();
    let target = join_upstream(&upstream, &parts.uri)?;
    let path = parts.uri.path();

    // 先读取 body，以便从中提取 model 字段
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .context("read body")?;
    details.body_bytes = bytes.len();

    let should_stamp = turn_state::should_stamp_http(parts.method.as_str(), path);
    let content_encoding = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok());
    details.content_encoding = logs::safe_content_encoding(content_encoding);
    details.transport = if parts
        .headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
    {
        "http_sse".into()
    } else {
        "http".into()
    };
    debug_log(&format!(
        "[proxy] {} {} body={}bytes encoding={} should_stamp={}",
        parts.method,
        path,
        bytes.len(),
        details.content_encoding,
        should_stamp
    ));

    let client_account = request_account(&parts.headers);
    let applied_account = login::apply_kit_auth_headers(&mut parts.headers, Path::new(&home));
    let effective_account = request_account(&parts.headers);
    let account_changed = applied_account.is_some() && client_account != effective_account;
    details.account_id = effective_account.as_deref().map(|id| logs::safe_text(id, 128));
    let account_snapshot = applied_account.unwrap_or_else(|| login::login_status(Path::new(&home)));
    if effective_account.is_some() && account_snapshot.account_id == effective_account {
        details.account_email = account_snapshot.email.map(|email| logs::safe_text(&email, 254));
    }

    let mut injected_token: Option<String> = None;
    if should_stamp {
        let request_model = turn_state::extract_model_from_body(&bytes);
        details.model = request_model
            .as_deref()
            .map(|model| logs::safe_text(model, 80))
            .filter(|model| !model.is_empty());
        if request_model.is_none() {
            debug_log(&format!(
                "[proxy] 未能解析 model，body={}bytes encoding={}",
                bytes.len(),
                details.content_encoding
            ));
        }

        // 被动发现：从请求中提取模型，自动注册到 token 池
        if let Some(ref model) = request_model {
            let is_new = app.turn_state.lock().await.register_model(model);
            if is_new {
                debug_log(&format!("[discover] 发现新模型: {}，通知 fetch 循环预取 token", model));
                app.model_notify.notify_one();
            }
        }

        let client_already_has = turn_state::has_http_turn_state(&parts.headers);
        if client_already_has {
            let store = app.turn_state.lock().await;
            // Match the immutable outgoing credential snapshot, not the latest UI account.
            let token = if effective_account.as_deref().is_some_and(|id| store.is_bound_to_account(id)) {
                request_model.as_deref().and_then(|model| store.peek_for_model(model))
            } else {
                // 无法识别模型时不注入，保留客户端原 token
                None
            };
            if let Some(token) = token {
                turn_state::apply_http_header(&mut parts.headers, &token);
                details.turn_state_action = "replaced".into();
                details.turn_state_len = Some(token.len());
                eprintln!(
                    "[stamp] 替换 turn_state → token len={} model={:?} 到 {} {}",
                    token.len(),
                    request_model,
                    parts.method,
                    path
                );
                injected_token = Some(token);
            } else if account_changed {
                parts.headers.remove(turn_state::HEADER_NAME);
                details.turn_state_action = "removed_account_mismatch".into();
            } else {
                details.turn_state_action = if request_model.is_some() {
                    "preserved_no_ticket".into()
                } else {
                    "preserved_unknown_model".into()
                };
                details.turn_state_len = parts
                    .headers
                    .get(turn_state::HEADER_NAME)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::len);
                eprintln!(
                    "[stamp] 无可用 token（model={:?}），保留客户端原值 {} {}",
                    request_model, parts.method, path
                );
            }
        } else {
            details.turn_state_action = "initial_request".into();
            eprintln!(
                "[stamp] 首次请求，不注入 turn_state（等服务端下发） {} {} model={:?}",
                parts.method, path, request_model
            );
        }
    } else {
        details.turn_state_action = "not_applicable".into();
    }
    let mut builder = http
        .request(
            reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?,
            target,
        )
        .body(bytes);
    for (name, value) in &parts.headers {
        if is_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(account) = parts
        .headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        *activity = Some(app.traffic.begin(account, Instant::now()));
    }
    let upstream_resp = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            details.error_kind = Some(logs::request_error_kind(&error));
            return Err(error).context("upstream http");
        }
    };
    let resp_status_u16 = upstream_resp.status().as_u16();
    details.peer_addr = upstream_resp.remote_addr().map(|addr| addr.to_string());
    details.final_origin = Some(logs::endpoint_origin(upstream_resp.url().as_str()));
    details.http_version = Some(
        match upstream_resp.version() {
            Version::HTTP_09 => "HTTP/0.9",
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2",
            Version::HTTP_3 => "HTTP/3",
            _ => "HTTP/unknown",
        }
        .into(),
    );

    // 记录上游响应详情，方便排查 token 失效
    let upstream_turn_state = turn_state::header_token(upstream_resp.headers());
    details.returned_turn_state_len = upstream_turn_state.as_ref().map(|token| token.len());
    let injected_len = injected_token.as_ref().map(|t| t.len());
    let upstream_ts_len = upstream_turn_state.as_ref().map(|t| t.len());
    let same_token = match (&injected_token, &upstream_turn_state) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    eprintln!(
        "[resp] {} {} → {} | 注入={}字节 上游返回={}字节 same={}",
        parts.method,
        path,
        resp_status_u16,
        injected_len
            .map(|l| l.to_string())
            .unwrap_or_else(|| "无".into()),
        upstream_ts_len
            .map(|l| l.to_string())
            .unwrap_or_else(|| "无".into()),
        same_token
    );

    let status = StatusCode::from_u16(resp_status_u16)?;
    let mut headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if is_hop(name) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_ref()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(n, v);
        }
    }
    let stream = upstream_resp.bytes_stream();
    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

// WebSocket 代理已移除 — 所有 WS 升级请求在 proxy() 入口返回 426，
// 触发 Codex CLI 自动切换到 HTTP SSE 模式。
// 这保证了所有请求都经过 proxy_http()，可以可靠地提取 model 并注入对应 token。

fn is_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

fn request_account(headers: &HeaderMap) -> Option<String> {
    headers.get("chatgpt-account-id").and_then(|value| value.to_str().ok())
        .map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

pub fn join_upstream(upstream: &str, uri: &Uri) -> Result<String> {
    let mut base = upstream.trim().to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    let mut url = Url::parse(&base).context("upstream url")?;
    let path = uri.path().trim_start_matches('/');
    url = url.join(path).context("join path")?;
    url.set_query(uri.query());
    Ok(url.to_string())
}

#[cfg(test)]
#[path = "upstream_proxy_tests.rs"]
mod upstream_proxy_tests;

#[cfg(test)]
#[path = "account_switch_tests.rs"]
mod account_switch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn ticket_for_len(target_len: usize) -> String {
        let mut raw = vec![0_u8; target_len * 3 / 4];
        raw[0] = 0x80;
        raw[1..9].copy_from_slice(&(chrono::Utc::now().timestamp() as u64).to_be_bytes());
        URL_SAFE_NO_PAD.encode(raw)
    }

    #[test]
    fn warp_route_never_falls_back_to_saved_manual_proxy() {
        let settings = Settings {
            outbound_mode: OutboundMode::Warp,
            outbound_proxy: "http://127.0.0.1:7890".into(),
            ..Settings::default()
        };
        let app = App::new(settings.clone()).unwrap();
        assert!(app.fetch_settings(&settings).is_err());
        let mut manual = settings;
        manual.outbound_mode = OutboundMode::Manual;
        assert_eq!(
            app.fetch_settings(&manual).unwrap().outbound_proxy,
            "http://127.0.0.1:7890"
        );
    }

    #[test]
    fn joins_path_and_query() {
        let uri: Uri = "http://127.0.0.1:8787/responses?foo=1".parse().unwrap();
        let out = join_upstream("https://chatgpt.com/backend-api/codex", &uri).unwrap();
        assert_eq!(out, "https://chatgpt.com/backend-api/codex/responses?foo=1");
    }

    #[test]
    fn joins_nested_path() {
        let uri: Uri = "http://127.0.0.1:8787/v1/responses".parse().unwrap();
        let out = join_upstream("https://chatgpt.com/backend-api/codex/", &uri).unwrap();
        assert_eq!(out, "https://chatgpt.com/backend-api/codex/v1/responses");
    }

    #[test]
    fn fetch_round_rotates_models_without_starvation() {
        let models = vec!["astra".to_string(), "sol".to_string(), "other".to_string()];
        let selected: Vec<_> = (1..=5)
            .map(|round| model_for_fetch_round(&models, round).unwrap())
            .collect();
        assert_eq!(selected, vec!["astra", "sol", "other", "astra", "sol"]);
        assert_eq!(model_for_fetch_round(&[], 1), None);
    }

    #[test]
    fn fetch_errors_never_retry_without_delay() {
        assert_eq!(fetch_retry_delay(FetchRetryClass::Normal), fetch::RETRY_INTERVAL);
        assert_eq!(fetch_retry_delay(FetchRetryClass::Backoff), fetch::ERROR_BACKOFF);
        assert_eq!(fetch_retry_delay(FetchRetryClass::Auth), fetch::AUTH_BACKOFF);
        assert_eq!(fetch_retry_delay(FetchRetryClass::Stale), fetch::RETRY_INTERVAL);
        assert!(fetch::RETRY_INTERVAL >= Duration::from_secs(6));
        assert_eq!(fetch::CONNECT_ATTEMPTS, 4);
        assert!(fetch::CONNECT_RETRY_INTERVAL >= Duration::from_secs(6));
    }

    #[test]
    fn fetch_failure_class_uses_structured_status_and_error_kind() {
        for status in [401, 403] {
            let details = NetworkLogDetails {
                response_status: Some(status),
                ..NetworkLogDetails::default()
            };
            assert_eq!(classify_fetch_failure(&details), FetchRetryClass::Auth);
        }
        for status in [429, 503] {
            let details = NetworkLogDetails {
                response_status: Some(status),
                ..NetworkLogDetails::default()
            };
            assert_eq!(classify_fetch_failure(&details), FetchRetryClass::Backoff);
        }
        let connect = NetworkLogDetails {
            error_kind: Some("connect".into()),
            ..NetworkLogDetails::default()
        };
        assert_eq!(classify_fetch_failure(&connect), FetchRetryClass::Backoff);
        assert_eq!(
            classify_fetch_failure(&NetworkLogDetails::default()),
            FetchRetryClass::Normal
        );
    }

    #[test]
    fn fetched_ticket_must_match_the_models_exact_bound_length() {
        let mut store = TurnStateStore::default();
        store.set_model_bound_len("astra", Some(turn_state::QUALITY_TOKEN_LEN));
        let original_292 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        assert!(capture_fetched_ticket(&mut store, "astra", &original_292));
        let token_332 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN_332);
        assert!(!capture_fetched_ticket(&mut store, "astra", &token_332));
        assert_eq!(
            store.peek_for_model("astra").as_deref(),
            Some(original_292.as_str())
        );

        let token_292 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        assert!(capture_fetched_ticket(&mut store, "astra", &token_292));
        assert_eq!(store.peek_for_model("astra").as_deref(), Some(token_292.as_str()));

        let mut auto = TurnStateStore::default();
        assert!(capture_fetched_ticket(&mut auto, "astra", &token_332));
        assert_eq!(auto.bound_len_for("astra"), turn_state::QUALITY_TOKEN_LEN_332);
    }

    #[tokio::test]
    async fn shared_fetch_slot_enforces_the_configured_delay() {
        let app = App::new(Settings::default()).unwrap();
        app.defer_next_fetch(Duration::from_millis(30)).await;
        let started = Instant::now();
        let generation = app.fetch_generation.load(Ordering::SeqCst);
        assert!(app.wait_for_fetch_slot(generation).await);
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn generation_change_interrupts_a_waiting_fetch_slot() {
        let app = Arc::new(App::new(Settings::default()).unwrap());
        app.defer_next_fetch(Duration::from_secs(5)).await;
        let generation = app.fetch_generation.load(Ordering::SeqCst);
        let waiter = {
            let app = app.clone();
            tokio::spawn(async move { app.wait_for_fetch_slot(generation).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        app.fetch_generation.fetch_add(2, Ordering::SeqCst);
        app.fetch_change_notify.notify_waiters();
        assert!(!waiter.await.unwrap());
    }
}
