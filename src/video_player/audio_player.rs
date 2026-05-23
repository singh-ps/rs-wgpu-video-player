use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BuildStreamError, StreamConfig,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// Max queued interleaved samples (~5 s stereo @ 48 kHz). Large enough to
/// absorb the demux burst at startup without dropping anything; small enough
/// to bound memory.
const RING_CAP: usize = (SAMPLE_RATE as usize) * (CHANNELS as usize) * 5;

/// Cap on driver-reported output latency. Real WASAPI / CoreAudio / Pulse
/// latencies are tens of ms; values above this are almost certainly a buggy
/// timestamp and would push the audio clock into the past, stalling video.
const MAX_LATENCY_US: u64 = 200_000;

/// Decoded interleaved f32 PCM (stereo 48 kHz) ring + master playback clock.
/// Decoder thread pushes via [`AudioPlayer::push`]; the cpal callback drains
/// inside [`AudioPlayer::start_stream`] and advances [`samples_consumed`].
#[derive(Clone)]
pub struct AudioPlayer {
    /// Interleaved stereo f32 samples queued for playback.
    samples: Arc<Mutex<VecDeque<f32>>>,
    /// Interleaved samples written to the driver. Combined with the device
    /// output latency this yields the audible playback clock.
    samples_consumed: Arc<AtomicU64>,
    /// Device output buffer latency in microseconds (driver-reported). The
    /// callback fills `playback - callback` from cpal's `OutputCallbackInfo`.
    output_latency_us: Arc<AtomicU64>,
    /// Set true once callback has emitted at least one real sample.
    started: Arc<AtomicBool>,
    /// Volume 0–100 (shared with VideoPlayer / UI).
    pub volume: Arc<AtomicU32>,
    /// Pause flag — shared with VideoPlayer.
    pub paused: Arc<AtomicBool>,
    /// Shutdown flag — shared with VideoPlayer.
    pub shutdown: Arc<AtomicBool>,
}

impl AudioPlayer {
    pub fn new(
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            samples: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            samples_consumed: Arc::new(AtomicU64::new(0)),
            output_latency_us: Arc::new(AtomicU64::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            volume,
            paused,
            shutdown,
        }
    }

    /// Push decoded samples (interleaved stereo f32). Drops oldest on overflow.
    pub fn push(&self, data: &[f32]) {
        let mut buf = match self.samples.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let total = buf.len() + data.len();
        if total > RING_CAP {
            let overflow = total - RING_CAP;
            let drop_n = overflow.min(buf.len());
            buf.drain(..drop_n);
        }
        buf.extend(data.iter().copied());
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
    #[allow(dead_code)]
    pub fn clock_us(&self) -> Option<u64> {
        if !self.started.load(Ordering::Relaxed) {
            return None;
        }
        let consumed = self.samples_consumed.load(Ordering::Relaxed);
        let per_channel = consumed / (CHANNELS as u64);
        let written_us = per_channel.saturating_mul(1_000_000) / (SAMPLE_RATE as u64);
        let latency_us = self.output_latency_us.load(Ordering::Relaxed);
        Some(written_us.saturating_sub(latency_us))
    }

    /// Open the cpal output stream. Returned stream must be kept alive.
    pub fn start_stream(&self) -> Result<cpal::Stream, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No default audio output device found")?;

        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = self.samples.clone();
        let consumed = self.samples_consumed.clone();
        let latency_us = self.output_latency_us.clone();
        let started = self.started.clone();
        let volume = self.volume.clone();
        let paused = self.paused.clone();
        let shutdown = self.shutdown.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // Report device output buffer latency (when the samples we
                    // write now will actually become audible). Drives A/V sync.
                    let ts = info.timestamp();
                    if let Some(d) = ts.playback.duration_since(&ts.callback) {
                        let raw = d.as_micros() as u64;
                        latency_us.store(raw.min(MAX_LATENCY_US), Ordering::Relaxed);
                    }
                    if shutdown.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                        // Silence; do NOT advance clock so video pacing pauses too.
                        for s in out.iter_mut() {
                            *s = 0.0;
                        }
                        return;
                    }

                    let vol = (volume.load(Ordering::Relaxed) as f32 / 100.0).clamp(0.0, 1.0);
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
                |err| eprintln!("[AudioPlayer] cpal stream error: {err}"),
                None,
            )
            .map_err(|e: BuildStreamError| format!("Failed to build cpal stream: {e}"))?;

        stream.play()?;
        Ok(stream)
    }
}
