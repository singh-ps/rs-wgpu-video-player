use crate::video_player::{
    audio_player::{AudioPlayer, CHANNELS, SAMPLE_RATE},
    frame_buffer::{Frame, FrameBuffer},
    PixelFormat, PlaybackParams,
};
use ffmpeg::{
    codec::context::Context,
    format,
    media::Type,
    software::{
        resampling,
        scaling::{Context as Scaler, Flags},
    },
    util::{
        channel_layout::ChannelLayout,
        format::{sample::Type as SampleType, Pixel, Sample},
        frame::{Audio as AudioFrame, Video},
    },
};
use ffmpeg_next as ffmpeg;
use std::{
    error::Error,
    ffi::CStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

use ffmpeg::ffi::{
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_hwdevice_get_type_name,
    av_hwframe_transfer_data, avcodec_get_hw_config, AVBufferRef, AVCodec, AVCodecContext,
    AVPixelFormat, AVPixelFormat::AV_PIX_FMT_NONE, AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
};

/// Bounded packet queues. Sized generously so demux only ever blocks when the
/// audio decoder genuinely needs backpressure (see `wait_for_audio_room`).
const VIDEO_PKT_QUEUE: usize = 1024;
const AUDIO_PKT_QUEUE: usize = 1024;

/// Demux throttle threshold: when the audio sample ring holds more than this
/// many interleaved samples (~3 s @ 48 kHz stereo), pause demuxing of audio
/// packets. Bounds the audio pre-buffer while leaving video flow alone.
const DEMUX_AHEAD_SAMPLES: usize = (SAMPLE_RATE as usize) * (CHANNELS as usize) * 3;

/// Drop a video frame if it would be displayed more than this far past its
/// PTS — i.e. the decoder is falling behind real-time.
const LATE_DROP_US: i64 = 100_000;

/// Max single sleep slice while pacing video — keeps shutdown / pause checks
/// responsive even when waiting many frame periods.
const PACE_SLICE: Duration = Duration::from_millis(20);

pub fn loop_decoder(
    input: String,
    params: PlaybackParams,
    buffer: FrameBuffer,
    audio: Option<AudioPlayer>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut ictx = format::input(&input)?;

    // ── Stream discovery ─────────────────────────────────────────────────────
    let (vindex, vparams, vtb) = {
        let vstream = ictx
            .streams()
            .best(Type::Video)
            .ok_or("No video stream found")?;
        (vstream.index(), vstream.parameters(), vstream.time_base())
    };

    let (aindex, aparams) = match (audio.as_ref(), ictx.streams().best(Type::Audio)) {
        (Some(_), Some(s)) => (s.index(), Some(s.parameters())),
        (Some(_), None) => {
            eprintln!("[decoder] No audio stream in source — audio disabled.");
            (usize::MAX, None)
        }
        _ => (usize::MAX, None),
    };

    // ── Channels: demux → decode threads ─────────────────────────────────────
    let (vtx, vrx) = mpsc::sync_channel::<ffmpeg::Packet>(VIDEO_PKT_QUEUE);
    let (atx_opt, arx_opt) = if aparams.is_some() {
        let (a, b) = mpsc::sync_channel::<ffmpeg::Packet>(AUDIO_PKT_QUEUE);
        (Some(a), Some(b))
    } else {
        (None, None)
    };

    // ── Video decode thread ─────────────────────────────────────────────────
    let v_shutdown = shutdown.clone();
    let v_paused = paused.clone();
    let v_buffer = buffer.clone();
    let v_pix = params.pixel_format;
    let v_audio = audio.clone();

    let video_handle = std::thread::spawn(move || {
        if let Err(e) = video_decode_thread(
            vrx, vparams, vtb, v_pix, v_buffer, v_audio, v_shutdown, v_paused,
        ) {
            eprintln!("[decoder] video thread error: {e}");
        }
    });

    // ── Audio decode thread ─────────────────────────────────────────────────
    let demux_audio = audio.clone();
    let audio_handle = match (arx_opt, aparams, audio) {
        (Some(arx), Some(ap_params), Some(ap)) => {
            let a_shutdown = shutdown.clone();
            Some(std::thread::spawn(move || {
                if let Err(e) = audio_decode_thread(arx, ap_params, ap, a_shutdown) {
                    eprintln!("[decoder] audio thread error: {e}");
                }
            }))
        }
        _ => None,
    };

    // ── Demux loop ─────────────────────────────────────────────────────────
    'demux: for (stream, packet) in ictx.packets() {
        // Pause
        while paused.load(Ordering::Relaxed) {
            if shutdown.load(Ordering::Relaxed) {
                break 'demux;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let sidx = stream.index();
        if sidx == vindex {
            // Video flows freely. Vrx capacity bounds memory; demux blocks
            // briefly on a full vrx, which is the desired backpressure.
            if vtx.send(packet).is_err() {
                break; // video decoder gone
            }
        } else if sidx == aindex {
            if let Some(tx) = atx_opt.as_ref() {
                // Throttle only the audio dispatch — when the sample ring is
                // already well-stocked (~3 s), wait. This indirectly paces
                // demuxing of further video packets via packet interleave
                // order in the container.
                wait_for_audio_room(&demux_audio, &shutdown, &paused);
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                if tx.send(packet).is_err() {
                    // Audio thread gone — keep going on video.
                }
            }
        }
    }

    drop(vtx);
    drop(atx_opt);

    if let Err(e) = video_handle.join() {
        eprintln!("[decoder] video thread panicked: {e:?}");
    }
    if let Some(h) = audio_handle {
        if let Err(e) = h.join() {
            eprintln!("[decoder] audio thread panicked: {e:?}");
        }
    }

    buffer.finish();
    Ok(())
}

/// Sleep the demux loop while the audio sample ring is already well-stocked.
/// No-op when audio is not configured / not yet started.
fn wait_for_audio_room(
    audio: &Option<AudioPlayer>,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
) {
    let ap = match audio.as_ref() {
        Some(a) => a,
        None => return,
    };
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        if paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if ap.queued_samples() <= DEMUX_AHEAD_SAMPLES {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[allow(clippy::too_many_arguments)]
fn video_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    vparams: ffmpeg::codec::Parameters,
    vtb: ffmpeg::Rational,
    pix: PixelFormat,
    buffer: FrameBuffer,
    audio: Option<AudioPlayer>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Build the decoder context, then attempt to attach a HW device BEFORE
    // calling avcodec_open2 (which happens inside `.open_as(codec)`).
    let ctx = Context::from_parameters(vparams)?;
    let codec_id = ctx.id();
    let codec = ffmpeg::decoder::find(codec_id).ok_or("decoder not found for codec")?;

    let mut dec_wrap = ctx.decoder();
    let hw_pix_fmt: Option<i32> = unsafe {
        try_enable_hw_decoder(dec_wrap.as_mut_ptr(), codec.as_ptr() as *const AVCodec)
    };

    let mut vdec = dec_wrap.open_as(codec).and_then(|o| o.video())?;

    let out_w = vdec.width();
    let out_h = vdec.height();
    let out_pix = match pix {
        PixelFormat::RGB24 => Pixel::RGB24,
        PixelFormat::RGBA => Pixel::RGBA,
    };

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
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        while paused.load(Ordering::Relaxed) {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if let Err(e) = vdec.send_packet(&packet) {
            eprintln!("[video] send_packet error: {e}");
            continue;
        }
        drain_video(&mut vdec, &mut scaler, &mut vyuv, &mut vout,
                    &mut first_pts_us, &mut wall_anchor, vtb, &buffer,
                    out_pix, out_w, out_h, hw_pix_fmt, &audio,
                    &shutdown, &paused);
    }

    if let Err(e) = vdec.send_eof() {
        eprintln!("[video] send_eof error: {e}");
    }
    drain_video(&mut vdec, &mut scaler, &mut vyuv, &mut vout,
                &mut first_pts_us, &mut wall_anchor, vtb, &buffer,
                out_pix, out_w, out_h, hw_pix_fmt, &audio,
                &shutdown, &paused);

    Ok(())
}

/// Iterate the codec's HW configurations, create the first device we can,
/// wire it onto the AVCodecContext, and install a `get_format` callback that
/// picks the matching HW pixel format. Returns the HW pix_fmt on success so
/// the caller can detect HW frames coming back out of the decoder.
unsafe fn try_enable_hw_decoder(
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
                eprintln!("[video] hw decode enabled ({name})");
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
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
) {
    while vdec.receive_frame(vyuv).is_ok() {
        if shutdown.load(Ordering::Relaxed) {
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
                    eprintln!("[video] hw transfer failed (code {r})");
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
            match Scaler::get(src_fmt, src_w, src_h, out_pix, out_w, out_h, Flags::BILINEAR) {
                Ok(s) => *scaler = Some((s, src_fmt, src_w, src_h)),
                Err(e) => {
                    eprintln!("[video] scaler init error: {e}");
                    continue;
                }
            }
        }
        let s = match scaler.as_mut() {
            Some((s, _, _, _)) => s,
            None => continue,
        };
        if let Err(e) = s.run(src, vout) {
            eprintln!("[video] scaling error: {e}");
            continue;
        }

        let abs_pts = pts_to_us(vyuv.timestamp().unwrap_or(0), vtb.0, vtb.1)
            .unwrap_or(0)
            .max(0) as u64;
        let first = *first_pts_us.get_or_insert(abs_pts);
        let ts_us = abs_pts.saturating_sub(first);

        // Pace presentation against audio clock (or wall-clock fallback).
        // Drop frame if we're far past its display time.
        if pace_video(audio, wall_anchor, ts_us, shutdown, paused) {
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
    }
}

/// Block until the frame's presentation time arrives, paced by the audio
/// master clock when available, falling back to wall-clock relative to the
/// first frame. Returns `true` if the frame is so late it should be dropped.
fn pace_video(
    audio: &Option<AudioPlayer>,
    wall_anchor: &mut Option<(Instant, u64)>,
    ts_us: u64,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
) -> bool {
    if let Some(ap) = audio.as_ref() {
        if let Some(_) = ap.clock_us() {
            *wall_anchor = None;
            let wait_start = Instant::now();
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return false;
                }
                while paused.load(Ordering::Relaxed) {
                    if shutdown.load(Ordering::Relaxed) {
                        return false;
                    }
                    std::thread::sleep(PACE_SLICE);
                }
                let now = ap.clock_us().unwrap_or(0) as i64;
                let diff = ts_us as i64 - now;
                if diff <= 0 {
                    return -diff > LATE_DROP_US;
                }
                // Safety bail-out in case the audio clock stalls.
                if wait_start.elapsed() > Duration::from_secs(2) {
                    return false;
                }
                let sleep_us = (diff as u64).min(PACE_SLICE.as_micros() as u64);
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
        let sleep = (target - elapsed).min(PACE_SLICE);
        std::thread::sleep(sleep);
        // Loop until target reached or shutdown — keep responsive.
        while !shutdown.load(Ordering::Relaxed) {
            let elapsed = Instant::now().duration_since(start_inst);
            if elapsed >= target {
                break;
            }
            std::thread::sleep((target - elapsed).min(PACE_SLICE));
        }
    } else if (elapsed - target).as_micros() as i64 > LATE_DROP_US {
        return true;
    }
    false
}

fn audio_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    aparams: ffmpeg::codec::Parameters,
    player: AudioPlayer,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let ctx = Context::from_parameters(aparams)?;
    let mut dec = ctx.decoder().audio()?;

    let in_layout = if dec.channel_layout().bits() == 0 {
        ChannelLayout::default(dec.channels() as i32)
    } else {
        dec.channel_layout()
    };

    let mut resampler = resampling::Context::get(
        dec.format(),
        in_layout,
        dec.rate(),
        Sample::F32(SampleType::Packed),
        ChannelLayout::STEREO,
        SAMPLE_RATE,
    )?;

    let mut frame = AudioFrame::empty();

    for packet in rx {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = dec.send_packet(&packet) {
            eprintln!("[audio] send_packet error: {e}");
            continue;
        }
        drain_audio(&mut dec, &mut resampler, &mut frame, &player, &shutdown);
    }

    if let Err(e) = dec.send_eof() {
        eprintln!("[audio] send_eof error: {e}");
    }
    drain_audio(&mut dec, &mut resampler, &mut frame, &player, &shutdown);
    flush_resampler(&mut resampler, &player);
    Ok(())
}

fn drain_audio(
    dec: &mut ffmpeg::codec::decoder::Audio,
    resampler: &mut resampling::Context,
    frame: &mut AudioFrame,
    player: &AudioPlayer,
    shutdown: &Arc<AtomicBool>,
) {
    while dec.receive_frame(frame).is_ok() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let mut resampled = AudioFrame::empty();
        if let Err(e) = resampler.run(frame, &mut resampled) {
            eprintln!("[audio] resample error: {e}");
            continue;
        }
        push_resampled(&resampled, player);
    }
}

fn flush_resampler(resampler: &mut resampling::Context, player: &AudioPlayer) {
    loop {
        let mut more = AudioFrame::empty();
        match resampler.flush(&mut more) {
            Ok(_) => {
                if more.samples() == 0 {
                    break;
                }
                push_resampled(&more, player);
            }
            Err(e) => {
                eprintln!("[audio] resampler flush error: {e}");
                break;
            }
        }
    }
}

fn push_resampled(resampled: &AudioFrame, player: &AudioPlayer) {
    let n_per_ch = resampled.samples();
    if n_per_ch == 0 {
        return;
    }
    let n_samples = n_per_ch.checked_mul(CHANNELS as usize).unwrap_or(0);
    if n_samples == 0 {
        return;
    }
    let raw = resampled.data(0);
    let needed_bytes = n_samples
        .checked_mul(std::mem::size_of::<f32>())
        .unwrap_or(0);
    if raw.len() < needed_bytes {
        return;
    }
    let samples: &[f32] =
        unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const f32, n_samples) };
    player.push(samples);
}

#[inline]
fn pts_to_us(pts: i64, tb_num: i32, tb_den: i32) -> Option<i64> {
    if tb_den == 0 {
        return None;
    }
    let us = (pts as i128) * (tb_num as i128) * 1_000_000i128 / (tb_den as i128);
    Some(us as i64)
}
