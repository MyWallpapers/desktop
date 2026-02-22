// Prevents additional console window on Windows in release
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    // =========================================================================
    // FIX MAJEUR WEBVIEW2 : ANTI-OCCLUSION & BACKGROUNDING
    // =========================================================================
    // Empêche Chromium/WebView2 de suspendre le rendu graphique et l'écoute
    // des événements lorsqu'il est injecté derrière les icônes du bureau.
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--disable-features=CalculateNativeWinOcclusion,CalculateWindowOcclusion --disable-backgrounding-occluded-windows"
    );

    mywallpaper_desktop_lib::main();
}
