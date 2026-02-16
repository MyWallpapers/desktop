//! System tray — minimal menu with Quit action.

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use tracing::{debug, info};

/// Setup the system tray with icon and quit menu
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    info!("Setting up system tray...");

    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .unwrap_or_else(|_| Image::new_owned(vec![255u8; 32 * 32 * 4], 32, 32));

    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app).item(&quit_item).build()?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("MyWallpaper Desktop")
        .menu(&menu)
        .on_menu_event(move |app, event| {
            if event.id().as_ref() == "quit" {
                info!("Quit triggered from tray");
                app.exit(0);
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
