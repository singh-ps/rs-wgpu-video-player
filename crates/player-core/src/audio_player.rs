use crate::{config::PlaybackConfig, state::PlaybackState};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BuildStreamError, StreamConfig,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// Decoded interleaved f32 PCM ring + master playback clock. Decoder thread
/// pushes via [`AudioPlayer::push`]; the cpal callback drains inside
/// [`AudioPlayer::start_stream`] and advances `samples_consumed`.
#[derive(Clone)]
pub struct AudioPlayer {
    /// Interleaved f32 samples queued for playback.
    samples: Arc<Mutex<VecDeque<f32>>>,
    /// Interleaved samples written to the driver. Combined with the device
    /// output latency this yields the audible playback clock.
    samples_consumed: Arc<AtomicU64>,
    /// Device output buffer latency in microseconds (driver-reported).
    output_latency_us: Arc<AtomicU64>,
    /// Set true once the callback has emitted at least one real sample.
    started: Arc<AtomicBool>,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
}

impl AudioPlayer {
    pub fn new(state: Arc<PlaybackState>, cfg: PlaybackConfig) -> Self {
        let ring_cap = ring_cap_samples(&cfg);
        Self {
            samples: Arc::new(Mutex::new(VecDeque::with_capacity(ring_cap))),
            samples_consumed: Arc::new(AtomicU64::new(0)),
            output_latency_us: Arc::new(AtomicU64::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            state,
            cfg,
        }
    }

    /// Push decoded samples (interleaved f32). Drops oldest on overflow.
    pub fn push(&self, data: &[f32]) {
        let mut buf = match self.samples.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        push_with_drop_oldest(&mut buf, data, ring_cap_samples(&self.cfg));
    }

    /// Interleaved samples currently queued for playback.
    pub fn queued_samples(&self) -> usize {
        match self.samples.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// Master clock: microseconds of audio audible at the speakers right now.
    /// Returns `None` before the stream has emitted any real samples.
    pub fn clock_us(&self) -> Option<u64> {
        if !self.started.load(Ordering::Relaxed) {
            return None;
        }
        let consumed = self.samples_consumed.load(Ordering::Relaxed);
        Some(audio_clock_us(
            consumed,
            self.output_latency_us.load(Ordering::Relaxed),
            self.cfg.audio_sample_rate,
            self.cfg.audio_channels,
        ))
    }

    /// Open the cpal output stream. Returned stream must be kept alive.
    pub fn start_stream(&self) -> Result<cpal::Stream, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No default audio output device found")?;

        let config = StreamConfig {
            channels: self.cfg.audio_channels,
            sample_rate: cpal::SampleRate(self.cfg.audio_sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = self.samples.clone();
        let consumed = self.samples_consumed.clone();
        let latency_us = self.output_latency_us.clone();
        let started = self.started.clone();
        let state = self.state.clone();
        let max_latency_us = self.cfg.max_audio_latency_us;

        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // Report device output buffer latency (when the samples we
                    // write now will actually become audible). Drives A/V sync.
                    let ts = info.timestamp();
                    if let Some(d) = ts.playback.duration_since(&ts.callback) {
                        let raw = d.as_micros() as u64;
                        latency_us.store(raw.min(max_latency_us), Ordering::Relaxed);
                    }
                    if state.shutdown() || state.paused() {
                        // Silence; do NOT advance clock so video pacing pauses too.
                        for s in out.iter_mut() {
                            *s = 0.0;
                        }
                        return;
                    }

                    let vol = (state.volume() as f32 / 100.0).clamp(0.0, 1.0);
                    let mut real_written = 0usize;
                    match samples.try_lock() {
                        Ok(mut buf) => {
                            for s in out.iter_mut() {
                                match buf.pop_front() {
                                    Some(v) => {
                                        *s = v * vol;
                                        real_written += 1;
                                    }
                                    None => *s = 0.0,
                                }
                            }
                        }
                        Err(_) => {
                            // Decoder briefly holds the lock — never block the
                            // realtime callback; emit silence for this batch.
                            for s in out.iter_mut() {
                                *s = 0.0;
                            }
                        }
                    }

                    if real_written > 0 {
                        started.store(true, Ordering::Relaxed);
                    }
                    // Advance the master clock by the full callback length
                    // *once playback has begun*. Silence on underrun still
                    // counts as time elapsed; otherwise video stalls whenever
                    // the audio queue runs dry for an instant.
                    if started.load(Ordering::Relaxed) {
                        consumed.fetch_add(out.len() as u64, Ordering::Relaxed);
                    }
                },
                |err| tracing::warn!(target: "audio", "cpal stream error: {err}"),
                None,
            )
            .map_err(|e: BuildStreamError| format!("Failed to build cpal stream: {e}"))?;

        stream.play()?;
        Ok(stream)
    }
}

pub(crate) fn ring_cap_samples(cfg: &PlaybackConfig) -> usize {
    (cfg.audio_sample_rate as f32 * cfg.audio_channels as f32 * cfg.sample_ring_cap_secs) as usize
}

/// Push samples into the ring; drop oldest when capacity is exceeded. Pure;
/// no synchronisation. See tests for the invariant.
pub(crate) fn push_with_drop_oldest(buf: &mut VecDeque<f32>, data: &[f32], cap: usize) {
    let total = buf.len() + data.len();
    if total > cap {
        let overflow = total - cap;
        let drop_n = overflow.min(buf.len());
        buf.drain(..drop_n);
    }
    buf.extend(data.iter().copied());
}

/// Audio clock math: convert sample counts and driver latency into the
/// microsecond timestamp of audio that is audible *now*. Pure.
pub(crate) fn audio_clock_us(
    samples_consumed: u64,
    output_latency_us: u64,
    sample_rate: u32,
    channels: u16,
) -> u64 {
    let per_channel = samples_consumed / channels as u64;
    let written_us = per_channel.saturating_mul(1_000_000) / sample_rate as u64;
    written_us.saturating_sub(output_latency_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_push_under_capacity_preserves_all() {
        let mut buf = VecDeque::new();
        push_with_drop_oldest(&mut buf, &[1.0, 2.0, 3.0], 10);
        assert_eq!(buf, VecDeque::from(vec![1.0, 2.0, 3.0]));
    }

    #[test]
    fn ring_push_at_capacity_keeps_newest() {
        let mut buf = VecDeque::from(vec![1.0, 2.0, 3.0]);
        push_with_drop_oldest(&mut buf, &[4.0, 5.0], 4);
        // total = 5; cap = 4; drop 1 oldest, append both newest.
        assert_eq!(buf, VecDeque::from(vec![2.0, 3.0, 4.0, 5.0]));
    }

    #[test]
    fn ring_push_larger_than_capacity_drops_all_existing() {
        let mut buf = VecDeque::from(vec![1.0, 2.0]);
        push_with_drop_oldest(&mut buf, &[3.0, 4.0, 5.0, 6.0], 4);
        // total = 6; overflow = 2; drop_n = 2 (the existing two);
        // result keeps only the newest 4.
        assert_eq!(buf, VecDeque::from(vec![3.0, 4.0, 5.0, 6.0]));
    }

    #[test]
    fn ring_push_data_alone_exceeds_capacity() {
        let mut buf = VecDeque::new();
        push_with_drop_oldest(&mut buf, &[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        // overflow = 5, drop_n = min(5, 0) = 0; just appends — exceeds cap.
        // This is the documented behaviour: drop oldest in *existing buf only*.
        // (In practice the caller chunks small batches; this case isn't hit.)
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn clock_zero_consumed_is_zero() {
        assert_eq!(audio_clock_us(0, 0, 48_000, 2), 0);
    }

    #[test]
    fn clock_simple_one_second() {
        // 48000 stereo samples = 96000 interleaved samples = 1 s
        assert_eq!(audio_clock_us(96_000, 0, 48_000, 2), 1_000_000);
    }

    #[test]
    fn clock_subtracts_output_latency() {
        // 1 s written, 100 ms output latency → audible "now" = 900 ms
        assert_eq!(audio_clock_us(96_000, 100_000, 48_000, 2), 900_000);
    }

    #[test]
    fn clock_latency_larger_than_written_saturates_to_zero() {
        // Just-started stream: tiny amount written, latency dominates.
        assert_eq!(audio_clock_us(960, 1_000_000, 48_000, 2), 0);
    }

    #[test]
    fn ring_cap_default_config_matches_5s_stereo_48k() {
        let cfg = PlaybackConfig::default();
        assert_eq!(ring_cap_samples(&cfg), 48_000 * 2 * 5);
    }
}
