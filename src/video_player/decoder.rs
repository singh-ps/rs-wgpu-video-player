use crate::video_player::{
    frame_buffer::{Frame, FrameBuffer},
    PixelFormat, PlaybackParams,
};
use ffmpeg::{
    codec::context::Context,
    format,
    media::Type,
    software::scaling::{Context as Scaler, Flags},
    util::{format::Pixel, frame::Video},
};
use ffmpeg_next as ffmpeg;
use std::{
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub fn loop_decoder(
    input: String,
    params: PlaybackParams,
    buffer: FrameBuffer,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    ffmpeg::init()?;

    let mut ictx = format::input(&input)?;
    let vstream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| "No video stream found")?;
    let vindex = vstream.index();

    let ctx = Context::from_parameters(vstream.parameters())?;
    let mut dec = ctx.decoder().video()?;

    let out_w = dec.width();
    let out_h = dec.height();

    let out_pix = match params.pixel_format {
        PixelFormat::RGB24 => Pixel::RGB24,
        PixelFormat::RGB8 => Pixel::RGB8,
    };

    // Build scaler.
    let mut scaler = Scaler::get(
        dec.format(),
        dec.width(),
        dec.height(),
        out_pix,
        out_w,
        out_h,
        Flags::BILINEAR,
    )?;

    let mut yuv = Video::empty();
    let mut out = Video::empty();

    // PTS conversion + pacing info
    let tb = vstream.time_base();
    let fr = vstream.avg_frame_rate();
    let frame_dt = if !params.is_live && fr.1 > 0 {
        Duration::from_secs_f64(fr.1 as f64 / fr.0 as f64)
    } else {
        Duration::from_millis(0)
    };
    let mut last_tick = Instant::now();

    // Main demux/decode
    let mut pkt_ctr = 0usize;
    for (stream, packet) in ictx.packets() {
        if shutdown.load(Ordering::Relaxed) {
            buffer.finish();
            return Ok(());
        }

        // Tiny cooperative yield
        pkt_ctr += 1;
        if pkt_ctr % 5 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }

        if stream.index() != vindex {
            continue;
        }

        if let Err(e) = dec.send_packet(&packet) {
            eprintln!("decoder send_packet error: {e}");
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        while let Ok(()) = dec.send_packet(&packet) {
            if shutdown.load(Ordering::Relaxed) {
                buffer.finish();
                return Ok(());
            }

            if let Err(e) = scaler.run(&yuv, &mut out) {
                eprintln!("Scaling error: {e}");
                continue;
            }

            // Copy scaler output plane (producer-side allocation per frame).
            let plane = out.data(0);
            let pixels: Arc<[u8]> = Vec::from(plane).into();

            let ts_us =
                pts_to_us(yuv.timestamp().unwrap_or(0), tb.0 as u32, tb.1 as u32).unwrap_or(0);

            let frame = Arc::new(Frame {
                data: pixels,
                width: out_w as u32,
                height: out_h as u32,
                ts_us: ts_us as u64,
            });

            buffer.push(frame);

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

    // Flush (files). Live may not reach here.
    let _ = dec.send_eof();
    while dec.receive_frame(&mut yuv).is_ok() {
        if shutdown.load(Ordering::Relaxed) {
            buffer.finish();
            return Ok(());
        }
        if scaler.run(&yuv, &mut out).is_ok() {
            let plane = out.data(0);
            let pixels: Arc<[u8]> = Vec::from(plane).into();
            let ts_us =
                pts_to_us(yuv.timestamp().unwrap_or(0), tb.0 as u32, tb.1 as u32).unwrap_or(0);
            let frame = Arc::new(Frame {
                data: pixels,
                width: out_w as u32,
                height: out_h as u32,
                ts_us: ts_us as u64,
            });
            buffer.push(frame);
        }
    }

    buffer.finish();
    Ok(())
}

#[inline]
fn pts_to_us(pts: i64, tb_num: u32, tb_den: u32) -> Option<i64> {
    if tb_den == 0 {
        return None;
    }
    let us = (pts as i128) * (tb_num as i128) * 1_000_000i128 / (tb_den as i128);
    Some(us as i64)
}
