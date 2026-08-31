use crate::{
    audio_player::AudioPlayer,
    config::PlaybackConfig,
    decoder::{demux::TaggedPacket, hw::try_enable_hw_decoder, pts_to_us},
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
    rx: mpsc::Receiver<TaggedPacket>,
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
    // audio sample-clock (also 0-based). NOT reset on seek — the timeline
    // stays continuous so post-seek positions read as true stream positions.
    let mut first_pts_us: Option<u64> = None;
    // Wall-clock anchor used only as a fallback before audio clock is live.
    let mut wall_anchor: Option<(Instant, u64)> = None;
    // Seek epoch of the packets currently being decoded.
    let mut epoch: u64 = 0;

    for (pkt_epoch, packet) in rx {
        if state.shutdown() {
            break;
        }
        // Stale pre-seek packet still in the queue: drain without decoding.
        if pkt_epoch < state.epoch() {
            continue;
        }
        if pkt_epoch != epoch {
            // First packet after a seek: discard decoder-internal state so we
            // start clean from the new keyframe.
            vdec.flush();
            wall_anchor = None;
            epoch = pkt_epoch;
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
            epoch,
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
        epoch,
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
    epoch: u64,
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
        // Drop frame if we're far past its display time or a seek superseded
        // this epoch while we were waiting.
        if pace_video(audio, wall_anchor, ts_us, state, cfg, epoch) {
            continue;
        }

        // `data(0)` spans `stride * height`, and the scaler's output frame is
        // allocated with 32-byte row alignment — so whenever `out_w * 4` is
        // not a multiple of 32 the rows carry padding the consumer knows
        // nothing about. Pack to a tight `out_w * 4` stride here.
        let Some(pixels) = pack_rows(
            vout.data(0),
            vout.stride(0),
            out_w as usize * 4,
            out_h as usize,
        ) else {
            // Unreachable given ffmpeg's data() length contract; debug so a
            // genuinely malformed plane can't spam a line per frame.
            tracing::debug!(target: "video",
                "short frame plane: stride={} height={out_h}", vout.stride(0));
            continue;
        };
        let pixels: Arc<[u8]> = pixels.into();
        buffer.push(Arc::new(Frame {
            data: pixels,
            width: out_w,
            height: out_h,
            ts_us,
        }));

        static FRAME_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = FRAME_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n.is_multiple_of(30) {
            let clk = audio
                .as_ref()
                .and_then(|a| a.clock_us())
                .unwrap_or(u64::MAX);
            tracing::debug!(target: "video",
                "pushed frame #{n} ts_us={ts_us} audio_clock_us={clk}");
        }
    }
}

/// Copy `height` rows of `row_bytes` out of a `stride`-pitched plane into a
/// tightly packed buffer. Returns `None` if the plane is shorter than the
/// rows imply. Borrows nothing from ffmpeg; see tests for the invariant.
fn pack_rows(plane: &[u8], stride: usize, row_bytes: usize, height: usize) -> Option<Vec<u8>> {
    if stride < row_bytes || plane.len() < stride * height {
        return None;
    }
    if stride == row_bytes {
        return Some(plane[..row_bytes * height].to_vec());
    }
    let mut packed = Vec::with_capacity(row_bytes * height);
    for row in plane.chunks_exact(stride).take(height) {
        packed.extend_from_slice(&row[..row_bytes]);
    }
    Some(packed)
}

/// Block until the frame's presentation time arrives, paced by the audio
/// master clock when available, falling back to wall-clock relative to the
/// first frame. Returns `true` if the frame should be dropped — because it's
/// far past its display time, or because a seek invalidated its epoch while
/// we were waiting.
fn pace_video(
    audio: &Option<AudioPlayer>,
    wall_anchor: &mut Option<(Instant, u64)>,
    ts_us: u64,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
    epoch: u64,
) -> bool {
    if let Some(ap) = audio.as_ref() {
        if ap.clock_us().is_some() {
            *wall_anchor = None;
            let wait_start = Instant::now();
            loop {
                if state.shutdown() {
                    return false;
                }
                if state.epoch() != epoch {
                    return true; // seeked away while waiting — drop this frame
                }
                while state.paused() {
                    if state.shutdown() {
                        return false;
                    }
                    if state.epoch() != epoch {
                        return true;
                    }
                    std::thread::sleep(cfg.pace_slice);
                }
                // Clock can go None mid-wait (seek in progress) — fall through
                // to the epoch check next iteration rather than treating the
                // frame as infinitely early.
                let now = match ap.clock_us() {
                    Some(c) => c as i64,
                    None => {
                        std::thread::sleep(cfg.pace_slice);
                        continue;
                    }
                };
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
            if state.epoch() != epoch {
                return true;
            }
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

#[cfg(test)]
mod tests {
    use super::pack_rows;

    #[test]
    fn packed_plane_is_copied_verbatim() {
        let plane: Vec<u8> = (0..12).collect();
        assert_eq!(pack_rows(&plane, 4, 4, 3), Some((0..12).collect()));
    }

    #[test]
    fn padded_rows_have_padding_stripped() {
        // stride 6, row_bytes 4: two trailing pad bytes per row.
        let plane: Vec<u8> = vec![
            1, 2, 3, 4, 0, 0, //
            5, 6, 7, 8, 0, 0, //
        ];
        assert_eq!(
            pack_rows(&plane, 6, 4, 2),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 8])
        );
    }

    #[test]
    fn trailing_rows_beyond_height_are_ignored() {
        let plane: Vec<u8> = vec![1, 2, 9, 3, 4, 9, 5, 6, 9];
        assert_eq!(pack_rows(&plane, 3, 2, 2), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn short_plane_is_rejected() {
        let plane = vec![0u8; 11];
        assert_eq!(pack_rows(&plane, 4, 4, 3), None);
    }

    #[test]
    fn stride_below_row_bytes_is_rejected() {
        let plane = vec![0u8; 32];
        assert_eq!(pack_rows(&plane, 2, 4, 2), None);
    }

    /// Row stride ffmpeg produces for `width` RGBA pixels: `av_frame_get_buffer`
    /// aligns rows to 32 bytes.
    fn aligned_stride(width: usize) -> usize {
        (width * 4).div_ceil(32) * 32
    }

    #[test]
    fn rgba_854_padding_is_stripped_at_real_alignment() {
        // 854 * 4 = 3416, which rounds up to 3424 — 8 pad bytes per row. This
        // is the width that sheared before the fix.
        let (w, h) = (854usize, 4usize);
        let stride = aligned_stride(w);
        assert_ne!(stride, w * 4, "854 must actually be padded");

        // Fill each row with its own index, leave the padding as 0xFF so any
        // leakage is visible.
        let mut plane = vec![0xFFu8; stride * h];
        for (i, row) in plane.chunks_exact_mut(stride).enumerate() {
            row[..w * 4].fill(i as u8);
        }

        let packed = pack_rows(&plane, stride, w * 4, h).expect("plane is long enough");
        assert_eq!(packed.len(), w * 4 * h);
        for (i, row) in packed.chunks_exact(w * 4).enumerate() {
            assert!(
                row.iter().all(|&b| b == i as u8),
                "row {i} picked up padding or a neighbouring row"
            );
        }
    }

    #[test]
    fn rgba_1920_takes_the_unpadded_fast_path() {
        let (w, h) = (1920usize, 2usize);
        let stride = aligned_stride(w);
        assert_eq!(stride, w * 4, "1920 was already aligned");

        let plane: Vec<u8> = (0..stride * h).map(|i| i as u8).collect();
        let packed = pack_rows(&plane, stride, w * 4, h).expect("plane is long enough");
        assert_eq!(packed, plane, "aligned planes must be copied verbatim");
    }
}
