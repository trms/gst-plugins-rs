// Copyright (C) 2026 Ray Tiley <ray.tiley@trms.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;

use std::sync::Arc;

use super::Shared;
use crate::pattern;

mod imp;

// Internal to avsynctestsrc, not registered as an element factory.
glib::wrapper! {
    pub struct VideoSrc(ObjectSubclass<imp::VideoSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl VideoSrc {
    pub(super) fn new(shared: Arc<Shared>) -> Self {
        let obj = glib::Object::builder::<Self>()
            .property("name", "videosrc")
            .build();
        obj.imp().set_shared(shared);
        obj
    }
}

pub(super) fn caps() -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format_list([
            gst_video::VideoFormat::I420,
            gst_video::VideoFormat::Nv12,
            gst_video::VideoFormat::Uyvy,
            gst_video::VideoFormat::Bgrx,
        ])
        .width_range(pattern::MIN_WIDTH as i32..=i32::MAX)
        .height_range(pattern::MIN_HEIGHT as i32..=i32::MAX)
        .framerate_range(gst::Fraction::new(1, 1)..=gst::Fraction::new(i32::MAX, 1))
        .build()
}
