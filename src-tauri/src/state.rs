use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use codex_state_kit::browser_login::BrowserLogin;
use codex_state_kit::warp::{WarpPaths, WarpRuntime};
use codex_state_kit::{load_settings, App, PendingLogin, ProxyHandle};

#[derive(Clone)]
pub enum LoginSession {
    Device(PendingLogin),
    Browser(Arc<BrowserLogin>),
}

#[derive(Default)]
pub struct LoginSlot {
    pub generation: u64,
    pub pending: Option<LoginSession>,
    pub closed: bool,
}

impl LoginSlot {
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(pending) = self.pending.take() {
            match pending {
                LoginSession::Device(pending) => pending.cancel(),
                LoginSession::Browser(pending) => pending.cancel(),
            }
        }
    }
}

pub struct AppState {
    pub proxy: ProxyHandle,
    pub pending_login: Mutex<LoginSlot>,
    restored: AtomicBool,
}

impl AppState {
    pub fn initialize(warp_paths: WarpPaths) -> Result<Self> {
        let settings = load_settings();
        let app = Arc::new(App::with_warp(
            settings,
            WarpRuntime::new(Some(warp_paths)),
        )?);
        let proxy = ProxyHandle::new(app);
        proxy.enable_auto_attach();
        Ok(Self {
            proxy,
            pending_login: Mutex::new(LoginSlot::default()),
            restored: AtomicBool::new(false),
        })
    }

    pub fn core(&self) -> Arc<App> {
        self.proxy.app()
    }

    pub fn restore_once(&self) {
        if self.restored.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut login = self.pending_login.lock().expect("pending login");
            login.closed = true;
            login.cancel();
        }
        if let Err(err) = self.proxy.restore_managed_routes() {
            eprintln!("restore on exit failed: {err:#}");
        }
        let proxy = self.proxy.clone();
        tauri::async_runtime::spawn(async move {
            proxy.app().warp.stop().await;
            proxy.stop().await;
        });
    }
}
