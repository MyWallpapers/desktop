//! System tray functionality
//!
//! Rich tray menu with layer controls, edit mode, hub access, and settings.

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};
use tracing::{debug, info};

/// Emit a tray action to the Tauri webview window.
fn emit_tray_action(app: &AppHandle, action: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
        let _ = window.emit("tray-action", action);
    }
}

/// Setup the system tray with icon and enriched menu
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    info!("Setting up system tray...");

    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .unwrap_or_else(|_| Image::new_owned(vec![255u8; 32 * 32 * 4], 32, 32));

    // Layers submenu (dynamically populated via frontend events)
    let layers_placeholder = MenuItemBuilder::with_id("layers_placeholder", "No layers loaded")
        .enabled(false)
        .build(app)?;

    let layers_submenu = SubmenuBuilder::with_id(app, "layers", "Layers")
        .item(&layers_placeholder)
        .build()?;

    // Menu items
    let edit_layout = MenuItemBuilder::with_id("edit_layout", "Edit Layout").build(app)?;

    let open_hub = MenuItemBuilder::with_id("open_hub", "Open Hub").build(app)?;

    let settings = MenuItemBuilder::with_id("settings", "Settings").build(app)?;

    let check_updates =
        MenuItemBuilder::with_id("check_updates", "Check for Updates").build(app)?;

    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&layers_submenu)
        .separator()
        .item(&edit_layout)
        .item(&open_hub)
        .item(&settings)
        .separator()
        .item(&check_updates)
        .item(&quit_item)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("MyWallpaper Desktop")
        .menu(&menu)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();
            match id {
                "quit" => {
                    info!("Quit triggered from tray");
                    app.exit(0);
                }
                "edit_layout" => {
                    info!("Edit layout triggered from tray");
                    emit_tray_action(app, "edit_layout");
                }
                "open_hub" => {
                    info!("Open hub triggered from tray");
                    emit_tray_action(app, "open_hub");
                }
                "settings" => {
                    info!("Settings triggered from tray");
                    emit_tray_action(app, "settings");
                }
                "check_updates" => {
                    info!("Check updates triggered from tray");
                    emit_tray_action(app, "check_updates");
                }
                _ => {
                    // Handle dynamic layer toggle items (prefixed with "layer_")
                    if let Some(layer_id) = id.strip_prefix("layer_") {
                        info!("Toggle layer from tray: {}", layer_id);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.emit("tray-toggle-layer", layer_id);
                        }
                    }
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    debug!("Tray icon clicked");
                    if let Some(window) = tray.app_handle().get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        })
        .build(app)?;

    info!("System tray setup complete");
    Ok(())
}
