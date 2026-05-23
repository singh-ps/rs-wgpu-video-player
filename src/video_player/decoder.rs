use crate::video_player::{
    audio_player::AudioPlayer,
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
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub fn loop_decoder(
    input: String,
    params: PlaybackParams,
    buffer: FrameBuffer,
    audio: Option<AudioPlayer>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut ictx = format::input(&input)?;

    // ── Video stream ──────────────────────────────────────────────────────────
    let vstream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| "No video stream found")?;
    let vindex = vstream.index();

    let ctx = Context::from_parameters(vstream.parameters())?;
    let mut vdec = ctx.decoder().video()?;

    let out_w = vdec.width();
    let out_h = vdec.height();

    let out_pix = match params.pixel_format {
        PixelFormat::RGB24 => Pixel::RGB24,
        PixelFormat::RGBA => Pixel::RGBA,
    };

    let mut scaler = Scaler::get(
        vdec.format(),
        vdec.width(),
        vdec.height(),
        out_pix,
        out_w,
        out_h,
        Flags::BILINEAR,
    )?;

    let mut vyuv = Video::empty();
    let mut vout = Video::empty();

    // PTS / pacing
    let tb = vstream.time_base();
    let fr = vstream.avg_frame_rate();
    let frame_dt = if !params.is_live && fr.1 > 0 {
        Duration::from_secs_f64(fr.1 as f64 / fr.0 as f64)
    } else {
        Duration::ZERO
    };
    let mut last_tick = Instant::now();

    // ── Audio stream → dedicated decode thread ────────────────────────────────
    //
    // The demux loop sleeps per video frame (for pacing). If audio decoding ran
    // inline it would be starved during those sleeps, causing the cpal callback
    // to drain silence → pulsating artefacts. Instead we forward raw packets
    // over an mpsc channel so the audio decode thread runs unblocked.
    let audio_packet_tx: Option<mpsc::SyncSender<ffmpeg::Packet>>;
    let mut aindex: usize = usize::MAX;

    if let Some(ap) = audio {
        if let Some(astream) = ictx.streams().best(Type::Audio) {
            aindex = astream.index();
            let actx = Context::from_parameters(astream.parameters())?;
            let adec = actx.decoder().audio()?;

            let resampler = resampling::Context::get(
                adec.format(),
                adec.channel_layout(),
                adec.rate(),
                Sample::F32(SampleType::Packed),
                ChannelLayout::STEREO,
                48000,
            )?;

            // Bounded channel: at most 256 undecoded audio packets queued.
            // This gives backpressure without running unbounded.
            let (tx, rx) = mpsc::sync_channel::<ffmpeg::Packet>(256);
            audio_packet_tx = Some(tx);

            let shutdown_a = shutdown.clone();
            std::thread::spawn(move || {
                audio_decode_thread(rx, adec, resampler, ap, shutdown_a);
            });
        } else {
            eprintln!("[decoder] No audio stream in source — audio disabled.");
            audio_packet_tx = None;
        }
    } else {
        audio_packet_tx = None;
    }

    // ── Main demux loop ────────────────────────────────────────────────────────
    let mut pkt_ctr = 0usize;

    for (stream, packet) in ictx.packets() {
        // Pause check
        while paused.load(Ordering::Relaxed) {
            if shutdown.load(Ordering::Relaxed) {
                buffer.finish();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
            last_tick = Instant::now();
        }

        if shutdown.load(Ordering::Relaxed) {
            buffer.finish();
            return Ok(());
        }

        pkt_ctr += 1;
        if pkt_ctr % 5 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }

        let sidx = stream.index();

        // ── Video path ─────────────────────────────────────────────────────────
        if sidx == vindex {
            if let Err(e) = vdec.send_packet(&packet) {
                eprintln!("[decoder] video send_packet error: {e}");
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }

            while let Ok(()) = vdec.receive_frame(&mut vyuv) {
                while paused.load(Ordering::Relaxed) {
                    if shutdown.load(Ordering::Relaxed) {
                        buffer.finish();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    last_tick = Instant::now();
                }
                if shutdown.load(Ordering::Relaxed) {
                    buffer.finish();
                    return Ok(());
                }

                if let Err(e) = scaler.run(&vyuv, &mut vout) {
                    eprintln!("[decoder] scaling error: {e}");
                    continue;
                }

                let plane = vout.data(0);
                let pixels: Arc<[u8]> = Vec::from(plane).into();
                let ts_us = pts_to_us(
                    vyuv.timestamp().unwrap_or(0),
                    tb.0 as u32,
                    tb.1 as u32,
                )
                .unwrap_or(0);

                buffer.push(Arc::new(Frame {
                    data: pixels,
                    width: out_w as u32,
                    height: out_h as u32,
                    ts_us: ts_us as u64,
                }));

                if frame_dt.as_millis() > 0 {
                    let elapsed = last_tick.elapsed();
                    let sleep = if elapsed < frame_dt {
                        frame_dt - elapsed
                    } else {
                        Duration::from_millis(1)
                    };
                    std::thread::sleep(sleep);
                    last_tick = Instant::now();
                }
            }

        // ── Audio path: forward packet to audio thread ─────────────────────────
        } else if sidx == aindex {
            if let Some(ref tx) = audio_packet_tx {
                // try_send: if the channel is full, drop the packet rather than
                // blocking the video path. A 256-packet buffer is ~5 s at 48kHz.
                let _ = tx.try_send(packet);
            }
        }
    }

    // ── Flush video decoder ────────────────────────────────────────────────────
    let _ = vdec.send_eof();
    while vdec.receive_frame(&mut vyuv).is_ok() {
        while paused.load(Ordering::Relaxed) {
            if shutdown.load(Ordering::Relaxed) {
                buffer.finish();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
            last_tick = Instant::now();
        }
        if shutdown.load(Ordering::Relaxed) {
            buffer.finish();
            return Ok(());
        }
        if scaler.run(&vyuv, &mut vout).is_ok() {
            let plane = vout.data(0);
            let pixels: Arc<[u8]> = Vec::from(plane).into();
            let ts_us =
                pts_to_us(vyuv.timestamp().unwrap_or(0), tb.0 as u32, tb.1 as u32).unwrap_or(0);
            buffer.push(Arc::new(Frame {
                data: pixels,
                width: out_w as u32,
                height: out_h as u32,
                ts_us: ts_us as u64,
            }));
            if frame_dt.as_millis() > 0 {
                let elapsed = last_tick.elapsed();
                let sleep = if elapsed < frame_dt {
                    frame_dt - elapsed
                } else {
                    Duration::from_millis(1)
                };
                std::thread::sleep(sleep);
                last_tick = Instant::now();
            }
        }
    }

    // Dropping audio_packet_tx here closes the channel → audio thread exits cleanly.
    buffer.finish();
    Ok(())
}

/// Dedicated audio decode + resample + push thread.
/// Runs independently of the video pacing sleeps.
fn audio_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    mut dec: ffmpeg::codec::decoder::Audio,
    mut resampler: resampling::Context,
    player: AudioPlayer,
    shutdown: Arc<AtomicBool>,
) {
    let mut frame = AudioFrame::empty();

    for packet in rx {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        if let Err(e) = dec.send_packet(&packet) {
            eprintln!("[audio_thread] send_packet error: {e}");
            continue;
        }

        while dec.receive_frame(&mut frame).is_ok() {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let mut resampled = AudioFrame::empty();
            if let Err(e) = resampler.run(&frame, &mut resampled) {
                eprintln!("[audio_thread] resample error: {e}");
                continue;
            }

            let raw = resampled.data(0);
            let n_samples = resampled.samples() * 2; // stereo × 2 channels
            if raw.len() >= n_samples * 4 {
                let samples: &[f32] = unsafe {
                    std::slice::from_raw_parts(raw.as_ptr() as *const f32, n_samples)
                };
                player.push(samples);
            }
        }
    }

    // Flush
    let _ = dec.send_eof();
    while dec.receive_frame(&mut frame).is_ok() {
        let mut resampled = AudioFrame::empty();
        if resampler.run(&frame, &mut resampled).is_ok() {
            let raw = resampled.data(0);
            let n_samples = resampled.samples() * 2;
            if raw.len() >= n_samples * 4 {
                let samples: &[f32] = unsafe {
                    std::slice::from_raw_parts(raw.as_ptr() as *const f32, n_samples)
                };
                player.push(samples);
            }
        }
    }
}

#[inline]
fn pts_to_us(pts: i64, tb_num: u32, tb_den: u32) -> Option<i64> {
    if tb_den == 0 {
        return None;
    }
    let us = (pts as i128) * (tb_num as i128) * 1_000_000i128 / (tb_den as i128);
    Some(us as i64)
}
