use crate::{
    audio_player::AudioPlayer,
    config::PlaybackConfig,
    decoder::{hw::try_enable_hw_decoder, pts_to_us},
    error::{PlayerError, Result},
    frame_buffer::{Frame, FrameBuffer},
    state::PlaybackState,
};
use ffmpeg::{
    codec::context::Context,
    software::scaling::{Context as Scaler, Flags},
    util::{format::Pixel, frame::Video},
};
use ffmpeg_next as ffmpeg;
use std::{
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use ffmpeg::ffi::av_hwframe_transfer_data;

pub fn video_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    vparams: ffmpeg::codec::Parameters,
    vtb: ffmpeg::Rational,
    buffer: FrameBuffer,
    audio: Option<AudioPlayer>,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
) -> Result<()> {
    // Build the decoder context, then attempt to attach a HW device BEFORE
    // calling avcodec_open2 (which happens inside `.open_as(codec)`).
    let ctx = Context::from_parameters(vparams)?;
    let codec_id = ctx.id();
    let codec = ffmpeg::decoder::find(codec_id).ok_or(PlayerError::DecoderNotFound)?;

    let mut dec_wrap = ctx.decoder();
    let hw_pix_fmt: Option<i32> =
        unsafe { try_enable_hw_decoder(dec_wrap.as_mut_ptr(), codec.as_ptr()) };

    let mut vdec = dec_wrap.open_as(codec).and_then(|o| o.video())?;

    let out_w = vdec.width();
    let out_h = vdec.height();
    let out_pix = Pixel::RGBA;

    // Scaler is initialised lazily on the first decoded frame: with HW
    // decoding we don't know the post-transfer SW pixel format up front, and
    // even with SW it can differ from `vdec.format()` after stream changes.
    let mut scaler: Option<(Scaler, Pixel, u32, u32)> = None;

    let mut vyuv = Video::empty();
    let mut vout = Video::empty();

    // Normalize PTS into a 0-based timeline so it can be compared with the
    // audio sample-clock (also 0-based).
    let mut first_pts_us: Option<u64> = None;
    // Wall-clock anchor used only as a fallback before audio clock is live.
    let mut wall_anchor: Option<(Instant, u64)> = None;

    for packet in rx {
        if state.shutdown() {
            break;
        }
        while state.paused() {
            if state.shutdown() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if let Err(e) = vdec.send_packet(&packet) {
            tracing::warn!(target: "video", "send_packet error: {e}");
            continue;
        }
        drain_video(
            &mut vdec,
            &mut scaler,
            &mut vyuv,
            &mut vout,
            &mut first_pts_us,
            &mut wall_anchor,
            vtb,
            &buffer,
            out_pix,
            out_w,
            out_h,
            hw_pix_fmt,
            &audio,
            &state,
            &cfg,
        );
    }

    if let Err(e) = vdec.send_eof() {
        tracing::warn!(target: "video", "send_eof error: {e}");
    }
    drain_video(
        &mut vdec,
        &mut scaler,
        &mut vyuv,
        &mut vout,
        &mut first_pts_us,
        &mut wall_anchor,
        vtb,
        &buffer,
        out_pix,
        out_w,
        out_h,
        hw_pix_fmt,
        &audio,
        &state,
        &cfg,
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_video(
    vdec: &mut ffmpeg::codec::decoder::Video,
    scaler: &mut Option<(Scaler, Pixel, u32, u32)>,
    vyuv: &mut Video,
    vout: &mut Video,
    first_pts_us: &mut Option<u64>,
    wall_anchor: &mut Option<(Instant, u64)>,
    vtb: ffmpeg::Rational,
    buffer: &FrameBuffer,
    out_pix: Pixel,
    out_w: u32,
    out_h: u32,
    hw_pix_fmt: Option<i32>,
    audio: &Option<AudioPlayer>,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
) {
    while vdec.receive_frame(vyuv).is_ok() {
        if state.shutdown() {
            return;
        }

        // If this frame is in HW memory, transfer to system memory before
        // scaling. Compare against the raw AVFrame format field, not
        // ffmpeg-next's Pixel enum — the enum discriminants don't match the
        // FFmpeg AVPixelFormat numeric values.
        let raw_fmt = unsafe { (*vyuv.as_ptr()).format };
        let mut sw_frame = Video::empty();
        let src: &Video = match hw_pix_fmt {
            Some(want) if raw_fmt == want => unsafe {
                let r = av_hwframe_transfer_data(sw_frame.as_mut_ptr(), vyuv.as_ptr(), 0);
                if r < 0 {
                    tracing::warn!(target: "video", "hw transfer failed (code {r})");
                    continue;
                }
                &sw_frame
            },
            _ => &*vyuv,
        };

        let src_fmt = src.format();
        let src_w = src.width();
        let src_h = src.height();

        let need_init = match scaler {
            Some((_, f, w, h)) => *f != src_fmt || *w != src_w || *h != src_h,
            None => true,
        };
        if need_init {
            match Scaler::get(
                src_fmt,
                src_w,
                src_h,
                out_pix,
                out_w,
                out_h,
                Flags::BILINEAR,
            ) {
                Ok(s) => *scaler = Some((s, src_fmt, src_w, src_h)),
                Err(e) => {
                    tracing::warn!(target: "video", "scaler init error: {e}");
                    continue;
                }
            }
        }
        let s = match scaler.as_mut() {
            Some((s, _, _, _)) => s,
            None => continue,
        };
        if let Err(e) = s.run(src, vout) {
            tracing::warn!(target: "video", "scaling error: {e}");
            continue;
        }

        let abs_pts = pts_to_us(vyuv.timestamp().unwrap_or(0), vtb.0, vtb.1)
            .unwrap_or(0)
            .max(0) as u64;
        let first = *first_pts_us.get_or_insert(abs_pts);
        let ts_us = abs_pts.saturating_sub(first);

        // Pace presentation against audio clock (or wall-clock fallback).
        // Drop frame if we're far past its display time.
        if pace_video(audio, wall_anchor, ts_us, state, cfg) {
            continue;
        }

        let plane = vout.data(0);
        let pixels: Arc<[u8]> = Vec::from(plane).into();
        buffer.push(Arc::new(Frame {
            data: pixels,
            width: out_w,
            height: out_h,
            ts_us,
        }));

        static FRAME_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = FRAME_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n.is_multiple_of(30) {
            let clk = audio.as_ref().and_then(|a| a.clock_us()).unwrap_or(u64::MAX);
            tracing::debug!(target: "video",
                "pushed frame #{n} ts_us={ts_us} audio_clock_us={clk}");
        }
    }
}

/// Block until the frame's presentation time arrives, paced by the audio
/// master clock when available, falling back to wall-clock relative to the
/// first frame. Returns `true` if the frame is so late it should be dropped.
fn pace_video(
    audio: &Option<AudioPlayer>,
    wall_anchor: &mut Option<(Instant, u64)>,
    ts_us: u64,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
) -> bool {
    if let Some(ap) = audio.as_ref() {
        if ap.clock_us().is_some() {
            *wall_anchor = None;
            let wait_start = Instant::now();
            loop {
                if state.shutdown() {
                    return false;
                }
                while state.paused() {
                    if state.shutdown() {
                        return false;
                    }
                    std::thread::sleep(cfg.pace_slice);
                }
                let now = ap.clock_us().unwrap_or(0) as i64;
                let diff = ts_us as i64 - now;
                if diff <= 0 {
                    return -diff > cfg.late_drop_us;
                }
                // Safety bail-out in case the audio clock stalls.
                if wait_start.elapsed() > Duration::from_secs(2) {
                    tracing::warn!(target: "video", "pace bailout: ts_us={ts_us} clock={now} diff_ms={}", diff / 1000);
                    return false;
                }
                let sleep_us = (diff as u64).min(cfg.pace_slice.as_micros() as u64);
                std::thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    // Wall-clock fallback (no audio configured yet, or audio not started).
    let now = Instant::now();
    let (start_inst, first_pts) = match *wall_anchor {
        Some(a) => a,
        None => {
            *wall_anchor = Some((now, ts_us));
            return false;
        }
    };
    let elapsed = now.duration_since(start_inst);
    let target = Duration::from_micros(ts_us.saturating_sub(first_pts));
    if target > elapsed {
        let sleep = (target - elapsed).min(cfg.pace_slice);
        std::thread::sleep(sleep);
        // Loop until target reached or shutdown — keep responsive.
        while !state.shutdown() {
            let elapsed = Instant::now().duration_since(start_inst);
            if elapsed >= target {
                break;
            }
            std::thread::sleep((target - elapsed).min(cfg.pace_slice));
        }
    } else if (elapsed - target).as_micros() as i64 > cfg.late_drop_us {
        return true;
    }
    false
}
