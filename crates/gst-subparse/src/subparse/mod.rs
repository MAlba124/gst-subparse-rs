// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::glib;
use gst::prelude::*;

pub(crate) mod imp;

glib::wrapper! {
    pub struct SubParse(ObjectSubclass<imp::SubParse>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rssubparse",
        gst::Rank::NONE,
        SubParse::static_type(),
    )
}
