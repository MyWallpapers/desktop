//! Tauri command handlers
//!
//! These commands are invoked from the frontend via `window.__TAURI__.invoke()`.
//! Actual business logic lives in `commands_core`; these are thin Tauri wrappers.

use crate::commands_core;
use tauri::{Emitter, Manager};
use tracing::info;

// Re-export core types so existing `use crate::commands::*` still works
pub use commands_core::{SystemInfo, UpdateInfo};

// ============================================================================
// System Information
// ============================================================================

/// Get system information
#[tauri::command]
pub fn get_system_info() -> SystemInfo {
    commands_core::get_system_info()
}

// ============================================================================
// Auto-Update Commands
// ============================================================================

/// Check for application updates and return detailed info.
/// When `endpoint` is provided, uses a custom updater endpoint (e.g. for pre-release builds).
/// Otherwise uses the default endpoint from tauri.conf.json.
#[tauri::command]
pub async fn check_for_updates(
    app: tauri::AppHandle,
    endpoint: Option<String>,
) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;

    info!("Checking for updates (endpoint: {:?})...", endpoint);

    let updater = if let Some(url) = endpoint {
        let parsed: url::Url = url
            .parse()
            .map_err(|e| format!("Invalid endpoint URL: {}", e))?;
        app.updater_builder()
            .endpoints(vec![parsed])
            .map_err(|e| format!("Invalid endpoint: {}", e))?
            .build()
            .map_err(|e| format!("Failed to build updater: {}", e))?
    } else {
        app.updater()
            .map_err(|e| format!("Updater not available: {}", e))?
    };

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

/// Download and install the available update.
/// When `endpoint` is provided, uses a custom updater endpoint (e.g. for pre-release builds).
#[tauri::command]
pub async fn download_and_install_update(
    app: tauri::AppHandle,
    endpoint: Option<String>,
) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    info!(
        "Starting update download and install (endpoint: {:?})...",
        endpoint
    );

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("update-progress", "checking");
    }

    let updater = if let Some(url) = endpoint {
        let parsed: url::Url = url
            .parse()
            .map_err(|e| format!("Invalid endpoint URL: {}", e))?;
        app.updater_builder()
            .endpoints(vec![parsed])
            .map_err(|e| format!("Invalid endpoint: {}", e))?
            .build()
            .map_err(|e| format!("Failed to build updater: {}", e))?
    } else {
        app.updater()
            .map_err(|e| format!("Updater not available: {}", e))?
    };

    let update = updater
        .check()
        .await
        .map_err(|e| format!("Update check failed: {}", e))?
        .ok_or_else(|| "No update available".to_string())?;

    info!("Downloading update v{}...", update.version);

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("update-progress", "downloading");
    }

    update
        .download_and_install(
            |chunk_length, content_length| {
                if let Some(len) = content_length {
                    tracing::debug!("Download progress: {}%", chunk_length * 100 / len as usize);
                }
            },
            || {
                info!("Download complete, installing...");
            },
        )
        .await
        .map_err(|e| format!("Update install failed: {}", e))?;

    info!("Update installed successfully. Restart required.");

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
#[tauri::command]
pub async fn open_oauth_in_browser(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    info!("Opening OAuth URL in browser: {}", url);

    commands_core::validate_oauth_url(&url)?;

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
        window
            .emit("reload-app", ())
            .map_err(|e| format!("Failed to emit reload event: {}", e))?;
    }

    Ok(())
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
