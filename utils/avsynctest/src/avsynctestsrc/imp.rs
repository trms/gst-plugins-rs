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

use std::sync::{Arc, LazyLock};

use super::Shared;
use super::audiosrc::AudioSrc;
use super::videosrc::VideoSrc;
use crate::pattern;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "avsynctestsrc",
        gst::DebugColorFlags::empty(),
        Some("A/V Sync Test Source"),
    )
});

pub(super) const DEFAULT_VOLUME: f64 = 0.8;
pub(super) const DEFAULT_IS_LIVE: bool = false;
pub(super) const DEFAULT_ENCODE_CLOCK_TIME: bool = false;
pub(super) const DEFAULT_AUDIO_OFFSET: i64 = 0;

const MIN_FLASH_DURATION: u64 = 2 * pattern::RAMP_DURATION;

pub struct AvSyncTestSrc {
    shared: Arc<Shared>,
    video_src: VideoSrc,
    audio_src: AudioSrc,
}

#[glib::object_subclass]
impl ObjectSubclass for AvSyncTestSrc {
    const NAME: &'static str = "GstAvSyncTestSrc";
    type Type = super::AvSyncTestSrc;
    type ParentType = gst::Bin;

    fn new() -> Self {
        let shared = Arc::new(Shared::default());

        Self {
            video_src: VideoSrc::new(shared.clone()),
            audio_src: AudioSrc::new(shared.clone()),
            shared,
        }
    }
}

impl ObjectImpl for AvSyncTestSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("interval")
                    .nick("Interval")
                    .blurb("Time between events, in nanoseconds")
                    .minimum(pattern::MIN_INTERVAL)
                    .default_value(pattern::DEFAULT_INTERVAL)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("flash-duration")
                    .nick("Flash Duration")
                    .blurb(
                        "Length of each flash and tone, in nanoseconds. \
                         At most half the interval",
                    )
                    .minimum(MIN_FLASH_DURATION)
                    .default_value(pattern::DEFAULT_FLASH_DURATION)
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("frequencies")
                    .element_spec(
                        &glib::ParamSpecDouble::builder("frequency")
                            .nick("Frequency")
                            .blurb("Tone frequency in Hz")
                            .minimum(1.0)
                            .default_value(pattern::DEFAULT_FREQUENCIES[0])
                            .build(),
                    )
                    .nick("Frequencies")
                    .blurb("Tone frequencies in Hz, used in turn by event index, at least 200 Hz apart")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("volume")
                    .nick("Volume")
                    .blurb("Peak amplitude of the tone")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_VOLUME)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("is-live")
                    .nick("Is Live")
                    .blurb("Pace output against the clock")
                    .default_value(DEFAULT_IS_LIVE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("encode-clock-time")
                    .nick("Encode Clock Time")
                    .blurb("Encode the clock time of each frame, for measuring end-to-end latency")
                    .default_value(DEFAULT_ENCODE_CLOCK_TIME)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecInt64::builder("audio-offset")
                    .nick("Audio Offset")
                    .blurb(
                        "Time of each tone after its flash, in nanoseconds. Negative for early \
                         audio, to test detectors and pipelines",
                    )
                    .minimum(-i64::MAX)
                    .default_value(DEFAULT_AUDIO_OFFSET)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.shared.settings.lock().unwrap();

        match pspec.name() {
            "interval" => {
                settings.interval = value.get().expect("type checked upstream");
            }
            "flash-duration" => {
                settings.flash_duration = value.get().expect("type checked upstream");
            }
            "frequencies" => {
                settings.frequencies = value
                    .get::<gst::Array>()
                    .expect("type checked upstream")
                    .iter()
                    .map(|v| v.get::<f64>().expect("type checked upstream"))
                    .collect();
            }
            "volume" => {
                settings.volume = value.get().expect("type checked upstream");
            }
            "is-live" => {
                settings.is_live = value.get().expect("type checked upstream");
            }
            "encode-clock-time" => {
                settings.encode_clock_time = value.get().expect("type checked upstream");
            }
            "audio-offset" => {
                settings.audio_offset = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.shared.settings.lock().unwrap();

        match pspec.name() {
            "interval" => settings.interval.to_value(),
            "flash-duration" => settings.flash_duration.to_value(),
            "frequencies" => settings
                .frequencies
                .iter()
                .map(|f| f.to_send_value())
                .collect::<gst::Array>()
                .to_value(),
            "volume" => settings.volume.to_value(),
            "is-live" => settings.is_live.to_value(),
            "encode-clock-time" => settings.encode_clock_time.to_value(),
            "audio-offset" => settings.audio_offset.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();

        obj.add_many([
            self.video_src.upcast_ref::<gst::Element>(),
            self.audio_src.upcast_ref(),
        ])
        .unwrap();

        for (name, src) in [
            ("video", self.video_src.upcast_ref::<gst::Element>()),
            ("audio", self.audio_src.upcast_ref()),
        ] {
            let templ = obj.pad_template(name).unwrap();
            let pad = gst::GhostPad::builder_from_template_with_target(
                &templ,
                &src.static_pad("src").unwrap(),
            )
            .unwrap()
            .build();

            // Drop buffers while our pad is unlinked so the other stream keeps running instead
            // of the source erroring out with not-linked
            let ghost = pad.downgrade();
            src.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, _| {
                    if ghost.upgrade().is_some_and(|ghost| ghost.is_linked()) {
                        gst::PadProbeReturn::Ok
                    } else {
                        gst::PadProbeReturn::Drop
                    }
                },
            );

            let shared = self.shared.clone();
            pad.connect_linked(move |pad, peer| {
                gst::debug!(CAT, obj = pad, "Linked to {}", peer.name());
                shared.notify_linked();
            });

            obj.add_pad(&pad).unwrap();
        }
    }
}

impl GstObjectImpl for AvSyncTestSrc {}

impl ElementImpl for AvSyncTestSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "A/V Sync Test Source",
                "Source/Audio/Video",
                "Generates aligned video flashes and audio tones for measuring A/V sync",
                "Ray Tiley <ray.tiley@trms.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "video",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &super::videosrc::caps(),
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "audio",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &super::audiosrc::caps(),
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
            let settings = self.shared.settings();

            gst::debug!(CAT, imp = self, "Starting with {settings:?}");

            if settings.flash_duration > settings.interval / 2 {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["flash-duration must be at most half of interval"]
                );
                return Err(gst::StateChangeError);
            }

            if let Err(err) = pattern::validate_frequencies(&settings.frequencies) {
                gst::element_imp_error!(self, gst::LibraryError::Settings, ["{err}"]);
                return Err(gst::StateChangeError);
            }

            self.shared.reset_live_offset();
        }

        self.parent_change_state(transition)
    }
}

impl BinImpl for AvSyncTestSrc {}
