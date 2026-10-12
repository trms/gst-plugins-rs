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
use gst_video::prelude::*;

use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use super::super::{Settings, Shared, Stream};
use crate::pattern::{self, CodeLayout, EventGrid};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "avsynctestsrc-video",
        gst::DebugColorFlags::empty(),
        Some("A/V Sync Test Source, video"),
    )
});

#[derive(Default)]
struct State {
    settings: Settings,
    info: Option<gst_video::VideoInfo>,
    grid: Option<EventGrid>,
    next_frame: u64,
    /// Start of the last seek, applied on the next buffer once the framerate is known.
    seek_start: Option<u64>,
    stop_time: Option<u64>,
}

#[derive(Default)]
pub struct VideoSrc {
    shared: OnceLock<Arc<Shared>>,
    state: Mutex<State>,
}

impl VideoSrc {
    pub(super) fn set_shared(&self, shared: Arc<Shared>) {
        self.shared.set(shared).unwrap();
    }

    fn shared(&self) -> &Shared {
        self.shared.get().unwrap()
    }

    /// The clock time at which the frame with timestamp `pts` is to be rendered, once playing.
    fn clock_time(&self, pts: u64) -> Option<u64> {
        let obj = self.obj();
        obj.clock()?;
        let base_time = obj.base_time()?;
        let segment = obj.segment().downcast::<gst::ClockTime>().ok()?;
        let running_time = segment.to_running_time(gst::ClockTime::from_nseconds(pts))?;

        Some((base_time + running_time).nseconds())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for VideoSrc {
    const NAME: &'static str = "GstAvSyncTestVideoSrc";
    type Type = super::VideoSrc;
    type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for VideoSrc {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_live(false);
        obj.set_format(gst::Format::Time);
    }
}

impl GstObjectImpl for VideoSrc {}

impl ElementImpl for VideoSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "A/V Sync Test Video Source",
                "Source/Video",
                "Video half of avsynctestsrc",
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

impl BaseSrcImpl for VideoSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State {
            settings: self.shared().settings(),
            ..Default::default()
        };
        self.shared().set_flushing(Stream::Video, false);

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::default();
        self.shared().set_framerate(None);

        Ok(())
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {caps}"))?;

        let fps = info.fps();
        let mut state = self.state.lock().unwrap();
        let grid = EventGrid::new(
            state.settings.interval,
            fps.numer() as u32,
            fps.denom() as u32,
        );

        if grid.frame_duration() == 0 {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ["framerate {fps} is too high for frames to have a duration"]
            );
            return Err(gst::loggable_error!(CAT, "Frames have no duration"));
        }

        if grid.frame_duration() > state.settings.interval {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ["interval is shorter than one frame at {fps}"]
            );
            return Err(gst::loggable_error!(CAT, "Interval shorter than a frame"));
        }

        gst::debug!(CAT, imp = self, "Configuring for caps {caps}");

        if let Some(old_grid) = state.grid
            && old_grid != grid
        {
            state.next_frame = grid.first_frame_at_or_after(old_grid.frame_pts(state.next_frame));
        }

        let latency_changed = state.grid.map(|g| g.frame_duration()) != Some(grid.frame_duration());
        state.info = Some(info);
        state.grid = Some(grid);
        drop(state);

        self.shared().set_framerate(Some(grid.framerate()));

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
            s.fixate_field_nearest_int("width", 1280);
            s.fixate_field_nearest_int("height", 720);
            s.fixate_field_nearest_fraction("framerate", gst::Fraction::new(30, 1));
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
            let Some(grid) = self.state.lock().unwrap().grid else {
                return false;
            };

            // A frame is complete one frame after its timestamp. There is no upper limit, as
            // timestamps are on the grid whenever the frame gets made, so nothing is dropped
            // however much latency the pipeline configures
            let latency = gst::ClockTime::from_nseconds(grid.frame_duration());
            gst::debug!(CAT, imp = self, "Reporting latency {latency}");
            q.set(true, latency, None);
            return true;
        }

        BaseSrcImplExt::parent_query(self, query)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        self.shared().set_flushing(Stream::Video, true);
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        self.shared().set_flushing(Stream::Video, false);
        Ok(())
    }
}

impl PushSrcImpl for VideoSrc {
    fn create(
        &self,
        _buffer: Option<&mut gst::BufferRef>,
    ) -> Result<CreateSuccess, gst::FlowError> {
        // Unlinked output is dropped by the bin, don't generate it faster than real time
        if !self.obj().is_live()
            && !self
                .shared()
                .wait_linked(Stream::Video, self.obj().upcast_ref())
        {
            gst::debug!(CAT, imp = self, "Flushing while waiting to be linked");
            return Err(gst::FlowError::Flushing);
        }

        let mut state = self.state.lock().unwrap();

        let (Some(info), Some(grid)) = (state.info.clone(), state.grid) else {
            gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Not negotiated"]);
            return Err(gst::FlowError::NotNegotiated);
        };

        if let Some(start) = state.seek_start.take() {
            state.next_frame = grid.first_frame_at_or_after(start);
        }

        let frame = state.next_frame;
        let pts = grid.frame_pts(frame);
        if state.stop_time.is_some_and(|stop| pts >= stop) {
            gst::debug!(CAT, imp = self, "Reached stop time");
            return Err(gst::FlowError::Eos);
        }

        let duration = grid.frame_pts(frame + 1) - pts;
        let event = grid.event_for_frame(frame);
        let flash = grid.frame_is_flash(frame, state.settings.flash_duration);
        let encode_clock_time = state.settings.encode_clock_time;

        if frame == grid.event_frame(event) {
            gst::debug!(
                CAT,
                imp = self,
                "Event {event} at frame {frame}, {}",
                gst::ClockTime::from_nseconds(pts)
            );
        }

        state.next_frame += 1;
        drop(state);

        let pts = if self.obj().is_live() {
            let src = self.obj();
            let pts = pts + self.shared().live_offset(src.upcast_ref());
            if !self
                .shared()
                .wait_until(Stream::Video, src.upcast_ref(), pts)
            {
                gst::debug!(CAT, imp = self, "Flushing while waiting for the clock");
                return Err(gst::FlowError::Flushing);
            }
            pts
        } else {
            pts
        };

        let clock_time = encode_clock_time
            .then(|| self.clock_time(pts))
            .flatten()
            .map(pattern::encode_clock_time);

        let mut buffer = gst::Buffer::with_size(info.size()).map_err(|_| gst::FlowError::Error)?;
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_nseconds(pts));
            buffer.set_duration(gst::ClockTime::from_nseconds(duration));
            buffer.set_offset(frame);
            buffer.set_offset_end(frame + 1);

            let mut vframe = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info)
                .map_err(|_| gst::FlowError::Error)?;
            render(
                &mut vframe,
                flash,
                [Some(event as u32), Some(frame as u32), clock_time],
            )?;
        }

        Ok(CreateSuccess::NewBuffer(buffer))
    }
}

const LUMA_WHITE: u8 = 235;
const LUMA_BLACK: u8 = 16;
const CHROMA_NEUTRAL: u8 = 128;

fn luma(white: bool) -> u8 {
    if white { LUMA_WHITE } else { LUMA_BLACK }
}

/// Draws the flash area and the code rows, black for `None`. See [`CodeLayout`].
fn render(
    frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    flash: bool,
    codes: [Option<u32>; pattern::CODE_ROWS as usize],
) -> Result<(), gst::FlowError> {
    let width = frame.width();
    let height = frame.height();
    let format = frame.format();
    let layout = CodeLayout::new(width, height);

    let flash_line = vec![flash; width as usize];
    let code_lines = codes.map(|code| match code {
        Some(value) => (0..width)
            .map(|x| pattern::cell_is_white(layout.cell_at(x), value))
            .collect::<Vec<_>>(),
        None => vec![false; width as usize],
    });
    let line_at = |y: u32| match layout.row_at(y) {
        None => &flash_line,
        Some(row) => &code_lines[row as usize],
    };

    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;

    for (y, row) in data.chunks_mut(stride).take(height as usize).enumerate() {
        let line = line_at(y as u32);

        match format {
            gst_video::VideoFormat::I420 | gst_video::VideoFormat::Nv12 => {
                for (p, &white) in row.iter_mut().zip(line) {
                    *p = luma(white);
                }
            }
            gst_video::VideoFormat::Uyvy => {
                for (p, pair) in row.as_chunks_mut::<4>().0.iter_mut().zip(line.chunks(2)) {
                    let second = pair.get(1).unwrap_or(&pair[0]);
                    p.copy_from_slice(&[
                        CHROMA_NEUTRAL,
                        luma(pair[0]),
                        CHROMA_NEUTRAL,
                        luma(*second),
                    ]);
                }
            }
            gst_video::VideoFormat::Bgrx => {
                for (p, &white) in row.as_chunks_mut::<4>().0.iter_mut().zip(line) {
                    let v = if white { 255 } else { 0 };
                    p.copy_from_slice(&[v, v, v, 255]);
                }
            }
            _ => unreachable!(),
        }
    }

    // Chroma planes of the planar formats are neutral everywhere
    for plane in 1..frame.n_planes() {
        frame
            .plane_data_mut(plane)
            .map_err(|_| gst::FlowError::Error)?
            .fill(CHROMA_NEUTRAL);
    }

    Ok(())
}
