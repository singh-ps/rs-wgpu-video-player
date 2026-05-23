use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BuildStreamError, StreamConfig,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

/// Ring-buffer of decoded, interleaved f32 PCM samples (stereo 48 kHz).
/// The decoder thread pushes into it; the cpal callback drains it.
/// Mirrors `FrameBuffer` in design: cheap to clone, internally Arc-wrapped.
#[derive(Clone)]
pub struct AudioPlayer {
    /// Interleaved stereo f32 samples queued for playback.
    pub samples: Arc<Mutex<VecDeque<f32>>>,
    /// Volume level 0–100 (shared with VideoPlayer / Slint UI).
    pub volume: Arc<AtomicU32>,
    /// Pause state — same Arc shared with VideoPlayer.
    pub paused: Arc<AtomicBool>,
    /// Shutdown state — same Arc shared with VideoPlayer.
    pub shutdown: Arc<AtomicBool>,
}

impl AudioPlayer {
    pub fn new(
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            samples: Arc::new(Mutex::new(VecDeque::new())),
            volume,
            paused,
            shutdown,
        }
    }

    /// Push decoded samples (interleaved stereo f32) from the decoder thread.
    /// Mirrors `FrameBuffer::push()`.
    pub fn push(&self, data: &[f32]) {
        if let Ok(mut buf) = self.samples.lock() {
            buf.extend(data.iter().copied());
        }
    }

    /// Start the cpal output stream. The returned `cpal::Stream` **must be kept
    /// alive** for as long as audio should play — dropping it stops the stream.
    pub fn start_stream(&self) -> Result<cpal::Stream, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No default audio output device found")?;

        // Request stereo f32 at 48 kHz — matches the resampler output in decoder.rs.
        let config = StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(48000),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = self.samples.clone();
        let volume = self.volume.clone();
        let paused = self.paused.clone();
        let shutdown = self.shutdown.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    if shutdown.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                        // Fill with silence when paused or shut down.
                        out.fill(0.0);
                        return;
                    }

                    let vol = volume.load(Ordering::Relaxed) as f32 / 100.0;
                    let mut buf = samples.lock().unwrap();
                    for sample in out.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0.0) * vol;
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
