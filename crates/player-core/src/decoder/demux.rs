use crate::{
    audio_player::AudioPlayer, config::PlaybackConfig, decoder::DemuxCommand,
    state::PlaybackState,
};
use ffmpeg_next as ffmpeg;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

/// A demuxed packet tagged with the seek epoch it was read under. Decode
/// threads discard packets whose epoch is older than the current global
/// epoch, which is how a seek empties the (possibly deep) packet queues
/// without decoding stale data.
pub type TaggedPacket = (u64, ffmpeg::Packet);

#[allow(clippy::too_many_arguments)]
pub fn run_demux(
    ictx: &mut ffmpeg::format::context::Input,
    vindex: usize,
    aindex: usize,
    vtx: &mpsc::SyncSender<TaggedPacket>,
    atx: Option<&mpsc::SyncSender<TaggedPacket>>,
    audio: &Option<AudioPlayer>,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
    cmd_rx: &mpsc::Receiver<DemuxCommand>,
) {
    let demux_ahead_samples = (cfg.audio_sample_rate as f32
        * cfg.audio_channels as f32
        * cfg.demux_ahead_secs) as usize;

    // Container start offset (µs). The UI timeline is 0-based; seek targets
    // arrive 0-based and must be shifted onto the container's timeline.
    let start_offset_us = {
        let raw = unsafe { (*ictx.as_ptr()).start_time };
        if raw == ffmpeg::ffi::AV_NOPTS_VALUE || raw < 0 {
            0
        } else {
            raw
        }
    };

    'session: loop {
        // The packets() iterator borrows ictx mutably, so a seek can only
        // happen between iterator lifetimes: note the request, break the
        // inner loop, seek, then re-enter packets().
        let mut pending_seek: Option<u64> = None;

        'packets: for (stream, packet) in ictx.packets() {
            while state.paused() {
                if state.shutdown() {
                    break 'session;
                }
                if let Some(t) = poll_seek(cmd_rx) {
                    pending_seek = Some(t);
                    break 'packets;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if state.shutdown() {
                break 'session;
            }
            if let Some(t) = poll_seek(cmd_rx) {
                pending_seek = Some(t);
                break 'packets;
            }

            let epoch = state.epoch();
            let sidx = stream.index();
            if sidx == vindex {
                // Video flows freely. vrx capacity bounds memory; demux blocks
                // briefly on a full vrx, which is the desired backpressure.
                if vtx.send((epoch, packet)).is_err() {
                    break 'session; // video decoder gone
                }
            } else if sidx == aindex {
                if let Some(tx) = atx {
                    // Throttle only the audio dispatch — when the sample ring
                    // is already well-stocked, wait. This indirectly paces
                    // demuxing of further video packets via packet interleave
                    // order in the container.
                    wait_for_audio_room(audio, state, demux_ahead_samples);
                    if state.shutdown() {
                        break 'session;
                    }
                    if tx.send((epoch, packet)).is_err() {
                        // Audio thread gone — keep going on video.
                    }
                }
            }
        }

        match pending_seek {
            Some(target_us) => {
                let ts = start_offset_us.saturating_add(target_us as i64);
                // Reset the audio clock BEFORE bumping the epoch or sending
                // any post-seek packet: from this moment clock_us() is None,
                // so the video thread can't pace new frames against the stale
                // pre-seek position.
                if let Some(ap) = audio.as_ref() {
                    ap.begin_seek();
                }
                let new_epoch = state.bump_epoch();
                // `..ts` only supplies the max bound to avformat_seek_file
                // (max_ts = ts, inclusive in ffmpeg semantics).
                match ictx.seek(ts, ..ts) {
                    Ok(()) => {
                        tracing::info!(target: "demux", "seek to {}ms (epoch {})", target_us / 1000, new_epoch)
                    }
                    Err(e) => {
                        tracing::warn!(target: "demux", "seek failed: {e}")
                    }
                }
                // Loop back into packets() under the new epoch.
            }
            // packets() ran out without a seek request: genuine EOF/error.
            None => break 'session,
        }
    }
}

/// Drain the command channel, keeping only the most recent seek target —
/// scrubbing the slider can queue many requests; only the last matters.
fn poll_seek(cmd_rx: &mpsc::Receiver<DemuxCommand>) -> Option<u64> {
    let mut target = None;
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            DemuxCommand::Seek(us) => target = Some(us),
        }
    }
    target
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
