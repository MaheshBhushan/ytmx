//! Sign-in through the omp browser relay: read the cookies of a signed-in
//! music.youtube.com tab over the relay's Chrome DevTools Protocol socket.
//! Works wherever the browser's cookie files can't be read directly
//! (Windows app-bound encryption, Flatpak/Snap browsers, locked keyrings).

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_URL: &str = "ws://127.0.0.1:9224/cdp";

pub fn url() -> String {
    std::env::var("YTMX_RELAY").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

/// Is a relay listening? (Cheap TCP check, used to pick the default login method.)
pub async fn available(url: &str) -> bool {
    let Some(hostport) = url.strip_prefix("ws://").and_then(|r| r.split('/').next()) else { return false };
    tokio::time::timeout(Duration::from_millis(400), tokio::net::TcpStream::connect(hostport)).await.is_ok_and(|r| r.is_ok())
}

struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next: u64,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws.send(Message::Text(msg.to_string().into())).await?;
        let wait = async {
            while let Some(m) = self.ws.next().await {
                let Message::Text(t) = m? else { continue };
                let v: Value = serde_json::from_str(&t)?;
                if v["id"].as_u64() == Some(id) {
                    if let Some(e) = v.get("error") {
                        bail!("{method}: {}", e["message"].as_str().unwrap_or("relay error"));
                    }
                    return Ok(v["result"].clone());
                }
            }
            bail!("the relay closed the connection")
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.with_context(|| format!("{method}: the relay did not answer"))?
    }
}

/// Cookie header for youtube.com from the browser behind the relay, plus the tab it came from.
pub async fn cookies(url: &str) -> Result<(String, String)> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.with_context(|| {
        format!("no omp browser relay at {url}. Start it with `omp browser-relay` (first time: `omp browser-relay install`, then load the extension in Chrome)")
    })?;
    let mut cdp = Cdp { ws, next: 0 };
    let targets = cdp.call("Target.getTargets", json!({}), None).await?;
    let tabs = targets["targetInfos"].as_array().cloned().unwrap_or_default();
    let tab = tabs
        .iter()
        .find(|t| t["url"].as_str().is_some_and(|u| u.starts_with("https://music.youtube.com")))
        .or_else(|| tabs.iter().find(|t| t["url"].as_str().is_some_and(|u| u.contains("youtube.com"))))
        .ok_or_else(|| anyhow!("the relay sees no YouTube tab: open https://music.youtube.com in Chrome, signed in, and retry (is the extension badge \"on\"?)"))?;
    let target = tab["targetId"].as_str().unwrap_or_default().to_string();
    let label = format!("{} ({})", tab["title"].as_str().unwrap_or("tab"), tab["url"].as_str().unwrap_or(""));
    let attached = cdp.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None).await?;
    let session = attached["sessionId"].as_str().ok_or_else(|| anyhow!("could not attach to the YouTube tab"))?.to_string();
    let res = cdp.call("Network.getCookies", json!({ "urls": ["https://music.youtube.com", "https://www.youtube.com"] }), Some(&session)).await;
    let _ = cdp.call("Target.detachFromTarget", json!({ "sessionId": session }), None).await;
    let _ = cdp.ws.close(None).await;
    let res = res?;
    let mut seen = std::collections::HashSet::new();
    let header = res["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["domain"].as_str().is_some_and(|d| d.trim_start_matches('.').ends_with("youtube.com")))
        .filter_map(|c| {
            let name = c["name"].as_str()?;
            seen.insert(name.to_string()).then(|| format!("{name}={}", c["value"].as_str().unwrap_or("")))
        })
        .collect::<Vec<_>>()
        .join("; ");
    Ok((header, label))
}
