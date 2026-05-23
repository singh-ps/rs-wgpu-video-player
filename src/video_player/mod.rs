use std::{
    error::Error,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

mod audio_player;
pub use audio_player::AudioPlayer;

mod decoder;
use decoder::loop_decoder;

mod frame_buffer;
use frame_buffer::{Frame, FrameBuffer};

mod probe;
pub use probe::get_video_info;

#[allow(dead_code)]
#[derive(Default)]
pub enum PixelFormat {
    #[default]
    RGBA,
    RGB24,
}

#[derive(Default)]
pub struct PlaybackParams {
    pub pixel_format: PixelFormat,
    pub is_live: bool,
}

pub struct VideoPlayer {
    pub frame_buffer: FrameBuffer,
    pub audio_player: AudioPlayer,
    is_initialized: bool,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// Volume 0–100, shared with AudioPlayer and the Slint UI.
    volume: Arc<AtomicU32>,
    /// Must be kept alive for the cpal stream to keep playing.
    _audio_stream: Option<cpal::Stream>,
}

impl VideoPlayer {
    pub fn new() -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(100));

        let audio_player = AudioPlayer::new(volume.clone(), paused.clone(), shutdown.clone());

        // Start cpal output stream immediately; it will output silence until the
        // decoder pushes samples.
        let _audio_stream = match audio_player.start_stream() {
            Ok(s) => {
                eprintln!("[VideoPlayer] cpal audio stream started");
                Some(s)
            }
            Err(e) => {
                eprintln!("[VideoPlayer] Failed to open audio output: {e}");
                None
            }
        };

        Self {
            frame_buffer: FrameBuffer::new(),
            audio_player,
            is_initialized: false,
            shutdown,
            paused,
            volume,
            _audio_stream,
        }
    }

    pub async fn start_playback(
        &mut self,
        url: &str,
        params: PlaybackParams,
    ) -> Result<(), Box<dyn Error>> {
        if self.is_initialized {
            return Err("VideoPlayer is already initialized".into());
        }

        let shutdown_clone = self.shutdown.clone();
        let paused_clone = self.paused.clone();
        self.shutdown.store(false, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        self.is_initialized = true;

        let audio_clone = self.audio_player.clone();

        tokio::task::spawn_blocking({
            let url = url.to_string();
            let buffer = self.frame_buffer.clone();
            move || loop_decoder(url, params, buffer, Some(audio_clone), shutdown_clone, paused_clone)
        });

        Ok(())
    }

    pub fn stop_playback(&mut self) {
        if !self.is_initialized {
            return;
        }
        self.shutdown.store(true, Ordering::Relaxed);
        self.is_initialized = false;
    }

    pub fn toggle_pause(&self) -> bool {
        let current = self.paused.load(Ordering::Relaxed);
        let new_state = !current;
        self.paused.store(new_state, Ordering::Relaxed);
        new_state
    }

    /// Set volume level 0–100. Immediately reflected in the cpal callback.
    pub fn set_volume(&self, vol: u32) {
        self.volume.store(vol.clamp(0, 100), Ordering::Relaxed);
    }

    /// Get the current volume level 0–100.
    #[allow(dead_code)]
    pub fn get_volume(&self) -> u32 {
        self.volume.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }

    #[allow(dead_code)]
    pub fn get_latest_frame(&mut self) -> Option<Arc<Frame>> {
        self.frame_buffer.consume()
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
