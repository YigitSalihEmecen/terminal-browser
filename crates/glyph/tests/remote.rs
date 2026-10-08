//! Client ⇄ server over a real loopback WebSocket, with a fake page backend (no Chromium).

use std::{sync::Arc, time::Duration};

use glyph_client::remote::{connect, RemoteOpts};
use glyph_client::Connection;
use glyph_proto::*;
use glyph_server::{
    net::{self, NetCfg, NetHandle, SessionFactory},
    outbox::Outbox,
    SessionHandle,
};
use tokio::{sync::mpsc::unbounded_channel, time::timeout};

/// A tiny "browser": row 0 says "ready", typed characters appear on row 1, and `Navigate{url:
/// "flood"}` rewrites row 2 a thousand times without waiting for acks.
struct Fake;

impl SessionFactory for Fake {
    fn open(&self, caps: ClientCaps) -> SessionHandle {
        let (tx, mut rx) = unbounded_channel::<ClientMsg>();
        let (out_tx, out_rx) = unbounded_channel::<ServerMsg>();
        tokio::spawn(async move {
            let _ = out_tx.send(ServerMsg::Hello(ServerHello {
                version: PROTO_VERSION,
                session: "fake".into(),
                profile: Profile::Balanced,
            }));
            let mut ob = Outbox::new(1, 2, 0);
            let mut grid = Grid::new(caps.cols, caps.rows, Style::default());
            grid.put_str(0, 0, "ready", Style::default(), caps.cols);
            for m in ob.offer(grid.clone(), vec![]) {
                let _ = out_tx.send(m);
            }
            let mut typed = String::new();
            while let Some(m) = rx.recv().await {
                let msgs = match m {
                    ClientMsg::Key(KeyEvent {
                        code: KeyCode::Char(c),
                        ..
                    }) => {
                        typed.push(c);
                        grid.put_str(0, 1, &typed, Style::default(), caps.cols);
                        ob.offer(grid.clone(), vec![])
                    }
                    ClientMsg::Ack { seq, .. } => ob.ack(seq).0,
                    ClientMsg::Navigate { url } if url == "big" => {
                        for y in 0..caps.rows {
                            grid.put_str(0, y, &format!("row {y}: the quick brown fox jumps over the lazy dog, again and again"), Style::default(), caps.cols);
                        }
                        ob.force_full();
                        ob.offer(grid.clone(), vec![])
                    }
                    ClientMsg::Navigate { url } if url == "flood" => {
                        let mut all = vec![];
                        for i in 0..1000 {
                            grid.put_str(
                                0,
                                2,
                                &format!("tick {i:04}"),
                                Style::default(),
                                caps.cols,
                            );
                            all.extend(ob.offer(grid.clone(), vec![]));
                        }
                        all
                    }
                    _ => vec![],
                };
                for m in msgs {
                    let _ = out_tx.send(m);
                }
            }
        });
        SessionHandle { tx, rx: out_rx }
    }
}

fn caps() -> ClientCaps {
    ClientCaps {
        cols: 60,
        rows: 12,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        images: false,
    }
}

async fn start(token: Option<&str>) -> (NetHandle, Arc<glyph_server::metrics::Metrics>) {
    let mut cfg = NetCfg::new("127.0.0.1:0".parse().unwrap());
    cfg.token = token.map(str::to_owned);
    let m = cfg.metrics.clone();
    (net::serve(cfg, Arc::new(Fake)).await.unwrap(), m)
}

fn opts(h: &NetHandle, token: Option<&str>) -> RemoteOpts {
    RemoteOpts {
        url: format!("ws://{}", h.addr),
        token: token.map(str::to_owned),
        fingerprint: None,
    }
}

struct View {
    grid: Option<Grid>,
    seq: u64,
}

impl View {
    fn apply(&mut self, m: &ServerMsg) -> bool {
        match m {
            ServerMsg::FullFrame {
                seq,
                cols,
                rows,
                runs,
                ..
            } => {
                let mut g = Grid::new(*cols, *rows, Style::default());
                apply_runs(&mut g, runs);
                self.grid = Some(g);
                self.seq = *seq;
                true
            }
            ServerMsg::Diff {
                seq, base, runs, ..
            } => {
                assert_eq!(*base, self.seq, "diff chain broken");
                apply_runs(self.grid.as_mut().unwrap(), runs);
                self.seq = *seq;
                true
            }
            _ => false,
        }
    }
    fn row(&self, y: u16) -> String {
        self.grid
            .as_ref()
            .map(|g| {
                g.dump_text()
                    .lines()
                    .nth(y as usize)
                    .unwrap_or("")
                    .to_owned()
            })
            .unwrap_or_default()
    }
}

async fn next(c: &mut Connection) -> ServerMsg {
    timeout(Duration::from_secs(5), c.rx.recv())
        .await
        .expect("timeout")
        .expect("closed")
}

#[tokio::test]
async fn frames_keys_and_acks_round_trip() {
    let (h, _) = start(Some("s3cret")).await;
    let mut c = connect(&opts(&h, Some("s3cret")), caps()).await.unwrap();
    let mut v = View { grid: None, seq: 0 };
    assert!(matches!(next(&mut c).await, ServerMsg::Hello(_)));
    loop {
        let m = next(&mut c).await;
        if v.apply(&m) {
            c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
            break;
        }
    }
    assert_eq!(v.row(0), "ready");
    for ch in "héllo 日本".chars() {
        c.tx.send(ClientMsg::Key(KeyEvent {
            code: KeyCode::Char(ch),
            mods: Mods::default(),
        }))
        .unwrap();
    }
    while v.row(1) != "héllo 日本" {
        let m = next(&mut c).await;
        if v.apply(&m) {
            c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
        }
    }
}

#[tokio::test]
async fn wrong_missing_and_right_tokens() {
    let (h, _) = start(Some("s3cret")).await;
    let e = connect(&opts(&h, Some("nope")), caps())
        .await
        .err()
        .expect("wrong token accepted")
        .to_string();
    assert!(e.contains("authentication failed"), "{e}");
    let e = connect(&opts(&h, None), caps())
        .await
        .err()
        .expect("missing token accepted")
        .to_string();
    assert!(e.contains("authentication failed"), "{e}");
    assert!(connect(&opts(&h, Some("s3cret")), caps()).await.is_ok());
}

#[tokio::test]
async fn loopback_without_a_token_is_allowed_but_remote_binds_need_one() {
    let (h, _) = start(None).await;
    assert!(connect(&opts(&h, None), caps()).await.is_ok());
    let cfg = NetCfg::new("0.0.0.0:0".parse().unwrap());
    let e = net::serve(cfg, Arc::new(Fake))
        .await
        .err()
        .expect("0.0.0.0 without token must be refused");
    assert!(e.to_string().contains("without a token"), "{e}");
    let mut cfg = NetCfg::new("0.0.0.0:0".parse().unwrap());
    cfg.token = Some("x".into());
    assert!(net::serve(cfg, Arc::new(Fake)).await.is_ok());
}

#[tokio::test]
async fn browsers_cannot_drive_a_localhost_server() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (h, _) = start(None).await;
    let mut req = format!("ws://{}", h.addr).into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req)
        .await
        .expect_err("origin accepted");
    assert!(err.to_string().contains("403"), "{err}");
    // no Origin header (a real client) is fine
    assert!(tokio_tungstenite::connect_async(format!("ws://{}", h.addr))
        .await
        .is_ok());
    // an explicitly allowed origin is let through
    let mut cfg = NetCfg::new("127.0.0.1:0".parse().unwrap());
    cfg.allow_origins = vec!["https://ok.example".into()];
    let h2 = net::serve(cfg, Arc::new(Fake)).await.unwrap();
    let mut req = format!("ws://{}", h2.addr).into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "https://ok.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
}

#[tokio::test]
async fn repeated_bad_tokens_lock_the_address_out() {
    let (h, _) = start(Some("s3cret")).await;
    for _ in 0..5 {
        assert!(connect(&opts(&h, Some("bad")), caps()).await.is_err());
    }
    // even the right token is refused now (the TCP connection is dropped unanswered)
    let r = timeout(
        Duration::from_secs(5),
        connect(&opts(&h, Some("s3cret")), caps()),
    )
    .await
    .expect("lockout must fail fast, not hang");
    assert!(r.is_err(), "locked-out address was served");
}

#[tokio::test]
async fn slow_client_gets_a_coalesced_latest_state_not_a_backlog() {
    let (h, _) = start(None).await;
    let mut c = connect(&opts(&h, None), caps()).await.unwrap();
    let mut v = View { grid: None, seq: 0 };
    // take the first frame and ack it
    loop {
        let m = next(&mut c).await;
        if v.apply(&m) {
            c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
            break;
        }
    }
    // server produces 1000 states; the client reads but does NOT ack
    c.tx.send(ClientMsg::Navigate {
        url: "flood".into(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut frames = 0;
    while let Ok(m) = c.rx.try_recv() {
        if v.apply(&m) {
            frames += 1;
        }
    }
    assert!(
        frames <= 2,
        "window is 2 but {frames} frames arrived without any ack"
    );
    assert!(frames >= 1);
    // acking releases exactly one coalesced diff that carries the final state
    c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
    let mut more = 0;
    while v.row(2) != "tick 0999" {
        let m = next(&mut c).await;
        if v.apply(&m) {
            more += 1;
            c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
        }
    }
    assert!(
        more <= 2,
        "state should converge in a frame or two, not replay 1000 updates (got {more})"
    );
}

#[tokio::test]
async fn streaming_compression_shrinks_frames_a_lot() {
    let (h, metrics) = start(None).await;
    let mut c = connect(
        &opts(&h, None),
        ClientCaps {
            cols: 200,
            rows: 50,
            ..caps()
        },
    )
    .await
    .unwrap();
    c.tx.send(ClientMsg::Navigate { url: "big".into() })
        .unwrap();
    let mut v = View { grid: None, seq: 0 };
    let raw;
    loop {
        let m = next(&mut c).await;
        if v.apply(&m) {
            if v.row(49).starts_with("row 49") {
                raw = postcard::to_stdvec(&m).unwrap().len();
                break;
            }
            c.tx.send(ClientMsg::Ack { tab: 1, seq: v.seq }).unwrap();
        }
    }
    let wire = metrics.snapshot().bytes_out as usize;
    assert!(raw > 5_000, "test frame too small to mean anything: {raw}");
    assert!(wire * 3 < raw + 600, "wire {wire} bytes vs raw frame {raw}");
}

#[tokio::test]
async fn version_mismatch_and_garbage_are_handled_without_killing_the_server() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (h, _) = start(None).await;
    let url = format!("ws://{}", h.addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let mut enc = glyph_proto::codec::Encoder::new(3).unwrap();
    let mut dec = glyph_proto::codec::Decoder::new().unwrap();
    let hello = ClientMsg::Hello(ClientHello {
        version: 999,
        token: None,
        caps: caps(),
    });
    ws.send(Message::Binary(enc.encode(&hello).unwrap().into()))
        .await
        .unwrap();
    let Message::Binary(b) = ws.next().await.unwrap().unwrap() else {
        panic!()
    };
    let ServerMsg::Error(e) = dec.decode::<ServerMsg>(&b).unwrap() else {
        panic!("expected error")
    };
    assert!(e.contains("version"), "{e}");

    // garbage instead of a Hello: connection ends
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Binary(vec![1, 2, 3, 4, 5].into()))
        .await
        .unwrap();
    let end = timeout(Duration::from_secs(3), async {
        while let Some(Ok(m)) = ws.next().await {
            if m.is_close() {
                break;
            }
        }
    })
    .await;
    assert!(end.is_ok(), "server kept a garbage connection open");

    // and the server still serves good clients
    assert!(connect(&opts(&h, None), caps()).await.is_ok());
}

#[tokio::test]
async fn tls_with_pinned_fingerprint() {
    let ss = net::self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let mut cfg = NetCfg::new("127.0.0.1:0".parse().unwrap());
    cfg.tls = Some(ss.config);
    cfg.token = Some("t".into());
    let h = net::serve(cfg, Arc::new(Fake)).await.unwrap();
    let url = format!("wss://127.0.0.1:{}", h.addr.port());

    // pinned: works
    let ok = RemoteOpts {
        url: url.clone(),
        token: Some("t".into()),
        fingerprint: Some(ss.fingerprint.clone()),
    };
    let mut c = connect(&ok, caps()).await.unwrap();
    assert!(matches!(next(&mut c).await, ServerMsg::Hello(_)));

    // wrong pin: refused
    let bad = RemoteOpts {
        fingerprint: Some(format!("sha256:{}", "00".repeat(32))),
        ..ok.clone()
    };
    let e = connect(&bad, caps())
        .await
        .err()
        .expect("wrong pin accepted")
        .to_string();
    assert!(
        e.contains("TLS") || e.contains("fingerprint") || e.contains("certificate"),
        "{e}"
    );

    // no pin: a self-signed certificate is not trusted by the CA store
    let none = RemoteOpts {
        fingerprint: None,
        ..ok.clone()
    };
    assert!(
        connect(&none, caps()).await.is_err(),
        "self-signed cert trusted without a pin"
    );

    // plain ws:// to a TLS port fails cleanly
    let plain = RemoteOpts {
        url: format!("ws://127.0.0.1:{}", h.addr.port()),
        ..ok
    };
    assert!(timeout(Duration::from_secs(12), connect(&plain, caps()))
        .await
        .expect("hung")
        .is_err());
}

#[tokio::test]
async fn too_many_sessions_are_refused_politely() {
    let mut cfg = NetCfg::new("127.0.0.1:0".parse().unwrap());
    cfg.max_sessions = 1;
    let h = net::serve(cfg, Arc::new(Fake)).await.unwrap();
    let _first = connect(&opts(&h, None), caps()).await.unwrap();
    let e = connect(&opts(&h, None), caps())
        .await
        .err()
        .expect("second session accepted")
        .to_string();
    assert!(e.contains("full"), "{e}");
}

/// A hard-killed `glyph serve` must not leave a headless Chromium running (needs Chromium).
#[cfg(unix)]
#[test]
fn killing_glyph_with_sigkill_does_not_orphan_chromium() {
    if glyph_server::browser::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_glyph");
    let mut child = std::process::Command::new(bin)
        .args(["serve", "--bind", "127.0.0.1:0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mine = |parent: u32| {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("glyph-profile-") && l.contains("--remote-debugging-port"))
            .filter(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u32>().ok())
                    == Some(parent)
            })
            .count()
    };
    // wait for Chromium to appear as our child
    let up = std::time::Instant::now();
    while mine(child.id()) == 0 {
        assert!(
            up.elapsed() < Duration::from_secs(20),
            "chromium never started"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let chrome_parent = child.id();
    child.kill().unwrap(); // SIGKILL: no destructors run
    child.wait().unwrap();
    // the watchdog polls every 2 s, then TERMs; give it a few rounds
    let find_orphans = || {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| {
                l.contains("glyph-profile-")
                    && l.contains("--remote-debugging-port")
                    && !l.contains("sh -c")
            })
            .filter(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u32>().ok())
                    == Some(1)
            })
            .count()
    };
    let t = std::time::Instant::now();
    while find_orphans() > 0 && t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = chrome_parent;
    assert_eq!(
        find_orphans(),
        0,
        "an orphaned Chromium survived the SIGKILL"
    );
}
