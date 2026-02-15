//! Desktop Clone — OS wallpaper & desktop icon extraction
//!
//! Cross-platform commands to read the user's current wallpaper image
//! and enumerate their desktop icons (name, icon image, executable path).

use base64::Engine;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use typeshare::typeshare;

// ============================================================================
// Types
// ============================================================================

/// OS wallpaper information
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct WallpaperInfo {
    pub path: String,
    pub image_base64: String,
    pub mime_type: String,
}

/// Desktop icon information
#[typeshare]
#[derive(Debug, Serialize, Deserialize)]
pub struct DesktopIcon {
    pub name: String,
    pub icon_base64: String,
    pub exec_path: String,
    pub is_directory: bool,
}

// ============================================================================
// Helpers
// ============================================================================

/// Decode percent-encoded characters in a URL path (e.g. %20 → space)
fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = hex_val(bytes[i + 1]);
            let l = hex_val(bytes[i + 2]);
            if let (Some(hv), Some(lv)) = (h, l) {
                result.push(hv * 16 + lv);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Guess MIME type from file extension
fn mime_from_path(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("tiff" | "tif") => "image/tiff",
        _ => "image/png",
    }
}

/// Read a file and return its base64-encoded contents
fn file_to_base64(path: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Failed to read file {}: {}", path.display(), e))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// Get the display name for a desktop item (strip extension for known types)
fn display_name(path: &std::path::Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // For .desktop files on Linux, we'll parse the Name field instead
    #[cfg(target_os = "linux")]
    if path.extension().and_then(|e| e.to_str()) == Some("desktop") {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if let Some(n) = line.strip_prefix("Name=") {
                    return n.to_string();
                }
            }
        }
    }

    name
}

// ============================================================================
// Get OS Wallpaper
// ============================================================================

/// Get the current OS wallpaper image as base64
#[tauri::command]
pub async fn get_os_wallpaper() -> Result<WallpaperInfo, String> {
    info!("Getting OS wallpaper...");
    let path = get_wallpaper_path()?;
    let file_path = std::path::Path::new(&path);

    if !file_path.exists() {
        return Err(format!("Wallpaper file does not exist: {}", path));
    }

    let mime_type = mime_from_path(file_path);
    let image_base64 = file_to_base64(file_path)?;

    info!("Wallpaper loaded: {} ({}, {} bytes base64)", path, mime_type, image_base64.len());

    Ok(WallpaperInfo {
        path,
        image_base64,
        mime_type: mime_type.to_string(),
    })
}

/// Platform-specific wallpaper path detection
#[cfg(target_os = "windows")]
fn get_wallpaper_path() -> Result<String, String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktop = hkcu
        .open_subkey("Control Panel\\Desktop")
        .map_err(|e| format!("Failed to open registry key: {}", e))?;

    let path: String = desktop
        .get_value("WallPaper")
        .map_err(|e| format!("Failed to read WallPaper registry value: {}", e))?;

    if path.is_empty() {
        return Err("No wallpaper set (registry value is empty)".to_string());
    }

    Ok(path)
}

#[cfg(target_os = "macos")]
fn get_wallpaper_path() -> Result<String, String> {
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg("tell application \"Finder\" to get POSIX path of (get desktop picture as alias)")
        .output()
        .map_err(|e| format!("Failed to run osascript: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("osascript failed: {}", stderr));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err("No wallpaper path returned by osascript".to_string());
    }

    Ok(path)
}

#[cfg(target_os = "linux")]
fn get_wallpaper_path() -> Result<String, String> {
    // Try GNOME first (most common)
    if let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.background", "picture-uri"])
        .output()
    {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // gsettings returns 'file:///path/to/image' — strip quotes and file:// prefix
            let path = raw
                .trim_matches('\'')
                .trim_matches('"')
                .strip_prefix("file://")
                .unwrap_or(&raw)
                .to_string();
            // URL-decode percent-encoded characters (e.g. %20 → space)
            let path = percent_decode(&path);

            if !path.is_empty() && std::path::Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    // Try GNOME dark variant
    if let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.background", "picture-uri-dark"])
        .output()
    {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let path = raw
                .trim_matches('\'')
                .trim_matches('"')
                .strip_prefix("file://")
                .unwrap_or(&raw)
                .to_string();
            let path = percent_decode(&path);

            if !path.is_empty() && std::path::Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    // Try KDE Plasma
    let kde_config = dirs::config_dir()
        .map(|d| d.join("plasma-org.kde.plasma.desktop-appletsrc"));
    if let Some(config_path) = kde_config {
        if config_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&config_path) {
                for line in contents.lines() {
                    if let Some(img) = line.strip_prefix("Image=") {
                        let path = img
                            .strip_prefix("file://")
                            .unwrap_or(img)
                            .to_string();
                        if std::path::Path::new(&path).exists() {
                            return Ok(path);
                        }
                    }
                }
            }
        }
    }

    Err("Could not detect wallpaper path on this Linux desktop environment".to_string())
}

// ============================================================================
// Get Desktop Icons
// ============================================================================

/// Get the list of desktop icons with their images
#[tauri::command]
pub async fn get_desktop_icons() -> Result<Vec<DesktopIcon>, String> {
    info!("Getting desktop icons...");

    let desktop_dir = dirs::desktop_dir()
        .ok_or_else(|| "Could not find Desktop directory".to_string())?;

    if !desktop_dir.exists() {
        return Err(format!("Desktop directory does not exist: {}", desktop_dir.display()));
    }

    let mut icons = Vec::new();

    // Read desktop directory entries
    let entries = std::fs::read_dir(&desktop_dir)
        .map_err(|e| format!("Failed to read desktop directory: {}", e))?;

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip hidden files
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(true)
        {
            continue;
        }

        let is_directory = path.is_dir();
        let name = display_name(&path);
        let exec_path = path.to_string_lossy().to_string();

        // Extract icon image (platform-specific)
        let icon_base64 = match extract_icon(&path) {
            Ok(b64) => b64,
            Err(e) => {
                warn!("Failed to extract icon for {}: {}", name, e);
                // Use empty string as fallback — frontend will show a default icon
                String::new()
            }
        };

        icons.push(DesktopIcon {
            name,
            icon_base64,
            exec_path,
            is_directory,
        });
    }

    // On Windows, also enumerate Public Desktop
    #[cfg(target_os = "windows")]
    {
        if let Some(public_desktop) = std::env::var_os("PUBLIC") {
            let public_desktop = std::path::PathBuf::from(public_desktop).join("Desktop");
            if public_desktop.exists() {
                if let Ok(entries) = std::fs::read_dir(&public_desktop) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(true) {
                            continue;
                        }
                        let is_directory = path.is_dir();
                        let name = display_name(&path);
                        let exec_path = path.to_string_lossy().to_string();
                        let icon_base64 = extract_icon(&path).unwrap_or_default();
                        icons.push(DesktopIcon { name, icon_base64, exec_path, is_directory });
                    }
                }
            }
        }
    }

    info!("Found {} desktop icons", icons.len());
    Ok(icons)
}

// ============================================================================
// Icon Extraction (platform-specific)
// ============================================================================

/// Extract icon for a file/directory as base64 PNG
#[cfg(target_os = "windows")]
fn extract_icon(path: &std::path::Path) -> Result<String, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
    use windows::Win32::Graphics::Gdi::*;

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let mut shfi = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            windows::core::PCWSTR(wide_path.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };

    if result == 0 || shfi.hIcon.is_invalid() {
        return Err("SHGetFileInfoW failed to get icon".to_string());
    }

    // Convert HICON to PNG bytes using the image crate
    // This is a simplified approach — get icon bitmap info
    let icon_base64 = hicon_to_base64_png(shfi.hIcon)?;

    unsafe { let _ = DestroyIcon(shfi.hIcon); }

    Ok(icon_base64)
}

#[cfg(target_os = "windows")]
fn hicon_to_base64_png(_hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Result<String, String> {
    // TODO: Full HICON → PNG conversion using GetIconInfo + GetDIBits
    // For now, return empty to use frontend fallback icons
    // This will be implemented in a follow-up with proper GDI bitmap extraction
    Err("HICON to PNG conversion not yet implemented".to_string())
}

#[cfg(target_os = "macos")]
fn extract_icon(path: &std::path::Path) -> Result<String, String> {
    // Use sips to convert the file's icon to PNG
    let tmp_path = format!("/tmp/mw_icon_{}.png", std::process::id());

    let output = std::process::Command::new("sips")
        .args(["-s", "format", "png", "--out", &tmp_path])
        .arg(path)
        .output()
        .map_err(|e| format!("Failed to run sips: {}", e))?;

    if !output.status.success() {
        // Fallback: try qlmanage for thumbnail
        let output = std::process::Command::new("qlmanage")
            .args(["-t", "-s", "64", "-o", "/tmp"])
            .arg(path)
            .output()
            .map_err(|e| format!("Failed to run qlmanage: {}", e))?;

        if !output.status.success() {
            return Err("Both sips and qlmanage failed".to_string());
        }

        // qlmanage outputs to /tmp/<filename>.png
        let ql_path = format!(
            "/tmp/{}.png",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("icon")
        );
        let result = file_to_base64(std::path::Path::new(&ql_path));
        let _ = std::fs::remove_file(&ql_path);
        return result;
    }

    let result = file_to_base64(std::path::Path::new(&tmp_path));
    let _ = std::fs::remove_file(&tmp_path);
    result
}

#[cfg(target_os = "linux")]
fn extract_icon(path: &std::path::Path) -> Result<String, String> {
    // For .desktop files, parse the Icon field and look up in icon theme
    if path.extension().and_then(|e| e.to_str()) == Some("desktop") {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if let Some(icon_name) = line.strip_prefix("Icon=") {
                    return find_linux_icon(icon_name.trim());
                }
            }
        }
    }

    // For regular files/directories, try to find a generic icon
    if path.is_dir() {
        return find_linux_icon("folder");
    }

    // Try to match by MIME type using xdg-mime
    if let Ok(output) = std::process::Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .output()
    {
        if output.status.success() {
            let mime = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // Convert MIME to icon name: application/pdf → application-pdf
            let icon_name = mime.replace('/', "-");
            if let Ok(b64) = find_linux_icon(&icon_name) {
                return Ok(b64);
            }
        }
    }

    Err("Could not find icon for this file type".to_string())
}

#[cfg(target_os = "linux")]
fn find_linux_icon(icon_name: &str) -> Result<String, String> {
    // If icon_name is an absolute path, just read it
    if icon_name.starts_with('/') {
        let path = std::path::Path::new(icon_name);
        if path.exists() {
            return file_to_base64(path);
        }
    }

    // Try common icon theme directories
    let icon_sizes = ["48x48", "64x64", "32x32", "scalable"];
    let icon_categories = ["apps", "places", "mimetypes", "devices", "actions"];
    let icon_extensions = ["png", "svg"];

    // Get current icon theme
    let theme = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                        .to_string(),
                )
            } else {
                None
            }
        })
        .unwrap_or_else(|| "hicolor".to_string());

    let base_dirs = [
        format!("/usr/share/icons/{}", theme),
        "/usr/share/icons/hicolor".to_string(),
        format!(
            "{}/.local/share/icons/{}",
            std::env::var("HOME").unwrap_or_default(),
            theme
        ),
        "/usr/share/pixmaps".to_string(),
    ];

    for base in &base_dirs {
        // Direct check in pixmaps
        if base.ends_with("pixmaps") {
            for ext in &icon_extensions {
                let path = format!("{}/{}.{}", base, icon_name, ext);
                if std::path::Path::new(&path).exists() {
                    return file_to_base64(std::path::Path::new(&path));
                }
            }
            continue;
        }

        for size in &icon_sizes {
            for category in &icon_categories {
                for ext in &icon_extensions {
                    let path = format!("{}/{}/{}/{}.{}", base, size, category, icon_name, ext);
                    if std::path::Path::new(&path).exists() {
                        // Skip SVG for base64 (we'd need to rasterize) — prefer PNG
                        if *ext == "svg" {
                            continue;
                        }
                        return file_to_base64(std::path::Path::new(&path));
                    }
                }
            }
        }
    }

    Err(format!("Icon '{}' not found in any theme directory", icon_name))
}

// ============================================================================
// Open Desktop Item
// ============================================================================

/// Open a desktop item (file, folder, or application) using the system handler
#[tauri::command]
pub async fn open_desktop_item(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    info!("Opening desktop item: {}", path);

    // Validate the path exists
    let file_path = std::path::Path::new(&path);
    if !file_path.exists() {
        return Err(format!("Path does not exist: {}", path));
    }

    // Use the opener plugin to open with the system handler
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("Failed to open item: {}", e))?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mime_from_path() {
        assert_eq!(mime_from_path(std::path::Path::new("test.png")), "image/png");
        assert_eq!(mime_from_path(std::path::Path::new("test.jpg")), "image/jpeg");
        assert_eq!(mime_from_path(std::path::Path::new("test.bmp")), "image/bmp");
    }

    #[test]
    fn test_display_name() {
        assert_eq!(display_name(std::path::Path::new("/home/user/Desktop/Firefox.desktop")), "Firefox");
        assert_eq!(display_name(std::path::Path::new("/home/user/Desktop/Document.pdf")), "Document");
    }
}
