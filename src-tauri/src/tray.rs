//! System tray — toggle window layer mode + quit.

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};
use tracing::{debug, info, warn};

use crate::window_layer::{self, WindowLayerMode, WindowLayerState};

/// Setup the system tray with icon, toggle mode, and quit menu
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    info!("Setting up system tray...");

    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .unwrap_or_else(|_| Image::new_owned(vec![255u8; 32 * 32 * 4], 32, 32));

    let toggle_item =
        MenuItemBuilder::with_id("toggle_mode", "Toggle Wallpaper Mode").build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&toggle_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("MyWallpaper Desktop")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "toggle_mode" => {
                let state = app.state::<WindowLayerState>();
                let new_mode = {
                    let current = state.mode.lock().unwrap();
                    match *current {
                        WindowLayerMode::Desktop => WindowLayerMode::Interactive,
                        WindowLayerMode::Interactive => WindowLayerMode::Desktop,
                    }
                };

                info!("Tray: toggling to {:?}", new_mode);

                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = window_layer::apply_layer_mode_pub(&window, new_mode) {
                        warn!("Tray: failed to apply layer mode: {}", e);
                        return;
                    }
                }

                let mut current = state.mode.lock().unwrap();
                *current = new_mode;
                let _ = app.emit("layer-mode-changed", new_mode);
            }
            "quit" => {
                info!("Quit triggered from tray");
                app.exit(0);
            }
            _ => {}
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
