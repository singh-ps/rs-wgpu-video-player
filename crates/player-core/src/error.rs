use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlayerError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),
    #[error("no video stream in source")]
    NoVideoStream,
    #[error("no decoder found for codec")]
    DecoderNotFound,
    #[error("playback already running")]
    AlreadyRunning,
}

pub type Result<T> = std::result::Result<T, PlayerError>;
