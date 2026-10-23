//! Run the playback pipeline without a UI and report buffering / A/V timing.
//! Usage: cargo run -p player-core --example playback_probe -- [URL] [seconds]
use player_core::VideoPlayer;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8".to_owned());
    let seconds: u64 = args.next().as_deref().unwrap_or("30").parse()?;
    let rt = tokio::runtime::Builder::new_current_thread().build()?;
    let (mut player, _stream) = VideoPlayer::new();
    let mut events = rt.block_on(player.start_playback(&url))?;
    let mut frames = player.frame_buffer.subscribe();
    let start = Instant::now();
    let mut last_report = start;
    let mut last_frame = None;
    let mut received = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        while let Ok(event) = events.try_recv() {
            println!("event: {event:?}");
        }
        if frames.has_changed().unwrap_or(false) {
            last_frame = frames.borrow_and_update().clone();
            received += 1;
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let audio = player.audio_player.as_ref();
            let clock = audio.and_then(|a| a.clock_us());
            let frame_us = last_frame.as_ref().map(|f| f.ts_us);
            let skew_ms = frame_us
                .zip(clock)
                .map(|(v, a)| (v as i64 - a as i64) / 1000);
            println!(
                "wall={:.1}s frames={received} video_us={frame_us:?} audio_us={clock:?} skew_ms={skew_ms:?} queued_samples={}",
                start.elapsed().as_secs_f64(),
                audio.map_or(0, |a| a.queued_samples())
            );
            last_report = Instant::now();
            received = 0;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    player.stop_playback();
    // A stalled network read must not keep the diagnostic process alive.
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}
