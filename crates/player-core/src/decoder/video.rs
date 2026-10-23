use crate::{
    audio_player::AudioPlayer,
    config::{PixelFormat, PlaybackConfig},
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
    let out_pix = match cfg.output_format {
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Bgra => Pixel::BGRA,
    };

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
    // Pacing state that outlives a single frame; reset on seek.
    let mut pace = PaceState::default();
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
            pace = PaceState::default();
            epoch = pkt_epoch;
        }
        // Absorbed here as well as inside `pace_video`, so a pause taken
        // between frames still shifts the wall-clock anchor.
        if absorb_pause(&state, &cfg, epoch, &mut pace.wall_anchor) {
            if state.shutdown() {
                return Ok(());
            }
            continue; // seeked while paused — this packet is stale now
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
            &mut pace,
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
        &mut pace,
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
    pace: &mut PaceState,
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

        let abs_pts = pts_to_us(vyuv.timestamp().unwrap_or(0), vtb.0, vtb.1)
            .unwrap_or(0)
            .max(0) as u64;
        let first = *first_pts_us.get_or_insert(abs_pts);
        let ts_us = abs_pts.saturating_sub(first);

        // Drop already-late frames before GPU readback / RGBA conversion.
        // Still do the actual presentation wait AFTER conversion, so the
        // conversion cost does not add latency to every healthy frame.
        let clock = audio.as_ref().and_then(|ap| ap.clock_us());
        if state.epoch() != epoch
            || clock.is_some_and(|now| now.saturating_sub(ts_us) > cfg.late_drop_us.max(0) as u64)
        {
            continue;
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

        // Conversion may have overlapped a seek, shutdown or clock advance.
        if pace_video(audio, pace, ts_us, state, cfg, epoch) {
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

/// Pacing state owned by the video decode thread. Both fields have to outlive
/// a single frame, and both are discarded on seek.
#[derive(Default)]
struct PaceState {
    /// Anchor for wall-clock pacing, used while no audio clock is live.
    wall_anchor: Option<(Instant, u64)>,
    /// Consumed-sample position the audio clock gave up waiting on. While this
    /// is set, frames are paced on wall-clock instead. Sample position, not
    /// `clock_us`: the clock also moves when the driver revises its reported
    /// output latency, which would clear the latch without a single new
    /// sample having played.
    audio_stalled_at: Option<u64>,
}

/// Block while playback is paused, then shift `anchor` forward by the paused
/// duration: wall-clock pacing measures elapsed real time, so counting a pause
/// as playback would make every later frame look late enough to drop. Returns
/// `true` if the caller should give up on the frame (shutdown, or a seek moved
/// the epoch on while we waited).
fn absorb_pause(
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
    epoch: u64,
    anchor: &mut Option<(Instant, u64)>,
) -> bool {
    if !state.paused() {
        return false;
    }
    let paused_at = Instant::now();
    let mut abort = false;
    while state.paused() {
        if state.shutdown() || state.epoch() != epoch {
            abort = true;
            break;
        }
        std::thread::sleep(cfg.pace_slice);
    }
    if let Some((start, _)) = anchor.as_mut() {
        *start += paused_at.elapsed();
    }
    abort
}

/// Block until the frame's presentation time arrives, paced by the audio
/// master clock when available, falling back to wall-clock relative to the
/// first frame. Returns `true` if the frame should be dropped — because it's
/// far past its display time, or because a seek invalidated its epoch while
/// we were waiting.
fn pace_video(
    audio: &Option<AudioPlayer>,
    pace: &mut PaceState,
    ts_us: u64,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
    epoch: u64,
) -> bool {
    if state.shutdown() || state.epoch() != epoch {
        return true;
    }
    if let Some(ap) = audio.as_ref() {
        // Never wait on a clock already declared stalled. The timeout below
        // is per-call, so re-arming it for every frame would creep playback
        // forward one frame per timeout. Pace on wall-clock until audio is
        // consumed again — the first resumed sample hands pacing straight
        // back to the clock.
        let consumed = ap.consumed_samples();
        let still_stalled = pace.audio_stalled_at == Some(consumed);
        if ap.clock_us().is_some() && !still_stalled {
            pace.audio_stalled_at = None;
            pace.wall_anchor = None;
            let mut progress_at = Instant::now();
            let mut previous_consumed = consumed;
            loop {
                if state.shutdown() {
                    return true;
                }
                if state.epoch() != epoch {
                    return true; // seeked away while waiting — drop this frame
                }
                if state.paused() {
                    if absorb_pause(state, cfg, epoch, &mut pace.wall_anchor) {
                        return true;
                    }
                    // A pause is not a stall: it must not count toward the
                    // timeout, and the clock does not move while paused.
                    progress_at = Instant::now();
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
                // An underrun is network buffering, not a dead device: the
                // clock freezes rather than racing ahead, so holding this
                // frame keeps A/V aligned. The wait must stay bounded though
                // — audio that never resumes (decode error, mid-stream audio
                // gap) would otherwise hold the picture for the rest of the
                // session. Latch the stall and let wall-clock take over.
                //
                // Progress means consumed samples, not `clock_us`: the clock
                // also shifts when the driver revises its reported latency,
                // and treating that as progress would re-arm the timeout
                // forever on a dead output stream.
                let consumed = ap.consumed_samples();
                if previous_consumed != consumed {
                    previous_consumed = consumed;
                    progress_at = Instant::now();
                }
                if progress_at.elapsed() > cfg.audio_stall_timeout() {
                    tracing::warn!(target: "video",
                        "audio stalled for {:?} at sample {consumed}, pacing on \
                         wall-clock: ts_us={ts_us} clock={now} diff_ms={} queued={}",
                        cfg.audio_stall_timeout(),
                        diff / 1000,
                        ap.queued_samples());
                    pace.audio_stalled_at = Some(consumed);
                    return false;
                }
                let sleep_us = (diff as u64).min(cfg.pace_slice.as_micros() as u64);
                std::thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    // Wall-clock fallback: no audio configured, audio not started yet, or an
    // audio clock that stopped moving for longer than the stall timeout.
    if absorb_pause(state, cfg, epoch, &mut pace.wall_anchor) {
        return true;
    }
    let (start_inst, first_pts) = match pace.wall_anchor {
        Some(a) => a,
        None => {
            pace.wall_anchor = Some((Instant::now(), ts_us));
            return false;
        }
    };
    let target = Duration::from_micros(ts_us.saturating_sub(first_pts));
    let elapsed = start_inst.elapsed();
    if target > elapsed {
        // Loop until the target arrives — staying responsive to pause, seek
        // and shutdown rather than sleeping the whole span in one go.
        loop {
            if state.shutdown() || state.epoch() != epoch {
                return true;
            }
            if absorb_pause(state, cfg, epoch, &mut pace.wall_anchor) {
                return true;
            }
            // `absorb_pause` moves the anchor, so re-read it every pass
            // instead of pacing against a pre-pause instant.
            let start_inst = pace.wall_anchor.map_or(start_inst, |(s, _)| s);
            let elapsed = start_inst.elapsed();
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
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// A frozen audio clock with `started` already set: the state the pipeline
    /// lands in when audio stops arriving mid-stream and never resumes.
    fn stalled_clock(
        margin_secs: f32,
    ) -> (PlaybackConfig, Arc<PlaybackState>, Option<AudioPlayer>) {
        // Zero ring headroom, so the stall timeout is the margin alone.
        let cfg = PlaybackConfig {
            sample_ring_cap_secs: 1.0,
            demux_ahead_secs: 1.0,
            audio_stall_margin_secs: margin_secs,
            ..PlaybackConfig::default()
        };
        let state = Arc::new(PlaybackState::new());
        let player = AudioPlayer::new(state.clone(), cfg);
        player.push(&[0.0f32; 960]);
        player.render(&mut [0.0f32; 960]); // Starts the clock; ring empty again.
        assert_eq!(player.clock_us(), Some(10_000));
        (cfg, state, Some(player))
    }

    #[test]
    fn pace_bailout_presents_frame_when_the_audio_clock_dies() {
        let (cfg, state, audio) = stalled_clock(0.05);
        let mut pace = PaceState::default();

        // The frozen clock sits 990 ms behind this frame. Waiting on it
        // forever would hold the picture for the rest of the session, so the
        // pacer gives up and presents the frame instead of dropping it.
        let start = Instant::now();
        let dropped = pace_video(&audio, &mut pace, 1_000_000, &state, &cfg, 0);
        assert!(!dropped, "bailout must present the frame, not drop it");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "pace wait must stay bounded"
        );
    }

    #[test]
    fn audio_stall_is_latched_instead_of_re_armed_per_frame() {
        let (cfg, state, audio) = stalled_clock(0.3);
        let timeout = cfg.audio_stall_timeout();
        let mut pace = PaceState::default();

        // The stall costs the timeout exactly once.
        let start = Instant::now();
        assert!(!pace_video(&audio, &mut pace, 1_000_000, &state, &cfg, 0));
        assert!(start.elapsed() >= timeout);
        assert_eq!(pace.audio_stalled_at, Some(960));

        // The clock is still frozen. Every later frame must pace on
        // wall-clock, not wait out the timeout again — otherwise playback
        // creeps forward one frame per timeout instead of stalling or playing.
        for ts_us in [1_040_000u64, 1_080_000, 1_120_000] {
            let start = Instant::now();
            assert!(!pace_video(&audio, &mut pace, ts_us, &state, &cfg, 0));
            let waited = start.elapsed();
            assert!(
                waited < timeout,
                "frame at {ts_us} waited {waited:?}, stall timeout was re-armed"
            );
        }

        // One resumed sample hands pacing back to the audio clock.
        if let Some(ap) = audio.as_ref() {
            ap.push(&[0.0f32; 960]);
            ap.render(&mut [0.0f32; 960]);
        }
        assert!(!pace_video(&audio, &mut pace, 0, &state, &cfg, 0));
        assert_eq!(pace.audio_stalled_at, None);
    }

    #[test]
    fn latency_jitter_is_not_audio_progress() {
        let (cfg, state, audio) = stalled_clock(0.3);
        let timeout = cfg.audio_stall_timeout();
        let mut pace = PaceState::default();

        // The driver revises its reported latency while we wait. That moves
        // `clock_us` without a single sample being consumed; counting it as
        // progress would keep the timeout re-armed forever.
        let jitter = Arc::new(AtomicBool::new(true));
        let shaker = {
            let (ap, jitter) = (audio.clone().unwrap(), jitter.clone());
            std::thread::spawn(move || {
                let mut us = 0;
                while jitter.load(AtomicOrdering::Relaxed) {
                    us = if us == 0 { 5_000 } else { 0 };
                    ap.set_output_latency_us(us);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };

        let start = Instant::now();
        assert!(!pace_video(&audio, &mut pace, 1_000_000, &state, &cfg, 0));
        let waited = start.elapsed();
        jitter.store(false, AtomicOrdering::Relaxed);
        shaker.join().unwrap();

        assert!(
            waited < timeout * 3,
            "latency jitter kept the stall timeout alive for {waited:?}"
        );
        assert_eq!(pace.audio_stalled_at, Some(960));
    }

    #[test]
    fn wall_clock_fallback_excludes_paused_time() {
        let cfg = PlaybackConfig::default();
        let state = Arc::new(PlaybackState::new());
        let audio = None; // No audio at all: straight to the wall-clock path.
        let mut pace = PaceState::default();

        // First frame anchors the wall clock.
        assert!(!pace_video(&audio, &mut pace, 0, &state, &cfg, 0));

        // Pause for three times `late_drop_us`, unpausing from another thread.
        // The frame must not be presented before the resume, and the paused
        // time must not count against its display time — otherwise everything
        // after a pause looks late enough to drop.
        assert!(state.toggle_pause());
        let resumer = {
            let state = state.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                assert!(!state.toggle_pause());
            })
        };
        let start = Instant::now();
        let dropped = pace_video(&audio, &mut pace, 50_000, &state, &cfg, 0);
        let waited = start.elapsed();
        resumer.join().unwrap();

        assert!(!dropped, "paused time was counted as playback time");
        assert!(
            waited >= Duration::from_millis(300),
            "frame presented while paused after {waited:?}"
        );
    }

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
