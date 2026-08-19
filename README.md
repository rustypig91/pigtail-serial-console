# Pigtail

A desktop serial terminal for embedded firmware development, built in Rust with [egui](https://github.com/emilk/egui).

Most serial terminals just show you text. Pigtail is built around the parts of firmware debugging that actually cause pain:

- **Reconnects by device identity, not port path.** When a target resets and re-enumerates on a new port, Pigtail finds it again automatically.
- **Filtering reveals history, not just new output.** Type a filter and it applies retroactively to everything already captured, not only what arrives afterward.
- **Live plotting linked to the log.** Numeric values extracted from the stream (`temp:23.4, rpm:1200`, or a regex) are plotted live, and clicking a plot point jumps to the log line that produced it.
- **Nothing is lost.** Raw bytes are written to disk continuously as they arrive; the UI is just a view over that capture.

## Features

- Auto-reconnect by USB VID/PID/serial number, with a visible marker showing exactly where a gap occurred
- Regex or plain-text filtering and search over full scrollback
- Highlight rules (color/bold by pattern)
- Multiple ports as tabs, plus a merged view interleaving all ports by timestamp
- Hex view alongside the text view
- Transmit with configurable line endings, send history, and hex input
- DTR/RTS toggles and break signal
- Connection profiles with auto-connect on plug-in
- Export the current (filtered) view to `.txt` or `.csv`
- Startup notice when a newer release is published, with "skip this version"; the
  only network request pigtail makes, and switchable off in Settings
<<<<<<< HEAD

## Installing

Every release publishes installers alongside the plain binaries on the
[releases page](https://github.com/rustypig91/pigtail-serial-console/releases):

| Platform | Asset | Notes |
| --- | --- | --- |
| Windows | `pigtail-v<version>-x86_64-pc-windows-msvc.msi` | Install wizard; adds a Start Menu entry and an Add/Remove Programs entry. Installing a newer version replaces the old one in place. |
| Debian/Ubuntu | `pigtail_<version>-1_amd64.deb` | `sudo apt install ./pigtail_<version>-1_amd64.deb` — pulls in its own dependencies and registers a desktop entry. |
| Any Linux | `pigtail-v<version>-x86_64.AppImage` | `chmod +x` and run; no installation, bundles its libraries. Needs the host's GPU drivers for OpenGL. |
| Portable | `.zip` / `.tar.gz` | Just the binary, no installation. |

The MSI is not code-signed, so Windows SmartScreen shows an "unrecognized app"
warning on first run; choose "More info" → "Run anyway".
=======
>>>>>>> add-update-check

## Building

Requires stable Rust.

```sh
cargo build --release
```

On Linux, install `libudev-dev` (serial port access) and `libgtk-3-dev` (file dialogs) first.

Run in development with `cargo run -p pigtail`.

### Building the packages

CI does this on every tag, but each one can be built by hand:

```sh
cargo wix -p pigtail                                    # Windows .msi (needs cargo-wix + WiX v3)
cargo deb -p pigtail                                    # .deb (needs cargo-deb)
crates/pigtail/packaging/linux/build-appimage.sh \
    target/release/pigtail 0.2.0 .                      # .AppImage
```

## Workspace layout

- `crates/serialcore` — UI-agnostic engine: port enumeration, framing, storage, filtering, extraction. No GUI dependency.
- `crates/pigtail` — the egui application.
- `crates/pigtail/wix` — WiX source for the Windows installer.
- `crates/pigtail/packaging` — icons, desktop entry, and the AppImage build script.

## License

MIT, see [LICENSE](LICENSE).
