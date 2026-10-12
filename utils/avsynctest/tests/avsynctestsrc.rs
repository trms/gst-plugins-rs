// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_video::prelude::*;

use std::sync::{Arc, Mutex};

use gstavsynctest::pattern::{self, CODE_BITS, CODE_CELLS, CodeLayout};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstavsynctest::plugin_register_static().expect("avsynctest test");
    });
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    pts: u64,
    offset: u64,
    flash: bool,
    event: u32,
    counter: u32,
    clock_time: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct ToneBurst {
    start: u64,
    crossings: u32,
    length: u64,
}

#[derive(Default)]
struct AudioAnalysis {
    rate: u32,
    last_sample: Option<u64>,
    last_loud: Option<u64>,
    prev_positive: bool,
    tones: Vec<ToneBurst>,
}

impl AudioAnalysis {
    fn push(&mut self, sample: u64, value: f64) {
        // Restarted by the seek
        if self.last_sample.is_some_and(|last| sample <= last) {
            *self = AudioAnalysis {
                rate: self.rate,
                ..Default::default()
            };
        }
        self.last_sample = Some(sample);

        if value.abs() < 1e-4 {
            return;
        }

        // Anything after 10 ms of silence starts a new tone
        let gap = (self.rate / 100) as u64;
        if self.last_loud.is_none_or(|last| sample - last > gap) {
            self.tones.push(ToneBurst {
                start: sample,
                crossings: 0,
                length: 0,
            });
        } else if (value > 0.0) != self.prev_positive {
            self.tones.last_mut().unwrap().crossings += 1;
        }

        let tone = self.tones.last_mut().unwrap();
        tone.length = sample - tone.start + 1;
        self.prev_positive = value > 0.0;
        self.last_loud = Some(sample);
    }
}

fn luma_at(frame: &gst_video::VideoFrameRef<&gst::BufferRef>, x: u32, y: u32) -> u8 {
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).unwrap();
    let (x, y) = (x as usize, y as usize);

    match frame.format() {
        gst_video::VideoFormat::I420 | gst_video::VideoFormat::Nv12 => data[y * stride + x],
        gst_video::VideoFormat::Uyvy => data[y * stride + x * 2 + 1],
        // Map full range back to video range so one threshold works for all formats
        gst_video::VideoFormat::Bgrx => {
            if data[y * stride + x * 4] > 128 {
                235
            } else {
                16
            }
        }
        _ => unreachable!(),
    }
}

/// Reads a code row, `None` if it has no start marker.
fn decode_row(frame: &gst_video::VideoFrameRef<&gst::BufferRef>, row: u32) -> Option<u32> {
    let layout = CodeLayout::new(frame.width(), frame.height());
    let white = |cell| {
        let (x, y) = layout.cell_centre(row, cell);
        luma_at(frame, x, y) > 128
    };

    if !white(0) {
        assert!(
            (1..CODE_CELLS).all(|cell| !white(cell)),
            "start marker missing"
        );
        return None;
    }
    assert!(!white(CODE_CELLS - 1), "end marker missing");

    Some((1..=CODE_BITS).fold(0, |acc, cell| (acc << 1) | white(cell) as u32))
}

/// Runs the source from 0 to `stop` and returns what arrived at the two sinks.
fn run(
    props: &str,
    video_caps: &str,
    audio_caps: &str,
    stop: gst::ClockTime,
) -> (Vec<Frame>, AudioAnalysis) {
    init();

    let pipeline = gst::parse::launch(&format!(
        "avsynctestsrc name=src {props} \
         src.video ! {video_caps} ! appsink name=vsink sync=false \
         src.audio ! {audio_caps} ! appsink name=asink sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();

    let frames = Arc::new(Mutex::new(Vec::new()));
    let audio = Arc::new(Mutex::new(AudioAnalysis::default()));

    let vsink = pipeline
        .by_name("vsink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let frames_clone = frames.clone();
    vsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().unwrap();
                let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
                let buffer = sample.buffer().unwrap();
                let frame =
                    gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();

                let mut frames = frames_clone.lock().unwrap();
                // Restarted by the seek
                if buffer.offset() == 0 {
                    frames.clear();
                }
                frames.push(Frame {
                    pts: buffer.pts().unwrap().nseconds(),
                    offset: buffer.offset(),
                    flash: luma_at(&frame, info.width() / 2, info.height() / 4) > 128,
                    event: decode_row(&frame, pattern::EVENT_ROW).unwrap(),
                    counter: decode_row(&frame, pattern::COUNTER_ROW).unwrap(),
                    clock_time: decode_row(&frame, pattern::CLOCK_ROW),
                });

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let asink = pipeline
        .by_name("asink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let audio_clone = audio.clone();
    asink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().unwrap();
                let info = gst_audio::AudioInfo::from_caps(sample.caps().unwrap()).unwrap();
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable().unwrap();
                let bpf = info.bpf() as usize;
                let width = bpf / info.channels() as usize;

                let mut audio = audio_clone.lock().unwrap();
                audio.rate = info.rate();
                for (i, frame) in map.chunks_exact(bpf).enumerate() {
                    let channels = frame
                        .chunks_exact(width)
                        .map(|s| match info.format() {
                            gst_audio::AUDIO_FORMAT_F32 => {
                                f32::from_ne_bytes(s.try_into().unwrap()) as f64
                            }
                            gst_audio::AUDIO_FORMAT_S16 => {
                                i16::from_ne_bytes(s.try_into().unwrap()) as f64 / 32768.0
                            }
                            gst_audio::AUDIO_FORMAT_S32 => {
                                i32::from_ne_bytes(s.try_into().unwrap()) as f64 / 2_147_483_648.0
                            }
                            _ => unreachable!(),
                        })
                        .collect::<Vec<_>>();
                    assert!(channels.iter().all(|&c| c == channels[0]));
                    audio.push(buffer.offset() + i as u64, channels[0]);
                }

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Paused).unwrap();
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        match msg.view() {
            gst::MessageView::AsyncDone(..) => break,
            gst::MessageView::Error(err) => panic!("{err:?}"),
            _ => (),
        }
    }

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
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(30)) {
        match msg.view() {
            gst::MessageView::Eos(..) => {
                eos = true;
                break;
            }
            gst::MessageView::Error(err) => panic!("{err:?}"),
            _ => (),
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos, "no EOS");

    let frames = std::mem::take(&mut *frames.lock().unwrap());
    let audio = std::mem::take(&mut *audio.lock().unwrap());
    (frames, audio)
}

/// First frames of each flash.
fn flash_starts(frames: &[Frame]) -> Vec<Frame> {
    frames
        .iter()
        .enumerate()
        .filter(|(i, f)| f.flash && (*i == 0 || !frames[i - 1].flash))
        .map(|(_, f)| *f)
        .collect()
}

#[test]
fn test_flash_and_tone_aligned() {
    let (frames, audio) = run(
        "",
        "video/x-raw,format=I420,width=320,height=240,framerate=30000/1001",
        "audio/x-raw,format=F32LE,rate=48000,channels=1",
        gst::ClockTime::from_seconds(16),
    );

    assert!(!frames.is_empty());

    let grid = pattern::EventGrid::new(pattern::DEFAULT_INTERVAL, 30000, 1001);

    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(frame.offset, i as u64);
        assert_eq!(frame.counter, i as u32);
        assert_eq!(frame.event as u64, grid.event_for_frame(i as u64));
        assert_eq!(frame.pts, grid.frame_pts(i as u64));
        assert_eq!(frame.clock_time, None);
    }

    let flashes = flash_starts(&frames);
    assert_eq!(
        flashes.iter().map(|f| f.offset).collect::<Vec<_>>(),
        [0, 150, 300, 450]
    );
    // 100 ms at 29.97 fps is three frames
    assert_eq!(frames.iter().filter(|f| f.flash).count(), 4 * 3);

    // The tone starts with a zero sample, so the first audible one is one sample later
    let rate = audio.rate as u64;
    let tones = audio.tones.iter().collect::<Vec<_>>();
    assert_eq!(tones.len(), 4, "{tones:?}");

    for ((tone, flash), expected_freq) in
        tones.iter().zip(&flashes).zip(pattern::DEFAULT_FREQUENCIES)
    {
        let audio_time = pattern::sample_time(tone.start, audio.rate);
        let offset = audio_time as i64 - flash.pts as i64;
        assert!(
            (0..=2 * pattern::SECOND as i64 / rate as i64).contains(&offset),
            "event {}: offset {offset} ns",
            flash.event
        );

        let freq = tone.crossings as f64 / 2.0 * rate as f64 / tone.length as f64;
        assert!(
            (freq - expected_freq).abs() < expected_freq * 0.05,
            "event {}: {freq} Hz, expected {expected_freq} Hz",
            flash.event
        );

        let length = pattern::sample_time(tone.length, audio.rate);
        assert!(length.abs_diff(pattern::DEFAULT_FLASH_DURATION) < 1_000_000);
    }
}

#[test]
fn test_video_formats() {
    for format in ["I420", "NV12", "UYVY", "BGRx"] {
        let (frames, _) = run(
            "interval=500000000",
            &format!("video/x-raw,format={format},width=190,height=101,framerate=25/1"),
            "audio/x-raw,format=S16LE,rate=44100,channels=2",
            gst::ClockTime::from_seconds(2),
        );

        assert_eq!(frames.len(), 50, "{format}");
        assert_eq!(
            flash_starts(&frames)
                .iter()
                .map(|f| f.offset)
                .collect::<Vec<_>>(),
            [0, 13, 25, 38],
            "{format}"
        );
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.counter, i as u32, "{format}");
        }
    }
}

#[test]
fn test_audio_formats() {
    for format in ["S16LE", "S32LE", "F32LE"] {
        let (_, audio) = run(
            "interval=1000000000",
            "video/x-raw,format=I420,width=320,height=240,framerate=30/1",
            &format!(
                "audio/x-raw,format={format},rate=44100,channels=6,channel-mask=(bitmask)0x3f"
            ),
            gst::ClockTime::from_seconds(3),
        );

        let starts = audio.tones.iter().map(|t| t.start).collect::<Vec<_>>();
        assert_eq!(starts, [1, 44101, 88201], "{format}");
    }
}

#[test]
fn test_flash_longer_than_half_interval() {
    init();

    let src = gst::ElementFactory::make("avsynctestsrc")
        .property("interval", 1_000_000_000u64)
        .property("flash-duration", 600_000_000u64)
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    pipeline.add(&src).unwrap();

    assert!(pipeline.set_state(gst::State::Paused).is_err());
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Counts buffers arriving at an appsink.
fn count_buffers(sink: &gst::Element) -> Arc<Mutex<u64>> {
    let count = Arc::new(Mutex::new(0));
    let count_clone = count.clone();
    sink.downcast_ref::<gst_app::AppSink>()
        .unwrap()
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    sink.pull_sample().unwrap();
                    *count_clone.lock().unwrap() += 1;
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    count
}

/// Waits for `cond` to hold, failing on any error message from the pipeline.
fn wait_for(
    pipeline: &gst::Pipeline,
    timeout: gst::ClockTime,
    mut cond: impl FnMut(&gst::Message) -> bool,
) {
    let bus = pipeline.bus().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_nanos(timeout.nseconds());

    while std::time::Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
            if let gst::MessageView::Error(err) = msg.view() {
                panic!("{err:?}");
            }
            if cond(&msg) {
                return;
            }
        } else if cond(&gst::message::Application::new(gst::Structure::new_empty(
            "tick",
        ))) {
            return;
        }
    }

    panic!("timed out");
}

#[test]
fn test_one_pad_unlinked() {
    init();

    for (pad, caps) in [
        ("video", "video/x-raw,width=320,height=240"),
        ("audio", "audio/x-raw"),
    ] {
        let pipeline = gst::parse::launch(&format!(
            "avsynctestsrc name=src interval=500000000 src.{pad} ! {caps} ! appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let count = count_buffers(&pipeline.by_name("sink").unwrap());

        pipeline.set_state(gst::State::Paused).unwrap();
        wait_for(&pipeline, gst::ClockTime::from_seconds(10), |msg| {
            matches!(msg.view(), gst::MessageView::AsyncDone(..))
        });
        pipeline
            .seek(
                1.0,
                gst::SeekFlags::FLUSH,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::Set,
                gst::ClockTime::from_seconds(2),
            )
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        wait_for(&pipeline, gst::ClockTime::from_seconds(10), |msg| {
            matches!(msg.view(), gst::MessageView::Eos(..))
        });
        pipeline.set_state(gst::State::Null).unwrap();

        assert!(*count.lock().unwrap() > 0, "{pad}");
    }
}

#[test]
fn test_one_pad_unlinked_live() {
    init();

    let pipeline = gst::parse::launch(
        "avsynctestsrc name=src is-live=true src.audio ! appsink name=sink sync=true",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let count = count_buffers(&pipeline.by_name("sink").unwrap());

    pipeline.set_state(gst::State::Playing).unwrap();
    // 10 ms buffers, so at least a few dozen in half a second
    wait_for(&pipeline, gst::ClockTime::from_seconds(5), |_| {
        *count.lock().unwrap() >= 30
    });
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_link_later() {
    init();

    let pipeline = gst::parse::launch(
        "avsynctestsrc name=src src.video ! video/x-raw,width=320,height=240 ! appsink name=vsink sync=true",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let video_count = count_buffers(&pipeline.by_name("vsink").unwrap());

    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for(&pipeline, gst::ClockTime::from_seconds(5), |_| {
        *video_count.lock().unwrap() >= 3
    });

    let asink = gst::ElementFactory::make("appsink")
        .property("sync", false)
        .build()
        .unwrap();
    let audio_count = count_buffers(&asink);
    pipeline.add(&asink).unwrap();
    asink.sync_state_with_parent().unwrap();
    pipeline
        .by_name("src")
        .unwrap()
        .link_pads(Some("audio"), &asink, None)
        .unwrap();

    wait_for(&pipeline, gst::ClockTime::from_seconds(5), |_| {
        *audio_count.lock().unwrap() > 0
    });
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_clock_time_encoded() {
    init();

    let pipeline = gst::parse::launch(
        "avsynctestsrc is-live=true encode-clock-time=true \
         ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
         ! appsink name=sink sync=true",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    for _ in 0..5 {
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .unwrap();
        let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
        let buffer = sample.buffer().unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
        let segment = sample
            .segment()
            .unwrap()
            .downcast_ref::<gst::ClockTime>()
            .unwrap();
        let running_time = segment.to_running_time(buffer.pts().unwrap()).unwrap();

        // The clock time the frame is rendered at
        let clock_time = pipeline.base_time().unwrap() + running_time;
        assert_eq!(
            decode_row(&frame, pattern::CLOCK_ROW),
            Some(pattern::encode_clock_time(clock_time.nseconds()))
        );
    }

    pipeline.set_state(gst::State::Null).unwrap();
}
