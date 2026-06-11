use ffmpeg_next as ffmpeg;
use std::ffi::CStr;

use ffmpeg::ffi::{
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_hwdevice_get_type_name,
    avcodec_get_hw_config, AVBufferRef, AVCodec, AVCodecContext, AVPixelFormat,
    AVPixelFormat::AV_PIX_FMT_NONE, AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
};

/// Iterate the codec's HW configurations, create the first device we can,
/// wire it onto the AVCodecContext, and install a `get_format` callback that
/// picks the matching HW pixel format. Returns the HW pix_fmt on success so
/// the caller can detect HW frames coming back out of the decoder.
pub unsafe fn try_enable_hw_decoder(
    ctx: *mut AVCodecContext,
    codec: *const AVCodec,
) -> Option<i32> {
    if codec.is_null() {
        return None;
    }
    let mut i: i32 = 0;
    loop {
        let cfg = avcodec_get_hw_config(codec, i);
        if cfg.is_null() {
            return None;
        }
        let cfg_ref = &*cfg;
        let methods = cfg_ref.methods as u32;
        if methods & (AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as u32) != 0 {
            let mut dev: *mut AVBufferRef = std::ptr::null_mut();
            let r = av_hwdevice_ctx_create(
                &mut dev,
                cfg_ref.device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if r >= 0 && !dev.is_null() {
                (*ctx).hw_device_ctx = av_buffer_ref(dev);
                av_buffer_unref(&mut dev);
                let want_fmt = cfg_ref.pix_fmt as i32;
                (*ctx).opaque = want_fmt as usize as *mut std::ffi::c_void;
                (*ctx).get_format = Some(get_hw_format);
                let name_ptr = av_hwdevice_get_type_name(cfg_ref.device_type);
                let name = if name_ptr.is_null() {
                    "?".to_string()
                } else {
                    CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
                };
                tracing::info!(target: "video", "hw decode enabled ({name})");
                return Some(want_fmt);
            }
        }
        i += 1;
    }
}

unsafe extern "C" fn get_hw_format(
    ctx: *mut AVCodecContext,
    fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    let want = (*ctx).opaque as usize as i32;
    let mut p = fmts;
    while (*p as i32) != (AV_PIX_FMT_NONE as i32) {
        if (*p as i32) == want {
            return *p;
        }
        p = p.add(1);
    }
    // No HW format offered — fall back to the first SW format ffmpeg suggests
    // (which is what would have happened without us).
    *fmts
}
