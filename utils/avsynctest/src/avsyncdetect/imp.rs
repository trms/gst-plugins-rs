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
use gst_video::prelude::*;

use std::collections::VecDeque;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::analyzer::{
    self,
    audio::{self as audio_analyzer, ToneDetector},
    frames::{FrameCounter, FrameError},
    pairing::{self, Pair, Pairer},
    stats::{ClockRate, LinearFit, OffsetStats},
    video::FlashDetector,
};
use crate::pattern::{self, CODE_CELLS, CodeLayout, EventGrid};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "avsyncdetect",
        gst::DebugColorFlags::empty(),
        Some("A/V Sync Detector"),
    )
});

const DEFAULT_POST_MESSAGES: bool = true;
const DEFAULT_WINDOW: u32 = 10;
const MIN_WINDOW: u32 = 2;
const MAX_WINDOW: u32 = 10_000;
// EBU R37 limits for a single stage
const DEFAULT_TOLERANCE_EARLY: u64 = 40_000_000;
const DEFAULT_TOLERANCE_LATE: u64 = 60_000_000;

#[derive(Clone, Debug)]
struct Settings {
    interval: u64,
    frequencies: Vec<f64>,
    post_messages: bool,
    window: u32,
    tolerance_early: u64,
    tolerance_late: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval: pattern::DEFAULT_INTERVAL,
            frequencies: pattern::DEFAULT_FREQUENCIES.to_vec(),
            post_messages: DEFAULT_POST_MESSAGES,
            window: DEFAULT_WINDOW,
            tolerance_early: DEFAULT_TOLERANCE_EARLY,
            tolerance_late: DEFAULT_TOLERANCE_LATE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Video,
    Audio,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Video => "video",
            Stream::Audio => "audio",
        }
    }
}

/// Result of a latency query upstream of a sink pad.
#[derive(Clone, Copy, Debug)]
struct UpstreamLatency {
    live: bool,
    min: gst::ClockTime,
    max: Option<gst::ClockTime>,
}

#[derive(Default)]
struct VideoState {
    info: Option<gst_video::VideoInfo>,
    segment: gst::FormattedSegment<gst::ClockTime>,
    detector: FlashDetector,
    /// Index and stream time of the last flash.
    last_event: Option<(u64, u64)>,
}

#[derive(Default)]
struct AudioState {
    info: Option<gst_audio::AudioInfo>,
    segment: gst::FormattedSegment<gst::ClockTime>,
    frequencies: Vec<f64>,
    detector: Option<ToneDetector>,
    /// Index and stream time of the last tone.
    last_event: Option<(u64, u64)>,
}

/// An event that can be paired, with its running time.
#[derive(Clone, Copy, Debug)]
enum Detected {
    Video {
        index: u64,
        time: u64,
        /// End-to-end latency of the frame, if its clock time is encoded.
        latency: Option<i64>,
    },
    Audio {
        /// Estimated, only the residue is used for pairing.
        index: u64,
        residue: u64,
        time: u64,
        stream_time: u64,
        rate: u32,
    },
}

/// A video frame, for counting dropped and duplicated frames.
#[derive(Clone, Copy, Debug)]
struct Frame {
    counter: Option<u32>,
    pts: gst::ClockTime,
    running_time: Option<gst::ClockTime>,
}

/// What was found in one buffer.
#[derive(Default)]
struct Analysis {
    messages: Vec<gst::Structure>,
    detected: Vec<Detected>,
    /// Running time up to which all events have been reported.
    position: Option<u64>,
    /// Running time of the start of the buffer.
    running_time: Option<u64>,
    frame: Option<Frame>,
    /// End-to-end latency of a video frame.
    latency: Option<i64>,
}

/// Pairs the events of both streams and keeps statistics. This is kept over flushes, and after
/// stopping so that the properties can still be read.
struct Measurement {
    interval: u64,
    framerate: Option<(u32, u32)>,
    pairer: Pairer,
    offsets: OffsetStats,
    drift: LinearFit,
    video_rate: ClockRate,
    audio_rate: ClockRate,
    frames: FrameCounter,
    /// Clock running time on arrival minus buffer running time, per stream.
    lateness: [OffsetStats; 2],
    /// End-to-end latency per stream, from the encoded clock time.
    latency: [OffsetStats; 2],
    /// Latency of the last flashes by index, for the audio latency of their pairs.
    event_latencies: VecDeque<(u64, i64)>,
    /// Pairs are never further apart than this many events.
    max_event_latencies: usize,
    /// Latency of the pipeline, from the latency event.
    pipeline_latency: Option<gst::ClockTime>,
    /// Latency upstream of each sink pad, from the latency query.
    upstream_latency: [Option<UpstreamLatency>; 2],
    within_tolerance: bool,
    /// Streams that have caps, and which of them are at EOS.
    active: [bool; 2],
    eos: [bool; 2],
    summary_posted: bool,
}

impl Default for Measurement {
    fn default() -> Self {
        Measurement::new(&Settings::default())
    }
}

impl Measurement {
    fn new(settings: &Settings) -> Self {
        let window = settings.window as usize;
        let modulus = settings.frequencies.len().max(1);

        Measurement {
            interval: settings.interval,
            framerate: None,
            pairer: Pairer::new(settings.interval, modulus as u64),
            offsets: OffsetStats::default(),
            drift: LinearFit::new(window),
            video_rate: ClockRate::new(window),
            audio_rate: ClockRate::new(window),
            frames: FrameCounter::default(),
            lateness: Default::default(),
            latency: Default::default(),
            event_latencies: VecDeque::new(),
            max_event_latencies: modulus,
            pipeline_latency: None,
            upstream_latency: [None; 2],
            within_tolerance: true,
            active: [false; 2],
            eos: [false; 2],
            summary_posted: false,
        }
    }

    /// Running times start again after a flush, forget what depends on them.
    fn flush(&mut self, stream: Stream) {
        self.pairer.flush();
        self.event_latencies.clear();
        self.drift.clear();
        match stream {
            Stream::Video => {
                self.video_rate.clear();
                self.frames.resync();
            }
            Stream::Audio => self.audio_rate.clear(),
        }
        self.eos = [false; 2];
        self.summary_posted = false;
    }

    /// Time of a video event from the start of the stream, as the source generates it.
    fn nominal_time(&self, index: u64) -> u64 {
        match self.framerate {
            Some((fps_n, fps_d)) => EventGrid::new(self.interval, fps_n, fps_d).event_time(index),
            // Without video the frame it is on is unknown
            None => index.saturating_mul(self.interval),
        }
    }

    fn push(&mut self, detected: Detected) -> Option<Pair> {
        match detected {
            Detected::Video {
                index,
                time,
                latency,
            } => {
                if let Some(latency) = latency {
                    if self.event_latencies.len() == self.max_event_latencies {
                        self.event_latencies.pop_front();
                    }
                    self.event_latencies.push_back((index, latency));
                }
                self.video_rate.push(self.nominal_time(index), time);
                self.pairer.push_video(pairing::VideoEvent { index, time })
            }
            Detected::Audio {
                index,
                residue,
                time,
                stream_time,
                rate,
            } => {
                let nominal = pattern::sample_time(
                    pattern::sample_at_or_after(self.nominal_time(index), rate),
                    rate,
                );
                self.audio_rate.push(nominal, time);
                self.pairer.push_audio(pairing::AudioEvent {
                    residue,
                    time,
                    stream_time,
                })
            }
        }
    }

    fn advance(&mut self, stream: Stream, position: u64) {
        match stream {
            Stream::Video => self.pairer.advance_video(position),
            Stream::Audio => self.pairer.advance_audio(position),
        }
    }

    /// Change of the offset in nanoseconds per second.
    fn drift(&self) -> Option<f64> {
        self.drift
            .slope()
            .map(|slope| slope * pattern::SECOND as f64)
    }

    /// Records a pair. Returns its message, and a warning if it is the first offset outside the
    /// tolerance.
    fn record(
        &mut self,
        pair: &Pair,
        tolerance_early: u64,
        tolerance_late: u64,
        name: &str,
    ) -> (gst::Structure, Option<String>) {
        let offset = pair.offset();
        self.offsets.push(offset);
        self.drift.push(pair.video_time as f64, offset as f64);

        // Audio arrives here offset from the video it was generated with
        let video_latency = self
            .event_latencies
            .iter()
            .find(|(index, _)| *index == pair.index)
            .map(|(_, latency)| *latency);
        let audio_latency = video_latency.map(|latency| latency + offset);
        if let Some(latency) = audio_latency {
            self.latency[Stream::Audio as usize].push(latency);
        }

        let within = offset >= -(tolerance_early as i64) && offset <= tolerance_late as i64;
        let warning = (self.within_tolerance && !within).then(|| {
            let (relation, tolerance) = if offset > 0 {
                ("after", tolerance_late)
            } else {
                ("before", tolerance_early)
            };
            format!(
                "Audio is {} {relation} video at event {}, more than the tolerance of {}",
                gst::ClockTime::from_nseconds(offset.unsigned_abs()),
                pair.index,
                gst::ClockTime::from_nseconds(tolerance),
            )
        });
        self.within_tolerance = within;

        let s = gst::Structure::builder("avsync-pair")
            .field("element", name)
            .field("index", pair.index)
            .field(
                "video-running-time",
                gst::ClockTime::from_nseconds(pair.video_time),
            )
            .field(
                "audio-running-time",
                gst::ClockTime::from_nseconds(pair.audio_time),
            )
            .field("offset", offset)
            .field_if_some("drift", self.drift())
            .field("within-tolerance", within)
            .field_if_some("video-latency", video_latency)
            .field_if_some("audio-latency", audio_latency)
            .build();

        (s, warning)
    }

    /// Message for frames dropped or duplicated before `frame`.
    fn frame_error(name: &str, frame: &Frame, error: FrameError) -> gst::Structure {
        gst::Structure::builder("avsync-frame-error")
            .field("element", name)
            .field_if_some("frame-counter", frame.counter)
            .field("pts", frame.pts)
            .field_if_some("running-time", frame.running_time)
            .field("dropped", error.dropped)
            .field("duplicated", error.duplicated)
            .build()
    }

    fn summary(&self, name: &str) -> gst::Structure {
        let [video_lateness, audio_lateness] = &self.lateness;
        let [video_latency, audio_latency] = &self.latency;
        let [video_upstream, audio_upstream] = self.upstream_latency;

        gst::Structure::builder("avsync-summary")
            .field("element", name)
            .field("matched-events", self.offsets.count())
            .field("missed-audio", self.pairer.missed_audio())
            .field("missed-video", self.pairer.missed_video())
            .field("frames-dropped", self.frames.dropped())
            .field("frames-duplicated", self.frames.duplicated())
            .field_if_some("last-offset", self.offsets.last())
            .field_if_some("avg-offset", self.offsets.average())
            .field_if_some("min-offset", self.offsets.min())
            .field_if_some("max-offset", self.offsets.max())
            .field_if_some("drift", self.drift())
            .field_if_some("audio-clock-ppm", self.audio_rate.ppm())
            .field_if_some("video-clock-ppm", self.video_rate.ppm())
            .field_if_some("max-audio-lateness", audio_lateness.max())
            .field_if_some("max-video-lateness", video_lateness.max())
            .field_if_some("avg-audio-latency", audio_latency.average())
            .field_if_some("avg-video-latency", video_latency.average())
            .field_if_some("max-audio-latency", audio_latency.max())
            .field_if_some("max-video-latency", video_latency.max())
            .field_if_some("pipeline-latency", self.pipeline_latency)
            .field_if_some("audio-upstream-latency", audio_upstream.map(|u| u.min))
            .field_if_some("video-upstream-latency", video_upstream.map(|u| u.min))
            .build()
    }
}

pub struct AvSyncDetect {
    video_sink: gst::Pad,
    video_src: gst::Pad,
    audio_sink: gst::Pad,
    audio_src: gst::Pad,
    settings: Mutex<Settings>,
    video: Mutex<VideoState>,
    audio: Mutex<AudioState>,
    measurement: Mutex<Measurement>,
    /// Whether running times of the clock can be compared with buffer running times.
    playing: AtomicBool,
}

impl AvSyncDetect {
    fn chain(
        &self,
        stream: Stream,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let playing = self.playing.load(Ordering::SeqCst);
        let arrival = playing.then(|| self.obj().current_running_time()).flatten();
        let base_time = playing.then(|| self.obj().base_time()).flatten();

        let mut analysis = Analysis::default();
        match stream {
            Stream::Video => self.analyze_video(&buffer, base_time, &mut analysis),
            Stream::Audio => self.analyze_audio(&buffer, &mut analysis),
        };
        self.measure(stream, arrival, analysis);

        match stream {
            Stream::Video => self.video_src.push(buffer),
            Stream::Audio => self.audio_src.push(buffer),
        }
    }

    /// Pairs the events of a buffer and posts all messages for it. `arrival` is the clock's
    /// running time when the buffer arrived.
    fn measure(&self, stream: Stream, arrival: Option<gst::ClockTime>, analysis: Analysis) {
        let (post_messages, interval, tolerance_early, tolerance_late) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.post_messages,
                settings.interval,
                settings.tolerance_early,
                settings.tolerance_late,
            )
        };
        let obj = self.obj();
        let name = obj.name();
        let element = |s| gst::message::Element::builder(s).src(&*obj).build();

        let mut messages = vec![];
        if post_messages {
            messages.extend(analysis.messages.into_iter().map(element));
        }

        let mut pairs = vec![];
        let mut warnings = vec![];
        {
            let mut measurement = self.measurement.lock().unwrap();

            if let (Some(arrival), Some(running_time)) = (arrival, analysis.running_time) {
                let lateness = arrival.nseconds() as i64 - running_time as i64;
                measurement.lateness[stream as usize].push(lateness);
            }
            if let Some(latency) = analysis.latency {
                measurement.latency[stream as usize].push(latency);
            }

            if let Some(frame) = analysis.frame
                && let Some(error) = measurement.frames.push(frame.counter)
            {
                gst::debug!(
                    CAT,
                    imp = self,
                    "{} frames dropped and {} duplicated before {}",
                    error.dropped,
                    error.duplicated,
                    frame.pts
                );
                if post_messages {
                    messages.push(element(Measurement::frame_error(&name, &frame, error)));
                }
            }

            for detected in analysis.detected {
                pairs.extend(measurement.push(detected));
            }
            if let Some(position) = analysis.position {
                measurement.advance(stream, position);
            }

            for pair in &pairs {
                let (s, warning) = measurement.record(pair, tolerance_early, tolerance_late, &name);
                gst::debug!(CAT, imp = self, "Paired {pair:?}, offset {}", pair.offset());
                if post_messages {
                    messages.push(element(s));
                }
                warnings.extend(warning);
            }
        }

        for pair in &pairs {
            self.correct_audio_index(pair, interval);
        }

        // Posted with no lock held, as bus handlers may read our properties
        for msg in messages {
            let _ = obj.post_message(msg);
        }
        for warning in warnings {
            gst::element_imp_warning!(self, gst::LibraryError::Failed, ["{warning}"]);
        }
    }

    /// The paired video event gives the full index of an audio event, from which later tones
    /// are counted.
    fn correct_audio_index(&self, pair: &Pair, interval: u64) {
        let corrected = {
            let mut state = self.audio.lock().unwrap();
            let modulus = state.frequencies.len() as u64;
            let Some((index, time)) = state.last_event else {
                return;
            };
            if modulus == 0 {
                return;
            }

            let corrected = analyzer::estimate_index(
                Some((pair.index, pair.audio_stream_time)),
                time,
                interval,
                index % modulus,
                modulus,
            );
            state.last_event = Some((corrected, time));
            (corrected != index).then_some((index, corrected))
        };

        if let Some((index, corrected)) = corrected {
            gst::debug!(
                CAT,
                imp = self,
                "Audio event index {index} corrected to {corrected}"
            );
            // Its clock rate was measured against the wrong nominal times
            self.measurement.lock().unwrap().audio_rate.clear();
        }
    }

    /// `base_time` is set once running times can be converted to clock times.
    fn analyze_video(
        &self,
        buffer: &gst::BufferRef,
        base_time: Option<gst::ClockTime>,
        analysis: &mut Analysis,
    ) -> Option<()> {
        let interval = self.settings.lock().unwrap().interval;

        let mut state = self.video.lock().unwrap();
        let VideoState {
            info,
            segment,
            detector,
            last_event,
        } = &mut *state;

        let info = info.as_ref()?;
        let pts = buffer.pts()?;
        let running_time = segment.to_running_time(pts);
        analysis.position = running_time.map(gst::ClockTime::nseconds);
        analysis.running_time = analysis.position;

        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, info)
            .inspect_err(|_| gst::warning!(CAT, imp = self, "Failed to map video frame"))
            .ok()?;
        let picture = Picture::new(&frame)?;

        let counter = picture.code_row(pattern::COUNTER_ROW);
        if counter.is_none() {
            gst::log!(CAT, imp = self, "Can't read frame counter at {pts}");
        }
        analysis.frame = Some(Frame {
            counter,
            pts,
            running_time,
        });

        // From the clock time the source encoded to the clock time of this frame here
        let latency = picture
            .code_row(pattern::CLOCK_ROW)
            .zip(base_time.zip(running_time))
            .map(|(code, (base_time, running_time))| {
                pattern::time_since_clock_code((base_time + running_time).nseconds(), code)
            });
        analysis.latency = latency;

        let brightness = picture.flash_brightness();
        if !detector.push(pts.nseconds(), brightness) {
            return None;
        }

        let index = match picture.code_row(pattern::EVENT_ROW) {
            Some(index) => index as u64,
            None => {
                gst::debug!(CAT, imp = self, "Can't read event index at {pts}");
                analyzer::estimate_index(*last_event, stream_time(segment, pts), interval, 0, 1)
            }
        };
        *last_event = Some((index, stream_time(segment, pts)));

        gst::debug!(
            CAT,
            imp = self,
            "Flash {index} at {pts}, running time {}, brightness {brightness:.3}",
            running_time.display()
        );

        if let Some(time) = running_time {
            analysis.detected.push(Detected::Video {
                index,
                time: time.nseconds(),
                latency,
            });
        }
        analysis.messages.push(
            gst::Structure::builder("avsync-video-event")
                .field("element", self.obj().name())
                .field("index", index)
                .field_if_some("frame-counter", counter)
                .field("pts", pts)
                .field_if_some("running-time", running_time)
                .field("brightness", brightness)
                .field_if_some("latency", latency)
                .build(),
        );

        Some(())
    }

    fn analyze_audio(&self, buffer: &gst::BufferRef, analysis: &mut Analysis) -> Option<()> {
        let interval = self.settings.lock().unwrap().interval;

        let mut state = self.audio.lock().unwrap();
        let AudioState {
            info,
            segment,
            frequencies,
            detector,
            last_event,
        } = &mut *state;

        let (Some(info), Some(detector)) = (info.as_ref(), detector.as_mut()) else {
            return None;
        };
        let map = buffer
            .map_readable()
            .inspect_err(|_| gst::warning!(CAT, imp = self, "Failed to map audio buffer"))
            .ok()?;

        let samples = mix_down(info, &map);
        let events = detector.push(buffer.pts().map(gst::ClockTime::nseconds), &samples);

        if let Some(pts) = buffer.pts() {
            analysis.running_time = segment.to_running_time(pts).map(gst::ClockTime::nseconds);
            let end = pts
                + gst::ClockTime::from_nseconds(pattern::sample_time(
                    samples.len() as u64,
                    info.rate(),
                ));
            // Tones up to the report delay before the end may still be reported, so the
            // position is a timestamp before conversion to running time
            let reportable = end.saturating_sub(gst::ClockTime::from_nseconds(
                audio_analyzer::MAX_REPORT_DELAY,
            ));
            analysis.position = segment
                .to_running_time(reportable)
                .map(gst::ClockTime::nseconds);
        }

        for event in events {
            let pts = gst::ClockTime::from_nseconds(event.time);
            let frequency = frequencies[event.frequency_index];
            let running_time = segment.to_running_time(pts);
            let time = stream_time(segment, pts);
            let residue = event.frequency_index as u64;
            let index = analyzer::estimate_index(
                *last_event,
                time,
                interval,
                residue,
                frequencies.len() as u64,
            );
            *last_event = Some((index, time));

            gst::debug!(
                CAT,
                imp = self,
                "Tone {index} of {frequency} Hz at {pts}, running time {}, level {:.3}, \
                 noise floor {:.4}",
                running_time.display(),
                event.level,
                event.noise_floor
            );

            if let Some(running_time) = running_time {
                analysis.detected.push(Detected::Audio {
                    index,
                    residue,
                    time: running_time.nseconds(),
                    stream_time: time,
                    rate: info.rate(),
                });
            }
            analysis.messages.push(
                gst::Structure::builder("avsync-audio-event")
                    .field("element", self.obj().name())
                    .field("index", index)
                    .field("frequency", frequency)
                    .field("pts", pts)
                    .field_if_some("running-time", running_time)
                    .field("level", event.level)
                    .field("noise-floor", event.noise_floor)
                    .build(),
            );
        }

        Some(())
    }

    fn sink_event(&self, stream: Stream, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        match event.view() {
            EventView::Caps(c) => {
                if !self.set_caps(stream, c.caps()) {
                    return false;
                }
            }
            EventView::Segment(s) => {
                let Ok(segment) = s.segment().clone().downcast::<gst::ClockTime>() else {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["Only time segments are supported"]
                    );
                    return false;
                };
                match stream {
                    Stream::Video => {
                        let mut state = self.video.lock().unwrap();
                        let changed = state.segment != segment;
                        if changed {
                            // A flash may straddle the jump of a seek without flushing
                            state.detector.reset();
                        }
                        state.segment = segment;
                        drop(state);
                        if changed {
                            // And the counter jumps
                            self.measurement.lock().unwrap().frames.resync();
                        }
                    }
                    // The tone detector resets itself on a timestamp discontinuity
                    Stream::Audio => self.audio.lock().unwrap().segment = segment,
                }
            }
            EventView::FlushStop(_) => {
                self.reset(stream);
                self.measurement.lock().unwrap().flush(stream);
            }
            EventView::Gap(g) => {
                let (timestamp, duration) = g.get();
                let end = timestamp + duration.unwrap_or(gst::ClockTime::ZERO);
                let running_time = match stream {
                    Stream::Video => {
                        let mut state = self.video.lock().unwrap();
                        // A flash in progress is not continued after it
                        state.detector.reset();
                        let running_time = state.segment.to_running_time(end);
                        drop(state);
                        // The frames it covers are neither dropped nor duplicated
                        self.measurement.lock().unwrap().frames.resync();
                        running_time
                    }
                    Stream::Audio => {
                        let mut state = self.audio.lock().unwrap();
                        // Silence, a tone in progress is not continued after it
                        if let Some(detector) = state.detector.as_mut() {
                            detector.reset();
                        }
                        state.segment.to_running_time(end)
                    }
                };
                if let Some(running_time) = running_time {
                    self.measurement
                        .lock()
                        .unwrap()
                        .advance(stream, running_time.nseconds());
                }
            }
            EventView::Eos(_) => self.stream_eos(stream),
            _ => (),
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    /// Records the latency upstream of this point as the query passes.
    fn src_query(&self, stream: Stream, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        if query.type_() != gst::QueryType::Latency {
            return gst::Pad::query_default(pad, Some(&*self.obj()), query);
        }

        let sink = match stream {
            Stream::Video => &self.video_sink,
            Stream::Audio => &self.audio_sink,
        };
        if !sink.peer_query(query) {
            return false;
        }

        if let gst::QueryView::Latency(q) = query.view() {
            let (live, min, max) = q.result();
            gst::debug!(
                CAT,
                imp = self,
                "{stream:?} upstream latency {min} to {}, live {live}",
                max.display()
            );
            self.measurement.lock().unwrap().upstream_latency[stream as usize] =
                Some(UpstreamLatency { live, min, max });
        }

        true
    }

    /// Posts the latency of the pipeline with the latency upstream of this point.
    fn src_event(&self, stream: Stream, pad: &gst::Pad, event: gst::Event) -> bool {
        if let gst::EventView::Latency(l) = event.view() {
            let latency = l.latency();
            gst::debug!(CAT, imp = self, "{stream:?} pipeline latency {latency}");

            let s = {
                let mut measurement = self.measurement.lock().unwrap();
                measurement.pipeline_latency = Some(latency);
                let upstream = measurement.upstream_latency[stream as usize];

                gst::Structure::builder("avsync-latency")
                    .field("element", self.obj().name())
                    .field("stream", stream.name())
                    .field("pipeline-latency", latency)
                    .field_if_some("live", upstream.map(|u| u.live))
                    .field_if_some("upstream-min-latency", upstream.map(|u| u.min))
                    .field_if_some("upstream-max-latency", upstream.and_then(|u| u.max))
                    .build()
            };

            if self.settings.lock().unwrap().post_messages {
                let obj = self.obj();
                let _ = obj.post_message(gst::message::Element::builder(s).src(&*obj).build());
            }
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    /// Posts the summary once all streams are at EOS.
    fn stream_eos(&self, stream: Stream) {
        let name = self.obj().name();
        let summary = {
            let mut measurement = self.measurement.lock().unwrap();
            measurement.eos[stream as usize] = true;
            // Events of the other stream can no longer be paired with this one
            match stream {
                Stream::Video => measurement.pairer.end_video(),
                Stream::Audio => measurement.pairer.end_audio(),
            }
            let done = measurement
                .active
                .iter()
                .zip(measurement.eos)
                .all(|(&active, eos)| !active || eos);
            if !done || measurement.summary_posted {
                return;
            }
            measurement.summary_posted = true;
            measurement.summary(&name)
        };

        gst::info!(CAT, imp = self, "Summary {summary}");
        let obj = self.obj();
        let _ = obj.post_message(gst::message::Element::builder(summary).src(&*obj).build());
    }

    fn set_caps(&self, stream: Stream, caps: &gst::CapsRef) -> bool {
        gst::debug!(CAT, imp = self, "Configuring {stream:?} for caps {caps}");

        match stream {
            Stream::Video => {
                let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
                    gst::error!(CAT, imp = self, "Invalid caps {caps}");
                    return false;
                };

                let fps = info.fps();
                {
                    let mut measurement = self.measurement.lock().unwrap();
                    measurement.framerate = (fps.numer() > 0 && fps.denom() > 0)
                        .then_some((fps.numer() as u32, fps.denom() as u32));
                    measurement.active[stream as usize] = true;
                }
                self.video.lock().unwrap().info = Some(info);
            }
            Stream::Audio => {
                let Ok(info) = gst_audio::AudioInfo::from_caps(caps) else {
                    gst::error!(CAT, imp = self, "Invalid caps {caps}");
                    return false;
                };

                let frequencies = self.settings.lock().unwrap().frequencies.clone();
                let nyquist = info.rate() as f64 / 2.0;
                if let Some(freq) = frequencies.iter().find(|&&f| f >= nyquist) {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Negotiation,
                        [
                            "Frequency {freq} Hz is not below half the sample rate {}",
                            info.rate()
                        ]
                    );
                    return false;
                }

                self.measurement.lock().unwrap().active[stream as usize] = true;

                let mut state = self.audio.lock().unwrap();
                state.detector = Some(ToneDetector::new(info.rate(), &frequencies));
                state.frequencies = frequencies;
                state.info = Some(info);
            }
        }

        true
    }

    fn reset(&self, stream: Stream) {
        match stream {
            Stream::Video => {
                let mut state = self.video.lock().unwrap();
                state.detector.reset();
                state.segment = Default::default();
                state.last_event = None;
            }
            Stream::Audio => {
                let mut state = self.audio.lock().unwrap();
                if let Some(detector) = state.detector.as_mut() {
                    detector.reset();
                }
                state.segment = Default::default();
                state.last_event = None;
            }
        }
    }

    fn iterate_internal_links(&self, pad: &gst::Pad) -> gst::Iterator<gst::Pad> {
        let other = if pad == &self.video_sink {
            &self.video_src
        } else if pad == &self.video_src {
            &self.video_sink
        } else if pad == &self.audio_sink {
            &self.audio_src
        } else {
            &self.audio_sink
        };

        gst::Iterator::from_vec(vec![other.clone()])
    }
}

/// Position of `pts` in the stream, which is where the source generated it unless the pipeline
/// restamped it. Event indices are estimated from this.
fn stream_time(segment: &gst::FormattedSegment<gst::ClockTime>, pts: gst::ClockTime) -> u64 {
    segment.to_stream_time(pts).unwrap_or(pts).nseconds()
}

/// Reads brightness from a mapped frame of any of the supported formats.
struct Picture<'a> {
    data: &'a [u8],
    stride: usize,
    pixel_stride: usize,
    /// Offsets of Y, or of R, G and B.
    offsets: [usize; 3],
    rgb: bool,
    width: u32,
    height: u32,
}

impl<'a> Picture<'a> {
    fn new(frame: &'a gst_video::VideoFrameRef<&gst::BufferRef>) -> Option<Self> {
        let finfo = frame.format_info();
        let plane = finfo.plane()[0];
        let poffset = finfo.poffset();

        Some(Picture {
            data: frame.plane_data(plane).ok()?,
            stride: frame.plane_stride()[plane as usize] as usize,
            pixel_stride: finfo.pixel_stride()[0] as usize,
            offsets: [0, 1, 2].map(|c| poffset.get(c).copied().unwrap_or(0) as usize),
            rgb: finfo.is_rgb(),
            width: frame.width(),
            height: frame.height(),
        })
    }

    fn luma(&self, x: u32, y: u32) -> f64 {
        let pos = y as usize * self.stride + x as usize * self.pixel_stride;
        if self.rgb {
            let [r, g, b] = self.offsets.map(|o| self.data[pos + o] as f64);
            (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
        } else {
            self.data[pos + self.offsets[0]] as f64 / 255.0
        }
    }

    /// Mean brightness of the centre of the flash area, from a grid of about 64 × 36 points.
    fn flash_brightness(&self) -> f64 {
        let layout = CodeLayout::new(self.width, self.height);
        let (x0, x1) = (self.width / 4, self.width * 3 / 4);
        let (y0, y1) = (
            self.height / 4,
            (self.height * 3 / 4).min(layout.code_top()),
        );
        let step_x = ((x1 - x0) / 64).max(1) as usize;
        let step_y = (y1.saturating_sub(y0) / 36).max(1) as usize;

        let (sum, count) = (y0..y1)
            .step_by(step_y)
            .flat_map(|y| (x0..x1).step_by(step_x).map(move |x| (x, y)))
            .fold((0.0, 0), |(sum, count), (x, y)| {
                (sum + self.luma(x, y), count + 1)
            });

        if count == 0 { 0.0 } else { sum / count as f64 }
    }

    /// Reads a code row, averaging a few points around the centre of each cell.
    fn code_row(&self, row: u32) -> Option<u32> {
        let layout = CodeLayout::new(self.width, self.height);
        let dx = (self.width / (CODE_CELLS * 4)) as i64;
        let dy = (layout.row_height() / 4) as i64;

        let cells = (0..CODE_CELLS)
            .map(|cell| {
                let (cx, cy) = layout.cell_centre(row, cell);
                let mut sum = 0.0;
                for oy in [-dy, 0, dy] {
                    for ox in [-dx, 0, dx] {
                        let x = (cx as i64 + ox).clamp(0, self.width as i64 - 1);
                        let y = (cy as i64 + oy).clamp(0, self.height as i64 - 1);
                        sum += self.luma(x as u32, y as u32);
                    }
                }
                sum / 9.0
            })
            .collect::<Vec<_>>();

        analyzer::video::decode_row(&cells)
    }
}

/// Averages the channels of interleaved audio.
fn mix_down(info: &gst_audio::AudioInfo, data: &[u8]) -> Vec<f32> {
    let bpf = info.bpf() as usize;
    let channels = info.channels() as usize;
    let width = bpf / channels;
    let format = info.format();

    data.chunks_exact(bpf)
        .map(|frame| {
            frame
                .chunks_exact(width)
                .map(|s| decode(format, s))
                .sum::<f32>()
                / channels as f32
        })
        .collect()
}

fn decode(format: gst_audio::AudioFormat, s: &[u8]) -> f32 {
    match format {
        gst_audio::AUDIO_FORMAT_S16 => i16::from_ne_bytes(s.try_into().unwrap()) as f32 / 32768.0,
        gst_audio::AUDIO_FORMAT_S32 => {
            (i32::from_ne_bytes(s.try_into().unwrap()) as f64 / 2_147_483_648.0) as f32
        }
        gst_audio::AUDIO_FORMAT_F32 => f32::from_ne_bytes(s.try_into().unwrap()),
        gst_audio::AUDIO_FORMAT_F64 => f64::from_ne_bytes(s.try_into().unwrap()) as f32,
        _ => unreachable!(),
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AvSyncDetect {
    const NAME: &'static str = "GstAvSyncDetect";
    type Type = super::AvSyncDetect;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sink_pad = |name, stream| {
            gst::Pad::builder_from_template(&klass.pad_template(name).unwrap())
                .chain_function(move |_pad, parent, buffer| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |imp| imp.chain(stream, buffer),
                    )
                })
                .event_function(move |pad, parent, event| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.sink_event(stream, pad, event),
                    )
                })
                .iterate_internal_links_function(|pad, parent| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || gst::Iterator::from_vec(vec![]),
                        |imp| imp.iterate_internal_links(pad),
                    )
                })
                .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::PROXY_ALLOCATION)
                .build()
        };

        let src_pad = |name, stream| {
            gst::Pad::builder_from_template(&klass.pad_template(name).unwrap())
                .query_function(move |pad, parent, query| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.src_query(stream, pad, query),
                    )
                })
                .event_function(move |pad, parent, event| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.src_event(stream, pad, event),
                    )
                })
                .iterate_internal_links_function(|pad, parent| {
                    AvSyncDetect::catch_panic_pad_function(
                        parent,
                        || gst::Iterator::from_vec(vec![]),
                        |imp| imp.iterate_internal_links(pad),
                    )
                })
                .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::PROXY_ALLOCATION)
                .build()
        };

        Self {
            video_sink: sink_pad("video_sink", Stream::Video),
            video_src: src_pad("video_src", Stream::Video),
            audio_sink: sink_pad("audio_sink", Stream::Audio),
            audio_src: src_pad("audio_src", Stream::Audio),
            settings: Default::default(),
            video: Default::default(),
            audio: Default::default(),
            measurement: Default::default(),
            playing: AtomicBool::new(false),
        }
    }
}

impl ObjectImpl for AvSyncDetect {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("interval")
                    .nick("Interval")
                    .blurb("Time between events, in nanoseconds. Must match the source")
                    .minimum(pattern::MIN_INTERVAL)
                    .default_value(pattern::DEFAULT_INTERVAL)
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
                    .blurb(
                        "Tone frequencies in Hz, used in turn by event index, at least 200 Hz \
                         apart. Must match the source",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("post-messages")
                    .nick("Post Messages")
                    .blurb("Post an element message for each detected and each paired event")
                    .default_value(DEFAULT_POST_MESSAGES)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("window")
                    .nick("Window")
                    .blurb("Number of recent events the drift and clock rates are measured over")
                    .minimum(MIN_WINDOW)
                    .maximum(MAX_WINDOW)
                    .default_value(DEFAULT_WINDOW)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("tolerance-early")
                    .nick("Tolerance Early")
                    .blurb("How far audio may be ahead of video before a warning, in nanoseconds")
                    .maximum(i64::MAX as u64)
                    .default_value(DEFAULT_TOLERANCE_EARLY)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("tolerance-late")
                    .nick("Tolerance Late")
                    .blurb("How far audio may be behind video before a warning, in nanoseconds")
                    .maximum(i64::MAX as u64)
                    .default_value(DEFAULT_TOLERANCE_LATE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt64::builder("last-offset")
                    .nick("Last Offset")
                    .blurb("Audio time minus video time of the last pair, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("avg-offset")
                    .nick("Average Offset")
                    .blurb("Average audio time minus video time, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("min-offset")
                    .nick("Minimum Offset")
                    .blurb("Minimum audio time minus video time, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("max-offset")
                    .nick("Maximum Offset")
                    .blurb("Maximum audio time minus video time, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecDouble::builder("drift")
                    .nick("Drift")
                    .blurb("Change of the offset over the window, in nanoseconds per second")
                    .read_only()
                    .build(),
                glib::ParamSpecDouble::builder("audio-clock-ppm")
                    .nick("Audio Clock PPM")
                    .blurb("How much faster audio timestamps advance than the pattern, in ppm")
                    .read_only()
                    .build(),
                glib::ParamSpecDouble::builder("video-clock-ppm")
                    .nick("Video Clock PPM")
                    .blurb("How much faster video timestamps advance than the pattern, in ppm")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("matched-events")
                    .nick("Matched Events")
                    .blurb("Number of paired events")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("missed-audio")
                    .nick("Missed Audio")
                    .blurb("Number of video events without audio")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("missed-video")
                    .nick("Missed Video")
                    .blurb("Number of audio events without video")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("frames-dropped")
                    .nick("Frames Dropped")
                    .blurb("Number of video frames missing according to the frame counter")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("frames-duplicated")
                    .nick("Frames Duplicated")
                    .blurb("Number of repeated video frames according to the frame counter")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("audio-lateness")
                    .nick("Audio Lateness")
                    .blurb("Clock running time minus running time of the last audio buffer, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("video-lateness")
                    .nick("Video Lateness")
                    .blurb("Clock running time minus running time of the last video buffer, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("max-audio-lateness")
                    .nick("Maximum Audio Lateness")
                    .blurb("Maximum lateness of audio buffers, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("max-video-lateness")
                    .nick("Maximum Video Lateness")
                    .blurb("Maximum lateness of video buffers, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("audio-latency")
                    .nick("Audio Latency")
                    .blurb(
                        "Latency of the last paired audio event from the encoded clock time, in \
                         nanoseconds. Zero unless the stream was restamped on the way",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("video-latency")
                    .nick("Video Latency")
                    .blurb(
                        "Latency of the last video frame from the encoded clock time, in \
                         nanoseconds. Zero unless the stream was restamped on the way",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("avg-audio-latency")
                    .nick("Average Audio Latency")
                    .blurb(
                        "Average latency of paired audio events from the encoded clock time, in \
                         nanoseconds",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("avg-video-latency")
                    .nick("Average Video Latency")
                    .blurb(
                        "Average latency of video frames from the encoded clock time, in \
                         nanoseconds",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("max-audio-latency")
                    .nick("Maximum Audio Latency")
                    .blurb(
                        "Maximum latency of paired audio events from the encoded clock time, in \
                         nanoseconds",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecInt64::builder("max-video-latency")
                    .nick("Maximum Video Latency")
                    .blurb(
                        "Maximum latency of video frames from the encoded clock time, in \
                         nanoseconds",
                    )
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("pipeline-latency")
                    .nick("Pipeline Latency")
                    .blurb("Latency configured on the pipeline, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("audio-upstream-latency")
                    .nick("Audio Upstream Latency")
                    .blurb("Minimum latency reported upstream of the audio sink pad, in nanoseconds")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("video-upstream-latency")
                    .nick("Video Upstream Latency")
                    .blurb("Minimum latency reported upstream of the video sink pad, in nanoseconds")
                    .read_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "interval" => {
                settings.interval = value.get().expect("type checked upstream");
            }
            "frequencies" => {
                settings.frequencies = value
                    .get::<gst::Array>()
                    .expect("type checked upstream")
                    .iter()
                    .map(|v| v.get::<f64>().expect("type checked upstream"))
                    .collect();
            }
            "post-messages" => {
                settings.post_messages = value.get().expect("type checked upstream");
            }
            "window" => {
                settings.window = value.get().expect("type checked upstream");
            }
            "tolerance-early" => {
                settings.tolerance_early = value.get().expect("type checked upstream");
            }
            "tolerance-late" => {
                settings.tolerance_late = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        {
            let measurement = self.measurement.lock().unwrap();
            let offsets = &measurement.offsets;
            let [video_lateness, audio_lateness] = &measurement.lateness;
            let [video_latency, audio_latency] = &measurement.latency;
            let upstream = |stream: Stream| {
                measurement.upstream_latency[stream as usize]
                    .map_or(0, |u| u.min.nseconds())
                    .to_value()
            };
            match pspec.name() {
                "last-offset" => return offsets.last().unwrap_or(0).to_value(),
                "avg-offset" => return offsets.average().unwrap_or(0).to_value(),
                "min-offset" => return offsets.min().unwrap_or(0).to_value(),
                "max-offset" => return offsets.max().unwrap_or(0).to_value(),
                "drift" => return measurement.drift().unwrap_or(0.0).to_value(),
                "audio-clock-ppm" => {
                    return measurement.audio_rate.ppm().unwrap_or(0.0).to_value();
                }
                "video-clock-ppm" => {
                    return measurement.video_rate.ppm().unwrap_or(0.0).to_value();
                }
                "matched-events" => return offsets.count().to_value(),
                "missed-audio" => return measurement.pairer.missed_audio().to_value(),
                "missed-video" => return measurement.pairer.missed_video().to_value(),
                "frames-dropped" => return measurement.frames.dropped().to_value(),
                "frames-duplicated" => return measurement.frames.duplicated().to_value(),
                "audio-lateness" => return audio_lateness.last().unwrap_or(0).to_value(),
                "video-lateness" => return video_lateness.last().unwrap_or(0).to_value(),
                "max-audio-lateness" => return audio_lateness.max().unwrap_or(0).to_value(),
                "max-video-lateness" => return video_lateness.max().unwrap_or(0).to_value(),
                "audio-latency" => return audio_latency.last().unwrap_or(0).to_value(),
                "video-latency" => return video_latency.last().unwrap_or(0).to_value(),
                "avg-audio-latency" => return audio_latency.average().unwrap_or(0).to_value(),
                "avg-video-latency" => return video_latency.average().unwrap_or(0).to_value(),
                "max-audio-latency" => return audio_latency.max().unwrap_or(0).to_value(),
                "max-video-latency" => return video_latency.max().unwrap_or(0).to_value(),
                "pipeline-latency" => {
                    return measurement
                        .pipeline_latency
                        .map_or(0, gst::ClockTime::nseconds)
                        .to_value();
                }
                "audio-upstream-latency" => return upstream(Stream::Audio),
                "video-upstream-latency" => return upstream(Stream::Video),
                _ => (),
            }
        }

        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "interval" => settings.interval.to_value(),
            "frequencies" => settings
                .frequencies
                .iter()
                .map(|f| f.to_send_value())
                .collect::<gst::Array>()
                .to_value(),
            "post-messages" => settings.post_messages.to_value(),
            "window" => settings.window.to_value(),
            "tolerance-early" => settings.tolerance_early.to_value(),
            "tolerance-late" => settings.tolerance_late.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        for pad in [
            &self.video_sink,
            &self.video_src,
            &self.audio_sink,
            &self.audio_src,
        ] {
            obj.add_pad(pad).unwrap();
        }
    }
}

impl GstObjectImpl for AvSyncDetect {}

impl ElementImpl for AvSyncDetect {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "A/V Sync Detector",
                "Filter/Analyzer/Audio/Video",
                "Detects the flashes and tones of avsynctestsrc",
                "Ray Tiley <ray.tiley@trms.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let video_caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::I420,
                    gst_video::VideoFormat::Yv12,
                    gst_video::VideoFormat::Y41b,
                    gst_video::VideoFormat::Y42b,
                    gst_video::VideoFormat::Y444,
                    gst_video::VideoFormat::Nv12,
                    gst_video::VideoFormat::Nv21,
                    gst_video::VideoFormat::Nv16,
                    gst_video::VideoFormat::Nv61,
                    gst_video::VideoFormat::Nv24,
                    gst_video::VideoFormat::Uyvy,
                    gst_video::VideoFormat::Yuy2,
                    gst_video::VideoFormat::Yvyu,
                    gst_video::VideoFormat::Vyuy,
                    gst_video::VideoFormat::Ayuv,
                    gst_video::VideoFormat::Gray8,
                    gst_video::VideoFormat::Bgrx,
                    gst_video::VideoFormat::Rgbx,
                    gst_video::VideoFormat::Xrgb,
                    gst_video::VideoFormat::Xbgr,
                    gst_video::VideoFormat::Bgra,
                    gst_video::VideoFormat::Rgba,
                    gst_video::VideoFormat::Argb,
                    gst_video::VideoFormat::Abgr,
                    gst_video::VideoFormat::Rgb,
                    gst_video::VideoFormat::Bgr,
                ])
                .build();
            let audio_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format_list([
                    gst_audio::AUDIO_FORMAT_S16,
                    gst_audio::AUDIO_FORMAT_S32,
                    gst_audio::AUDIO_FORMAT_F32,
                    gst_audio::AUDIO_FORMAT_F64,
                ])
                .build();

            [
                ("video_sink", gst::PadDirection::Sink, &video_caps),
                ("video_src", gst::PadDirection::Src, &video_caps),
                ("audio_sink", gst::PadDirection::Sink, &audio_caps),
                ("audio_src", gst::PadDirection::Src, &audio_caps),
            ]
            .into_iter()
            .map(|(name, direction, caps)| {
                gst::PadTemplate::new(name, direction, gst::PadPresence::Always, caps).unwrap()
            })
            .collect()
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            let settings = self.settings.lock().unwrap().clone();
            gst::debug!(CAT, imp = self, "Starting with {settings:?}");

            if let Err(err) = pattern::validate_frequencies(&settings.frequencies) {
                gst::element_imp_error!(self, gst::LibraryError::Settings, ["{err}"]);
                return Err(gst::StateChangeError);
            }

            *self.measurement.lock().unwrap() = Measurement::new(&settings);
        }

        match transition {
            // Before upstream starts pushing in PLAYING
            gst::StateChange::PausedToPlaying => self.playing.store(true, Ordering::SeqCst),
            gst::StateChange::PlayingToPaused => self.playing.store(false, Ordering::SeqCst),
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            *self.video.lock().unwrap() = Default::default();
            *self.audio.lock().unwrap() = Default::default();
        }

        Ok(ret)
    }
}
