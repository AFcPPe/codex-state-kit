use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};
use url::Url;

use crate::login::{has_chatgpt_login, overlay_kit_onto_official, restore_official_auth};
use crate::settings::{home_dir, Settings};

pub const PROVIDER_ID: &str = "codex_state_kit";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backup {
    pub codex_home: String,
    #[serde(default)]
    pub previous_openai_base_url: Option<String>,
    #[serde(default)]
    pub previous_model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_base_url: Option<String>,
    #[serde(default)]
    pub previous_cli_auth_store: Option<String>,
    #[serde(default)]
    pub had_cli_auth_store_key: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub name: String,
    pub base_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexConfigView {
    pub codex_home: String,
    pub model_provider: Option<String>,
    pub openai_base_url: Option<String>,
    pub providers: Vec<ProviderView>,
    pub suggested_base_url: String,
    pub attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn backup_path() -> PathBuf {
    if cfg!(debug_assertions) {
        home_dir().join(".codex-state-kit-dev.backup.json")
    } else {
        home_dir().join(".codex-state-kit.backup.json")
    }
}

/// Owns the desktop session's route. Serialize access with a mutex, including exit.
pub struct ManagedRoutes {
    backup_file: PathBuf,
    current: Option<Settings>,
    closed: bool,
}

impl ManagedRoutes {
    pub fn new(backup_file: PathBuf) -> Self {
        Self { backup_file, current: None, closed: false }
    }

    pub fn sync(&mut self, settings: &Settings, proxy_ready: bool) -> Result<()> {
        if self.closed { return Ok(()); }
        let home = Path::new(&settings.codex_home);
        let ready = proxy_ready && has_chatgpt_login(home);
        if ready && self.current.as_ref().is_some_and(|old| old.codex_home == settings.codex_home)
            && is_attached(home, &format!("http://{}", settings.proxy_listen)) {
            return Ok(());
        }
        let previous = self.current.clone();
        self.restore_current()?;
        if !ready { return Ok(()); }
        // Record ownership before writing so even a failed write can be restored on exit.
        self.current = Some(settings.clone());
        if let Err(err) = attach_codex_config_at(settings, &self.backup_file) {
            if self.backup_file.exists() {
                self.restore_current().context("恢复未完成的自动接入")?;
            } else {
                self.current = None;
            }
            if let Some(old) = previous {
                self.current = Some(old.clone());
                attach_codex_config_at(&old, &self.backup_file)
                    .context("新目录接入失败，恢复旧目录接入失败")?;
            }
            return Err(err);
        }
        Ok(())
    }

    fn restore_current(&mut self) -> Result<()> {
        if let Some(settings) = &self.current {
            restore_at(&self.backup_file, Path::new(&settings.codex_home))?;
            self.current = None;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.closed = true;
        self.restore_current()
    }
}

pub fn validate_codex_home(home: &Path) -> Result<()> {
    if !home.is_dir() { bail!("Codex 工作目录不存在，请选择有效目录。"); }
    let config = home.join("config.toml");
    if config.exists() {
        parse_doc(&std::fs::read_to_string(config)?)?;
    }
    Ok(())
}

pub fn attach_codex_config(settings: &Settings) -> Result<String> {
    attach_codex_config_at(settings, &backup_path())
}

pub fn attach_codex_config_at(settings: &Settings, backup_file: &Path) -> Result<String> {
    let home = Path::new(&settings.codex_home);
    if !has_chatgpt_login(home) {
        bail!("尚未登录 ChatGPT。请先在本应用完成 ChatGPT 登录。");
    }
    attach_at(
        home,
        backup_file,
        &format!("http://{}", settings.proxy_listen),
    )
}

pub fn restore_codex_config(home: &Path) -> Result<String> {
    restore_at(&backup_path(), home)
}

pub fn update_attached_base_url(settings: &Settings) -> Result<Option<String>> {
    let home = Path::new(&settings.codex_home);
    let raw = match read_config_text(home) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    if !takeover_present(&raw) {
        return Ok(None);
    }
    attach_at(
        home,
        &backup_path(),
        &format!("http://{}", settings.proxy_listen),
    )
    .map(Some)
}

pub fn inspect_codex_config(home: &Path, suggested_base_url: &str) -> CodexConfigView {
    match read_codex_snapshot(home) {
        Ok((model_provider, openai_base_url, providers)) => CodexConfigView {
            attached: is_attached(home, suggested_base_url),
            codex_home: home.display().to_string(),
            model_provider,
            openai_base_url,
            providers,
            suggested_base_url: suggested_base_url.to_string(),
            error: None,
        },
        Err(err) => CodexConfigView {
            attached: false,
            codex_home: home.display().to_string(),
            model_provider: None,
            openai_base_url: None,
            providers: Vec::new(),
            suggested_base_url: suggested_base_url.to_string(),
            error: Some(err.to_string()),
        },
    }
}

pub fn is_attached(home: &Path, proxy_base_url: &str) -> bool {
    read_config_text(home)
        .ok()
        .and_then(|raw| live_attached(&raw, proxy_base_url).ok())
        .unwrap_or(false)
}

fn read_codex_snapshot(home: &Path) -> Result<(Option<String>, Option<String>, Vec<ProviderView>)> {
    let raw = read_config_text(home)?;
    let value: toml::Value = raw.parse::<toml::Value>().context("parse config.toml")?;
    let model_provider = value
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let openai_base_url = value
        .get("openai_base_url")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_base_url);
    let mut providers = Vec::new();
    if let Some(table) = value.get("model_providers").and_then(toml::Value::as_table) {
        for (name, spec) in table {
            let base_url = spec
                .get("base_url")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
            providers.push(ProviderView {
                name: name.clone(),
                base_url,
            });
        }
    }
    Ok((model_provider, openai_base_url, providers))
}

fn attach_at(home: &Path, backup_file: &Path, next: &str) -> Result<String> {
    let config_path = home.join("config.toml");
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", config_path.display())),
    };
    let next = normalize_base_url(next);
    let doc = parse_doc(&raw)?;
    let current = openai_base_url(&doc);
    let legacy = has_legacy_fwd(&doc);
    let already = live_attached(&raw, &next)?;
    if already && current.as_deref() == Some(next.as_str()) && !legacy {
        ensure_sidecar(home, backup_file, &raw, false)?;
        overlay_kit_onto_official(home)?;
        return Ok("already attached".into());
    }
    if takeover_present(&raw) {
        ensure_sidecar(home, backup_file, &raw, false)?;
    } else {
        ensure_sidecar(home, backup_file, &raw, true)?;
    }
    let patched = apply_fwd_route(&raw, &next)?;
    atomic_write_text(&config_path, &patched)?;
    overlay_kit_onto_official(home)?;
    Ok(format!("patched Codex openai_base_url -> {next}"))
}

fn restore_at(backup_file: &Path, fallback_home: &Path) -> Result<String> {
    let backup = load_backup_at(backup_file);
    let home = backup
        .as_ref()
        .map(|item| PathBuf::from(&item.codex_home))
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| fallback_home.to_path_buf());
    let config_path = home.join("config.toml");
    let bak = config_bak_path(&home);
    restore_official_auth(&home)?;

    if bak.exists() {
        std::fs::copy(&bak, &config_path)
            .with_context(|| format!("restore {}", config_path.display()))?;
        let _ = std::fs::remove_file(&bak);
        clear_backup_at(backup_file);
        return Ok(format!("restored {}", config_path.display()));
    }

    if !config_path.exists() {
        clear_backup_at(backup_file);
        return Ok("nothing to restore".into());
    }

    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let takeover = takeover_present(&raw);

    if let Some(backup) = backup.as_ref() {
        if let (Some(provider), Some(original)) = (
            legacy_hijacked_provider(backup),
            backup
                .original_base_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ) {
            let mut patched = set_provider_base_url(&raw, provider, original)?;
            patched = remove_fwd_route(&patched, Some(backup))?;
            atomic_write_text(&config_path, &patched)?;
            clear_backup_at(backup_file);
            return Ok(format!("restored `{provider}` base_url -> {original}"));
        }
    }

    if !takeover && backup.is_none() {
        return Ok("nothing to restore".into());
    }

    let patched = remove_fwd_route(&raw, backup.as_ref())?;
    atomic_write_text(&config_path, &patched)?;
    clear_backup_at(backup_file);
    Ok(format!("restored {}", config_path.display()))
}

fn ensure_sidecar(home: &Path, backup_file: &Path, raw: &str, overwrite: bool) -> Result<()> {
    if !overwrite && load_backup_at(backup_file).is_some() {
        return Ok(());
    }
    let doc = parse_doc(raw)?;
    let previous_openai = openai_base_url(&doc);
    let previous_provider = active_model_provider(&doc).filter(|id| id != PROVIDER_ID);
    save_backup_at(
        backup_file,
        &Backup {
            codex_home: home.display().to_string(),
            previous_openai_base_url: previous_openai,
            previous_model_provider: previous_provider,
            provider: None,
            original_base_url: None,
            previous_cli_auth_store: cli_auth_credentials_store(&doc),
            had_cli_auth_store_key: doc.get("cli_auth_credentials_store").is_some(),
        },
    )
}

fn apply_fwd_route(config_text: &str, proxy_base_url: &str) -> Result<String> {
    let mut doc = parse_doc(config_text)?;
    doc["openai_base_url"] = toml_edit::value(normalize_base_url(proxy_base_url));
    doc["cli_auth_credentials_store"] = toml_edit::value("file");
    strip_legacy_fwd(&mut doc);
    Ok(doc.to_string())
}

fn remove_fwd_route(config_text: &str, backup: Option<&Backup>) -> Result<String> {
    let mut doc = parse_doc(config_text)?;
    match backup
        .and_then(effective_previous_openai)
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(url) => doc["openai_base_url"] = toml_edit::value(normalize_base_url(url)),
        None => {
            doc.as_table_mut().remove("openai_base_url");
        }
    }
    strip_legacy_fwd(&mut doc);
    if active_model_provider(&doc).is_none() {
        if let Some(id) = backup
            .and_then(effective_previous_provider)
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty() && *id != PROVIDER_ID)
        {
            doc["model_provider"] = toml_edit::value(id);
        }
    }
    restore_cli_auth_store(&mut doc, backup);
    Ok(doc.to_string())
}

fn cli_auth_credentials_store(doc: &DocumentMut) -> Option<String> {
    doc.get("cli_auth_credentials_store")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn restore_cli_auth_store(doc: &mut DocumentMut, backup: Option<&Backup>) {
    match backup {
        Some(item) if item.had_cli_auth_store_key => {
            match item
                .previous_cli_auth_store
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => doc["cli_auth_credentials_store"] = toml_edit::value(value),
                None => {
                    doc.as_table_mut().remove("cli_auth_credentials_store");
                }
            }
        }
        _ => {
            doc.as_table_mut().remove("cli_auth_credentials_store");
        }
    }
}

fn strip_legacy_fwd(doc: &mut DocumentMut) {
    if active_model_provider(doc).as_deref() == Some(PROVIDER_ID) {
        doc.as_table_mut().remove("model_provider");
    }
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        providers.remove(PROVIDER_ID);
        if providers.is_empty() {
            doc.as_table_mut().remove("model_providers");
        }
    } else if let Some(item) = doc.as_table_mut().remove("model_providers") {
        if let Ok(mut providers) = item_into_table(item) {
            providers.remove(PROVIDER_ID);
            if !providers.is_empty() {
                doc["model_providers"] = Item::Table(providers);
            }
        }
    }
}

fn set_provider_base_url(config_text: &str, provider: &str, base_url: &str) -> Result<String> {
    let mut doc = parse_doc(config_text)?;
    let providers = ensure_providers_table(&mut doc)?;
    let Some(item) = providers.get_mut(provider) else {
        bail!("missing [model_providers.{provider}]");
    };
    let table = item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[model_providers.{provider}] must be a table"))?;
    table["base_url"] = toml_edit::value(base_url);
    Ok(doc.to_string())
}

fn live_attached(raw: &str, proxy_base_url: &str) -> Result<bool> {
    let doc = parse_doc(raw)?;
    let expected = normalize_base_url(proxy_base_url);
    Ok(has_legacy_fwd(&doc) || openai_base_url(&doc).as_deref() == Some(expected.as_str()))
}

fn takeover_present(raw: &str) -> bool {
    parse_doc(raw).ok().is_some_and(|doc| {
        has_legacy_fwd(&doc) || openai_base_url(&doc).is_some_and(|url| is_loopback_http(&url))
    })
}

fn openai_base_url(doc: &DocumentMut) -> Option<String> {
    doc.get("openai_base_url")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_base_url)
}

fn has_legacy_fwd(doc: &DocumentMut) -> bool {
    active_model_provider(doc).as_deref() == Some(PROVIDER_ID) || has_our_table(doc)
}

fn has_our_table(doc: &DocumentMut) -> bool {
    doc.get("model_providers")
        .and_then(Item::as_table)
        .and_then(|table| table.get(PROVIDER_ID))
        .and_then(Item::as_table)
        .is_some()
}

fn active_model_provider(doc: &DocumentMut) -> Option<String> {
    doc.get("model_provider")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn ensure_providers_table(doc: &mut DocumentMut) -> Result<&mut Table> {
    if doc.get("model_providers").is_none() {
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc["model_providers"] = Item::Table(parent);
    } else if doc
        .get("model_providers")
        .and_then(Item::as_table)
        .is_none()
    {
        let item = doc
            .as_table_mut()
            .remove("model_providers")
            .expect("model_providers");
        let table = item_into_table(item)?;
        doc["model_providers"] = Item::Table(table);
    }
    doc.get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("model_providers must be a table"))
}

fn item_into_table(item: Item) -> Result<Table> {
    item.into_table()
        .map_err(|_| anyhow::anyhow!("model_providers must be a table"))
}

fn parse_doc(raw: &str) -> Result<DocumentMut> {
    raw.parse::<DocumentMut>()
        .map_err(|err| anyhow::anyhow!("Invalid Codex config.toml: {err}"))
}

fn read_config_text(home: &Path) -> Result<String> {
    let path = home.join("config.toml");
    std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
}

fn atomic_write_text(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    ));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

fn config_bak_path(home: &Path) -> PathBuf {
    home.join("config.toml.codex-state-kit.bak")
}

fn load_backup_at(path: &Path) -> Option<Backup> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn save_backup_at(path: &Path, backup: &Backup) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(backup)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn clear_backup_at(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn effective_previous_openai(backup: &Backup) -> Option<String> {
    backup
        .previous_openai_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_base_url)
}

fn effective_previous_provider(backup: &Backup) -> Option<String> {
    backup
        .previous_model_provider
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != PROVIDER_ID)
        .map(str::to_string)
        .or_else(|| {
            backup
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty() && *id != PROVIDER_ID)
                .map(str::to_string)
        })
}

fn legacy_hijacked_provider(backup: &Backup) -> Option<&str> {
    backup
        .original_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    backup
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != PROVIDER_ID)
}

fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn is_loopback_http(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_chatgpt_auth(home: &Path) {
        fs::create_dir_all(home).unwrap();
        fs::write(
            home.join("auth.json"),
            r#"{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {
    "id_token": "a.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0In0.sig",
    "access_token": "access",
    "refresh_token": "refresh",
    "account_id": "acct"
  }
}"#,
        )
        .unwrap();
    }

    fn write_config(home: &Path, raw: &str) {
        fs::create_dir_all(home).unwrap();
        fs::write(home.join("config.toml"), raw).unwrap();
    }

    fn settings_for(home: &Path) -> Settings {
        Settings {
            proxy_listen: "127.0.0.1:8787".into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: home.display().to_string(),
            outbound_proxy: String::new(),
            ..Settings::default()
        }
    }

    fn empty_backup(home: &Path) -> Backup {
        Backup {
            codex_home: home.display().to_string(),
            previous_openai_base_url: None,
            previous_model_provider: None,
            provider: None,
            original_base_url: None,
            previous_cli_auth_store: None,
            had_cli_auth_store_key: false,
        }
    }

    #[test]
    fn managed_routes_wait_for_proxy_and_login_then_restore_on_exit() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(&home, "model = \"test-model\"\n");
        let settings = settings_for(&home);
        let mut routes = ManagedRoutes::new(backup.clone());
        routes.sync(&settings, true).unwrap();
        assert!(!backup.exists());
        write_chatgpt_auth(&home);
        routes.sync(&settings, false).unwrap();
        assert!(!backup.exists());
        routes.sync(&settings, true).unwrap();
        routes.sync(&settings, true).unwrap();
        assert!(is_attached(&home, "http://127.0.0.1:8787"));
        routes.shutdown().unwrap();
        routes.shutdown().unwrap();
        routes.sync(&settings, true).unwrap();
        assert!(!is_attached(&home, "http://127.0.0.1:8787"));
        assert!(!backup.exists());
        assert!(fs::read_to_string(home.join("config.toml")).unwrap().contains("test-model"));
        assert!(home.join("auth.json").exists());
    }

    #[test]
    fn managed_directory_switch_restores_each_original_route() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        for home in [&first, &second] { write_chatgpt_auth(home); }
        write_config(&first, "openai_base_url = \"https://first.example/v1\"\n");
        write_config(&second, "openai_base_url = \"https://second.example/v1\"\n");
        let mut routes = ManagedRoutes::new(root.path().join("backup.json"));
        routes.sync(&settings_for(&first), true).unwrap();
        routes.sync(&settings_for(&second), true).unwrap();
        assert!(fs::read_to_string(first.join("config.toml")).unwrap().contains("https://first.example/v1"));
        assert!(is_attached(&second, "http://127.0.0.1:8787"));
        routes.shutdown().unwrap();
        assert!(fs::read_to_string(second.join("config.toml")).unwrap().contains("https://second.example/v1"));
    }

    #[test]
    fn failed_directory_switch_keeps_old_route_and_backup() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        for home in [&first, &second] { write_chatgpt_auth(home); }
        write_config(&first, "openai_base_url = \"https://original.example\"\n");
        write_config(&second, "invalid = [");
        let mut routes = ManagedRoutes::new(root.path().join("backup.json"));
        routes.sync(&settings_for(&first), true).unwrap();
        assert!(routes.sync(&settings_for(&second), true).is_err());
        assert!(is_attached(&first, "http://127.0.0.1:8787"));
        assert_eq!(fs::read_to_string(second.join("config.toml")).unwrap(), "invalid = [");
        routes.shutdown().unwrap();
        assert!(fs::read_to_string(first.join("config.toml")).unwrap().contains("https://original.example"));
    }

    #[test]
    fn managed_routes_handle_new_login_without_existing_config() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        write_chatgpt_auth(&home);
        let mut routes = ManagedRoutes::new(root.path().join("backup.json"));
        routes.sync(&settings_for(&home), true).unwrap();
        assert!(is_attached(&home, "http://127.0.0.1:8787"));
        routes.shutdown().unwrap();
        assert!(!is_attached(&home, "http://127.0.0.1:8787"));
    }

    #[test]
    fn switching_to_logged_out_directory_releases_old_route() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        write_chatgpt_auth(&first);
        write_config(&first, "model = \"first\"\n");
        write_config(&second, "model = \"second\"\n");
        let mut routes = ManagedRoutes::new(root.path().join("backup.json"));
        routes.sync(&settings_for(&first), true).unwrap();
        routes.sync(&settings_for(&second), true).unwrap();
        assert!(!is_attached(&first, "http://127.0.0.1:8787"));
        assert!(!is_attached(&second, "http://127.0.0.1:8787"));
        write_chatgpt_auth(&second);
        routes.sync(&settings_for(&second), true).unwrap();
        assert!(is_attached(&second, "http://127.0.0.1:8787"));
        routes.shutdown().unwrap();
    }

    #[test]
    fn insert_official_login_openai_base_url() {
        let raw = "model = \"gpt-6-astra\"\n";
        let out = apply_fwd_route(raw, "http://127.0.0.1:8787").unwrap();
        assert!(out.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        assert!(out.contains("cli_auth_credentials_store = \"file\""));
        assert!(out.contains("model = \"gpt-6-astra\""));
        assert!(!out.contains("model_provider"));
        assert!(!out.contains("[model_providers.codex_state_kit]"));
    }

    #[test]
    fn attach_does_not_mutate_existing_provider() {
        let raw = r#"model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"
"#;
        let out = apply_fwd_route(raw, "http://127.0.0.1:8787").unwrap();
        assert!(out.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        assert!(out.contains("model_provider = \"openai\""));
        assert!(out.contains("[model_providers.openai]"));
        assert!(out.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(!out.contains("[model_providers.codex_state_kit]"));
    }

    #[test]
    fn attach_official_and_restore_preserves_extra_keys() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        let original = r#"# keep-me
model = "gpt-6-astra"

[mcp_servers.example]
command = "example"
"#;
        write_config(&home, original);
        write_chatgpt_auth(&home);

        let msg = attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        assert!(msg.contains("patched Codex openai_base_url"));
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        assert!(patched.contains("cli_auth_credentials_store = \"file\""));
        assert!(!patched.contains("model_provider"));
        assert!(!patched.contains("[model_providers.codex_state_kit]"));
        assert!(patched.contains("[mcp_servers.example]"));
        assert!(patched.contains("# keep-me"));
        assert!(!home.join("config.toml.codex-state-kit.bak").exists());
        assert!(backup.exists());
        assert!(is_attached(&home, "http://127.0.0.1:8787"));

        let again = attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        assert_eq!(again, "already attached");

        let sidecar: Backup = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert!(sidecar.previous_openai_base_url.is_none());
        assert!(sidecar.previous_model_provider.is_none());

        let restored = restore_at(&backup, &home).unwrap();
        assert!(restored.contains("restored"));
        let after = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!after.contains("openai_base_url"));
        assert!(!after.contains("cli_auth_credentials_store"));
        assert!(!after.contains("model_provider"));
        assert!(!after.contains("[model_providers.codex_state_kit]"));
        assert!(after.contains("[mcp_servers.example]"));
        assert!(after.contains("model = \"gpt-6-astra\""));
        assert!(!backup.exists());
        assert!(!is_attached(&home, "http://127.0.0.1:8787"));
    }

    #[test]
    fn attach_overlays_kit_account_and_restore_brings_official_back() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            "model = \"gpt-6-astra\"\ncli_auth_credentials_store = \"keyring\"\n",
        );
        fs::write(
            home.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official","refresh_token":"keep","account_id":"official-acct"}}"#,
        )
        .unwrap();
        fs::write(
            crate::login::kit_auth_path(&home),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"kit","refresh_token":"kit-refresh","account_id":"kit-acct"}}"#,
        )
        .unwrap();

        attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        let official = fs::read_to_string(home.join("auth.json")).unwrap();
        assert!(official.contains("kit-acct"));
        assert!(!official.contains("official-acct"));
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("cli_auth_credentials_store = \"file\""));
        let sidecar: Backup = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert!(sidecar.had_cli_auth_store_key);
        assert_eq!(sidecar.previous_cli_auth_store.as_deref(), Some("keyring"));

        restore_at(&backup, &home).unwrap();
        let restored = fs::read_to_string(home.join("auth.json")).unwrap();
        assert!(restored.contains("official-acct"));
        assert!(!restored.contains("kit-acct"));
        assert!(!crate::login::official_auth_backup_path(&home).exists());
        let after = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.contains("cli_auth_credentials_store = \"keyring\""));
        assert!(!after.contains("openai_base_url"));
    }

    #[test]
    fn restore_previous_provider_and_keeps_user_table() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            r#"model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"

[mcp_servers.example]
command = "example"
"#,
        );
        write_chatgpt_auth(&home);
        attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(patched.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        assert!(patched.contains("model_provider = \"openai\""));
        restore_at(&backup, &home).unwrap();
        let after = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.contains("model_provider = \"openai\""));
        assert!(after.contains("[model_providers.openai]"));
        assert!(!after.contains("openai_base_url"));
        assert!(!after.contains("cli_auth_credentials_store"));
        assert!(!after.contains("[model_providers.codex_state_kit]"));
        assert!(after.contains("[mcp_servers.example]"));
    }

    #[test]
    fn attach_updates_own_base_url_only() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(&home, "model = \"gpt-6-astra\"\n");
        write_chatgpt_auth(&home);
        attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        attach_at(&home, &backup, "http://127.0.0.1:9999").unwrap();
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("openai_base_url = \"http://127.0.0.1:9999\""));
        assert!(!patched.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        let sidecar: Backup = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert!(sidecar.previous_openai_base_url.is_none());
    }

    #[test]
    fn restore_previous_openai_base_url() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            "model = \"gpt-6-astra\"\nopenai_base_url = \"https://example.openai.azure.com/openai\"\n",
        );
        write_chatgpt_auth(&home);
        attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        let sidecar: Backup = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert_eq!(
            sidecar.previous_openai_base_url.as_deref(),
            Some("https://example.openai.azure.com/openai")
        );
        restore_at(&backup, &home).unwrap();
        let after = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.contains("openai_base_url = \"https://example.openai.azure.com/openai\""));
    }

    #[test]
    fn attach_cleans_legacy_codex_state_kit_table() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            r#"model = "gpt-6-astra"
model_provider = "codex_state_kit"

[model_providers.codex_state_kit]
name = "OpenAI"
base_url = "http://127.0.0.1:8787"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = true
"#,
        );
        write_chatgpt_auth(&home);
        save_backup_at(
            &backup,
            &Backup {
                previous_openai_base_url: None,
                previous_model_provider: None,
                ..empty_backup(&home)
            },
        )
        .unwrap();
        attach_at(&home, &backup, "http://127.0.0.1:8787").unwrap();
        let patched = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(patched.contains("openai_base_url = \"http://127.0.0.1:8787\""));
        assert!(!patched.contains("model_provider"));
        assert!(!patched.contains("[model_providers.codex_state_kit]"));
        let sidecar: Backup = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert!(sidecar.previous_model_provider.is_none());
    }

    #[test]
    fn restore_legacy_codex_state_kit_without_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            r#"model = "gpt-6-astra"
model_provider = "codex_state_kit"

[mcp_servers.example]
command = "example"

[model_providers.codex_state_kit]
name = "OpenAI"
base_url = "http://127.0.0.1:8787"
"#,
        );
        restore_at(&backup, &home).unwrap();
        let after = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!after.contains("model_provider"));
        assert!(!after.contains("[model_providers.codex_state_kit]"));
        assert!(after.contains("[mcp_servers.example]"));
        assert!(!after.contains("openai_base_url"));
    }

    #[test]
    fn attach_without_chatgpt_login_is_blocked() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(&home, "model = \"gpt-6-astra\"\n");
        let err = attach_codex_config_at(&settings_for(&home), &backup).unwrap_err();
        assert!(err.to_string().contains("尚未登录 ChatGPT"));
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            "model = \"gpt-6-astra\"\n"
        );
    }

    #[test]
    fn attach_with_api_key_only_is_blocked() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(&home, "model = \"gpt-6-astra\"\n");
        fs::write(
            home.join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        let err = attach_codex_config_at(&settings_for(&home), &backup).unwrap_err();
        assert!(err.to_string().contains("尚未登录 ChatGPT"));
    }

    #[test]
    fn restore_legacy_bak_once() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        let original = "model = \"gpt-6-astra\"\n";
        write_config(&home, "model_provider = \"codex_state_kit\"\n");
        fs::write(home.join("config.toml.codex-state-kit.bak"), original).unwrap();
        save_backup_at(
            &backup,
            &Backup {
                previous_openai_base_url: None,
                previous_model_provider: None,
                provider: Some("codex_state_kit".into()),
                original_base_url: Some(String::new()),
                ..empty_backup(&home)
            },
        )
        .unwrap();
        restore_at(&backup, &home).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
        assert!(!home.join("config.toml.codex-state-kit.bak").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn restore_legacy_hijack_without_bak() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        let backup = root.path().join("backup.json");
        write_config(
            &home,
            r#"model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
base_url = "http://127.0.0.1:8787"
"#,
        );
        save_backup_at(
            &backup,
            &Backup {
                previous_openai_base_url: None,
                previous_model_provider: None,
                provider: Some("openai".into()),
                original_base_url: Some("https://api.openai.com/v1".into()),
                ..empty_backup(&home)
            },
        )
        .unwrap();
        let msg = restore_at(&backup, &home).unwrap();
        assert!(msg.contains("restored `openai` base_url"));
        let raw = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(raw.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(raw.contains("model_provider = \"openai\""));
        assert!(!backup.exists());
    }
}
