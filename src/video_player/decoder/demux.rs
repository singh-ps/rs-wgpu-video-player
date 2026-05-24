use crate::video_player::{
    audio_player::{AudioPlayer, CHANNELS, SAMPLE_RATE},
    state::PlaybackState,
};
use ffmpeg_next as ffmpeg;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

/// Demux throttle threshold: when the audio sample ring holds more than this
/// many interleaved samples (~3 s @ 48 kHz stereo), pause demuxing of audio
/// packets. Bounds the audio pre-buffer while leaving video flow alone.
const DEMUX_AHEAD_SAMPLES: usize = (SAMPLE_RATE as usize) * (CHANNELS as usize) * 3;

pub fn run_demux(
    ictx: &mut ffmpeg::format::context::Input,
    vindex: usize,
    aindex: usize,
    vtx: &mpsc::SyncSender<ffmpeg::Packet>,
    atx: Option<&mpsc::SyncSender<ffmpeg::Packet>>,
    audio: &Option<AudioPlayer>,
    state: &Arc<PlaybackState>,
) {
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
                // already well-stocked (~3 s), wait. This indirectly paces
                // demuxing of further video packets via packet interleave
                // order in the container.
                wait_for_audio_room(audio, state);
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
fn wait_for_audio_room(audio: &Option<AudioPlayer>, state: &Arc<PlaybackState>) {
    let ap = match audio.as_ref() {
        Some(a) => a,
        None => return,
    };
    loop {
        if state.shutdown() {
            return;
        }
        if state.paused() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if ap.queued_samples() <= DEMUX_AHEAD_SAMPLES {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
