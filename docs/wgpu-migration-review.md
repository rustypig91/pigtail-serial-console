# wgpu migration review — 2026-10-11

Compared `switch-to-wgpu` with `main` (`180cf233`). The reviewed migration
starts at `0c2b2a67`; the review fixes are described below. Tests and timings
used Rust stable 1.98.0 on an Intel Core i7-1260P running Linux.

## Findings and fixes

- egui's default headless viewport is unbounded. An ANSI input test using
  `RawInput::default()` attempted to allocate an enormous VT grid and did not
  complete. The shared test harness now supplies a finite 1024 × 768 viewport
  when a test does not specify one. The existing ANSI input test now passes.
- Demo-feature tests did not compile: the dropped-file fixture still used
  egui's former struct API, a migrated closure retained an unused variable,
  and two older receive tests omitted the drain budget. All are corrected.
  CI now runs workspace tests with all features on both platform jobs.
- Clear-button pulses, font-size toasts, and selection edge scrolling requested
  immediate redraws repeatedly. Linux's non-vsync presentation no longer
  provides a display-rate limit for these requests. Their follow-up requests
  now wait 16 ms, compensating for egui's subtraction of the predicted frame
  time. A regression test checks both the delay and that input can still wake
  the UI immediately. Reader wakes and receive/macro scheduling are unchanged.

## CPU layout and tessellation comparison

The ignored `render_load_diagnostic` exercises real workspace rendering at
1280 × 900 in Log, Hex, ANSI/VT, and plot modes, plus a wrapped log with
100,000 retained lines. It warms up for 50 frames and measures another 1,000.
It includes layout and tessellation, but excludes GPU submission/presentation,
live serial ingestion, disk capture, and changing filters or searches.

The same probe was added to an isolated copy of main, adapting only the egui
frame API and the pre-existing demo test drain-budget compilation errors.
Three alternating comparisons were pinned to the same CPU. The table gives
the median of those runs' median and p95 values. The timed branch binary
contains the migration and demo/test-harness fixes; the subsequent animation
scheduling fix does not run in these input-free scenes.

| View | Main median / p95 (ms) | Branch median / p95 (ms) |
| --- | ---: | ---: |
| Log | 0.307 / 0.366 | 0.385 / 0.453 |
| Hex | 0.256 / 0.297 | 0.299 / 0.318 |
| ANSI/VT | 4.874 / 6.845 | 4.305 / 6.626 |
| Plot | 0.346 / 0.374 | 0.356 / 0.376 |
| Wrapped 100,000-line log | 0.228 / 0.249 | 0.211 / 0.224 |

This does **not** establish zero performance overhead: Log and Hex take
slightly more CPU time, while ANSI and the large log improve. All measured
p95 CPU times fit within a 60 Hz frame budget, leaving GPU time unmeasured.
These are static-scene measurements, not a sustained receive-throughput or
GPU frame-time benchmark. Short tests, CPU frequency changes, and host load
limit the precision of small differences.

The existing receive-load diagnostic also passed on both builds, preserving
all 52,416 ordinary-line updates and 2,097,152 newline updates from its
injected workloads. Both completed ordinary input in 14 drains. The longest
newline drain was 16.5 ms on main and 18.6 ms on the branch; this time budget is
cooperative and does not guarantee that every drain completes within 8 ms.
The probe excludes capture writes and GPU rendering, and a single run cannot
attribute that small worst-case difference to the renderer migration.

To reproduce on the branch:

```sh
cargo +stable test -p pigtail --release --features demo --locked \
    render_load_diagnostic -- --ignored --nocapture
```

## Native rendering and memory

Both optimized demos ran under an isolated Xvfb display. Main used software
OpenGL; the branch used Vulkan with Mesa's lavapipe driver. Keyboard-driven
Log/Hex/ANSI switching, Settings opening/closing, search, and Ctrl+wheel font
changes worked. Captured frames were inspected for console text, colours,
wrapping, header/footer placement, plots, and modal layout. Idle Log, Hex,
ANSI and Settings each consumed no measurable CPU over three-second samples
in either build. Search caret and font animations consume CPU in both builds;
these short software-renderer samples are too variable for a throughput claim.

The final branch's RSS was about 123 MiB in Log and 142 MiB after ANSI versus
116 MiB and 131 MiB on main. After opening Settings it was 144 MiB versus
131 MiB. Renderer/driver state and font caches therefore add some memory cost.

Both builds also passed a native Wayland smoke test on Intel Iris Xe with
Mesa 26.0.8. The branch logged selection of the hardware Vulkan adapter and
no surface errors during the test. After startup, both used no measurable CPU
over an eight-second idle sample. RSS was 110.6 MiB on the branch versus
90.1 MiB on main. These short runs do not verify long-term memory growth.

## Validation and remaining coverage

- `cargo +stable fmt --all -- --check`
- `cargo +stable test --workspace --locked`
- `cargo +stable test --workspace --all-features --locked` — 448 passed;
  three manual diagnostics ignored
- `cargo +stable clippy --workspace --all-targets --all-features --locked`
- `cargo +stable check -p pigtail --features demo --locked`
- Optimized demo builds, comparative CPU diagnostic, native software-renderer
  UI inspection, and hardware Vulkan/Wayland startup and idle checks

The tests cover raw preservation, receive overload, reconnect ordering,
VT history, wrapping, selection, filters/search, merged views, split-pane
layout and input routing, transmit/macros, transfers, and update verification.
Real serial devices, sustained GPU presentation under receive load, Windows
DX12/Vulkan runtime, Windows sleep/resume, monitor changes, and actual package
installation were not exercised in this review. Platform packaging runs are
requested through the PR's `build` label. A blanket guarantee of no performance
degradation or universal driver compatibility would exceed this evidence.
