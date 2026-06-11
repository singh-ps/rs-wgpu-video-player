use std::sync::Arc;
use tokio::sync::watch::{channel, Receiver, Sender};

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
    pub ts_us: u64,
}

/// Latest-frame-wins channel between the video decoder and the UI consumer.
/// `push` overwrites whatever is currently there; subscribers that fall behind
/// only ever see the most recent frame. This is the structural backpressure
/// against the renderer — without it, a slow UI silently accumulates frames
/// the user will never see.
#[derive(Clone)]
pub struct FrameBuffer {
    tx: Sender<Option<Arc<Frame>>>,
    rx: Receiver<Option<Arc<Frame>>>,
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameBuffer {
    pub fn new() -> Self {
        let (tx, rx) = channel(None);
        Self { tx, rx }
    }

    /// Overwrite whatever is currently in the buffer.
    pub fn push(&self, frame: Arc<Frame>) {
        let _ = self.tx.send(Some(frame));
    }

    pub fn subscribe(&self) -> Receiver<Option<Arc<Frame>>> {
        self.rx.clone()
    }
}
