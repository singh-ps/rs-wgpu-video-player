#[derive(Debug)]
pub enum PlaybackEvent {
    Duration(u64),
    Ended,
    Error(String),
}
