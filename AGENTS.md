# Agent guide

This guide applies to the whole repository. Keep [README.md](README.md) focused
on users: the product, installation, basic usage, and a quick source build.
Keep detailed implementation, behavior, validation, packaging, and release
information here. Update both documents when a change affects their guidance.

## Architecture and code map

- `crates/serialcore` — UI-agnostic engine: port enumeration, framing, storage, filtering, extraction. No GUI dependency.
- `crates/pigtail` — the egui application.
- `crates/pigtail/wix` — WiX source for the Windows MSI.
- `crates/pigtail/packaging` — icons, desktop entry, the Inno Setup script for `setup.exe`, and the AppImage build script.

- `crates/serialcore/src` contains session/reader/source handling, framing and
  storage, filtering and extraction, transfers, configuration, and update logic.
- `crates/pigtail/src/app.rs` coordinates application state;
  `terminal.rs` handles terminal state and `panes/` contains the UI components.
- `crates/pigtail/src/demo.rs` implements the hardware-free demo.
- `scripts/` contains native release builds and screenshot capture.
- `.github/workflows/build.yml` defines platform builds and release publishing.
- `docs/receive-load-investigation.md` records receive-load analysis.

Keep `serialcore` independent of GUI dependencies. Put rendering and UI
interaction in `pigtail`. Preserve raw capture independently of filtering,
plotting, and the selected display mode. Treat the behavior reference below as
regression context when changing related code.

## Development and validation

Run commands from the repository root. Use a stable Rust toolchain meeting
`workspace.package.rust-version` in `Cargo.toml`. Versions, edition, and lint
policy are shared at the workspace level; Rust warnings and Clippy's `all`
lints are denied.

For Rust changes, choose checks appropriate to the affected code:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked
cargo check -p pigtail --features demo --locked
```

Use focused crate or test selections during iteration. Check the demo feature
when touching code shared with the demo. For UI changes, inspect the affected
views in a running app or demo; the demo cannot verify real serial I/O,
reconnection, or command responses. For documentation-only edits, review the
diff, links, paths, and command examples; no Rust build is needed.

## Detailed behavior reference

The following describes user-visible behavior to preserve when modifying the
application.

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
- Custom window header with draggable device tabs, status dots, close buttons,
  and minimize/maximize/close controls. Drag empty header space to move the window,
  double-click it to maximize or restore, and drag an edge or corner to resize.
  View controls, search, clear, export, and the app menu sit above the console;
  ANSI/VT sits beside Hex; highlights are in the overflow menu. A thin footer shows
  connection status, line counts, port settings, and view diagnostics, with a
  clickable pin icon for autoscroll. The header tint blends subtly
  with scrolling terminal history underneath it.
- Optional **ANSI/VT** screen for interactive shells and device menus: cursor
  positioning, erase and redraw operations, scrolling regions, styled/colored
  text, and alternate screen buffers. Select **Log**, **Hex**, or **ANSI/VT** in
  the toolbar; Log is the default for new connections, and each connection remembers
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
  using plain text by default, with `.*` to toggle regex and **Aa** for case sensitivity.
  Invalid regex patterns show an amber warning; hover for details. **Next**/**Prev** (Enter/Shift+Enter)
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
- Two resizable console panes, side by side or stacked, each with its own tabs,
  toolbar, status footer, search and optional plot. Open at least two tabs, then choose
  **Split right** or **Split below** from that tab's right-click menu.
  Click a pane to focus it; typing, macros and tab shortcuts target that pane.
  Drag the divider to resize, or double-click it to restore equal sizes.
  Drag tabs within a header to reorder them, or onto the other pane's header
  to move them. You can also use **Move to other pane** in a tab's menu.
  **Close split** in the same menu joins the tabs without disconnecting devices.
  The split orientation, divider position, tab groups, selected tabs, and focused
  pane are restored on restart.
- Transmit with configurable line endings, send history, and hex input
- Drop files onto a console, or use **Send file…**, to send raw bytes, paced text lines, or hex-decoded data
- Named transmit macros with reorderable command, delay, and regex wait steps,
  finite or indefinite looping, and assignable Ctrl+Shift+0 through Ctrl+Shift+9 shortcuts
- Global send delay in Settings (milliseconds between outgoing bytes, default 0),
  applied to typing, paste, macros, and file transfers
- DTR/RTS toggles and break signal
- Export the current (filtered) view to `.txt` or `.csv`
- Startup notice when a newer release is published, with "skip this version" and
  one-click download and update. Startup checks can be switched off in Settings.

### Keyboard shortcuts and appearance

Use **Ctrl+Shift+Page Up / Page Down** to scroll the console one page up or down.
Use **Ctrl+Shift+Up / Down** to scroll one line up or down.
Use **Ctrl+Shift+Tab** to cycle forwards through tabs in the current pane.
Use **F6** to switch focus between split panes.
Use **F1** for About, **F2** for Settings, and **Ctrl+Shift+S** to save the current view as text.

In **Settings → Appearance**, choose Dark or Light and a separate Blue, Green, or
Red base color. Each theme remembers its own base color across restarts.
Plain Page Up / Page Down and arrow keys retain their normal terminal behavior.

Use **Ctrl+Shift+Space** to pin the console to the bottom. On connection tabs,
**Ctrl+Shift+Q / W / E** selects the **Log / Hex / ANSI** view.
Use **Ctrl+Shift+P** to toggle the plot on connection tabs.
Use **Ctrl+Shift+M** to open the transmit macros window.

## Installation and update behavior

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

## Building, demos, packaging, and CI

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

### Demo build

Start a ready-to-capture demo without serial hardware:

```sh
cargo run -p pigtail --release --features demo
```

This special build always opens a 1280 × 900 dark window with generic output
from “device 1” and “device 2”, colored prompts and sensor labels, and temperature
and humidity plots.
The sample output stays still while you arrange the window and take screenshots.
Log, ANSI, and hex views use the same sample bytes; switching tabs and exploring
the display controls works as usual. The connections are simulated and do not
respond to commands.
Applying port options cannot reconnect these simulated devices or open real ports.

Demo builds use temporary app directories, skip port discovery, disable
updates, and do not save settings or session captures. Your regular settings
and connections are preserved. Omit `--features demo` to run the normal app.

The Ubuntu build job also builds this demo after packaging the normal app,
captures its window under Xvfb with software rendering, and attaches
`pigtail-screenshot.png` to the release on `v*` tag builds.
To build any branch manually, open Actions → build → Run workflow and choose
the branch. All builds upload the packages and screenshot as
artifacts on that workflow run, with a separate artifact for each platform.
Only `v*` tag builds create a GitHub release, after both Linux and Windows
builds succeed. APT publishing runs after the release succeeds.
Pull requests also build when labeled `build` (both platforms), `build-linux`
(Linux packages and screenshot), or `build-windows` (Windows packages).
Adding a label starts a build; subsequent commits rebuild the selected platforms.
PR builds upload artifacts to their workflow run.
Rerunning a failed platform replaces its workflow artifacts while retaining
successful platforms' artifacts from earlier attempts of the same run.

To capture it locally on Linux, install `xvfb`, `xauth`, `xdotool`, `imagemagick`,
and Mesa's software rendering libraries, then run:

```sh
cargo build -p pigtail --release --features demo
bash scripts/capture-screenshot.sh target/release/pigtail /tmp/pigtail-screenshot.png
```

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

## APT distribution

Install through APT (Ubuntu 22.04 or newer, amd64)

Available after the shared [Rusty APT repository](https://github.com/rustypig91/rusty-apt)
is configured and the first package is published. Packages currently support
x86-64 (amd64) only:

```bash
sudo mkdir -p /etc/apt/keyrings
curl -fsSL https://rustypig91.github.io/rusty-apt/rusty.asc | sudo tee /etc/apt/keyrings/rusty.asc >/dev/null
sudo chmod 644 /etc/apt/keyrings/rusty.asc
echo 'deb [arch=amd64 signed-by=/etc/apt/keyrings/rusty.asc] https://rustypig91.github.io/rusty-apt stable main' | sudo tee /etc/apt/sources.list.d/rusty.list
sudo apt update
sudo apt install pigtail
```

Upgrade with `sudo apt update && sudo apt upgrade`; remove with
`sudo apt remove pigtail`. To remove the shared repository configuration:

```bash
sudo rm -f /etc/apt/sources.list.d/rusty.list /etc/apt/keyrings/rusty.asc
sudo apt update
```

Release maintainers: configure `APT_PUBLISH_TOKEN` with Actions write access
only to `rustypig91/rusty-apt`; see its README for signing and Pages setup.
