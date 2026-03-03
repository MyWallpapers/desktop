//! System tray — quit.

use log::{error, info};
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png")).unwrap_or_else(|_| {
        error!("[tray] Failed to load icon, using fallback.");
        Image::new_owned(vec![255u8; 32 * 32 * 4], 32, 32)
    });

    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app).item(&quit_item).build()?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("MyWallpaper Desktop")
        .menu(&menu)
        .on_menu_event(move |app, event| {
            if event.id().as_ref() == "quit" {
                crate::window_layer::restore_desktop_icons_and_unhook();
                app.exit(0);
            }
        })
        .build(app)?;

    info!("[tray] System tray ready.");
    Ok(())
}
