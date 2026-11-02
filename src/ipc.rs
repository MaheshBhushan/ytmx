//! Line-delimited JSON between clients (CLI, TUI) and the player daemon.

use crate::model::Item;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use interprocess::local_socket::tokio::{Stream, prelude::*};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};

pub async fn connect_local(kind: &str) -> std::io::Result<Stream> {
    Stream::connect(crate::paths::ipc_name(kind)?).await
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// watchEndpoint / watchPlaylistEndpoint
    Play { endpoint: Value },
    /// queueAddEndpoint
    Enqueue { endpoint: Value, next: bool },
    /// Explicit items, e.g. a CLI selection.
    Add { items: Vec<Item>, next: bool },
    Toggle,
    Pause,
    Resume,
    Next,
    Prev,
    Seek { seconds: f64, relative: bool },
    /// Toggle mute (the system output's when ytmx drives the system volume).
    Mute,
    Volume { value: f64, relative: bool },
    Jump { index: usize },
    Remove { index: usize },
    RemoveVideo { video_id: String },
    Move { from: usize, to: usize },
    Clear,
    Shuffle,
    Repeat { mode: Option<Repeat> },
    Autoplay { on: Option<bool> },
    Radio,
    /// Refresh an item's like status after a rating change elsewhere.
    SetLike { video_id: String, status: String },
    Stop,
    State,
    Queue,
    Watch,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub current: Option<Item>,
    pub index: Option<usize>,
    pub queue_len: usize,
    pub queue_version: u64,
    pub paused: bool,
    pub loading: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    #[serde(default)]
    pub muted: bool,
    /// `volume` is the desktop's output volume (tray, media keys) rather than mpv's.
    #[serde(default)]
    pub system_volume: bool,
    pub repeat: Repeat,
    pub autoplay: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Monotonic message counter + text, so clients show each toast once.
    #[serde(default)]
    pub toast_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toast: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<Vec<Item>>,
}

/// Events streamed to `Watch` connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    State(State),
    Queue { version: u64, items: Vec<Item> },
}

pub struct Client {
    lines: Lines<BufReader<ReadHalf<Stream>>>,
    w: WriteHalf<Stream>,
}

impl Client {
    pub async fn connect() -> Result<Client> {
        if let Ok(s) = connect_local("player").await {
            return Ok(Self::from(s));
        }
        // Start the daemon detached from this terminal.
        let exe = std::env::current_exe()?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::paths::cache_dir().join("daemon.log"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Survive the terminal closing, and never open a console window.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        cmd.spawn().context("starting the ytmx daemon")?;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if let Ok(s) = connect_local("player").await {
                return Ok(Self::from(s));
            }
        }
        bail!("the ytmx daemon did not start; see {}", crate::paths::cache_dir().join("daemon.log").display())
    }

    /// Connect only if a daemon is already running.
    pub async fn existing() -> Option<Client> {
        connect_local("player").await.ok().map(Self::from)
    }

    fn from(s: Stream) -> Client {
        let (r, w) = tokio::io::split(s);
        Client { lines: BufReader::new(r).lines(), w }
    }

    pub async fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        Ok(())
    }

    pub async fn recv_line(&mut self) -> Result<String> {
        self.lines.next_line().await?.ok_or_else(|| {
            anyhow!("the ytmx player stopped unexpectedly; see {}", crate::paths::cache_dir().join("daemon.log").display())
        })
    }

    pub async fn request(&mut self, req: &Request) -> Result<Response> {
        self.send(req).await?;
        let r: Response = serde_json::from_str(&self.recv_line().await?)?;
        if !r.ok {
            bail!("{}", r.error.unwrap_or_else(|| "request failed".into()));
        }
        Ok(r)
    }

    pub async fn next_event(&mut self) -> Result<Event> {
        Ok(serde_json::from_str(&self.recv_line().await?)?)
    }
}
