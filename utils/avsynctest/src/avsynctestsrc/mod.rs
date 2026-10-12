// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-avsynctestsrc
 * @see_also: avsyncdetect
 *
 * Generates a video and an audio test pattern for measuring A/V synchronisation.
 *
 * Every #GstAvSyncTestSrc:interval the video flashes white and the audio plays a tone of the
 * same length. Event N falls on the first video frame at or after N × interval, and the tone
 * starts on the first audio sample at or after that frame's timestamp, so the two streams are
 * aligned to within one audio sample when they leave this element. Any offset measured
 * downstream was introduced by the pipeline under test.
 *
 * The tone frequency rotates through #GstAvSyncTestSrc:frequencies by event index, so a
 * detector can pair audio and video events even when they are offset by more than one interval.
 *
 * #GstAvSyncTestSrc:audio-offset moves the tones away from the flashes by a known amount, to
 * check a detector or to see how a pipeline treats streams that are out of sync. Positive values
 * make audio late. Tones that would fall before the start of the stream are left out.
 *
 * Three rows of large blocks at the bottom of each frame carry 32 bit numbers: the latest event
 * index, a frame counter, and the clock time of the frame if #GstAvSyncTestSrc:encode-clock-time
 * is set. Each row is split into 34 equal cells: a white start cell, the 32 bits MSB first (white
 * for 1), and a black end cell. The clock time row is all black when it is not encoded.
 *
 * ## End-to-end latency
 *
 * With #GstAvSyncTestSrc:encode-clock-time, the clock time row carries the time each frame is to
 * be rendered at, its running time plus the base time, in microseconds modulo 2^32. A detector in
 * another pipeline that uses the same clock, for example after a capture card that the output of
 * this pipeline is looped into, can then measure the time from here to there. This needs both
 * pipelines to use the same clock, such as the system clock on one machine or a PTP or NTP clock
 * across machines. The time is only known in PLAYING, frames generated before are not encoded.
 *
 * Either pad can be left unlinked. Its buffers are dropped and the other stream keeps running.
 * When not live, an unlinked stream pauses until its pad is linked, so it doesn't generate
 * buffers that nobody will see. It continues from where it stopped, so after linking it can
 * lag behind the other stream.
 *
 * ## Example pipelines
 * |[
 * gst-launch-1.0 avsynctestsrc name=src \
 *     src.video ! videoconvert ! autovideosink \
 *     src.audio ! audioconvert ! autoaudiosink
 * ]|
 * Watch and listen to both streams.
 *
 * |[
 * gst-launch-1.0 avsynctestsrc ! videoconvert ! autovideosink
 * ]|
 * Video only, with the audio pad left unlinked.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;

use std::sync::{Condvar, Mutex};

use crate::pattern;

mod audiosrc;
mod imp;
mod videosrc;

glib::wrapper! {
    pub struct AvSyncTestSrc(ObjectSubclass<imp::AvSyncTestSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "avsynctestsrc",
        gst::Rank::NONE,
        AvSyncTestSrc::static_type(),
    )
}

#[derive(Clone, Debug)]
struct Settings {
    interval: u64,
    flash_duration: u64,
    frequencies: Vec<f64>,
    volume: f64,
    is_live: bool,
    encode_clock_time: bool,
    audio_offset: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval: pattern::DEFAULT_INTERVAL,
            flash_duration: pattern::DEFAULT_FLASH_DURATION,
            frequencies: pattern::DEFAULT_FREQUENCIES.to_vec(),
            volume: imp::DEFAULT_VOLUME,
            is_live: imp::DEFAULT_IS_LIVE,
            encode_clock_time: imp::DEFAULT_ENCODE_CLOCK_TIME,
            audio_offset: imp::DEFAULT_AUDIO_OFFSET,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Video = 0,
    Audio = 1,
}

#[derive(Debug, Default)]
struct Timing {
    framerate: Option<(u32, u32)>,
    flushing: [bool; 2],
    /// Added to the timestamps of both streams when live.
    live_offset: Option<u64>,
    clock_ids: [Option<gst::SingleShotClockId>; 2],
}

/// State shared between the bin and its two internal sources.
///
/// The audio events sit on the video frame grid, so the audio source waits until the video
/// source has negotiated its framerate. When not live, a source whose pad is not linked waits
/// until it is instead of generating buffers that would be dropped.
///
/// When live, both sources wait for the clock themselves and offset their timestamps by the same
/// running time. GstBaseSrc would otherwise offset each by the running time of its own first
/// buffer, which differs between the two.
#[derive(Debug, Default)]
struct Shared {
    settings: Mutex<Settings>,
    timing: Mutex<Timing>,
    timing_cond: Condvar,
}

impl Shared {
    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn set_framerate(&self, framerate: Option<(u32, u32)>) {
        self.timing.lock().unwrap().framerate = framerate;
        self.timing_cond.notify_all();
    }

    /// Blocks until the video framerate is known. Returns `None` when unblocked by a flush.
    fn wait_framerate(&self) -> Option<(u32, u32)> {
        let mut timing = self.timing.lock().unwrap();
        loop {
            if timing.flushing[Stream::Audio as usize] {
                return None;
            }
            if let Some(framerate) = timing.framerate {
                return Some(framerate);
            }
            timing = self.timing_cond.wait(timing).unwrap();
        }
    }

    /// Blocks until the bin's pad for `src` is linked. Returns `false` when unblocked by a flush.
    fn wait_linked(&self, stream: Stream, src: &gst::Element) -> bool {
        let mut timing = self.timing.lock().unwrap();
        loop {
            if timing.flushing[stream as usize] {
                return false;
            }
            if is_linked(src) {
                return true;
            }
            timing = self.timing_cond.wait(timing).unwrap();
        }
    }

    /// Wakes up sources waiting in [`Shared::wait_linked`].
    fn notify_linked(&self) {
        // Taking the lock makes sure a waiter is either before its check or already waiting
        drop(self.timing.lock().unwrap());
        self.timing_cond.notify_all();
    }

    fn set_flushing(&self, stream: Stream, flushing: bool) {
        let mut timing = self.timing.lock().unwrap();
        timing.flushing[stream as usize] = flushing;
        if flushing && let Some(id) = &timing.clock_ids[stream as usize] {
            id.unschedule();
        }
        drop(timing);
        self.timing_cond.notify_all();
    }

    fn reset_live_offset(&self) {
        self.timing.lock().unwrap().live_offset = None;
    }

    /// The offset for live timestamps, the running time of the first live buffer of either
    /// stream.
    fn live_offset(&self, src: &gst::Element) -> u64 {
        let mut timing = self.timing.lock().unwrap();
        *timing.live_offset.get_or_insert_with(|| {
            src.current_running_time()
                .map_or(0, gst::ClockTime::nseconds)
        })
    }

    /// Blocks until the clock reaches `running_time`. Returns `false` when unblocked by a flush.
    fn wait_until(&self, stream: Stream, src: &gst::Element, running_time: u64) -> bool {
        let (Some(clock), Some(base_time)) = (src.clock(), src.base_time()) else {
            return true;
        };
        let id = clock.new_single_shot_id(base_time + gst::ClockTime::from_nseconds(running_time));

        {
            let mut timing = self.timing.lock().unwrap();
            if timing.flushing[stream as usize] {
                return false;
            }
            timing.clock_ids[stream as usize] = Some(id.clone());
        }

        let (res, _) = id.wait();
        self.timing.lock().unwrap().clock_ids[stream as usize] = None;

        res != Err(gst::ClockError::Unscheduled)
    }
}

/// The bin's ghost pad for one of the internal sources.
fn ghost_pad(src: &gst::Element) -> Option<gst::Pad> {
    let proxy = src
        .static_pad("src")?
        .peer()?
        .downcast::<gst::ProxyPad>()
        .ok()?;
    proxy.internal().map(|pad| pad.upcast())
}

/// Whether the bin's pad for `src` is linked to anything.
fn is_linked(src: &gst::Element) -> bool {
    ghost_pad(src).is_some_and(|pad| pad.is_linked())
}
