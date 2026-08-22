//! Timestamps and the app-global monotonic clock reference.
//!
//! The spec requires two clocks (§7.3): wall-clock (`DateTime<Utc>`) for display
//! and monotonic microseconds since session start for deltas and plotting.
//! Intervals are never computed from wall-clock — NTP steps would produce
//! nonsense. For the merged multi-port view (§7.12) there is a *single* clock
//! reference for the whole app, shared across readers, so microsecond stamps are
//! directly comparable between ports.
//!
//! That comparability is the whole point of the axis, and it has to hold for
//! restored history too: a console shows the previous session's output above
//! this one's, and every interval the UI draws — deltas, distance from a mark,
//! the merged view's ordering, the plot's x-axis — subtracts one stamp from
//! another without caring which run each came from. A line recorded before this
//! run began therefore takes a *negative* stamp, projected onto this axis
//! through the only reference two runs share, the wall clock (see
//! [`SessionClock::micros_at`]). Zero is this run's start.

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
    /// Microseconds since this run's session start, from a monotonic source.
    /// Use this for all interval and plot-axis math. Negative on a line that
    /// predates the run — restored history — which is projected onto the axis
    /// from its wall clock rather than measured on the monotonic one.
    pub micros: i64,
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
        let micros = self.inner.start_instant.elapsed().as_micros() as i64;
        Timestamp {
            wall: Utc::now(),
            micros,
        }
    }

    /// Where a wall-clock moment sits on this run's axis. For data this run
    /// recorded itself the monotonic [`now`](Self::now) is the better stamp;
    /// this is for data that arrived in an *earlier* run — a restored capture —
    /// whose only stamp is a wall clock, and which has to land on the same axis
    /// as the live output it is shown above. Such a moment is before the
    /// anchor, so the result is normally negative.
    ///
    /// Saturates rather than wrapping: a capture torn by a crash can name an
    /// offset that puts its lines hundreds of thousands of years out, which
    /// microseconds cannot hold.
    pub fn micros_at(&self, wall: DateTime<Utc>) -> i64 {
        (wall - self.inner.start_wall).num_microseconds().unwrap_or(
            if wall < self.inner.start_wall {
                i64::MIN
            } else {
                i64::MAX
            },
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_lands_before_this_run_on_the_axis() {
        let clock = SessionClock::new();
        let start = clock.start_wall();
        assert_eq!(clock.micros_at(start), 0, "the anchor is the axis' zero");
        assert_eq!(
            clock.micros_at(start - chrono::Duration::hours(30)),
            -108_000_000_000,
            "yesterday's session sits below zero, not above this run's output"
        );
        assert_eq!(
            clock.micros_at(start + chrono::Duration::milliseconds(250)),
            250_000
        );
    }

    #[test]
    fn an_absurd_moment_does_not_wrap_the_axis() {
        // A capture torn by a crash can name a start hundreds of thousands of
        // years out. chrono clamps the span it can represent, and this clamps
        // what is left of it, so such a line lands far off the axis — where it
        // belongs — rather than wrapping around it into the live output.
        let clock = SessionClock::new();
        let millennium = 1_000i64 * 365 * 86_400 * 1_000_000;
        assert!(clock.micros_at(DateTime::<Utc>::MIN_UTC) < -millennium);
        assert!(clock.micros_at(DateTime::<Utc>::MAX_UTC) > millennium);
    }
}
