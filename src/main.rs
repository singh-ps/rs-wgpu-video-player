use std::error::Error;

mod app;
use app::App;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let app = App {};
    app.run().await
}
