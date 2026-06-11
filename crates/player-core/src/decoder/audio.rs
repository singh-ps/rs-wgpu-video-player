use crate::{
    audio_player::AudioPlayer, config::PlaybackConfig, error::Result, state::PlaybackState,
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

pub fn audio_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    aparams: ffmpeg::codec::Parameters,
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

    for packet in rx {
        if state.shutdown() {
            break;
        }
        if let Err(e) = dec.send_packet(&packet) {
            tracing::warn!(target: "audio", "send_packet error: {e}");
            continue;
        }
        drain_audio(&mut dec, &mut resampler, &mut frame, &player, &state, &cfg);
    }

    if let Err(e) = dec.send_eof() {
        tracing::warn!(target: "audio", "send_eof error: {e}");
    }
    drain_audio(&mut dec, &mut resampler, &mut frame, &player, &state, &cfg);
    flush_resampler(&mut resampler, &player, &cfg);
    Ok(())
}

fn drain_audio(
    dec: &mut ffmpeg::codec::decoder::Audio,
    resampler: &mut resampling::Context,
    frame: &mut AudioFrame,
    player: &AudioPlayer,
    state: &Arc<PlaybackState>,
    cfg: &PlaybackConfig,
) {
    while dec.receive_frame(frame).is_ok() {
        if state.shutdown() {
            return;
        }
        let mut resampled = AudioFrame::empty();
        if let Err(e) = resampler.run(frame, &mut resampled) {
            tracing::warn!(target: "audio", "resample error: {e}");
            continue;
        }
        push_resampled(&resampled, player, cfg);
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
