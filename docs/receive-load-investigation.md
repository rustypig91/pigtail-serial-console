# Receive load investigation

Tested on 2026-10-10, using synthetic input without serial hardware. The baseline
below records the original behavior. Receive-load optimizations have now been
implemented; the diagnostic tests remain available for comparison.

## Implemented bounds

- Reader channels have 16 slots and admit batches of at most 512 KiB allocated
  heap each: at most 8 MiB of queued batch allocations per port. The separate
  backlog retains its 8 MiB limit and ordered dropped-output notices. Channel
  slots, allocator overhead and the UI's current batch are additional.
- After disconnect or EOF, retained batches finish before the stream boundary.
  This final drain still accepts clear and shutdown commands while waiting for
  channel space. Clearing then drops the backlog and truncates the capture,
  instead of replaying cleared output or leaving its on-disk bytes intact.
- Reads are framed in 1 KiB pieces. Batches flush at 256 KiB allocated heap,
  at 64 KiB raw input, or before exceeding 4,000 line updates. A defensive
  admission check drops oversized batches with a gap notice. Capture writes
  still occur before parsing and live-view dropping.
- Each update shares an 8 ms ingest budget equally across ports. It yields
  between lines and 4 KiB raw chunks, with additional per-port work ceilings
  of 4,000 lines and 64 events. Pending data resumes in order on subsequent
  updates. Clearing the console also discards its partially consumed batch.
  The budget is cooperative: one line operation, history compaction, derived
  indices or rendering can still make an update exceed it.
- Incomplete framer controls stop retaining bytes at 4 KiB. Oversized strings
  are ignored through their terminator, after which normal parsing resumes.
  The framer diagnostic retains 4,096 bytes for OSC and 8,192 bytes for CSI
  after both 1 MiB and 16 MiB of malformed input, instead of 32–48 MiB.
- A wrapper also bounds OSC strings entering the VT screen parser. Its `vte`
  dependency uses an unbounded OSC vector with the default `std` feature.
  This second path would otherwise retain malformed input even after fixing
  the framer. Ordinary terminal controls are compared against the original
  parser at every split boundary in a regression test.
- Line history shares a 128 MiB retention budget across text, line metadata
  and heap-backed colour spans, in addition to the configured line cap.
  Vector spare capacity is additional. Old lines are evicted in bulk to half
  the byte budget. Extraction retains at most 64 distinct series and shares
  one million retained points across them; established keys continue updating
  after the distinct-key ceiling is reached. The existing history-limit
  indicator also covers series limiting.

## Reproduce

```sh
cargo test -p pigtail --release --offline receive_load_diagnostic -- --ignored --nocapture
cargo test -p serialcore --offline unterminated_escape_allocation_diagnostic -- --ignored --nocapture
```

The UI probe queues 32 synthetic reads of up to 64 KiB, then calls the actual
`Connection::drain_events`. It includes VT screen processing, raw-ring storage,
ANSI parsing and line storage. It excludes reader-thread capture writes,
rendering, filtering, search and plot extraction. The injected channel is
unbounded for test convenience; injection deliberately bypasses the new reader
limits to retain the original workload. Queue allocation counts raw bytes, line-vector capacity and
text-string capacity; allocator overhead and channel slots are excluded.

Baseline results from an optimized build on this machine:

| Input (~2 MiB raw) | Line updates | Queued allocation | Single UI drain | Line-store allocation after drain |
| --- | ---: | ---: | ---: | ---: |
| 40-byte CRLF lines | 52,416 | 8.4 MiB | 39 ms | 8.4 MiB |
| Newlines only | 2,097,152 | 146 MiB | 541 ms | 96 MiB |

With the optimizations, the same injected workloads took 8.1 ms and 8.8 ms
respectively for the first drain. All input then completed over 14 and 549
drains, with maximum observed drain times of 8.1 ms and 17.0 ms. This measures
responsiveness, not higher total throughput: overload is spread across updates,
and the real reader sheds stale live-view data once its bounded queues fill.
The unchanged 146 MiB injected allocation is intentionally outside the reader
so the original UI workload remains comparable; the stalled-reader regression
separately verifies the new queue bounds.

Timings are host-specific, but the amplification and absence of a drain budget
are structural. A UI update can consume the entire queue and continue consuming
events arriving during the drain, delaying rendering and input handling.

## Original findings

1. **The reader channel has a batch-count limit, not a byte limit.**
   `reader.rs` permits 1,024 queued events. Its separate 8 MiB backlog limit
   applies only to batches that have not entered that channel. Scaling the
   measured newline batches to 1,024 yields about **4,672 MiB of queued heap**
   per port, before UI history and other allocations. This is an estimate of a
   filled channel, not an observed multi-gigabyte allocation. The diagnostic
   deliberately uses a much smaller load.

2. **The 4,000-line batching threshold can be overshot substantially.**
   It is checked after framing a full read. One 64 KiB read consisting of
   newlines produces 65,536 lines in one batch. Many short lines therefore
   cost much more memory and CPU than the same raw-byte volume in long lines.

3. **UI ingest has no time or work budget.**
   `Connection::drain_events` loops until `try_recv` finds the channel empty.
   Every byte is processed by VT emulation even in log/hex view, and each line
   receives ANSI parsing and storage. This directly reproduces long UI stalls.

4. **Unfinished VT escape sequences have unbounded storage.**
   In the default VT100 framing mode, `consume_escape` appends to `esc_raw`
   until a sequence terminates. CSI also retains parameter bytes separately.
   These buffers bypass the 64 KiB line cap. Feeding `ESC ]` followed by
   16 MiB of `1` characters allocated **32 MiB**; `ESC [` followed by the same
   input allocated **48 MiB**. Both emitted zero lines and kept an empty normal
   line tail. These are vector-capacity measurements, not process RSS; growth
   continues with the input rather than being bounded by history settings.

5. **History can compound memory pressure.**
   The default retains one million lines, a 64 MiB raw ring, and VT scrollback
   per port. Text-arena eviction has a 2 GiB logical-byte threshold; vector
   capacity and line metadata add further memory. Numeric extraction bounds
   points per series but does not bound the number of distinct series names.
   Those additional paths were inspected, not load-tested here.

The existing `serialcore/tests/throughput.rs` passes, but covers only
framing/store/filter for 75,000 ordinary lines under a 200,000-line cap. It
does not exceed that cap, queue reader events, run VT screen emulation, or
exercise unfinished escapes, so it does not detect these cases.

## Validation

### PR #131 byte-preservation review

The reader now completes its retained backlog before reporting disconnect,
reconnect or closure. Previously a nonblocking final drain could silently leave
batches behind at EOF or deliver them after a reconnect boundary; the smaller
channel made this reachable within a single newline-heavy read. Final delivery
may block after reading stops; the existing shutdown path drains while joining.
The screen OSC guard also stays in Escape for the C0, DEL and high bytes VTE
ignores there, preventing those bytes from bypassing its allocation ceiling.

Regression tests compare exact raw bytes, framed text, timestamps and flags
across 1 KiB split boundaries for Classic, LF-only and VT100, including CR/LF,
backspace, valid and invalid UTF-8, colours, cursor edits and overlength lines.
UI tests compare styled history and the original VT screen while yielding
between raw chunks and lines. EOF tests fill the channel before consumption.
An overload test verifies that delivered bytes plus reported gap bytes account
for every input byte, and that the raw capture contains the entire input.

This is not a lossless live-view guarantee. The 8 MiB backlog still sheds output
under sustained overload, and the smaller channel reaches that policy sooner.
The new history budget can evict lines sooner, and controls exceeding the new
parser ceilings are discarded from interpreted output. Raw capture precedes
these limits and is byte-preserving when logging succeeds.

`cargo test --workspace --offline` and
`cargo clippy --workspace --all-targets --offline` pass. New regression tests
cover stalled-reader heap budgets, batch splitting with exact raw-byte/line
preservation, oversized-batch gap notices, escape-buffer recovery, partial
batch ordering and clearing, progress with tiny per-port budgets, metadata
retention and bounded extraction. The ignored UI diagnostic now finishes all
pending data across repeated drains and verifies the total line count.

These tests establish application-side stalls and memory amplification. They
do not establish whether the reported whole-machine slowdown comes from swap,
CPU saturation, GPU rendering or capture-disk contention. Replaying the user's
actual input while sampling process CPU/RSS, swap and disk activity would
distinguish those causes. Raw logging is buffered and avoids per-record fsync;
it has not been implicated by these tests.

### Live Log investigation, 2026-10-13

Investigated the configured `/dev/ttyACM0` connection at 115,200 baud, in Log
view with a 10,000-line cap, disabled highlight rules, no active search, and no
plot. The reported sluggishness persists in release builds and with Pin off,
but disappears when selecting a quiet connection tab.

The ordinary release binary consumed approximately 100–120% process CPU during
a short baseline, with RSS around 115–125 MiB. A captured interval contained
6,055 payload bytes and 1,211 newline characters over 6.12 seconds: roughly
1 KB/s and 200 newlines/s. This is received volume, not the configured baud
rate or a guarantee of identical traffic throughout every subsequent run.

A temporary source copy added timings around preparation, receive processing,
derived-index maintenance, workspace drawing, and remaining application work.
It selected the serial and quiet tabs alternately for four 12-second phases.
Both connections remained open throughout. Settings and captures were isolated
under temporary XDG directories using a copy of the user's configuration.
No transmit commands were issued, and the original release executable was
restored after building the probe. The final probe used release optimization
with application link-time optimization disabled; these figures are diagnostic
comparisons rather than exact timings of the shipped binary.

Two-second reporting windows spanning startup or a tab switch were excluded
from the following averages. Columns show milliseconds per UI frame; complete
frame time comes from eframe's previous-frame integration timer, which includes
rendering and excludes its measured presentation waits. It is an elapsed-time
measurement, not a separate CPU-counter measurement.

| Backend / selected tab | Frames/s | Receive | Derived indices | Workspace drawing | App UI total | Complete frame |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Vulkan / serial Log | 58.1 | 0.057 | 0.009 | 3.906 | 4.197 | 6.676 |
| Vulkan / quiet Log | 59.8 | 0.058 | 0.009 | 1.110 | 1.399 | 3.026 |
| OpenGL / serial Log | 58.3 | 0.054 | 0.015 | 3.741 | 4.024 | 6.033 |
| OpenGL / quiet Log | 58.9 | 0.057 | 0.010 | 1.140 | 1.434 | 3.063 |

The normal backend selected Intel Iris Xe Vulkan on Mesa 26.0.8, not software
rendering. The OpenGL override selected the same Intel GPU. Changing backends
did not substantially change workspace drawing cost. The line store reached
its cap and evicted old history correctly; receive processing remained small
across that boundary. RSS stayed approximately 121–127 MiB during the probe
runs. There were no reported surface errors in those runs.

Short GDB stack samples found visible Log work in `render_row`, `wrapped_text`,
font shaping/layout, widget creation, and text-accessibility processing, as well
as normal renderer and event-loop work. These intrusive samples locate work;
they do not quantify the percentage attributable to each function. Kernel
performance counters were unavailable under the host's perf policy, which
was left unchanged.

The measurements identify visible Log drawing as the main additional UI cost
for this input. Ordinary drawing already virtualizes rows, and the wrap index
updates incrementally, so replacing a presumed full-history drawing scan is
not supported by this investigation. Follow-up work should measure and reduce
repeated per-row layout/widget work while preserving selection, context menus,
and accessibility, then compare the same live workload again. Disk-retention
changes should be evaluated independently.

This does not fully explain the reported subjective lag: the warmed comparison
recorded occasional complete-frame spikes up to about 23 ms but no prolonged
freeze, and did not measure input-to-paint latency while opening menus or
dragging the window. Startup had larger transients. A follow-up should include
those interactions and tail-latency measurements rather than treating average
frame time as a complete responsiveness result. No performance fix or capture
retention change was implemented in this investigation.

### Log drawing optimization follow-up

The first optimization pass shared the console style across rows, removed the
extra text-column UI container from ordinary Log/merged rows, and added a
single-run path for text without ANSI spans, highlight rules or search matches.
This produced only a small improvement in the live comparison. Additional
per-row timings identified timestamp/gutter work as the largest remaining
component (approximately 23–27 microseconds per row in that probe).

The subsequent pass caches formatted timestamps using egui's frame cache, which
evicts entries no longer visited. The key includes wall and monotonic time,
display format, previous-line time and mark. Wall-clock formats are refreshed
once a second to reflect system timezone changes. Timestamp painting uses a
single-font galley when it fits, retaining egui's normal label response and
accessibility; narrow rows retain the previous wrapping path. Text selection,
its stable row identities, recoloring, clipboard capture and context-menu
behavior continue through the existing selection painter.

Validation includes the normal application suite, the all-features application
suite, workspace Clippy and formatting. Existing tests cover offscreen
selection/copy, wrapped text, caret placement, merged panes, header backdrops,
search styling and menus during incoming output. A new cache test checks format,
mark, preceding-line and timestamp changes. During workspace validation, a
receive-budget test failed under concurrent build load and passed on rerun;
an updater test failed because `/tmp` had reached its per-user quota and passed
with `TMPDIR` pointing to the build directory. The remaining core tests passed.
No capture-retention policy or receive scheduling was changed.

The final temporary probe repeated the live Vulkan comparison with the same
200-newline/s input and 10,000-line cap. It also included per-row timing calls,
so the following comparison is conservative but remains host- and run-specific.
Startup and tab-switch reporting windows were excluded as before.

| Selected tab | Baseline workspace drawing | Optimized workspace drawing | Baseline complete frame | Optimized complete frame |
| --- | ---: | ---: | ---: | ---: |
| Serial Log | 3.906 ms | 3.243 ms | 6.676 ms | 6.253 ms |
| Quiet Log | 1.110 ms | 1.046 ms | 3.026 ms | 2.878 ms |

Serial Log drawing fell approximately 17%; complete-frame time fell about 6%.
Receive processing remained 0.057 ms and derived indices 0.009 ms per frame.
The optimized serial view averaged 57.5 frames/s versus 58.1 before, within
the variation seen between these runs; this is not evidence of higher frame
rate. Timestamp/gutter work fell to roughly 14–15 microseconds per row. The
optimized probe still used about one core of process CPU over its full run,
including startup and both tab selections. The largest sampled warmed serial
frame was 18.5 ms versus 22.7 ms before; maxima are not comparable latency
percentiles and do not establish the absence of further stalls.

A native X11 capture of the updated view confirmed the timestamp/text columns,
header backdrop, footer and retained-line count. The behavior suites continue
to exercise selection and context menus. These are measured drawing
optimizations, not a verified resolution of every reported responsiveness
problem. Interactive input-to-paint latency and remaining rendering/platform
costs remain worthwhile follow-up measurements if the lag persists.

### Additional PR review fixes

Clear now discards both the UI's partially consumed batch and output already
queued in the reader channel, while preserving queued connection, error and
transfer events. Applying new port options also discards the old reader's
partial batch and resets the raw parser boundary before replacing the reader;
old bytes and line updates cannot appear after the settings-change marker.
Plot-history growth/backfill keeps the shared one-million-point limit instead
of temporarily restoring the uncapped per-series setting.

Three regression tests reproduced these failures before the fixes and pass
with them. The full workspace suite passes with 422 tests and two ignored
manual diagnostics; workspace Clippy and changed-file formatting also pass.

## Document selection and scrolling, 2026-10-14

The retained-row selection workaround defeated virtualization after interacting
with the log. egui requires both label-selection endpoints to be visited each
frame, so `selection_entries` kept the union of previously visited rows and the
current viewport. Scrolling or following incoming output could therefore grow
the work toward the entire retained history. This also applied after a **single
click**: egui reports a zero-width selection as active, although no selected text
is visible. It explains a way for modest-rate output to become expensive as
history accumulates, without an apparent selection on screen.

Log and merged views now retain document endpoints by connection and absolute
line identity. Rendering remains limited to the viewport, including during
selection drags and after a single click. Only copying reads the selected
displayed history; it does not create off-screen widgets or text layouts.
Endpoint lookup accounts for filtering, eviction and late merged data; missing
endpoints clear selection instead of targeting replacement text. Hex keeps its
existing selection mechanism. Search and highlight styling has a bounded cache
of recently visible layouts; font/DPI/atlas resets invalidate cached galleys.
Plain and ANSI-only text delegates to egui's existing layout cache, avoiding the
extra fingerprint cost for those inexpensive jobs.

### Reproduce and measurements

```sh
cargo +stable test -p pigtail --release --locked --offline \
    log_scroll_diagnostic -- --ignored --nocapture
```

The probe creates 1,000- and 10,000-line histories in a 1280 × 900 headless
viewport. It measures 120 frames each of navigation across history, navigation
with a retained selection, and pinned output with one appended line per frame.
The 10,000-line case also runs with ANSI colour spans. Scrolling uses explicit
line navigation, rather than wheel events; it exercises newly visible row
layout and selection maintenance. The smaller history has more viewport overlap
between navigation targets, so it is not a controlled history-size comparison.

The baseline is the prior source with the identical diagnostic added in an
isolated temporary copy. Both executables use stable Rust 1.98 and release
optimization. Three alternating baseline/updated runs were made after builds
finished and the native demo was closed. Each timing below is the mean of the
three per-run medians or 95th percentiles, in milliseconds. Shapes are the
maximum observed drawing-shape count.

| 10,000-line workload | Before median | After median | Before p95 | After p95 | Shapes before / after |
| --- | ---: | ---: | ---: | ---: | ---: |
| Plain scrolling | 0.722 | 0.725 | 0.778 | 0.829 | 139 / 139 |
| Plain scrolling with selection | 12.261 | 0.740 | 24.021 | 0.767 | 19,873 / 139 |
| ANSI scrolling | 0.883 | 0.886 | 0.972 | 0.955 | 139 / 139 |
| ANSI scrolling with selection | 13.421 | 0.905 | 27.020 | 1.175 | 19,873 / 139 |
| Plain incoming output | 0.152 | 0.137 | 0.211 | 0.170 | 134 / 134 |
| ANSI incoming output | 0.168 | 0.157 | 0.219 | 0.191 | 134 / 134 |

The selection-dependent increase in rendering work is removed. Ordinary
scrolling is essentially unchanged in these runs; incoming drawing improves
modestly. These are headless UI/CPU-tessellation measurements, not native GPU
frame times or input-to-paint latency. Appending happens before the timed frame,
so the probe does not measure store append/compaction, capture writes, receive
parsing, plot extraction or the complete application UI. The incoming phase
follows selection removal, so its maximum also includes that transition.
Search/highlight cache correctness is tested separately; the table does not
measure its performance. Actual hardware traffic and native interaction latency
remain necessary to establish whether every reported source of lag is resolved.

### Validation

Regression tests cover a single click and a nearly full-history selection at
10,000 lines in both single and merged views, with bounded drawing shapes and
layout-cache size. The single-click test fails against the previous source,
producing 9,971 shapes in its first case; it passes with the updated viewer.
Copy includes off-screen selected Unicode and empty lines without rendering
them. Existing suites still cover selection reversal, scrolling beyond the
native window, PointerGone, late merged rows, clipboard preservation during
search seeding, context menus and split-pane input ownership. Additional tests
cover wrapped Unicode word/line clicks, Escape, invalidated endpoints,
provisional text changes, search styling, font size, font resets and DPI changes.

Formatting, workspace/all-target Clippy and the demo feature check pass on
stable Rust. The full workspace suite with all features passes in release mode
with `-- --test-threads=1`. Default and all-feature debug workspace suites also
passed during validation, but repeated debug runs were intermittent: the core
throughput check sometimes exceeded its 50 ms wall-clock budget, including
during a concurrent release build, and a saved-session cleanup test failed once
in the parallel suite and passed individually. `serialcore` was not changed by
this work; the release run passes both checks. A native X11 demo capture
verified timestamps, coloured rows, header backdrop, footer and
plot layout. Demo settings/captures remained isolated, and the test window was
closed afterward. Capture retention, raw bytes and the configured line cap are
unchanged.
