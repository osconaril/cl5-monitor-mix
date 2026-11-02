use std::sync::Arc;

use fake_console::FakeConsole;

/// Starts the isolated console simulator. It intentionally binds only to
/// loopback unless a test harness supplies `FAKE_CONSOLE_ADDR`.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let address = std::env::var("FAKE_CONSOLE_ADDR").unwrap_or_else(|_| "127.0.0.1:49280".into());
    let console = Arc::new(FakeConsole::new());
    let bound = console.listen(&address).await?;
    println!("fake CL5 console listening on {bound}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
