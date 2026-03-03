//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.

mod commands;
mod discord;
pub mod error;
pub mod events;
mod media;
mod system_monitor;
mod tray;
mod window_layer;

use log::{error, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

static MW_INIT_SCRIPT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"window.__MW_INIT__ = {{ isTauri: true, platform: "{}", arch: "{}", appVersion: "{}", tauriVersion: "{}", debug: {} }};"#,
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        tauri::VERSION,
        cfg!(debug_assertions),
    )
});

pub fn main() {
    // Keep at most 5 log files, delete older ones.
    #[cfg(target_os = "windows")]
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) {
        let log_dir = base.join("com.mywallpaper.desktop").join("logs");
        if let Ok(canonical) = log_dir.canonicalize() {
            if canonical.starts_with(base.canonicalize().unwrap_or_default()) {
                if let Ok(mut logs) = std::fs::read_dir(&canonical).map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
                        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
                        .collect::<Vec<_>>()
                }) {
                    logs.sort_by(|a, b| b.1.cmp(&a.1));
                    logs.into_iter().skip(5).for_each(|(p, _)| {
                        let _ = std::fs::remove_file(p);
                    });
                }
            }
        }
    }

    info!(
        "[main] Starting MyWallpaper Desktop v{} ({}/{})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    start_with_tauri_webview();
}

fn start_with_tauri_webview() {
    use events::{AppEvent, EmitAppEvent};
    use tauri::{webview::PageLoadEvent, Listener, Manager};

    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .clear_targets()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Webview,
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: None },
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stdout,
                ))
                .build(),
        )
        .plugin(tauri_plugin_process::init())
        // MacosLauncher is required by the API but inert on Windows
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            args.into_iter()
                .filter_map(|a| commands::validate_deep_link(&a))
                .for_each(|url| {
                    let _ = app.emit_app_event(&AppEvent::DeepLink { url });
                });
        }))
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                let _ = webview.eval(&*MW_INIT_SCRIPT);
            }
            if payload.event() == PageLoadEvent::Finished {
                // Heartbeat: frontend pings every 5s so backend can detect unresponsive WebView
                let _ = webview.eval(
                    r#"
                    if (!window.__MW_HEARTBEAT__) {
                        window.__MW_HEARTBEAT__ = true;
                        setInterval(() => {
                            if (window.__TAURI__?.event) {
                                window.__TAURI__.event.emit('webview-heartbeat');
                            }
                        }, 5000);
                    }
                    "#,
                );
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            if let Err(e) = tray::setup_tray(&handle) {
                error!("[setup] Failed to setup system tray: {}", e);
            }

            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(event.payload()) {
                    urls.into_iter()
                        .filter_map(|u| commands::validate_deep_link(&u))
                        .for_each(|url| {
                            let _ = deep_link_handle.emit_app_event(&AppEvent::DeepLink { url });
                        });
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_background_color(Some(tauri::webview::Color(0, 0, 0, 255)));
                window_layer::setup_desktop_window(&window);
                let _ = window.show();
            }

            system_monitor::start_monitor(handle.clone(), 3);
            discord::init();

            // WebView heartbeat watchdog — auto-reload if frontend stops responding
            fn now_secs() -> u64 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            }
            let last_heartbeat = Arc::new(AtomicU64::new(now_secs()));
            let hb = last_heartbeat.clone();
            handle.listen("webview-heartbeat", move |_| {
                hb.store(now_secs(), Ordering::Relaxed);
            });

            let hb_handle = handle.clone();
            let hb_ref = last_heartbeat.clone();
            std::thread::spawn(move || {
                use std::time::Duration;
                use tauri::Manager;
                // Grace period for initial page load
                std::thread::sleep(Duration::from_secs(30));
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    let elapsed = now_secs() - hb_ref.load(Ordering::Relaxed);
                    if elapsed > 15 {
                        warn!("[heartbeat] WebView unresponsive ({}s), reloading", elapsed);
                        if let Some(w) = hb_handle.get_webview_window("main") {
                            let _ = w.eval("window.location.reload()");
                            hb_ref.store(now_secs(), Ordering::Relaxed);
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_system_info,
            commands::get_system_data,
            commands::subscribe_system_data,
            commands::check_for_updates,
            commands::download_and_install_update,
            commands::restart_app,
            commands::open_oauth_in_browser,
            commands::reload_window,
            commands::get_media_info,
            commands::media_play_pause,
            commands::media_next,
            commands::media_prev,
            commands::update_discord_presence,
            window_layer::set_desktop_icons_visible,
        ])
        .build(tauri::generate_context!())
        .expect("Error while building MyWallpaper Desktop");

    app.run(|_app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = event {
            window_layer::restore_desktop_icons_and_unhook();
        }
    });
}
