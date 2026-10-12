// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Tone detection. Samples are mono, full scale is 1.0.

use std::f64::consts::PI;

use crate::pattern::{self, RAMP_DURATION};

/// Peak level a tone must exceed to be detected, -40 dBFS, or a multiple of the noise floor if
/// that is higher.
const ON_LEVEL: f64 = 0.01;
const OFF_LEVEL: f64 = 0.005;
/// A tone must be 12 dB above the noise floor, and is over once it drops to 6 dB above it.
const ON_RATIO: f64 = 4.0;
const OFF_RATIO: f64 = 2.0;
/// The level is measured over blocks of this length.
const BLOCK_DURATION: u64 = 1_000_000;
/// Time constant for the noise floor to rise towards louder blocks. It drops to a quieter block
/// straight away. A tone raises it too, which ends tones longer than about 0.7 s early, after
/// they have been reported, and the first quiet block after a tone brings it back down.
const FLOOR_RISE_TIME: f64 = pattern::SECOND as f64;
/// Length of tone after its ramp that is used to tell the frequencies apart.
const IDENTIFY_DURATION: u64 = 10_000_000;
/// Fraction of the power that must be in the identified frequency.
const MIN_PURITY: f64 = 0.5;

/// Tones are reported at most this long after they start, counting from the start of the samples
/// that were pushed.
pub const MAX_REPORT_DELAY: u64 = RAMP_DURATION + IDENTIFY_DURATION + 4 * BLOCK_DURATION;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneEvent {
    /// Time of the first sample of the tone.
    pub time: u64,
    pub frequency_index: usize,
    /// Peak amplitude once the ramp is over.
    pub level: f64,
    /// Level of the quiet blocks before the tone.
    pub noise_floor: f64,
}

#[derive(Clone, Copy, Debug)]
enum Detection {
    Off,
    /// The block starting at `detected` was loud, waiting for enough of the tone to locate and
    /// identify it. `off` is the block where it went quiet again, for short tones.
    Pending {
        detected: u64,
        off: Option<u64>,
    },
    On,
}

/// Finds the first sample of each tone.
///
/// The level of each 1 ms block finds the tone roughly, against a noise floor that follows the
/// level of the quiet blocks between tones. A least squares fit of the known ramp shape at the
/// identified frequency then finds the sample the tone starts on.
#[derive(Clone, Debug)]
pub struct ToneDetector {
    rate: u32,
    frequencies: Vec<f64>,
    block: u64,
    ramp: u64,
    identify: u64,
    /// Fraction of the way the noise floor rises towards a louder block.
    floor_rise: f64,
    /// Time of sample 0. Samples are counted from the first buffer after a reset.
    base_time: Option<u64>,
    position: u64,
    history: Vec<f32>,
    history_start: u64,
    /// Samples before this have been measured in blocks.
    processed: u64,
    /// Level of the quiet blocks.
    floor: f64,
    detection: Detection,
}

impl ToneDetector {
    pub fn new(rate: u32, frequencies: &[f64]) -> Self {
        assert!(rate > 0 && !frequencies.is_empty());

        let block = pattern::duration_to_samples(BLOCK_DURATION, rate).max(1);
        let block_duration = pattern::sample_time(block, rate) as f64;

        ToneDetector {
            rate,
            frequencies: frequencies.to_vec(),
            block,
            ramp: pattern::duration_to_samples(RAMP_DURATION, rate).max(1),
            identify: pattern::duration_to_samples(IDENTIFY_DURATION, rate).max(1),
            floor_rise: 1.0 - (-block_duration / FLOOR_RISE_TIME).exp(),
            base_time: None,
            position: 0,
            history: vec![],
            history_start: 0,
            processed: 0,
            floor: 0.0,
            detection: Detection::Off,
        }
    }

    pub fn reset(&mut self) {
        self.base_time = None;
        self.position = 0;
        self.history.clear();
        self.history_start = 0;
        self.processed = 0;
        self.floor = 0.0;
        self.detection = Detection::Off;
    }

    fn time_of(&self, sample: u64) -> u64 {
        self.base_time.unwrap_or(0) + pattern::sample_time(sample, self.rate)
    }

    fn samples(&self, start: u64, end: u64) -> &[f32] {
        &self.history[(start - self.history_start) as usize..(end - self.history_start) as usize]
    }

    /// Feeds samples starting at `time`, or straight after the previous ones if `None`.
    ///
    /// Samples that don't follow on from the previous ones reset the detector. Tones are
    /// reported once enough of them has been seen, which can be a few buffers later.
    pub fn push(&mut self, time: Option<u64>, samples: &[f32]) -> Vec<ToneEvent> {
        if let Some(time) = time {
            let tolerance = pattern::SECOND / self.rate as u64 + 1;
            let continuous =
                self.base_time.is_some() && self.time_of(self.position).abs_diff(time) <= tolerance;
            if !continuous {
                self.reset();
                self.base_time = Some(time);
            }
        } else if self.base_time.is_none() {
            return vec![];
        }

        self.history.extend_from_slice(samples);
        self.position += samples.len() as u64;

        let mut events = vec![];
        while self.processed + self.block <= self.position {
            let start = self.processed;
            self.processed += self.block;
            let level = level(self.samples(start, self.processed));

            // The floor follows quieter blocks at once and louder ones slowly
            if level < self.floor {
                self.floor = level;
            } else {
                self.floor += (level - self.floor) * self.floor_rise;
            }
            let on_level = ON_LEVEL.max(ON_RATIO * self.floor);
            let off_level = OFF_LEVEL.max(OFF_RATIO * self.floor);

            self.detection = match self.detection {
                Detection::Off if level > on_level => Detection::Pending {
                    detected: start,
                    off: None,
                },
                Detection::Pending {
                    detected,
                    off: None,
                } if level < off_level => Detection::Pending {
                    detected,
                    off: Some(start),
                },
                Detection::On if level < off_level => Detection::Off,
                detection => detection,
            };

            if let Detection::Pending { detected, off } = self.detection {
                let ready = detected + self.ramp + self.identify;
                let end = off.map_or(ready, |off| off.min(ready));
                if self.processed >= end {
                    let event = self.locate(detected, end);
                    // Anything that is not a tone, such as noise, is treated as quiet so the
                    // next loud block is looked at again
                    self.detection = if off.is_none() && event.is_some() {
                        Detection::On
                    } else {
                        Detection::Off
                    };
                    events.extend(event);
                }
            }
        }

        // Keep enough to locate a tone found in the next blocks
        let keep = match self.detection {
            Detection::Pending { detected, .. } => detected,
            _ => self.processed,
        };
        let keep = keep.saturating_sub(4 * self.block).max(self.history_start);
        self.history.drain(..(keep - self.history_start) as usize);
        self.history_start = keep;

        events
    }

    /// Identifies and locates a tone that was loud from the block at `detected` until `end`.
    fn locate(&self, detected: u64, end: u64) -> Option<ToneEvent> {
        let tone = self.samples(detected, end);
        let power = tone.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / tone.len() as f64;
        let (frequency_index, amplitude) = self
            .frequencies
            .iter()
            .map(|&f| goertzel(tone, f, self.rate))
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        if amplitude * amplitude / 2.0 < MIN_PURITY * power {
            return None;
        }

        // The tone starts in the detected block or shortly before, while its level is still low
        let search_start = detected
            .saturating_sub(2 * self.block)
            .max(self.history_start);
        let search_end = detected + self.block;
        let window_start = search_start
            .saturating_sub(self.block)
            .max(self.history_start);
        let window_end = (search_end + self.ramp + 2 * self.block).min(end);
        let w = 2.0 * PI * self.frequencies[frequency_index] / self.rate as f64;

        let (start, level) = self.fit(w, window_start, window_end, search_start..search_end)?;
        // The model has no ramp down at the end. Tones are at least two ramps long, so fit again
        // only up to the end of the ramp up in case this is a short one.
        let (start, _) = self.fit(
            w,
            window_start,
            (start + self.ramp).clamp(search_end, window_end),
            search_start..search_end,
        )?;

        Some(ToneEvent {
            time: self.time_of(start),
            frequency_index,
            level,
            noise_floor: self.floor,
        })
    }

    /// Finds the start in `starts` for which a tone of angular frequency `w` with the known ramp,
    /// and any amplitude and phase, best explains the samples between `window_start` and
    /// `window_end`. Returns the start and the tone's amplitude.
    fn fit(
        &self,
        w: f64,
        window_start: u64,
        window_end: u64,
        starts: std::ops::Range<u64>,
    ) -> Option<(u64, f64)> {
        let x = self.samples(window_start, window_end);
        let (cos, sin): (Vec<f64>, Vec<f64>) = (0..x.len())
            .map(|n| ((w * n as f64).cos(), (w * n as f64).sin()))
            .unzip();

        let mut best: Option<(u64, f64, f64)> = None;
        for start in starts {
            let offset = (start - window_start) as usize;
            let (mut cc, mut cs, mut ss, mut xc, mut xs) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for n in offset..x.len() {
                let envelope = envelope((n - offset) as u64, self.ramp);
                let (c, s) = (envelope * cos[n], envelope * sin[n]);
                let v = x[n] as f64;
                cc += c * c;
                cs += c * s;
                ss += s * s;
                xc += v * c;
                xs += v * s;
            }

            let det = cc * ss - cs * cs;
            if det <= f64::EPSILON {
                continue;
            }
            let a = (xc * ss - xs * cs) / det;
            let b = (xs * cc - xc * cs) / det;
            let explained = a * xc + b * xs;

            if best.is_none_or(|(_, e, _)| explained > e) {
                best = Some((start, explained, a.hypot(b)));
            }
        }

        best.map(|(start, _, level)| (start, level))
    }
}

/// Raised cosine ramp matching [`pattern::Tone`], `pos` samples after the start.
fn envelope(pos: u64, ramp: u64) -> f64 {
    if pos < ramp {
        0.5 * (1.0 - (PI * pos as f64 / ramp as f64).cos())
    } else {
        1.0
    }
}

/// Peak level of a sine wave with the same power as `samples`.
fn level(samples: &[f32]) -> f64 {
    let power = samples.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / samples.len() as f64;
    (2.0 * power).sqrt()
}

/// Amplitude of `frequency` in `samples`.
fn goertzel(samples: &[f32], frequency: f64, rate: u32) -> f64 {
    let coeff = 2.0 * (2.0 * PI * frequency / rate as f64).cos();
    let (s1, s2) = samples
        .iter()
        .fold((0.0, 0.0), |(s1, s2), &x| (x as f64 + coeff * s1 - s2, s1));
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / samples.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{DEFAULT_FLASH_DURATION, DEFAULT_FREQUENCIES, EventGrid, SECOND};

    fn signal(tones: &[pattern::Tone], samples: u64, volume: f64) -> Vec<f32> {
        (0..samples)
            .map(|s| (volume * tones.iter().map(|t| t.value(s)).sum::<f64>()) as f32)
            .collect()
    }

    fn run(detector: &mut ToneDetector, rate: u32, signal: &[f32], chunk: usize) -> Vec<ToneEvent> {
        signal
            .chunks(chunk)
            .enumerate()
            .flat_map(|(i, chunk_samples)| {
                let time = pattern::sample_time((i * chunk) as u64, rate);
                detector.push(Some(time), chunk_samples)
            })
            .collect()
    }

    /// White noise of the given RMS level.
    fn noise(samples: u64, rms: f64, seed: u32) -> Vec<f32> {
        let mut seed = seed;
        (0..samples)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Uniform in ±√3 × rms
                ((seed as f64 / u32::MAX as f64 - 0.5) * 2.0 * 3f64.sqrt() * rms) as f32
            })
            .collect()
    }

    fn mix(signal: &mut [f32], other: &[f32]) {
        for (a, b) in signal.iter_mut().zip(other) {
            *a += b;
        }
    }

    /// Feeds tones of `volume` from sample `first` with noise of `noise_rms` added and checks
    /// every one is found at the right time, within `tolerance` samples.
    fn check_noisy(
        rate: u32,
        flash_duration: u64,
        volume: f64,
        first: u64,
        noise_rms: f64,
        tolerance: u64,
        chunk: usize,
    ) {
        let grid = EventGrid::new(SECOND, 30000, 1001);
        let samples = 5 * rate as u64;
        let expected = grid.tones_in(first, samples, rate, flash_duration, &DEFAULT_FREQUENCIES);
        let mut signal = signal(&expected, samples, volume);
        mix(&mut signal, &noise(samples, noise_rms, 7));
        // Quantise like S16
        for x in &mut signal {
            *x = (*x * 32767.0).round() / 32767.0;
        }

        let mut detector = ToneDetector::new(rate, &DEFAULT_FREQUENCIES);
        let found = run(&mut detector, rate, &signal, chunk);

        assert_eq!(found.len(), expected.len(), "{found:?}");
        let tolerance = pattern::sample_time(tolerance, rate);
        for (found, tone) in found.iter().zip(&expected) {
            let time = pattern::sample_time(tone.start, rate);
            assert!(
                found.time.abs_diff(time) <= tolerance,
                "{found:?} expected at {time}"
            );
            assert_eq!(DEFAULT_FREQUENCIES[found.frequency_index], tone.frequency);
            // Short tones have no steady part to measure
            if flash_duration >= IDENTIFY_DURATION {
                assert!((found.level - volume).abs() < 0.01 + noise_rms, "{found:?}");
            }
        }
    }

    fn check(rate: u32, flash_duration: u64, volume: f64, chunk: usize) {
        check_noisy(rate, flash_duration, volume, 0, 0.0, 0, chunk);
    }

    #[test]
    fn exact_onset() {
        check(48000, DEFAULT_FLASH_DURATION, 0.8, 480);
        check(44100, DEFAULT_FLASH_DURATION, 0.8, 441);
        check(96000, DEFAULT_FLASH_DURATION, 0.8, 1024);
        check(48000, DEFAULT_FLASH_DURATION, 0.05, 333);
    }

    #[test]
    fn short_tones() {
        check(48000, 2 * RAMP_DURATION, 0.8, 480);
        check(44100, 5_000_000, 0.8, 100);
    }

    #[test]
    fn closest_frequencies() {
        // Frequencies the minimum spacing apart are told apart
        let rate = 48000;
        let frequencies = [1000.0, 1000.0 + pattern::MIN_FREQUENCY_SPACING];
        let grid = EventGrid::new(SECOND, 25, 1);
        let samples = 4 * rate as u64;
        let expected = grid.tones_in(0, samples, rate, DEFAULT_FLASH_DURATION, &frequencies);
        let signal = signal(&expected, samples, 0.8);

        let mut detector = ToneDetector::new(rate, &frequencies);
        let found = run(&mut detector, rate, &signal, 480);

        assert_eq!(found.len(), expected.len(), "{found:?}");
        for (event, (found, tone)) in found.iter().zip(&expected).enumerate() {
            assert_eq!(found.time, pattern::sample_time(tone.start, rate));
            assert_eq!(found.frequency_index, event % 2, "{found:?}");
        }
    }

    #[test]
    fn tones_in_noise() {
        // Noise at -40 dBFS, above the fixed detection level
        check_noisy(48000, DEFAULT_FLASH_DURATION, 0.8, 0, 0.01, 1, 480);
        check_noisy(44100, DEFAULT_FLASH_DURATION, 0.8, 0, 0.01, 1, 441);
        // Noise at -30 dBFS
        check_noisy(48000, DEFAULT_FLASH_DURATION, 0.8, 0, 0.03, 2, 333);
        // The shortest tones, once the floor has adapted to the noise. Before that the end of
        // such a tone is not seen and it is identified from mostly noise.
        check_noisy(48000, 2 * RAMP_DURATION, 0.8, 48000, 0.03, 2, 480);
        // A quiet tone 20 dB above the noise
        check_noisy(48000, DEFAULT_FLASH_DURATION, 0.1, 0, 0.01, 4, 480);
    }

    #[test]
    fn noise_is_not_a_tone() {
        // Noise at -17 dBFS
        let noise = noise(48000, 0.144, 1);

        let mut detector = ToneDetector::new(48000, &DEFAULT_FREQUENCIES);
        assert!(run(&mut detector, 48000, &noise, 480).is_empty());
        // The floor has settled around the noise level
        let floor = detector.floor;
        assert!((0.1..0.25).contains(&floor), "{floor}");
    }

    #[test]
    fn tones_after_noise() {
        // A second of noise at -30 dBFS, then tones with the same noise. Noise above the fixed
        // detection level used to leave the detector waiting for silence that never came.
        let rate = 48000;
        let grid = EventGrid::new(SECOND, 25, 1);
        let samples = 4 * rate as u64;
        let expected = grid.tones_in(
            rate as u64,
            samples,
            rate,
            DEFAULT_FLASH_DURATION,
            &DEFAULT_FREQUENCIES,
        );
        let mut signal = signal(&expected, samples, 0.8);
        mix(&mut signal, &noise(samples, 0.03, 3));

        let mut detector = ToneDetector::new(rate, &DEFAULT_FREQUENCIES);
        let found = run(&mut detector, rate, &signal, 480);

        assert_eq!(found.len(), expected.len(), "{found:?}");
        for (found, tone) in found.iter().zip(&expected) {
            let time = pattern::sample_time(tone.start, rate);
            assert!(
                found.time.abs_diff(time) <= pattern::sample_time(2, rate),
                "{found:?} expected at {time}"
            );
        }
    }

    #[test]
    fn discontinuity() {
        let rate = 48000;
        let tone = pattern::Tone {
            start: 0,
            length: 4800,
            ramp: 48,
            frequency: DEFAULT_FREQUENCIES[2],
            rate,
        };
        let signal = signal(&[tone], 9600, 0.8);

        let mut detector = ToneDetector::new(rate, &DEFAULT_FREQUENCIES);
        // The start of a tone, too short to report, then a jump to a full tone at 10 s
        assert!(detector.push(Some(0), &signal[..96]).is_empty());
        let found = detector.push(Some(10 * SECOND), &signal);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].time, 10 * SECOND);
        assert_eq!(found[0].frequency_index, 2);
    }
}
