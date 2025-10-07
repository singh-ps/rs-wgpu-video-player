use ffmpeg::{codec::context::Context, ffi::AV_TIME_BASE, format, media::Type};
use ffmpeg_next as ffmpeg;
use std::error::Error;

#[derive(Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_us: Option<i64>,
    pub avg_fps: Option<f64>,
}

pub fn get_video_info(input: &str) -> Result<VideoInfo, Box<dyn Error>> {
    ffmpeg::init()?;
    let ictx = format::input(input)?;
    let vstream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| "No video stream")?;

    let ctx = Context::from_parameters(vstream.parameters())?;
    let dec = ctx.decoder().video()?;

    let width = dec.width();
    let height = dec.height();

    let fps = vstream.avg_frame_rate();
    let avg_fps = if fps.1 != 0 {
        Some(fps.0 as f64 / fps.1 as f64)
    } else {
        None
    };

    let duration_us = if ictx.duration() > 0 {
        Some((ictx.duration() as i128 * 1_000_000i128 / AV_TIME_BASE as i128) as i64)
    } else {
        None
    };

    Ok(VideoInfo {
        width,
        height,
        duration_us,
        avg_fps,
    })
}
