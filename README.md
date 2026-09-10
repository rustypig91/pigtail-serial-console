# Rusty's Pigtail - Serial Terminal

A desktop serial terminal built in Rust with [egui](https://github.com/emilk/egui).

Most serial terminals just show you text. Rusty's Pigtail is built around the parts of serial debugging that actually cause pain:

- **Reconnects by device identity, not port path.** When a target resets and re-enumerates on a new port, Rusty's Pigtail finds it again automatically.
- **Filtering reveals history, not just new output.** Type a filter and it applies retroactively to everything already captured, not only what arrives afterward.
- **Live plotting linked to the log.** Numeric values extracted from the stream (`temp:23.4, rpm:1200`, or a regex) are plotted live, and clicking a plot point jumps to the log line that produced it. Adding or editing a rule re-reads the whole session, so it also plots the output that already scrolled past.
- **Nothing is lost.** Raw bytes are written to disk continuously as they arrive; the UI is just a view over that capture.

## Features

- Auto-reconnect by USB VID/PID/serial number, with a visible marker showing exactly where a gap occurred
- Regex or plain-text filtering and search over full scrollback
- Highlight rules (color/bold by pattern)
- Multiple ports as persistent, user-named tabs, plus optional merged views that label
  and interleave selected connections by timestamp.
  Choose **New merged view** from the **+** menu and select the
  connections to include. You can open several merged views and close each
  with a middle-click or its tab's right-click menu. Merged views can start
  empty; right-click their tab and choose **Options** to change
  its name and included connections at any time. Drag merged tabs alongside
  connection tabs to reorder them. Closing either kind of tab asks for confirmation
  unless you have disabled that preference.
- Hex view alongside the text view
- Optional **ANSI/VT** screen for interactive shells and device menus: cursor
  positioning, erase and redraw operations, scrolling regions, styled/colored
  text, and alternate screen buffers. Select **Log**, **Hex**, or **ANSI/VT** in
  the footer; Log is the default for new connections, and each connection remembers
  its selected view across restarts. **Settings → VT scrollback rows** controls retained
  VT history per connection (default 2,000, range 0–100,000; 0 disables scrollback).
  The setting persists across restarts and applies to open connections when editing
  finishes, rebuilding from retained receive data. ANSI/VT history is restored
  across app restarts by replaying the same saved raw
  session captures used by Log and Hex. Prior captures are separated by session
  markers; the existing history restore budget and clear-history boundaries apply.
  The VT grid retains the configured number of scrolled rows, while the full raw capture
  remains on disk. Use the mouse wheel or scrollbar to review it;
  **Pin** or typing returns to live output. New output preserves your scroll position.
  **Ctrl+Shift+F** in ANSI/VT searches the screen and retained scrollback
  with regex and optional case sensitivity; **Next**/**Prev** (Enter/Shift+Enter)
  scroll to highlighted matches. Switch to Log to search older output. Raw capture and chronological
  history continue in every mode. Screen mode supports application cursor keys
  and bracketed paste, and sends Up/Down to the device even when local history is enabled.
  The screen fills the available console area and automatically adjusts its rows
  and columns when the window or font size changes. Plain serial has no resize
  notification protocol, so configure the device shell to match if needed. Connection
  interruptions and dropped live output discard partial escape sequences while
  preserving VT history. Clear console clears VT history too. Alternate-screen
  applications retain the main screen's history but do not add their redraws to it.
  Merged views remain chronological logs.
- Transmit with configurable line endings, send history, and hex input
- Drop files onto a console, or use **Send file…**, to send raw bytes, paced text lines, or hex-decoded data
- Named transmit macros with reorderable command, delay, and regex wait steps,
  finite or indefinite looping, and assignable Ctrl+Shift+0 through Ctrl+Shift+9 shortcuts
- DTR/RTS toggles and break signal
- Export the current (filtered) view to `.txt` or `.csv`
- Startup notice when a newer release is published, with "skip this version" and
  one-click download and update. Startup checks can be switched off in Settings.

## Installing

Every release publishes installers alongside the plain binaries on the
[releases page](https://github.com/rustypig91/pigtail-serial-console/releases):

| Platform | Asset | Notes |
| --- | --- | --- |
| Windows | `pigtail-v<version>-x86_64-setup.exe` | The one most people want. Install wizard with an optional desktop shortcut; recommends installing for the current user in AppData, with an all-users option. |
| Windows | `pigtail-v<version>-x86_64-pc-windows-msvc.msi` | Same application, for scripted or managed deployment (`msiexec /i ... /qn`, Group Policy). Adds a Start Menu entry and an Add/Remove Programs entry. |
| Debian/Ubuntu | `pigtail_<version>-1_amd64.deb` | `sudo apt install ./pigtail_<version>-1_amd64.deb` — pulls in its own dependencies and registers a desktop entry. |
| Any Linux | `pigtail-v<version>-x86_64.AppImage` | `chmod +x` and run; no installation, bundles its libraries. Needs the host's GPU drivers for OpenGL. |
| Portable | `.zip` / `.tar.gz` | Just the binary, no installation. |

The setup wizard offers the installation scope even when started as administrator
or when Pigtail is already installed. "Install for me" defaults to
`%LOCALAPPDATA%\Programs\Rusty's Pigtail - Serial Terminal`; "Install for all users"
uses Program Files. Selecting a different scope creates a separate installation;
it does not move or uninstall the previous copy. If setup is run using another
account's administrator credentials, the per-user destination belongs to that
account. Launch setup normally to install into your own profile.

Neither Windows installer is code-signed, so SmartScreen shows an
"unrecognized app" warning on first run; choose "More info" → "Run anyway".
Install one or the other, not both — Windows treats them as separate products
and each keeps its own Add/Remove Programs entry.

### Updating

Press **Update** in the update notice to download, verify, install, and restart
Rusty's Pigtail. Downloads run in the background and show progress; failed downloads
leave the current installation untouched and can be retried. Settings are saved
before installation; active connections close when the application restarts.
Windows setup installations use the setup installer and may show a Windows
permission prompt. Portable Windows/Linux builds and writable AppImages update
in place. Installations managed by MSI or a Linux package manager should be
updated with that installer/package manager, especially in protected folders.
Debian package installations still check for updates and show release notices, but disable in-app installation.
To update, download the latest `.deb` and install it with `sudo apt install ./pigtail_<version>-1_amd64.deb`.

Automatic updates require the matching release asset and its GitHub SHA-256
digest; older releases without these assets cannot be installed automatically.

## Building

Requires stable Rust.

On Linux, install the development headers for udev (serial port access) and GTK 3 (file dialogs) first:

```sh
sudo apt install libudev-dev libgtk-3-dev
```

Then build:

```sh
cargo build --release
```

Run in development with `cargo run -p pigtail`.

### Building the packages

To build all release artifacts for your current platform, run one of these
commands from the repository root:

```powershell
# Windows (Windows PowerShell 5.1 or PowerShell 7)
.\scripts\build-release.cmd
```

```sh
# x86_64 Linux (Debian/Ubuntu)
bash scripts/build-release.sh
```

The Windows `.cmd` launcher runs the PowerShell build script with
`-ExecutionPolicy Bypass` for that process only, allowing this unsigned local
script without changing your saved execution policy. You can also run it directly:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-release.ps1
```

Both scripts read the workspace version automatically and build the whole
workspace in release mode using `Cargo.lock`. Results go into
`target/release-assets/<target>/`. Windows produces a ZIP, standalone updater
EXE, MSI, and setup EXE; Linux produces a tar.gz, standalone updater executable,
DEB, and AppImage. Run each script on its respective OS; these are native x64
builds. They only build packages; they do not publish a release.

Install Rust via rustup with a toolchain meeting the `rust-version` in
`Cargo.toml`. Windows also needs Visual Studio Build Tools with the
**Desktop development with C++** workload and Windows SDK. The Windows script
downloads WiX 3.14.1 and portable Inno Setup 6.7.3 (matching CI) on first use
and caches them under `target/release-tools`; no global installer-tool setup
is needed.

On Debian/Ubuntu, install the prerequisites first:

```sh
sudo apt-get install build-essential pkg-config libudev-dev python3 curl \
    ca-certificates file binutils dpkg-dev patchelf \
    libx11-6 libxcursor1 libxi6 libxrandr2 libxkbcommon0 libxkbcommon-x11-0
```

The Linux script installs `cargo-deb` if missing. AppImage packaging downloads
linuxdeploy and its AppImage plugin on each run. Internet access is needed for
uncached tools, Rust targets, and dependencies. For Linux compatibility matching
CI, build on Ubuntu 22.04; binaries built on newer distributions may require a
newer glibc on the destination machine.

CI does this on every tag, but each one can be built by hand:

```sh
ISCC /DAppVersion=0.2.0 /DSourceBinDir=target\release \
    crates\pigtail\packaging\windows\pigtail.iss        # setup.exe (needs Inno Setup 6)
cargo wix -p pigtail                                    # Windows .msi (needs cargo-wix + WiX v3)
cargo deb -p pigtail                                    # .deb (needs cargo-deb)
crates/pigtail/packaging/linux/build-appimage.sh \
    target/release/pigtail 0.2.0 .                      # .AppImage
```

## Workspace layout

- `crates/serialcore` — UI-agnostic engine: port enumeration, framing, storage, filtering, extraction. No GUI dependency.
- `crates/pigtail` — the egui application.
- `crates/pigtail/wix` — WiX source for the Windows MSI.
- `crates/pigtail/packaging` — icons, desktop entry, the Inno Setup script for `setup.exe`, and the AppImage build script.

## License

MIT, see [LICENSE](LICENSE).

Use **Ctrl+Shift+Page Up / Page Down** to scroll the console one page up or down.
Use **Ctrl+Shift+Up / Down** to scroll one line up or down.
Plain Page Up / Page Down and arrow keys retain their normal terminal behavior.

Use **Ctrl+Shift+Space** to pin the console to the bottom. On connection tabs,
**Ctrl+Shift+Q / W / E** selects the **Log / Hex / ANSI** view.
Use **Ctrl+Shift+P** to toggle the plot on connection tabs.
