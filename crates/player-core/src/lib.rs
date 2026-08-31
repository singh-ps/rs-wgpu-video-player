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
use decoder::{loop_decoder, DemuxCommand};

mod error;
pub use error::{PlayerError, Result};

mod event;
pub use event::PlaybackEvent;

mod frame_buffer;
pub use frame_buffer::{Frame, FrameBuffer};

mod state;
use state::PlaybackState;

pub struct VideoPlayer {
    /// `None` when no audio output could be opened; playback then runs
    /// video-only, paced by the wall clock. Never hold an `AudioPlayer`
    /// without a live output stream — nothing would drain its sample ring and
    /// the demux loop's audio backpressure would block forever.
    pub audio_player: Option<AudioPlayer>,
    pub frame_buffer: FrameBuffer,
    is_initialized: bool,
    state: Arc<PlaybackState>,
    cfg: PlaybackConfig,
    /// Live while a playback session runs; commands go to the demux loop.
    cmd_tx: Option<std::sync::mpsc::Sender<DemuxCommand>>,
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

        let (audio_player, audio_stream) = match audio_player.start_stream() {
            Ok(s) => {
                tracing::info!(target: "audio", "cpal audio stream started");
                (Some(audio_player), Some(s))
            }
            Err(e) => {
                tracing::warn!(target: "audio",
                    "failed to open audio output: {e} — playing video only");
                (None, None)
            }
        };

        let player = Self {
            audio_player,
            frame_buffer: FrameBuffer::new(),
            is_initialized: false,
            state,
            cfg,
            cmd_tx: None,
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
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<DemuxCommand>();
        self.cmd_tx = Some(cmd_tx);

        let audio = self.audio_player.clone();
        let state = self.state.clone();
        let cfg = self.cfg;
        let buffer = self.frame_buffer.clone();
        let dec_url = url.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = loop_decoder(dec_url, buffer, tx.clone(), audio, state, cfg, cmd_rx) {
                let _ = tx.send(PlaybackEvent::Error(format!("{e}")));
            }
        });

        Ok(rx)
    }

    pub fn stop_playback(&mut self) {
        if !self.is_initialized {
            return;
        }
        self.cmd_tx = None;
        self.state.request_shutdown();
        self.is_initialized = false;
    }

    /// Seek to `us` on the 0-based playback timeline. No-op when nothing is
    /// playing. The demux loop picks the request up between packets; rapid
    /// repeated calls (slider scrubbing) coalesce to the most recent target.
    pub fn seek_to_us(&self, us: u64) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(DemuxCommand::Seek(us));
        }
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
