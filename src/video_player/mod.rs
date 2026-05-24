use std::{error::Error, sync::Arc};

mod audio_player;
pub use audio_player::AudioPlayer;

mod decoder;
use decoder::loop_decoder;

mod frame_buffer;
use frame_buffer::FrameBuffer;

mod probe;
pub use probe::get_video_info;

mod state;
use state::PlaybackState;

pub struct VideoPlayer {
    pub frame_buffer: FrameBuffer,
    pub audio_player: AudioPlayer,
    is_initialized: bool,
    state: Arc<PlaybackState>,
    /// Kept alive to keep the cpal output stream running.
    #[allow(dead_code)]
    audio_stream: Option<cpal::Stream>,
}

impl VideoPlayer {
    pub fn new() -> Self {
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

        Self {
            frame_buffer: FrameBuffer::new(),
            audio_player,
            is_initialized: false,
            state,
            audio_stream,
        }
    }

    pub async fn start_playback(&mut self, url: &str) -> Result<(), Box<dyn Error>> {
        if self.is_initialized {
            return Err("VideoPlayer is already initialized".into());
        }

        self.state.reset();
        self.is_initialized = true;

        let audio_clone = self.audio_player.clone();
        let state = self.state.clone();

        tokio::task::spawn_blocking({
            let url = url.to_string();
            let buffer = self.frame_buffer.clone();
            move || loop_decoder(url, buffer, Some(audio_clone), state)
        });

        Ok(())
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
