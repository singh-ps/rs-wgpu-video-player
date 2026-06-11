use std::{env, error::Error};

mod app;
use app::App;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("player_core=info,player_slint=info")),
        )
        .init();

    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8".to_string());

    let app = App {};
    app.run(url).await
}
