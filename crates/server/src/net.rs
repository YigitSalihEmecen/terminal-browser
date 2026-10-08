//! WebSocket front door: TLS, token authentication, origin checks, the message pump.
//!
//! Threat model: anyone who can reach the port is untrusted until they present the token. The
//! server is *not* an open proxy: an authenticated client can only drive browser tabs it owns,
//! and navigation is restricted by scheme (and optionally private-network host) — see DESIGN.md.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use glyph_proto::{
    codec::{Decoder, Encoder},
    ClientCaps, ClientMsg, ServerMsg, PROTO_VERSION,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::Semaphore,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    protocol::WebSocketConfig,
    Message,
};

use crate::{metrics::Metrics, server::SessionHandle};

/// Something that can start a browsing session for a client. `Server` implements it; tests use a
/// fake so the transport can be exercised without Chromium.
pub trait SessionFactory: Send + Sync + 'static {
    fn open(&self, caps: ClientCaps) -> SessionHandle;
}

impl SessionFactory for Arc<crate::server::Server> {
    fn open(&self, caps: ClientCaps) -> SessionHandle {
        self.open_session(caps)
    }
}

#[derive(Clone)]
pub struct NetCfg {
    pub bind: SocketAddr,
    /// Required unless bound to loopback.
    pub token: Option<String>,
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// `Origin` values accepted from browsers. Empty: any request carrying an Origin is refused
    /// (a web page the user visits must not be able to drive a localhost server).
    pub allow_origins: Vec<String>,
    pub max_sessions: usize,
    pub zstd_level: i32,
    pub metrics: Arc<Metrics>,
}

impl NetCfg {
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            bind,
            token: None,
            tls: None,
            allow_origins: vec![],
            max_sessions: 8,
            zstd_level: 3,
            metrics: Arc::default(),
        }
    }
}

pub struct NetHandle {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl NetHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

const AUTH_WINDOW: Duration = Duration::from_secs(60);
const AUTH_MAX_FAILS: u32 = 5;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(20);
const DEAD_AFTER: Duration = Duration::from_secs(75);
const MAX_CLIENT_MSG: usize = 2 << 20;

/// Per-IP failed-authentication budget.
#[derive(Default)]
struct Strikes(Mutex<HashMap<IpAddr, (u32, Instant)>>);

impl Strikes {
    fn blocked(&self, ip: IpAddr) -> bool {
        let mut m = self.0.lock().unwrap();
        match m.get(&ip) {
            Some((n, t)) if t.elapsed() < AUTH_WINDOW => *n >= AUTH_MAX_FAILS,
            Some(_) => {
                m.remove(&ip);
                false
            }
            None => false,
        }
    }
    fn fail(&self, ip: IpAddr) {
        let mut m = self.0.lock().unwrap();
        let e = m.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= AUTH_WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
        if m.len() > 4096 {
            m.retain(|_, (_, t)| t.elapsed() < AUTH_WINDOW);
        }
    }
}

/// Constant-time equality (length leaks, content does not).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn random_token() -> Result<String> {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).map_err(|e| anyhow!("no randomness available: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub async fn serve(cfg: NetCfg, factory: Arc<dyn SessionFactory>) -> Result<NetHandle> {
    if !cfg.bind.ip().is_loopback() && cfg.token.as_deref().is_none_or(str::is_empty) {
        bail!(
            "refusing to listen on {} without a token (non-loopback binds require one)",
            cfg.bind
        );
    }
    let listener = TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    let addr = listener.local_addr()?;
    let acceptor = cfg.tls.clone().map(TlsAcceptor::from);
    let strikes = Arc::new(Strikes::default());
    let handshakes = Arc::new(Semaphore::new(32));
    let sessions = Arc::new(Semaphore::new(cfg.max_sessions.max(1)));
    let cfg = Arc::new(cfg);
    let task = tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = listener.accept().await else {
                continue;
            };
            if strikes.blocked(peer.ip()) {
                continue; // drop the TCP connection without spending anything on it
            }
            let Ok(hs_permit) = handshakes.clone().try_acquire_owned() else {
                continue;
            };
            let (cfg, acceptor, factory, strikes, sessions) = (
                cfg.clone(),
                acceptor.clone(),
                factory.clone(),
                strikes.clone(),
                sessions.clone(),
            );
            tokio::spawn(async move {
                let _ = sock.set_nodelay(true);
                let res = match acceptor {
                    Some(a) => match tokio::time::timeout(IO_TIMEOUT, a.accept(sock)).await {
                        Ok(Ok(tls)) => {
                            connection(tls, peer, cfg, factory, strikes, sessions, hs_permit).await
                        }
                        _ => return,
                    },
                    None => {
                        connection(sock, peer, cfg, factory, strikes, sessions, hs_permit).await
                    }
                };
                if let Err(e) = res {
                    tracing::debug!("{peer}: {e:#}");
                }
            });
        }
    });
    Ok(NetHandle { addr, task })
}

async fn connection<S>(
    stream: S,
    peer: SocketAddr,
    cfg: Arc<NetCfg>,
    factory: Arc<dyn SessionFactory>,
    strikes: Arc<Strikes>,
    sessions: Arc<Semaphore>,
    hs_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let allow = cfg.allow_origins.clone();
    let bearer: Arc<Mutex<Option<String>>> = Arc::default();
    let bearer2 = bearer.clone();
    // the Result<Response, ErrorResponse> shape is fixed by tungstenite's handshake callback trait
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if let Some(o) = req.headers().get("origin") {
            let o = o.to_str().unwrap_or("");
            if !allow.iter().any(|a| a == o) {
                let mut r = ErrorResponse::new(Some("origin not allowed".into()));
                *r.status_mut() = StatusCode::FORBIDDEN;
                return Err(r);
            }
        }
        if let Some(a) = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            *bearer2.lock().unwrap() = a.strip_prefix("Bearer ").map(str::to_owned);
        }
        Ok(resp)
    };
    let wsc = WebSocketConfig::default()
        .max_message_size(Some(MAX_CLIENT_MSG))
        .max_frame_size(Some(MAX_CLIENT_MSG));
    let ws = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(wsc)),
    )
    .await
    .map_err(|_| anyhow!("handshake timeout"))??;
    let (mut sink, mut stream) = ws.split();
    let mut dec = Decoder::new()?;
    let mut enc = Encoder::new(cfg.zstd_level)?;

    // first message must be Hello
    let first = tokio::time::timeout(IO_TIMEOUT, stream.next())
        .await
        .map_err(|_| anyhow!("no hello"))?;
    let Some(Ok(Message::Binary(bytes))) = first else {
        bail!("first message was not a binary Hello")
    };
    let ClientMsg::Hello(hello) = dec.decode::<ClientMsg>(&bytes)? else {
        bail!("first message was not Hello")
    };

    let supplied = hello
        .token
        .clone()
        .or_else(|| bearer.lock().unwrap().clone());
    let ok = match (&cfg.token, supplied) {
        (Some(want), Some(got)) => ct_eq(want.as_bytes(), got.as_bytes()),
        (Some(_), None) => false,
        (None, _) => cfg.bind.ip().is_loopback(),
    };
    if !ok {
        strikes.fail(peer.ip());
        // fixed delay, generic answer: nothing to learn about which part was wrong
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = sink
            .send(Message::Binary(
                enc.encode(&ServerMsg::Error("authentication failed".into()))?
                    .into(),
            ))
            .await;
        let _ = sink.close().await;
        return Ok(());
    }
    if hello.version != PROTO_VERSION {
        let m = format!(
            "protocol version mismatch: server {PROTO_VERSION}, client {}",
            hello.version
        );
        let _ = sink
            .send(Message::Binary(enc.encode(&ServerMsg::Error(m))?.into()))
            .await;
        return Ok(());
    }
    drop(hs_permit);
    let Ok(_slot) = sessions.clone().try_acquire_owned() else {
        let _ = sink
            .send(Message::Binary(
                enc.encode(&ServerMsg::Error("server is full".into()))?
                    .into(),
            ))
            .await;
        return Ok(());
    };

    let SessionHandle { tx, mut rx } = factory.open(hello.caps);
    let mut tick = tokio::time::interval(KEEPALIVE);
    let mut last_seen = Instant::now();
    let m = cfg.metrics.clone();
    use std::sync::atomic::Ordering::Relaxed;
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(msg) = out else { break };
                let bytes = enc.encode(&msg)?;
                m.bytes_out.fetch_add(bytes.len() as u64, Relaxed);
                sink.send(Message::Binary(bytes.into())).await?;
            }
            inc = stream.next() => match inc {
                Some(Ok(Message::Binary(b))) => {
                    last_seen = Instant::now();
                    m.bytes_in.fetch_add(b.len() as u64, Relaxed);
                    let msg = dec.decode::<ClientMsg>(&b)?;
                    if matches!(msg, ClientMsg::Hello(_)) { continue; }
                    if tx.send(msg).is_err() { break; }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => last_seen = Instant::now(), // ping/pong/text: liveness only
                Some(Err(e)) => return Err(e.into()),
            },
            _ = tick.tick() => {
                if last_seen.elapsed() > DEAD_AFTER { bail!("client timed out"); }
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------- TLS

pub fn tls_from_pem(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()?;
    let key =
        PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    server_config(certs, key)
}

fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(cfg))
}

pub struct SelfSigned {
    pub config: Arc<rustls::ServerConfig>,
    /// `sha256:` + lowercase hex of the certificate: what `glyph connect --fingerprint` pins.
    pub fingerprint: String,
}

pub fn self_signed(names: Vec<String>) -> Result<SelfSigned> {
    let ck = rcgen::generate_simple_self_signed(names)?;
    let der = ck.cert.der().clone();
    let key =
        PrivateKeyDer::try_from(ck.signing_key.serialize_der()).map_err(|e| anyhow!("{e}"))?;
    Ok(SelfSigned {
        fingerprint: fingerprint(&der),
        config: server_config(vec![der], key)?,
    })
}

pub fn fingerprint(der: &CertificateDer<'_>) -> String {
    let h = Sha256::digest(der.as_ref());
    format!(
        "sha256:{}",
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }

    #[test]
    fn strikes_block_after_budget_and_expire_per_ip() {
        let s = Strikes::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..AUTH_MAX_FAILS {
            assert!(!s.blocked(ip));
            s.fail(ip);
        }
        assert!(s.blocked(ip));
        assert!(!s.blocked(other));
    }

    #[test]
    fn self_signed_has_a_stable_fingerprint_format() {
        let s = self_signed(vec!["localhost".into()]).unwrap();
        assert!(
            s.fingerprint.starts_with("sha256:") && s.fingerprint.len() == 7 + 64,
            "{}",
            s.fingerprint
        );
    }

    #[test]
    fn caps_are_clamped() {
        let c = crate::server::clamp_caps(ClientCaps {
            cols: 60000,
            rows: 0,
            cell_px_w: 9999,
            cell_px_h: 0,
            graphics: glyph_proto::GraphicsProto::None,
            scheme: Default::default(),
            page_style: Default::default(),
            images: false,
        });
        assert_eq!((c.cols, c.rows, c.cell_px_w), (400, 3, 64));
    }
}
