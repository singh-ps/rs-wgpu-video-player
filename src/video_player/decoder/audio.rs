use crate::video_player::{
    audio_player::{AudioPlayer, CHANNELS, SAMPLE_RATE},
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

pub fn audio_decode_thread(
    rx: mpsc::Receiver<ffmpeg::Packet>,
    aparams: ffmpeg::codec::Parameters,
    player: AudioPlayer,
    state: Arc<PlaybackState>,
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
        SAMPLE_RATE,
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
        drain_audio(&mut dec, &mut resampler, &mut frame, &player, &state);
    }

    if let Err(e) = dec.send_eof() {
        tracing::warn!(target: "audio", "send_eof error: {e}");
    }
    drain_audio(&mut dec, &mut resampler, &mut frame, &player, &state);
    flush_resampler(&mut resampler, &player);
    Ok(())
}

fn drain_audio(
    dec: &mut ffmpeg::codec::decoder::Audio,
    resampler: &mut resampling::Context,
    frame: &mut AudioFrame,
    player: &AudioPlayer,
    state: &Arc<PlaybackState>,
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
        push_resampled(&resampled, player);
    }
}

fn flush_resampler(resampler: &mut resampling::Context, player: &AudioPlayer) {
    loop {
        let mut more = AudioFrame::empty();
        match resampler.flush(&mut more) {
            Ok(_) => {
                if more.samples() == 0 {
                    break;
                }
                push_resampled(&more, player);
            }
            Err(e) => {
                tracing::warn!(target: "audio", "resampler flush error: {e}");
                break;
            }
        }
    }
}

fn push_resampled(resampled: &AudioFrame, player: &AudioPlayer) {
    let n_per_ch = resampled.samples();
    if n_per_ch == 0 {
        return;
    }
    let n_samples = n_per_ch.checked_mul(CHANNELS as usize).unwrap_or(0);
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
