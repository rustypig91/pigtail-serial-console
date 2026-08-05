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

## Building

Requires stable Rust.

```sh
cargo build --release
```

On Linux, install `libudev-dev` (serial port access) and `libgtk-3-dev` (file dialogs) first.

Run in development with `cargo run -p pigtail`.

## Workspace layout

- `crates/serialcore` — UI-agnostic engine: port enumeration, framing, storage, filtering, extraction. No GUI dependency.
- `crates/pigtail` — the egui application.

## License

MIT, see [LICENSE](LICENSE).
