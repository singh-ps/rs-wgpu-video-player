mod frame_buffer;
mod player;

#[derive(Default)]
pub enum OutputFormat {
    #[default]
    RGB8,
    RGB24,
}

pub struct VideoPlayer {}

impl VideoPlayer {
    pub fn new() -> Self {
        Self {}
    }
}
