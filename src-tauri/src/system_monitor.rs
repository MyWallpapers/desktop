//! System data collection for widget consumption.
//!
//! Provides one-shot and real-time system metrics (CPU, memory, battery, disk, network,
//! GPU, display, audio, uptime) that the frontend filters per-widget based on manifest permissions.

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<Vec<DisplayInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioInfo>,
    /// Seconds since system boot
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<u64>,
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

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    /// GPU name (e.g., "NVIDIA GeForce RTX 4090")
    pub name: String,
    /// VRAM in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vram: Option<u64>,
    /// GPU usage percentage (0-100)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<f32>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    /// Display name/model
    pub name: String,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Refresh rate in Hz
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_rate: Option<u32>,
    /// Scale factor / DPI multiplier (e.g., 1.0, 1.25, 1.5, 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_factor: Option<f32>,
    /// Whether this is the primary display
    pub primary: bool,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInfo {
    /// System volume level (0.0 - 1.0)
    pub volume: f32,
    /// Whether the system audio is muted
    pub muted: bool,
    /// Name of the current output device
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
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
// GPU — DXGI (Windows)
// ============================================================================

#[cfg(target_os = "windows")]
fn collect_gpu_info() -> Option<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;

        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();

        Some(GpuInfo {
            name,
            vram: Some(desc.DedicatedVideoMemory as u64),
            usage: None, // DXGI doesn't expose real-time GPU usage
        })
    }
}

#[cfg(not(target_os = "windows"))]
fn collect_gpu_info() -> Option<GpuInfo> {
    None
}

// ============================================================================
// Display — GDI (Windows)
// ============================================================================

#[cfg(target_os = "windows")]
fn collect_display_info() -> Option<Vec<DisplayInfo>> {
    use std::mem::{size_of, zeroed};
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    };
    use windows::Win32::UI::HiDpi::GetDpiForSystem;

    unsafe {
        let mut displays = Vec::new();
        let mut idx = 0u32;

        loop {
            let mut dd: DISPLAY_DEVICEW = zeroed();
            dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;

            if !EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dd, 0).as_bool() {
                break;
            }

            // Only active displays
            if dd.StateFlags & 1 != 0 {
                // DISPLAY_DEVICE_ACTIVE
                let adapter_name = PCWSTR(dd.DeviceName.as_ptr());
                let mut dm: DEVMODEW = zeroed();
                dm.dmSize = size_of::<DEVMODEW>() as u16;

                if EnumDisplaySettingsW(adapter_name, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
                    // Try getting the monitor name (second-level enumeration)
                    let mut monitor: DISPLAY_DEVICEW = zeroed();
                    monitor.cb = size_of::<DISPLAY_DEVICEW>() as u32;
                    let name = if EnumDisplayDevicesW(adapter_name, 0, &mut monitor, 0).as_bool() {
                        String::from_utf16_lossy(&monitor.DeviceString)
                            .trim_end_matches('\0')
                            .to_string()
                    } else {
                        String::from_utf16_lossy(&dd.DeviceString)
                            .trim_end_matches('\0')
                            .to_string()
                    };

                    let primary = dd.StateFlags & 4 != 0; // DISPLAY_DEVICE_PRIMARY_DEVICE

                    displays.push(DisplayInfo {
                        name,
                        width: dm.dmPelsWidth,
                        height: dm.dmPelsHeight,
                        refresh_rate: if dm.dmDisplayFrequency > 0 {
                            Some(dm.dmDisplayFrequency)
                        } else {
                            None
                        },
                        scale_factor: None, // Set below
                        primary,
                    });
                }
            }

            idx += 1;
        }

        // Apply system DPI as scale factor
        let dpi = GetDpiForSystem();
        let scale = dpi as f32 / 96.0;
        for d in &mut displays {
            d.scale_factor = Some(scale);
        }

        if displays.is_empty() {
            None
        } else {
            Some(displays)
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn collect_display_info() -> Option<Vec<DisplayInfo>> {
    None
}

// ============================================================================
// Audio — WASAPI (Windows)
// ============================================================================

#[cfg(target_os = "windows")]
fn collect_audio_info() -> Option<AudioInfo> {
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, IAudioEndpointVolume, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    unsafe {
        // Ensure COM is initialized for this thread (no-op if already done)
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .ok()?;
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;

        let level = volume.GetMasterVolumeLevelScalar().ok()?;
        let muted = volume.GetMute().ok()?.as_bool();

        Some(AudioInfo {
            volume: level,
            muted,
            output_device: None, // Requires IPropertyStore + PKEY_Device_FriendlyName
        })
    }
}

#[cfg(not(target_os = "windows"))]
fn collect_audio_info() -> Option<AudioInfo> {
    None
}

// ============================================================================
// Background Monitor
// ============================================================================

/// Collect system data using a reusable System instance (for the background monitor).
fn collect_with_system(sys: &mut sysinfo::System, categories: &[String]) -> SystemData {
    let mut data = SystemData::default();
    let needs = |cat: &str| categories.iter().any(|c| c == cat);

    if needs("cpu") {
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

    if needs("memory") {
        sys.refresh_memory();
        data.memory = Some(MemoryInfo {
            total: sys.total_memory(),
            used: sys.used_memory(),
            free: sys.available_memory(),
        });
    }

    if needs("disk") {
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

    if needs("network") {
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

    if needs("battery") {
        data.battery = collect_battery_info();
    }
    if needs("media") {
        data.media = crate::media::get_media_info().ok();
    }
    if needs("gpu") {
        data.gpu = collect_gpu_info();
    }
    if needs("display") {
        data.display = collect_display_info();
    }
    if needs("audio") {
        data.audio = collect_audio_info();
    }
    if needs("uptime") {
        data.uptime = Some(sysinfo::System::uptime());
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
