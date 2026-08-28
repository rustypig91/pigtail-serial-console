//! The reader thread: one blocking reader per port, batching, and the reconnect
//! state machine (spec §5, §7.6).
//!
//! Non-negotiable rules honoured here:
//! 1. Batch before sending (~16ms or a few thousand lines).
//! 2. Never block on the UI: the channel and local backlog are bounded; if the
//!    UI remains behind, oldest live-view batches are dropped with a visible
//!    gap notice — reading always takes priority.
//! 3. The raw session log is written before any parsing.
//! 4. The UI owns the store; data flows only through channels.

use crate::clock::{SessionClock, Timestamp};
use crate::config::{PortConfig, PortIdentity};
use crate::enumerate::{enumerate_ports, match_identity, MatchResult};
use crate::framer::{FramedLine, Framer};
use crate::session::{SessionMeta, SessionWriter};
use crate::source::{ByteSource, SerialSource, SourceError};
use crate::store::{LineFlags, PortId};
use crate::wake::Wake;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const READ_BUF: usize = 64 * 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(16);
const BATCH_MAX_LINES: usize = 4000;
/// Heap budget for batches that could not be handed to the UI yet.
///
/// The channel itself is bounded, but without a second bound here a stalled UI
/// (for example while a native file dialog is open) lets the reader retain
/// output forever. When capture is available, the session writer has already
/// recorded these bytes before they reach this queue; crossing the budget only
/// discards stale live-view copies and reports the gap to the UI.
const MAX_BACKLOG_BYTES: usize = 8 * 1024 * 1024;
// Kept short so an interactive prompt's echo (characters the device sends back
// with no trailing newline) appears promptly instead of feeling laggy.
const PROVISIONAL_AFTER: Duration = Duration::from_millis(20);
#[cfg(not(test))]
const CHANNEL_CAPACITY: usize = 1024;
/// Small under test so a full channel — the state the blocking sends have to
/// survive — is reachable in a few milliseconds rather than the sixteen
/// seconds of output it takes at the real size. Nothing here depends on the
/// number itself; integration tests link the lib built without `cfg(test)`
/// and so still run at the real capacity.
#[cfg(test)]
const CHANNEL_CAPACITY: usize = 4;

/// Connection state machine (spec §7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
    Lost,
    Reconnecting,
    Closed,
}

impl std::fmt::Display for ConnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ConnState::Disconnected => "disconnected",
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::Lost => "lost",
            ConnState::Reconnecting => "reconnecting",
            ConnState::Closed => "closed",
        };
        f.write_str(s)
    }
}

/// A batch of framed lines plus the raw bytes that produced them.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub lines: Vec<FramedLine>,
    /// Raw bytes exactly as read, for the live hex view.
    pub raw: Vec<u8>,
}

/// An event from a reader thread to the UI.
#[derive(Clone, Debug)]
pub enum ReaderEvent {
    State(ConnState),
    Batch(Batch),
    /// The UI fell far enough behind that old live-view batches had to be
    /// discarded. Ordered ahead of the retained batches, so the UI can put a
    /// visible boundary exactly where the gap occurred.
    OutputDropped {
        raw_bytes: u64,
        line_updates: u64,
        /// Logical stream position of the first discarded batch. This can be
        /// older than the eviction itself when an open line spans batches.
        at: Timestamp,
    },
    Error {
        scope: ErrorScope,
        msg: String,
    },
}

/// What an error is actually about, so the UI can tell a broken link from a
/// failure alongside a working one. The two need opposite handling: a
/// successful (re)connect makes a `Connection` error obsolete, but says
/// nothing about a capture file that couldn't be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorScope {
    /// Opening or reading the device: the link is down, and the reader is
    /// either retrying or giving up.
    Connection,
    /// Something beside the link — the capture file, a transmit, a control
    /// line — while the connection itself may be perfectly healthy.
    Session,
}

impl ReaderEvent {
    fn connection_error(msg: impl Into<String>) -> Self {
        ReaderEvent::Error {
            scope: ErrorScope::Connection,
            msg: msg.into(),
        }
    }

    fn session_error(msg: impl Into<String>) -> Self {
        ReaderEvent::Error {
            scope: ErrorScope::Session,
            msg: msg.into(),
        }
    }
}

/// Commands from the UI to a reader thread.
#[derive(Clone, Debug)]
enum ReaderCommand {
    Transmit(Vec<u8>),
    SetDtr(bool),
    SetRts(bool),
    SendBreak,
    /// Throw away the capture written so far and the partly-framed line, so
    /// clearing the console clears the log on disk too.
    ClearLog,
    Shutdown,
}

/// What to read from, and whether to reconnect.
pub enum SourceSpec {
    /// A live serial device, resolved by identity (reconnectable).
    Serial {
        identity: PortIdentity,
        config: PortConfig,
        /// Path the user selected for the first open (disambiguates identical
        /// no-serial devices). Reconnects resolve by identity.
        initial_path: Option<String>,
    },
    /// A prebuilt one-shot source (e.g. a scripted test fixture); no reconnect.
    OneShot(Box<dyn ByteSource>),
}

/// Configuration for spawning a reader.
pub struct ReaderConfig {
    pub port_id: PortId,
    pub clock: SessionClock,
    /// Directory for the session log, or `None` to skip logging (tests).
    pub session_dir: Option<PathBuf>,
    pub meta: SessionMeta,
    /// How incoming carriage returns are framed into lines.
    pub terminal: crate::config::TerminalMode,
    /// Signalled after every event, so a sleeping UI comes back to drain the
    /// channel. Without it the events sit there unseen (spec §5, rule 4).
    pub wake: Wake,
}

/// Handle to a running reader thread.
pub struct ReaderHandle {
    pub port_id: PortId,
    pub events: Receiver<ReaderEvent>,
    cmd: Sender<ReaderCommand>,
    join: Option<JoinHandle<()>>,
}

impl ReaderHandle {
    pub fn transmit(&self, bytes: Vec<u8>) {
        let _ = self.cmd.send(ReaderCommand::Transmit(bytes));
    }
    pub fn set_dtr(&self, on: bool) {
        let _ = self.cmd.send(ReaderCommand::SetDtr(on));
    }
    pub fn set_rts(&self, on: bool) {
        let _ = self.cmd.send(ReaderCommand::SetRts(on));
    }
    pub fn send_break(&self) {
        let _ = self.cmd.send(ReaderCommand::SendBreak);
    }
    /// Truncate this port's session capture back to its header and drop
    /// whatever the reader still holds unsent, so output the user cleared from
    /// the console doesn't survive on disk (or come back as preloaded history
    /// next launch).
    pub fn clear_log(&self) {
        let _ = self.cmd.send(ReaderCommand::ClearLog);
    }

    /// Signal shutdown and join the thread.
    pub fn shutdown(mut self) {
        self.shutdown_in_place();
    }

    /// [`ReaderHandle::shutdown`] without consuming the handle, which is left
    /// inert: its thread is gone, so every command sent from here on is
    /// dropped. For the caller holding the handle by reference — reconnecting
    /// a tab replaces one in place, and has to close the old reader before the
    /// new one opens the same device.
    pub fn shutdown_in_place(&mut self) {
        let _ = self.cmd.send(ReaderCommand::Shutdown);
        let Some(join) = self.join.take() else {
            return;
        };
        // Drain while it winds down. The reader *blocks* to send state and
        // error events (they are rare, and must not be dropped), and we are
        // the only one who empties that channel — so joining without draining
        // deadlocks the two of us, with the UI thread the one held. The events
        // are of no further use: this connection is going away.
        while !join.is_finished() {
            while self.events.try_recv().is_ok() {}
            // The reader can be inside a blocking read for as long as the
            // port's read timeout, so this waits rather than spins.
            std::thread::sleep(Duration::from_millis(1));
        }
        while self.events.try_recv().is_ok() {}
        let _ = join.join();
    }
}

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.shutdown_in_place();
        }
    }
}

/// Spawn a reader thread for the given source. Fails only if the OS refuses to
/// create the thread (e.g. resource exhaustion), which callers should surface
/// as a "couldn't open port" error rather than letting it crash the app.
pub fn spawn(config: ReaderConfig, spec: SourceSpec) -> std::io::Result<ReaderHandle> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(CHANNEL_CAPACITY);
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let port_id = config.port_id;
    let name = format!("reader-{}", port_id.0);
    let join = std::thread::Builder::new()
        .name(name)
        .spawn(move || run(config, spec, event_tx, cmd_rx))?;
    Ok(ReaderHandle {
        port_id,
        events: event_rx,
        cmd: cmd_tx,
        join: Some(join),
    })
}

/// A factory that (re)opens the underlying source.
type Opener = Box<dyn FnMut() -> Result<Box<dyn ByteSource>, SourceError> + Send>;

/// The event channel paired with the UI wake signal.
///
/// Every send goes through here so that "the UI finds out" is structural rather
/// than something each of the dozen send sites has to remember: one missed wake
/// means that event sits in the channel until some unrelated input happens to
/// redraw the app.
struct EventTx {
    tx: Sender<ReaderEvent>,
    wake: Wake,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DroppedOutput {
    raw_bytes: u64,
    line_updates: u64,
    at: Option<Timestamp>,
}

struct QueuedBatch {
    batch: Batch,
    /// Earliest stream timestamp this batch can affect. This can precede every
    /// line it currently contains when its raw bytes continue an open line.
    order_at: Timestamp,
}

/// Batches waiting for room on the UI channel, with an explicit heap budget.
///
/// Accounting capacities rather than lengths tracks the allocations the queue
/// actually keeps alive. `VecDeque`'s own slots are included per resident
/// `Batch`; its small spare-capacity slack remains bounded by the largest queue
/// it has held, which is itself bounded here.
#[derive(Default)]
struct Backlog {
    batches: VecDeque<QueuedBatch>,
    allocated_bytes: usize,
    dropped: DroppedOutput,
}

impl Backlog {
    fn batch_bytes(batch: &Batch) -> usize {
        std::mem::size_of::<QueuedBatch>()
            .saturating_add(batch.raw.capacity())
            .saturating_add(
                batch
                    .lines
                    .capacity()
                    .saturating_mul(std::mem::size_of::<FramedLine>()),
            )
            .saturating_add(
                batch
                    .lines
                    .iter()
                    .map(|line| line.text.capacity())
                    .fold(0usize, usize::saturating_add),
            )
    }

    fn push_unbounded(&mut self, batch: Batch, order_at: Timestamp) {
        self.allocated_bytes = self
            .allocated_bytes
            .saturating_add(Self::batch_bytes(&batch));
        self.batches.push_back(QueuedBatch { batch, order_at });
    }

    fn enforce_limit(&mut self, limit: usize) {
        while self.allocated_bytes > limit {
            let Some(queued) = self.pop_front() else {
                break;
            };
            self.dropped.raw_bytes = self
                .dropped
                .raw_bytes
                .saturating_add(queued.batch.raw.len() as u64);
            self.dropped.line_updates = self
                .dropped
                .line_updates
                .saturating_add(queued.batch.lines.len() as u64);
            self.dropped.at.get_or_insert(queued.order_at);
        }
    }

    #[cfg(test)]
    fn push_with_limit(&mut self, batch: Batch, at: Timestamp, limit: usize) {
        self.push_unbounded(batch, at);
        self.enforce_limit(limit);
    }

    fn pop_front(&mut self) -> Option<QueuedBatch> {
        let queued = self.batches.pop_front()?;
        self.allocated_bytes = self
            .allocated_bytes
            .saturating_sub(Self::batch_bytes(&queued.batch));
        Some(queued)
    }

    fn push_front(&mut self, queued: QueuedBatch) {
        self.allocated_bytes = self
            .allocated_bytes
            .saturating_add(Self::batch_bytes(&queued.batch));
        self.batches.push_front(queued);
    }

    fn clear(&mut self) {
        self.batches.clear();
        self.allocated_bytes = 0;
        self.dropped = DroppedOutput::default();
    }
}

impl EventTx {
    /// Send, blocking if the channel is full. Used for state and error events,
    /// which are rare and must not be dropped.
    fn send(&self, ev: ReaderEvent) {
        if self.tx.send(ev).is_ok() {
            self.wake.signal();
        }
    }

    /// Send without blocking. Used for batches, which must never stall the read
    /// loop (rule 2); a full channel leaves them in the backlog to retry.
    fn try_send(&self, ev: ReaderEvent) -> Result<(), TrySendError<ReaderEvent>> {
        let result = self.tx.try_send(ev);
        if result.is_ok() {
            self.wake.signal();
        }
        result
    }
}

fn run(
    config: ReaderConfig,
    spec: SourceSpec,
    event_tx: Sender<ReaderEvent>,
    cmd_rx: Receiver<ReaderCommand>,
) {
    let clock = config.clock.clone();
    let event_tx = EventTx {
        tx: event_tx,
        wake: config.wake.clone(),
    };

    // Build the opener and whether we reconnect.
    let (mut opener, reconnect): (Opener, bool) = match spec {
        SourceSpec::Serial {
            identity,
            config: pcfg,
            initial_path,
        } => {
            let mut first = true;
            let opener: Opener = Box::new(move || {
                let path = resolve_path(&identity, &pcfg, &mut first, &initial_path)?;
                Ok(Box::new(SerialSource::open(&path, &pcfg)?) as Box<dyn ByteSource>)
            });
            (opener, true)
        }
        SourceSpec::OneShot(src) => {
            let mut slot = Some(src);
            let opener: Opener = Box::new(move || {
                slot.take()
                    .ok_or_else(|| SourceError::Disconnected("source exhausted".into()))
            });
            (opener, false)
        }
    };

    // Session writer: created once, survives reconnects (spec §7.6).
    let mut writer: Option<SessionWriter> = match &config.session_dir {
        Some(dir) => match SessionWriter::create(dir, &config.meta) {
            Ok(w) => Some(w),
            Err(e) => {
                event_tx.send(ReaderEvent::session_error(format!("session log: {e}")));
                None
            }
        },
        None => None,
    };

    let mut framer = Framer::with_mode(config.terminal);
    let mut backlog = Backlog::default();
    // Lives outside the connect loop only so a `ClearLog` arriving during a
    // reconnect backoff can empty it too; it is always drained before a
    // disconnect, so each connection still starts with an empty batch.
    let mut pending = Batch::default();
    let mut backoff = Duration::from_millis(100);
    let mut lost_at: Option<Instant> = None;
    let mut first_connect = true;

    'outer: loop {
        // (Re)connect phase.
        let state = if first_connect {
            ConnState::Connecting
        } else {
            ConnState::Reconnecting
        };
        event_tx.send(ReaderEvent::State(state));

        // Reported once per distinct message within a (re)connect phase: an
        // open failure like permission denied won't clear itself between
        // backoff retries, so repeating it every attempt would just spam
        // identical messages. But the underlying cause can change mid-phase
        // (e.g. "device not present" while unplugged, then "permission
        // denied" once it reappears with the wrong group), so re-report
        // whenever the message itself changes.
        let mut last_reported: Option<String> = None;
        let mut source = loop {
            match opener() {
                Ok(s) => break s,
                Err(e) => {
                    if !reconnect {
                        event_tx.send(ReaderEvent::connection_error(e.to_string()));
                        event_tx.send(ReaderEvent::State(ConnState::Closed));
                        return;
                    }
                    let msg = e.to_string();
                    if last_reported.as_deref() != Some(msg.as_str()) {
                        event_tx.send(ReaderEvent::connection_error(msg.clone()));
                        last_reported = Some(msg);
                    }
                    // Wait out the backoff while remaining responsive to Shutdown.
                    let targets = ClearTargets {
                        writer: &mut writer,
                        framer: &mut framer,
                        pending: &mut pending,
                        backlog: &mut backlog,
                    };
                    if wait_or_shutdown(&cmd_rx, backoff, targets, &event_tx) {
                        event_tx.send(ReaderEvent::State(ConnState::Closed));
                        return;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
        };

        // Connected. Emit a reconnect marker if this was a reconnect.
        if let Some(lost) = lost_at.take() {
            let outage = lost.elapsed();
            let marker = reconnect_marker(&clock, outage);
            let order_at = marker.ts;
            backlog.push_unbounded(
                Batch {
                    lines: vec![marker],
                    raw: Vec::new(),
                },
                order_at,
            );
        }
        backoff = Duration::from_millis(100);
        first_connect = false;
        event_tx.send(ReaderEvent::State(ConnState::Connected));

        // Read loop.
        let mut buf = vec![0u8; READ_BUF];
        let mut last_send = Instant::now();
        let mut last_byte = Instant::now();
        let mut provisional_flushed = false;

        loop {
            // Handle any queued commands.
            let targets = ClearTargets {
                writer: &mut writer,
                framer: &mut framer,
                pending: &mut pending,
                backlog: &mut backlog,
            };
            match drain_commands(&cmd_rx, source.as_mut(), targets, &event_tx) {
                CommandOutcome::Shutdown => {
                    // Flush and exit.
                    framer.flush_final(&mut pending.lines);
                    flush_batch(&mut pending, &mut backlog, &event_tx, &framer, clock.now());
                    drain_backlog(&mut backlog, &event_tx);
                    if let Some(w) = &mut writer {
                        let _ = w.flush();
                    }
                    event_tx.send(ReaderEvent::State(ConnState::Closed));
                    break 'outer;
                }
                CommandOutcome::Continue => {}
            }

            match source.read(&mut buf) {
                Ok(0) => {
                    // Timeout: consider a provisional flush after silence.
                    if !provisional_flushed && last_byte.elapsed() >= PROVISIONAL_AFTER {
                        if let Some(line) = framer.flush_provisional() {
                            pending.lines.push(line);
                        }
                        provisional_flushed = true;
                    }
                }
                Ok(n) => {
                    let ts = clock.now();
                    last_byte = Instant::now();
                    provisional_flushed = false;
                    if let Some(w) = &mut writer {
                        // A capture only ever records bytes this run read, so
                        // its offsets are never the negative side of the axis
                        // (which belongs to *restored* history); the file
                        // format keeps them unsigned.
                        if let Err(e) = w.write_record(ts.micros.max(0) as u64, &buf[..n]) {
                            event_tx.send(ReaderEvent::session_error(format!("log write: {e}")));
                        }
                    }
                    pending.raw.extend_from_slice(&buf[..n]);
                    framer.push(&buf[..n], ts, &mut pending.lines);
                }
                Err(SourceError::Disconnected(msg)) => {
                    event_tx.send(ReaderEvent::connection_error(msg));
                    break;
                }
                Err(e) => {
                    // Treat any read error as a loss; reconnect will retry.
                    event_tx.send(ReaderEvent::connection_error(e.to_string()));
                    break;
                }
            }

            // Batch flush (rule 1).
            let full = pending.lines.len() >= BATCH_MAX_LINES;
            if (!pending.lines.is_empty() || !pending.raw.is_empty())
                && (full || last_send.elapsed() >= BATCH_INTERVAL)
            {
                flush_batch(&mut pending, &mut backlog, &event_tx, &framer, clock.now());
                last_send = Instant::now();
            }
            // Try to drain any backlog that built up while the channel was full.
            drain_backlog(&mut backlog, &event_tx);
        }

        // Left the read loop due to disconnect. The line being framed will never
        // receive its terminator, so close it here rather than carrying it into
        // the next connection: its provisional form is already on screen wearing
        // a caret, and the reconnect marker would otherwise land *below* a line
        // still presenting itself as the live one.
        framer.flush_final(&mut pending.lines);
        flush_batch(&mut pending, &mut backlog, &event_tx, &framer, clock.now());
        drain_backlog(&mut backlog, &event_tx);
        if let Some(w) = &mut writer {
            let _ = w.flush();
        }

        if !reconnect {
            event_tx.send(ReaderEvent::State(ConnState::Closed));
            break 'outer;
        }

        // Enter Lost/Reconnecting.
        lost_at = Some(Instant::now());
        event_tx.send(ReaderEvent::State(ConnState::Lost));
    }
}

/// Resolve the OS path for a serial identity. On the first open we honour a
/// user-selected path (to disambiguate identical no-serial devices); afterwards
/// we resolve strictly by identity so a re-enumerated device is found on its new
/// path (spec §7.6).
fn resolve_path(
    identity: &PortIdentity,
    _config: &PortConfig,
    first: &mut bool,
    initial_path: &Option<String>,
) -> Result<String, SourceError> {
    let discovered = enumerate_ports();

    if *first {
        if let Some(path) = initial_path {
            if discovered.iter().any(|d| &d.path == path) {
                *first = false;
                return Ok(path.clone());
            }
        }
    }
    *first = false;

    match match_identity(identity, &discovered) {
        MatchResult::Definite(i) => Ok(discovered[i].path.clone()),
        MatchResult::Ambiguous(_) => Err(SourceError::Open(
            "multiple identical devices present; cannot disambiguate".into(),
        )),
        MatchResult::None => {
            if !identity.has_usb() && !identity.path_fallback.is_empty() {
                Ok(identity.path_fallback.clone())
            } else {
                Err(SourceError::Disconnected("device not present".into()))
            }
        }
    }
}

enum CommandOutcome {
    Continue,
    Shutdown,
}

/// State a `ClearLog` command has to reach into: everything holding bytes that
/// the user just deleted from the console.
struct ClearTargets<'a> {
    writer: &'a mut Option<SessionWriter>,
    framer: &'a mut Framer,
    pending: &'a mut Batch,
    backlog: &'a mut Backlog,
}

/// Discard the capture written so far plus everything still queued here. Data
/// already handed to the channel is the UI's, and is dropped on its side.
fn clear_log(targets: ClearTargets<'_>, event_tx: &EventTx) {
    targets.pending.lines.clear();
    targets.pending.raw.clear();
    targets.backlog.clear();
    targets.framer.reset();
    if let Some(w) = targets.writer {
        if let Err(e) = w.truncate() {
            event_tx.send(ReaderEvent::session_error(format!("clear log: {e}")));
        }
    }
}

fn drain_commands(
    cmd_rx: &Receiver<ReaderCommand>,
    source: &mut dyn ByteSource,
    targets: ClearTargets<'_>,
    event_tx: &EventTx,
) -> CommandOutcome {
    let ClearTargets {
        writer,
        framer,
        pending,
        backlog,
    } = targets;
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            ReaderCommand::Shutdown => return CommandOutcome::Shutdown,
            // Reborrowed rather than moved: another command may follow it.
            ReaderCommand::ClearLog => clear_log(
                ClearTargets {
                    writer: &mut *writer,
                    framer: &mut *framer,
                    pending: &mut *pending,
                    backlog: &mut *backlog,
                },
                event_tx,
            ),
            ReaderCommand::Transmit(bytes) => {
                if let Err(e) = source.write(&bytes) {
                    event_tx.send(ReaderEvent::session_error(format!("transmit: {e}")));
                }
            }
            ReaderCommand::SetDtr(on) => {
                if let Err(e) = source.set_dtr(on) {
                    event_tx.send(ReaderEvent::session_error(format!("dtr: {e}")));
                }
            }
            ReaderCommand::SetRts(on) => {
                if let Err(e) = source.set_rts(on) {
                    event_tx.send(ReaderEvent::session_error(format!("rts: {e}")));
                }
            }
            ReaderCommand::SendBreak => {
                if let Err(e) = source.send_break() {
                    event_tx.send(ReaderEvent::session_error(format!("break: {e}")));
                }
            }
        }
    }
    CommandOutcome::Continue
}

/// Move `pending` into the backlog, then try to push backlog entries onto the
/// channel. If the channel is full we retain them up to the backlog budget and
/// keep reading (rule 2: never block on the UI).
fn flush_batch(
    pending: &mut Batch,
    backlog: &mut Backlog,
    event_tx: &EventTx,
    framer: &Framer,
    at: Timestamp,
) {
    if pending.lines.is_empty() && pending.raw.is_empty() {
        return;
    }
    let order_at = pending
        .lines
        .iter()
        .map(|line| line.ts)
        .chain(framer.open_line_timestamp())
        .min_by_key(|ts| ts.micros)
        .unwrap_or(at);
    enqueue_batch(std::mem::take(pending), backlog, event_tx, order_at);
}

/// Give queued batches every currently available channel slot before enforcing
/// the local memory budget. This matters at the boundary: if the UI just made
/// room, dropping an old batch before retrying the channel would manufacture a
/// gap even though the output could have been delivered.
fn enqueue_batch(batch: Batch, backlog: &mut Backlog, event_tx: &EventTx, order_at: Timestamp) {
    enqueue_batch_with_limit(batch, backlog, event_tx, order_at, MAX_BACKLOG_BYTES);
}

fn enqueue_batch_with_limit(
    batch: Batch,
    backlog: &mut Backlog,
    event_tx: &EventTx,
    order_at: Timestamp,
    limit: usize,
) {
    drain_backlog(backlog, event_tx);
    backlog.push_unbounded(batch, order_at);
    drain_backlog(backlog, event_tx);
    backlog.enforce_limit(limit);
    drain_backlog(backlog, event_tx);
}

/// Hand as much of the backlog to the UI as the channel will take.
///
/// Every byte the reader produces passes through here, so a batch is *moved*
/// onto the channel rather than copied onto it: a `Batch` owns up to a whole
/// `READ_BUF` of raw bytes plus a `String` per line, and cloning one to satisfy
/// a borrow — then dropping the original — was a full memcpy and a fresh
/// allocation per line on the hot path, for nothing. `TrySendError::Full` hands
/// the value back, which is what makes the move safe: a batch the channel
/// refuses goes back on the front, in order, and is retried later.
///
/// The front is also where entries are taken from, hence `VecDeque`: draining
/// *n* batches out of a `Vec` with `remove(0)` is O(n²), and the moment that
/// matters is when the reader is already behind.
fn drain_backlog(backlog: &mut Backlog, event_tx: &EventTx) {
    if let Some(at) = backlog.dropped.at {
        let ev = ReaderEvent::OutputDropped {
            raw_bytes: backlog.dropped.raw_bytes,
            line_updates: backlog.dropped.line_updates,
            at,
        };
        match event_tx.try_send(ev) {
            Ok(()) => backlog.dropped = DroppedOutput::default(),
            Err(TrySendError::Full(_)) => return,
            Err(TrySendError::Disconnected(_)) => {
                backlog.clear();
                return;
            }
        }
    }

    while let Some(queued) = backlog.pop_front() {
        let QueuedBatch { batch, order_at } = queued;
        match event_tx.try_send(ReaderEvent::Batch(batch)) {
            Ok(()) => {}
            // Keep accumulating; retry later. Order is preserved: this one goes
            // back where it came from, ahead of everything queued behind it.
            Err(TrySendError::Full(ev)) => {
                if let ReaderEvent::Batch(batch) = ev {
                    backlog.push_front(QueuedBatch { batch, order_at });
                }
                break;
            }
            Err(TrySendError::Disconnected(_)) => {
                backlog.clear();
                break;
            }
        }
    }
}

/// Sleep for `dur`, returning `true` if a Shutdown arrived meanwhile.
fn wait_or_shutdown(
    cmd_rx: &Receiver<ReaderCommand>,
    dur: Duration,
    targets: ClearTargets<'_>,
    event_tx: &EventTx,
) -> bool {
    match cmd_rx.recv_timeout(dur) {
        Ok(ReaderCommand::Shutdown) => true,
        // Handled even while disconnected: the capture is still on disk, and a
        // console cleared during an outage must not have its history reappear
        // as preloaded output on the next launch.
        Ok(ReaderCommand::ClearLog) => {
            clear_log(targets, event_tx);
            false
        }
        // Nothing to write to while disconnected, and staying here until
        // reconnect would just delay input the user typed against a stale
        // idea of the link. Report it instead of silently eating it.
        Ok(ReaderCommand::Transmit(_)) => {
            report_dropped_command(event_tx, "transmit");
            false
        }
        Ok(ReaderCommand::SetDtr(_)) => {
            report_dropped_command(event_tx, "dtr");
            false
        }
        Ok(ReaderCommand::SetRts(_)) => {
            report_dropped_command(event_tx, "rts");
            false
        }
        Ok(ReaderCommand::SendBreak) => {
            report_dropped_command(event_tx, "break");
            false
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => false,
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => true,
    }
}

fn report_dropped_command(event_tx: &EventTx, label: &str) {
    event_tx.send(ReaderEvent::session_error(format!(
        "{label}: dropped, not connected"
    )));
}

fn reconnect_marker(clock: &SessionClock, outage: Duration) -> FramedLine {
    let ts: Timestamp = clock.now();
    FramedLine {
        text: format!("reconnected after {:.1}s", outage.as_secs_f64()),
        ts,
        flags: LineFlags::RECONNECT_MARKER,
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortConfig;
    use crate::source::ScriptedSource;

    fn test_meta() -> SessionMeta {
        SessionMeta {
            identity: PortIdentity::default(),
            config: PortConfig::default(),
            start_wall: chrono::Utc::now(),
            app_version: "test".into(),
            port_label: "test".into(),
            cleared: false,
        }
    }

    fn collect_lines(handle: &ReaderHandle, timeout: Duration) -> (Vec<String>, Vec<ConnState>) {
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        let mut states = Vec::new();
        while Instant::now() < deadline {
            match handle.events.recv_timeout(Duration::from_millis(50)) {
                Ok(ReaderEvent::Batch(b)) => {
                    lines.extend(b.lines.into_iter().map(|l| l.text));
                }
                Ok(ReaderEvent::State(s)) => {
                    states.push(s);
                    if s == ConnState::Closed {
                        break;
                    }
                }
                Ok(ReaderEvent::OutputDropped { .. }) => {}
                Ok(ReaderEvent::Error { .. }) => {}
                Err(_) => {}
            }
        }
        (lines, states)
    }

    /// The reader *blocks* to send state and error events, and the UI is the
    /// only thing that empties that channel — so a shutdown that joins the
    /// thread without draining first is two parties waiting on each other,
    /// with the UI thread the one held. That is a frozen window, and closing
    /// a tab or applying port options is where it would happen.
    #[test]
    fn shutdown_does_not_deadlock_on_a_full_event_channel() {
        let src = ScriptedSource::new(vec![(b"x\n".to_vec(), Duration::from_millis(5)); 40])
            .eof_when_done();
        let config = ReaderConfig {
            port_id: PortId(0),
            clock: SessionClock::new(),
            session_dir: None,
            meta: test_meta(),
            terminal: crate::config::TerminalMode::Classic,
            wake: Wake::none(),
        };
        let handle = spawn(config, SourceSpec::OneShot(Box::new(src))).unwrap();

        // Deliberately never drained: the channel fills, and the reader parks
        // on the next state or error it has to get out.
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            handle.events.is_full(),
            "the channel has to be full to test"
        );

        // Off-thread, so a shutdown that never returns fails the test instead
        // of hanging the suite.
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            handle.shutdown();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "shutdown never returned: it is waiting for a reader that is \
             waiting for it"
        );
    }

    #[test]
    fn oneshot_delivers_lines_and_closes() {
        let src = ScriptedSource::new(vec![
            (b"hello\nwor".to_vec(), Duration::ZERO),
            (b"ld\n".to_vec(), Duration::ZERO),
        ])
        .no_delays()
        .eof_when_done();

        let config = ReaderConfig {
            port_id: PortId(0),
            clock: SessionClock::new(),
            session_dir: None,
            meta: test_meta(),
            terminal: crate::config::TerminalMode::Classic,
            wake: Wake::none(),
        };
        let handle = spawn(config, SourceSpec::OneShot(Box::new(src))).unwrap();
        let (lines, states) = collect_lines(&handle, Duration::from_secs(2));
        assert_eq!(lines, vec!["hello", "world"]);
        assert!(states.contains(&ConnState::Connected));
        assert!(states.contains(&ConnState::Closed));
    }

    #[test]
    fn error_scope_separates_the_capture_file_from_the_link() {
        // A session log that can't be created is reported *before* the first
        // connect, so scoping it to the connection would have the UI wipe it
        // the moment the port opens — leaving a run that silently captures
        // nothing. Losing the link afterwards is what `Connection` is for.
        let not_a_dir = std::env::temp_dir().join(format!(
            "pigtail-not-a-dir-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&not_a_dir, b"x").unwrap();

        let src = ScriptedSource::new(vec![(b"hello\n".to_vec(), Duration::ZERO)])
            .no_delays()
            .eof_when_done();
        let config = ReaderConfig {
            port_id: PortId(0),
            clock: SessionClock::new(),
            session_dir: Some(not_a_dir.clone()),
            meta: test_meta(),
            terminal: crate::config::TerminalMode::Classic,
            wake: Wake::none(),
        };
        let handle = spawn(config, SourceSpec::OneShot(Box::new(src))).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut scopes = Vec::new();
        while Instant::now() < deadline {
            match handle.events.recv_timeout(Duration::from_millis(50)) {
                Ok(ReaderEvent::Error { scope, .. }) => scopes.push(scope),
                Ok(ReaderEvent::State(ConnState::Closed)) => break,
                Ok(_) => {}
                Err(_) => {}
            }
        }
        std::fs::remove_file(&not_a_dir).ok();

        assert_eq!(scopes, vec![ErrorScope::Session, ErrorScope::Connection]);
    }

    #[test]
    fn losing_the_connection_closes_the_open_line() {
        // A prompt with no terminator, then the device goes away. The line can
        // never be completed by the device, so the reader must close it itself:
        // left open it would keep its PROVISIONAL flag and its caret, which the
        // console draws — a live-looking cursor on a dead connection, sitting
        // above the "reconnected" marker once the device comes back.
        let src = ScriptedSource::new(vec![(b"usr:~$ ".to_vec(), Duration::ZERO)])
            .no_delays()
            .eof_when_done();
        let config = ReaderConfig {
            port_id: PortId(0),
            clock: SessionClock::new(),
            session_dir: None,
            meta: test_meta(),
            terminal: crate::config::TerminalMode::Vt100,
            wake: Wake::none(),
        };
        let handle = spawn(config, SourceSpec::OneShot(Box::new(src))).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut lines: Vec<FramedLine> = Vec::new();
        while Instant::now() < deadline {
            match handle.events.recv_timeout(Duration::from_millis(50)) {
                Ok(ReaderEvent::Batch(b)) => lines.extend(b.lines),
                Ok(ReaderEvent::State(ConnState::Closed)) => break,
                Ok(_) => {}
                Err(_) => {}
            }
        }
        let last = lines.last().expect("the prompt must not be lost");
        assert_eq!(last.text, "usr:~$ ");
        assert!(!last.flags.contains(LineFlags::PROVISIONAL));
        assert_eq!(last.cursor, None);
    }

    #[test]
    fn provisional_prompt_is_flushed() {
        // A prompt with no newline must appear as a provisional line.
        let src = ScriptedSource::new(vec![(b"> ".to_vec(), Duration::ZERO)]);
        // Do NOT mark eof_when_done: returns Ok(0) forever, so the provisional
        // path triggers. We shut down explicitly after collecting.
        let config = ReaderConfig {
            port_id: PortId(0),
            clock: SessionClock::new(),
            session_dir: None,
            meta: test_meta(),
            terminal: crate::config::TerminalMode::Classic,
            wake: Wake::none(),
        };
        let handle = spawn(config, SourceSpec::OneShot(Box::new(src))).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got_provisional = false;
        while Instant::now() < deadline && !got_provisional {
            if let Ok(ReaderEvent::Batch(b)) = handle.events.recv_timeout(Duration::from_millis(50))
            {
                for l in &b.lines {
                    if l.text == "> " && l.flags.contains(LineFlags::PROVISIONAL) {
                        got_provisional = true;
                    }
                }
            }
        }
        assert!(got_provisional, "expected a provisional prompt line");
        handle.shutdown();
    }

    /// A backlog is only ever built when the UI is behind, and it has to come
    /// back out in the order it went in, whole. The batches are *moved* onto
    /// the channel now rather than cloned onto it (issue #42), and a move is
    /// only safe because a refused send hands the value back — so this pins
    /// down that a batch the channel would not take is put back on the front
    /// rather than dropped or reordered.
    #[test]
    fn a_full_channel_holds_the_backlog_in_order_and_loses_nothing() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let event_tx = EventTx {
            tx,
            wake: Wake::none(),
        };
        let at = SessionClock::new().now();
        let mut backlog = Backlog::default();
        for n in 0..5 {
            backlog.push_with_limit(
                Batch {
                    lines: Vec::new(),
                    raw: vec![n as u8],
                },
                at,
                usize::MAX,
            );
        }

        // Only two fit.
        drain_backlog(&mut backlog, &event_tx);
        assert_eq!(backlog.batches.len(), 3, "the rest stays put");
        assert_eq!(
            backlog.batches.front().map(|b| b.batch.raw.clone()),
            Some(vec![2]),
            "and the one the channel refused is back at the front, not dropped"
        );

        // The UI drains; the rest follows, still in order.
        let mut seen = Vec::new();
        while let Ok(ReaderEvent::Batch(b)) = rx.try_recv() {
            seen.push(b.raw[0]);
        }
        drain_backlog(&mut backlog, &event_tx);
        while let Ok(ReaderEvent::Batch(b)) = rx.try_recv() {
            seen.push(b.raw[0]);
        }
        drain_backlog(&mut backlog, &event_tx);
        while let Ok(ReaderEvent::Batch(b)) = rx.try_recv() {
            seen.push(b.raw[0]);
        }
        assert!(backlog.batches.is_empty());
        assert!(backlog.dropped.at.is_none());
        assert_eq!(seen, vec![0, 1, 2, 3, 4], "every batch, once, in order");
    }

    /// A receiver that has gone away takes the backlog with it rather than
    /// leaving the reader retrying a send that can never land.
    #[test]
    fn a_disconnected_channel_clears_the_backlog() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        drop(rx);
        let event_tx = EventTx {
            tx,
            wake: Wake::none(),
        };
        let at = SessionClock::new().now();
        let mut backlog = Backlog::default();
        for n in 0..3 {
            backlog.push_with_limit(
                Batch {
                    lines: Vec::new(),
                    raw: vec![n as u8],
                },
                at,
                usize::MAX,
            );
        }
        drain_backlog(&mut backlog, &event_tx);
        assert!(backlog.batches.is_empty());
        assert!(backlog.dropped.at.is_none());
    }

    /// The remaining half of issue #42: once the UI channel is full, retained
    /// batches have a hard heap budget. Oldest output is discarded first and
    /// counted, while newer output stays in order.
    #[test]
    fn backlog_drops_oldest_batches_at_its_memory_limit() {
        let at = SessionClock::new().now();
        let later = Timestamp {
            wall: at.wall + chrono::Duration::microseconds(1),
            micros: at.micros + 1,
        };
        let make_batch = |n| Batch {
            lines: Vec::new(),
            raw: vec![n; 32],
        };
        let one_batch = Backlog::batch_bytes(&make_batch(0));
        let limit = one_batch * 2;
        let mut backlog = Backlog::default();

        backlog.push_with_limit(make_batch(0), at, limit);
        backlog.push_with_limit(make_batch(1), later, limit);
        backlog.push_with_limit(make_batch(2), later, limit);

        assert!(backlog.allocated_bytes <= limit);
        assert_eq!(backlog.batches.len(), 2);
        assert_eq!(backlog.batches[0].batch.raw[0], 1, "the oldest was dropped");
        assert_eq!(backlog.batches[1].batch.raw[0], 2);
        assert_eq!(backlog.dropped.raw_bytes, 32);
        assert_eq!(backlog.dropped.line_updates, 0);
        assert_eq!(backlog.dropped.at, Some(at));
    }

    /// Raw bytes in a later batch can continue a line whose timestamp belongs
    /// to an earlier batch. If this batch is eventually dropped, its marker
    /// needs that open-line timestamp so the merged view cannot sort a retained
    /// completion ahead of the gap.
    #[test]
    fn dropped_partial_batch_keeps_the_open_lines_ordering_timestamp() {
        let clock = SessionClock::new();
        let line_at = clock.now();
        let flush_at = Timestamp {
            wall: line_at.wall + chrono::Duration::seconds(1),
            micros: line_at.micros + 1_000_000,
        };
        let mut framer = Framer::new();
        let mut pending = Batch {
            lines: Vec::new(),
            raw: b"partial".to_vec(),
        };
        framer.push(&pending.raw, line_at, &mut pending.lines);

        let (tx, _rx) = crossbeam_channel::bounded(0);
        let event_tx = EventTx {
            tx,
            wake: Wake::none(),
        };
        let mut backlog = Backlog::default();
        flush_batch(&mut pending, &mut backlog, &event_tx, &framer, flush_at);

        assert_eq!(backlog.batches.len(), 1);
        assert_eq!(backlog.batches[0].order_at, line_at);
        backlog.enforce_limit(0);
        assert!(backlog.batches.is_empty());
        assert_eq!(backlog.dropped.at, Some(line_at));
    }

    /// Catch-up and eviction can race at the budget boundary. If the UI has
    /// made room, queued output must use it before the reader decides anything
    /// needs to be discarded.
    #[test]
    fn available_channel_room_is_used_before_backlog_eviction() {
        let at = SessionClock::new().now();
        let make_batch = |n| Batch {
            lines: Vec::new(),
            raw: vec![n; 32],
        };
        let limit = Backlog::batch_bytes(&make_batch(0));
        let mut backlog = Backlog::default();
        backlog.push_with_limit(make_batch(0), at, limit);

        let (tx, rx) = crossbeam_channel::bounded(2);
        let event_tx = EventTx {
            tx,
            wake: Wake::none(),
        };
        enqueue_batch_with_limit(make_batch(1), &mut backlog, &event_tx, at, limit);

        let seen: Vec<u8> = rx
            .try_iter()
            .map(|ev| match ev {
                ReaderEvent::Batch(batch) => batch.raw[0],
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(seen, vec![0, 1]);
        assert!(backlog.batches.is_empty());
        assert!(backlog.dropped.at.is_none());
    }

    /// The gap notice must precede every retained batch and survive a full
    /// channel itself; otherwise the UI either hides the loss or draws the
    /// marker after the data it applies to.
    #[test]
    fn dropped_output_notice_is_sent_before_retained_batches() {
        let at = SessionClock::new().now();
        let make_batch = |n| Batch {
            lines: Vec::new(),
            raw: vec![n; 32],
        };
        let limit = Backlog::batch_bytes(&make_batch(0));
        let mut backlog = Backlog::default();
        backlog.push_with_limit(make_batch(0), at, limit);
        backlog.push_with_limit(make_batch(1), at, limit);

        let (tx, rx) = crossbeam_channel::bounded(1);
        let event_tx = EventTx {
            tx,
            wake: Wake::none(),
        };
        drain_backlog(&mut backlog, &event_tx);

        match rx.recv().unwrap() {
            ReaderEvent::OutputDropped {
                raw_bytes,
                line_updates,
                at: event_at,
            } => {
                assert_eq!(raw_bytes, 32);
                assert_eq!(line_updates, 0);
                assert_eq!(event_at, at);
            }
            other => panic!("expected the gap notice first, got {other:?}"),
        }
        assert_eq!(backlog.batches.len(), 1, "the refused batch stays queued");

        drain_backlog(&mut backlog, &event_tx);
        match rx.recv().unwrap() {
            ReaderEvent::Batch(batch) => assert_eq!(batch.raw, vec![1; 32]),
            other => panic!("expected the retained batch, got {other:?}"),
        }
        assert!(backlog.batches.is_empty());
        assert!(backlog.dropped.at.is_none());
    }

    /// Regression test for #6: a `Transmit`/`SetDtr`/`SetRts`/`SendBreak`
    /// arriving during reconnect backoff has nowhere to go (there is no open
    /// source to write to), but it must not vanish without a trace — the UI
    /// needs an `Error` to tell the user their input was dropped.
    #[test]
    fn commands_dropped_while_reconnecting_are_reported() {
        for (cmd, expected_label) in [
            (ReaderCommand::Transmit(b"hi".to_vec()), "transmit"),
            (ReaderCommand::SetDtr(true), "dtr"),
            (ReaderCommand::SetRts(true), "rts"),
            (ReaderCommand::SendBreak, "break"),
        ] {
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
            let (event_tx, event_rx) = crossbeam_channel::bounded(8);
            let event_tx = EventTx {
                tx: event_tx,
                wake: Wake::none(),
            };
            let mut writer: Option<SessionWriter> = None;
            let mut framer = Framer::with_mode(crate::config::TerminalMode::Classic);
            let mut pending = Batch::default();
            let mut backlog = Backlog::default();

            cmd_tx.send(cmd).unwrap();
            let targets = ClearTargets {
                writer: &mut writer,
                framer: &mut framer,
                pending: &mut pending,
                backlog: &mut backlog,
            };
            let shutdown = wait_or_shutdown(&cmd_rx, Duration::from_millis(10), targets, &event_tx);
            assert!(!shutdown);

            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ReaderEvent::Error {
                    scope: ErrorScope::Session,
                    msg,
                }) => assert!(msg.contains(expected_label), "unexpected message: {msg}"),
                other => panic!("expected a session error, got {other:?}"),
            }
        }
    }
}
