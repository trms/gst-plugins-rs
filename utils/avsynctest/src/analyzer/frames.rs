// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Counting dropped and duplicated frames from the frame counter.

/// Frames lost or repeated between two frames whose counters could be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameError {
    pub dropped: u64,
    pub duplicated: u64,
}

#[derive(Clone, Debug, Default)]
pub struct FrameCounter {
    /// Counter of the last frame it could be read from.
    last: Option<u32>,
    /// Frames since then whose counter could not be read.
    unreadable: u64,
    dropped: u64,
    duplicated: u64,
}

impl FrameCounter {
    /// Counts one frame, with its counter if it could be read.
    pub fn push(&mut self, counter: Option<u32>) -> Option<FrameError> {
        let Some(counter) = counter else {
            if self.last.is_some() {
                self.unreadable += 1;
            }
            return None;
        };

        let last = self.last.replace(counter)?;
        let frames = std::mem::take(&mut self.unreadable) + 1;
        let advance = counter.wrapping_sub(last);
        if advance > u32::MAX / 2 {
            // Went backwards, start counting again from here
            return None;
        }

        let advance = advance as u64;
        let error = FrameError {
            dropped: advance.saturating_sub(frames),
            duplicated: frames.saturating_sub(advance),
        };
        self.dropped += error.dropped;
        self.duplicated += error.duplicated;

        (error != FrameError::default()).then_some(error)
    }

    /// Starts counting again from the next frame, keeping the totals.
    pub fn resync(&mut self) {
        self.last = None;
        self.unreadable = 0;
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn duplicated(&self) -> u64 {
        self.duplicated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(counter: &mut FrameCounter, frames: &[Option<u32>]) -> Vec<Option<FrameError>> {
        frames.iter().map(|&frame| counter.push(frame)).collect()
    }

    fn error(dropped: u64, duplicated: u64) -> Option<FrameError> {
        Some(FrameError {
            dropped,
            duplicated,
        })
    }

    #[test]
    fn continuous() {
        let mut counter = FrameCounter::default();
        let errors = push_all(&mut counter, &(5..100).map(Some).collect::<Vec<_>>());
        assert!(errors.iter().all(Option::is_none));
        assert_eq!((counter.dropped(), counter.duplicated()), (0, 0));
    }

    #[test]
    fn dropped_and_duplicated() {
        let mut counter = FrameCounter::default();
        let errors = push_all(
            &mut counter,
            &[
                Some(0),
                Some(1),
                Some(3),
                Some(3),
                Some(3),
                Some(4),
                Some(8),
            ],
        );
        assert_eq!(
            errors,
            [
                None,
                None,
                error(1, 0),
                error(0, 1),
                error(0, 1),
                None,
                error(3, 0)
            ]
        );
        assert_eq!((counter.dropped(), counter.duplicated()), (4, 2));
    }

    #[test]
    fn unreadable() {
        let mut counter = FrameCounter::default();
        // Unreadable frames before the first readable one don't count
        let errors = push_all(&mut counter, &[None, None, Some(10), None, None, Some(13)]);
        assert!(errors.iter().all(Option::is_none));

        // Two frames arrived for a counter advance of 4
        assert_eq!(
            push_all(&mut counter, &[None, Some(17)]),
            [None, error(2, 0)]
        );
        // Three frames for an advance of 1
        assert_eq!(
            push_all(&mut counter, &[None, None, Some(18)]),
            [None, None, error(0, 2)]
        );
    }

    #[test]
    fn wrap_and_backwards() {
        let mut counter = FrameCounter::default();
        assert_eq!(
            push_all(&mut counter, &[Some(u32::MAX - 1), Some(u32::MAX), Some(1)]),
            [None, None, error(1, 0)]
        );

        // A jump back is not counted, counting continues from there
        assert_eq!(
            push_all(&mut counter, &[Some(0), Some(1), Some(3)]),
            [None, None, error(1, 0)]
        );
        assert_eq!((counter.dropped(), counter.duplicated()), (2, 0));
    }

    #[test]
    fn resync() {
        let mut counter = FrameCounter::default();
        push_all(&mut counter, &[Some(0), Some(2)]);
        counter.resync();
        assert_eq!(
            push_all(&mut counter, &[Some(100), Some(101)]),
            [None, None]
        );
        assert_eq!(counter.dropped(), 1);
    }
}
