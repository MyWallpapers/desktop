# MyWallpaper Desktop

Tauri v2 desktop application for [MyWallpaper](https://dev.mywallpaper.online) — animated wallpapers with addon support for Windows, macOS, and Linux.

The app runs as a system tray application. The window covers the full screen with no decorations, sitting behind all other windows (desktop layer). The frontend is loaded remotely from `dev.mywallpaper.online` — no local frontend build required.

## Architecture

```
src-tauri/
├── src/
│   ├── main.rs            # Entry point
│   ├── lib.rs             # App init, plugins, window setup
│   ├── commands.rs        # Tauri IPC commands
│   ├── commands_core.rs   # Platform-independent business logic
│   ├── tray.rs            # System tray (quit only)
│   └── window_layer.rs    # Desktop injection, mouse engine, visibility watchdog
├── icons/                 # App icons (all platforms)
├── capabilities/          # Tauri permission capabilities
├── tauri.conf.json        # Tauri configuration
└── Cargo.toml             # Rust dependencies
```

### Commands (IPC)

| Command | Description |
|---|---|
| `get_system_info` | OS, arch, app version, Tauri version |
| `check_for_updates` | Check for app updates via GitHub releases |
| `download_and_install_update` | Download and install update with progress events |
| `restart_app` | Restart to apply update |
| `open_oauth_in_browser` | Open OAuth URL in default browser |
| `reload_window` | Emit reload event to frontend |
| `set_desktop_icons_visible` | Show/hide native desktop icons |

### Platform-specific behavior

- **Windows**: WorkerW injection (Win11 24H2+ and Legacy), MSAA mouse hook with icon detection, transparent window
- **macOS**: `kCGDesktopWindowLevel` behind desktop icons, Finder-based icon toggle, transparent window

## Releasing

Releases are fully automated via GitHub Actions. Go to **Actions > Desktop Release > Run workflow**:

| Input | Options | Description |
|---|---|---|
| **bump** (required) | `patch` / `minor` / `major` | Version bump type |
| **mode** (required) | `prod` / `dev` | Build optimization level |

The workflow automatically:
1. Bumps version in `tauri.conf.json`, `Cargo.toml`, `package.json`
2. Commits and tags (`vX.Y.Z` or `vX.Y.Z-dev`)
3. Builds for all 4 platforms in parallel
4. Creates a signed GitHub release with all installers

### Build profiles

**prod** — Maximum optimization, smallest binary:
- `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`

**dev** — Fastest compilation, devtools enabled:
- `opt-level = 0`, `lto = false`, `codegen-units = 256`, `incremental = true`
- Includes `--features devtools` for browser inspector

### Auto-updater

The app checks for updates from this repo's releases. Updater artifacts are signed with a minisign keypair. The public key is in `tauri.conf.json`, the private key is stored as a GitHub Actions secret (`TAURI_SIGNING_PRIVATE_KEY`).

Endpoint: `https://github.com/MyWallpapers/app/releases/latest/download/latest.json`

## Development

```bash
npm install
npm run tauri:dev     # Dev mode (connects to dev.mywallpaper.online)
npm run tauri:build   # Local release build
```

### Requirements

- Rust (stable)
- Node.js 20+
- Platform dependencies:
  - **Linux**: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`
  - **macOS**: Xcode Command Line Tools
  - **Windows**: Visual Studio Build Tools, WebView2

## Secrets (GitHub Actions)

| Secret | Description |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Minisign private key for updater signing |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for the signing key |
