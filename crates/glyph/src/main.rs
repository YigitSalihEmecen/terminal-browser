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

#[derive(Clone, Copy, ValueEnum)]
enum ConfigCmd {
    /// Where the config file is looked for.
    Path,
    /// Print the documented example config.
    Default,
    /// Load the config and report problems.
    Check,
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

#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    server: ServerArgs,
    /// Address to listen on. Anything but loopback requires a token and (by default) TLS.
    #[arg(long, default_value = "127.0.0.1:7878")]
    bind: std::net::SocketAddr,
    /// Shared secret clients must present (or set GLYPH_TOKEN). Generated if a remote bind has none.
    #[arg(long)]
    token: Option<String>,
    /// Read the token from a file.
    #[arg(long, conflicts_with = "token")]
    token_file: Option<PathBuf>,
    /// TLS certificate chain (PEM) and private key (PEM).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Generate a throw-away certificate at start and print its fingerprint for pinning.
    #[arg(long, conflicts_with = "tls_cert")]
    tls_self_signed: bool,
    /// Extra DNS names / IPs for the self-signed certificate.
    #[arg(long)]
    tls_name: Vec<String>,
    /// Allow a non-loopback bind without TLS (e.g. behind an SSH tunnel or TLS-terminating proxy).
    #[arg(long)]
    no_tls: bool,
    /// Accept browsers' WebSocket upgrades from this Origin (default: none).
    #[arg(long)]
    allow_origin: Vec<String>,
    /// Let remote clients reach loopback/private-network hosts from the server's network position.
    #[arg(long)]
    allow_private_hosts: bool,
    #[arg(long, default_value_t = 8)]
    max_sessions: usize,
    /// zstd level for the wire (1 fastest … 19 smallest).
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,
    /// Print server stats (RSS, CPU, bytes) every N seconds.
    #[arg(long)]
    stats_interval: Option<u64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the browser here and let other terminals drive it over WebSocket.
    Serve(Box<ServeArgs>),
    /// Drive a browser running elsewhere (`glyph serve`).
    Connect {
        /// ws://host:port, wss://host:port or host:port
        addr: String,
        /// Server token (or set GLYPH_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// Pin the server's certificate (sha256:<hex>) instead of verifying it against CAs.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Browse in this terminal; the browser runs in-process.
    Local {
        #[command(flatten)]
        server: ServerArgs,
        /// URL or search to open at start.
        url: Option<String>,
    },
    /// Inspect configuration: `path`, `default` (prints the documented example) or `check`.
    Config {
        #[arg(value_enum, default_value = "check")]
        what: ConfigCmd,
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
        Cmd::Config { what } => match what {
            ConfigCmd::Path => println!(
                "{}",
                cli.config
                    .or_else(glyph_client::config::default_path)
                    .map_or("(no home directory)".into(), |p| p.display().to_string())
            ),
            ConfigCmd::Default => print!("{}", include_str!("../../../glyph.example.toml")),
            ConfigCmd::Check => {
                let c = Config::load(cli.config).map_err(anyhow::Error::msg)?;
                if c.warnings.is_empty() {
                    println!("ok");
                } else {
                    for w in &c.warnings {
                        println!("warning: {w}");
                    }
                    std::process::exit(1);
                }
            }
        },
        Cmd::Serve(args) => serve(*args).await?,
        Cmd::Connect {
            addr,
            token,
            fingerprint,
        } => {
            let cfg = Config::load(cli.config).map_err(anyhow::Error::msg)?;
            let opts = glyph_client::remote::RemoteOpts {
                url: addr,
                token: token.or_else(|| std::env::var("GLYPH_TOKEN").ok()),
                fingerprint,
            };
            glyph_client::run(cfg, move |caps| async move {
                glyph_client::remote::connect(&opts, caps).await
            })
            .await?;
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

async fn serve(a: ServeArgs) -> Result<()> {
    use glyph_server::net::{self, NetCfg};
    let loopback = a.bind.ip().is_loopback();

    let token = match (a.token, a.token_file) {
        (Some(t), _) => Some(t),
        (None, Some(f)) => Some(
            std::fs::read_to_string(&f)
                .with_context(|| format!("reading {}", f.display()))?
                .trim()
                .to_owned(),
        ),
        (None, None) => std::env::var("GLYPH_TOKEN").ok(),
    }
    .filter(|t| !t.is_empty());
    let (token, generated) = match token {
        Some(t) => (Some(t), false),
        None if !loopback => (Some(net::random_token()?), true),
        None => (None, false),
    };

    let mut fingerprint = None;
    let tls = if let (Some(c), Some(k)) = (&a.tls_cert, &a.tls_key) {
        Some(net::tls_from_pem(c, k)?)
    } else if a.tls_self_signed {
        let mut names = vec!["localhost".to_owned()];
        if !a.bind.ip().is_unspecified() {
            names.push(a.bind.ip().to_string());
        }
        names.extend(a.tls_name.iter().cloned());
        let ss = net::self_signed(names)?;
        fingerprint = Some(ss.fingerprint);
        Some(ss.config)
    } else if !loopback && !a.no_tls {
        anyhow::bail!("{} is not a loopback address: add --tls-self-signed (or --tls-cert/--tls-key), or --no-tls if something else provides encryption", a.bind);
    } else {
        None
    };

    let srv = Server::start(ServerCfg {
        profile: a.server.profile.into(),
        chrome: a.server.chrome,
        // remote clients get http/https only; no file:, data:, chrome:, …
        extra_schemes: vec![],
        block_private: !loopback && !a.allow_private_hosts,
        ..Default::default()
    })
    .await
    .context("starting the browser")?;

    let mut ncfg = NetCfg::new(a.bind);
    ncfg.token = token.clone();
    ncfg.tls = tls.clone();
    ncfg.allow_origins = a.allow_origin;
    ncfg.max_sessions = a.max_sessions;
    ncfg.zstd_level = a.zstd_level;
    ncfg.metrics = srv.metrics.clone();
    let handle = net::serve(ncfg, std::sync::Arc::new(srv.clone())).await?;

    let scheme = if tls.is_some() { "wss" } else { "ws" };
    eprintln!(
        "glyph serving on {scheme}://{}  (profile: {:?})",
        handle.addr, srv.cfg.profile
    );
    if let Some(t) = &token {
        eprintln!(
            "token: {t}{}",
            if generated {
                "   (generated; pass --token to choose your own)"
            } else {
                ""
            }
        );
    } else {
        eprintln!("no token: loopback only");
    }
    if let Some(f) = &fingerprint {
        eprintln!("fingerprint: {f}");
    }
    let host = if handle.addr.ip().is_unspecified() {
        "<this-host>".to_owned()
    } else {
        handle.addr.ip().to_string()
    };
    let mut cmd = format!("glyph connect {scheme}://{host}:{}", handle.addr.port());
    if token.is_some() {
        cmd.push_str(" --token <token>");
    }
    if let Some(f) = &fingerprint {
        cmd.push_str(&format!(" --fingerprint {f}"));
    }
    eprintln!("connect with: {cmd}");
    if srv.cfg.block_private {
        eprintln!(
            "private-network targets are blocked for clients (use --allow-private-hosts to change)"
        );
    }

    if let Some(secs) = a.stats_interval {
        let srv = srv.clone();
        tokio::spawn(async move {
            let mut last = (
                std::time::Instant::now(),
                srv.metrics.snapshot(),
                glyph_server::metrics::sample_tree(srv.browser.pid().unwrap_or(0)),
            );
            let mut tick = tokio::time::interval(Duration::from_secs(secs.max(1)));
            tick.tick().await;
            loop {
                tick.tick().await;
                let now = std::time::Instant::now();
                let m = srv.metrics.snapshot();
                let p = glyph_server::metrics::sample_tree(srv.browser.pid().unwrap_or(0));
                let dt = now.duration_since(last.0).as_secs_f64();
                let cpu = match (&p, &last.2) {
                    (Some(a), Some(b)) => ((a.cpu_secs - b.cpu_secs) / dt * 100.0).max(0.0),
                    _ => 0.0,
                };
                eprintln!(
                    "stats: mem {} MB (rss-sum {} MB, {} procs), cpu {:.1}%, out {:.1} KB/s, frames +{}, refresh {:.1} ms",
                    p.and_then(|p| p.mem_kb).map_or(0, |k| k / 1024),
                    p.map_or(0, |p| p.rss_kb / 1024),
                    p.map_or(0, |p| p.procs),
                    cpu,
                    (m.bytes_out - last.1.bytes_out) as f64 / 1024.0 / dt,
                    (m.frames_full + m.frames_diff) - (last.1.frames_full + last.1.frames_diff),
                    m.avg_refresh_ms
                );
                last = (now, m, p);
            }
        });
    }

    tokio::signal::ctrl_c().await?;
    eprintln!("shutting down");
    handle.abort();
    srv.browser.close().await;
    Ok(())
}
