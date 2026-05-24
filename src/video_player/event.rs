use crate::video_player::frame_buffer::Frame;
use std::sync::Arc;

#[derive(Debug)]
pub enum PlaybackEvent {
    Frame(Arc<Frame>),
    Duration(u64),
    Ended,
    Error(String),
}
