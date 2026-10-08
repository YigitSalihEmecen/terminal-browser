//! Minimal Chrome DevTools Protocol client over one WebSocket.
//!
//! Commands are `{id, sessionId?, method, params}`; replies carry the same `id`; events carry
//! `method`/`params` (+ `sessionId` when the target was attached with `flatten: true`).
//! Results stay as [`RawValue`] until a typed caller deserialises them, so a multi-megabyte
//! `DOMSnapshot` is parsed exactly once.

use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

#[derive(Debug, Clone, Deserialize)]
pub struct CdpError {
    pub code: i64,
    pub message: String,
}

impl fmt::Display for CdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CDP error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for CdpError {}

/// A protocol event. `params` is left raw; use [`Event::parse`].
#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Box<RawValue>,
}

impl Event {
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(self.params.get()).with_context(|| format!("parsing {}", self.method))
    }
}

type Reply = oneshot::Sender<Result<Box<RawValue>, CdpError>>;

enum Cmd {
    Call {
        session: Option<Arc<str>>,
        method: String,
        params: Value,
        reply: Reply,
    },
    Subscribe {
        session: String,
        tx: mpsc::UnboundedSender<Event>,
    },
    Unsubscribe {
        session: String,
    },
}

#[derive(Serialize)]
struct Outgoing<'a> {
    id: u64,
    method: &'a str,
    params: &'a Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    result: Option<Box<RawValue>>,
    error: Option<CdpError>,
    params: Option<Box<RawValue>>,
}

/// Cheap-to-clone handle to the connection task.
#[derive(Clone)]
pub struct Cdp {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Cdp {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let cfg = WebSocketConfig::default()
            .max_message_size(Some(256 << 20))
            .max_frame_size(Some(256 << 20));
        let (ws, _) = tokio_tungstenite::connect_async_with_config(ws_url, Some(cfg), false)
            .await
            .with_context(|| format!("connecting to {ws_url}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(ws, rx));
        Ok(Self { tx })
    }

    /// Raw call. `session = None` addresses the browser endpoint.
    pub async fn call_raw(
        &self,
        session: Option<&Arc<str>>,
        method: &str,
        params: Value,
    ) -> Result<Box<RawValue>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Call {
                session: session.cloned(),
                method: method.to_owned(),
                params,
                reply,
            })
            .map_err(|_| anyhow!("CDP connection closed"))?;
        let res = rx
            .await
            .map_err(|_| anyhow!("CDP connection closed during {method}"))?;
        res.map_err(|e| anyhow!(e).context(method.to_owned()))
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        session: Option<&Arc<str>>,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let raw = self.call_raw(session, method, params).await?;
        serde_json::from_str(raw.get()).with_context(|| format!("decoding result of {method}"))
    }

    /// Events for one session (`""` = browser-level events).
    pub fn subscribe(&self, session: &str) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.tx.send(Cmd::Subscribe {
            session: session.to_owned(),
            tx,
        });
        rx
    }

    pub fn unsubscribe(&self, session: &str) {
        let _ = self.tx.send(Cmd::Unsubscribe {
            session: session.to_owned(),
        });
    }

    /// Handle for one attached target.
    pub fn session(&self, id: &str) -> Session {
        Session {
            cdp: self.clone(),
            id: Arc::from(id),
        }
    }
}

/// A flattened target session: every call is tagged with its `sessionId`.
#[derive(Clone)]
pub struct Session {
    cdp: Cdp,
    id: Arc<str>,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        self.cdp.subscribe(&self.id)
    }
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Box<RawValue>> {
        self.cdp.call_raw(Some(&self.id), method, params).await
    }
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        self.cdp.call(Some(&self.id), method, params).await
    }
    /// Call and ignore the result body.
    pub async fn send(&self, method: &str, params: Value) -> Result<()> {
        self.call_raw(method, params).await.map(|_| ())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Last clone going away unregisters the event route.
        if Arc::strong_count(&self.id) == 1 {
            self.cdp.unsubscribe(&self.id);
        }
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn run(ws: Ws, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
    let (mut sink, mut stream) = ws.split();
    let mut next_id = 0u64;
    let mut pending: HashMap<u64, Reply> = HashMap::new();
    let mut routes: HashMap<String, mpsc::UnboundedSender<Event>> = HashMap::new();

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Call { session, method, params, reply } => {
                        next_id += 1;
                        let out = Outgoing { id: next_id, method: &method, params: &params, session_id: session.as_deref() };
                        let text = serde_json::to_string(&out).expect("serialisable");
                        pending.insert(next_id, reply);
                        if sink.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    Cmd::Subscribe { session, tx } => { routes.insert(session, tx); }
                    Cmd::Unsubscribe { session } => { routes.remove(&session); }
                }
            }
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(inc) = serde_json::from_str::<Incoming>(&text) else {
                    tracing::warn!("undecodable CDP message");
                    continue;
                };
                if let Some(id) = inc.id {
                    if let Some(reply) = pending.remove(&id) {
                        let _ = reply.send(match inc.error {
                            Some(e) => Err(e),
                            None => Ok(inc.result.unwrap_or_else(empty_object)),
                        });
                    }
                } else if let (Some(method), Some(params)) = (inc.method, inc.params) {
                    let key = inc.session_id.unwrap_or_default();
                    if let Some(tx) = routes.get(&key) {
                        if tx.send(Event { method, params }).is_err() {
                            routes.remove(&key);
                        }
                    }
                }
            }
        }
    }
    for (_, reply) in pending {
        let _ = reply.send(Err(CdpError {
            code: -1,
            message: "connection closed".into(),
        }));
    }
}

fn empty_object() -> Box<RawValue> {
    RawValue::from_string("{}".into()).expect("valid json")
}
