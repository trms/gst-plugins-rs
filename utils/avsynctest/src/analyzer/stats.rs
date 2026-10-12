// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Statistics of measured offsets and event times.

use std::collections::VecDeque;

/// Last, average, minimum and maximum of all values so far, such as offsets or lateness.
#[derive(Clone, Debug, Default)]
pub struct OffsetStats {
    count: u64,
    sum: i128,
    last: i64,
    min: i64,
    max: i64,
}

impl OffsetStats {
    pub fn push(&mut self, offset: i64) {
        if self.count == 0 {
            self.min = offset;
            self.max = offset;
        } else {
            self.min = self.min.min(offset);
            self.max = self.max.max(offset);
        }
        self.count += 1;
        self.sum += offset as i128;
        self.last = offset;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn last(&self) -> Option<i64> {
        (self.count > 0).then_some(self.last)
    }

    pub fn average(&self) -> Option<i64> {
        (self.count > 0).then(|| (self.sum / self.count as i128) as i64)
    }

    pub fn min(&self) -> Option<i64> {
        (self.count > 0).then_some(self.min)
    }

    pub fn max(&self) -> Option<i64> {
        (self.count > 0).then_some(self.max)
    }
}

/// Least squares line through the last `window` points.
#[derive(Clone, Debug)]
pub struct LinearFit {
    window: usize,
    points: VecDeque<(f64, f64)>,
}

impl LinearFit {
    pub fn new(window: usize) -> Self {
        assert!(window >= 2);

        LinearFit {
            window,
            points: VecDeque::with_capacity(window),
        }
    }

    pub fn push(&mut self, x: f64, y: f64) {
        if self.points.len() == self.window {
            self.points.pop_front();
        }
        self.points.push_back((x, y));
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Slope of the line, if there are at least two points at different `x`.
    pub fn slope(&self) -> Option<f64> {
        if self.points.len() < 2 {
            return None;
        }

        // Relative to the means, as the times are large
        let n = self.points.len() as f64;
        let (sx, sy) = self
            .points
            .iter()
            .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
        let (mx, my) = (sx / n, sy / n);
        let (sxx, sxy) = self.points.iter().fold((0.0, 0.0), |(sxx, sxy), (x, y)| {
            (sxx + (x - mx) * (x - mx), sxy + (x - mx) * (y - my))
        });

        (sxx > 0.0).then(|| sxy / sxx)
    }
}

/// Rate of a stream's timestamps relative to the pattern, from the measured and the nominal time
/// of each event.
#[derive(Clone, Debug)]
pub struct ClockRate {
    fit: LinearFit,
}

impl ClockRate {
    pub fn new(window: usize) -> Self {
        ClockRate {
            fit: LinearFit::new(window),
        }
    }

    pub fn push(&mut self, nominal: u64, measured: u64) {
        self.fit.push(nominal as f64, measured as f64);
    }

    pub fn clear(&mut self) {
        self.fit.clear();
    }

    /// How much faster than nominal the timestamps advance, in parts per million.
    pub fn ppm(&self) -> Option<f64> {
        self.fit.slope().map(|slope| (slope - 1.0) * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        let mut stats = OffsetStats::default();
        assert_eq!(stats.average(), None);
        assert_eq!(stats.last(), None);

        for offset in [10, -30, 50, 2] {
            stats.push(offset);
        }
        assert_eq!(stats.count(), 4);
        assert_eq!(stats.last(), Some(2));
        assert_eq!(stats.average(), Some(8));
        assert_eq!(stats.min(), Some(-30));
        assert_eq!(stats.max(), Some(50));
    }

    #[test]
    fn linear_fit() {
        let mut fit = LinearFit::new(3);
        assert_eq!(fit.slope(), None);
        fit.push(1e12, 5.0);
        assert_eq!(fit.slope(), None);
        fit.push(1e12, 6.0);
        assert_eq!(fit.slope(), None);

        // Only the last 3 points count
        fit.clear();
        fit.push(0.0, 100.0);
        for n in 0..3 {
            let x = 1e13 + n as f64 * 5e9;
            fit.push(x, 2.0 + 1e-5 * (x - 1e13));
        }
        assert_eq!(fit.points.len(), 3);
        assert!((fit.slope().unwrap() - 1e-5).abs() < 1e-12);
    }

    #[test]
    fn clock_rate() {
        let mut rate = ClockRate::new(10);
        for n in 0..10u64 {
            let nominal = n * 5_000_000_000;
            rate.push(nominal, 1_000_000_000 + nominal + nominal / 10_000);
        }
        assert!((rate.ppm().unwrap() - 100.0).abs() < 1e-3);
    }
}
