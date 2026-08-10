// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Cue-IR output support: the `text-format` property value and the buffer
//! meta that carries a [`CueIr`] alongside plain-text buffers.
//!
//! Both parser elements default to the C `subparse` behaviour (styling inline
//! as pango markup in the buffer text). Setting `text-format=cue-ir` switches
//! them to pushing plain UTF-8 text buffers (`text/x-raw, format=utf8`) with a
//! [`CueIrMeta`] attached, whose payload is the plain-old-data
//! [`subparse_formats::ir`] structs. A custom renderer (parley/vello_cpu or
//! anything else) pulls the meta off the buffer and draws the styled cue,
//! while elements that only understand `text/x-raw` still get readable text.
//!
//! In-process consumers downcast with `buffer.meta::<CueIrMeta>()` — this
//! crate builds as an rlib precisely so an application can link it and name
//! these types.

use gst::glib;
use gst::meta::{MetaAPI, MetaAPIExt};

use subparse_formats::ir::CueIr;

/// How the parser elements deliver styling, selected by their `text-format`
/// property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstRsSubParseTextFormat")]
pub enum TextFormat {
    /// Styling inline in the buffer text as pango markup
    /// (`text/x-raw, format=pango-markup`), the upstream C behaviour.
    #[default]
    #[enum_value(
        name = "Pango markup: styling inline in the buffer text",
        nick = "pango-markup"
    )]
    PangoMarkup = 0,
    /// Plain UTF-8 buffer text (`text/x-raw, format=utf8`) with a
    /// [`CueIrMeta`] carrying the structured styling.
    #[enum_value(
        name = "Cue IR: plain UTF-8 text plus a CueIrMeta with structured styling",
        nick = "cue-ir"
    )]
    CueIr = 1,
}

/// A buffer meta holding the styled-cue IR for the text in the buffer.
///
/// The meta owns a plain Rust [`CueIr`] value (no serialization involved), so
/// reading it back is free. It survives buffer copies (the transform function
/// clones it onto the destination).
#[repr(transparent)]
pub struct CueIrMeta(imp::CueIrMeta);

// SAFETY: CueIr is a plain owned value (Strings/Vecs of POD), Send + Sync.
unsafe impl Send for CueIrMeta {}
unsafe impl Sync for CueIrMeta {}

impl CueIrMeta {
    /// Attach `ir` to `buffer`.
    pub fn add(
        buffer: &mut gst::BufferRef,
        ir: CueIr,
    ) -> gst::meta::MetaRefMut<'_, Self, gst::meta::Standalone> {
        unsafe {
            // `gst_buffer_add_meta` hands the params pointer to our init
            // function, which moves the value out with `ptr::read`, so it must
            // not be dropped here.
            let mut params = std::mem::ManuallyDrop::new(imp::CueIrMetaParams { ir });

            let meta = gst::ffi::gst_buffer_add_meta(
                buffer.as_mut_ptr(),
                imp::cue_ir_meta_get_info(),
                &mut *params as *mut imp::CueIrMetaParams as glib::ffi::gpointer,
            ) as *mut imp::CueIrMeta;

            Self::from_mut_ptr(buffer, meta)
        }
    }

    /// The styled cue this buffer's text corresponds to.
    pub fn ir(&self) -> &CueIr {
        &self.0.ir
    }
}

unsafe impl MetaAPI for CueIrMeta {
    type GstType = imp::CueIrMeta;

    fn meta_api() -> glib::Type {
        imp::cue_ir_meta_api_get_type()
    }
}

impl std::fmt::Debug for CueIrMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CueIrMeta").field("ir", self.ir()).finish()
    }
}

mod imp {
    use glib::translate::{IntoGlib, from_glib};
    use gst::glib;
    use std::sync::LazyLock;
    use std::{mem, ptr};

    use subparse_formats::ir::CueIr;

    pub(super) struct CueIrMetaParams {
        pub ir: CueIr,
    }

    /// The C-layout struct `gst_buffer_add_meta` allocates: the required
    /// `GstMeta` header followed by our Rust payload, which init/free below
    /// construct and drop in place.
    #[repr(C)]
    pub struct CueIrMeta {
        parent: gst::ffi::GstMeta,
        pub(super) ir: CueIr,
    }

    pub(super) fn cue_ir_meta_api_get_type() -> glib::Type {
        static TYPE: LazyLock<glib::Type> = LazyLock::new(|| unsafe {
            let t = from_glib(gst::ffi::gst_meta_api_type_register(
                c"GstRsSubParseCueIrMetaAPI".as_ptr() as *const _,
                [ptr::null::<std::os::raw::c_char>()].as_ptr() as *mut *const _,
            ));

            assert_ne!(t, glib::Type::INVALID);

            t
        });

        *TYPE
    }

    unsafe extern "C" fn cue_ir_meta_init(
        meta: *mut gst::ffi::GstMeta,
        params: glib::ffi::gpointer,
        _buffer: *mut gst::ffi::GstBuffer,
    ) -> glib::ffi::gboolean {
        assert!(!params.is_null());

        unsafe {
            let meta = &mut *(meta as *mut CueIrMeta);
            let params = ptr::read(params as *const CueIrMetaParams);

            ptr::write(&mut meta.ir, params.ir);
        }

        true.into_glib()
    }

    unsafe extern "C" fn cue_ir_meta_free(
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
    ) {
        unsafe {
            let meta = &mut *(meta as *mut CueIrMeta);
            ptr::drop_in_place(&mut meta.ir);
        }
    }

    unsafe extern "C" fn cue_ir_meta_transform(
        dest: *mut gst::ffi::GstBuffer,
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
        _type_: glib::ffi::GQuark,
        _data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean {
        unsafe {
            let meta = &*(meta as *const CueIrMeta);
            super::CueIrMeta::add(gst::BufferRef::from_mut_ptr(dest), meta.ir.clone());
        }

        true.into_glib()
    }

    pub(super) fn cue_ir_meta_get_info() -> *const gst::ffi::GstMetaInfo {
        struct MetaInfo(ptr::NonNull<gst::ffi::GstMetaInfo>);
        unsafe impl Send for MetaInfo {}
        unsafe impl Sync for MetaInfo {}

        static META_INFO: LazyLock<MetaInfo> = LazyLock::new(|| unsafe {
            MetaInfo(
                ptr::NonNull::new(gst::ffi::gst_meta_register(
                    cue_ir_meta_api_get_type().into_glib(),
                    c"GstRsSubParseCueIrMeta".as_ptr() as *const _,
                    mem::size_of::<CueIrMeta>(),
                    Some(cue_ir_meta_init),
                    Some(cue_ir_meta_free),
                    Some(cue_ir_meta_transform),
                ) as *mut gst::ffi::GstMetaInfo)
                .expect("Failed to register CueIrMeta"),
            )
        });

        META_INFO.0.as_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subparse_formats::ir::Span;

    fn init() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    #[test]
    fn add_read_copy_and_drop() {
        init();

        let ir = CueIr::from_pango_markup("<i>Six</i>");
        let mut buffer = gst::Buffer::from_slice("Six".as_bytes());
        CueIrMeta::add(buffer.get_mut().unwrap(), ir.clone());

        let meta = buffer.meta::<CueIrMeta>().expect("meta present");
        assert_eq!(meta.ir(), &ir);
        assert_eq!(meta.ir().plain_text(), "Six");

        // The transform function must carry the meta across a (deep) copy.
        let copy = buffer.copy_deep().unwrap();
        let meta = copy.meta::<CueIrMeta>().expect("meta copied");
        assert_eq!(meta.ir(), &ir);

        drop(buffer);
        drop(copy);
    }

    #[test]
    fn heap_payload_survives_the_meta_lifecycle() {
        init();

        // A cue with plenty of heap-owned data, so a bad init/free would be
        // loud under a leak checker and likely to crash outright.
        let mut ir =
            CueIr::from_pango_markup("<v Fred><c.yellow>hello</c> <ruby>a<rt>b</rt></ruby>");
        ir.lines[0].spans.push(Span::plain("x".repeat(4096)));

        let mut buffer = gst::Buffer::from_slice(Vec::from("payload"));
        CueIrMeta::add(buffer.get_mut().unwrap(), ir.clone());
        assert_eq!(buffer.meta::<CueIrMeta>().unwrap().ir(), &ir);
        drop(buffer);
    }
}
