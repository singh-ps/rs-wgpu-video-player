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
    /// Media sample position: samples written to the driver plus samples
    /// skipped on overflow. Temporary underrun silence does not advance it.
    samples_consumed: Arc<AtomicU64>,
    /// Device output buffer latency in microseconds (driver-reported).
    output_latency_us: Arc<AtomicU64>,
    /// Set true once the callback has emitted at least one real sample.
    started: Arc<AtomicBool>,
    /// Decoder reached EOF. Only then may silence advance beyond the last
    /// audio sample, allowing a longer video track to finish.
    finished: Arc<AtomicBool>,
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
            finished: Arc::new(AtomicBool::new(false)),
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
        let dropped = push_with_drop_oldest(&mut buf, data, ring_cap_samples(&self.cfg));
        // Dropping media skips forward on the timeline. Without accounting
        // for this, the clock permanently trails what the speakers play.
        self.samples_consumed
            .fetch_add(dropped as u64, Ordering::Relaxed);
        drop(buf);
        if dropped > 0 {
            tracing::warn!(target: "audio", "sample ring overflow: skipped {dropped} samples");
        }
    }

    /// Interleaved samples currently queued for playback.
    pub fn queued_samples(&self) -> usize {
        match self.samples.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// Begin a seek: drop all queued PCM and suspend the clock. `clock_us`
    /// returns `None` from here until post-seek samples actually play, so
    /// video pacing falls back to wall-clock instead of comparing new frames
    /// against the stale pre-seek position. Called from the demux thread
    /// *before* any post-seek packet is dispatched.
    pub fn begin_seek(&self) {
        let mut buf = match self.samples.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Serialize the reset with render/push, so an in-flight callback
        // cannot restore the old clock after it has been reset.
        self.started.store(false, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.samples_consumed.store(0, Ordering::Relaxed);
        buf.clear();
    }

    /// Re-seed the clock at `us` (normalized timeline). Called by the audio
    /// decode thread with the PTS of the first post-seek frame, before pushing
    /// its samples. The callback flips `started` back on once real samples
    /// play, and the clock advances from this value — so A/V realign exactly
    /// where the audio landed, not where the seek was requested.
    pub fn resume_at_us(&self, us: u64) {
        let samples = us
            .saturating_mul(self.cfg.audio_sample_rate as u64)
            .saturating_mul(self.cfg.audio_channels as u64)
            / 1_000_000;
        self.samples_consumed.store(samples, Ordering::Relaxed);
    }

    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::Release);
    }

    /// Fill one device buffer without blocking the realtime thread. Keep the
    /// clock on the media timeline: underrun/lock-contention silence is not
    /// decoded audio and must not move video ahead of audio when it resumes.
    pub(crate) fn render(&self, out: &mut [f32]) {
        out.fill(0.0);
        if self.state.shutdown() || self.state.paused() {
            return;
        }
        // `set_volume` clamps, but the field is public — never let a stray
        // value above 100 clip the samples we hand the driver.
        let vol = (self.state.volume() as f32 / 100.0).clamp(0.0, 1.0);
        let Ok(mut buf) = self.samples.try_lock() else {
            return;
        };
        let mut real_written = 0;
        for s in out.iter_mut() {
            let Some(v) = buf.pop_front() else { break };
            *s = v * vol;
            real_written += 1;
        }
        if real_written > 0 {
            self.started.store(true, Ordering::Relaxed);
        }
        let advanced = if self.finished.load(Ordering::Acquire) && buf.is_empty() {
            // End of audio is different from temporary network starvation.
            out.len()
        } else {
            real_written
        };
        if self.started.load(Ordering::Relaxed) {
            self.samples_consumed
                .fetch_add(advanced as u64, Ordering::Relaxed);
        }
    }

    /// Media position in samples: what the clock is built from, before output
    /// latency is subtracted. Unlike [`Self::clock_us`] this only moves when
    /// audio is actually consumed, so it — not the clock — is what pacing
    /// watches to tell a stalled output stream from driver latency jitter.
    pub fn consumed_samples(&self) -> u64 {
        self.samples_consumed.load(Ordering::Relaxed)
    }

    /// Driver-reported output latency, clamped to the configured ceiling.
    pub(crate) fn set_output_latency_us(&self, raw_us: u64) {
        self.output_latency_us
            .store(raw_us.min(self.cfg.max_audio_latency_us), Ordering::Relaxed);
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

        let player = self.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // Report device output buffer latency (when the samples we
                    // write now will actually become audible). Drives A/V sync.
                    let ts = info.timestamp();
                    if let Some(d) = ts.playback.duration_since(&ts.callback) {
                        player.set_output_latency_us(d.as_micros() as u64);
                    }
                    player.render(out);
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

/// Keep the newest `cap` samples and return the number of skipped samples,
/// including any discarded prefix of an oversized incoming batch.
pub(crate) fn push_with_drop_oldest(buf: &mut VecDeque<f32>, data: &[f32], cap: usize) -> usize {
    let dropped = (buf.len() + data.len()).saturating_sub(cap);
    let from_queue = dropped.min(buf.len());
    buf.drain(..from_queue);
    buf.extend(data[dropped - from_queue..].iter().copied());
    dropped
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

    fn test_player() -> AudioPlayer {
        AudioPlayer::new(Arc::new(PlaybackState::new()), PlaybackConfig::default())
    }

    #[test]
    fn network_starvation_and_recovery_preserve_media_clock() {
        let player = test_player();
        let mut out = [0.0; 960]; // 10 ms of stereo PCM
        player.push(&vec![0.5; 96_000]);
        for _ in 0..100 {
            player.render(&mut out);
        }
        assert_eq!(player.clock_us(), Some(1_000_000));
        // Three seconds of network starvation must not make video run three
        // seconds ahead of the audio that will arrive next.
        for _ in 0..300 {
            player.render(&mut out);
            assert!(out.iter().all(|&s| s == 0.0));
        }
        assert_eq!(player.clock_us(), Some(1_000_000));
        player.push(&vec![0.25; 9_600]);
        for _ in 0..10 {
            player.render(&mut out);
        }
        assert_eq!(player.clock_us(), Some(1_100_000));
        assert!(out.iter().all(|&s| s == 0.25));
    }

    #[test]
    fn partial_callback_counts_only_real_samples() {
        let player = test_player();
        player.push(&[0.5; 480]);
        let mut out = [1.0; 960];
        player.render(&mut out);
        assert_eq!(player.clock_us(), Some(5_000));
        assert!(out[..480].iter().all(|&s| s == 0.5));
        assert!(out[480..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn callback_lock_contention_does_not_advance_media_clock() {
        let player = test_player();
        player.push(&[0.5; 1920]);
        let mut out = [0.0; 960];
        player.render(&mut out);
        let guard = player.samples.lock().unwrap();
        player.render(&mut out);
        assert_eq!(player.clock_us(), Some(10_000));
        assert!(out.iter().all(|&s| s == 0.0));
        drop(guard);
        player.render(&mut out);
        assert_eq!(player.clock_us(), Some(20_000));
    }

    #[test]
    fn overflow_clock_matches_samples_that_actually_play() {
        let cfg = PlaybackConfig {
            sample_ring_cap_secs: 0.02,
            ..PlaybackConfig::default()
        };
        let player = AudioPlayer::new(Arc::new(PlaybackState::new()), cfg);
        player.push(&[0.25; 1920]);
        player.push(&[0.5; 960]); // Drops the oldest 10 ms.
        assert_eq!(player.queued_samples(), 1920);
        let mut out = [0.0; 1920];
        player.render(&mut out);
        assert_eq!(player.clock_us(), Some(30_000));
        assert!(out[..960].iter().all(|&s| s == 0.25));
        assert!(out[960..].iter().all(|&s| s == 0.5));
    }

    #[test]
    fn audio_eof_allows_longer_video_track_to_finish() {
        let player = test_player();
        player.push(&[0.5; 960]);
        player.finish();
        let mut out = [0.0; 960];
        for _ in 0..3 {
            player.render(&mut out);
        }
        assert_eq!(player.clock_us(), Some(30_000));
        assert!(out.iter().all(|&s| s == 0.0));
        player.begin_seek();
        player.resume_at_us(1_000_000);
        player.push(&[0.5; 960]);
        player.render(&mut out);
        player.render(&mut out);
        assert_eq!(player.clock_us(), Some(1_010_000));
    }

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
        let dropped = push_with_drop_oldest(&mut buf, &[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        assert_eq!(dropped, 2);
        assert_eq!(buf, VecDeque::from(vec![3.0, 4.0, 5.0]));
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
