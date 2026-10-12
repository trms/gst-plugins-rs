// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Event detection for avsyncdetect.
//!
//! This has no GStreamer types so it can be unit tested directly. All times are in nanoseconds.

pub mod audio;
pub mod frames;
pub mod pairing;
pub mod stats;
pub mod video;

/// Estimates the index of an event at `time` that is known to be `residue` modulo `modulus`.
///
/// The estimate counts intervals from the `previous` event, given as its index and time, or from
/// time 0 for the first event.
pub fn estimate_index(
    previous: Option<(u64, u64)>,
    time: u64,
    interval: u64,
    residue: u64,
    modulus: u64,
) -> u64 {
    assert!(interval > 0 && modulus > 0 && residue < modulus);

    let guess = match previous {
        Some((index, previous_time)) => {
            index as f64 + (time as f64 - previous_time as f64) / interval as f64
        }
        None => time as f64 / interval as f64,
    };

    let cycles = ((guess - residue as f64) / modulus as f64).round().max(0.0) as u64;
    cycles * modulus + residue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_estimate() {
        let interval = 5_000_000_000;

        assert_eq!(estimate_index(None, 0, interval, 0, 1), 0);
        assert_eq!(estimate_index(None, 15_033_000_000, interval, 0, 1), 3);
        assert_eq!(estimate_index(None, 15_033_000_000, interval, 3, 4), 3);
        // Off by more than one interval, the residue decides
        assert_eq!(estimate_index(None, 22_000_000_000, interval, 3, 4), 3);
        assert_eq!(estimate_index(None, 22_000_000_000, interval, 1, 4), 5);
        // Counting from a previous event whose time is not index × interval
        assert_eq!(
            estimate_index(Some((7, 1_000_000_000)), 11_000_000_000, interval, 1, 4),
            9
        );
        assert_eq!(estimate_index(None, 100, interval, 2, 4), 2);
    }
}
