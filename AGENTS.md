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
- `crates/pigtail/src/plots.rs` owns device-aware plot tabs, draft previews,
  source routing, paced extraction/backfill and the shared sample budget.
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
- Highlight rules belong to each connection. The window title shows
  **Highlight rules — <connection name>** and stays attached to that connection
  when switching tabs or panes; closing the connection closes its editor.
  Merged rows use their source connection's rules. Right-click a merged row to
  edit its source's rules; merged empty space has no highlight-rule action.
  Adding a rule from selected text applies it to the right-clicked connection.
  A colored highlighter button beside Pin in the footer indicates that valid,
  enabled rules are applied to the displayed Log. Clicking opens that port's
  rules; merged views with several active sources offer a connection selector.
  Hidden highlights, inactive/invalid rules, Hex and ANSI/VT show no indicator.
  Footer controls retain their space in narrow panes; the merged Send to
  selector truncates within the remaining space.
  Rules persist with each saved connection, including an explicitly empty list.
  Legacy shared rules are copied into remembered connections that lack their
  own list, then omitted from the global configuration on save. New connections
  start with independent, disabled ERROR/WARNING/INFO rules. Malformed per-port
  rule lists fall back to empty with a configuration warning.
  Malformed saved-connection entry types return configuration errors rather
  than panicking during per-connection rule and macro migration. Regression
  coverage includes scalar, nested-array and mixed valid/invalid entries.
  The console right-click menu omits **New plot** and **Macros**; those remain
  available from the **+** menu and app menu/shortcut respectively. Selection
  actions for adding a macro or highlight rule remain available.
  Regression coverage checks independent single/merged rendering, editor titles
  and edits after tab changes, closing the editor's connection, merged-row and
  stale selection targets, and migration/persistence of empty and malformed lists.
  Footer frame coverage checks both themes, narrow/wide layouts, visibility
  switches and display modes, and direct/merged editor targets.
  The 2026-10-17 revision passed formatting, app tests and all-target Clippy with
  default/all features, and the demo check. Workspace runs hit the known core
  receive-overload timeout; isolated reruns passed with both feature selections,
  as did the remaining core tests and integration tests. Native visual inspection
  was unavailable because the display server could not be reached.
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
- Custom window header with draggable device tabs, colored type icons, close buttons,
  and minimize/maximize/close controls. Tabs reuse the connection plug, plot chart,
  and merged arrows from the + menu. Connection icons retain the existing status
  colors: green when connected, yellow while connecting/reconnecting or missing,
  red when lost, and gray when disconnected/closed. Plot icons stay green and
  merged icons stay blue. Labels remain neutral. Drag empty header space to move the window,
  double-click it to maximize or restore, and drag an edge or corner to resize.
  Header dragging uses the original primary-press position rather than egui's
  cached widget ownership. Native move/resize loops can consume releases and
  leave that ownership stale; batched pointer motion can also target a different
  widget before the press is processed. Dragging works before the first resize
  in single and split workspaces. Resize borders and overlaid windows retain
  priority; disabled headers and already released clicks never start a drag.
  Issue #157 regressions cover startup, stale ownership, batched motion, both
  split orientations, resize priority, overlays and double-click maximize.
  The 2026-10-21 fix passed formatting, workspace tests and all-target Clippy
  with default/all features, and the demo check on stable Rust 1.98. Native
  inspection was unavailable because the display server could not be reached;
  Windows dragging still needs a native smoke test.
  PR #158 review also covers several mouse presses/releases arriving in one
  frame. Move and resize use only the final press, and completed clicks never
  start either native operation. Completed header clicks still contribute to
  double-click maximize/restore, including when both clicks arrive in one frame.
  Regression frames reproduce conflicting move/resize commands and lost
  double-clicks before the fixes. The review passed formatting, workspace tests
  and all-target Clippy with default/all features, plus the demo feature check
  on stable Rust 1.98.
  Further PR #158 review covers motion and release positions between batched
  clicks. Moving beyond the click tolerance cancels the pending double-click,
  even when the pointer returns before painting or a release arrives without
  a motion event. Rendered-frame regressions cover same-frame and separate-frame
  presses and reproduce unwanted maximize before the correction.
  Formatting, workspace tests and all-target Clippy with default/all features,
  plus the demo check/build, passed on stable Rust 1.98. The initial default
  run hit an updater-test `Text file busy` error; both its isolated rerun and
  the complete workspace rerun passed. Native software-Vulkan/X11 demo inspection
  verified the header and double-click maximize/restore. Synthetic dragging did
  not move the window under Xwayland, so native move/resize still needs a manual
  smoke test, including on Windows.
  View controls, search, clear, export, and the app menu sit above the console;
  ANSI/VT sits beside Hex; highlights are in the overflow menu. A thin footer shows
  one combined connection/error status at the bottom left, line counts and port
  settings, with a clickable pin icon for autoscroll. History eviction does not add a
  “History limited” indicator to the footer. Unpinned Log and merged views
  retain the same top visible line and its wrapped-row/fractional position when
  older displayed lines are evicted. Once that line is evicted, the view stays
  at the top of the remaining history as further lines are discarded. Eviction
  alone never re-enables Pin. Log scroll anchors belong to the rendered view;
  switching connection tabs cannot interpret another connection's line identity.
  Hex invalidates the shared scroll area's Log anchor, so returning to Log does
  not apply a position saved before scrolling in Hex. At the configured line limit,
  each new Log line discards only the oldest excess line; retention no longer drops 10% in
  one batch. Metadata uses a deque and discarded text is compacted periodically,
  independently of logical eviction. Separate text/metadata memory limits may
  discard additional oldest lines when unusually large output exceeds those
  budgets. Raw capture remains complete. The header tint blends subtly
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
  reconnection clears link errors. A link error temporarily takes priority over
  a session failure; clearing it restores that failure, including failures
  reported during the outage. Dropped-command warnings do not replace an
  existing error or warning (including waiting connection states), are never
  deferred behind a later link error, and clear on the next connection state
  update, including successful reconnection. Error details remain available in the
  overflow menu.
  Issue #154 regression coverage checks dropped transmit/file/control commands,
  every waiting state, competing errors/warnings in both arrival orders, and
  rendered-footer recovery. Formatting, app tests with default/all features,
  all-target Clippy with default/all features, and the demo check passed.
  Workspace runs encountered core load-test timeout and throughput timing
  failures; both passed isolated reruns. Native GUI inspection was unavailable
  because display automation tools were absent.
  Console context menus belong to the pane's selected view rather than a
  virtualized row. Incoming output may scroll or evict the clicked row while
  the menu stays open at its original position. Row actions keep the original
  line and connection target, including in merged logs; they do not follow
  whichever row later appears beneath the pointer. This also applies to Hex.
  Console menu gestures capture the original target from the previous rendered
  frame on button press and open on release independently of the row widget.
  Incoming bursts can scroll or evict that row before press processing or between
  press and release without preventing the menu from opening while pinned.
  Only visible widget targets are cached; labels are resolved when a press occurs.
  Menus still close on an action, Escape, or an outside click.
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
  markers; the log line limit and clear-history boundaries apply.
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
- Each connection owns its named transmit macros, with reorderable command,
  delay and regex wait steps, finite or indefinite looping, and assignable
  Ctrl+Shift+0 through Ctrl+Shift+9 shortcuts. New connections start empty.
  The catalog and Add/Edit titles name their connection and keep that target
  through tab or pane changes. In merged views, open Macros for the selected
  **Send to** connection; disconnected connections can still be edited.
  **Copy from connection** in the catalog offers individual macros or **Copy all
  macros** from another open connection. Copies are independent and keep their
  assigned key if free on the destination, otherwise use its first free key in
  1–9, 0 order; no free key leaves the copy Unassigned. Unassigned/invalid source
  keys also use the first free destination key. Shortcut lookup, conflict checks,
  run controls, edit confirmation and deletion indices are scoped to the owner.
  Each connection's macros persist with its saved session. Legacy shared macros
  are copied into saved connections lacking an explicit list; empty lists remain
  empty. With no saved destination, old definitions remain available under
  **Saved macros from previous version** in the copy menu, without being inherited
  by new connections. Malformed per-connection lists fall back to empty and warn.
  The catalog list scrolls with Add/Copy controls outside its scrolling area.
  The Add/Edit macro window includes a **Shortcut** selector with **Unassigned**
  and all ten digit shortcuts. Occupied shortcuts name their owner and are
  disabled; the catalog's existing reassignment confirmation remains available.
  New drafts suggest the first free shortcut in 1–9, 0 order, without reserving
  it. Save commits the selected shortcut with the definition; Cancel or closing
  discards it. Saving rejects conflicting or invalid assignments. Command and
  wait-for fields center their text vertically within the step row.
  Rendered-frame coverage checks both step field alignments, selecting/saving
  and cancelling shortcuts, occupied entries, persistence and duplicate rejection.
  Per-connection regressions cover copy-one/all, key preservation/collisions and
  exhaustion, source independence, same-key execution on separate ports and
  merged Send to, catalog/editor ownership after tab changes and closure, and
  running edit/delete isolation. Configuration tests cover legacy migration,
  empty/explicit lists, malformed options and orphaned imports.
  The 2026-10-17 macro revision passed formatting, workspace tests and all-target
  Clippy with default/all features, and the demo check. Native visual inspection was unavailable
  because the display server could not be reached.
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
- Export the current (filtered) Log subset to `.txt` or `.csv`. After choosing
  a destination, the UI copies the selected text and minimal metadata into an
  immutable snapshot; timestamp/CSV formatting and all file I/O run on a worker.
  Incoming output, filtering changes, eviction and closing tabs cannot change
  that snapshot. Merged exports retain their displayed ordering and source tags.
  One export runs at a time across both panes. A nonmodal **Exporting view**
  window shows the destination, line progress and **Cancel**, leaving the console
  usable. It remains visible while cancellation finishes; Cancel is disabled
  once atomic replacement starts. The worker writes a unique temporary file
  beside the destination, flushes and syncs it, then atomically replaces the
  destination. Accepted cancellation and write/commit failures preserve an
  existing destination and remove the temporary file. Failures use the existing
  **Couldn't export** dialog. Closing the app requests cancellation and waits for
  worker cleanup before exiting; a replacement already in progress finishes first.
- Startup notice when a newer release is published, with "skip this version" and
  one-click download and update. Startup checks can be switched off in Settings.

### Export validation

The 2026-10-16 export revision passed formatting, workspace tests with default
and all features, all-target Clippy with default/all features, and the demo
check/build. Final focused tests cover active/merged snapshots after source
closure, CSV escaping, progress/Cancel rendering, worker error reporting,
exit-time cleanup, cancellation during serialization (including long lines), and preserving the
destination/removing temporary files on cancellation or write/commit failure.
Native X11 demo inspection used a throttled 500,000-row fixture to verify the
progress window alongside split logs/plots. Wayland ignored synthetic mouse
events, so native Cancel interaction was covered by rendered-frame and worker
tests instead. Snapshot creation still copies the selected resident text and
minimal metadata on the UI thread; export memory scales with that snapshot,
without retaining another formatted copy of the full output.

### Plot tabs

**+ → New plot** opens a draft with a tab name and one chart. Each extraction
rule selects one connection and uses key/value pairs (prefix and separators,
including optional space pairing) or regex with named numeric captures. The
selector includes unavailable remembered references and shows connection state.
The paced preview lists matching series and invalid/no-match feedback from
retained output. Create selects a new tab in the current pane; Cancel creates
nothing. Empty tabs/charts and unavailable devices are allowed. Tab options and
chart Settings use Apply/Cancel drafts; cancelling preserves definitions and
live samples. Controls scroll in small windows, with action buttons outside.
The setup editor has a compact numbered chart selector and edits one chart at
a time; a chart's Settings button selects that chart. All settings for the
selected chart remain directly visible, including line prefix, value separators
and space pairing for name/value parsing. Add/remove chart and rule buttons
remain visible rather than living in menus or collapsed sections. New charts
added in the draft start with one rule, reusing the selected chart's source when
available. **Link chart time axes** is enabled by default and is the inverse of
the saved independent-axis option. Explanations and parsing examples live in
tooltips that appear immediately on hover and stay visible through clicks.
Each rule's bounded preview shows an actual finalized received line that
produced numeric values, the extracted names and numbers, and the series found
so far. Examples retain their original prefix and whitespace; local echo,
session markers and provisional lines are excluded. Preview distinguishes
scanning, waiting for output, no matching values, invalid patterns and missing
or ambiguous sources; hover reveals error and history details. Changing a rule
or clearing its source invalidates its old example. Preview examples belong
only to the draft, are copied at most once per processing batch, and never add
source text retention to normal live extraction.
The 2026-10-15 setup refresh passed formatting, workspace tests with default
and all features, Clippy for all targets with default and all features, and the
demo feature check/build. After the final layout adjustment, affected app tests
and Clippy passed again with default/all features. Regression coverage includes
real preview lines and values, prefix/Unicode preservation, local echo and
partial-line exclusion, clear/edit invalidation, source availability, stable
window width, chart selection/add/remove/cancel and immediate help through
press/release. Native software-Vulkan/X11 demo inspection verified chart-specific
Settings, regex and name/value previews, hover help during clicks, and scrolling
settings with fixed actions at 1280 × 900 and 720 × 540.

Plot tabs use the same ordering, drag/drop, split/move, middle-click close and
close confirmation as other tabs. Closing one keeps all serial connections open.
Plots never implicitly transmit typing, paste, macros or files to a device.
Each chart has Follow, Fit, Settings, a Series menu for color/visibility/Y2, and
an overflow menu for move up/down, duplicate and remove. Legend visibility also
updates the saved series preference. Duplicates get fresh chart/rule IDs. Add chart
works even after removing the last chart. Vertical dividers preserve relative
heights; double-click a divider restores equal heights for that pair.

Time axes link within each tab by default. Pan/zoom and Follow share a horizontal
range; Fit uses the union of visible sample time ranges and each chart's own
value range. A linked vertical cursor compares measurements. Independent time
axes give every chart its own navigation and Follow/Fit. Y2 retains the existing
affine mapping, secondary labels and original-value hover tooltips.

Connections, plots, charts and rules have persistent IDs. Source references also
retain device identity and a remembered label, but never bind by editable name
or runtime PortId. Removing a connection preserves its samples and leaves an
unresolved reference; opening a new identical device requires explicit rebinding.
Ambiguous references produce warnings and do not select an arbitrary source.
Series keys include connection ID, rule ID and extracted name, so overlapping
rules and same-named measurements stay separate. Labels include device and rule.
Styling survives edits/backfills while the key remains the same.

Extraction runs for inactive tabs, independently of console search, Log/Hex/ANSI,
and raw capture. It uses finalized receive lines, skips local echo and session
markers, and rechecks a provisional line when finalized instead of duplicating
partial values. A partial line stranded before local echo is skipped so it cannot
block later samples; only the provisional tail waits for completion.
Retained restored history uses the application's shared clock
basis, including negative times; capture markers split traces across gaps.
Each rule keeps at most 64 boundary IDs so history growth can preserve an evicted
marker; when older provenance is unavailable, replay uses a conservative gap.
Backfills and draft previews run outside painting in rotating, bounded batches
with a four-millisecond frame budget and at most 4,000 lines per rule per batch.
Routes index consumers by source; regexes compile only when definitions change.
The UI explains that discarded source history cannot be recovered. Growing the
history limit replays the resident suffix while preserving sparse older samples.
Clear invalidates all consumers of that source, preserving other sources/styles.

Plot memory shares a one-million-sample ceiling across all charts; the history
setting still controls each series' requested cap. Each chart allows at most 64
series, with up to 64 rules, 16 charts per tab and 32 plot tabs. Definitions and
presentation preferences are saved, including layouts, bounds, time-axis mode,
source references, styling, pane placement and selection. Saved horizontal bounds
carry the app clock anchor and rebase at restart so a manual view still shows
the same historical interval. Backfill preserves saved manual chart bounds;
new empty charts defer their initial Fit until samples arrive. Samples are rebuilt
from restored source history, never serialized as another raw capture. Legacy
configs load with no plots. Legacy connection IDs are assigned and saved without
inventing a migration for extraction rules, which were never persisted. Malformed
new optional settings fall back individually and emit configuration warnings.

Clicking a sample selects its actual source connection's Log, reveals the line
and scrolls to its absolute source-line reference. Missing, ambiguous, evicted
or cleared references show a concise explanation. Log selections mark only charts
with retained samples from that line, including across split panes.
Sample navigation across split panes runs after both panes finish rendering,
closing the destination's search so its remembered filter cannot hide the line.
The query and selected search display mode remain available when search reopens.

Plot-tab validation on 2026-10-14 passed formatting, workspace tests with default
and all features, Clippy for all targets with default and all features, and the
demo feature check. Regression coverage includes source/rule collisions, partial
lines, inactive routing, clear/reconnect gaps (including evicted markers during
history growth), global sample limits, malformed/legacy config, clock rebasing,
create/cancel/apply, chart duplication/reordering/removal, linked Fit/Follow/pan,
independent pan, split restore, point navigation and input isolation. Native
lavapipe/X11 demo inspection verified the two-chart split layout, small windows,
scrolling draft actions, device selectors, source closure/warnings, dividers,
shared cursor and selecting Device 2's sample into Device 2's Log pane. Real
serial reconnection still needs a hardware check; the demo cannot verify it.
The 2026-10-15 review added rendered-frame regression coverage for clicking a
sample with an active filter in the destination Log, with the plot in either
pane and both split orientations. Formatting, workspace tests and Clippy with
default/all features, and the demo feature check passed. Native demo inspection
was unavailable in that review environment because display tools were absent.
Further review found that initially empty linked charts consumed their shared
Fit before samples arrived. The shared Fit now waits for plottable data;
rendered-frame regression coverage checks the resulting common time range with
Follow both on and off. Plot-history settling releases excess series allocation
only when a retention budget decreases, avoiding repeated shrink/grow copies as
samples arrive between UI frames.

### Saved session history

Clear console immediately resets the connection's Log, Hex and ANSI/VT state.
The UI discards incoming batches and dropped-output notices until the reader
acknowledges every pending clear in event order. The reader sends each boundary
after resetting framing/backlog and attempting capture truncation, including
when truncation fails; errors and connection/transfer events remain visible.
Output after the final boundary is retained even if already queued when the UI
finishes clearing. Replacing a reader resets its pending acknowledgements.

The 2026-10-16 clear-boundary revision reproduced issue #83 before the fix.
Regression coverage includes racing batches and gap notices, repeated clears,
fresh queued output, control events, reader replacement, capture failures and
clearing a full channel at EOF. Formatting, workspace tests and all-target
Clippy with default/all features, and the demo feature check passed. Native
GUI inspection was unavailable because display tools were absent.

Captures sharing an app clock anchor are ordered by their first raw record.
Empty captures use the raw file's modification time (creation or latest clear),
so a cleared empty capture stops older history without hiding or deleting output
from subsequent reconnects in the same run.

Saved captures have no age limit. **Settings → Max log lines** controls restored
Log history and saved captures per device identity. Restore walks newest-first
by framed log lines, including closing session markers, with no raw-byte restore
budget; selected captures replay in bounded chunks. It honors each capture's framing mode and existing clear boundaries.
Raw capture remains independent of search filters and the selected view.

Background cleanup starts after history restore, runs every five seconds and
also receives settings changes. It counts complete raw records incrementally
with the log framer, including a final partial line. A capture stays whole while
any of its lines or its closing marker contributes to the retained Log window;
when none fits, its `.session.bin` and metadata sidecar are deleted together.
Active writers are protected by process-local registration and OS locks on
`.session.lock` sidecars, and their raw capture remains complete. Cleanup defers
a device while another Pigtail process owns one of its captures; other devices
continue cleanup. Lock sidecars are removed with discarded captures. A persistent
`.session-directory.lock` serializes capture creation and cleanup across
processes so a reused name cannot inherit an unlinked lock handle or a stale
deletion decision. Clear
boundaries discard older saved captures too. Every clear invalidates cached line
counts, including repeated clears followed by regrowth between cleanup passes.
Foreign writers invalidate their device's cached counts; changed closed files
are recounted before their lines contribute to deletion budgets.
Capture creation registers the live writer before cleanup can classify it as
closed. Unreadable captures are validated and preserved even outside the line
window.
The obsolete `session_retention_days` setting is ignored and omitted on save;
there is no age retention control or cleanup confirmation in Settings.

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

In **Settings → Appearance → Text font**, choose the built-in Hack font (default)
or a monospace family installed on the current system. The global choice applies
immediately to Log, merged logs, Hex, ANSI/VT and monospace text editors in both
panes, and persists by family name across restarts. Discovery uses `fontdb` once
per process, including system and user font directories; restart after installing
fonts. The selected face (including its collection index) is loaded on demand;
other system fonts are not copied into egui. Missing or unreadable choices fall
back to Hack and emit a warning without overwriting the saved preference.
Reset restores Hack. Font changes invalidate text layouts and recompute wrapping
and VT cell dimensions; unpinned Log views retain their line anchor. Only egui's
existing default fonts are bundled, and its symbol fallbacks remain available.
The 2026-10-15 system-font revision passed formatting, workspace tests and
all-target Clippy with default/all features, and the demo feature check.
Coverage includes saved family names and malformed settings, installed-family
filtering, face loading, missing-font fallback, host-font rendering and clicking
the settings picker with live font replacement. Native visual inspection was
unavailable because display and screenshot tools were absent.

In **Settings → Appearance**, choose Dark or Light and a separate Blue, Green, or
Red base color. Each theme remembers its own base color across restarts.
Plain Page Up / Page Down and arrow keys retain their normal terminal behavior.
Ctrl+wheel over the console changes the font size by one point per frame.
Nonzero wheel deltas apply at gesture start, move, and end; zero-delta
gesture boundaries do not change the size.

Use **Ctrl+Shift+Space** to pin the console to the bottom. On connection tabs,
**Ctrl+Shift+Q / W / E** selects the **Log / Hex / ANSI** view.
Use **Ctrl+Shift+P** on a connection to select the most recently used plot tab
referencing it, including in the other split pane. If none exists, open New plot
with that connection preselected. It does nothing on merged or plot tabs.
Use **Ctrl+Shift+M** to open the transmit macros window.
The console toolbar has no plot button. **+ → New plot** and **Ctrl+Shift+P**
provide plot access. The **+** menu uses plug, chart and merging-path icons for
**New connection**, **New plot** and **New merged view**, with aligned icon slots
and accessible text labels. New plot remains disabled at the plot-tab limit.
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

Console row pitch is rounded to device pixels, matching egui's text layout.
Log, merged and Hex virtualization and scroll offsets use that same pitch so
incoming bursts do not change fractional baseline positions or line spacing.
Rendering regression frames cover pinned growth, unpinned eviction, wrapped
and merged logs, 1×/1.25×/1.5×/2× display scales, multiple font sizes and a
100,000-line history.

Log and merged rows share their console style and paint selectable text within
the existing row container. Plain unstyled lines bypass styling-range allocation.
Timestamp labels retain their normal accessibility responses; their formatted
values use a frame cache that discards undrawn entries and refreshes wall-clock
formats once a second. Mark, previous-line and timestamp changes invalidate the
cached value. See the live measurements in the receive-load investigation.

Log and merged-view selection uses stored-line endpoints in
`crates/pigtail/src/panes/log_selection.rs`, independently of the virtualized
row widgets. A plain click or a retained selection never expands the rendered
range beyond the viewport. Copy reads the selected displayed lines directly, including empty
lines and Unicode text; timestamps and source labels are excluded. Endpoints
follow stable connection/line identities across scrolling and late merged data,
and selection clears if an endpoint is evicted, cleared or filtered out.
Word/line clicks, Shift-click, drag reversal, selection edge scrolling (including
PointerGone), selection colors, search-selection capture, context menus and
text accessibility remain supported. Scrollbar presses and drags never start
text selection, including when a floating scrollbar overlaps a text row or the
held pointer moves from the scrollbar into the console. Search/highlight text
layouts are cached for recently visible rows only, with invalidation for text, styling (including
search and highlight case sensitivity), wrapping,
font size, DPI and font-atlas resets. Plain and ANSI-only jobs use egui's cache
directly. The 2026-10-14 measurements and validation are recorded in the
receive-load investigation.

The 2026-10-14 line-retention change was checked with exact-cap, stable-index,
Unicode, continuation, color-span, clear, shrink and memory-budget tests. The
throughput test now feeds 75,000 lines through a 20,000-line store so it exercises
sustained eviction. A storage-only optimized probe compared the prior batched
store with the deque store: after filling each cap, 500,000 appends of a 48-byte
line took median 35.3/33.2 ms at 1,000 lines, 51.1/46.9 ms at 10,000 lines and
76.1/55.6 ms at 100,000 lines (before/after, five alternating trials, Rust 1.98).
These synthetic timings exclude framing, filtering, rendering and real serial I/O.

Dependencies were checked against the registry and updated to current stable versions
on 2026-10-11. The GUI uses eframe/egui 0.36.2, egui_plot 0.37.0 and wgpu 30.0.1.
This includes the upstream fix for DX12 diagnostic hooks being initialized when
DX12 is disabled, which caused AMD Vulkan presentation to flood Windows logs.
Environment overrides remain available; the Windows default is described below.
The migration uses parent-UI panels, output hooks for clipboard selection capture,
and fractional font advances for console wrapping. Headless UI tests explicitly
discard texture uploads because they have no GPU consumer.
The Windows migration passed workspace tests, Clippy, formatting and the demo
feature check. A captured demo frame confirmed the saved dark theme, console and
plot layout; an automatic-backend run no longer emitted the repeated AMD DXGI
resource-state diagnostics. This machine still logs a missing Rockstar Social
Club Vulkan layer manifest registered by another application. Linux runtime and
Windows sleep/resume validation remain necessary on the affected hardware.

The application uses eframe's wgpu renderer. Windows defaults to Direct3D 12
only, excluding Vulkan from instance creation and adapter enumeration. Store
certification of package 1.4.0.0 on a Surface Laptop Go reported a launch-time
access violation (`0xc0000005`) in Intel's `igvk64.dll` 31.0.101.2125 at offset
`0x64ea82`. This matches the class of older Intel Vulkan startup crashes in
[wgpu #10124](https://github.com/gfx-rs/wgpu/pull/10124); the locked wgpu 30.0.1
does not include that fix. The event log has no stack, so the exact Vulkan call
is unconfirmed. Preferring DX12 while leaving Vulkan enabled is insufficient
because startup enumerates enabled backends before choosing an adapter.
The Windows default applies to direct, Store and demo builds. Explicit
`WGPU_BACKEND` overrides remain available for diagnostics and can opt back into
Vulkan; `WGPU_POWER_PREF` and other upstream settings are preserved. Other
platforms retain upstream backend selection. There is no automatic Vulkan
fallback on Windows. Regression tests check the default instance backend mask
and explicit single/multiple-backend overrides without loading GPU drivers.
The direct wgpu dependency enables its standard backend features (eframe disables
wgpu's defaults). This renderer replaces the glow/glutin renderer involved in
the AMD `atio6axx.dll` crash on Windows resume.

The 2026-10-26 Windows backend revision passed formatting, workspace tests and
all-target Clippy with default/all features, Store-feature graphics tests, and
the demo feature check/build on stable Rust 1.98.1. The default-backend
regression failed with the old backend selection and passed with the fix.
A Windows demo launch with `WGPU_BACKEND` unset selected the AMD Radeon RX
6800 XT via DX12, enumerated only DX12 adapters, and loaded Direct3D 12 modules
without a Vulkan loader/driver. The Windows SDK MakePri/MakeAppx build succeeded
for the unsigned `pigtail-store-1.6.0.0-x64.msix`. Native visual inspection was
unavailable because the computer-use native pipe could not be reached. Testing
the installed package on the certification Surface/Intel driver, real serial
I/O and Store certification remain outstanding.

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
and humidity charts in an **Environment plot** tab. A
**Merged device logs** tab combines both simulated connections. All four tabs
share the main pane without a split, with device 1 selected at startup.
Both devices provide 90 temperature/humidity readings with distinct curves,
so each chart shows two lines rather than a line and a single sample dot.
Demo timestamps use the shared app clock, keeping merged rows chronological
and plot samples aligned with their displayed times despite different capture
lengths. Regression coverage checks merged ordering and all four series' sample
counts.
The PR #159 review passed formatting, workspace tests with all features,
default-feature app tests, all-target Clippy with all features, and VS Code task
JSON validation on stable Rust 1.98. The new merged chronology regression failed
before the clock correction. Native inspection was unavailable because the
display server could not be reached.
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

### Microsoft Store MSIX

#### Privacy policy publication

`docs/site/privacy/index.html` is the public policy for both distribution
channels. Keep it aligned with local captures/configuration/diagnostics, device
identifiers, file and clipboard access, retention/deletion, support submissions,
and network activity. Store builds do not contact the GitHub updater;
direct-download builds have opt-out startup checks and user-requested downloads.
Update the policy's visible date when its content changes. Its contact address
matches the package maintainer in `crates/pigtail/Cargo.toml`.

`.github/workflows/privacy-policy.yml` uploads only `docs/site` and deploys it
to GitHub Pages. The default policy URL for Partner Center is
`https://rustypig91.github.io/pigtail-serial-console/privacy/`. It becomes live
only after a successful deployment. In repository **Settings → Pages**, select
**GitHub Actions** as the build source. The `github-pages` environment must allow
release tags (`v*`) and the branch used for manual publication (normally `main`);
a branch-only deployment restriction will block tag builds. No custom token or
domain is required. Pages deployment replaces this repository's entire Pages
site, so keep any future public pages under `docs/site` too.

The existing build workflow calls the reusable policy workflow after its
`release` job succeeds, alongside APT publication. It publishes the policy from
the released commit, and does not deploy on PRs, ordinary branch builds, or
failed releases. This explicit call avoids relying on a `release` event, which
is not emitted to other workflows when the release is created with
`GITHUB_TOKEN`. Use **Actions → Publish privacy policy → Run workflow** from
`main` for first publication or a policy-only update without releasing the app.
The workflow reports the deployed policy URL in its job summary. Deployment
runs serialize without cancelling an in-progress Pages publication.
Artifacts are named by run/attempt; deployment uses the build job's recorded
name so full reruns avoid upload collisions and deploy-only retries reuse the
successful build artifact (within its retention period).

Setup references: [GitHub Pages custom workflows](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages)
and [publishing source settings](https://docs.github.com/en/pages/getting-started-with-github-pages/configuring-a-publishing-source-for-your-github-pages-site).

Partner Center listing PNGs live in
`crates/pigtail/packaging/windows/store-listing/`: app tiles at 71, 150 and
300 pixels square, box art at 1080 and 2160 pixels square, and poster art at
720 × 1080 and 1440 × 2160. Regenerate with
`python3 scripts/generate-store-images.py` (requires Pillow). These use the
original `crates/pigtail/src/icon.png`, preserve its proportions, and center it
on transparent app tiles or dark, padded box/poster backgrounds. They are listing
uploads, separate from the MSIX manifest assets generated during packaging.
The poster exports follow the explicit pixel sizes shown in Partner Center.

The Microsoft Store edition is built separately with the `microsoft-store`
feature. Direct-download builds must omit that feature. It replaces the app
menu's **Check for updates** with **Open in Microsoft Store**, opening product
`9P5WS4PFD96H`. Settings explains that Microsoft Store manages updates and has
no GitHub startup-check control. Both startup/manual GitHub checks and download
entry points reject Store builds even if old update settings are present.
Pigtail never requests Store installation, termination or restart itself;
Windows, Store settings and enterprise policies control external updates.
The GitHub and Store channels can publish at different times. Store builds do
not use GitHub availability as an update signal.

Store configuration, captures and diagnostic logs use the application directory
name `pigtail-store` instead of `pigtail`, with the existing `directories`
platform conventions. Nothing is imported automatically. On Windows the
requested paths are `%APPDATA%\pigtail\pigtail-store\config\pigtail.toml` and
`%APPDATA%\pigtail\pigtail-store\data\`; Windows package virtualization may
redirect these within the package's per-user storage. **Settings → Folders**
opens the locations visible to that edition. Both editions can coexist without
writing the same live files, but a serial device may only allow one process to
open its port at a time. To copy settings or history manually, first launch and
close the Store edition to create its folders, close both editions, back up the
destination, then copy the direct edition's `pigtail.toml` and (optionally) the
whole `sessions` directory to the corresponding Store locations. Never merge
live session files. This is a manual migration procedure, not an import UI.
Do not promise capture retention across Store uninstallation; back up/export
important data first. Demo builds still use temporary directories.

The registered product is **Rusty’s Pigtail – Serial Terminal**:

| Manifest field | Value |
| --- | --- |
| Identity Name | `rustypig91.RustysPigtailSerialTerminal` |
| Identity Publisher | `CN=B41BE868-9458-4B44-AE02-EA569AC9D53B` |
| PublisherDisplayName | `rustypig91` |

`crates/pigtail/packaging/windows/msix/AppxManifest.xml` packages the existing
Win32 application at medium integrity with `runFullTrust`; it is not a UWP
AppContainer conversion. That capability is needed for existing native serial
access and desktop file dialogs. No driver, service, administrator elevation or
separate updater is installed. The initial package supports x64 Windows 10
2004 (build 19041) or newer. ARM64 and x86 packages are not produced.

From Windows with Rust, Visual Studio C++ Build Tools and the Windows SDK:

```powershell
./scripts/build-msix.ps1
# Only when a packaging revision needs a higher Store version:
./scripts/build-msix.ps1 -PackageVersion 1.4.1.0
```

The script builds only the Store feature (never demo) with a static MSVC CRT,
in `target/store`, independently of existing release binaries. It generates
50/44/150-pixel logos and 200% variants from the existing 512-pixel icon, fills
the manifest version, indexes the scaled assets with the SDK's MakePri into
one `resources.pri`, and calls MakeAppx with validation enabled.
The default package version maps workspace `major.minor.patch` to
`major.minor.patch.0`; prerelease versions need an explicit numeric override.
Store versions must increase, each component is at most 65535, the first is
nonzero, and the fourth is zero (reserved for Microsoft). An override affects
only package versioning; the app's displayed version remains the workspace
version. Staging is temporary and cleaned even on failure; Cargo environment
overrides are restored. The unsigned output is
`target/release-assets/store/pigtail-store-<four-part-version>-x64.msix`.
Relative `-OutputDirectory` paths resolve from the repository root using
PowerShell's current location, independently of the host process directory.
Cargo overrides and the caller's location are restored even if staging cleanup
fails; the cleanup failure remains an error.
Plain `RUSTFLAGS` are split on whitespace without producing empty compiler
arguments; existing `CARGO_ENCODED_RUSTFLAGS`, including an explicitly empty
value, take precedence when appending the static CRT option.
It is for Partner Center ingestion, not public unsigned sideloading. Microsoft
signs the package during Store publication; no certificate secret is needed.

The separate **Microsoft Store MSIX** workflow tests the Store edition and
builds on relevant PRs, main pushes, version tags and manual dispatch. Download
its `pigtail-store-msix-<run-id>` artifact for submission. It does not add an
unsigned MSIX to GitHub Releases or publish to Partner Center automatically.
Rerunning the job replaces that run's artifact. The workflow also runs
`scripts/tests/build-msix.Tests.ps1`, which simulates build and cleanup failures
without Rust builds or SDK tools and checks relative output paths and caller
environment/location restoration.
Tag runs require the tag to match the workspace version. Existing EXE/MSI,
portable and Linux packaging remains in the `build` workflow.

Before the first submission, test on Windows with a real serial device. From
a Windows SDK Developer PowerShell, unpack a built package into a persistent
test directory, then register its manifest for local development:

```powershell
MakeAppx.exe unpack /p <path-to.msix> /d <test-directory>
Add-AppxPackage -Register <test-directory>\AppxManifest.xml
```

Local registration may require Windows Developer Mode. Keep the test directory
while registered and launch the Start menu entry rather than the bare EXE.
This tests package identity; test the final Store installation separately.
Verify launch/icons, enumerate/connect/reconnect, receive/capture/send, modem
controls, file dialogs/export, macro and plot persistence, and independent
folders with the direct edition installed. Check that the Store menu opens
the right product and no GitHub update notice appears. Test an upgrade from an
older package through the Store with captures/settings present, including an
active serial session; do not assume external updates can never interrupt it.
Local development registration does not test Store signing or update delivery.

In Partner Center, create a submission, upload the `.msix` under **Packages**,
complete availability/pricing, age ratings, properties and English listing
(description, screenshots, support/privacy URLs as required). Explain
`runFullTrust` in certification notes: the Rust desktop serial terminal uses
native Windows serial APIs, file dialogs and local capture/export; it installs
no drivers/services and leaves updates to the Store. Mention that live serial
I/O needs a Windows-supported serial adapter/device, and supply concrete
reviewer test steps. Do not submit a demo-feature binary. Review all submission
sections before publishing; the code/workflow does not publish a listing.

The initial 2026-10-24 implementation passed formatting, workspace tests with
default/all features, all-target workspace Clippy with default/all features,
and the demo feature check on stable Rust 1.98. Final app reruns after UI
adjustments passed with default/all features. Rendered frames cover the menu
action and scroll-reachable update settings in both themes; regressions also
cover Store check/download rejection and independent config/data/log paths.
PowerShell parsing and invalid-version/environment-restoration checks passed
using portable PowerShell on Linux; manifest XML/identity and workflow YAML
were checked. No Windows SDK or connected native GUI was available here, so
MakePri/MakeAppx, actual MSIX installation, serial I/O, Store delivery and
upgrade preservation still require the Windows workflow and native testing.

The PR #160 review fixed relative output resolution, caller-state restoration
after staging cleanup failures, and artifact replacement on workflow reruns.
Both PowerShell failure regressions reproduced before their fixes and passed
afterwards. Store-feature app tests (341 passed), all-feature workspace tests,
all-target all-feature Clippy, formatting, the demo check and workflow YAML
validation passed on stable Rust 1.98. Native Windows validation remains required.

A further PR #160 review fixed leading/trailing whitespace in plain `RUSTFLAGS`
creating empty encoded arguments and preventing Cargo from probing rustc.
The regression failed before the fix; a minimal Cargo build also reproduced
the compiler failure and passed with the corrected flags. PowerShell coverage
checks encoded-flag precedence, whitespace-only/unset flags, and restoration
after build and cleanup failures. These checks, Store-feature app tests,
all-feature workspace tests, all-target all-feature Clippy, and formatting
passed on stable Rust 1.98. Native Windows package testing remains outstanding.

A subsequent PR #160 review fixed an explicitly empty
`CARGO_ENCODED_RUSTFLAGS` falling back to plain `RUSTFLAGS`. A minimal Cargo
probe confirmed empty encoded flags suppress plain flags; the packaging
regression reproduced the incorrect fallback before the fix and passed after
it, including caller-state restoration after build and cleanup failures.
The empty-value case runs on PowerShell 7.5+, which preserves empty environment
variables. PowerShell regressions, Store-feature app tests (341 passed),
formatting and diff checks passed on stable Rust 1.98.

References: [manual desktop packaging](https://learn.microsoft.com/en-us/windows/msix/desktop/desktop-to-uwp-manual-conversion),
[local package testing](https://learn.microsoft.com/en-us/windows/msix/desktop/desktop-to-uwp-debug),
[Store signing](https://learn.microsoft.com/en-in/windows/apps/publish/faq/get-started-with-the-microsoft-store),
[version rules](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/package-version-numbering),
and [Store links](https://learn.microsoft.com/en-us/windows/apps/develop/launch/launch-store-app).

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
