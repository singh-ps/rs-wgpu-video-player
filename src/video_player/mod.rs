use std::{
    error::Error,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::task::JoinHandle;

mod decoder;

mod frame_buffer;
use frame_buffer::FrameBuffer;

mod probe;
pub use probe::get_video_info;

#[derive(Default)]
pub enum PixelFormat {
    #[default]
    RGB8,
    RGB24,
}

#[derive(Default)]
pub struct PlaybackParams {
    pub pixel_format: PixelFormat,
    pub is_live: bool,
}

pub struct VideoPlayer {
    frame_buffer: FrameBuffer,
    is_initialized: bool,
    decoder_handle: Option<JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>>,
    shutdown: AtomicBool,
}

impl VideoPlayer {
    pub fn new() -> Self {
        Self {
            frame_buffer: FrameBuffer::new(),
            is_initialized: false,
            decoder_handle: None,
            shutdown: AtomicBool::new(false),
        }
    }

    pub async fn start_playback(
        &mut self,
        url: &str,
        params: PlaybackParams,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if self.is_initialized {
            return Err("VideoPlayer is already initialized".into());
        }

        Ok(())
    }

    pub fn stop_playback(&mut self) {
        if !self.is_initialized {
            return;
        }

        self.shutdown.store(true, Ordering::Relaxed);

        // deal with the decoder handle

        self.is_initialized = false;
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
