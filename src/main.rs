use std::{env, error::Error};

mod app;
use app::App;

mod renderer;
mod video_player;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8".to_string());

    let app = App {};
    app.run(url).await
}
