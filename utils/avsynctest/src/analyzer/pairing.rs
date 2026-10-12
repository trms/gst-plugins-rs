// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Pairing of video and audio events.
//!
//! Video events carry their full index. Audio events only carry the index modulo the number of
//! frequencies, so an audio event is paired with the video event of a matching index that is
//! nearest in time, within half a cycle of frequencies. Larger offsets pair with the wrong event.
//!
//! The streams can arrive in any order, so events wait until the other stream has gone past the
//! time where their partner could be. Times are running times.

use std::collections::VecDeque;

/// Events waiting for a partner, per stream. Beyond this the oldest is given up on, and counted
/// as missed if the other stream has been analyzed at all.
const MAX_PENDING: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEvent {
    pub index: u64,
    pub time: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioEvent {
    /// Index modulo the number of frequencies.
    pub residue: u64,
    pub time: u64,
    /// Stream time of the event, passed on in the pair.
    pub stream_time: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub index: u64,
    pub video_time: u64,
    pub audio_time: u64,
    pub audio_stream_time: u64,
}

impl Pair {
    /// Audio time minus video time. Positive if audio is late.
    pub fn offset(&self) -> i64 {
        self.audio_time as i64 - self.video_time as i64
    }
}

#[derive(Clone, Debug)]
pub struct Pairer {
    modulus: u64,
    range: u64,
    video: VecDeque<VideoEvent>,
    audio: VecDeque<AudioEvent>,
    /// Running time up to which each stream has been analyzed.
    video_position: Option<u64>,
    audio_position: Option<u64>,
    missed_video: u64,
    missed_audio: u64,
}

impl Pairer {
    pub fn new(interval: u64, modulus: u64) -> Self {
        assert!(interval > 0 && modulus > 0);

        Pairer {
            modulus,
            range: Self::range_for(interval, modulus),
            video: VecDeque::new(),
            audio: VecDeque::new(),
            video_position: None,
            audio_position: None,
            missed_video: 0,
            missed_audio: 0,
        }
    }

    /// Largest offset that pairs with the right event.
    pub fn range_for(interval: u64, modulus: u64) -> u64 {
        interval.saturating_mul(modulus) / 2
    }

    /// Whether an event at `time` can still pair with an event of the other stream, whose
    /// analysis is at `position`.
    fn can_pair(&self, time: u64, position: Option<u64>) -> bool {
        position.is_none_or(|position| time.saturating_add(self.range) >= position)
    }

    /// Audio events for which no video event was found.
    pub fn missed_video(&self) -> u64 {
        self.missed_video
    }

    /// Video events for which no audio event was found.
    pub fn missed_audio(&self) -> u64 {
        self.missed_audio
    }

    /// Forgets pending events and positions, for when running times start again. The missed
    /// counts are kept.
    pub fn flush(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.video_position = None;
        self.audio_position = None;
    }

    pub fn push_video(&mut self, event: VideoEvent) -> Option<Pair> {
        let residue = event.index % self.modulus;
        let found = nearest(&self.audio, event.time, self.range, |a| {
            a.residue == residue
        });

        match found {
            Some(n) => {
                let audio = self.audio.remove(n).unwrap();
                Some(pair(event, audio))
            }
            None if !self.can_pair(event.time, self.audio_position) => {
                // The audio it could have paired with has already been analyzed
                self.missed_audio += 1;
                None
            }
            None => {
                if self.video.len() == MAX_PENDING {
                    self.video.pop_front();
                    self.missed_audio += self.audio_position.is_some() as u64;
                }
                self.video.push_back(event);
                None
            }
        }
    }

    pub fn push_audio(&mut self, event: AudioEvent) -> Option<Pair> {
        let modulus = self.modulus;
        let found = nearest(&self.video, event.time, self.range, |v| {
            v.index % modulus == event.residue
        });

        match found {
            Some(n) => {
                let video = self.video.remove(n).unwrap();
                Some(pair(video, event))
            }
            None if !self.can_pair(event.time, self.video_position) => {
                // The video it could have paired with has already been analyzed
                self.missed_video += 1;
                None
            }
            None => {
                if self.audio.len() == MAX_PENDING {
                    self.audio.pop_front();
                    self.missed_video += self.video_position.is_some() as u64;
                }
                self.audio.push_back(event);
                None
            }
        }
    }

    /// All video up to `position` has been analyzed. Audio events that could only have paired
    /// with earlier video are missed.
    pub fn advance_video(&mut self, position: u64) {
        self.video_position = Some(position);
        let range = self.range;
        let before = self.audio.len();
        self.audio
            .retain(|a| a.time.saturating_add(range) >= position);
        self.missed_video += (before - self.audio.len()) as u64;
    }

    /// All audio up to `position` has been analyzed. Video events that could only have paired
    /// with earlier audio are missed.
    pub fn advance_audio(&mut self, position: u64) {
        self.audio_position = Some(position);
        let range = self.range;
        let before = self.video.len();
        self.video
            .retain(|v| v.time.saturating_add(range) >= position);
        self.missed_audio += (before - self.video.len()) as u64;
    }

    /// No more video will arrive. All pending audio events are missed, and so are any audio
    /// events still to come.
    pub fn end_video(&mut self) {
        self.video_position = Some(u64::MAX);
        self.missed_video += self.audio.len() as u64;
        self.audio.clear();
    }

    /// No more audio will arrive. All pending video events are missed, and so are any video
    /// events still to come.
    pub fn end_audio(&mut self) {
        self.audio_position = Some(u64::MAX);
        self.missed_audio += self.video.len() as u64;
        self.video.clear();
    }
}

fn pair(video: VideoEvent, audio: AudioEvent) -> Pair {
    Pair {
        index: video.index,
        video_time: video.time,
        audio_time: audio.time,
        audio_stream_time: audio.stream_time,
    }
}

trait Timed {
    fn time(&self) -> u64;
}

impl Timed for VideoEvent {
    fn time(&self) -> u64 {
        self.time
    }
}

impl Timed for AudioEvent {
    fn time(&self) -> u64 {
        self.time
    }
}

/// Position of the event nearest to `time` that is within `range` and matches.
fn nearest<T: Timed>(
    events: &VecDeque<T>,
    time: u64,
    range: u64,
    matches: impl Fn(&T) -> bool,
) -> Option<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches(e) && e.time().abs_diff(time) <= range)
        .min_by_key(|(_, e)| e.time().abs_diff(time))
        .map(|(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: u64 = 1_000_000_000;

    fn video(index: u64, time: u64) -> VideoEvent {
        VideoEvent { index, time }
    }

    fn audio(residue: u64, time: u64) -> AudioEvent {
        AudioEvent {
            residue,
            time,
            stream_time: time,
        }
    }

    /// Feeds events `0..count` with audio `offset` from video, interleaved by time as they
    /// would arrive, and returns the offsets of the pairs.
    fn run(pairer: &mut Pairer, count: u64, offset: i64) -> Vec<(u64, i64)> {
        let start = 10 * INTERVAL as i64;
        let mut items = vec![];
        for n in 0..count {
            let tv = start + (n * INTERVAL) as i64;
            items.push((tv, true, n));
            items.push((tv + offset, false, n));
        }
        items.sort();

        let mut pairs = vec![];
        for (time, is_video, n) in items {
            let time = time as u64;
            let found = if is_video {
                let found = pairer.push_video(video(n, time));
                pairer.advance_video(time);
                found
            } else {
                let found = pairer.push_audio(audio(n % pairer.modulus, time));
                pairer.advance_audio(time);
                found
            };
            pairs.extend(found.map(|p| (p.index, p.offset())));
        }

        pairs
    }

    #[test]
    fn either_order() {
        let mut pairer = Pairer::new(INTERVAL, 4);
        assert_eq!(pairer.push_video(video(5, 1000)), None);
        let pair = pairer.push_audio(audio(1, 1500)).unwrap();
        assert_eq!(pair.index, 5);
        assert_eq!(pair.offset(), 500);

        assert_eq!(pairer.push_audio(audio(2, 2000)), None);
        let pair = pairer.push_video(video(6, 2600)).unwrap();
        assert_eq!(pair.index, 6);
        assert_eq!(pair.offset(), -600);
    }

    #[test]
    fn residue_must_match() {
        let mut pairer = Pairer::new(INTERVAL, 4);
        assert_eq!(pairer.push_video(video(5, 1000)), None);
        assert_eq!(pairer.push_audio(audio(2, 1000)), None);
        assert_eq!(pairer.push_audio(audio(1, 1100)).unwrap().index, 5);
    }

    #[test]
    fn offsets() {
        for offset in [
            0,
            1,
            -1,
            100_000_000,
            -66_733_333,
            1_999_999_999,
            -1_999_999_999,
        ] {
            let mut pairer = Pairer::new(INTERVAL, 4);
            let pairs = run(&mut pairer, 12, offset);
            let expected = (0..12).map(|n| (n, offset)).collect::<Vec<_>>();
            assert_eq!(pairs, expected, "offset {offset}");
            assert_eq!(pairer.missed_audio(), 0);
            assert_eq!(pairer.missed_video(), 0);
        }
    }

    #[test]
    fn beyond_range() {
        // Audio 2.5 intervals late pairs with the video event 4 later
        let mut pairer = Pairer::new(INTERVAL, 4);
        let pairs = run(&mut pairer, 12, 2_500_000_000);
        assert!(!pairs.is_empty());
        for (index, offset) in &pairs {
            assert!(*index >= 4);
            assert_eq!(*offset, -1_500_000_000);
        }
        // The first video events have no audio
        assert_eq!(pairer.missed_audio(), 4);

        // With one frequency only half an interval works
        let mut pairer = Pairer::new(INTERVAL, 1);
        assert_eq!(Pairer::range_for(INTERVAL, 1), INTERVAL / 2);
        let pairs = run(&mut pairer, 6, 400_000_000);
        assert!(pairs.iter().all(|(_, offset)| *offset == 400_000_000));
        assert_eq!(pairs.len(), 6);
    }

    #[test]
    fn missed() {
        let mut pairer = Pairer::new(INTERVAL, 4);
        // Video 0 has no audio, audio 2 has no video
        assert_eq!(pairer.push_video(video(0, 0)), None);
        pairer.advance_video(0);
        assert_eq!(pairer.push_video(video(1, INTERVAL)), None);
        pairer.advance_video(INTERVAL);
        assert!(pairer.push_audio(audio(1, INTERVAL + 10)).is_some());
        pairer.advance_audio(INTERVAL + 10);
        assert_eq!(pairer.push_audio(audio(2, 2 * INTERVAL)), None);
        pairer.advance_audio(2 * INTERVAL);
        assert_eq!(pairer.missed_audio(), 0);

        pairer.advance_audio(2 * INTERVAL + 1);
        assert_eq!(pairer.missed_audio(), 1);
        pairer.advance_video(4 * INTERVAL);
        assert_eq!(pairer.missed_video(), 0);
        pairer.advance_video(4 * INTERVAL + 1);
        assert_eq!(pairer.missed_video(), 1);
    }

    #[test]
    fn late_events() {
        // Events behind the other stream's position are missed straight away
        let mut pairer = Pairer::new(INTERVAL, 4);
        pairer.advance_audio(10 * INTERVAL);
        assert_eq!(pairer.push_video(video(1, INTERVAL)), None);
        assert_eq!(pairer.missed_audio(), 1);
        assert!(pairer.video.is_empty());

        pairer.advance_video(10 * INTERVAL);
        assert_eq!(pairer.push_audio(audio(2, 2 * INTERVAL)), None);
        assert_eq!(pairer.missed_video(), 1);
        assert!(pairer.audio.is_empty());

        // Within range they still wait
        assert_eq!(pairer.push_video(video(9, 9 * INTERVAL)), None);
        assert_eq!(pairer.missed_audio(), 1);
        assert_eq!(pairer.video.len(), 1);
    }

    #[test]
    fn end_of_stream() {
        let mut pairer = Pairer::new(INTERVAL, 4);
        assert_eq!(pairer.push_video(video(0, 0)), None);
        assert_eq!(pairer.push_video(video(1, INTERVAL)), None);
        assert_eq!(pairer.push_audio(audio(2, 2 * INTERVAL)), None);
        pairer.advance_video(INTERVAL);
        pairer.advance_audio(2 * INTERVAL);

        pairer.end_audio();
        assert_eq!(pairer.missed_audio(), 2);
        assert_eq!(pairer.missed_video(), 0);
        // Video arriving after audio ended is missed at once
        assert_eq!(pairer.push_video(video(3, 3 * INTERVAL)), None);
        assert_eq!(pairer.missed_audio(), 3);

        pairer.end_video();
        assert_eq!(pairer.missed_video(), 1);
        assert_eq!(pairer.push_audio(audio(0, 4 * INTERVAL)), None);
        assert_eq!(pairer.missed_video(), 2);

        // Nothing pending after a flush, and the counts are kept
        pairer.flush();
        assert_eq!(pairer.push_video(video(5, 5 * INTERVAL)), None);
        assert_eq!(pairer.missed_audio(), 3);
        assert_eq!(pairer.video.len(), 1);
    }

    #[test]
    fn queue_overflow() {
        // Video alone fills the queue without anything being missed
        let mut pairer = Pairer::new(INTERVAL, 4);
        for n in 0..MAX_PENDING as u64 + 10 {
            assert_eq!(pairer.push_video(video(n, n * INTERVAL)), None);
        }
        assert_eq!(pairer.missed_audio(), 0);

        // Once audio has been analyzed, what stalled audio evicts is missed audio
        let mut pairer = Pairer::new(INTERVAL, 4);
        pairer.advance_audio(0);
        for n in 0..MAX_PENDING as u64 + 10 {
            assert_eq!(pairer.push_video(video(n, n * INTERVAL)), None);
        }
        assert_eq!(pairer.missed_audio(), 10);
        pairer.end_audio();
        assert_eq!(pairer.missed_audio(), MAX_PENDING as u64 + 10);

        let mut pairer = Pairer::new(INTERVAL, 4);
        pairer.advance_video(0);
        for n in 0..MAX_PENDING as u64 + 10 {
            assert_eq!(pairer.push_audio(audio(n % 4, n * INTERVAL)), None);
        }
        assert_eq!(pairer.missed_video(), 10);
    }

    #[test]
    fn huge_interval() {
        let mut pairer = Pairer::new(u64::MAX, 4);
        assert_eq!(Pairer::range_for(u64::MAX, 4), u64::MAX / 2);
        assert_eq!(pairer.push_video(video(0, u64::MAX - 1)), None);
        pairer.advance_audio(u64::MAX);
        assert_eq!(pairer.missed_audio(), 0);
    }

    #[test]
    fn one_stream() {
        // Without the other stream nothing is missed
        let mut pairer = Pairer::new(INTERVAL, 4);
        for n in 0..2000 {
            pairer.push_video(video(n, n * INTERVAL));
            pairer.advance_video(n * INTERVAL);
        }
        assert_eq!(pairer.missed_audio(), 0);
        assert_eq!(pairer.video.len(), MAX_PENDING);
    }

    #[test]
    fn flush() {
        let mut pairer = Pairer::new(INTERVAL, 4);
        pairer.push_video(video(8, 8 * INTERVAL));
        pairer.flush();
        assert_eq!(pairer.push_audio(audio(0, 8 * INTERVAL)), None);
        pairer.advance_audio(20 * INTERVAL);
        assert_eq!(pairer.missed_audio(), 0);
    }
}
