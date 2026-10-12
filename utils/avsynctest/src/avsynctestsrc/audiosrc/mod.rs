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

mod imp;

// Internal to avsynctestsrc, not registered as an element factory.
glib::wrapper! {
    pub struct AudioSrc(ObjectSubclass<imp::AudioSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl AudioSrc {
    pub(super) fn new(shared: Arc<Shared>) -> Self {
        let obj = glib::Object::builder::<Self>()
            .property("name", "audiosrc")
            .build();
        obj.imp().set_shared(shared);
        obj
    }
}

pub(super) fn caps() -> gst::Caps {
    gst_audio::AudioCapsBuilder::new_interleaved()
        .format_list([
            gst_audio::AUDIO_FORMAT_S16,
            gst_audio::AUDIO_FORMAT_S32,
            gst_audio::AUDIO_FORMAT_F32,
        ])
        .build()
}
