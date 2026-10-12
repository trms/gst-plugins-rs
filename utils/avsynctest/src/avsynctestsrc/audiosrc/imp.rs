// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use super::super::{Settings, Shared, Stream};
use crate::pattern::{self, EventGrid};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "avsynctestsrc-audio",
        gst::DebugColorFlags::empty(),
        Some("A/V Sync Test Source, audio"),
    )
});

const BUFFER_DURATION: u64 = 10_000_000;

fn samples_per_buffer(rate: u32) -> u64 {
    pattern::duration_to_samples(BUFFER_DURATION, rate).max(1)
}

#[derive(Default)]
struct State {
    settings: Settings,
    info: Option<gst_audio::AudioInfo>,
    next_sample: u64,
    /// Start of the last seek, applied on the next buffer.
    seek_start: Option<u64>,
    stop_time: Option<u64>,
}

#[derive(Default)]
pub struct AudioSrc {
    shared: OnceLock<Arc<Shared>>,
    state: Mutex<State>,
}

impl AudioSrc {
    pub(super) fn set_shared(&self, shared: Arc<Shared>) {
        self.shared.set(shared).unwrap();
    }

    fn shared(&self) -> &Shared {
        self.shared.get().unwrap()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AudioSrc {
    const NAME: &'static str = "GstAvSyncTestAudioSrc";
    type Type = super::AudioSrc;
    type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for AudioSrc {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_live(false);
        obj.set_format(gst::Format::Time);
    }
}

impl GstObjectImpl for AudioSrc {}

impl ElementImpl for AudioSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "A/V Sync Test Audio Source",
                "Source/Audio",
                "Audio half of avsynctestsrc",
                "Ray Tiley <ray.tiley@trms.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &super::caps(),
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            self.obj()
                .set_live(self.shared().settings.lock().unwrap().is_live);
        }

        self.parent_change_state(transition)
    }
}

impl BaseSrcImpl for AudioSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State {
            settings: self.shared().settings(),
            ..Default::default()
        };
        self.shared().set_flushing(Stream::Audio, false);

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_audio::AudioInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {caps}"))?;

        let mut state = self.state.lock().unwrap();

        let nyquist = info.rate() as f64 / 2.0;
        if let Some(freq) = state.settings.frequencies.iter().find(|&&f| f >= nyquist) {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                [
                    "Frequency {freq} Hz is not below half the sample rate {}",
                    info.rate()
                ]
            );
            return Err(gst::loggable_error!(CAT, "Frequency too high for rate"));
        }

        gst::debug!(CAT, imp = self, "Configuring for caps {caps}");

        let old_rate = state.info.as_ref().map(|info| info.rate());
        if let Some(old_rate) = old_rate
            && old_rate != info.rate()
        {
            state.next_sample = pattern::sample_at_or_after(
                pattern::sample_time(state.next_sample, old_rate),
                info.rate(),
            );
        }

        let latency_changed = old_rate != Some(info.rate());
        state.info = Some(info);
        drop(state);

        if latency_changed {
            let _ = self
                .obj()
                .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
        }

        Ok(())
    }

    fn fixate(&self, mut caps: gst::Caps) -> gst::Caps {
        caps.truncate();
        {
            let caps = caps.make_mut();
            let s = caps.structure_mut(0).unwrap();
            s.fixate_field_nearest_int("rate", 48000);
            s.fixate_field_nearest_int("channels", 2);

            // Required for more than two channels
            let channels = s.get::<i32>("channels").unwrap_or(2);
            if channels > 2 && !s.has_field("channel-mask") {
                s.set(
                    "channel-mask",
                    gst::Bitmask::new(gst_audio::AudioChannelPosition::fallback_mask(
                        channels as u32,
                    )),
                );
            }
        }

        self.parent_fixate(caps)
    }

    fn is_seekable(&self) -> bool {
        !self.obj().is_live()
    }

    fn do_seek(&self, segment: &mut gst::Segment) -> bool {
        let Some(segment) = segment.downcast_mut::<gst::ClockTime>() else {
            return false;
        };

        if segment.rate() < 0.0 {
            gst::error!(CAT, imp = self, "Reverse playback not supported");
            return false;
        }

        let mut state = self.state.lock().unwrap();
        state.seek_start = Some(segment.start().unwrap_or(gst::ClockTime::ZERO).nseconds());
        state.stop_time = segment.stop().map(gst::ClockTime::nseconds);

        true
    }

    fn times(&self, _buffer: &gst::BufferRef) -> (Option<gst::ClockTime>, Option<gst::ClockTime>) {
        // When live, create() waits for the clock itself. See Shared.
        (None, None)
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Latency(q) = query.view_mut()
            && self.obj().is_live()
        {
            let Some(rate) = self.state.lock().unwrap().info.as_ref().map(|i| i.rate()) else {
                return false;
            };

            // A buffer is complete one buffer after its timestamp. There is no upper limit, as
            // timestamps are on the grid whenever the buffer gets made, so nothing is dropped
            // however much latency the pipeline configures
            let latency =
                gst::ClockTime::from_nseconds(pattern::sample_time(samples_per_buffer(rate), rate));
            gst::debug!(CAT, imp = self, "Reporting latency {latency}");
            q.set(true, latency, None);
            return true;
        }

        BaseSrcImplExt::parent_query(self, query)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        self.shared().set_flushing(Stream::Audio, true);
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        self.shared().set_flushing(Stream::Audio, false);
        Ok(())
    }
}

impl PushSrcImpl for AudioSrc {
    fn create(
        &self,
        _buffer: Option<&mut gst::BufferRef>,
    ) -> Result<CreateSuccess, gst::FlowError> {
        // Events are on the video frame grid, so wait for video to negotiate first
        let Some((fps_n, fps_d)) = self.shared().wait_framerate() else {
            gst::debug!(CAT, imp = self, "Flushing while waiting for framerate");
            return Err(gst::FlowError::Flushing);
        };

        // Unlinked output is dropped by the bin, don't generate it faster than real time
        if !self.obj().is_live()
            && !self
                .shared()
                .wait_linked(Stream::Audio, self.obj().upcast_ref())
        {
            gst::debug!(CAT, imp = self, "Flushing while waiting to be linked");
            return Err(gst::FlowError::Flushing);
        }

        let volume = self.shared().settings.lock().unwrap().volume;

        let mut state = self.state.lock().unwrap();

        let Some(info) = state.info.clone() else {
            gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Not negotiated"]);
            return Err(gst::FlowError::NotNegotiated);
        };
        let rate = info.rate();
        let grid = EventGrid::new(state.settings.interval, fps_n, fps_d);

        if let Some(start) = state.seek_start.take() {
            state.next_sample = pattern::sample_at_or_after(start, rate);
        }

        let start = state.next_sample;
        let mut end = start + samples_per_buffer(rate);
        if let Some(stop) = state.stop_time {
            let stop = pattern::sample_at_or_after(stop, rate);
            if start >= stop {
                gst::debug!(CAT, imp = self, "Reached stop time");
                return Err(gst::FlowError::Eos);
            }
            end = end.min(stop);
        }

        let tones = grid.tones_in_offset(
            start,
            end,
            rate,
            state.settings.flash_duration,
            &state.settings.frequencies,
            state.settings.audio_offset,
        );

        for tone in tones.iter().filter(|t| (start..end).contains(&t.start)) {
            gst::debug!(
                CAT,
                imp = self,
                "Tone of {} Hz at sample {}, {}",
                tone.frequency,
                tone.start,
                gst::ClockTime::from_nseconds(pattern::sample_time(tone.start, rate))
            );
        }

        state.next_sample = end;
        drop(state);

        let offset = if self.obj().is_live() {
            let src = self.obj();
            let offset = self.shared().live_offset(src.upcast_ref());
            let pts = pattern::sample_time(start, rate) + offset;
            if !self
                .shared()
                .wait_until(Stream::Audio, src.upcast_ref(), pts)
            {
                gst::debug!(CAT, imp = self, "Flushing while waiting for the clock");
                return Err(gst::FlowError::Flushing);
            }
            offset
        } else {
            0
        };

        let bpf = info.bpf() as usize;
        let width = bpf / info.channels() as usize;
        let format = info.format();

        let mut buffer = gst::Buffer::with_size((end - start) as usize * bpf)
            .map_err(|_| gst::FlowError::Error)?;
        {
            let buffer = buffer.get_mut().unwrap();
            let pts = pattern::sample_time(start, rate);
            buffer.set_pts(gst::ClockTime::from_nseconds(pts + offset));
            buffer.set_duration(gst::ClockTime::from_nseconds(
                pattern::sample_time(end, rate) - pts,
            ));
            buffer.set_offset(start);
            buffer.set_offset_end(end);

            let mut map = buffer.map_writable().map_err(|_| gst::FlowError::Error)?;

            for (sample, frame) in (start..end).zip(map.chunks_exact_mut(bpf)) {
                let value = volume * tones.iter().map(|t| t.value(sample)).sum::<f64>();

                let (first, rest) = frame.split_at_mut(width);
                encode(format, value, first);
                for channel in rest.chunks_exact_mut(width) {
                    channel.copy_from_slice(first);
                }
            }
        }

        Ok(CreateSuccess::NewBuffer(buffer))
    }
}

/// Full scale is 2^(bits - 1), like audioconvert. The casts saturate at +1.0.
fn encode(format: gst_audio::AudioFormat, value: f64, out: &mut [u8]) {
    match format {
        gst_audio::AUDIO_FORMAT_S16 => {
            out.copy_from_slice(&((value * 32768.0).round() as i16).to_ne_bytes())
        }
        gst_audio::AUDIO_FORMAT_S32 => {
            out.copy_from_slice(&((value * 2_147_483_648.0).round() as i32).to_ne_bytes())
        }
        gst_audio::AUDIO_FORMAT_F32 => out.copy_from_slice(&(value as f32).to_ne_bytes()),
        _ => unreachable!(),
    }
}
