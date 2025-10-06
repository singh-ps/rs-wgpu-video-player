use std::error::Error;

mod app;
use app::App;

mod renderer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let app = App {};
    app.run().await
}
