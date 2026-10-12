// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Timing and layout of the A/V sync test pattern.
//!
//! This is shared between the source and the detector so that both agree on where events are
//! and how they are encoded. It has no GStreamer types so it can be unit tested directly.
//!
//! All times are in nanoseconds, starting at 0 for the first frame and sample.

pub const SECOND: u64 = 1_000_000_000;

pub const DEFAULT_INTERVAL: u64 = 5 * SECOND;
pub const MIN_INTERVAL: u64 = 100_000_000;
pub const DEFAULT_FLASH_DURATION: u64 = 100_000_000;
pub const DEFAULT_FREQUENCIES: [f64; 4] = [1000.0, 1500.0, 2000.0, 2500.0];

/// Length of the raised cosine fade at the start and end of each tone.
pub const RAMP_DURATION: u64 = 1_000_000;

/// Smallest difference between two frequencies, in Hz. The detector identifies a tone from its
/// first 10 ms, which resolves frequencies about 100 Hz apart, with a margin for leakage.
pub const MIN_FREQUENCY_SPACING: f64 = 200.0;

/// Checks that frequencies are finite and can be told apart by the detector.
pub fn validate_frequencies(frequencies: &[f64]) -> Result<(), String> {
    if frequencies.is_empty() {
        return Err("frequencies must not be empty".into());
    }
    if let Some(f) = frequencies.iter().find(|f| !f.is_finite() || **f <= 0.0) {
        return Err(format!("frequency {f} Hz is not a positive finite number"));
    }

    for (n, a) in frequencies.iter().enumerate() {
        for b in &frequencies[n + 1..] {
            if (a - b).abs() < MIN_FREQUENCY_SPACING {
                return Err(format!(
                    "frequencies {a} Hz and {b} Hz are less than {MIN_FREQUENCY_SPACING} Hz apart"
                ));
            }
        }
    }

    Ok(())
}

/// Bits per code row.
pub const CODE_BITS: u32 = 32;
/// Cells per code row: a white start marker, the bits MSB first, a black end marker.
pub const CODE_CELLS: u32 = CODE_BITS + 2;
/// Row carrying the event index.
pub const EVENT_ROW: u32 = 0;
/// Row carrying the frame counter.
pub const COUNTER_ROW: u32 = 1;
/// Row carrying the clock time of the frame, all black when not encoded.
pub const CLOCK_ROW: u32 = 2;
pub const CODE_ROWS: u32 = 3;
/// Each code row is this fraction of the frame height.
const CODE_ROW_HEIGHT_DIVISOR: u32 = 12;

pub const MIN_WIDTH: u32 = CODE_CELLS * 2;
pub const MIN_HEIGHT: u32 = CODE_ROWS * CODE_ROW_HEIGHT_DIVISOR * 2;

fn mul_div_floor(a: u64, b: u64, c: u64) -> u64 {
    (a as u128 * b as u128 / c as u128) as u64
}

fn mul_div_ceil(a: u64, b: u64, c: u64) -> u64 {
    (a as u128 * b as u128).div_ceil(c as u128) as u64
}

/// Event times on the video frame grid.
///
/// Event `n` happens on the first frame at or after `n * interval`. The audio tone for that
/// event starts on the first sample at or after that frame's timestamp, so both streams agree
/// to within one sample even if the interval is not a whole number of frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventGrid {
    interval: u64,
    fps_n: u64,
    fps_d: u64,
}

impl EventGrid {
    pub fn new(interval: u64, fps_n: u32, fps_d: u32) -> Self {
        assert!(interval > 0 && fps_n > 0 && fps_d > 0);

        EventGrid {
            interval,
            fps_n: fps_n as u64,
            fps_d: fps_d as u64,
        }
    }

    pub fn interval(&self) -> u64 {
        self.interval
    }

    pub fn framerate(&self) -> (u32, u32) {
        (self.fps_n as u32, self.fps_d as u32)
    }

    pub fn frame_duration(&self) -> u64 {
        self.frame_pts(1)
    }

    pub fn frame_pts(&self, frame: u64) -> u64 {
        mul_div_floor(frame, self.fps_d * SECOND, self.fps_n)
    }

    pub fn first_frame_at_or_after(&self, time: u64) -> u64 {
        mul_div_ceil(time, self.fps_n, self.fps_d * SECOND)
    }

    pub fn event_frame(&self, event: u64) -> u64 {
        self.first_frame_at_or_after(event.saturating_mul(self.interval))
    }

    pub fn event_time(&self, event: u64) -> u64 {
        self.frame_pts(self.event_frame(event))
    }

    /// The latest event whose frame is at or before `frame`.
    pub fn event_for_frame(&self, frame: u64) -> u64 {
        self.frame_pts(frame) / self.interval
    }

    /// Whether `frame` is part of the flash of the latest event.
    pub fn frame_is_flash(&self, frame: u64, flash_duration: u64) -> bool {
        let event = self.event_for_frame(frame);
        self.frame_pts(frame) < self.event_time(event) + flash_duration
    }

    /// The tone of `event` at the given sample rate.
    pub fn tone(&self, event: u64, rate: u32, flash_duration: u64, frequencies: &[f64]) -> Tone {
        self.tone_offset(event, rate, flash_duration, frequencies, 0)
            .unwrap()
    }

    /// The tone of `event`, `offset` nanoseconds after its frame. `None` if that is before the
    /// start of the stream.
    pub fn tone_offset(
        &self,
        event: u64,
        rate: u32,
        flash_duration: u64,
        frequencies: &[f64],
        offset: i64,
    ) -> Option<Tone> {
        let time = self.event_time(event).checked_add_signed(offset)?;
        let length = duration_to_samples(flash_duration, rate).max(1);

        Some(Tone {
            start: sample_at_or_after(time, rate),
            length,
            ramp: duration_to_samples(RAMP_DURATION, rate).clamp(1, (length / 2).max(1)),
            frequency: frequency_for_event(event, frequencies),
            rate,
        })
    }

    /// Tones overlapping the samples `[start, end)`.
    pub fn tones_in(
        &self,
        start: u64,
        end: u64,
        rate: u32,
        flash_duration: u64,
        frequencies: &[f64],
    ) -> Vec<Tone> {
        self.tones_in_offset(start, end, rate, flash_duration, frequencies, 0)
    }

    /// Tones overlapping the samples `[start, end)`, with every tone `offset` nanoseconds after
    /// its frame.
    pub fn tones_in_offset(
        &self,
        start: u64,
        end: u64,
        rate: u32,
        flash_duration: u64,
        frequencies: &[f64],
        offset: i64,
    ) -> Vec<Tone> {
        // A tone starts less than one frame plus one sample after its event's nominal time
        // plus the offset, and is shorter than the interval, so looking one event further back
        // is enough.
        let event_time = |time: u64| time.saturating_add_signed(offset.saturating_neg());
        let first = (event_time(sample_time(start, rate)) / self.interval).saturating_sub(1);
        let last = event_time(sample_time(end, rate)) / self.interval;

        (first..=last)
            .filter_map(|event| self.tone_offset(event, rate, flash_duration, frequencies, offset))
            .filter(|tone| tone.start < end && tone.end() > start)
            .collect()
    }
}

pub fn frequency_for_event(event: u64, frequencies: &[f64]) -> f64 {
    frequencies[(event % frequencies.len() as u64) as usize]
}

pub fn sample_time(sample: u64, rate: u32) -> u64 {
    mul_div_floor(sample, SECOND, rate as u64)
}

pub fn sample_at_or_after(time: u64, rate: u32) -> u64 {
    mul_div_ceil(time, rate as u64, SECOND)
}

pub fn duration_to_samples(duration: u64, rate: u32) -> u64 {
    mul_div_floor(duration, rate as u64, SECOND)
}

/// One tone burst, in samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub start: u64,
    pub length: u64,
    pub ramp: u64,
    pub frequency: f64,
    pub rate: u32,
}

impl Tone {
    pub fn end(&self) -> u64 {
        self.start + self.length
    }

    /// Amplitude between -1.0 and 1.0 at `sample`, 0.0 outside the tone.
    pub fn value(&self, sample: u64) -> f64 {
        use std::f64::consts::PI;

        if sample < self.start || sample >= self.end() {
            return 0.0;
        }

        let pos = sample - self.start;
        let from_edge = pos.min(self.length - 1 - pos);
        let gain = if from_edge < self.ramp {
            0.5 * (1.0 - (PI * from_edge as f64 / self.ramp as f64).cos())
        } else {
            1.0
        };

        gain * (2.0 * PI * self.frequency * pos as f64 / self.rate as f64).sin()
    }
}

/// Encodes a clock time in microseconds, modulo 2^32. This wraps after about 71 minutes.
pub fn encode_clock_time(time: u64) -> u32 {
    (time / 1000) as u32
}

/// Time from an encoded clock time to the clock time `now`, in nanoseconds. Correct for up to
/// about 35 minutes in either direction, and up to 1 µs more than the actual time because the
/// encoded time was rounded down.
pub fn time_since_clock_code(now: u64, code: u32) -> i64 {
    let micros = encode_clock_time(now).wrapping_sub(code) as i32 as i64;
    micros * 1000 + (now % 1000) as i64
}

/// Position of the code rows in a frame.
///
/// The rows sit at the bottom of the frame. Each row is split into [`CODE_CELLS`] equal cells,
/// white for a set bit. The rest of the frame is the flash area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeLayout {
    width: u32,
    height: u32,
    row_height: u32,
}

impl CodeLayout {
    pub fn new(width: u32, height: u32) -> Self {
        CodeLayout {
            width,
            height,
            row_height: (height / CODE_ROW_HEIGHT_DIVISOR).max(1),
        }
    }

    pub fn row_height(&self) -> u32 {
        self.row_height
    }

    pub fn code_top(&self) -> u32 {
        self.height.saturating_sub(CODE_ROWS * self.row_height)
    }

    /// The code row containing line `y`, or `None` for the flash area.
    pub fn row_at(&self, y: u32) -> Option<u32> {
        let top = self.code_top();
        if y < top {
            None
        } else {
            Some(((y - top) / self.row_height).min(CODE_ROWS - 1))
        }
    }

    pub fn cell_at(&self, x: u32) -> u32 {
        (x as u64 * CODE_CELLS as u64 / self.width as u64) as u32
    }

    /// Centre of a cell in row `row`, for reading the code back.
    pub fn cell_centre(&self, row: u32, cell: u32) -> (u32, u32) {
        let x = ((2 * cell as u64 + 1) * self.width as u64 / (2 * CODE_CELLS as u64)) as u32;
        let y = self.code_top() + row * self.row_height + self.row_height / 2;
        (x, y)
    }
}

pub fn cell_is_white(cell: u32, value: u32) -> bool {
    match cell {
        0 => true,
        c if c >= CODE_CELLS - 1 => false,
        c => (value >> (CODE_BITS - c)) & 1 == 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_on_frame_grid() {
        // 29.97 fps does not divide 5 s evenly
        let grid = EventGrid::new(DEFAULT_INTERVAL, 30000, 1001);

        assert_eq!(grid.event_frame(0), 0);
        assert_eq!(grid.event_frame(1), 150);
        assert_eq!(grid.event_frame(2), 300);
        assert_eq!(grid.event_frame(3), 450);

        for event in 1..1000 {
            let frame = grid.event_frame(event);
            assert!(grid.frame_pts(frame) >= event * DEFAULT_INTERVAL);
            assert!(grid.frame_pts(frame - 1) < event * DEFAULT_INTERVAL);
            assert_eq!(grid.event_for_frame(frame), event);
            assert_eq!(grid.event_for_frame(frame - 1), event - 1);
        }
    }

    #[test]
    fn flash_frames() {
        let grid = EventGrid::new(DEFAULT_INTERVAL, 30, 1);

        let flashing = (0..300)
            .filter(|&f| grid.frame_is_flash(f, DEFAULT_FLASH_DURATION))
            .collect::<Vec<_>>();
        assert_eq!(flashing, [0, 1, 2, 150, 151, 152]);
    }

    #[test]
    fn tone_aligned_with_frame() {
        let grid = EventGrid::new(DEFAULT_INTERVAL, 30000, 1001);

        for rate in [44100, 48000, 96000] {
            for event in 0..100 {
                let tone = grid.tone(event, rate, DEFAULT_FLASH_DURATION, &DEFAULT_FREQUENCIES);
                let video = grid.event_time(event);
                let audio = sample_time(tone.start, rate);
                assert!(audio >= video);
                assert!(audio - video < SECOND / rate as u64 + 1);
            }
        }
    }

    #[test]
    fn tones_in_range() {
        let grid = EventGrid::new(SECOND, 25, 1);
        let rate = 48000;

        let mut found = vec![];
        let mut start = 0;
        while start < 10 * rate as u64 {
            let end = start + 480;
            for tone in grid.tones_in(start, end, rate, DEFAULT_FLASH_DURATION, &[1000.0]) {
                if found.last() != Some(&tone.start) {
                    found.push(tone.start);
                }
            }
            start = end;
        }

        assert_eq!(found, (0..10).map(|n| n * rate as u64).collect::<Vec<_>>());
    }

    #[test]
    fn tones_offset() {
        let grid = EventGrid::new(SECOND, 25, 1);
        let rate = 48000;
        let samples = 5 * rate as u64;

        let starts = |offset: i64| {
            grid.tones_in_offset(0, samples, rate, DEFAULT_FLASH_DURATION, &[1000.0], offset)
                .iter()
                .map(|tone| tone.start)
                .collect::<Vec<_>>()
        };
        // Audio 100 ms late
        assert_eq!(starts(100_000_000), [4800, 52800, 100800, 148800, 196800]);
        // Audio 1.5 s early, the first two tones are before the stream
        assert_eq!(
            starts(-1_500_000_000),
            [24000, 72000, 120000, 168000, 216000]
        );
        // Ten events early, the tones of events 10 onwards take their place
        assert_eq!(starts(-10 * SECOND as i64), starts(0));
        assert_eq!(
            grid.tone_offset(0, rate, DEFAULT_FLASH_DURATION, &[1000.0], -1),
            None
        );
    }

    #[test]
    fn tone_envelope() {
        let tone = Tone {
            start: 100,
            length: 4800,
            ramp: 48,
            frequency: 1000.0,
            rate: 48000,
        };

        assert_eq!(tone.value(99), 0.0);
        assert_eq!(tone.value(100), 0.0);
        assert_eq!(tone.value(100 + 4800), 0.0);
        let peak = (100..4900).map(|s| tone.value(s).abs()).fold(0.0, f64::max);
        assert!(peak > 0.99 && peak <= 1.0);
    }

    #[test]
    fn frequencies_valid() {
        assert!(validate_frequencies(&DEFAULT_FREQUENCIES).is_ok());
        assert!(validate_frequencies(&[440.0]).is_ok());
        assert!(validate_frequencies(&[1000.0, 1000.0 + MIN_FREQUENCY_SPACING]).is_ok());
        assert!(validate_frequencies(&[]).is_err());
        assert!(validate_frequencies(&[1000.0, 1000.0]).is_err());
        assert!(validate_frequencies(&[1000.0, 2000.0, 1100.0]).is_err());
        assert!(validate_frequencies(&[1000.0, f64::NAN]).is_err());
        assert!(validate_frequencies(&[f64::INFINITY]).is_err());
        assert!(validate_frequencies(&[0.0, 1000.0]).is_err());
    }

    #[test]
    fn frequency_rotation() {
        let freqs = DEFAULT_FREQUENCIES;
        assert_eq!(frequency_for_event(0, &freqs), 1000.0);
        assert_eq!(frequency_for_event(3, &freqs), 2500.0);
        assert_eq!(frequency_for_event(4, &freqs), 1000.0);
    }

    #[test]
    fn code_cells() {
        let value = 0x8000_0001;
        let cells = (0..CODE_CELLS)
            .map(|c| cell_is_white(c, value))
            .collect::<Vec<_>>();
        assert!(cells[0]);
        assert!(cells[1]);
        assert!(cells[2..32].iter().all(|w| !w));
        assert!(cells[32]);
        assert!(!cells[33]);

        let layout = CodeLayout::new(1280, 720);
        assert_eq!(layout.code_top(), 720 - 180);
        assert_eq!(layout.row_at(539), None);
        assert_eq!(layout.row_at(540), Some(0));
        assert_eq!(layout.row_at(719), Some(2));
        for row in 0..CODE_ROWS {
            for cell in 0..CODE_CELLS {
                let (x, y) = layout.cell_centre(row, cell);
                assert_eq!(layout.cell_at(x), cell);
                assert_eq!(layout.row_at(y), Some(row));
            }
        }
    }

    #[test]
    fn clock_time_code() {
        let time = 1_760_000_000_123_456_789;
        let code = encode_clock_time(time);

        assert_eq!(time_since_clock_code(time - 789, code), 0);
        let latencies = [
            0,
            999,
            1000,
            150_000_000,
            -5_000_000,
            30 * 60 * SECOND as i64,
        ];
        for latency in latencies {
            let now = time.checked_add_signed(latency).unwrap();
            let since = time_since_clock_code(now, code);
            assert!(
                (latency..latency + 1000).contains(&since),
                "{latency} {since}"
            );
        }

        // Across the wrap around
        let time = (u32::MAX as u64 - 10) * 1000;
        let code = encode_clock_time(time);
        assert_eq!(time_since_clock_code(time + 20_000, code), 20_000);
        assert_eq!(time_since_clock_code(time - 20_000, code), -20_000);
    }
}
