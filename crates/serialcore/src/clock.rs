//! Timestamps and the app-global monotonic clock reference.
//!
//! The spec requires two clocks (§7.3): wall-clock (`DateTime<Utc>`) for display
//! and monotonic microseconds since session start for deltas and plotting.
//! Intervals are never computed from wall-clock — NTP steps would produce
//! nonsense. For the merged multi-port view (§7.12) there is a *single* clock
//! reference for the whole app, shared across readers, so microsecond stamps are
//! directly comparable between ports.

use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Instant;

/// A moment stamped on chunk arrival.
///
/// `wall` is kept in UTC everywhere it is stored or written to disk; the UI
/// converts to the local zone at the moment it formats a timestamp, so a
/// capture stays readable across a DST change or a machine in another zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Wall-clock, for display only.
    pub wall: DateTime<Utc>,
    /// Microseconds since session start, from a monotonic source. Use this for
    /// all interval and plot-axis math.
    pub micros: u64,
}

/// The shared, app-global clock. Clone is cheap (`Arc`); every reader stamps
/// against the same reference so cross-port timestamps are comparable.
#[derive(Clone)]
pub struct SessionClock {
    inner: Arc<ClockInner>,
}

struct ClockInner {
    start_instant: Instant,
    start_wall: DateTime<Utc>,
}

impl SessionClock {
    /// Start a new session clock anchored to now.
    pub fn new() -> SessionClock {
        SessionClock {
            inner: Arc::new(ClockInner {
                start_instant: Instant::now(),
                start_wall: Utc::now(),
            }),
        }
    }

    /// Stamp the current moment.
    pub fn now(&self) -> Timestamp {
        let micros = self.inner.start_instant.elapsed().as_micros() as u64;
        Timestamp {
            wall: Utc::now(),
            micros,
        }
    }

    /// Wall-clock time this session started.
    pub fn start_wall(&self) -> DateTime<Utc> {
        self.inner.start_wall
    }
}

impl Default for SessionClock {
    fn default() -> Self {
        SessionClock::new()
    }
}
