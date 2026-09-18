use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http::header::SEC_WEBSOCKET_PROTOCOL;
use serde::Serialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::MaybeTlsStream;
use url::Url;

use crate::attach::{self, is_attached};
use crate::fetch;
use crate::login::{self, has_chatgpt_login};
use crate::logs::{self, LogEntry};
use crate::settings::{save_settings, OutboundMode, Settings, SettingsPatch};
use crate::turn_state::{self, TurnStateStore, TurnStateView};
use crate::warp::{WarpRuntime, WarpStatus};

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
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub warp: WarpStatus,
    pub fetch_error: Option<String>,
    pub fetch_ok_at: Option<String>,
    pub turn_state: TurnStateView,
    pub degraded: bool,
    pub degraded_at: Option<String>,
    pub logs: Vec<LogEntry>,
}

pub struct App {
    pub warp: WarpRuntime,
    pub settings: Mutex<Settings>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    pub proxy_ok: AtomicBool,
    pub login_http: reqwest::Client,
    leftover_restored: AtomicBool,
    proxy_error: Mutex<Option<String>>,
    fetch_error: Mutex<Option<String>>,
    fetch_ok_at: Mutex<Option<String>>,
    fetch_round: AtomicU32,
    turn_state: Mutex<TurnStateStore>,
    http: reqwest::Client,
    degraded: AtomicBool,
    degraded_at: Mutex<Option<String>>,
    pub degrade_notify: Notify,
    pub warp_wake: Notify,
}

impl App {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_warp(settings, WarpRuntime::default())
    }

    pub fn with_warp(settings: Settings, warp: WarpRuntime) -> Result<Self> {
        Ok(Self {
            warp,
            settings: Mutex::new(settings),
            logs: Mutex::new(VecDeque::with_capacity(80)),
            proxy_ok: AtomicBool::new(false),
            login_http: crate::login::http_client()?,
            leftover_restored: AtomicBool::new(false),
            proxy_error: Mutex::new(None),
            fetch_error: Mutex::new(None),
            fetch_ok_at: Mutex::new(None),
            fetch_round: AtomicU32::new(0),
            turn_state: Mutex::new(TurnStateStore::load()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()?,
            degraded: AtomicBool::new(false),
            degraded_at: Mutex::new(None),
            degrade_notify: Notify::new(),
            warp_wake: Notify::new(),
        })
    }

    pub async fn status(&self) -> Status {
        let settings = self.settings.lock().await.clone();
        let logs = self.logs.lock().await.iter().cloned().collect();
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
            outbound_mode: settings.outbound_mode,
            warp_http2: settings.warp_http2,
            warp: self.warp.status(),
            fetch_error: self.fetch_error.lock().await.clone(),
            fetch_ok_at: self.fetch_ok_at.lock().await.clone(),
            turn_state: self.turn_state.lock().await.view(),
            degraded: self.degraded.load(Ordering::Relaxed),
            degraded_at: self.degraded_at.lock().await.clone(),
            logs,
        }
    }

    pub async fn refresh_turn_state(&self) -> Result<Status> {
        let settings = self.settings.lock().await.clone();
        self.fetch_once(&settings).await?;
        Ok(self.status().await)
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

    async fn fetch_once(&self, settings: &Settings) -> Result<String> {
        let effective = self.fetch_settings(settings).map_err(|err| err.to_string());
        let settings = match effective {
            Ok(settings) => settings,
            Err(message) => {
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            let message = "尚未登录 ChatGPT".to_string();
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        let creds = match login::chatgpt_credentials(Path::new(&settings.codex_home)) {
            Ok(creds) => creds,
            Err(err) => {
                let message = format!("{err:#}");
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        let client = match fetch::http_client(&settings.outbound_proxy) {
            Ok(client) => client,
            Err(err) => {
                let message = format!("{err:#}");
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        let token = match fetch::fetch_turn_state(&client, &settings, &creds).await {
            Ok(token) => token,
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!("turn-state fetch failed: {message}");
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        if turn_state::TurnState::from_token(&token, "fetch").is_none() {
            let message = "上游 token 无法解析".to_string();
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        // 312 长度 token 是降智 token，不入池，视为采集失败
        if turn_state::is_degraded_token(&token) {
            let message = format!("采到 312 token（{}字节），已丢弃，等待重试", token.len());
            eprintln!("⚠ {message}");
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        self.turn_state.lock().await.capture(&token, "fetch");
        *self.fetch_error.lock().await = None;
        *self.fetch_ok_at.lock().await =
            Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        Ok(token)
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

        // 312 降智信号 → 清池
        if self.degraded.swap(false, Ordering::Relaxed) {
            eprintln!("312 降智 / 服务端拒绝信号，清池重打 292");
            self.turn_state.lock().await.invalidate_all();
            *self.degraded_at.lock().await = None;
        }

        // 有可用 292 token → 什么都不做，一直用到服务端拒绝
        if self.turn_state.lock().await.peek_freshest().is_some() {
            return fetch::CHECK_INTERVAL;
        }

        // 没有可用 292 token → 并发打，拿到即停
        const CONCURRENCY: usize = 10;

        self.fetch_round
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let round = self.fetch_round.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("池中无 292 token，第 {} 轮并发 {} 路打...", round, CONCURRENCY);
        *self.fetch_error.lock().await =
            Some(format!("正在获取 292 Token（第 {} 轮，{} 路并发）…", round, CONCURRENCY));

        let mut handles = tokio::task::JoinSet::new();
        for _ in 0..CONCURRENCY {
            let s = settings.clone();
            let rotated = s.outbound_proxy.clone();
            let client = match fetch::http_client(&rotated) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let creds = match login::chatgpt_credentials(Path::new(&s.codex_home)) {
                Ok(c) => c,
                Err(_) => continue,
            };
            handles.spawn(async move { fetch::fetch_turn_state(&client, &s, &creds).await });
        }

        let mut got_292 = false;
        while let Some(result) = handles.join_next().await {
            if let Ok(Ok(token)) = result {
                if !turn_state::is_degraded_token(&token) {
                    if turn_state::TurnState::from_token(&token, "fetch").is_some() {
                        self.turn_state.lock().await.capture(&token, "fetch");
                        *self.fetch_error.lock().await = None;
                        *self.fetch_ok_at.lock().await = Some(
                            chrono::Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        );
                        eprintln!("✅ 拿到 292 token（{}字节），复用中", token.len());
                        got_292 = true;
                        handles.abort_all();
                        break;
                    }
                } else {
                    eprintln!("⚠ 并发拿到 312 token（{}字节），丢弃", token.len());
                }
            }
        }

        if got_292 {
            fetch::CHECK_INTERVAL
        } else {
            Duration::ZERO
        }
    }

    pub(crate) fn signal_degradation(&self) {
        self.degraded.store(true, Ordering::Relaxed);
        self.degrade_notify.notify_one();
    }

    async fn record(&self, method: &str, path: &str, status: u16, started: Instant) {
        let entry = LogEntry::new(method, path, status, started);
        let mut logs = self.logs.lock().await;
        logs::push(&mut logs, entry);
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
        if route_changed {
            self.stop_fetch_loop().await;
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
        self.stop_fetch_loop().await;
        let result = self
            .app
            .warp
            .connect(accept_terms, settings.warp_http2)
            .await;
        self.start_fetch_loop().await;
        result?;
        Ok(self.app.status().await)
    }

    pub async fn stop_warp(&self) -> Status {
        let _change = self.settings_change.lock().await;
        self.stop_fetch_loop().await;
        self.app.warp.stop().await;
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
        return proxy_ws(app, req).await;
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
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    match forward_http(&app, req).await {
        Ok(resp) => {
            app.record(method.as_str(), &path, resp.status().as_u16(), started)
                .await;
            resp
        }
        Err(err) => {
            app.record(method.as_str(), &path, 502, started).await;
            (StatusCode::BAD_GATEWAY, err.to_string()).into_response()
        }
    }
}

async fn forward_http(app: &App, req: Request<Body>) -> Result<Response> {
    let upstream = app.settings.lock().await.upstream.clone();
    let (mut parts, body) = req.into_parts();
    let target = join_upstream(&upstream, &parts.uri)?;
    let path = parts.uri.path();
    let mut injected_token: Option<String> = None;
    if turn_state::should_stamp_http(parts.method.as_str(), path) {
        // 官方协议：第一次请求不带 turn_state，服务端在响应中下发。
        // 只有客户端已携带 turn_state（同一 turn 的后续请求/重试）时，
        // 才用我们预取的 292 token 替换，确保 sticky routing 正常。
        let client_already_has = turn_state::has_http_turn_state(&parts.headers);
        if client_already_has {
            let store = app.turn_state.lock().await;
            if let Some(token) = store.peek_freshest() {
                turn_state::apply_http_header(&mut parts.headers, &token);
                eprintln!(
                    "[stamp] 替换 turn_state → 292 token len={} 到 {} {}",
                    token.len(),
                    parts.method,
                    path
                );
                injected_token = Some(token);
            } else {
                eprintln!(
                    "[stamp] 客户端携带 turn_state 但无可用 292 token，保留原值 {} {}",
                    parts.method, path
                );
            }
        } else {
            eprintln!(
                "[stamp] 首次请求，不注入 turn_state（等服务端下发） {} {}",
                parts.method, path
            );
        }
    }
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .context("read body")?;
    let mut builder = app
        .http
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
    let upstream_resp = builder.send().await.context("upstream http")?;
    let resp_status_u16 = upstream_resp.status().as_u16();

    // 记录上游响应详情，方便排查 token 失效
    let upstream_turn_state = turn_state::header_token(upstream_resp.headers());
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

async fn proxy_ws(app: Arc<App>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let upstream = app.settings.lock().await.upstream.clone();
    let target = match join_upstream(&upstream, req.uri()) {
        Ok(url) => to_ws_url(&url),
        Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    };
    let headers = req.headers().clone();
    let upgrade = match WebSocketUpgrade::from_request(req, &()).await {
        Ok(ws) => ws,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    app.record("WS", &path, 101, started).await;
    let ws_app = app.clone();
    upgrade
        .on_upgrade(move |socket| async move {
            if let Err(err) = pump_ws(ws_app, socket, target, headers).await {
                eprintln!("ws proxy: {err}");
            }
        })
        .into_response()
}

async fn pump_ws(
    app: Arc<App>,
    mut client: WebSocket,
    target: String,
    headers: HeaderMap,
) -> Result<()> {
    eprintln!("[ws] 连接上游 {}", target);
    let mut request = http::Request::builder().uri(&target).body(())?;
    for (name, value) in &headers {
        if name == header::HOST
            || name == header::CONNECTION
            || name == header::UPGRADE
            || name == header::SEC_WEBSOCKET_KEY
            || name == header::SEC_WEBSOCKET_VERSION
            || name == header::SEC_WEBSOCKET_EXTENSIONS
            || name.as_str().eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        request.headers_mut().insert(name.clone(), value.clone());
    }
    if let Some(proto) = headers.get(SEC_WEBSOCKET_PROTOCOL) {
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, proto.clone());
    }
    turn_state::clear_http_header(request.headers_mut());
    let (upstream, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("[ws] 上游连接失败: {err}");
            let _ = client
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: axum::extract::ws::CloseCode::from(1011u16),
                    reason: format!("upstream connect failed: {err}").into(),
                })))
                .await;
            anyhow::bail!("connect upstream websocket: {err}");
        }
    };
    eprintln!("[ws] 上游已连接，开始桥接");
    let result = bridge(app, &mut client, upstream).await;
    match &result {
        Err(err) => eprintln!("[ws] 桥接结束(错误): {err}"),
        Ok(_) => eprintln!("[ws] 桥接正常结束"),
    }
    // Close 帧已在 bridge 内部各退出路径发送，无需再发
    result
}

/// 双向桥接：client ↔ upstream
/// 用 CancellationToken 协调两方向，确保先发 Close 帧再退出，避免 10054。
async fn bridge(
    app: Arc<App>,
    client: &mut WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<()> {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let app_up = app.clone();
    let app_down = app;

    // 共享的取消标志
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_up = cancel.clone();
    let cancel_down = cancel.clone();

    // client → upstream
    let to_up = async {
        loop {
            tokio::select! {
                biased;
                _ = cancel_up.cancelled() => {
                    eprintln!("[ws] client→upstream 收到停止信号，发 Close 给上游");
                    let _ = up_tx.send(tungstenite::Message::Close(None)).await;
                    break;
                }
                msg = client_rx.next() => {
                    let Some(msg) = msg else { break };
                    let msg = match msg {
                        Ok(m) => m,
                        Err(err) => {
                            eprintln!("[ws←client] 读取错误: {err}");
                            break;
                        }
                    };
                    match msg {
                        Message::Text(t) => {
                            let text = maybe_stamp_ws(&app_up, t.to_string()).await;
                            if let Err(err) = up_tx.send(tungstenite::Message::Text(text.into())).await {
                                eprintln!("[ws→upstream] 发送错误: {err}");
                                break;
                            }
                        }
                        Message::Binary(b) => {
                            if let Err(err) = up_tx.send(tungstenite::Message::Binary(b)).await {
                                eprintln!("[ws→upstream] 发送错误: {err}");
                                break;
                            }
                        }
                        Message::Ping(p) => { let _ = up_tx.send(tungstenite::Message::Ping(p)).await; }
                        Message::Pong(p) => { let _ = up_tx.send(tungstenite::Message::Pong(p)).await; }
                        Message::Close(c) => {
                            let frame = c.map(|f| tungstenite::protocol::CloseFrame {
                                code: tungstenite::protocol::frame::coding::CloseCode::from(u16::from(f.code)),
                                reason: f.reason.to_string().into(),
                            });
                            let _ = up_tx.send(tungstenite::Message::Close(frame)).await;
                            break;
                        }
                    }
                }
            }
        }
        cancel.cancel();
    };

    // upstream → client
    let to_client = async {
        loop {
            tokio::select! {
                biased;
                _ = cancel_down.cancelled() => {
                    eprintln!("[ws] upstream→client 收到停止信号，发 Close 给客户端");
                    let _ = client_tx.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::CloseCode::from(1000u16),
                        reason: "peer finished".into(),
                    }))).await;
                    break;
                }
                msg = up_rx.next() => {
                    let Some(msg) = msg else {
                        eprintln!("[ws] 上游流结束（EOF），发 Close 给客户端");
                        let _ = client_tx.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: axum::extract::ws::CloseCode::from(1000u16),
                            reason: "upstream closed".into(),
                        }))).await;
                        break;
                    };
                    let msg = match msg {
                        Ok(m) => m,
                        Err(err) => {
                            eprintln!("[ws←upstream] 读取错误: {err}，发 Close 给客户端");
                            let _ = client_tx.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: axum::extract::ws::CloseCode::from(1011u16),
                                reason: format!("upstream error: {err}").into(),
                            }))).await;
                            break;
                        }
                    };
                    match msg {
                        tungstenite::Message::Text(t) => {
                            let text = t.to_string();
                            maybe_capture_response_ws(&app_down, &text).await;
                            if let Err(err) = client_tx.send(Message::Text(text.into())).await {
                                eprintln!("[ws→client] 发送错误: {err}");
                                break;
                            }
                        }
                        tungstenite::Message::Binary(b) => {
                            if let Err(err) = client_tx.send(Message::Binary(b)).await {
                                eprintln!("[ws→client] 发送错误: {err}");
                                break;
                            }
                        }
                        tungstenite::Message::Ping(p) => { let _ = client_tx.send(Message::Ping(p)).await; }
                        tungstenite::Message::Pong(p) => { let _ = client_tx.send(Message::Pong(p)).await; }
                        tungstenite::Message::Close(c) => {
                            eprintln!("[ws] 上游发送 Close 帧，转发给客户端");
                            let frame = c.map(|f| axum::extract::ws::CloseFrame {
                                code: axum::extract::ws::CloseCode::from(u16::from(f.code)),
                                reason: f.reason.to_string().into(),
                            });
                            let _ = client_tx.send(Message::Close(frame)).await;
                            break;
                        }
                        tungstenite::Message::Frame(_) => {}
                    }
                }
            }
        }
        cancel_down.cancel();
    };

    // 两个方向同时运行，都结束后才返回
    tokio::join!(to_up, to_client);
    Ok(())
}

/// WS 响应不做 token 捕获，只做占位（未来可扩展失效检测）
async fn maybe_capture_response_ws(_app: &App, _text: &str) {
    // 不管上游 WS 返回的 token，只靠 fetch 循环自己打 292
}

async fn maybe_stamp_ws(app: &App, text: String) -> String {
    if !turn_state::ws_looks_json_object(&text) {
        return text;
    }
    // 同 HTTP：只有客户端已携带 turn_state 时才替换，首次不注入
    if !turn_state::ws_has_turn_state(&text) {
        return text;
    }
    let store = app.turn_state.lock().await;
    let Some(token) = store.peek_freshest() else {
        return text;
    };
    eprintln!("[stamp-ws] 替换 turn_state → 292 token len={}", token.len());
    turn_state::stamp_ws_json(&text, &token).unwrap_or(text)
}

fn is_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
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

fn to_ws_url(http_url: &str) -> String {
    if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
