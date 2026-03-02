//! System data collection for widget consumption.
//!
//! Provides one-shot and real-time system metrics (CPU, memory, battery, disk, network)
//! that the frontend filters per-widget based on manifest permissions.

use log::{error, info};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use typeshare::typeshare;

// ============================================================================
// Types
// ============================================================================

#[typeshare]
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<BatteryInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<Vec<DiskInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<NetworkInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<crate::media::MediaInfo>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuInfo {
    pub cores: u32,
    /// CPU usage percentage (0-100)
    pub usage: f32,
    pub model: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInfo {
    /// Total memory in bytes
    pub total: u64,
    /// Used memory in bytes
    pub used: u64,
    /// Free memory in bytes
    pub free: u64,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryInfo {
    /// Battery level (0.0 - 1.0)
    pub level: f32,
    pub charging: bool,
    /// Battery health (0.0 - 1.0), if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<f32>,
    /// Estimated seconds until empty, if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_empty: Option<u64>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    pub name: String,
    /// Total disk space in bytes
    pub total: u64,
    /// Available disk space in bytes
    pub available: u64,
    /// Filesystem type (e.g., "NTFS")
    pub fs: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    pub name: String,
    /// Total bytes received
    pub received: u64,
    /// Total bytes transmitted
    pub transmitted: u64,
}

// ============================================================================
// Monitor State
// ============================================================================

static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
static POLL_CATEGORIES: LazyLock<Arc<Mutex<Vec<String>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

// ============================================================================
// Data Collection
// ============================================================================

/// Collect system data for the requested categories (one-shot).
/// One-shot collection: creates a temporary System, primes CPU baseline, then delegates.
pub fn collect_system_data(categories: &[String]) -> SystemData {
    let mut sys = sysinfo::System::new();

    // CPU needs two refreshes with a gap for accurate readings (no prior baseline)
    if categories.iter().any(|c| c == "cpu") {
        sys.refresh_cpu_usage();
        std::thread::sleep(Duration::from_millis(200));
    }

    collect_with_system(&mut sys, categories)
}

/// Collect battery info. Returns None on desktops without a battery.
fn collect_battery_info() -> Option<BatteryInfo> {
    let manager = battery::Manager::new().ok()?;
    let mut batteries = manager.batteries().ok()?;
    let batt = batteries.next()?.ok()?;

    use battery::State;
    let charging = matches!(batt.state(), State::Charging | State::Full);

    use battery::units::{energy::watt_hour, ratio::ratio, time::second};

    let time_to_empty = batt.time_to_empty().map(|t| t.get::<second>() as u64);

    let health = {
        let full = batt.energy_full().get::<watt_hour>();
        let design = batt.energy_full_design().get::<watt_hour>();
        if design > 0.0 {
            Some(full / design)
        } else {
            None
        }
    };

    Some(BatteryInfo {
        level: batt.state_of_charge().get::<ratio>(),
        charging,
        health,
        time_to_empty,
    })
}

// ============================================================================
// Background Monitor
// ============================================================================

/// Collect system data using a reusable System instance (for the background monitor).
fn collect_with_system(sys: &mut sysinfo::System, categories: &[String]) -> SystemData {
    let mut data = SystemData::default();

    let needs_cpu = categories.iter().any(|c| c == "cpu");
    let needs_memory = categories.iter().any(|c| c == "memory");
    let needs_disk = categories.iter().any(|c| c == "disk");
    let needs_network = categories.iter().any(|c| c == "network");
    let needs_battery = categories.iter().any(|c| c == "battery");
    let needs_media = categories.iter().any(|c| c == "media");

    if needs_cpu {
        sys.refresh_cpu_usage();

        let cpus = sys.cpus();
        let usage: f32 = if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32
        };
        let model = cpus
            .first()
            .map(|c| c.brand().to_string())
            .unwrap_or_default();

        data.cpu = Some(CpuInfo {
            cores: cpus.len() as u32,
            usage,
            model,
        });
    }

    if needs_memory {
        sys.refresh_memory();
        data.memory = Some(MemoryInfo {
            total: sys.total_memory(),
            used: sys.used_memory(),
            free: sys.available_memory(),
        });
    }

    if needs_disk {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        data.disk = Some(
            disks
                .iter()
                .map(|d| DiskInfo {
                    name: d.name().to_string_lossy().into_owned(),
                    total: d.total_space(),
                    available: d.available_space(),
                    fs: d.file_system().to_string_lossy().into_owned(),
                })
                .collect(),
        );
    }

    if needs_network {
        let networks = sysinfo::Networks::new_with_refreshed_list();
        data.network = Some(
            networks
                .iter()
                .map(|(name, net)| NetworkInfo {
                    name: name.clone(),
                    received: net.total_received(),
                    transmitted: net.total_transmitted(),
                })
                .collect(),
        );
    }

    if needs_battery {
        data.battery = collect_battery_info();
    }

    if needs_media {
        data.media = crate::media::get_media_info().ok();
    }

    data
}

/// Start the background system monitor thread.
/// Polls at `interval_secs` and emits `system-data-update` events.
pub fn start_monitor(app_handle: tauri::AppHandle, interval_secs: u64) {
    if MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        info!("[system_monitor] Monitor already running");
        return;
    }

    info!(
        "[system_monitor] Starting background monitor ({}s interval)",
        interval_secs
    );

    std::thread::spawn(move || {
        use crate::events::{AppEvent, EmitAppEvent};

        let mut sys = sysinfo::System::new();
        // Initial CPU refresh so the first poll has a baseline
        sys.refresh_cpu_usage();

        let interval = Duration::from_secs(interval_secs);

        while MONITOR_RUNNING.load(Ordering::SeqCst) {
            let categories = POLL_CATEGORIES.lock().unwrap().clone();

            if categories.is_empty() {
                // Rien à poller, on dort et on réessaie
                std::thread::sleep(interval);
                continue;
            }

            let data = collect_with_system(&mut sys, &categories);

            let event = AppEvent::SystemDataUpdate(Box::new(data));
            if let Err(e) = app_handle.emit_app_event(&event) {
                error!("[system_monitor] Failed to emit event: {}", e);
            }

            std::thread::sleep(interval);
        }

        info!("[system_monitor] Monitor stopped");
    });
}

/// Update the categories the monitor polls. Pass empty to pause polling.
pub fn set_poll_categories(categories: Vec<String>) {
    info!("[system_monitor] Poll categories updated: {:?}", categories);
    *POLL_CATEGORIES.lock().unwrap() = categories;
}
