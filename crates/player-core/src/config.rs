use std::time::Duration;

/// Tunables for the playback pipeline. Defaults match the values that shipped
/// before this struct existed.
///
/// Note: `audio_channels` is structurally pinned to 2 — the resampler target
/// is hardcoded to `ChannelLayout::STEREO`. Changing this field alone will
/// produce wrong output; it's exposed for symmetry with `audio_sample_rate`.
#[derive(Copy, Clone, Debug)]
pub struct PlaybackConfig {
    /// Packet queue capacity from demux → video decoder.
    pub video_pkt_queue: usize,
    /// Packet queue capacity from demux → audio decoder.
    pub audio_pkt_queue: usize,
    /// Audio ring capacity in seconds of playback (RING_CAP =
    /// rate × ch × secs).
    pub sample_ring_cap_secs: f32,
    /// Demux throttles audio dispatch when the ring exceeds this many seconds
    /// of buffered audio. Bounds pre-buffer; doesn't affect video.
    pub demux_ahead_secs: f32,
    /// Slack added to the worst healthy flat period when deciding the audio
    /// output stream is dead; see [`PlaybackConfig::audio_stall_timeout`].
    pub audio_stall_margin_secs: f32,
    /// Drop a video frame if it would display this far past its PTS.
    pub late_drop_us: i64,
    /// Max single sleep slice while pacing video; keeps shutdown/pause checks
    /// responsive.
    pub pace_slice: Duration,
    /// Cap on driver-reported output latency. Values above this are treated
    /// as buggy timestamps and clamped, so the audio clock can't drift into
    /// the past and stall video.
    pub max_audio_latency_us: u64,
    pub audio_sample_rate: u32,
    pub audio_channels: u16,
}

impl PlaybackConfig {
    /// How long the sample ring may sit above the backpressure threshold
    /// without draining before demux concludes the output stream is dead and
    /// stops throttling.
    ///
    /// Derived rather than stored: the ring can hold at most
    /// `sample_ring_cap_secs - demux_ahead_secs` seconds above the threshold,
    /// and that drains in realtime once the in-flight packets are consumed —
    /// so a fixed timeout would start reporting healthy playback as a stall
    /// the moment the ring capacity was raised past it.
    pub fn audio_stall_timeout(&self) -> Duration {
        let headroom = (self.sample_ring_cap_secs - self.demux_ahead_secs).max(0.0);
        Duration::from_secs_f32(headroom + self.audio_stall_margin_secs.max(0.0))
    }
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            video_pkt_queue: 1024,
            audio_pkt_queue: 1024,
            sample_ring_cap_secs: 5.0,
            demux_ahead_secs: 3.0,
            audio_stall_margin_secs: 3.0,
            late_drop_us: 100_000,
            pace_slice: Duration::from_millis(20),
            max_audio_latency_us: 200_000,
            audio_sample_rate: 48_000,
            audio_channels: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PlaybackConfig;
    use std::time::Duration;

    #[test]
    fn default_stall_timeout_is_five_seconds() {
        assert_eq!(
            PlaybackConfig::default().audio_stall_timeout(),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn stall_timeout_tracks_ring_capacity() {
        // The whole point: a bigger ring means a longer healthy flat period,
        // so the timeout has to grow with it or healthy playback reads as a
        // stall.
        let cfg = PlaybackConfig {
            sample_ring_cap_secs: 20.0,
            ..PlaybackConfig::default()
        };
        assert_eq!(cfg.audio_stall_timeout(), Duration::from_secs(20));
    }

    #[test]
    fn stall_timeout_survives_inverted_config() {
        // demux_ahead above the ring cap is nonsensical, but must not panic
        // in Duration::from_secs_f32 via a negative value.
        let cfg = PlaybackConfig {
            sample_ring_cap_secs: 1.0,
            demux_ahead_secs: 9.0,
            ..PlaybackConfig::default()
        };
        assert_eq!(cfg.audio_stall_timeout(), Duration::from_secs(3));
    }
}
