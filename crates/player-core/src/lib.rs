use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// Opaque handle to the live audio output stream. Hold it for as long as you
/// want audio to play; dropping it silences the device.
pub type AudioStream = cpal::Stream;

mod audio_player;
pub use audio_player::AudioPlayer;

mod config;
pub use config::PlaybackConfig;

mod decoder;
use decoder::loop_decoder;

mod error;
pub use error::{PlayerError, Result};

mod event;
pub use event::PlaybackEvent;

mod frame_buffer;
pub use frame_buffer::{Frame, FrameBuffer};

mod state;
use state::PlaybackState;

pub struct VideoPlayer {
    pub audio_player: AudioPlayer,
    pub frame_buffer: FrameBuffer,
    is_initialized: bool,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
}

impl VideoPlayer {
    /// Returns the player together with the cpal output stream. Caller must
    /// keep the stream alive for audio playback to continue; dropping it
    /// silences the stream.
    pub fn new() -> (Self, Option<AudioStream>) {
        Self::with_config(PlaybackConfig::default())
    }

    pub fn with_config(cfg: PlaybackConfig) -> (Self, Option<AudioStream>) {
        let state = Arc::new(PlaybackState::new());
        let audio_player = AudioPlayer::new(state.clone(), cfg);

        let audio_stream = match audio_player.start_stream() {
            Ok(s) => {
                tracing::info!(target: "audio", "cpal audio stream started");
                Some(s)
            }
            Err(e) => {
                tracing::warn!(target: "audio", "failed to open audio output: {e}");
                None
            }
        };

        let player = Self {
            audio_player,
            frame_buffer: FrameBuffer::new(),
            is_initialized: false,
            state,
            cfg,
        };
        (player, audio_stream)
    }

    pub async fn start_playback(&mut self, url: &str) -> Result<UnboundedReceiver<PlaybackEvent>> {
        if self.is_initialized {
            return Err(PlayerError::AlreadyRunning);
        }

        self.state.reset();
        self.is_initialized = true;

        let (tx, rx) = mpsc::unbounded_channel::<PlaybackEvent>();

        let audio = self.audio_player.clone();
        let state = self.state.clone();
        let cfg = self.cfg;
        let buffer = self.frame_buffer.clone();
        let dec_url = url.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = loop_decoder(dec_url, buffer, tx.clone(), Some(audio), state, cfg) {
                let _ = tx.send(PlaybackEvent::Error(format!("{e}")));
            }
        });

        Ok(rx)
    }

    pub fn stop_playback(&mut self) {
        if !self.is_initialized {
            return;
        }
        self.state.request_shutdown();
        self.is_initialized = false;
    }

    pub fn toggle_pause(&self) -> bool {
        self.state.toggle_pause()
    }

    /// Set volume level 0–100. Immediately reflected in the cpal callback.
    pub fn set_volume(&self, vol: u32) {
        self.state.set_volume(vol);
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.state.request_shutdown();
    }
}
