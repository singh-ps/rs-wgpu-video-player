mod audio;
mod demux;
mod hw;
mod video;

use crate::video_player::{
    audio_player::AudioPlayer,
    error::{PlayerError, Result},
    event::PlaybackEvent,
    state::PlaybackState,
};
use ffmpeg::{format, media::Type};
use ffmpeg_next as ffmpeg;
use std::sync::{mpsc, Arc};
use tokio::sync::mpsc::UnboundedSender;

/// Bounded packet queues. Sized generously so demux only ever blocks when the
/// audio decoder genuinely needs backpressure (see demux::wait_for_audio_room).
const VIDEO_PKT_QUEUE: usize = 1024;
const AUDIO_PKT_QUEUE: usize = 1024;

pub fn loop_decoder(
    input: String,
    events: UnboundedSender<PlaybackEvent>,
    audio: Option<AudioPlayer>,
    state: Arc<PlaybackState>,
) -> Result<()> {
    let mut ictx = format::input(&input)?;

    // ── Stream discovery ─────────────────────────────────────────────────────
    let (vindex, vparams, vtb) = {
        let vstream = ictx
            .streams()
            .best(Type::Video)
            .ok_or(PlayerError::NoVideoStream)?;
        (vstream.index(), vstream.parameters(), vstream.time_base())
    };

    let (aindex, aparams) = match (audio.as_ref(), ictx.streams().best(Type::Audio)) {
        (Some(_), Some(s)) => (s.index(), Some(s.parameters())),
        (Some(_), None) => {
            tracing::info!(target: "decoder", "no audio stream in source — audio disabled");
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
    let v_state = state.clone();
    let v_events = events.clone();
    let v_audio = audio.clone();

    let video_handle = std::thread::spawn(move || {
        if let Err(e) = video::video_decode_thread(vrx, vparams, vtb, v_events, v_audio, v_state) {
            tracing::warn!(target: "video", "thread error: {e}");
        }
    });

    // ── Audio decode thread ─────────────────────────────────────────────────
    let audio_handle = match (arx_opt, aparams, audio.clone()) {
        (Some(arx), Some(ap_params), Some(ap)) => {
            let a_state = state.clone();
            Some(std::thread::spawn(move || {
                if let Err(e) = audio::audio_decode_thread(arx, ap_params, ap, a_state) {
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
