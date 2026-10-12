// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Flash detection and code reading. Brightness is between 0.0 and 1.0.

use crate::pattern::{CODE_BITS, CODE_CELLS, SECOND};

/// Assumed levels before any frames are seen, so a flash on the very first frame is found.
const PRIOR_BLACK: f64 = 0.25;
const PRIOR_WHITE: f64 = 0.75;
/// Below this difference between the darkest and brightest levels nothing is detected.
const MIN_CONTRAST: f64 = 0.1;
/// Hysteresis thresholds, as fractions of the range between darkest and brightest.
const ON_THRESHOLD: f64 = 0.6;
const OFF_THRESHOLD: f64 = 0.4;
/// Time constant for the darkest and brightest levels to follow a change of the signal. The
/// brightest level drops back between flashes, a flash raises it again straight away.
const RELEASE_TIME: f64 = SECOND as f64;

/// Finds the rising edge of each flash.
///
/// The threshold sits between the darkest and brightest frames seen recently, so it adapts to
/// level changes from the pipeline under test.
#[derive(Clone, Debug)]
pub struct FlashDetector {
    black: f64,
    white: f64,
    last_time: Option<u64>,
    on: bool,
}

impl Default for FlashDetector {
    fn default() -> Self {
        FlashDetector {
            black: PRIOR_BLACK,
            white: PRIOR_WHITE,
            last_time: None,
            on: false,
        }
    }
}

impl FlashDetector {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds the brightness of the flash area of the frame at `time`. Returns `true` if a flash
    /// starts on this frame.
    pub fn push(&mut self, time: u64, brightness: f64) -> bool {
        let release = match self.last_time {
            Some(last) if time > last => 1.0 - (-((time - last) as f64) / RELEASE_TIME).exp(),
            _ => 0.0,
        };
        self.last_time = Some(time);

        if brightness < self.black {
            self.black = brightness;
        } else {
            self.black += (brightness - self.black) * release;
        }
        if brightness > self.white {
            self.white = brightness;
        } else {
            self.white -= (self.white - brightness) * release;
        }

        let contrast = self.white - self.black;
        if contrast < MIN_CONTRAST {
            self.on = false;
            return false;
        }

        let level = (brightness - self.black) / contrast;
        if !self.on && level > ON_THRESHOLD && brightness - self.black > MIN_CONTRAST {
            self.on = true;
            true
        } else {
            if self.on && level < OFF_THRESHOLD {
                self.on = false;
            }
            false
        }
    }
}

/// Reads a code row from the brightness of its [`CODE_CELLS`] cells.
///
/// The start and end cells give the white and black levels. Returns `None` if they are too close
/// to tell bits apart.
pub fn decode_row(cells: &[f64]) -> Option<u32> {
    assert_eq!(cells.len(), CODE_CELLS as usize);

    let white = cells[0];
    let black = cells[CODE_CELLS as usize - 1];
    if white - black < MIN_CONTRAST {
        return None;
    }

    let threshold = (white + black) / 2.0;
    Some(
        cells[1..=CODE_BITS as usize]
            .iter()
            .fold(0, |value, &cell| (value << 1) | (cell > threshold) as u32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{self, EventGrid};

    fn run(detector: &mut FlashDetector, frames: impl Iterator<Item = (u64, f64)>) -> Vec<u64> {
        frames
            .filter(|&(time, brightness)| detector.push(time, brightness))
            .map(|(time, _)| time)
            .collect()
    }

    #[test]
    fn flashes() {
        let grid = EventGrid::new(SECOND, 25, 1);
        let frame = |n| {
            let flash = grid.frame_is_flash(n, pattern::DEFAULT_FLASH_DURATION);
            let brightness = if flash { 235.0 / 255.0 } else { 16.0 / 255.0 };
            (grid.frame_pts(n), brightness)
        };

        let found = run(&mut FlashDetector::default(), (0..250).map(frame));
        assert_eq!(found, (0..10).map(|n| n * SECOND).collect::<Vec<_>>());
    }

    #[test]
    fn dim_flashes() {
        // A badly converted signal using a fifth of the full range. The first flash is missed
        // while adapting to it.
        let frames = (0..250u64).map(|n| (n * SECOND / 25, if n % 25 < 3 { 0.3 } else { 0.1 }));

        let found = run(&mut FlashDetector::default(), frames);
        assert_eq!(found, (1..10).map(|n| n * SECOND).collect::<Vec<_>>());
    }

    #[test]
    fn steady_picture() {
        let frames = (0..250u64).map(|n| (n * SECOND / 25, 0.5 + 0.02 * (n % 2) as f64));
        assert!(run(&mut FlashDetector::default(), frames).is_empty());
    }

    #[test]
    fn code() {
        let value = 0xdead_beef;
        let cells = (0..CODE_CELLS)
            .map(|c| {
                if pattern::cell_is_white(c, value) {
                    0.85
                } else {
                    0.1
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(decode_row(&cells), Some(value));

        assert_eq!(decode_row(&[0.5; CODE_CELLS as usize]), None);
    }
}
