//! InnerTube (`/youtubei/v1/*`) client, shaped after the WEB_REMIX requests
//! captured from a logged-in music.youtube.com tab.

use crate::auth::{ORIGIN, Session};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FALLBACK_CLIENT_VERSION: &str = "1.20261027.10.00";
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    pub session: Option<Session>,
    client_version: String,
    visitor: Option<String>,
    hl: String,
    gl: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ClientCache {
    at: u64,
    version: String,
    visitor: Option<String>,
    gl: Option<String>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Pull `"KEY":"value"` out of the ytcfg blob in the page HTML.
fn cfg_value(html: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = html.find(&pat)? + pat.len();
    let end = html[start..].find('"')?;
    Some(html[start..start + end].to_string())
}

impl Api {
    pub async fn new() -> Result<Self> {
        let session = Session::load()?;
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .gzip(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .timeout(Duration::from_secs(20))
            .build()?;
        let mut api = Api {
            http,
            session,
            client_version: FALLBACK_CLIENT_VERSION.into(),
            visitor: None,
            hl: std::env::var("YTMX_HL").unwrap_or_else(|_| "en".into()),
            gl: std::env::var("YTMX_GL").ok(),
        };
        api.load_client_config().await;
        Ok(api)
    }

    /// The web client version and visitor id rotate; refresh them from the
    /// homepage's ytcfg at most once a day.
    async fn load_client_config(&mut self) {
        let path = crate::paths::cache_dir().join("client.json");
        if let Ok(c) = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<ClientCache>(&b)?)) {
            self.client_version = c.version.clone();
            self.visitor = c.visitor.clone();
            if self.gl.is_none() {
                self.gl = c.gl.clone();
            }
            if now().saturating_sub(c.at) < 86_400 {
                return;
            }
        }
        let mut req = self.http.get(ORIGIN).header("accept-language", "en-US,en;q=0.9");
        if let Some(s) = &self.session {
            req = req.header("cookie", &s.cookie);
        }
        let Ok(resp) = req.send().await else { return };
        let Ok(html) = resp.text().await else { return };
        if let Some(v) = cfg_value(&html, "INNERTUBE_CLIENT_VERSION") {
            self.client_version = v;
            self.visitor = cfg_value(&html, "VISITOR_DATA");
            if self.gl.is_none() {
                self.gl = cfg_value(&html, "GL");
            }
            let cache = ClientCache { at: now(), version: self.client_version.clone(), visitor: self.visitor.clone(), gl: self.gl.clone() };
            let _ = std::fs::write(&path, serde_json::to_vec(&cache).unwrap_or_default());
        }
    }

    pub fn signed_in(&self) -> bool {
        self.session.is_some()
    }

    fn context(&self) -> Value {
        let mut client = json!({
            "clientName": "WEB_REMIX",
            "clientVersion": self.client_version,
            "hl": self.hl,
            "platform": "DESKTOP",
            "userAgent": USER_AGENT,
        });
        if let Some(gl) = &self.gl {
            client["gl"] = json!(gl);
        }
        if let Some(v) = &self.visitor {
            client["visitorData"] = json!(v);
        }
        json!({ "client": client, "user": {}, "request": { "useSsl": true } })
    }

    fn headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req = req
            .header("origin", ORIGIN)
            .header("x-origin", ORIGIN)
            .header("referer", format!("{ORIGIN}/"))
            .header("x-youtube-client-name", "67")
            .header("x-youtube-client-version", &self.client_version);
        if let Some(v) = &self.visitor {
            req = req.header("x-goog-visitor-id", v);
        }
        if let Some(s) = &self.session {
            req = req
                .header("cookie", &s.cookie)
                .header("authorization", s.authorization())
                .header("x-goog-authuser", s.authuser.to_string())
                .header("x-youtube-bootstrap-logged-in", "true");
        }
        req
    }

    /// POST /youtubei/v1/<endpoint> with `body` merged over the client context.
    pub async fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        let mut payload = json!({ "context": self.context() });
        if let Value::Object(m) = body {
            for (k, v) in m {
                payload[k] = v;
            }
        }
        let url = format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false");
        let resp = self.headers(self.http.post(&url)).json(&payload).send().await.with_context(|| format!("POST {endpoint}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.with_context(|| format!("decoding {endpoint} response"))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            if status.as_u16() == 401 || status.as_u16() == 403 {
                bail!("{endpoint}: {msg} (HTTP {status}); your session may have expired, run `ytmx login`");
            }
            bail!("{endpoint}: {msg} (HTTP {status})");
        }
        Ok(v)
    }

    pub async fn browse(&self, browse_id: &str, params: Option<&str>) -> Result<Value> {
        let mut b = json!({ "browseId": browse_id });
        if let Some(p) = params {
            b["params"] = json!(p);
        }
        self.call("browse", b).await
    }

    /// A browse page as the Android YouTube Music app (ANDROID_MUSIC) sees it.
    /// Used for lyrics only: that client gets per-line timestamps
    /// (`timedLyricsModel`) where the web client gets plain text. Sent without
    /// the session, since lyrics are public and some accounts get HTTP 400 for
    /// this client with cookies attached.
    pub async fn browse_android(&self, browse_id: &str) -> Result<Value> {
        const VERSION: &str = "7.21.50";
        let mut client = json!({ "clientName": "ANDROID_MUSIC", "clientVersion": VERSION, "hl": self.hl });
        if let Some(gl) = &self.gl {
            client["gl"] = json!(gl);
        }
        let payload = json!({ "context": { "client": client }, "browseId": browse_id });
        let resp = self
            .http
            .post(format!("{ORIGIN}/youtubei/v1/browse?prettyPrint=false"))
            .header("x-youtube-client-name", "21")
            .header("x-youtube-client-version", VERSION)
            .json(&payload)
            .send()
            .await
            .context("POST browse (lyrics)")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("decoding lyrics response")?;
        if v.get("error").is_some() {
            bail!("lyrics: HTTP {status}");
        }
        Ok(v)
    }

    pub async fn continuation(&self, endpoint: &str, token: &str) -> Result<Value> {
        self.call(endpoint, json!({ "continuation": token })).await
    }

    pub async fn search(&self, query: &str, params: Option<&str>) -> Result<Value> {
        let mut b = json!({ "query": query });
        if let Some(p) = params {
            b["params"] = json!(p);
        }
        self.call("search", b).await
    }

    pub async fn suggestions(&self, input: &str) -> Result<Value> {
        self.call("music/get_search_suggestions", json!({ "input": input })).await
    }

    /// `/next` builds the "Up next" panel (queue, radio continuation, lyrics tab).
    pub async fn next(&self, body: Value) -> Result<Value> {
        let mut b = json!({ "enablePersistentPlaylistPanel": true, "isAudioOnly": true, "tunerSettingValue": "AUTOMIX_SETTING_NORMAL" });
        if let Value::Object(m) = body {
            for (k, v) in m {
                b[k] = v;
            }
        }
        self.call("next", b).await
    }

    pub async fn get_queue(&self, video_ids: Option<&[String]>, playlist_id: Option<&str>) -> Result<Value> {
        let mut b = json!({});
        if let Some(ids) = video_ids {
            b["videoIds"] = json!(ids);
        }
        if let Some(p) = playlist_id {
            b["playlistId"] = json!(p);
        }
        self.call("music/get_queue", b).await
    }

    pub async fn player(&self, video_id: &str) -> Result<Value> {
        self.call(
            "player",
            json!({
                "videoId": video_id,
                "playbackContext": { "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } },
            }),
        )
        .await
    }

    /// Register a play in the account's history, the way the web player does:
    /// GET the `videostatsPlaybackUrl` from /player with ver=2 and a client playback nonce.
    pub async fn report_playback(&self, video_id: &str) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        let p = self.player(video_id).await?;
        let base = p
            .pointer("/playbackTracking/videostatsPlaybackUrl/baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no playback tracking url for {video_id}"))?;
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let cpn: String = (0..16).map(|_| ALPHA[rand::random_range(0..ALPHA.len())] as char).collect();
        let url = format!("{base}&ver=2&c=WEB_REMIX&cpn={cpn}");
        let resp = self.headers(self.http.get(url)).send().await?;
        if !resp.status().is_success() {
            bail!("playback report: HTTP {}", resp.status());
        }
        Ok(())
    }
}
