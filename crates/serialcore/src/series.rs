//! Numeric series storage: one ring buffer per series, with min/max decimation
//! for rendering (spec §7.13).
//!
//! Each point carries the originating line index so a plot point can be linked
//! back to the log line that produced it — the whole point of the feature. That
//! third field must never be dropped as an optimisation.

use std::collections::VecDeque;

/// Default per-series capacity (points).
pub const DEFAULT_CAPACITY: usize = 100_000;

/// One sample: seconds since session start, value, and originating line index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeriesPoint {
    pub t: f64,
    pub value: f64,
    pub line: u64,
}

/// A single numeric series backed by a fixed-capacity ring buffer.
pub struct Series {
    name: String,
    buf: VecDeque<SeriesPoint>,
    capacity: usize,
}

impl Series {
    pub fn new(name: impl Into<String>, capacity: usize) -> Series {
        Series {
            name: name.into(),
            buf: VecDeque::new(),
            capacity: capacity.max(2),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn last(&self) -> Option<SeriesPoint> {
        self.buf.back().copied()
    }

    /// Push a sample, evicting the oldest when at capacity.
    pub fn push(&mut self, t: f64, value: f64, line: u64) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(SeriesPoint { t, value, line });
    }

    /// The full value extent `(min, max)`, ignoring points that are not finite.
    ///
    /// A device is free to print `nan` or `inf`, and `parse::<f64>` takes both;
    /// letting either into a min/max would poison the window computed from it —
    /// NaN loses every comparison, so a single one would swallow the range.
    pub fn value_range(&self) -> Option<(f64, f64)> {
        let mut range: Option<(f64, f64)> = None;
        for p in self.buf.iter().filter(|p| p.value.is_finite()) {
            range = Some(match range {
                Some((lo, hi)) => (lo.min(p.value), hi.max(p.value)),
                None => (p.value, p.value),
            });
        }
        range
    }

    /// The full time extent `(min_t, max_t)`, if any points exist.
    pub fn t_range(&self) -> Option<(f64, f64)> {
        match (self.buf.front(), self.buf.back()) {
            (Some(a), Some(b)) => Some((a.t, b.t)),
            _ => None,
        }
    }

    /// All points in `[x0, x1]` as `[t, value]`, without decimation.
    fn raw_in_range(&self, x0: f64, x1: f64) -> Vec<[f64; 2]> {
        self.buf
            .iter()
            .filter(|p| p.t >= x0 && p.t <= x1)
            .map(|p| [p.t, p.value])
            .collect()
    }

    /// Find the point nearest to `t` within the window and return its line index
    /// (for plot→log linking). Returns `None` if empty.
    pub fn nearest_line(&self, t: f64) -> Option<u64> {
        self.buf
            .iter()
            .min_by(|a, b| {
                (a.t - t)
                    .abs()
                    .partial_cmp(&(b.t - t).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|p| p.line)
    }

    /// The point nearest to time `t` (for plot→log linking).
    pub fn nearest_point(&self, t: f64) -> Option<SeriesPoint> {
        self.buf
            .iter()
            .min_by(|a, b| {
                (a.t - t)
                    .abs()
                    .partial_cmp(&(b.t - t).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
    }

    /// Min/max-decimated points for the window `[x0, x1]` at a target column
    /// count `width` (spec §7.13). When the window has more points than `width`,
    /// each pixel column emits both the minimum and maximum sample in that column
    /// so a transient spike is never hidden by stride sampling.
    pub fn decimate(&self, x0: f64, x1: f64, width: usize) -> Vec<[f64; 2]> {
        let width = width.max(1);
        let in_range = self.raw_in_range(x0, x1);
        if in_range.len() <= width * 2 || x1 <= x0 {
            return in_range;
        }
        let span = x1 - x0;
        let mut out: Vec<[f64; 2]> = Vec::with_capacity(width * 2);

        // Per bucket, track the min-value and max-value points.
        let mut cur_bucket: isize = -1;
        let mut min_pt = [0.0f64; 2];
        let mut max_pt = [0.0f64; 2];

        let flush = |out: &mut Vec<[f64; 2]>, min_pt: [f64; 2], max_pt: [f64; 2]| {
            // Emit in time order so the polyline is monotonic in x.
            if min_pt[0] <= max_pt[0] {
                out.push(min_pt);
                if max_pt != min_pt {
                    out.push(max_pt);
                }
            } else {
                out.push(max_pt);
                if max_pt != min_pt {
                    out.push(min_pt);
                }
            }
        };

        for p in &in_range {
            let frac = ((p[0] - x0) / span).clamp(0.0, 1.0);
            let bucket = ((frac * width as f64) as usize).min(width - 1) as isize;
            if bucket != cur_bucket {
                if cur_bucket >= 0 {
                    flush(&mut out, min_pt, max_pt);
                }
                cur_bucket = bucket;
                min_pt = *p;
                max_pt = *p;
            } else {
                if p[1] < min_pt[1] {
                    min_pt = *p;
                }
                if p[1] > max_pt[1] {
                    max_pt = *p;
                }
            }
        }
        if cur_bucket >= 0 {
            flush(&mut out, min_pt, max_pt);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_oldest() {
        let mut s = Series::new("t", 3);
        for i in 0..5 {
            s.push(i as f64, i as f64, i);
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.t_range(), Some((2.0, 4.0)));
        assert_eq!(s.last().unwrap().line, 4);
    }

    #[test]
    fn line_index_is_preserved() {
        let mut s = Series::new("t", 100);
        s.push(0.0, 10.0, 42);
        s.push(1.0, 20.0, 99);
        assert_eq!(s.nearest_line(0.9), Some(99));
        assert_eq!(s.nearest_line(0.1), Some(42));
    }

    #[test]
    fn no_decimation_when_few_points() {
        let mut s = Series::new("t", 1000);
        for i in 0..10 {
            s.push(i as f64, i as f64, i);
        }
        let pts = s.decimate(0.0, 9.0, 100);
        assert_eq!(pts.len(), 10);
    }

    /// The key property (spec §10): a single-sample spike survives decimation to
    /// any target width.
    #[test]
    fn spike_survives_decimation_any_width() {
        let mut s = Series::new("t", 1_000_000);
        let n = 10_000;
        for i in 0..n {
            // Flat baseline of 0 with one enormous spike in the middle.
            let v = if i == n / 2 { 9999.0 } else { 0.0 };
            s.push(i as f64, v, i);
        }
        for width in [1usize, 2, 7, 50, 640, 1920] {
            let pts = s.decimate(0.0, (n - 1) as f64, width);
            let max = pts.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
            assert_eq!(max, 9999.0, "spike lost at width {width}");
            // Decimation must not blow up the point count.
            assert!(
                pts.len() <= width * 2 + 2,
                "too many points at width {width}"
            );
        }
    }

    #[test]
    fn decimated_points_are_x_monotonic() {
        let mut s = Series::new("t", 1_000_000);
        for i in 0..5000 {
            let v = ((i as f64) * 0.01).sin();
            s.push(i as f64, v, i);
        }
        let pts = s.decimate(0.0, 4999.0, 100);
        for w in pts.windows(2) {
            assert!(w[0][0] <= w[1][0], "x not monotonic: {:?} {:?}", w[0], w[1]);
        }
    }
    #[test]
    fn value_range_covers_every_point_and_skips_the_unplottable() {
        let mut s = Series::new("t", 100);
        for (i, v) in [3.0, -2.0, f64::NAN, 7.5, f64::INFINITY].iter().enumerate() {
            s.push(i as f64, *v, i as u64);
        }
        assert_eq!(s.value_range(), Some((-2.0, 7.5)));
    }

    #[test]
    fn value_range_of_nothing_plottable_is_none() {
        let mut s = Series::new("t", 100);
        assert_eq!(s.value_range(), None);
        s.push(0.0, f64::NAN, 0);
        assert_eq!(s.value_range(), None);
    }
}
