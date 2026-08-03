// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct SsaParse(ObjectSubclass<imp::SsaParse>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsssaparse",
        gst::Rank::NONE,
        SsaParse::static_type(),
    )
}
