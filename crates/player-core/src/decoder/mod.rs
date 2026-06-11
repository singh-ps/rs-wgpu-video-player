mod audio;
mod demux;
mod hw;
mod video;

use crate::{
    audio_player::AudioPlayer,
    config::PlaybackConfig,
    error::{PlayerError, Result},
    event::PlaybackEvent,
    frame_buffer::FrameBuffer,
    state::PlaybackState,
};
use ffmpeg::{ffi::AV_TIME_BASE, format, media::Type};
use ffmpeg_next as ffmpeg;
use std::sync::{mpsc, Arc};
use tokio::sync::mpsc::UnboundedSender;

use demux::TaggedPacket;

/// Commands the UI can send into the running demux loop.
#[derive(Debug)]
pub enum DemuxCommand {
    /// Seek to this position in microseconds on the 0-based UI timeline.
    Seek(u64),
}

#[allow(clippy::too_many_arguments)]
pub fn loop_decoder(
    input: String,
    buffer: FrameBuffer,
    events: UnboundedSender<PlaybackEvent>,
    audio: Option<AudioPlayer>,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
    cmd_rx: mpsc::Receiver<DemuxCommand>,
) -> Result<()> {
    // Bound stream probing. Without this, avformat_find_stream_info on an HLS
    // master playlist downloads sample segments from EVERY bitrate variant in
    // parallel (5 variants × 2 segments here), which can exceed the server's
    // per-client connection limit and stall playback into start-stop bursts.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("probesize", "500000"); // bytes; default 5 MB
    opts.set("analyzeduration", "500000"); // µs; default 5 s
    let mut ictx = format::input_with_dictionary(&input, opts)?;

    // Emit duration if the container knows it. Avoids a second `format::input`
    // call (which on HLS would open a parallel set of segment downloads).
    if ictx.duration() > 0 {
        let dur_us =
            (ictx.duration() as i128 * 1_000_000i128 / AV_TIME_BASE as i128) as u64;
        let _ = events.send(PlaybackEvent::Duration(dur_us));
    }

    // ── Stream discovery ─────────────────────────────────────────────────────
    let (vindex, vparams, vtb) = {
        let vstream = ictx
            .streams()
            .best(Type::Video)
            .ok_or(PlayerError::NoVideoStream)?;
        (vstream.index(), vstream.parameters(), vstream.time_base())
    };

    let (aindex, aparams, atb) = match (audio.as_ref(), ictx.streams().best(Type::Audio)) {
        (Some(_), Some(s)) => (s.index(), Some(s.parameters()), s.time_base()),
        (Some(_), None) => {
            tracing::info!(target: "decoder", "no audio stream in source — audio disabled");
            (usize::MAX, None, ffmpeg::Rational(0, 1))
        }
        _ => (usize::MAX, None, ffmpeg::Rational(0, 1)),
    };

    // ── Channels: demux → decode threads ─────────────────────────────────────
    let (vtx, vrx) = mpsc::sync_channel::<TaggedPacket>(cfg.video_pkt_queue);
    let (atx_opt, arx_opt) = if aparams.is_some() {
        let (a, b) = mpsc::sync_channel::<TaggedPacket>(cfg.audio_pkt_queue);
        (Some(a), Some(b))
    } else {
        (None, None)
    };

    // ── Video decode thread ─────────────────────────────────────────────────
    let v_state = state.clone();
    let v_buffer = buffer.clone();
    let v_audio = audio.clone();

    let video_handle = std::thread::spawn(move || {
        if let Err(e) =
            video::video_decode_thread(vrx, vparams, vtb, v_buffer, v_audio, v_state, cfg)
        {
            tracing::warn!(target: "video", "thread error: {e}");
        }
    });

    // ── Audio decode thread ─────────────────────────────────────────────────
    let audio_handle = match (arx_opt, aparams, audio.clone()) {
        (Some(arx), Some(ap_params), Some(ap)) => {
            let a_state = state.clone();
            Some(std::thread::spawn(move || {
                if let Err(e) = audio::audio_decode_thread(arx, ap_params, atb, ap, a_state, cfg)
                {
                    tracing::warn!(target: "audio", "thread error: {e}");
                }
            }))
        }
        _ => None,
    };

    // ── Demux loop ─────────────────────────────────────────────────────────
    demux::run_demux(
        &mut ictx,
        vindex,
        aindex,
        &vtx,
        atx_opt.as_ref(),
        &audio,
        &state,
        &cfg,
        &cmd_rx,
    );

    drop(vtx);
    drop(atx_opt);

    if let Err(e) = video_handle.join() {
        tracing::warn!(target: "video", "thread panicked: {e:?}");
    }
    if let Some(h) = audio_handle {
        if let Err(e) = h.join() {
            tracing::warn!(target: "audio", "thread panicked: {e:?}");
        }
    }

    let _ = events.send(PlaybackEvent::Ended);
    Ok(())
}

#[inline]
pub(super) fn pts_to_us(pts: i64, tb_num: i32, tb_den: i32) -> Option<i64> {
    if tb_den == 0 {
        return None;
    }
    let us = (pts as i128) * (tb_num as i128) * 1_000_000i128 / (tb_den as i128);
    Some(us as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_zero_denominator_is_none() {
        assert_eq!(pts_to_us(1000, 1, 0), None);
    }

    #[test]
    fn pts_simple_1_1000_timebase() {
        // 1500 ticks @ 1/1000 s/tick = 1.5 s = 1_500_000 µs
        assert_eq!(pts_to_us(1500, 1, 1000), Some(1_500_000));
    }

    #[test]
    fn pts_90khz_timebase_typical_mpegts() {
        // 90_000 ticks @ 1/90000 s/tick = 1 s
        assert_eq!(pts_to_us(90_000, 1, 90_000), Some(1_000_000));
    }

    #[test]
    fn pts_negative_supported() {
        assert_eq!(pts_to_us(-1000, 1, 1000), Some(-1_000_000));
    }
}
