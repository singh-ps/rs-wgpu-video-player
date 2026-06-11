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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = PlaybackState::new();
        assert!(!s.shutdown());
        assert!(!s.paused());
        assert_eq!(s.volume(), 100);
    }

    #[test]
    fn toggle_pause_flips_and_returns_new() {
        let s = PlaybackState::new();
        assert!(s.toggle_pause());
        assert!(s.paused());
        assert!(!s.toggle_pause());
        assert!(!s.paused());
    }

    #[test]
    fn request_shutdown_is_sticky_until_reset() {
        let s = PlaybackState::new();
        s.request_shutdown();
        assert!(s.shutdown());
        s.reset();
        assert!(!s.shutdown());
    }

    #[test]
    fn reset_clears_pause_too() {
        let s = PlaybackState::new();
        s.toggle_pause();
        s.request_shutdown();
        s.reset();
        assert!(!s.shutdown());
        assert!(!s.paused());
    }

    #[test]
    fn volume_clamps_to_100() {
        let s = PlaybackState::new();
        s.set_volume(150);
        assert_eq!(s.volume(), 100);
        s.set_volume(0);
        assert_eq!(s.volume(), 0);
        s.set_volume(42);
        assert_eq!(s.volume(), 42);
    }
}

