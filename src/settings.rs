use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboundMode {
    #[default]
    Manual,
    Warp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    pub outbound_proxy: String,
    #[serde(default)]
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub models: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_listen: default_listen().into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: home_dir().join(".codex").display().to_string(),
            outbound_proxy: String::new(),
            outbound_mode: OutboundMode::Warp,
            warp_http2: false,
            models: vec![],
        }
    }
}

/// 开发环境用 8788，打包版用 8787，互不冲突
fn default_listen() -> &'static str {
    if is_dev_mode() {
        "127.0.0.1:8788"
    } else {
        "127.0.0.1:8787"
    }
}

/// debug 编译 = 开发模式
pub fn is_dev_mode() -> bool {
    cfg!(debug_assertions)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    #[serde(default)]
    pub outbound_proxy: String,
    #[serde(default)]
    pub outbound_mode: OutboundMode,
    #[serde(default)]
    pub warp_http2: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

impl SettingsPatch {
    pub fn into_settings(self) -> Result<Settings> {
        let models = self.models;
        let settings = Settings {
            proxy_listen: self.proxy_listen.trim().to_string(),
            upstream: self.upstream.trim().to_string(),
            codex_home: self.codex_home.trim().to_string(),
            outbound_proxy: normalize_outbound_proxy(&self.outbound_proxy)?,
            outbound_mode: self.outbound_mode,
            warp_http2: self.warp_http2,
            models,
        };
        if settings.proxy_listen.is_empty()
            || settings.upstream.is_empty()
            || settings.codex_home.is_empty()
        {
            anyhow::bail!("listen / upstream / CODEX_HOME 不能为空");
        }
        let _: std::net::SocketAddr = settings.proxy_listen.parse().context("proxy_listen")?;
        Ok(settings)
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    if is_dev_mode() {
        home_dir().join(".codex-state-kit-dev.json")
    } else {
        home_dir().join(".codex-state-kit.json")
    }
}

pub fn load_settings() -> Settings {
    let path = settings_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| settings_from_json(&raw).ok())
        .unwrap_or_default()
}

fn settings_from_json(raw: &str) -> Result<Settings> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let mut settings: Settings = serde_json::from_value(value.clone())?;
    // Preserve configured legacy proxies; use embedded WARP for unconfigured installs.
    if value.get("outbound_mode").is_none() && settings.outbound_proxy.trim().is_empty() {
        settings.outbound_mode = OutboundMode::Warp;
    }
    Ok(settings)
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    let path = settings_path();
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn normalize_outbound_proxy(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let url = Url::parse(raw).context("出站代理地址无效")?;
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a" => {}
        other => bail!("不支持的出站代理协议: {other}。请用 socks5:// 或 http://"),
    }
    if url.host_str().is_none() {
        bail!("出站代理缺少主机");
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_outbound_proxy_is_ok() {
        assert_eq!(normalize_outbound_proxy("  ").unwrap(), "");
    }

    #[test]
    fn accepts_socks_and_http() {
        assert_eq!(
            normalize_outbound_proxy("socks5://127.0.0.1:1080").unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            normalize_outbound_proxy("http://127.0.0.1:7890").unwrap(),
            "http://127.0.0.1:7890"
        );
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = normalize_outbound_proxy("ftp://127.0.0.1:21").unwrap_err();
        assert!(err.to_string().contains("不支持的出站代理协议"));
    }

    #[test]
    fn patch_keeps_optional_proxy() {
        let settings = SettingsPatch {
            proxy_listen: "127.0.0.1:8787".into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: "/tmp/codex".into(),
            outbound_proxy: "socks5://127.0.0.1:1080".into(),
            outbound_mode: OutboundMode::Manual,
            warp_http2: false,
            models: vec![],
        }
        .into_settings()
        .unwrap();
        assert_eq!(settings.outbound_proxy, "socks5://127.0.0.1:1080");
    }

    #[test]
    fn legacy_settings_keep_manual_proxy() {
        let settings: Settings =
            serde_json::from_str(r#"{"outbound_proxy":"http://localhost:7890"}"#).unwrap();
        assert_eq!(settings.outbound_mode, OutboundMode::Manual);
        assert_eq!(settings.outbound_proxy, "http://localhost:7890");
    }

    #[test]
    fn embedded_warp_is_default_without_overriding_saved_choices() {
        assert_eq!(Settings::default().outbound_mode, OutboundMode::Warp);
        assert_eq!(
            settings_from_json("{}").unwrap().outbound_mode,
            OutboundMode::Warp
        );
        assert_eq!(
            settings_from_json(r#"{"outbound_proxy":"http://localhost:7890"}"#)
                .unwrap()
                .outbound_mode,
            OutboundMode::Manual
        );
        assert_eq!(
            settings_from_json(r#"{"outbound_mode":"manual"}"#)
                .unwrap()
                .outbound_mode,
            OutboundMode::Manual
        );
    }

    #[test]
    fn warp_selection_preserves_manual_url_and_round_trips() {
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxyListen": "127.0.0.1:8787", "upstream": "https://example.com",
            "codexHome": "test", "outboundProxy": "socks5://localhost:1080", "outboundMode": "warp"
        }))
        .unwrap();
        let settings = patch.into_settings().unwrap();
        let saved: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(saved.outbound_mode, OutboundMode::Warp);
        assert_eq!(saved.outbound_proxy, "socks5://localhost:1080");
        assert!(serde_json::from_str::<OutboundMode>("\"unknown\"").is_err());
    }
}
