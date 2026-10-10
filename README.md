# Rusty's Pigtail - Serial Terminal

A desktop serial terminal built in Rust with [egui](https://github.com/emilk/egui).

Rusty's Pigtail reconnects by USB device identity when a target resets, applies
filters to the full captured history, and plots numeric values linked to their
original log lines. Raw bytes are continuously saved to disk.

## Features

- Automatic reconnect by USB VID/PID/serial number, with connection-gap markers
- Plain-text and regex search, filtering, and highlight rules
- Persistent connection tabs, timestamped merged views, and two resizable panes
- Log, Hex, and ANSI/VT views with configurable terminal scrollback
- Live plots from numeric fields or regex extraction, including earlier output
- Configurable line endings, hex input, send history, and file transfers
- Transmit macros with delays, regex waits, loops, and keyboard shortcuts
- Byte pacing, DTR/RTS controls, and break signals
- Export filtered output to text or CSV
- Dark/light themes and release notices with supported in-app updates

## Installing

Download an installer or portable build from the
[releases page](https://github.com/rustypig91/pigtail-serial-console/releases).

| Platform | Package |
| --- | --- |
| Windows | `pigtail-v<version>-x86_64-setup.exe` for interactive installation, or `.msi` for managed deployment |
| Debian/Ubuntu | `pigtail_<version>-1_amd64.deb`; install with `sudo apt install ./pigtail_<version>-1_amd64.deb` |
| Linux | `pigtail-v<version>-x86_64.AppImage`; make executable with `chmod +x` and run |
| Portable | Extract the `.zip` or `.tar.gz` and run the binary |

Windows installers are unsigned, so SmartScreen may show an “unrecognized app”
warning. Choose **More info → Run anyway**. Use either the setup installer or
MSI; Windows treats them as separate products. Setup offers installation for
just your user or for all users.

Ubuntu 22.04 or newer (amd64) can also install through the shared
[Rusty APT repository](https://github.com/rustypig91/rusty-apt) once configured:

```sh
sudo apt update
sudo apt install pigtail
```

Use **Update** in the release notice for supported in-app updates. Update MSI
and Debian installations through their installer or package manager. Startup
update checks can be disabled in Settings.

## Basic usage

Open a serial connection from the **+** menu. Use **New merged view** to combine
connections, or right-click a tab and choose **Split right** or **Split below**
to open a second pane. Each connection remembers its selected display mode.

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+Q / W / E` | Select Log / Hex / ANSI/VT |
| `Ctrl+Shift+F` | Search ANSI/VT screen and retained scrollback |
| `Ctrl+Shift+Space` | Pin console to live output |
| `Ctrl+Shift+Page Up / Page Down` | Scroll a page |
| `Ctrl+Shift+Up / Down` | Scroll a line |
| `Ctrl+Shift+Tab` | Cycle forwards through tabs in the current pane |
| `F6` | Focus the other pane |
| `Ctrl+Shift+P` | Toggle plot |
| `Ctrl+Shift+M` | Open transmit macros |
| `Ctrl+Shift+S` | Save the current view as text |
| `F1 / F2` | About / Settings |

## Building from source

Install stable Rust meeting the `rust-version` in [Cargo.toml](Cargo.toml).
On Debian/Ubuntu, install development prerequisites:

```sh
sudo apt install libudev-dev libgtk-3-dev
cargo build --release
```

Run locally with `cargo run -p pigtail`. To explore the UI without hardware:

```sh
cargo run -p pigtail --release --features demo
```

The demo uses simulated output and temporary directories, preserving your
regular settings and captures.

See [AGENTS.md](AGENTS.md) for the code map, detailed behavior, development
checks, screenshot capture, packaging, and release workflow.

## License

MIT, see [LICENSE](LICENSE).
