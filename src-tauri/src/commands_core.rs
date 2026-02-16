//! Platform-independent command logic
//!
//! These functions contain the actual business logic, free of `tauri::` types.
//! Tauri command wrappers in `commands.rs` call into these.

use serde::{Deserialize, Serialize};
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

/// Update information response
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub body: Option<String>,
    pub date: Option<String>,
}

/// Layer information for the tray menu
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct LayerInfo {
    pub id: String,
    pub name: String,
    pub visible: bool,
}

// ============================================================================
// System Information
// ============================================================================

/// Get system information (platform-independent)
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
// OAuth
// ============================================================================

/// Validate an OAuth URL — must start with http(s)://
pub fn validate_oauth_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Invalid URL: must start with http:// or https://".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_oauth_url() {
        assert!(validate_oauth_url("https://example.com").is_ok());
        assert!(validate_oauth_url("http://example.com").is_ok());
        assert!(validate_oauth_url("ftp://example.com").is_err());
        assert!(validate_oauth_url("javascript:alert(1)").is_err());
    }
}
