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

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            video_pkt_queue: 1024,
            audio_pkt_queue: 1024,
            sample_ring_cap_secs: 5.0,
            demux_ahead_secs: 3.0,
            late_drop_us: 100_000,
            pace_slice: Duration::from_millis(20),
            max_audio_latency_us: 200_000,
            audio_sample_rate: 48_000,
            audio_channels: 2,
        }
    }
}
