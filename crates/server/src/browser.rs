//! Launching Chromium and creating pages.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use tokio::{process::Child, time::Instant};

use crate::cdp::{Cdp, Session};

/// Flags kept deliberately small: everything here removes a background service or a GPU/UI
/// dependency that a terminal browser never uses.
const BASE_FLAGS: &[&str] = &[
    "--headless=new",
    "--remote-debugging-port=0",
    "--disable-gpu",
    "--disable-extensions",
    "--disable-sync",
    "--disable-default-apps",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-translate",
    "--disable-hang-monitor",
    "--disable-search-engine-choice-screen",
    "--metrics-recording-only",
    "--no-first-run",
    "--no-default-browser-check",
    "--mute-audio",
    "--hide-scrollbars",
    "--force-color-profile=srgb",
    "--password-store=basic",
    "--use-mock-keychain",
    "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,CertificateTransparencyComponentUpdater",
];

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Explicit browser binary; otherwise `$GLYPH_CHROME` then well-known locations.
    pub chrome: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

pub struct Browser {
    cdp: Cdp,
    child: Child,
    pub chrome_path: PathBuf,
    _profile: tempfile::TempDir,
}

pub fn find_chrome(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("GLYPH_CHROME") {
        return Ok(p.into());
    }
    const FIXED: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    if let Some(p) = FIXED.iter().map(Path::new).find(|p| p.exists()) {
        return Ok(p.to_path_buf());
    }
    for name in [
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "chrome-headless-shell",
    ] {
        if let Some(path) = std::env::var_os("PATH") {
            if let Some(p) = std::env::split_paths(&path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
            {
                return Ok(p);
            }
        }
    }
    bail!("no Chrome/Chromium found; install one or set GLYPH_CHROME")
}

impl Browser {
    pub async fn launch(opts: &LaunchOptions) -> Result<Self> {
        let chrome_path = find_chrome(opts.chrome.as_deref())?;
        let profile = tempfile::Builder::new()
            .prefix("glyph-profile-")
            .tempdir()?;
        let mut cmd = tokio::process::Command::new(&chrome_path);
        cmd.args(BASE_FLAGS)
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args(&opts.extra_args)
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", chrome_path.display()))?;

        let ws_url = wait_for_endpoint(profile.path(), Duration::from_secs(20)).await?;
        let cdp = Cdp::connect(&ws_url).await?;
        Ok(Self {
            cdp,
            child,
            chrome_path,
            _profile: profile,
        })
    }

    pub fn cdp(&self) -> &Cdp {
        &self.cdp
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// An isolated cookie/storage jar (one per connected client).
    pub async fn new_context(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "browserContextId")]
            id: String,
        }
        let r: R = self
            .cdp
            .call(None, "Target.createBrowserContext", json!({}))
            .await?;
        Ok(r.id)
    }

    pub async fn dispose_context(&self, id: &str) -> Result<()> {
        self.cdp
            .call_raw(
                None,
                "Target.disposeBrowserContext",
                json!({ "browserContextId": id }),
            )
            .await
            .map(|_| ())
    }

    /// Create a blank target in `context` and attach to it.
    pub async fn new_target(&self, context: Option<&str>) -> Result<(String, Session)> {
        #[derive(Deserialize)]
        struct T {
            #[serde(rename = "targetId")]
            id: String,
        }
        #[derive(Deserialize)]
        struct A {
            #[serde(rename = "sessionId")]
            id: String,
        }
        let mut p = json!({ "url": "about:blank" });
        if let Some(c) = context {
            p["browserContextId"] = json!(c);
        }
        let t: T = self.cdp.call(None, "Target.createTarget", p).await?;
        let a: A = self
            .cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({ "targetId": t.id, "flatten": true }),
            )
            .await?;
        Ok((t.id, self.cdp.session(&a.id)))
    }

    pub async fn close_target(&self, target_id: &str) -> Result<()> {
        self.cdp
            .call_raw(None, "Target.closeTarget", json!({ "targetId": target_id }))
            .await
            .map(|_| ())
    }

    pub async fn shutdown(mut self) {
        let _ = self.cdp.call_raw(None, "Browser.close", json!({})).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

/// Chromium writes `DevToolsActivePort` (port\npath) into the profile dir once listening.
async fn wait_for_endpoint(profile: &Path, timeout: Duration) -> Result<String> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = tokio::fs::read_to_string(&file).await {
            let mut lines = s.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                return Ok(format!("ws://127.0.0.1:{port}{path}"));
            }
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "timed out waiting for Chromium to start (no DevToolsActivePort)"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
