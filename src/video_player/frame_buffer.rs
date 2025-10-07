use std::sync::Arc;
use tokio::sync::watch::{channel, Receiver, Sender};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
    pub ts_us: u64,
}

pub struct FrameBuffer {
    tx: Sender<Option<Arc<Frame>>>,
    rx: Receiver<Option<Arc<Frame>>>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        let (tx, rx) = channel(None);
        Self { tx, rx }
    }

    /// Push a new frame into the buffer, overwrite existing frame if any.
    pub fn push(&self, frame: Arc<Frame>) {
        let _ = self.tx.send(Some(frame));
    }

    /// Pull the latest frame from the buffer, if any. This does not consume the frame.
    #[inline]
    pub fn pull(&self) -> Option<Arc<Frame>> {
        self.rx.borrow().clone()
    }

    /// Consume the latest frame from the buffer.
    #[inline]
    pub fn consume(&mut self) -> Option<Arc<Frame>> {
        self.rx.borrow_and_update().clone()
    }

    /// Subscribe to latest frames.
    pub async fn get_next(&mut self) -> Option<Arc<Frame>> {
        if self.rx.changed().await.is_ok() {
            self.rx.borrow().clone()
        } else {
            None
        }
    }

    /// Finish the frame buffer, no more frames will be pushed.
    pub fn finish(&self) {
        let _ = self.tx.send(None);
    }

    /// Subscribe to the frame buffer, get a receiver to receive latest frames.
    #[inline]
    pub fn subscribe(&self) -> Receiver<Option<Arc<Frame>>> {
        self.rx.clone()
    }
}
