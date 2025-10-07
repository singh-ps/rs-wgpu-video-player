use std::{
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

mod decoder;
use decoder::loop_decoder;

mod frame_buffer;
use frame_buffer::{Frame, FrameBuffer};

mod probe;
pub use probe::get_video_info;

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
    frame_buffer: FrameBuffer,
    is_initialized: bool,
    shutdown: Arc<AtomicBool>,
}

impl VideoPlayer {
    pub fn new() -> Self {
        Self {
            frame_buffer: FrameBuffer::new(),
            is_initialized: false,
            shutdown: Arc::new(AtomicBool::new(false)),
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

        tokio::task::spawn_blocking({
            let url = url.to_string();
            let buffer = self.frame_buffer.clone();
            move || loop_decoder(url, params, buffer, shutdown_clone)
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

    pub fn get_latest_frame(&mut self) -> Option<Arc<Frame>> {
        self.frame_buffer.consume()
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
