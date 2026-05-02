# AWP

## Building from source

1. Clone the repo
2. Double-click `INSTALL.bat`
3. Wait for all dependencies to install
4. Run `npx tauri build` in the project folder
5. Installer will be at `src-tauri\target\release\bundle\nsis\ui_0.0.1_x64-setup.exe`

The setup script automatically installs:
- Node.js LTS
- Rust (with MSVC toolchain)
- Visual Studio Build Tools (C++ workload + Windows 11 SDK)
- WebView2 Runtime

If you already have any of these they will be skipped.

## Requirements
- Windows 10 or 11 (x64)
- Internet connection during first build (to download Rust crates)
