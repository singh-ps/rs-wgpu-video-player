use crate::{
    audio_player::AudioPlayer,
    config::PlaybackConfig,
    decoder::{demux::TaggedPacket, pts_to_us},
    error::Result,
    state::PlaybackState,
};
use ffmpeg::{
    codec::context::Context,
    software::resampling,
    util::{
        channel_layout::ChannelLayout,
        format::{sample::Type as SampleType, Sample},
        frame::Audio as AudioFrame,
    },
};
use ffmpeg_next as ffmpeg;
use std::sync::{mpsc, Arc};

/// Clock bookkeeping across seeks. `first_pts_us` anchors the 0-based
/// timeline (captured once, never reset); after a seek `reseed` makes the
/// next timestamped frame re-seed the playback clock at its position.
struct ClockSeed {
    first_pts_us: Option<i64>,
    reseed: bool,
}

pub fn audio_decode_thread(
    rx: mpsc::Receiver<TaggedPacket>,
    aparams: ffmpeg::codec::Parameters,
    atb: ffmpeg::Rational,
    player: AudioPlayer,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
) -> Result<()> {
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
        cfg.audio_sample_rate,
    )?;

    let mut frame = AudioFrame::empty();
    let mut seed = ClockSeed {
        first_pts_us: None,
        reseed: false,
    };
    let mut epoch: u64 = 0;

    for (pkt_epoch, packet) in rx {
        if state.shutdown() {
            break;
        }
        // Stale pre-seek packet: drain without decoding.
        if pkt_epoch < state.epoch() {
            continue;
        }
        if pkt_epoch != epoch {
            // First packet after a seek. The ring was already cleared by the
            // demux thread (begin_seek); here we discard decoder/resampler
            // internal state and arrange for the clock to be re-seeded from
            // the first post-seek frame's PTS.
            dec.flush();
            discard_resampler_state(&mut resampler);
            seed.reseed = true;
            epoch = pkt_epoch;
        }
        if let Err(e) = dec.send_packet(&packet) {
            tracing::warn!(target: "audio", "send_packet error: {e}");
            continue;
        }
        drain_audio(
            &mut dec, &mut resampler, &mut frame, &player, &state, &cfg, atb, &mut seed,
        );
    }

    if let Err(e) = dec.send_eof() {
        tracing::warn!(target: "audio", "send_eof error: {e}");
    }
    drain_audio(
        &mut dec, &mut resampler, &mut frame, &player, &state, &cfg, atb, &mut seed,
    );
    flush_resampler(&mut resampler, &player, &cfg);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_audio(
    dec: &mut ffmpeg::codec::decoder::Audio,
    resampler: &mut resampling::Context,
    frame: &mut AudioFrame,
    player: &AudioPlayer,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
    atb: ffmpeg::Rational,
    seed: &mut ClockSeed,
) {
    while dec.receive_frame(frame).is_ok() {
        if state.shutdown() {
            return;
        }

        if let Some(ts) = frame.timestamp() {
            if let Some(pts_us) = pts_to_us(ts, atb.0, atb.1) {
                // Anchor the timeline on the very first audio frame of the
                // session; the clock starts at 0 there by construction.
                let first = *seed.first_pts_us.get_or_insert(pts_us);
                if seed.reseed {
                    // Re-seed the clock where the audio actually landed (the
                    // demuxer seeks to a keyframe at or before the request).
                    let normalized = pts_us.saturating_sub(first).max(0) as u64;
                    player.resume_at_us(normalized);
                    seed.reseed = false;
                    tracing::debug!(target: "audio", "clock re-seeded at {}ms", normalized / 1000);
                }
            }
        }

        let mut resampled = AudioFrame::empty();
        if let Err(e) = resampler.run(frame, &mut resampled) {
            tracing::warn!(target: "audio", "resample error: {e}");
            continue;
        }
        push_resampled(&resampled, player, cfg);
    }
}

/// Throw away whatever the resampler is still holding (pre-seek samples).
fn discard_resampler_state(resampler: &mut resampling::Context) {
    loop {
        let mut sink = AudioFrame::empty();
        match resampler.flush(&mut sink) {
            Ok(_) if sink.samples() > 0 => continue,
            _ => break,
        }
    }
}

fn flush_resampler(
    resampler: &mut resampling::Context,
    player: &AudioPlayer,
    cfg: &PlaybackConfig,
) {
    loop {
        let mut more = AudioFrame::empty();
        match resampler.flush(&mut more) {
            Ok(_) => {
                if more.samples() == 0 {
                    break;
                }
                push_resampled(&more, player, cfg);
            }
            Err(e) => {
                tracing::warn!(target: "audio", "resampler flush error: {e}");
                break;
            }
        }
    }
}

fn push_resampled(resampled: &AudioFrame, player: &AudioPlayer, cfg: &PlaybackConfig) {
    let n_per_ch = resampled.samples();
    if n_per_ch == 0 {
        return;
    }
    let n_samples = n_per_ch.checked_mul(cfg.audio_channels as usize).unwrap_or(0);
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
