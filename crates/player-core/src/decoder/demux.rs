use crate::{
    audio_player::AudioPlayer, config::PlaybackConfig, state::PlaybackState,
};
use ffmpeg_next as ffmpeg;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

#[allow(clippy::too_many_arguments)]
pub fn run_demux(
    ictx: &mut ffmpeg::format::context::Input,
    vindex: usize,
    aindex: usize,
    vtx: &mpsc::SyncSender<ffmpeg::Packet>,
    atx: Option<&mpsc::SyncSender<ffmpeg::Packet>>,
    audio: &Option<AudioPlayer>,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
) {
    let demux_ahead_samples = (cfg.audio_sample_rate as f32
        * cfg.audio_channels as f32
        * cfg.demux_ahead_secs) as usize;

    'demux: for (stream, packet) in ictx.packets() {
        while state.paused() {
            if state.shutdown() {
                break 'demux;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if state.shutdown() {
            break;
        }

        let sidx = stream.index();
        if sidx == vindex {
            // Video flows freely. vrx capacity bounds memory; demux blocks
            // briefly on a full vrx, which is the desired backpressure.
            if vtx.send(packet).is_err() {
                break; // video decoder gone
            }
        } else if sidx == aindex {
            if let Some(tx) = atx {
                // Throttle only the audio dispatch — when the sample ring is
                // already well-stocked, wait. This indirectly paces demuxing
                // of further video packets via packet interleave order in the
                // container.
                wait_for_audio_room(audio, state, demux_ahead_samples);
                if state.shutdown() {
                    break;
                }
                if tx.send(packet).is_err() {
                    // Audio thread gone — keep going on video.
                }
            }
        }
    }
}

/// Sleep the demux loop while the audio sample ring is already well-stocked.
/// No-op when audio is not configured / not yet started.
fn wait_for_audio_room(
    audio: &Option<AudioPlayer>,
    state: &Arc<PlaybackState>,
    demux_ahead_samples: usize,
) {
    let ap = match audio.as_ref() {
        Some(a) => a,
        None => return,
    };
    let waited_start = std::time::Instant::now();
    let mut warned = false;
    loop {
        if state.shutdown() {
            return;
        }
        if state.paused() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if ap.queued_samples() <= demux_ahead_samples {
            return;
        }
        if !warned && waited_start.elapsed() > Duration::from_secs(3) {
            tracing::warn!(target: "demux",
                "audio backpressure stuck >3s, queued={} threshold={}",
                ap.queued_samples(), demux_ahead_samples);
            warned = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
