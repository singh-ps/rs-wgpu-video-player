use std::error::Error;

mod app;
use app::App;

mod renderer;
mod video_player;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let app = App {};
    app.run().await
}
