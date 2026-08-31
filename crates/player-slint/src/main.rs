use std::{env, error::Error};

mod app;
use app::App;

/// Subsystem targets used across the workspace, at their default level.
const DEFAULT_LOG_FILTER: &str = "video=info,audio=info,demux=info,decoder=info,app=info";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Logs are tagged by subsystem — `video`, `audio`, `demux`, `decoder`,
    // `app` — not by module path, so a crate-name directive like
    // `player_core=info` matches none of them and silences everything.
    //
    // Listing the subsystems explicitly rather than defaulting to a bare
    // `info` keeps slint/wgpu/cpal's own records out of the default output;
    // tracing-log is on by default, so a bare level picks those up too. Add
    // new subsystems here as they appear.
    //
    // Per-subsystem filtering still works through RUST_LOG, e.g.
    // `RUST_LOG=video=debug,audio=debug`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .init();

    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8".to_string());

    let app = App {};
    app.run(url).await
}
