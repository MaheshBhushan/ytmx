//! mpv over its JSON IPC socket.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot};

type Conn = interprocess::local_socket::tokio::Stream;

pub struct Mpv {
    writer: Mutex<WriteHalf<Conn>>,
    pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    _child: tokio::process::Child,
}

impl Mpv {
    /// Start an idle, video-less mpv and connect to it (a Unix socket, or a
    /// named pipe on Windows). Events go to `events`.
    pub async fn spawn(volume: f64, events: mpsc::UnboundedSender<Value>) -> Result<Arc<Mpv>> {
        crate::paths::remove_socket("mpv");
        let mut cmd = tokio::process::Command::new("mpv");
        let child = crate::paths::no_window(&mut cmd)
            .args([
                "--idle=yes",
                "--no-video",
                "--no-terminal",
                "--audio-display=no",
                "--ytdl=no",
                "--cache=yes",
                "--demuxer-max-bytes=64MiB",
                "--prefetch-playlist=yes",
                "--gapless-audio=weak",
                "--keep-open=no",
                "--msg-level=all=warn",
                "--force-media-title=ytmx",
            ])
            .arg(format!("--volume={volume}"))
            .arg(format!("--input-ipc-server={}", crate::paths::mpv_ipc_arg()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting mpv (is it installed and on PATH?)")?;
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = crate::ipc::connect_local("mpv").await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        let stream = stream.ok_or_else(|| anyhow!("mpv did not open its IPC socket"))?;
        let (r, w) = tokio::io::split(stream);
        let pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v.get("request_id").and_then(Value::as_u64) {
                    if let Some(tx) = p2.lock().unwrap().remove(&id) {
                        let _ = tx.send(v);
                    }
                } else if v.get("event").is_some() {
                    let _ = events.send(v);
                }
            }
            let _ = events.send(json!({ "event": "mpv-exited" }));
        });
        Ok(Arc::new(Mpv { writer: Mutex::new(w), pending, next_id: AtomicU64::new(1), _child: child }))
    }

    pub async fn cmd(&self, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&json!({ "command": args, "request_id": id }))?;
        line.push(b'\n');
        self.writer.lock().await.write_all(&line).await.context("mpv socket closed")?;
        let v = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await.context("mpv did not answer")?.context("mpv went away")?;
        match v.get("error").and_then(Value::as_str) {
            Some("success") | None => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
            Some(e) => bail!("mpv {}: {e}", args),
        }
    }

    pub async fn get(&self, prop: &str) -> Option<Value> {
        self.cmd(json!(["get_property", prop])).await.ok()
    }

    pub async fn set(&self, prop: &str, val: Value) -> Result<()> {
        self.cmd(json!(["set_property", prop, val])).await.map(|_| ())
    }

    pub async fn observe(&self, id: u64, prop: &str) -> Result<()> {
        self.cmd(json!(["observe_property", id, prop])).await.map(|_| ())
    }

    /// `mode` is "replace" or "append".
    pub async fn load(&self, url: &str, mode: &str, user_agent: Option<&str>) -> Result<()> {
        let mut opts = serde_json::Map::new();
        if let Some(ua) = user_agent {
            opts.insert("user-agent".into(), json!(ua));
        }
        self.cmd(json!({ "name": "loadfile", "url": url, "flags": mode, "options": opts })).await.map(|_| ())
    }
}
