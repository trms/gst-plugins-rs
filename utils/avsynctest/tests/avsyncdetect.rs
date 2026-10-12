// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use std::sync::{Arc, Mutex};

use gstavsynctest::pattern::{self, EventGrid, SECOND};

const INTERVAL: u64 = SECOND;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstavsynctest::plugin_register_static().expect("avsynctest test");
    });
}

#[derive(Debug, Clone, Copy)]
struct Event {
    index: u64,
    pts: u64,
    running_time: Option<u64>,
    frame_counter: Option<u32>,
    frequency: Option<f64>,
}

impl Event {
    fn from_structure(s: &gst::StructureRef) -> Self {
        Event {
            index: s.get("index").unwrap(),
            pts: s.get::<gst::ClockTime>("pts").unwrap().nseconds(),
            running_time: s
                .get_optional::<gst::ClockTime>("running-time")
                .unwrap()
                .map(gst::ClockTime::nseconds),
            frame_counter: s.get_optional("frame-counter").unwrap(),
            frequency: s.get_optional("frequency").unwrap(),
        }
    }
}

#[derive(Debug, Default)]
struct Events {
    video: Vec<Event>,
    audio: Vec<Event>,
}

fn pipeline(description: &str) -> gst::Pipeline {
    init();

    gst::parse::launch(&format!(
        "avsynctestsrc name=src interval={INTERVAL} \
         avsyncdetect name=detect interval={INTERVAL} \
         {description}"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap()
}

/// Runs until at least `video` and `audio` events were found. `on_event` is called for each
/// one, with `true` for video.
fn collect(
    pipeline: &gst::Pipeline,
    video: usize,
    audio: usize,
    mut on_event: impl FnMut(&gst::Pipeline, bool, &Event),
) -> Events {
    let bus = pipeline.bus().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut events = Events::default();
    while events.video.len() < video || events.audio.len() < audio {
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Element, gst::MessageType::Error],
            )
            .unwrap_or_else(|| panic!("Timed out with {events:?}"));

        match msg.view() {
            gst::MessageView::Error(err) => panic!("{err:?}"),
            gst::MessageView::Element(m) => {
                let s = m.structure().unwrap();
                let is_video = match s.name().as_str() {
                    "avsync-video-event" => true,
                    "avsync-audio-event" => false,
                    _ => continue,
                };
                assert_eq!(s.get::<&str>("element").unwrap(), "detect");

                let event = Event::from_structure(s);
                on_event(pipeline, is_video, &event);
                if is_video {
                    events.video.push(event);
                } else {
                    events.audio.push(event);
                }
            }
            _ => unreachable!(),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    events
}

fn check_video(events: &[Event], grid: &EventGrid) {
    for (n, event) in events.iter().enumerate() {
        let n = n as u64;
        assert_eq!(event.index, n, "{events:?}");
        assert_eq!(event.pts, grid.event_time(n), "{events:?}");
        assert_eq!(event.frame_counter, Some(grid.event_frame(n) as u32));
    }
}

fn check_audio(events: &[Event], grid: &EventGrid, rate: u32) {
    for (n, event) in events.iter().enumerate() {
        let n = n as u64;
        let tone = grid.tone(
            n,
            rate,
            pattern::DEFAULT_FLASH_DURATION,
            &pattern::DEFAULT_FREQUENCIES,
        );
        assert_eq!(event.index, n, "{events:?}");
        assert_eq!(
            event.pts,
            pattern::sample_time(tone.start, rate),
            "{events:?}"
        );
        assert_eq!(event.frequency, Some(tone.frequency));
    }
}

#[test]
fn test_direct() {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=30000/1001 ! detect.video_sink \
         src.audio ! audio/x-raw,rate=48000 ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink",
    );
    let events = collect(&pipeline, 6, 6, |_, _, _| {});

    let grid = EventGrid::new(INTERVAL, 30000, 1001);
    check_video(&events.video, &grid);
    check_audio(&events.audio, &grid, 48000);

    for (video, audio) in events.video.iter().zip(&events.audio) {
        assert_eq!(video.running_time, Some(video.pts));
        assert_eq!(audio.running_time, Some(audio.pts));
        // Aligned to within one sample
        assert!(audio.pts - video.pts < SECOND / 48000 + 1);
    }
}

#[test]
fn test_passthrough() {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240 ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink",
    );
    let detect = pipeline.by_name("detect").unwrap();

    let seen = Arc::new(Mutex::new(Vec::<(String, usize)>::new()));
    for name in ["video_sink", "video_src", "audio_sink", "audio_src"] {
        let seen = seen.clone();
        detect
            .static_pad(name)
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let buffer = info.buffer().unwrap();
                seen.lock()
                    .unwrap()
                    .push((name.to_string(), buffer.as_ptr() as usize));
                gst::PadProbeReturn::Ok
            });
    }

    collect(&pipeline, 2, 2, |_, _, _| {});

    let seen = seen.lock().unwrap();
    for stream in ["video", "audio"] {
        let buffers = |pad: &str| {
            seen.iter()
                .filter(|(name, _)| name == &format!("{stream}_{pad}"))
                .map(|(_, ptr)| *ptr)
                .collect::<Vec<_>>()
        };
        let (input, output) = (buffers("sink"), buffers("src"));
        assert!(output.len() > 10);
        assert_eq!(input[..output.len()], output[..]);
    }
}

#[test]
fn test_running_time_offset() {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink",
    );
    let detect = pipeline.by_name("detect").unwrap();
    // As if audio was 100 ms late and video two frames early
    detect
        .static_pad("audio_sink")
        .unwrap()
        .set_offset(100_000_000);
    detect
        .static_pad("video_sink")
        .unwrap()
        .set_offset(80_000_000);

    let events = collect(&pipeline, 4, 4, |_, _, _| {});

    let grid = EventGrid::new(INTERVAL, 25, 1);
    check_video(&events.video, &grid);
    check_audio(&events.audio, &grid, 48000);
    for event in &events.video {
        assert_eq!(event.running_time, Some(event.pts + 80_000_000));
    }
    for event in &events.audio {
        assert_eq!(event.running_time, Some(event.pts + 100_000_000));
    }
}

#[test]
fn test_video_formats() {
    for format in [
        "I420", "NV12", "UYVY", "YUY2", "Y444", "AYUV", "GRAY8", "BGRx", "RGBA", "ARGB", "RGB",
    ] {
        let pipeline = pipeline(&format!(
            "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! videoconvert ! \
             video/x-raw,format={format} ! detect.video_sink detect.video_src ! fakesink"
        ));
        let events = collect(&pipeline, 3, 0, |_, _, _| {});

        check_video(&events.video, &EventGrid::new(INTERVAL, 25, 1));
    }
}

#[test]
fn test_audio_formats() {
    for (caps, rate) in [
        ("audio/x-raw,format=S16LE,rate=44100,channels=2", 44100),
        ("audio/x-raw,format=S32LE,rate=96000,channels=1", 96000),
        ("audio/x-raw,format=F32LE,rate=48000,channels=6", 48000),
        (
            "audio/x-raw,format=F32LE,rate=48000 ! audioconvert ! audio/x-raw,format=F64LE",
            48000,
        ),
    ] {
        let pipeline = pipeline(&format!(
            "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! fakesink \
             src.audio ! {caps} ! detect.audio_sink detect.audio_src ! fakesink"
        ));
        let events = collect(&pipeline, 0, 3, |_, _, _| {});

        check_audio(&events.audio, &EventGrid::new(INTERVAL, 25, 1), rate);
    }
}

#[test]
fn test_flushing_seek() {
    const SEEK: u64 = 7_500_000_000;

    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink",
    );

    let mut seeked = false;
    let events = collect(&pipeline, 12, 12, |pipeline, is_video, event| {
        if is_video && event.index == 2 && !seeked {
            seeked = true;
            pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::from_nseconds(SEEK),
                )
                .unwrap();
        }
    });

    // Events before the seek may have run ahead of the seek position. Every event must be a
    // real one, and the ones after the seek have their running time from the new segment.
    let grid = EventGrid::new(INTERVAL, 25, 1);
    let mut after_seek = [0, 0];
    for (is_video, event) in events
        .video
        .iter()
        .map(|e| (true, e))
        .chain(events.audio.iter().map(|e| (false, e)))
    {
        let expected = if is_video {
            grid.event_time(event.index)
        } else {
            let tone = grid.tone(
                event.index,
                48000,
                pattern::DEFAULT_FLASH_DURATION,
                &pattern::DEFAULT_FREQUENCIES,
            );
            pattern::sample_time(tone.start, 48000)
        };
        assert_eq!(event.pts, expected, "{events:?}");

        let running_time = event.running_time.unwrap();
        if running_time != event.pts {
            assert_eq!(running_time, event.pts - SEEK, "{events:?}");
            after_seek[is_video as usize] += 1;
        }
    }
    assert!(after_seek[0] > 0 && after_seek[1] > 0, "{events:?}");
}

#[test]
fn test_live() {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=30/1 ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! queue ! fakesink sync=true \
         detect.audio_src ! queue ! fakesink sync=true",
    );
    pipeline
        .by_name("src")
        .unwrap()
        .set_property("is-live", true);

    let events = collect(&pipeline, 2, 2, |_, _, _| {});

    // Both streams are offset by the same running time from when they started
    let offset = events.video[0].pts;
    for (n, (video, audio)) in events.video.iter().zip(&events.audio).enumerate() {
        assert_eq!(video.index, n as u64);
        assert_eq!(audio.index, n as u64);
        assert_eq!(video.pts, offset + n as u64 * INTERVAL, "{events:?}");
        assert!(audio.pts - video.pts < SECOND / 48000 + 1, "{events:?}");
    }
}

#[derive(Debug, Clone, Copy)]
struct PairMessage {
    index: u64,
    offset: i64,
    drift: Option<f64>,
    within_tolerance: bool,
    video_latency: Option<i64>,
    audio_latency: Option<i64>,
}

#[derive(Debug, Default)]
struct Pairs {
    pairs: Vec<PairMessage>,
    warnings: usize,
    /// Dropped and duplicated frames of each frame error message.
    frame_errors: Vec<(u64, u64)>,
    latency_messages: Vec<gst::Structure>,
    summary: Option<gst::Structure>,
}

/// Runs until `count` pairs were found, or until EOS if `count` is `None`.
fn collect_pairs(pipeline: &gst::Pipeline, count: Option<usize>) -> Pairs {
    let bus = pipeline.bus().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut result = Pairs::default();
    while count.is_none_or(|count| result.pairs.len() < count) {
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[
                    gst::MessageType::Element,
                    gst::MessageType::Error,
                    gst::MessageType::Warning,
                    gst::MessageType::Eos,
                ],
            )
            .unwrap_or_else(|| panic!("Timed out with {result:?}"));

        match msg.view() {
            gst::MessageView::Error(err) => panic!("{err:?}"),
            gst::MessageView::Warning(_) => result.warnings += 1,
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Element(m) => {
                let s = m.structure().unwrap();
                match s.name().as_str() {
                    "avsync-pair" => result.pairs.push(PairMessage {
                        index: s.get("index").unwrap(),
                        offset: s.get("offset").unwrap(),
                        drift: s.get_optional("drift").unwrap(),
                        within_tolerance: s.get("within-tolerance").unwrap(),
                        video_latency: s.get_optional("video-latency").unwrap(),
                        audio_latency: s.get_optional("audio-latency").unwrap(),
                    }),
                    "avsync-latency" => result.latency_messages.push(s.to_owned()),
                    "avsync-frame-error" => result
                        .frame_errors
                        .push((s.get("dropped").unwrap(), s.get("duplicated").unwrap())),
                    "avsync-summary" => {
                        assert!(result.summary.is_none());
                        result.summary = Some(s.to_owned());
                    }
                    _ => (),
                }
            }
            _ => unreachable!(),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    result
}

/// Runs from the start to `stop` and EOS.
fn run_to(pipeline: &gst::Pipeline, stop: gst::ClockTime) -> Pairs {
    pipeline.set_state(gst::State::Paused).unwrap();
    pipeline.state(gst::ClockTime::from_seconds(10)).0.unwrap();
    pipeline
        .seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::Set,
            stop,
        )
        .unwrap();

    collect_pairs(pipeline, None)
}

fn both_streams(caps: &str) -> gst::Pipeline {
    pipeline(&format!(
        "src.video ! video/x-raw,width=320,height=240,{caps} ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink"
    ))
}

#[test]
fn test_pairs_direct() {
    let pipeline = both_streams("framerate=30000/1001");

    // Run to EOS after 7 events
    let result = run_to(&pipeline, gst::ClockTime::from_mseconds(6500));

    assert_eq!(result.warnings, 0);
    assert!(result.frame_errors.is_empty(), "{result:?}");
    // The clock time is not encoded
    assert!(
        result
            .pairs
            .iter()
            .all(|p| p.video_latency.is_none() && p.audio_latency.is_none()),
        "{result:?}"
    );
    assert_eq!(
        result.pairs.iter().map(|p| p.index).collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );
    for pair in &result.pairs {
        // Audio starts on the first sample at or after the frame
        assert!(
            (0..=(SECOND / 48000) as i64).contains(&pair.offset),
            "{pair:?}"
        );
        assert!(pair.within_tolerance);
    }
    assert!(result.pairs[0].drift.is_none());
    assert!(result.pairs[6].drift.unwrap().abs() < 10.0);

    let summary = result.summary.expect("no summary");
    assert_eq!(summary.get::<&str>("element").unwrap(), "detect");
    assert_eq!(summary.get::<u64>("matched-events").unwrap(), 7);
    assert_eq!(summary.get::<u64>("missed-audio").unwrap(), 0);
    assert_eq!(summary.get::<u64>("missed-video").unwrap(), 0);
    assert_eq!(summary.get::<u64>("frames-dropped").unwrap(), 0);
    assert_eq!(summary.get::<u64>("frames-duplicated").unwrap(), 0);
    let max = summary.get::<i64>("max-offset").unwrap();
    assert!(max <= (SECOND / 48000) as i64);
    for field in ["audio-clock-ppm", "video-clock-ppm"] {
        assert!(summary.get::<f64>(field).unwrap().abs() < 0.01, "{summary}");
    }

    // Still readable after stopping
    let detect = pipeline.by_name("detect").unwrap();
    assert_eq!(detect.property::<u64>("matched-events"), 7);
    assert_eq!(detect.property::<i64>("max-offset"), max);
}

#[test]
fn test_pair_offsets() {
    const FRAME: i64 = 40_000_000;

    // Pad offsets on audio and video, expected A/V offset, whether it is within the default
    // tolerance
    for (audio, video, expected, within) in [
        (100_000_000, 0, 100_000_000, false),
        (0, 2 * FRAME, -2 * FRAME, false),
        (0, FRAME, -FRAME, true),
        (0, 50_000_000, -50_000_000, false),
        (30_000_000, 0, 30_000_000, true),
        // Up to half of the 4 frequencies times the interval
        (1_900_000_000, 0, 1_900_000_000, false),
        (0, 1_900_000_000, -1_900_000_000, false),
    ] {
        let pipeline = both_streams("framerate=25/1");
        let detect = pipeline.by_name("detect").unwrap();
        detect.static_pad("audio_sink").unwrap().set_offset(audio);
        detect.static_pad("video_sink").unwrap().set_offset(video);

        let result = collect_pairs(&pipeline, Some(6));

        assert_eq!(
            result.pairs.iter().map(|p| p.index).collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>(),
            "{result:?}"
        );
        for pair in &result.pairs {
            assert_eq!(pair.offset, expected, "{result:?}");
            assert_eq!(pair.within_tolerance, within, "{result:?}");
        }
        // Only once, when going outside the tolerance
        assert_eq!(result.warnings, !within as usize, "{result:?}");

        assert_eq!(detect.property::<i64>("last-offset"), expected);
        assert_eq!(detect.property::<i64>("avg-offset"), expected);
        assert_eq!(detect.property::<u64>("missed-video"), 0);
    }
}

#[test]
fn test_pair_beyond_range() {
    // 2.5 intervals late pairs with the event 4 later, so appears 1.5 intervals early
    let pipeline = both_streams("framerate=25/1");
    let detect = pipeline.by_name("detect").unwrap();
    detect
        .static_pad("audio_sink")
        .unwrap()
        .set_offset(2_500_000_000);

    let result = collect_pairs(&pipeline, Some(4));

    for (n, pair) in result.pairs.iter().enumerate() {
        assert_eq!(pair.index, n as u64 + 4, "{result:?}");
        assert_eq!(pair.offset, -1_500_000_000, "{result:?}");
    }
    // The first video events have no audio
    assert_eq!(detect.property::<u64>("missed-audio"), 4);
}

#[test]
fn test_source_audio_offset() {
    // The source moves its tones, which the detector measures to within a sample
    for offset in [100_000_000i64, -100_000_000, 1_500_000_000] {
        let pipeline = both_streams("framerate=25/1");
        pipeline
            .by_name("src")
            .unwrap()
            .set_property("audio-offset", offset);

        let result = collect_pairs(&pipeline, Some(3));

        for pair in &result.pairs {
            assert!(
                (offset..offset + SECOND as i64 / 48000 + 1).contains(&pair.offset),
                "offset {offset}: {result:?}"
            );
        }
    }
}

#[test]
fn test_missed_at_eos() {
    // Audio 2.5 intervals late, running to EOS after 8 events. Audio events 0 to 3 pair with
    // video 4 to 7, video 0 to 3 have no audio, and audio 4 to 7 have no video because the
    // video stream ends before the events they pair with.
    let pipeline = both_streams("framerate=25/1");
    let detect = pipeline.by_name("detect").unwrap();

    // Offset the audio only after preroll, or the prerolled flash is already out of range and
    // counted as missed before the seek replays it
    pipeline.set_state(gst::State::Paused).unwrap();
    pipeline.state(gst::ClockTime::from_seconds(10)).0.unwrap();
    detect
        .static_pad("audio_sink")
        .unwrap()
        .set_offset(2_500_000_000);
    pipeline
        .seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(8),
        )
        .unwrap();
    let result = collect_pairs(&pipeline, None);

    assert_eq!(result.pairs.len(), 4, "{result:?}");
    let summary = result.summary.expect("no summary");
    assert_eq!(summary.get::<u64>("matched-events").unwrap(), 4);
    assert_eq!(summary.get::<u64>("missed-audio").unwrap(), 4);
    assert_eq!(summary.get::<u64>("missed-video").unwrap(), 4);
    assert_eq!(detect.property::<u64>("missed-audio"), 4);
    assert_eq!(detect.property::<u64>("missed-video"), 4);
}

#[test]
fn test_tolerance() {
    let pipeline = both_streams("framerate=25/1");
    let detect = pipeline.by_name("detect").unwrap();
    detect.set_property("tolerance-late", 120_000_000u64);
    detect
        .static_pad("audio_sink")
        .unwrap()
        .set_offset(100_000_000);

    let result = collect_pairs(&pipeline, Some(3));
    assert_eq!(result.warnings, 0);
    assert!(result.pairs.iter().all(|p| p.within_tolerance));
}

/// Relabels 48 kHz audio as `rate` and restamps it from the sample count, as if the device that
/// played it ran at a slightly different rate than the one that captured it.
fn relabel_rate(pad: &gst::Pad, rate: i32) {
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_, info| {
            match &mut info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    let buffer = buffer.make_mut();
                    let start = buffer.offset();
                    let end = buffer.offset_end();
                    let time = |sample: u64| {
                        gst::ClockTime::from_nseconds(
                            (sample as u128 * SECOND as u128 / rate as u128) as u64,
                        )
                    };
                    buffer.set_pts(time(start));
                    buffer.set_duration(time(end) - time(start));
                }
                Some(gst::PadProbeData::Event(event)) => {
                    if let gst::EventView::Caps(c) = event.view() {
                        let mut caps = c.caps_owned();
                        caps.make_mut().set("rate", rate);
                        *event = gst::event::Caps::new(&caps);
                    }
                }
                _ => (),
            }
            gst::PadProbeReturn::Ok
        },
    );
}

#[test]
fn test_clock_drift() {
    const RATE: i32 = 48005;

    let pipeline = both_streams("framerate=25/1");
    let detect = pipeline.by_name("detect").unwrap();
    detect.set_property("window", 8u32);
    relabel_rate(&detect.static_pad("audio_sink").unwrap(), RATE);

    let result = collect_pairs(&pipeline, Some(12));

    // Audio timestamps advance 48000 / 48005 as fast as they should
    let ppm = (48000.0 / RATE as f64 - 1.0) * 1e6;
    let audio_ppm = detect.property::<f64>("audio-clock-ppm");
    assert!((audio_ppm - ppm).abs() < 0.1, "{audio_ppm} {ppm}");
    assert!(detect.property::<f64>("video-clock-ppm").abs() < 0.01);

    // So audio gets earlier by that much every second
    let drift = detect.property::<f64>("drift");
    assert!((drift - ppm * 1000.0).abs() < 100.0, "{drift}");
    let last = result.pairs.last().unwrap().drift.unwrap();
    assert!((last - ppm * 1000.0).abs() < 100.0, "{last}");

    for pair in &result.pairs {
        let expected = pair.index as f64 * SECOND as f64 * ppm / 1e6;
        assert!(
            (pair.offset as f64 - expected).abs() < 25_000.0,
            "{result:?}"
        );
    }
}

#[test]
fn test_audio_only() {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! fakesink \
         src.audio ! detect.audio_sink detect.audio_src ! fakesink",
    );
    let detect = pipeline.by_name("detect").unwrap();
    relabel_rate(&detect.static_pad("audio_sink").unwrap(), 48010);

    let events = collect(&pipeline, 0, 6, |_, _, _| {});
    assert_eq!(
        events.audio.iter().map(|e| e.index).collect::<Vec<_>>(),
        (0..6).collect::<Vec<_>>()
    );

    // Measured against index × interval as the frames are unknown, here they are the same
    let ppm = (48000.0 / 48010.0 - 1.0) * 1e6;
    let audio_ppm = detect.property::<f64>("audio-clock-ppm");
    assert!((audio_ppm - ppm).abs() < 0.1, "{audio_ppm} {ppm}");
    assert_eq!(detect.property::<u64>("matched-events"), 0);
    assert_eq!(detect.property::<u64>("missed-audio"), 0);
    assert_eq!(detect.property::<u64>("missed-video"), 0);
}

/// Runs `src ! videorate ! framerate=to ! detect` to EOS. Returns the result and the number of
/// frames the detector saw.
fn rate_converted(from: &str, to: &str) -> (gst::Pipeline, Pairs, u64) {
    let pipeline = pipeline(&format!(
        "src.video ! video/x-raw,width=320,height=240,framerate={from} ! videorate \
         ! video/x-raw,framerate={to} ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! fakesink detect.audio_src ! fakesink"
    ));

    let frames = Arc::new(Mutex::new(0));
    let detect = pipeline.by_name("detect").unwrap();
    let frames_clone = frames.clone();
    detect.static_pad("video_sink").unwrap().add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_FLUSH,
        move |_, info| {
            let mut frames = frames_clone.lock().unwrap();
            match &info.data {
                // Frames prerolled before the seek are not counted by the detector either
                Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::FlushStop => {
                    *frames = 0
                }
                Some(gst::PadProbeData::Buffer(_)) => *frames += 1,
                _ => (),
            }
            gst::PadProbeReturn::Ok
        },
    );

    let result = run_to(&pipeline, gst::ClockTime::from_mseconds(2500));
    let frames = *frames.lock().unwrap();

    (pipeline, result, frames)
}

#[test]
fn test_frames_dropped() {
    let (pipeline, result, frames) = rate_converted("30/1", "15/1");

    // Every other frame is dropped
    assert!(frames > 30, "{frames}");
    assert_eq!(result.frame_errors, vec![(1, 0); frames as usize - 1]);

    let detect = pipeline.by_name("detect").unwrap();
    assert_eq!(detect.property::<u64>("frames-dropped"), frames - 1);
    assert_eq!(detect.property::<u64>("frames-duplicated"), 0);
    let summary = result.summary.unwrap();
    assert_eq!(summary.get::<u64>("frames-dropped").unwrap(), frames - 1);
}

#[test]
fn test_frames_duplicated() {
    let (pipeline, result, frames) = rate_converted("25/1", "50/1");

    // Every frame is shown twice
    let detect = pipeline.by_name("detect").unwrap();
    let duplicated = detect.property::<u64>("frames-duplicated");
    assert!(frames > 100, "{frames}");
    assert!(
        duplicated.abs_diff(frames / 2) <= 1,
        "{duplicated} {frames}"
    );
    assert_eq!(detect.property::<u64>("frames-dropped"), 0);
    assert!(
        result.frame_errors.iter().all(|&e| e == (0, 1)),
        "{result:?}"
    );
}

#[test]
fn test_frames_flushing_seek() {
    let pipeline = both_streams("framerate=25/1");
    let detect = pipeline.by_name("detect").unwrap();

    let mut seeked = false;
    collect(&pipeline, 4, 0, |pipeline, is_video, event| {
        if is_video && event.index == 1 && !seeked {
            seeked = true;
            pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::from_seconds(10),
                )
                .unwrap();
        }
    });

    // The jump of the frame counter is not counted
    assert_eq!(detect.property::<u64>("frames-dropped"), 0);
    assert_eq!(detect.property::<u64>("frames-duplicated"), 0);
}

#[test]
fn test_lateness() {
    const AUDIO_OFFSET: i64 = 200_000_000;

    let pipeline = live_pipeline();
    let detect = pipeline.by_name("detect").unwrap();

    // Not measured before playing
    assert_eq!(detect.property::<i64>("video-lateness"), 0);

    // Audio arrives as late as before, but its running time is later
    detect
        .static_pad("audio_sink")
        .unwrap()
        .set_offset(AUDIO_OFFSET);

    collect_pairs(&pipeline, Some(2));

    // The source pushes each buffer once the clock reaches its timestamp. Clock waits can
    // return up to 1 ms early, on Windows.
    let range = -1_000_000..50_000_000;
    let video = detect.property::<i64>("video-lateness");
    let max_video = detect.property::<i64>("max-video-lateness");
    assert!(range.contains(&video), "{video}");
    assert!(
        range.contains(&max_video) && max_video >= video,
        "{max_video}"
    );

    let audio = detect.property::<i64>("audio-lateness") + AUDIO_OFFSET;
    let max_audio = detect.property::<i64>("max-audio-lateness") + AUDIO_OFFSET;
    assert!(range.contains(&audio), "{audio}");
    assert!(
        range.contains(&max_audio) && max_audio >= audio,
        "{max_audio}"
    );
}

fn live_pipeline() -> gst::Pipeline {
    let pipeline = pipeline(
        "src.video ! video/x-raw,width=320,height=240,framerate=25/1 ! detect.video_sink \
         src.audio ! detect.audio_sink \
         detect.video_src ! queue ! fakesink sync=true \
         detect.audio_src ! queue ! fakesink sync=true",
    );
    pipeline
        .by_name("src")
        .unwrap()
        .set_property("is-live", true);

    pipeline
}

#[test]
fn test_clock_time_latency() {
    const VIDEO_DELAY: i64 = 100_000_000;
    // The clock time is rounded down to microseconds, and a tone starts up to a sample after
    // its flash
    let video_range = VIDEO_DELAY..VIDEO_DELAY + 1000;
    let audio_range = 0..SECOND as i64 / 48000 + 1000;

    let pipeline = live_pipeline();
    pipeline
        .by_name("src")
        .unwrap()
        .set_property("encode-clock-time", true);
    let detect = pipeline.by_name("detect").unwrap();

    // Video is rendered 100 ms after the time it was generated for
    detect
        .static_pad("video_sink")
        .unwrap()
        .set_offset(VIDEO_DELAY);

    let result = collect_pairs(&pipeline, Some(2));

    for pair in &result.pairs {
        let video = pair.video_latency.unwrap();
        let audio = pair.audio_latency.unwrap();
        assert!(video_range.contains(&video), "{pair:?}");
        assert!(audio_range.contains(&audio), "{pair:?}");
        assert_eq!(audio, video + pair.offset);
    }

    for name in ["video-latency", "avg-video-latency", "max-video-latency"] {
        let latency = detect.property::<i64>(name);
        assert!(video_range.contains(&latency), "{name} {latency}");
    }
    for name in ["audio-latency", "avg-audio-latency", "max-audio-latency"] {
        let latency = detect.property::<i64>(name);
        assert!(audio_range.contains(&latency), "{name} {latency}");
    }
}

#[test]
fn test_reported_latency() {
    let pipeline = live_pipeline();
    let detect = pipeline.by_name("detect").unwrap();

    let result = collect_pairs(&pipeline, Some(1));

    // One frame of video and one 10 ms buffer of audio
    let video = detect.property::<u64>("video-upstream-latency");
    let audio = detect.property::<u64>("audio-upstream-latency");
    assert_eq!(video, 40_000_000);
    assert_eq!(audio, 10_000_000);
    let latency = detect.property::<u64>("pipeline-latency");
    assert!(latency >= video, "{latency}");

    for (stream, upstream) in [("video", video), ("audio", audio)] {
        let s = result
            .latency_messages
            .iter()
            .rfind(|s| s.get::<&str>("stream").unwrap() == stream)
            .unwrap_or_else(|| panic!("no {stream} message in {result:?}"));
        assert_eq!(s.get::<&str>("element").unwrap(), "detect");
        assert_eq!(
            s.get::<gst::ClockTime>("pipeline-latency")
                .unwrap()
                .nseconds(),
            latency
        );
        assert!(s.get::<bool>("live").unwrap());
        assert_eq!(
            s.get::<gst::ClockTime>("upstream-min-latency")
                .unwrap()
                .nseconds(),
            upstream
        );
    }
}
