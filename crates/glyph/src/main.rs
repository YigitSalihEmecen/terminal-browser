use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use glyph_client::{Config, Connection};
use glyph_proto::Profile;
use glyph_server::{Server, ServerCfg};

#[derive(Parser)]
#[command(
    name = "glyph",
    version,
    about = "A remote-capable terminal web browser"
)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/glyph/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProfileArg {
    Lean,
    Balanced,
    Full,
}

impl From<ProfileArg> for Profile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Lean => Profile::Lean,
            ProfileArg::Balanced => Profile::Balanced,
            ProfileArg::Full => Profile::Full,
        }
    }
}

#[derive(Args)]
struct ServerArgs {
    /// Resource profile: lean (cheapest), balanced, full.
    #[arg(long, value_enum, default_value = "balanced")]
    profile: ProfileArg,
    /// Chrome/Chromium binary (default: $GLYPH_CHROME or auto-detect).
    #[arg(long)]
    chrome: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Browse in this terminal; the browser runs in-process.
    Local {
        #[command(flatten)]
        server: ServerArgs,
        /// URL or search to open at start.
        url: Option<String>,
    },
    /// Load a URL in headless Chromium and print the page's visible text.
    Dump {
        url: String,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Render a URL once to the terminal grid (debugging aid).
    Render {
        url: String,
        #[arg(long, default_value_t = 100)]
        cols: u16,
        #[arg(long, default_value_t = 30)]
        rows: u16,
        /// Print with truecolor ANSI instead of plain text.
        #[arg(long)]
        ansi: bool,
        /// Skip the screenshot (structure only).
        #[arg(long)]
        no_pixels: bool,
        /// Also list interaction regions.
        #[arg(long)]
        regions: bool,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
}

fn init_logging() {
    // The TUI owns the terminal, so logs go to a file, and only when asked for.
    if let Ok(path) = std::env::var("GLYPH_LOG_FILE") {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_env("GLYPH_LOG"))
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .try_init();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Local { server, url } => {
            let cfg = Config::load(cli.config).map_err(anyhow::Error::msg)?;
            let srv = Server::start(ServerCfg {
                profile: server.profile.into(),
                chrome: server.chrome,
                // a local user may open files and data: URLs; remote clients may not (see `serve`)
                extra_schemes: vec!["file".into(), "data".into()],
                ..Default::default()
            })
            .await
            .context("starting the browser")?;
            let start = url.map(|u| glyph_client::omnibox::resolve(&u, &cfg.ui.search));
            let srv2 = srv.clone();
            let res = glyph_client::run(cfg, move |caps| async move {
                let h = srv2.open_session(caps);
                if let Some(u) = start {
                    let _ = h.tx.send(glyph_proto::ClientMsg::Navigate { url: u });
                }
                Ok(Connection { tx: h.tx, rx: h.rx })
            })
            .await;
            srv.browser.close().await;
            res?;
        }
        Cmd::Dump { url, timeout } => {
            println!(
                "{}",
                glyph_server::dump_text(&url, Duration::from_secs(timeout)).await?
            );
        }
        Cmd::Render {
            url,
            cols,
            rows,
            ansi,
            no_pixels,
            regions,
            timeout,
        } => {
            let r = glyph_server::render_url(
                &url,
                cols,
                rows,
                !no_pixels,
                Duration::from_secs(timeout),
            )
            .await?;
            if ansi {
                print!("{}", r.grid.to_ansi());
            } else {
                print!("{}", r.grid.dump_text());
            }
            if regions {
                for g in &r.regions {
                    println!(
                        "#{} {:?} {:?} {:?} {:?}",
                        g.id,
                        g.kind,
                        g.rects.first(),
                        g.href,
                        g.label
                    );
                }
            }
        }
    }
    Ok(())
}
