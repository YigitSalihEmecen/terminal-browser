//! A single tab: navigation and simple evaluation. (Rendering lives in `tab.rs`.)

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cdp::Session;

pub struct Page {
    pub session: Session,
    pub target_id: String,
}

#[derive(Deserialize)]
struct NavResult {
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}

#[derive(Deserialize)]
struct EvalResult {
    result: RemoteObject,
    #[serde(rename = "exceptionDetails")]
    exception: Option<Value>,
}

#[derive(Deserialize)]
struct RemoteObject {
    value: Option<Value>,
}

impl Page {
    /// Navigate and wait for the `load` event (or `timeout`).
    pub async fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        let mut events = self.session.events();
        self.session.send("Page.enable", json!({})).await?;
        let r: NavResult = self
            .session
            .call("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(e) = r.error_text {
            return Err(anyhow!("navigation to {url} failed: {e}"));
        }
        let wait = async {
            while let Some(ev) = events.recv().await {
                if ev.method == "Page.loadEventFired" {
                    return true;
                }
            }
            false
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(true) | Err(_) => Ok(()), // on timeout, dump whatever has rendered so far
            Ok(false) => Err(anyhow!("page closed during navigation")),
        }
    }

    pub async fn eval_string(&self, expr: &str) -> Result<String> {
        let r: EvalResult = self
            .session
            .call(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
            )
            .await?;
        if let Some(ex) = r.exception {
            return Err(anyhow!("script exception: {ex}"));
        }
        match r.result.value {
            Some(Value::String(s)) => Ok(s),
            Some(v) => Ok(v.to_string()),
            None => Ok(String::new()),
        }
    }

    /// Visible text of the page, as the browser lays it out.
    pub async fn text(&self) -> Result<String> {
        self.eval_string("document.body ? document.body.innerText : ''")
            .await
    }
}
