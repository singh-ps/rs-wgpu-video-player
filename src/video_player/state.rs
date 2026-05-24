use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Shared flags read by the decoder threads, audio callback, and UI. Bundled so
/// new shared state doesn't force edits across every signature in the player.
pub struct PlaybackState {
    pub shutdown: AtomicBool,
    pub paused: AtomicBool,
    /// Volume 0-100.
    pub volume: AtomicU32,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackState {
    pub fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(100),
        }
    }

    pub fn shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
    pub fn reset(&self) {
        self.shutdown.store(false, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    pub fn toggle_pause(&self) -> bool {
        let n = !self.paused();
        self.paused.store(n, Ordering::Relaxed);
        n
    }

    pub fn set_volume(&self, v: u32) {
        self.volume.store(v.clamp(0, 100), Ordering::Relaxed);
    }
    pub fn volume(&self) -> u32 {
        self.volume.load(Ordering::Relaxed)
    }
}
