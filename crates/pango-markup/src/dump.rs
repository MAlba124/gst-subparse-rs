// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Conformance dump in the format of pango's tests/markup-parse.c.
//!
//! Sections separated by `\n\n---\n\n`: the flattened text, the attribute
//! list (`print_attr_list` from test-common.c), the per-range font
//! description from `pango_attr_iterator_get_font` (with the driver's quirk
//! of reusing one description across ranges without resetting it), and the
//! accelerator char when one was found. This exists so the vendored pango
//! test corpus diffs byte-for-byte against our output.

use crate::attr::{Attr, AttrKind, attr_to_string};
use crate::fontdesc::{self, FontDescription};
use crate::markup::Parsed;

fn stretch_to_width(stretch: i32) -> i32 {
    match stretch {
        0 => 500,
        1 => 625,
        2 => 750,
        3 => 875,
        5 => 1125,
        6 => 1250,
        7 => 1500,
        8 => 2000,
        _ => 1000,
    }
}

/// `pango_attr_iterator_get_font`. `desc` accumulates like the C, `language`
/// is the out param.
fn get_font(attrs: &[&Attr], desc: &mut FontDescription, language: &mut Option<String>) {
    let mut mask = 0u32;
    let mut have_language = false;
    let mut have_scale = false;
    let mut scale = 0.0f64;

    *language = None;

    // Stack top (highest priority) first.
    for attr in attrs.iter().rev() {
        match &attr.kind {
            AttrKind::FontDesc(d) => {
                let new_mask = d.mask & !mask;
                mask |= new_mask;
                desc.unset_fields(new_mask);
                let mut masked = d.clone();
                masked.mask = new_mask;
                desc.merge(&masked, false);
            }
            AttrKind::Family(f) => {
                if mask & fontdesc::MASK_FAMILY == 0 {
                    mask |= fontdesc::MASK_FAMILY;
                    desc.family = Some(f.clone());
                    desc.mask |= fontdesc::MASK_FAMILY;
                }
            }
            AttrKind::Style(v) => {
                if mask & fontdesc::MASK_STYLE == 0 {
                    mask |= fontdesc::MASK_STYLE;
                    desc.style = *v;
                    desc.mask |= fontdesc::MASK_STYLE;
                }
            }
            AttrKind::Variant(v) => {
                if mask & fontdesc::MASK_VARIANT == 0 {
                    mask |= fontdesc::MASK_VARIANT;
                    desc.variant = *v;
                    desc.mask |= fontdesc::MASK_VARIANT;
                }
            }
            AttrKind::Weight(v) => {
                if mask & fontdesc::MASK_WEIGHT == 0 {
                    mask |= fontdesc::MASK_WEIGHT;
                    desc.weight = *v;
                    desc.mask |= fontdesc::MASK_WEIGHT;
                }
            }
            AttrKind::Stretch(v) => {
                if mask & fontdesc::MASK_WIDTH == 0 {
                    mask |= fontdesc::MASK_WIDTH;
                    desc.width = stretch_to_width(*v);
                    desc.mask |= fontdesc::MASK_WIDTH;
                }
            }
            AttrKind::Width(v) => {
                if mask & fontdesc::MASK_WIDTH == 0 {
                    mask |= fontdesc::MASK_WIDTH;
                    desc.width = *v;
                    desc.mask |= fontdesc::MASK_WIDTH;
                }
            }
            AttrKind::Size(v) => {
                if mask & fontdesc::MASK_SIZE == 0 {
                    mask |= fontdesc::MASK_SIZE;
                    desc.set_size(*v);
                }
            }
            AttrKind::Scale(v) => {
                if !have_scale {
                    have_scale = true;
                    scale = *v;
                }
            }
            AttrKind::Language(l) if !have_language => {
                have_language = true;
                *language = Some(l.clone());
            }
            _ => {}
        }
    }

    if have_scale {
        let size = scale * desc.size as f64;
        if desc.size_is_absolute {
            desc.set_absolute_size(size as i32);
        } else {
            desc.set_size(size as i32);
        }
    }
}

/// The whole expected-file content for a successful parse.
pub fn dump(parsed: &Parsed) -> String {
    let mut out = String::new();

    out.push_str(&parsed.text);
    out.push_str("\n\n---\n\n");

    // print_attr_list.
    let mut it = parsed.attrs.iter_ranges();
    loop {
        let (start, end) = it.range();
        out.push_str(&format!("range {} {}\n", start, end));
        for attr in it.get_attrs() {
            out.push_str(&attr_to_string(attr));
            out.push('\n');
        }
        if !it.next_range() {
            break;
        }
    }

    out.push_str("\n\n---\n\n");

    // The font-description walk. One desc for all ranges, never reset.
    let mut desc = FontDescription {
        mask: 0,
        ..FontDescription::default()
    };
    let mut it = parsed.attrs.iter_ranges();
    loop {
        let (start, end) = it.range();
        let mut language = None;
        let attrs = it.stack_attrs();
        get_font(&attrs, &mut desc, &mut language);
        out.push_str(&format!(
            "[{}:{}] {} {}\n",
            start,
            end,
            language.as_deref().unwrap_or("(null)"),
            desc.to_description_string()
        ));
        if !it.next_range() {
            break;
        }
    }

    if let Some(accel) = parsed.accel_char {
        out.push_str("\n\n---\n\n");
        out.push(accel);
    }

    out
}
