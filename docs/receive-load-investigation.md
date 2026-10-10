# Receive load investigation

Tested on 2026-10-10, using synthetic input without serial hardware. The baseline
below records the original behavior. Receive-load optimizations have now been
implemented; the diagnostic tests remain available for comparison.

## Implemented bounds

- Reader channels have 16 slots and admit batches of at most 512 KiB allocated
  heap each: at most 8 MiB of queued batch allocations per port. The separate
  backlog retains its 8 MiB limit and ordered dropped-output notices. Channel
  slots, allocator overhead and the UI's current batch are additional.
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
