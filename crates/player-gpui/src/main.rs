use std::env;

use gpui::{
    px, size, AppContext as _, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};
use gpui_component::Root;

mod player_view;
use player_view::PlayerView;

/// Subsystem targets used across the workspace, at their default level.
const DEFAULT_LOG_FILTER: &str = "video=info,audio=info,demux=info,decoder=info,app=info";

fn main() {
    // Logs are tagged by subsystem — `video`, `audio`, `demux`, `decoder`,
    // `app` — not by module path, so a crate-name directive like
    // `player_core=info` matches none of them and silences everything.
    //
    // Listing the subsystems explicitly rather than defaulting to a bare
    // `info` keeps gpui/wgpu/cpal's own records out of the default output;
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

    // GPUI runs its own executor; the tokio runtime exists only to service
    // player-core's `spawn_blocking` decode loop. Entering it here makes
    // `Handle::current()` resolve on the main thread, where GPUI foreground
    // tasks (and therefore `start_playback`) run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .build()
        .expect("failed to build tokio runtime");
    let _tokio = runtime.enter();

    Application::new().run(move |cx| {
        gpui_component::init(cx);
        cx.activate(true);

        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("rs-wgpu-video-player".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| PlayerView::new(url.clone(), window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open window");
    });
}
