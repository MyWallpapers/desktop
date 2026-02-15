//! Tauri command handlers
//!
//! These commands are invoked from the frontend via `window.__TAURI__.invoke()`

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tracing::info;
use typeshare::typeshare;

/// System information response
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub app_version: String,
    pub tauri_version: String,
}

// ============================================================================
// System Information
// ============================================================================

/// Get system information
#[tauri::command]
pub fn get_system_info() -> SystemInfo {
    SystemInfo {
        os: std::env::consts::OS.to_string(),
        os_version: os_info::get().version().to_string(),
        arch: std::env::consts::ARCH.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        tauri_version: tauri::VERSION.to_string(),
    }
}

// ============================================================================
// Auto-Update Commands
// ============================================================================

/// Update information response
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub body: Option<String>,
    pub date: Option<String>,
}

/// Check for application updates and return detailed info
#[tauri::command]
pub async fn check_for_updates(app: tauri::AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;

    info!("Checking for updates...");

    match app.updater() {
        Ok(updater) => {
            match updater.check().await {
                Ok(Some(update)) => {
                    info!("Update available: v{}", update.version);
                    Ok(Some(UpdateInfo {
                        version: update.version.clone(),
                        current_version: env!("CARGO_PKG_VERSION").to_string(),
                        body: update.body.clone(),
                        date: update.date.map(|d| d.to_string()),
                    }))
                }
                Ok(None) => {
                    info!("No updates available");
                    Ok(None)
                }
                Err(e) => {
                    tracing::error!("Update check failed: {}", e);
                    Err(format!("Update check failed: {}", e))
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to get updater: {}", e);
            Err(format!("Updater not available: {}", e))
        }
    }
}

/// Download and install the available update
#[tauri::command]
pub async fn download_and_install_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    info!("Starting update download and install...");

    // Emit progress event
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("update-progress", "checking");
    }

    let updater = app.updater().map_err(|e| format!("Updater not available: {}", e))?;

    let update = updater
        .check()
        .await
        .map_err(|e| format!("Update check failed: {}", e))?
        .ok_or_else(|| "No update available".to_string())?;

    info!("Downloading update v{}...", update.version);

    // Emit download started
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("update-progress", "downloading");
    }

    // Download and install
    update
        .download_and_install(
            |chunk_length, content_length| {
                if let Some(len) = content_length {
                    let _percent = (chunk_length as f64 / len as f64 * 100.0) as u32;
                    tracing::debug!("Download progress: {}%", _percent);
                }
            },
            || {
                info!("Download complete, installing...");
            },
        )
        .await
        .map_err(|e| format!("Update install failed: {}", e))?;

    info!("Update installed successfully. Restart required.");

    // Emit completed
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("update-progress", "installed");
    }

    Ok(())
}

/// Restart the application to apply the update
#[tauri::command]
pub fn restart_app(app: tauri::AppHandle) {
    info!("Restarting application...");
    app.restart();
}

// ============================================================================
// OAuth Commands
// ============================================================================

/// Open OAuth URL in the default browser
/// The callback will redirect to mywallpaper://auth/callback which will be
/// intercepted by the deep-link plugin
#[tauri::command]
pub async fn open_oauth_in_browser(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    info!("Opening OAuth URL in browser: {}", url);

    // Validate URL to prevent command injection
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Invalid URL: must start with http:// or https://".to_string());
    }

    // Use the opener plugin for secure URL opening
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("Failed to open browser: {}", e))?;

    Ok(())
}

// ============================================================================
// Window Commands
// ============================================================================

/// Reload the main window (refresh the page)
#[tauri::command]
pub fn reload_window(app: tauri::AppHandle) -> Result<(), String> {
    info!("Reload window requested");

    if let Some(window) = app.get_webview_window("main") {
        // Emit event for frontend to reload
        window
            .emit("reload-app", ())
            .map_err(|e| format!("Failed to emit reload event: {}", e))?;
    }

    Ok(())
}

// ============================================================================
// Layer Management Commands
// ============================================================================

/// Layer information for the tray menu
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct LayerInfo {
    pub id: String,
    pub name: String,
    pub visible: bool,
}

/// Get current layers from the frontend (emits event, frontend responds)
#[tauri::command]
pub async fn get_layers(app: tauri::AppHandle) -> Result<Vec<LayerInfo>, String> {
    // Emit event to frontend requesting layer list
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("request-layers", ());
    }
    // For now, return empty — the frontend pushes layer updates via events
    Ok(vec![])
}

/// Toggle a layer's visibility
#[tauri::command]
pub async fn toggle_layer(app: tauri::AppHandle, layer_id: String) -> Result<(), String> {
    info!("Toggling layer: {}", layer_id);
    if let Some(window) = app.get_webview_window("main") {
        window
            .emit("toggle-layer", &layer_id)
            .map_err(|e| format!("Failed to emit toggle-layer event: {}", e))?;
    }
    Ok(())
}

// ============================================================================
// Localhost Proxy (Linux mixed-content workaround)
// ============================================================================

/// Proxy response from a localhost fetch
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProxyFetchResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
}

/// Fetch a localhost URL from the Rust side, bypassing WebKitGTK mixed-content
/// blocking. Only allows http://localhost and http://127.0.0.1 URLs.
#[tauri::command]
pub fn proxy_fetch(url: String) -> Result<ProxyFetchResponse, String> {
    // Security: only allow localhost URLs
    if !url.starts_with("http://localhost") && !url.starts_with("http://127.0.0.1") {
        return Err("proxy_fetch only allows localhost URLs".to_string());
    }

    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("Fetch failed: {}", e))?;

    let status = resp.status();
    let content_type = resp.header("content-type")
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = resp.into_string()
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    Ok(ProxyFetchResponse { status, body, content_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_info() {
        let info = get_system_info();
        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert!(!info.app_version.is_empty());
    }
}
