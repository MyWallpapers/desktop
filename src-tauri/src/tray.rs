//! System tray — quit.

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use log::{debug, info, error};

/// Setup the system tray with icon and quit menu
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    info!("[tray] Setting up system tray components...");

    debug!("[tray] Loading 32x32.png icon...");
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .unwrap_or_else(|_| {
            error!("[tray] Failed to load 32x32.png, using fallback transparent image.");
            Image::new_owned(vec![255u8; 32 * 32 * 4], 32, 32)
        });

    debug!("[tray] Building tray menu...");
    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&quit_item)
        .build()?;

    debug!("[tray] Registering tray icon...");
    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("MyWallpaper Desktop")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "quit" => {
                info!("[tray] Quit triggered from tray context menu.");

                // SAFETY: Restore icons across OS before quitting.
                crate::window_layer::restore_desktop_icons();

                app.exit(0);
            }
            _ => {
                debug!("[tray] Unhandled menu event ID: {}", event.id().as_ref());
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    debug!("[tray] Tray icon Left-Clicked. Showing main window.");
                    if let Some(window) = tray.app_handle().get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    } else {
                        error!("[tray] Main window not found on tray click!");
                    }
                }
            }
        })
        .build(app)?;

    info!("[tray] System tray setup complete.");
    Ok(())
}
