use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "glyph",
    version,
    about = "A remote-capable terminal web browser"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Load a URL in headless Chromium and print the page's visible text.
    Dump {
        url: String,
        /// Seconds to wait for the load event.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dump { url, timeout } => {
            let text = glyph_server::dump_text(&url, Duration::from_secs(timeout)).await?;
            println!("{text}");
        }
    }
    Ok(())
}
