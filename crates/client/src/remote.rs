//! `glyph connect`: WebSocket (ws/wss) link to a remote server.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use glyph_proto::{
    codec::{Decoder, Encoder},
    ClientCaps, ClientHello, ClientMsg, ServerMsg, PROTO_VERSION,
};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, protocol::WebSocketConfig, Message,
};

use crate::term::Connection;

#[derive(Clone, Debug, Default)]
pub struct RemoteOpts {
    /// `ws://host:port`, `wss://host:port` or bare `host:port` (= ws).
    pub url: String,
    pub token: Option<String>,
    /// Pin the server certificate (`sha256:<hex>`); disables CA/hostname verification.
    pub fingerprint: Option<String>,
}

const KEEPALIVE: Duration = Duration::from_secs(20);
const DEAD_AFTER: Duration = Duration::from_secs(75);

pub fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let hex: String = s
        .strip_prefix("sha256:")
        .unwrap_or(s)
        .chars()
        .filter(|c| *c != ':')
        .collect();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("fingerprint must be 'sha256:' followed by 64 hex digits");
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)?;
    }
    Ok(out)
}

/// Accepts exactly one certificate (by SHA-256 of its DER), regardless of names or CAs.
#[derive(Debug)]
struct Pinned {
    want: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(end_entity.as_ref());
        if got.as_slice() == self.want {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate does not match the pinned fingerprint".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls_config(fingerprint: Option<&str>) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    let cfg = match fingerprint {
        Some(f) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                want: parse_fingerprint(f)?,
                provider,
            }))
            .with_no_client_auth(),
        None => {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok(Arc::new(cfg))
}

pub fn normalise_url(raw: &str) -> Result<url::Url> {
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("ws://{raw}")
    };
    let u = url::Url::parse(&with_scheme).with_context(|| format!("bad server address {raw:?}"))?;
    if !matches!(u.scheme(), "ws" | "wss") {
        bail!("server address must be ws:// or wss://");
    }
    if u.host_str().is_none() {
        bail!("server address has no host");
    }
    Ok(u)
}

/// Connect, authenticate and return a ready [`Connection`]. Auth/version problems surface here,
/// before the terminal is touched.
pub async fn connect(opts: &RemoteOpts, caps: ClientCaps) -> Result<Connection> {
    let url = normalise_url(&opts.url)?;
    let host = url.host_str().expect("checked").to_owned();
    let tls = url.scheme() == "wss";
    let port = url.port().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow!("timed out connecting to {host}:{port}"))?
    .with_context(|| format!("connecting to {host}:{port}"))?;
    let _ = tcp.set_nodelay(true);
    let req = url.as_str().into_client_request()?;
    let wsc = WebSocketConfig::default()
        .max_message_size(Some(glyph_proto::codec::MAX_DECODED))
        .max_frame_size(Some(glyph_proto::codec::MAX_DECODED));
    if tls {
        let name = ServerName::try_from(host.clone())
            .map_err(|e| anyhow!("bad TLS server name {host:?}: {e}"))?;
        let stream = TlsConnector::from(tls_config(opts.fingerprint.as_deref())?)
            .connect(name, tcp)
            .await
            .map_err(|e| anyhow!("TLS handshake with {host}:{port} failed: {e}"))?;
        let (ws, _) = tokio_tungstenite::client_async_with_config(req, stream, Some(wsc)).await?;
        finish(ws, opts, caps).await
    } else {
        if opts.fingerprint.is_some() {
            bail!("--fingerprint only applies to wss:// addresses");
        }
        if !is_local(&host) {
            eprintln!("warning: ws:// to {host} sends your token and every keystroke unencrypted; use wss:// or an SSH tunnel");
        }
        let (ws, _) = tokio_tungstenite::client_async_with_config(req, tcp, Some(wsc)).await?;
        finish(ws, opts, caps).await
    }
}

fn is_local(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn finish<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    opts: &RemoteOpts,
    caps: ClientCaps,
) -> Result<Connection>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let mut enc = Encoder::new(3)?;
    let mut dec = Decoder::new()?;
    let hello = ClientMsg::Hello(ClientHello {
        version: PROTO_VERSION,
        token: opts.token.clone(),
        caps,
    });
    sink.send(Message::Binary(enc.encode(&hello)?.into()))
        .await?;

    // The first answer is either the session's Hello or an error ("authentication failed", …).
    let first = loop {
        let m = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .map_err(|_| anyhow!("server did not answer"))?;
        match m {
            Some(Ok(Message::Binary(b))) => break dec.decode::<ServerMsg>(&b)?,
            Some(Ok(Message::Close(_))) | None => {
                bail!("server closed the connection (wrong token or address?)")
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    };
    match &first {
        ServerMsg::Error(e) => bail!("{e}"),
        ServerMsg::Hello(h) if h.version != PROTO_VERSION => bail!(
            "protocol version mismatch: server {}, client {PROTO_VERSION}",
            h.version
        ),
        ServerMsg::Hello(_) => {}
        _ => bail!("unexpected first message from server"),
    }

    let (to_srv_tx, mut to_srv_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (from_srv_tx, from_srv_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let _ = from_srv_tx.send(first);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(KEEPALIVE);
        let mut last = tokio::time::Instant::now();
        let err = loop {
            tokio::select! {
                m = to_srv_rx.recv() => {
                    let Some(m) = m else { break None };
                    match enc.encode(&m) {
                        Ok(b) => if let Err(e) = sink.send(Message::Binary(b.into())).await { break Some(e.to_string()) },
                        Err(e) => break Some(e.to_string()),
                    }
                }
                m = stream.next() => match m {
                    Some(Ok(Message::Binary(b))) => {
                        last = tokio::time::Instant::now();
                        match dec.decode::<ServerMsg>(&b) {
                            Ok(m) => if from_srv_tx.send(m).is_err() { break None },
                            Err(e) => break Some(format!("bad message from server: {e}")),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Some("server closed the connection".into()),
                    Some(Ok(_)) => last = tokio::time::Instant::now(),
                    Some(Err(e)) => break Some(e.to_string()),
                },
                _ = tick.tick() => {
                    if last.elapsed() > DEAD_AFTER { break Some("connection timed out".into()); }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break Some("connection lost".into()); }
                }
            }
        };
        if let Some(e) = err {
            let _ = from_srv_tx.send(ServerMsg::Error(e));
        }
        let _ = sink.close().await;
    });
    Ok(Connection {
        tx: to_srv_tx,
        rx: from_srv_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_parse_in_common_spellings() {
        let hex = "ab".repeat(32);
        assert!(parse_fingerprint(&format!("sha256:{hex}")).is_ok());
        assert!(parse_fingerprint(&hex).is_ok());
        let colons: Vec<String> = (0..32).map(|_| "AB".to_string()).collect();
        assert_eq!(parse_fingerprint(&colons.join(":")).unwrap(), [0xab; 32]);
        assert!(parse_fingerprint("sha256:abcd").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(normalise_url("host:7878").unwrap().scheme(), "ws");
        assert_eq!(normalise_url("wss://h.example/x").unwrap().scheme(), "wss");
        assert!(normalise_url("http://h").is_err());
        assert!(normalise_url("ws://").is_err());
    }
}
