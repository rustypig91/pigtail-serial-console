# Agent guide

This guide applies to the whole repository. Keep [README.md](README.md) short:
a concise explanation of the product, installation, basic usage, and a quick
source build. Do not add small feature details, menu-item instructions,
troubleshooting paths, editor-specific commands, or other development details
to the README. Keep detailed implementation, behavior, validation, packaging,
and release information here. Update the README only when its concise overview
or essential installation and usage guidance needs to change; document smaller
changes here instead.

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
cargo test --workspace --all-features --locked
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
- Regex or plain-text search over full scrollback. Log and merged-view search
  has a compact search display dropdown: **Highlight**, **Only matches**, and
  **Hide matches**. The button shows the selected mode's icon (highlighter,
  funnel, or crossed-out eye), and the dropdown repeats each icon beside its
  label. It sits immediately to the right of the case-sensitivity control.
  Hover shows the short “Search display mode” tooltip and current mode.
  The query, regex and case controls determine which lines match. Only
  **Highlight** colors all search matches and shows scrollbar match markers;
  **Only matches** colors just the current match, preserving a visible navigation
  target while leaving other matching lines unhighlighted.
  Changes apply to retained history and incoming output; empty queries and
  invalid regex show all lines. **Hide matches** disables Next/Previous because
  their target lines are hidden; it keeps the same current/total match indicator
  and remembers the selected match. Returning to Highlight or Only matches
  reveals that match and resumes navigation from it. Match counts are shown only in the search
  header, not the footer. The footer has one “x lines” count for the retained
  lines displayed in Log or the merged view, including active search filtering;
  it has no separate “shown” count. Hex and ANSI/VT report the retained Log line
  count. Closing search by any path (×, Escape, the search
  toolbar button, or its context-menu toggle) restores all lines and removes
  search highlighting while retaining the query, current match and selected
  mode. Reopening search reuses that text and resumes the selected mode. Each connection and
  merged view remembers its mode for the current run; startup defaults to
  **Highlight**. Nothing is saved or migrated. Merged-view scrollbar match
  markers use merged sequence keys and wrapped row positions, so matches from
  different connections stay correctly placed. Hex and ANSI/VT display
  remain independent of line filtering. Text/CSV exports use the displayed Log
  subset; raw capture remains complete. There is no separate Filters window or
  Add as filter action; **Search selection** stays in the selection menu.
- Search history is shared across connection tabs, merged views, and split panes
  for the current run only, with at most 50 unique text/regex/case combinations.
  The small chevron inside the left side of the search field opens a scrollable
  **Recent searches** menu, newest first. Selecting an entry restores its query,
  regex and case settings, updates results, and returns focus to the search field;
  the current search display mode stays unchanged. Plain **Up/Down** in the focused
  search field recalls older/newer entries without wrapping. Down past the newest
  entry restores the text and settings present before browsing. Editing a recalled
  entry starts a new draft. Modified arrows (including Shift/Alt+Up/Down) remain
  available for text editing. Completed searches are recorded on Enter/Next/Previous,
  leaving or closing the field, switching its tab, or starting history browsing.
  Opening the history menu also records the current query. Empty queries and invalid
  regex are skipped; repeating a query with the same settings moves it to the top.
  Recall keys are consumed before text editing and never sent to the device.
  The search field expands into available toolbar space, with room reserved
  for its controls; long queries remain clipped without moving those controls.
  The textbox fills its allocated field width, leaving only the normal widget
  spacing before the regex control.
- Highlight rules (color/bold by pattern)
- Multiple ports as persistent, user-named tabs, plus optional merged views that label
  and interleave selected connections by timestamp.
  Choose **New merged view** from the **+** menu and select the
  connections to include. The merged view footer has a left-aligned **Send to**
  selector after its status text for choosing which included connection receives
  typing, pasted text, and macros.
  You can open several merged views and close each
  with a middle-click or its tab's right-click menu. Merged views can start
  empty; right-click their tab and choose **Options** to change
  its name and included connections at any time. Drag merged tabs alongside
  connection tabs to reorder them. Closing either kind of tab asks for confirmation
  unless you have disabled that preference.
- Hex view alongside the text view. Search in Hex is always Highlight: the
  search display-mode button is hidden, and Log's remembered filter mode does
  not affect it. Plain-text and regex queries search the displayed hexadecimal
  byte values and ASCII column within each 16-byte row, excluding offsets and
  session markers, including when long-running captures have offsets wider than
  eight hexadecimal digits. All matches are highlighted, with a brighter current row
  and scrollbar match markers. Next/Previous and Enter/Shift+Enter navigate
  matching rows and scroll them into view, using Hex's own count and cursor.
  Partial rows are rechecked as bytes arrive or are evicted; session boundaries
  prevent searches joining independent captures. Hex search navigation clamps
  its scroll offset before painting, keeping matches near the end from briefly
  leaving blank space below the last row. Closing search removes Hex
  highlighting. Raw data remains complete and all retained byte rows stay visible.
- Custom window header with draggable device tabs, status dots, close buttons,
  and minimize/maximize/close controls. Drag empty header space to move the window,
  double-click it to maximize or restore, and drag an edge or corner to resize.
  View controls, search, clear, export, and the app menu sit above the console;
  ANSI/VT sits beside Hex; highlights are in the overflow menu. A thin footer shows
  one combined connection/error status at the bottom left, line counts and port
  settings, with a clickable pin icon for autoscroll. History eviction does not add a
  “History limited” indicator to the footer. The header tint blends subtly
  with scrolling terminal history and its timestamps underneath it.
  The combined status shows green **Connected**, yellow **Device missing** when
  no device is available, and yellow **Reconnecting** after discovery while
  opening the port. Other waiting/closed states are yellow. Failures replace
  the connection label with a short red description (permission denied, port
  busy, capture/send/control failure); dropped commands and an already active
  transfer are yellow warnings. Hover shows full details immediately, including
  after clicking the status; it does not use the usual tooltip delay or click
  suppression. There is no separate Error tag. Device absence clears obsolete
  link errors without clearing
  unrelated session failures, which remain visible until dismissed. Successful
  reconnection clears link errors. Error details remain available in the
  overflow menu.
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
- Per-connection **Send delay** in Port options (milliseconds between outgoing bytes,
  default 0), applied to typing, paste, macros, and file transfers. The delay is saved
  with each connection and port preset. The obsolete global delay setting is ignored
  on load and removed on the next save; it is not migrated. Malformed global
  settings and port-option values fall back individually to defaults, preserving
  valid neighboring options. Unknown keys are omitted on save. Invalid TOML syntax
  still fails configuration loading. Discarding a malformed option or settings
  section, or the obsolete global send delay, emits a warning in `app.log` identifying
  the section and option where applicable. Port-option labels and controls have
  concise tooltips explaining serial parameters, receive framing, transmit pacing
  and endings, modem signals, local echo/history, and presets. Port options and
  New connection scroll within small windows while their Apply/Connect and Cancel
  buttons remain outside the scrolling area.
- DTR/RTS toggles and break signal
- Export the current (filtered) view to `.txt` or `.csv`
- Startup notice when a newer release is published, with "skip this version" and
  one-click download and update. Startup checks can be switched off in Settings.

### Keyboard shortcuts and appearance

In **Settings → Folders**, click the configuration directory path to open the
parent directory of the active `pigtail.toml` in the system file manager.
Click the **Logs and session captures** directory path to open the parent of
`crash.log`, also containing `app.log`, `app.previous.log`, and the sessions
subdirectory. Missing directories are created before opening.
Folder launchers run on background workers; their exit failures wake the UI,
are logged, and appear in the existing error dialog without blocking console input.
Demo builds open their isolated temporary directories; ordinary demo diagnostics
are still written to stderr.

Use **Ctrl+Shift+Page Up / Page Down** to scroll the console one page up or down.
Use **Ctrl+Shift+Up / Down** to scroll one line up or down.
Use **Ctrl+Shift+Tab** to cycle forwards through tabs in the current pane.
Use **F6** to switch focus between split panes.
Use **F1** for About, **F2** for Settings, and **Ctrl+Shift+S** to save the current view as text.

In **Settings → Appearance**, choose Dark or Light and a separate Blue, Green, or
Red base color. Each theme remembers its own base color across restarts.
Plain Page Up / Page Down and arrow keys retain their normal terminal behavior.
Ctrl+wheel over the console changes the font size by one point per frame.
Nonzero wheel deltas apply at gesture start, move, and end; zero-delta
gesture boundaries do not change the size.

Use **Ctrl+Shift+Space** to pin the console to the bottom. On connection tabs,
**Ctrl+Shift+Q / W / E** selects the **Log / Hex / ANSI** view.
Use **Ctrl+Shift+P** to toggle the plot on connection tabs.
Use **Ctrl+Shift+M** to open the transmit macros window.
The console right-click menu has no **Toggle plot** or **Toggle hex view** items;
use the toolbar controls or their keyboard shortcuts.

## Installation and update behavior

Every release publishes installers alongside the plain binaries on the
[releases page](https://github.com/rustypig91/pigtail-serial-console/releases):

| Platform | Asset | Notes |
| --- | --- | --- |
| Windows | `pigtail-v<version>-x86_64-setup.exe` | The one most people want. Install wizard with an optional desktop shortcut; recommends installing for the current user in AppData, with an all-users option. |
| Windows | `pigtail-v<version>-x86_64-pc-windows-msvc.msi` | Same application, for scripted or managed deployment (`msiexec /i ... /qn`, Group Policy). Adds a Start Menu entry and an Add/Remove Programs entry. |
| Debian/Ubuntu | `pigtail_<version>-1_amd64.deb` | `sudo apt install ./pigtail_<version>-1_amd64.deb` — pulls in its own dependencies and registers a desktop entry. |
| Any Linux | `pigtail-v<version>-x86_64.AppImage` | `chmod +x` and run; no installation, bundles its libraries. Needs the host's graphics loader and GPU driver (Vulkan recommended). |
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

### Rendering and diagnostics

The [wgpu migration review](docs/wgpu-migration-review.md) records comparative
CPU timings, native software and Intel Vulkan checks, memory overhead, and
remaining hardware validation. CI runs workspace tests with all features so
demo-only API migrations are checked too. Headless test frames supply finite
window bounds. Repeated application animation/selection-scroll redraw requests
are paced independently of vsync, while reader wakes remain immediate.
Receive-backlog frames use the same pacing. Timer requests compensate for egui's
predicted frame time so configuration writes and macro deadlines do not busy-loop
just before they are due.

Dependencies were checked against the registry and updated to current stable versions
on 2026-10-11. The GUI uses eframe/egui 0.36.2, egui_plot 0.37.0 and wgpu 30.0.1.
This includes the upstream fix for DX12 diagnostic hooks being initialized when
DX12 is disabled, which caused AMD Vulkan presentation to flood Windows logs.
Automatic backend selection and environment overrides remain available.
The migration uses parent-UI panels, output hooks for clipboard selection capture,
and fractional font advances for console wrapping. Headless UI tests explicitly
discard texture uploads because they have no GPU consumer.
The Windows migration passed workspace tests, Clippy, formatting and the demo
feature check. A captured demo frame confirmed the saved dark theme, console and
plot layout; an automatic-backend run no longer emitted the repeated AMD DXGI
resource-state diagnostics. This machine still logs a missing Rockstar Social
Club Vulkan layer manifest registered by another application. Linux runtime and
Windows sleep/resume validation remain necessary on the affected hardware.

The application uses eframe's wgpu renderer with standard wgpu backend and adapter
selection, including its `WGPU_BACKEND` and `WGPU_POWER_PREF` environment overrides.
The direct wgpu dependency enables its standard backend features (eframe disables
wgpu's defaults). No platform-specific backend is forced. This replaces the
glow/glutin renderer involved in the AMD `atio6axx.dll` crash on Windows resume.
Linux uses `AutoNoVsync` presentation: wgpu chooses Immediate, then Mailbox,
then FIFO according to surface support. This avoids the repeated frame-acquisition
timeouts observed with default FIFO presentation on Intel Iris Xe/Mesa 26.0.8
under Wayland. Mailbox was confirmed to restore both mouse and keyboard input.
Immediate presentation can tear on displays that support it. Other platforms
retain the default presentation mode. This does not change backend selection.
The affected machine must still validate the fix with repeated sleep/wake cycles,
including idle and active serial connections, minimized windows, and monitor changes.

Lost rendering surfaces are recreated; outdated/suboptimal surfaces are reconfigured.
eframe requests the recovery frame for lost/outdated surfaces and renders suboptimal
frames before reconfiguring. The status callback also requests a delayed repaint;
in particular, timeouts retry after at most 100 ms even when the terminal is idle.
This handles recoverable presentation errors, not full GPU device loss or native
driver crashes. Occluded windows skip frames quietly. Surface validation failures are logged without scheduling retries.

Normal builds write tracing diagnostics (including returned eframe errors) to
`app.log` alongside `crash.log`. The latest launch replaces `app.log`, preserving
the previous launch as `app.previous.log`. On Windows these files are under
`%APPDATA%\pigtail\pigtail\data`. On Linux they are under
`${XDG_DATA_HOME:-~/.local/share}/pigtail`. The panic log continues to append panic
backtraces; native driver crashes can bypass it. Demo builds log to stderr and
retain their isolated temporary directories.

Linux needs a working graphics driver; Vulkan is recommended, with its loader
(`libvulkan1` on Debian/Ubuntu). Mesa's `mesa-vulkan-drivers` supports Intel/AMD GPUs
and lavapipe software rendering; NVIDIA uses its vendor driver. DEB packages depend on `libvulkan1`;
AppImages use the host's loader, driver and ICD files. Screenshot capture forces
lavapipe under Xvfb so it works without a physical GPU.

### Source build

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

VS Code tasks are in `.vscode/tasks.json`. **Tasks: Run Task → Run Pigtail
(Release)** executes `cargo run --release -p pigtail --locked` from the workspace
root, using the normal configuration and sessions. The default build task
remains **Run Pigtail** (debug).

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
Every push to `main` and every pull request builds both platforms, runs workspace tests
with all features, and uploads packages and the Linux screenshot to the workflow
run. Pull requests do not require build labels. Feature-branch pushes build through
the pull-request event, avoiding duplicate push and pull-request builds.
New PR commits cancel older runs for that PR. Each `main` push, version-tag build,
and manual run has its own concurrency group and is not cancelled by newer runs.
Rerunning a failed platform replaces its workflow artifacts while retaining
successful platforms' artifacts from earlier attempts of the same run.

To capture it locally on Linux, install `xvfb`, `xauth`, `xdotool`, `imagemagick`,
`libvulkan1`, and `mesa-vulkan-drivers`, then run:

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
    libx11-6 libxcursor1 libxi6 libxrandr2 libxkbcommon0 libxkbcommon-x11-0 \
    libvulkan1 mesa-vulkan-drivers
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
