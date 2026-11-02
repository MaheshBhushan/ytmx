//! The player daemon: owns mpv, the queue, prefetching and radio.
//!
//! mpv's playlist is kept at most two entries long: [current, next]. The next
//! track's stream is resolved while the current one plays and appended, so
//! mpv pre-buffers it (`--prefetch-playlist`) and transitions gaplessly; a
//! manual skip to an already-appended track is just `playlist-next`.

use crate::api::Api;
use crate::ipc::{Event, Repeat, Request, Response, State};
use crate::model::{self, Item};
use crate::mpv::Mpv;
use crate::resolve::{Resolver, Stream};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};

type Reply = oneshot::Sender<Response>;

enum Mode {
    Replace,
    Insert { next: bool },
}

enum Internal {
    Resolved { epoch: u64, video_id: String, res: Result<Stream, String> },
    Prefetched { pepoch: u64, video_id: String, res: Result<Stream, String> },
    QueueReady { reply: Option<Reply>, res: Result<Loaded, String>, mode: Mode },
    Extended { res: Result<Loaded, String> },
}

struct Loaded {
    items: Vec<Item>,
    start: usize,
    cont: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Persist {
    volume: f64,
    repeat: Repeat,
    autoplay: bool,
}

impl Default for Persist {
    fn default() -> Self {
        Persist { volume: 70.0, repeat: Repeat::Off, autoplay: true }
    }
}

fn persist_path() -> std::path::PathBuf {
    crate::paths::config_dir().join("player.json")
}

struct Player {
    api: Api,
    res: Arc<Resolver>,
    mpv: Arc<Mpv>,
    int: mpsc::UnboundedSender<Internal>,
    queue: Vec<Item>,
    index: Option<usize>,
    qver: u64,
    paused: bool,
    loading: bool,
    position: f64,
    duration: f64,
    volume: f64,
    muted: bool,
    /// The desktop mixer; when present `volume` is the system output volume.
    mixer: Option<crate::sysvol::Mixer>,
    repeat: Repeat,
    autoplay: bool,
    radio_cont: Option<String>,
    extending: bool,
    advance_after_extend: bool,
    epoch: u64,
    pepoch: u64,
    /// (video id, url) appended to mpv after the current track.
    mpv_next: Option<(String, String)>,
    failures: u32,
    error: Option<String>,
    toast: Option<String>,
    toast_id: u64,
    state_tx: watch::Sender<State>,
    queue_tx: watch::Sender<(u64, Arc<Vec<Item>>)>,
}

/// Body for /next from a watchEndpoint / watchPlaylistEndpoint.
fn next_body(endpoint: &Value) -> Value {
    let (kind, v) = model::endpoint_kind(endpoint).map(|(k, v)| (k.to_string(), v.clone())).unwrap_or_default();
    let mut b = json!({});
    for k in ["videoId", "playlistId", "params", "index", "playlistSetVideoId", "playerParams"] {
        if let Some(x) = v.get(k) {
            b[k] = x.clone();
        }
    }
    if kind == "watchEndpoint" && b.get("playlistId").is_none() {
        // A bare song plays as its radio, like the web client.
        if let Some(id) = v["videoId"].as_str() {
            b["playlistId"] = json!(format!("RDAMVM{id}"));
            b["params"] = json!("wAEB");
        }
    }
    b
}

async fn load_next(api: &Api, body: Value) -> Result<Loaded, String> {
    let wanted = body.get("videoId").and_then(Value::as_str).map(String::from);
    let r = api.next(body).await.map_err(|e| format!("{e:#}"))?;
    let w = model::parse_next(&r);
    let items: Vec<Item> = w.items.into_iter().filter(|i| i.video_id.is_some()).collect();
    if items.is_empty() {
        return Err("nothing playable there".into());
    }
    let start = items.iter().position(|i| i.selected).or_else(|| wanted.and_then(|w| items.iter().position(|i| i.video_id.as_deref() == Some(&w)))).unwrap_or(0);
    Ok(Loaded { items, start, cont: w.continuation })
}

impl Player {
    fn publish(&self) {
        let cur = self.index.and_then(|i| self.queue.get(i)).cloned();
        self.state_tx.send_replace(State {
            current: cur,
            index: self.index,
            queue_len: self.queue.len(),
            queue_version: self.qver,
            paused: self.paused,
            loading: self.loading,
            position: self.position,
            duration: self.duration,
            volume: self.volume,
            muted: self.muted,
            system_volume: self.mixer.is_some(),
            repeat: self.repeat,
            autoplay: self.autoplay,
            error: self.error.clone(),
            toast_id: self.toast_id,
            toast: self.toast.clone(),
        });
    }

    fn publish_queue(&mut self) {
        self.qver += 1;
        self.queue_tx.send_replace((self.qver, Arc::new(self.queue.clone())));
        self.publish();
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.toast = Some(msg.into());
        self.toast_id += 1;
        self.publish();
    }

    fn save(&self) {
        // The stored volume is mpv's; with a system mixer keep whatever was stored.
        let volume = match self.mixer {
            Some(_) => std::fs::read(persist_path()).ok().and_then(|b| serde_json::from_slice::<Persist>(&b).ok()).map(|p| p.volume).unwrap_or(70.0),
            None => self.volume,
        };
        let p = Persist { volume, repeat: self.repeat, autoplay: self.autoplay };
        let _ = std::fs::write(persist_path(), serde_json::to_vec(&p).unwrap_or_default());
    }

    fn vid(&self, i: usize) -> Option<String> {
        self.queue.get(i).and_then(|t| t.video_id.clone())
    }

    fn next_index(&self) -> Option<usize> {
        let i = self.index?;
        if i + 1 < self.queue.len() {
            Some(i + 1)
        } else if self.repeat == Repeat::All && !self.queue.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    fn start(&mut self, i: usize) {
        let Some(vid) = self.vid(i) else { return };
        self.index = Some(i);
        self.epoch += 1;
        self.pepoch += 1;
        self.mpv_next = None;
        self.loading = true;
        self.error = None;
        self.position = 0.0;
        self.duration = 0.0;
        let (res, tx, epoch) = (self.res.clone(), self.int.clone(), self.epoch);
        tokio::spawn(async move {
            let r = res.resolve(&vid).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::Resolved { epoch, video_id: vid, res: r });
        });
        self.publish();
    }

    async fn on_resolved(&mut self, epoch: u64, vid: String, res: Result<Stream, String>) {
        if epoch != self.epoch {
            return;
        }
        match res {
            Ok(s) => {
                if let Err(e) = self.mpv.load(&s.url, "replace", s.user_agent.as_deref()).await {
                    self.error = Some(format!("{e:#}"));
                }
                let _ = self.mpv.set("pause", json!(false)).await;
                self.loading = false;
                self.failures = 0;
                self.report(vid);
                self.prefetch();
                self.maybe_extend();
                self.publish();
            }
            Err(e) => {
                self.loading = false;
                self.failures += 1;
                let title = self.index.and_then(|i| self.queue.get(i)).map(|t| t.title.clone()).unwrap_or_default();
                self.error = Some(format!("{title}: {e}"));
                self.say(format!("Can't play {title}; skipping"));
                if self.failures < 5
                    && let Some(n) = self.next_index() {
                        self.start(n);
                    }
            }
        }
    }

    fn report(&self, vid: String) {
        let api = self.api.clone();
        tokio::spawn(async move {
            if let Err(e) = api.report_playback(&vid).await {
                eprintln!("history report for {vid}: {e:#}");
            }
        });
    }

    fn prefetch(&mut self) {
        let Some(n) = self.next_index() else { return };
        let Some(vid) = self.vid(n) else { return };
        if self.mpv_next.as_ref().is_some_and(|(v, _)| *v == vid) {
            return;
        }
        let (res, tx, pepoch) = (self.res.clone(), self.int.clone(), self.pepoch);
        // Warm the one after too, so a double skip is also instant.
        let after = self.vid(n + 1);
        tokio::spawn(async move {
            let r = res.resolve(&vid).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::Prefetched { pepoch, video_id: vid, res: r });
            if let Some(a) = after {
                let _ = res.resolve(&a).await;
            }
        });
    }

    async fn on_prefetched(&mut self, pepoch: u64, vid: String, res: Result<Stream, String>) {
        if pepoch != self.pepoch || self.loading || self.mpv_next.is_some() {
            return;
        }
        let Some(n) = self.next_index() else { return };
        if self.vid(n).as_deref() != Some(&vid) || Some(n) == self.index {
            return;
        }
        if let Ok(s) = res
            && self.mpv.load(&s.url, "append", s.user_agent.as_deref()).await.is_ok() {
                self.mpv_next = Some((vid, s.url));
            }
    }

    /// The upcoming track changed: drop what mpv has queued and prefetch again.
    async fn invalidate_next(&mut self) {
        self.pepoch += 1;
        if self.mpv_next.take().is_some() {
            let _ = self.mpv.cmd(json!(["playlist-clear"])).await;
        }
        self.prefetch();
    }

    fn maybe_extend(&mut self) {
        let Some(i) = self.index else { return };
        if self.extending || i + 3 < self.queue.len() || self.repeat != Repeat::Off {
            return;
        }
        let body = if let Some(tok) = self.radio_cont.clone() {
            json!({ "continuation": tok })
        } else if self.autoplay {
            let Some(last) = self.queue.last().and_then(|t| t.video_id.clone()) else { return };
            json!({ "videoId": last, "playlistId": format!("RDAMVM{last}"), "params": "wAEB" })
        } else {
            return;
        };
        self.extending = true;
        let (api, tx) = (self.api.clone(), self.int.clone());
        tokio::spawn(async move {
            let res = async {
                let r = api.next(body).await.map_err(|e| format!("{e:#}"))?;
                let w = model::parse_next(&r);
                Ok(Loaded { items: w.items, start: 0, cont: w.continuation })
            }
            .await;
            let _ = tx.send(Internal::Extended { res });
        });
    }

    async fn on_extended(&mut self, res: Result<Loaded, String>) {
        self.extending = false;
        match res {
            Ok(l) => {
                let have: std::collections::HashSet<String> = self.queue.iter().filter_map(|t| t.video_id.clone()).collect();
                let fresh: Vec<Item> = l.items.into_iter().filter(|t| t.video_id.as_ref().is_some_and(|v| !have.contains(v))).collect();
                // A spent continuation with nothing new ends that radio; autoplay takes over.
                self.radio_cont = if fresh.is_empty() { None } else { l.cont };
                let added = !fresh.is_empty();
                self.queue.extend(fresh.into_iter().map(|mut t| {
                    t.selected = false;
                    t
                }));
                self.publish_queue();
                if self.advance_after_extend && added {
                    self.advance_after_extend = false;
                    if let Some(n) = self.next_index() {
                        self.start(n);
                    }
                } else {
                    self.prefetch();
                }
            }
            Err(e) => {
                self.advance_after_extend = false;
                eprintln!("radio: {e}");
            }
        }
    }

    async fn advance(&mut self, manual: bool) {
        if let Some(n) = self.next_index() {
            if self.mpv_next.as_ref().is_some_and(|(v, _)| self.vid(n).as_deref() == Some(v.as_str())) && !self.loading {
                // Already buffered in mpv: instant.
                let _ = self.mpv.cmd(json!(["playlist-next", "force"])).await;
                return;
            }
            self.start(n);
        } else if self.autoplay || self.radio_cont.is_some() {
            self.advance_after_extend = true;
            self.maybe_extend();
            if !self.extending {
                self.advance_after_extend = false;
            }
        } else if manual {
            self.say("End of the queue");
        }
    }

    /// mpv moved to the appended track on its own (or via playlist-next).
    async fn on_path(&mut self, path: &str) {
        let Some((vid, url)) = self.mpv_next.clone() else { return };
        if url != path {
            return;
        }
        if let Some(n) = self.next_index()
            && self.vid(n).as_deref() == Some(vid.as_str()) {
                self.index = Some(n);
            }
        self.mpv_next = None;
        self.epoch += 1;
        self.pepoch += 1;
        self.position = 0.0;
        let _ = self.mpv.cmd(json!(["playlist-remove", 0])).await;
        self.report(vid);
        self.prefetch();
        self.maybe_extend();
        self.publish();
    }

    async fn on_mpv(&mut self, ev: Value) {
        match ev["event"].as_str() {
            Some("property-change") => {
                let d = &ev["data"];
                match ev["name"].as_str() {
                    Some("path") => {
                        if let Some(p) = d.as_str() {
                            self.on_path(p).await;
                        }
                    }
                    Some("pause") => self.paused = d.as_bool().unwrap_or(false),
                    // With a system mixer mpv stays at 100; its volume isn't the one shown.
                    Some("volume") if self.mixer.is_none() => self.volume = d.as_f64().unwrap_or(self.volume),
                    Some("duration") => self.duration = d.as_f64().unwrap_or(0.0),
                    _ => return,
                }
                self.publish();
            }
            Some("end-file") => match ev["reason"].as_str() {
                Some("eof") if self.mpv_next.is_none() && !self.loading => self.advance(false).await,
                Some("error") if !self.loading => {
                    // Usually an expired URL; re-resolve once.
                    if let Some(i) = self.index {
                        if let Some(v) = self.vid(i) {
                            self.res.forget(&v);
                        }
                        self.failures += 1;
                        if self.failures < 3 {
                            self.start(i);
                        } else {
                            self.say("Playback failed");
                        }
                    }
                }
                _ => {}
            },
            Some("mpv-exited") => {
                eprintln!("mpv exited; stopping daemon");
                let _ = std::fs::remove_file(crate::paths::socket());
                std::process::exit(1);
            }
            _ => {}
        }
    }

    async fn on_queue_ready(&mut self, reply: Option<Reply>, res: Result<Loaded, String>, mode: Mode) {
        let resp = match res {
            Err(e) => Response { ok: false, error: Some(e), ..Default::default() },
            Ok(l) => {
                let n = l.items.len();
                let first = l.items.get(l.start).map(|t| t.title.clone()).unwrap_or_default();
                match mode {
                    Mode::Replace => {
                        self.queue = l.items;
                        self.radio_cont = l.cont;
                        self.publish_queue();
                        self.start(l.start);
                        Response { ok: true, message: Some(format!("Playing {first}")), ..Default::default() }
                    }
                    Mode::Insert { next } => {
                        let was_empty = self.index.is_none();
                        let at = match (self.index, next) {
                            (Some(i), true) => i + 1,
                            _ => self.queue.len(),
                        };
                        let at = at.min(self.queue.len());
                        self.queue.splice(at..at, l.items.into_iter().map(|mut t| {
                            t.selected = false;
                            t
                        }));
                        self.publish_queue();
                        if was_empty {
                            self.start(at);
                        } else {
                            self.invalidate_next().await;
                        }
                        let what = if n == 1 { first } else { format!("{n} tracks") };
                        Response { ok: true, message: Some(if next { format!("{what} will play next") } else { format!("Added {what} to the queue") }), ..Default::default() }
                    }
                }
            }
        };
        if let Some(r) = reply {
            if let Some(m) = &resp.message {
                self.toast = Some(m.clone());
                self.toast_id += 1;
            } else if let Some(e) = &resp.error {
                self.toast = Some(e.clone());
                self.toast_id += 1;
            }
            self.publish();
            let _ = r.send(resp);
        }
    }

    fn state(&self) -> State {
        self.state_tx.borrow().clone()
    }

    fn ok(&self) -> Response {
        Response { ok: true, state: Some(self.state()), ..Default::default() }
    }

    /// Handle a request; returns None when the reply comes later.
    async fn handle(&mut self, req: Request, reply: Reply) -> Option<Reply> {
        let resp = match req {
            Request::Play { endpoint } => {
                let (api, tx) = (self.api.clone(), self.int.clone());
                tokio::spawn(async move {
                    let res = load_next(&api, next_body(&endpoint)).await;
                    let _ = tx.send(Internal::QueueReady { reply: Some(reply), res, mode: Mode::Replace });
                });
                return None;
            }
            Request::Enqueue { endpoint, next } => {
                let (api, tx) = (self.api.clone(), self.int.clone());
                tokio::spawn(async move {
                    let t = endpoint.pointer("/queueAddEndpoint/queueTarget").cloned().unwrap_or_default();
                    let res = async {
                        let r = if let Some(v) = t["videoId"].as_str() {
                            api.get_queue(Some(&[v.to_string()]), None).await
                        } else {
                            api.get_queue(None, t["playlistId"].as_str()).await
                        }
                        .map_err(|e| format!("{e:#}"))?;
                        let items: Vec<Item> = model::items_in(&r).into_iter().filter(|i| i.video_id.is_some()).collect();
                        if items.is_empty() {
                            return Err("nothing to add".to_string());
                        }
                        Ok(Loaded { items, start: 0, cont: None })
                    }
                    .await;
                    let _ = tx.send(Internal::QueueReady { reply: Some(reply), res, mode: Mode::Insert { next } });
                });
                return None;
            }
            Request::Add { items, next } => {
                let items: Vec<Item> = items.into_iter().filter(|i| i.video_id.is_some()).collect();
                self.on_queue_ready(Some(reply), Ok(Loaded { items, start: 0, cont: None }), Mode::Insert { next }).await;
                return None;
            }
            Request::Toggle => {
                if self.index.is_none() && !self.queue.is_empty() {
                    self.start(0);
                } else {
                    let _ = self.mpv.cmd(json!(["cycle", "pause"])).await;
                }
                self.ok()
            }
            Request::Pause | Request::Resume => {
                let _ = self.mpv.set("pause", json!(matches!(req, Request::Pause))).await;
                self.ok()
            }
            Request::Next => {
                self.advance(true).await;
                self.ok()
            }
            Request::Prev => {
                match self.index {
                    Some(i) if self.position < 3.0 && i > 0 => self.start(i - 1),
                    _ => {
                        let _ = self.mpv.cmd(json!(["seek", 0, "absolute"])).await;
                    }
                }
                self.ok()
            }
            Request::Seek { seconds, relative } => {
                let mode = if relative { "relative" } else { "absolute" };
                match self.mpv.cmd(json!(["seek", seconds, mode, "exact"])).await {
                    Ok(_) => {
                        if let Some(p) = self.mpv.get("time-pos").await.and_then(|v| v.as_f64()) {
                            self.position = p;
                        }
                        self.publish();
                        self.ok()
                    }
                    Err(_) => Response { ok: false, error: Some("nothing is playing".into()), ..Default::default() },
                }
            }
            Request::Volume { value, relative } => {
                match self.mixer.clone() {
                    Some(m) => {
                        let v = if relative { self.volume + value } else { value };
                        if let Some(l) = m.set(v).await {
                            self.volume = l.volume;
                            self.muted = l.muted;
                        }
                    }
                    None => {
                        let v = if relative { self.volume + value } else { value }.clamp(0.0, 130.0);
                        let _ = self.mpv.set("volume", json!(v)).await;
                        self.volume = v;
                        self.save();
                    }
                }
                self.publish();
                self.ok()
            }
            Request::Mute => {
                match self.mixer.clone() {
                    Some(m) => {
                        if let Some(l) = m.toggle_mute().await {
                            self.volume = l.volume;
                            self.muted = l.muted;
                        }
                    }
                    None => {
                        self.muted = !self.muted;
                        let _ = self.mpv.set("mute", json!(self.muted)).await;
                    }
                }
                self.publish();
                self.ok()
            }
            Request::Jump { index } => {
                if index < self.queue.len() {
                    self.start(index);
                    self.ok()
                } else {
                    Response { ok: false, error: Some(format!("no queue entry {}", index + 1)), ..Default::default() }
                }
            }
            Request::Remove { index } => self.remove(index).await,
            Request::RemoveVideo { video_id } => match self.queue.iter().position(|t| t.video_id.as_deref() == Some(video_id.as_str())) {
                Some(i) => self.remove(i).await,
                None => Response { ok: false, error: Some("not in the queue".into()), ..Default::default() },
            },
            Request::Move { from, to } => {
                if from < self.queue.len() && to < self.queue.len() && from != to {
                    let t = self.queue.remove(from);
                    self.queue.insert(to, t);
                    if let Some(i) = self.index {
                        self.index = Some(if i == from {
                            to
                        } else if from < i && to >= i {
                            i - 1
                        } else if from > i && to <= i {
                            i + 1
                        } else {
                            i
                        });
                    }
                    self.publish_queue();
                    self.invalidate_next().await;
                }
                self.ok()
            }
            Request::Clear => {
                match self.index {
                    Some(i) => {
                        let cur = self.queue.remove(i);
                        self.queue = vec![cur];
                        self.index = Some(0);
                    }
                    None => self.queue.clear(),
                }
                self.radio_cont = None;
                self.publish_queue();
                self.invalidate_next().await;
                self.ok()
            }
            Request::Shuffle => {
                use rand::seq::SliceRandom;
                let from = self.index.map(|i| i + 1).unwrap_or(0);
                if from < self.queue.len() {
                    self.queue[from..].shuffle(&mut rand::rng());
                }
                self.publish_queue();
                self.invalidate_next().await;
                self.say("Shuffled the queue");
                self.ok()
            }
            Request::Repeat { mode } => {
                self.repeat = mode.unwrap_or(match self.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                });
                let _ = self.mpv.set("loop-file", json!(if self.repeat == Repeat::One { "inf" } else { "no" })).await;
                self.save();
                self.invalidate_next().await;
                self.publish();
                self.ok()
            }
            Request::Autoplay { on } => {
                self.autoplay = on.unwrap_or(!self.autoplay);
                self.save();
                self.publish();
                self.ok()
            }
            Request::Radio => {
                let Some(vid) = self.index.and_then(|i| self.vid(i)) else {
                    let _ = reply.send(Response { ok: false, error: Some("nothing is playing".into()), ..Default::default() });
                    return None;
                };
                let (api, tx) = (self.api.clone(), self.int.clone());
                tokio::spawn(async move {
                    let res = load_next(&api, json!({ "videoId": vid, "playlistId": format!("RDAMVM{vid}"), "params": "wAEB" })).await;
                    let _ = tx.send(Internal::QueueReady { reply: Some(reply), res, mode: Mode::Replace });
                });
                return None;
            }
            Request::SetLike { video_id, status } => {
                for t in self.queue.iter_mut().filter(|t| t.video_id.as_deref() == Some(video_id.as_str())) {
                    model::set_like(t, &status);
                }
                self.publish_queue();
                self.ok()
            }
            Request::Stop => {
                let _ = self.mpv.cmd(json!(["stop"])).await;
                self.epoch += 1;
                self.pepoch += 1;
                self.mpv_next = None;
                self.index = None;
                self.loading = false;
                self.position = 0.0;
                self.publish();
                self.ok()
            }
            Request::State => self.ok(),
            Request::Queue => Response { ok: true, state: Some(self.state()), queue: Some(self.queue.clone()), ..Default::default() },
            Request::Watch => self.ok(),
            Request::Quit => {
                let _ = reply.send(Response { ok: true, message: Some("Stopped".into()), ..Default::default() });
                let _ = self.mpv.cmd(json!(["quit"])).await;
                let _ = std::fs::remove_file(crate::paths::socket());
                std::process::exit(0);
            }
        };
        let _ = reply.send(resp);
        None
    }

    async fn remove(&mut self, index: usize) -> Response {
        if index >= self.queue.len() {
            return Response { ok: false, error: Some(format!("no queue entry {}", index + 1)), ..Default::default() };
        }
        self.queue.remove(index);
        match self.index {
            Some(i) if i == index => {
                if index < self.queue.len() {
                    self.publish_queue();
                    self.start(index);
                } else {
                    let _ = self.mpv.cmd(json!(["stop"])).await;
                    self.index = None;
                    self.mpv_next = None;
                    self.publish_queue();
                }
            }
            Some(i) => {
                if index < i {
                    self.index = Some(i - 1);
                }
                self.publish_queue();
                self.invalidate_next().await;
            }
            None => self.publish_queue(),
        }
        self.ok()
    }
}

async fn serve_conn(stream: UnixStream, req_tx: mpsc::UnboundedSender<(Request, Reply)>, mut state_rx: watch::Receiver<State>, mut queue_rx: watch::Receiver<(u64, Arc<Vec<Item>>)>) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response { ok: false, error: Some(format!("bad request: {e}")), ..Default::default() };
                let _ = w.write_all(format!("{}\n", serde_json::to_string(&resp).unwrap_or_default()).as_bytes()).await;
                continue;
            }
        };
        let watching = matches!(req, Request::Watch);
        let (tx, rx) = oneshot::channel();
        if req_tx.send((req, tx)).is_err() {
            return;
        }
        let Ok(resp) = rx.await else { return };
        if w.write_all(format!("{}\n", serde_json::to_string(&resp).unwrap_or_default()).as_bytes()).await.is_err() {
            return;
        }
        if watching {
            // Stream state and queue changes until the client goes away.
            let send = |ev: Event| format!("{}\n", serde_json::to_string(&ev).unwrap_or_default());
            let q = queue_rx.borrow_and_update().clone();
            let s = state_rx.borrow_and_update().clone();
            if w.write_all(send(Event::Queue { version: q.0, items: (*q.1).clone() }).as_bytes()).await.is_err() || w.write_all(send(Event::State(s)).as_bytes()).await.is_err() {
                return;
            }
            loop {
                tokio::select! {
                    r = queue_rx.changed() => {
                        if r.is_err() { return; }
                        let q = queue_rx.borrow_and_update().clone();
                        if w.write_all(send(Event::Queue { version: q.0, items: (*q.1).clone() }).as_bytes()).await.is_err() { return; }
                    }
                    r = state_rx.changed() => {
                        if r.is_err() { return; }
                        let s = state_rx.borrow_and_update().clone();
                        if w.write_all(send(Event::State(s)).as_bytes()).await.is_err() { return; }
                    }
                    l = lines.next_line() => {
                        if !matches!(l, Ok(Some(_))) { return; }
                    }
                }
            }
        }
    }
}

pub async fn run() -> Result<()> {
    let sock = crate::paths::socket();
    if UnixStream::connect(&sock).await.is_ok() {
        anyhow::bail!("a ytmx daemon is already running at {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    }

    let api = Api::new().await?;
    let res = Resolver::new(api.session.as_ref())?;
    let persist: Persist = std::fs::read(persist_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    // Volume is the desktop's when a mixer answers; mpv then stays at 100 so
    // nothing attenuates twice. Otherwise mpv's own volume, remembered here.
    let mixer = crate::sysvol::Mixer::detect().await;
    let start_level = match &mixer {
        Some(m) => m.get().await,
        None => None,
    };
    let (vol_tx, mut vol_rx) = mpsc::unbounded_channel();
    if let Some(m) = &mixer {
        m.watch(vol_tx);
    }
    let mpv = Mpv::spawn(&crate::paths::mpv_socket(), if mixer.is_some() { 100.0 } else { persist.volume }, ev_tx).await?;
    for (i, p) in ["path", "pause", "volume", "duration"].iter().enumerate() {
        mpv.observe(i as u64 + 1, p).await?;
    }
    if persist.repeat == Repeat::One {
        let _ = mpv.set("loop-file", json!("inf")).await;
    }

    let (int_tx, mut int_rx) = mpsc::unbounded_channel();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<(Request, Reply)>();
    let (state_tx, state_rx) = watch::channel(State::default());
    let (queue_tx, queue_rx) = watch::channel((0u64, Arc::new(Vec::new())));

    let mut p = Player {
        api,
        res,
        mpv,
        int: int_tx,
        queue: Vec::new(),
        index: None,
        qver: 0,
        paused: false,
        loading: false,
        position: 0.0,
        duration: 0.0,
        volume: start_level.map(|l| l.volume).unwrap_or(persist.volume),
        muted: start_level.is_some_and(|l| l.muted),
        mixer,
        repeat: persist.repeat,
        autoplay: persist.autoplay,
        radio_cont: None,
        extending: false,
        advance_after_extend: false,
        epoch: 0,
        pepoch: 0,
        mpv_next: None,
        failures: 0,
        error: None,
        toast: None,
        toast_id: 0,
        state_tx,
        queue_tx,
    };
    p.publish();

    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(serve_conn(s, req_tx.clone(), state_rx.clone(), queue_rx.clone()));
        }
    });

    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            Some((req, reply)) = req_rx.recv() => { p.handle(req, reply).await; }
            Some(ev) = ev_rx.recv() => p.on_mpv(ev).await,
            Some(l) = vol_rx.recv() => {
                p.volume = l.volume;
                p.muted = l.muted;
                p.publish();
            }
            Some(m) = int_rx.recv() => match m {
                Internal::Resolved { epoch, video_id, res } => p.on_resolved(epoch, video_id, res).await,
                Internal::Prefetched { pepoch, video_id, res } => p.on_prefetched(pepoch, video_id, res).await,
                Internal::QueueReady { reply, res, mode } => p.on_queue_ready(reply, res, mode).await,
                Internal::Extended { res } => p.on_extended(res).await,
            },
            _ = tick.tick() => {
                if p.index.is_some() && !p.paused && !p.loading
                    && let Some(pos) = p.mpv.get("time-pos").await.and_then(|v| v.as_f64()) {
                        p.position = pos;
                        p.publish();
                    }
            }
        }
    }
}
