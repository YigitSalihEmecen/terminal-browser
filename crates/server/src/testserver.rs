//! A tiny static HTTP server for fixtures (tests, benches and `record` tooling).

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// Serve files under `root` on an ephemeral loopback port. Returns the bound address.
/// Path traversal is rejected; this is test tooling, not a general server.
pub async fn serve_dir(root: PathBuf) -> Result<SocketAddr> {
    serve_dir_logged(root).await.map(|(a, _)| a)
}

/// Like [`serve_dir`], also returning the log of requested paths. `__PORT__` inside HTML files is
/// replaced with the bound port so fixtures can reference this server by absolute URL.
pub async fn serve_dir_logged(root: PathBuf) -> Result<(SocketAddr, Arc<Mutex<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let root = root.clone();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let rel = path
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("/")
                    .trim_start_matches('/');
                let rel = if rel.is_empty() { "index.html" } else { rel };
                log.lock().unwrap().push(format!("/{rel}"));
                let (status, body, ctype) = if rel.contains("..") {
                    ("400 Bad Request", b"bad path".to_vec(), "text/plain")
                } else {
                    match tokio::fs::read(root.join(rel)).await {
                        Ok(b) if rel.ends_with(".html") => (
                            "200 OK",
                            String::from_utf8_lossy(&b)
                                .replace("__PORT__", &addr.port().to_string())
                                .into_bytes(),
                            mime(rel),
                        ),
                        Ok(b) => ("200 OK", b, mime(rel)),
                        Err(_) => ("404 Not Found", b"not found".to_vec(), "text/plain"),
                    }
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    Ok((addr, log))
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}
