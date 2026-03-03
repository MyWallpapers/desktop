//! Tauri command handlers + business logic

use crate::error::{AppError, AppResult};
use crate::events::{AppEvent, EmitAppEvent};
use crate::system_monitor;
use log::info;
use serde::Serialize;
use typeshare::typeshare;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ============================================================================
// Types
// ============================================================================

#[typeshare]
#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub app_version: String,
    pub tauri_version: String,
}

#[typeshare]
#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub body: Option<String>,
    pub date: Option<String>,
}

// ============================================================================
// Validation
// ============================================================================

/// Parse and validate the updater endpoint URL, returning the parsed URL on success.
fn validate_updater_endpoint(url: &str) -> AppResult<url::Url> {
    let parsed =
        url::Url::parse(url).map_err(|_| AppError::Validation("Invalid endpoint URL".into()))?;
    if parsed.scheme() != "https" {
        return Err(AppError::Validation("Endpoint must use HTTPS".into()));
    }
    if parsed.host_str() != Some("github.com") {
        return Err(AppError::Validation(
            "Endpoint must be on github.com".into(),
        ));
    }
    if !parsed
        .path()
        .starts_with("/MyWallpapers/client/releases/download/")
    {
        return Err(AppError::Validation(
            "Endpoint must point to MyWallpapers/client releases".into(),
        ));
    }
    Ok(parsed)
}

fn is_private_ipv4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
}

pub fn validate_oauth_url(url_str: &str) -> AppResult<()> {
    let parsed =
        url::Url::parse(url_str).map_err(|_| AppError::Validation("Invalid URL".into()))?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                return Err(AppError::Validation(
                    "HTTP is only allowed for localhost".into(),
                ));
            }
            return Ok(());
        }
        _ => {
            return Err(AppError::Validation(
                "URL must use https:// (or http:// for localhost)".into(),
            ))
        }
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if is_private_ipv4(ip) {
                return Err(AppError::Validation(
                    "HTTPS to private/internal IPs is not allowed".into(),
                ));
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if ip.is_loopback() || ip.is_unspecified() {
                return Err(AppError::Validation(
                    "HTTPS to private/internal IPs is not allowed".into(),
                ));
            }
            let s = ip.segments();
            if s[0] & 0xfe00 == 0xfc00 || s[0] & 0xffc0 == 0xfe80 {
                return Err(AppError::Validation(
                    "HTTPS to private/internal IPs is not allowed".into(),
                ));
            }
            if ip.to_ipv4_mapped().is_some_and(is_private_ipv4) {
                return Err(AppError::Validation(
                    "HTTPS to private/internal IPs is not allowed".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_semver(v: &str) -> AppResult<(u32, u32, u32)> {
    let v = v.trim_start_matches('v');
    let v = v.split('-').next().unwrap_or(v);
    let p: Vec<&str> = v.split('.').collect();
    if p.len() != 3 {
        return Err(AppError::Validation(format!("Invalid version: {}", v)));
    }
    Ok((
        p[0].parse()
            .map_err(|_| AppError::Validation("bad major".into()))?,
        p[1].parse()
            .map_err(|_| AppError::Validation("bad minor".into()))?,
        p[2].parse()
            .map_err(|_| AppError::Validation("bad patch".into()))?,
    ))
}

fn validate_update_version(current: &str, candidate: &str) -> AppResult<()> {
    if parse_semver(candidate)? < parse_semver(current)? {
        return Err(AppError::Validation(format!(
            "Refusing downgrade from {} to {}",
            current, candidate
        )));
    }
    Ok(())
}

const ALLOWED_DEEP_LINK_ACTIONS: &[&str] = &["callback", "auth", "oauth", "login", "app"];

pub fn validate_deep_link(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "mywallpaper" {
        return None;
    }
    if let Some(host) = parsed.host_str() {
        if !host.is_empty() && !ALLOWED_DEEP_LINK_ACTIONS.contains(&host) {
            return None;
        }
    }
    Some(parsed.to_string())
}

// ============================================================================
// Commands
// ============================================================================

#[tauri::command]
pub fn get_system_info() -> SystemInfo {
    SystemInfo {
        os: std::env::consts::OS.to_string(),
        os_version: os_info::get().version().to_string(),
        arch: std::env::consts::ARCH.to_string(),
        app_version: APP_VERSION.to_string(),
        tauri_version: tauri::VERSION.to_string(),
    }
}

#[tauri::command]
pub fn get_system_data(categories: Vec<String>) -> system_monitor::SystemData {
    system_monitor::collect_system_data(system_monitor::parse_categories(&categories))
}

#[tauri::command]
pub fn subscribe_system_data(categories: Vec<String>) {
    system_monitor::set_poll_mask(system_monitor::parse_categories(&categories));
}

fn build_updater(
    app: &tauri::AppHandle,
    endpoint: Option<String>,
) -> AppResult<tauri_plugin_updater::Updater> {
    use tauri_plugin_updater::UpdaterExt;
    if let Some(url) = endpoint {
        let parsed = validate_updater_endpoint(&url)?;
        app.updater_builder()
            .endpoints(vec![parsed])
            .map_err(|e| AppError::Updater(format!("Invalid endpoint: {}", e)))?
            .build()
            .map_err(|e| AppError::Updater(format!("Build failed: {}", e)))
    } else {
        app.updater()
            .map_err(|e| AppError::Updater(format!("Updater not available: {}", e)))
    }
}

#[tauri::command]
pub async fn check_for_updates(
    app: tauri::AppHandle,
    endpoint: Option<String>,
) -> AppResult<Option<UpdateInfo>> {
    let updater = build_updater(&app, endpoint)?;
    match updater.check().await {
        Ok(Some(update)) => {
            validate_update_version(APP_VERSION, &update.version)?;
            info!("[updater] Update available: v{}", update.version);
            Ok(Some(UpdateInfo {
                version: update.version.clone(),
                current_version: APP_VERSION.to_string(),
                body: update.body.clone(),
                date: update.date.map(|d| d.to_string()),
            }))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(AppError::Updater(format!("Update check failed: {}", e))),
    }
}

#[tauri::command]
pub async fn download_and_install_update(
    app: tauri::AppHandle,
    endpoint: Option<String>,
) -> AppResult<()> {
    let emit = |s: &str| {
        let _ = app.emit_app_event(&AppEvent::UpdateProgress {
            status: s.to_string(),
        });
    };
    emit("checking");
    let updater = build_updater(&app, endpoint)?;
    let update = updater
        .check()
        .await
        .map_err(|e| AppError::Updater(format!("Update check failed: {}", e)))?
        .ok_or_else(|| AppError::Updater("No update available".to_string()))?;
    validate_update_version(APP_VERSION, &update.version)?;
    emit("downloading");
    update
        .download_and_install(
            |_, _| {},
            || info!("[updater] Download complete, installing..."),
        )
        .await
        .map_err(|e| AppError::Updater(format!("Update install failed: {}", e)))?;
    emit("installed");
    Ok(())
}

#[tauri::command]
pub fn restart_app(app: tauri::AppHandle) {
    app.restart();
}

#[tauri::command]
pub async fn open_oauth_in_browser(app: tauri::AppHandle, url: String) -> AppResult<()> {
    use tauri_plugin_opener::OpenerExt;
    validate_oauth_url(&url)?;
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| AppError::OAuth(format!("Failed to open browser: {}", e)))
}

#[tauri::command]
pub fn reload_window(app: tauri::AppHandle) -> AppResult<()> {
    app.emit_app_event(&AppEvent::ReloadApp)?;
    Ok(())
}

#[tauri::command]
pub fn get_media_info() -> AppResult<crate::media::MediaInfo> {
    crate::media::get_media_info()
}

#[tauri::command]
pub fn media_play_pause() -> AppResult<()> {
    crate::media::media_play_pause()
}

#[tauri::command]
pub fn media_next() -> AppResult<()> {
    crate::media::media_next()
}

#[tauri::command]
pub fn media_prev() -> AppResult<()> {
    crate::media::media_prev()
}

#[tauri::command]
pub fn update_discord_presence(details: String, state: String) -> AppResult<()> {
    crate::discord::update_presence(&details, &state)
}
