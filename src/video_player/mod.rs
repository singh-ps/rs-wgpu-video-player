use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};

mod audio_player;
pub use audio_player::AudioPlayer;

mod decoder;
use decoder::loop_decoder;

mod error;
pub use error::{PlayerError, Result};

mod event;
pub use event::PlaybackEvent;

mod frame_buffer;

mod probe;
use probe::get_video_info;

mod state;
use state::PlaybackState;

pub struct VideoPlayer {
    pub audio_player: AudioPlayer,
    is_initialized: bool,
    state: Arc<PlaybackState>,
}

impl VideoPlayer {
    /// Returns the player together with the cpal output stream. Caller must
    /// keep the stream alive for audio playback to continue; dropping it
    /// silences the stream.
    pub fn new() -> (Self, Option<cpal::Stream>) {
        let state = Arc::new(PlaybackState::new());
        let audio_player = AudioPlayer::new(state.clone());

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
            is_initialized: false,
            state,
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

        // Probe runs in parallel with playback — emits Duration once known.
        let probe_tx = tx.clone();
        let probe_url = url.to_string();
        tokio::task::spawn_blocking(move || {
            match get_video_info(&probe_url) {
                Ok(info) => {
                    if let Some(dur) = info.duration_us {
                        let _ = probe_tx.send(PlaybackEvent::Duration(dur as u64));
                    }
                }
                Err(e) => tracing::warn!(target: "probe", "failed: {e}"),
            }
        });

        let audio = self.audio_player.clone();
        let state = self.state.clone();
        let dec_url = url.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = loop_decoder(dec_url, tx.clone(), Some(audio), state) {
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
