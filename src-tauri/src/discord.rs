//! Discord Rich Presence — shows "Using MyWallpaper" in Discord.
//! Fails silently if Discord is not running.

use crate::error::{AppError, AppResult};
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use log::{info, warn};
use std::sync::Mutex;

// MyWallpaper Discord application ID (create at https://discord.com/developers/applications)
const DISCORD_APP_ID: &str = "1307092087033782272";

static CLIENT: Mutex<Option<DiscordIpcClient>> = Mutex::new(None);

fn build_activity<'a>(details: &'a str, state: &'a str) -> activity::Activity<'a> {
    activity::Activity::new()
        .state(state)
        .details(details)
        .assets(
            activity::Assets::new()
                .large_image("logo")
                .large_text("MyWallpaper Desktop"),
        )
}

/// Connect to Discord RPC. Fails silently if Discord is not running.
pub fn init() {
    std::thread::spawn(|| match DiscordIpcClient::new(DISCORD_APP_ID) {
        Ok(mut client) => {
            if client.connect().is_ok() {
                let _ =
                    client.set_activity(build_activity("Using MyWallpaper", "Animated Wallpaper"));
                *CLIENT.lock().unwrap() = Some(client);
                info!("[discord] Rich Presence connected");
            } else {
                warn!("[discord] Discord not running, skipping Rich Presence");
            }
        }
        Err(e) => {
            warn!("[discord] Failed to create IPC client: {}", e);
        }
    });
}

/// Update the Discord Rich Presence activity.
pub fn update_presence(details: &str, state: &str) -> AppResult<()> {
    if let Ok(mut guard) = CLIENT.lock() {
        if let Some(ref mut client) = *guard {
            client
                .set_activity(build_activity(details, state))
                .map_err(|e| AppError::Discord(e.to_string()))?;
        }
    }
    Ok(())
}
