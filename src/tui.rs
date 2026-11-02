//! Interactive TUI. All network work happens in background tasks that report
//! back over a channel, so input and drawing never wait on YouTube; playback
//! state is pushed by the daemon and the progress bar is interpolated locally.

use crate::api::Api;
use crate::exec::{self, Outcome};
use crate::ipc::{Client, Event, Repeat, Request, Response, State};
use crate::model::{self, Action, Cont, Item, Kind, Page, Section};
use anyhow::Result;
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::{execute, terminal};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const ACCENT: Color = Color::Rgb(255, 0, 80);
const DIM: Color = Color::Rgb(140, 140, 150);
const PLAYING: Color = Color::Rgb(90, 220, 140);

const NAV: [(&str, &str); 5] = [("Home", "FEmusic_home"), ("Explore", "FEmusic_explore"), ("Library", "FEmusic_library_landing"), ("Liked", "VLLM"), ("History", "FEmusic_history")];

enum Msg {
    Page { view: u64, res: Result<Outcome, String> },
    Outcome { res: Result<Outcome, String>, ctx: Option<Item>, toggled: Option<(u64, usize, usize)> },
    More { view: u64, section: Option<usize>, res: Result<Cont, String> },
    Suggest { seq: u64, items: Vec<Item> },
    Lyrics { vid: String, lyrics: exec::Lyrics },
    Player(Event),
    Reply(Result<Response, String>),
}

#[derive(Clone)]
enum Row {
    Section(usize),
    Text(String),
    Item(usize, usize),
    Blank,
}

struct View {
    id: u64,
    page: Page,
    endpoint: Option<Value>,
    cursor: usize,
    scroll: usize,
    rows: Vec<Row>,
    rows_width: u16,
    loading_more: bool,
    loading: bool,
    /// Top-level pages keep their nav name; feeds have no header title.
    fixed_title: bool,
}

impl View {
    fn new(id: u64, title: &str) -> View {
        View {
            id,
            page: Page { title: title.into(), ..Default::default() },
            endpoint: None,
            cursor: 0,
            scroll: 0,
            rows: Vec::new(),
            rows_width: 0,
            loading_more: false,
            loading: true,
            fixed_title: false,
        }
    }

    fn build_rows(&mut self, width: u16) {
        self.rows_width = width;
        self.rows.clear();
        for (si, s) in self.page.sections.iter().enumerate() {
            if !self.rows.is_empty() {
                self.rows.push(Row::Blank);
            }
            if !s.title.is_empty() {
                self.rows.push(Row::Section(si));
            }
            if let Some(t) = &s.text {
                for l in wrap(t, width.saturating_sub(4) as usize) {
                    self.rows.push(Row::Text(l));
                }
            }
            for ii in 0..s.items.len() {
                self.rows.push(Row::Item(si, ii));
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if !matches!(self.rows.get(self.cursor), Some(Row::Item(..))) {
            self.cursor = self.rows.iter().position(|r| matches!(r, Row::Item(..))).unwrap_or(0);
        }
    }

    fn item(&self) -> Option<(usize, usize, &Item)> {
        match self.rows.get(self.cursor)? {
            Row::Item(s, i) => Some((*s, *i, self.page.sections.get(*s)?.items.get(*i)?)),
            _ => None,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let mut c = self.cursor as isize;
        let step = delta.signum();
        let mut left = delta.abs();
        // Move by `delta` item rows, skipping headers/text, but allow text-only pages to scroll.
        let has_items = self.rows.iter().any(|r| matches!(r, Row::Item(..)));
        while left > 0 {
            let mut next = c + step;
            if has_items {
                while next >= 0 && (next as usize) < n && !matches!(self.rows[next as usize], Row::Item(..)) {
                    next += step;
                }
            }
            if next < 0 || next as usize >= n {
                break;
            }
            c = next;
            left -= 1;
        }
        self.cursor = c as usize;
    }

    /// Continuation to load when the cursor nears the bottom.
    fn wants_more(&self) -> Option<(Option<usize>, String)> {
        if self.loading_more || self.cursor + 8 < self.rows.len() {
            return None;
        }
        if let Some((si, s)) = self.page.sections.iter().enumerate().rev().find(|(_, s)| s.continuation.is_some()) {
            return Some((Some(si), s.continuation.clone()?));
        }
        self.page.continuation.clone().map(|c| (None, c))
    }
}

/// Display width as terminals advance the cursor: per code point (wcwidth).
/// `unicode-width`'s string width treats some Indic conjuncts as narrower
/// than terminals draw them.
fn cw(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Rows drawn one code point per cell with per-character widths, combining
/// marks folded into the preceding cell. This keeps ratatui's idea of where
/// text ends identical to the terminal's for Devanagari, Telugu, Tamil…,
/// which grapheme-based layout does not (text overflowed and left stray
/// vowel signs on other rows).
struct CharLines(Vec<Vec<(String, Style)>>);

impl ratatui::widgets::Widget for CharLines {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        for (row, spans) in self.0.into_iter().enumerate().take(area.height as usize) {
            let y = area.y + row as u16;
            let right = area.x + area.width;
            let mut x = area.x;
            'line: for (text, style) in spans {
                for c in text.chars() {
                    let w = c.width().unwrap_or(0) as u16;
                    if w == 0 {
                        if x > area.x {
                            let cell = &mut buf[(x - 1, y)];
                            let sym = format!("{}{c}", cell.symbol());
                            cell.set_symbol(&sym);
                        }
                        continue;
                    }
                    if x + w > right {
                        break 'line;
                    }
                    let mut tmp = [0u8; 4];
                    buf[(x, y)].set_symbol(c.encode_utf8(&mut tmp)).set_style(style);
                    for extra in 1..w {
                        buf[(x + extra, y)].reset();
                    }
                    x += w;
                }
            }
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            if cw(&line) + cw(word) + 1 > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

fn trunc(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn icon(k: Kind) -> &'static str {
    match k {
        Kind::Song => "♪",
        Kind::Video => "▶",
        Kind::Episode => "◆",
        Kind::Album => "◉",
        Kind::Artist | Kind::Profile => "☻",
        Kind::Playlist => "≡",
        Kind::Podcast => "◎",
        Kind::Category => "▸",
        Kind::Query => "⌕",
        Kind::Other => "·",
    }
}

enum PopupKind {
    Menu { ctx: Option<Item>, loc: Option<(u64, usize, usize)> },
    Pick { ctx: Option<Item>, create: Option<Value> },
    Confirm { endpoint: Value, ctx: Option<Item> },
    Help,
}

struct Popup {
    title: String,
    items: Vec<Action>,
    cursor: usize,
    kind: PopupKind,
}

#[derive(Clone)]
enum InputKind {
    Search,
    NewPlaylist { video_ids: Vec<String>, source: Option<String> },
    Rename { playlist_id: String },
}

struct Input {
    kind: InputKind,
    text: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Main,
    Side,
}

#[derive(PartialEq, Clone, Copy)]
enum Side {
    Queue,
    Lyrics,
}

struct App {
    api: Arc<Api>,
    tx: mpsc::UnboundedSender<Msg>,
    player: mpsc::UnboundedSender<Request>,
    views: Vec<View>,
    next_view: u64,
    nav: usize,
    focus: Focus,
    side: Side,
    state: State,
    state_at: Instant,
    queue: Vec<Item>,
    qcursor: usize,
    qscroll: usize,
    /// (video id, lyrics once loaded).
    lyrics: Option<(String, Option<exec::Lyrics>)>,
    /// Manual scroll offset; None follows playback.
    lyrics_scroll: Option<usize>,
    /// Screen row → timed line index, from the last draw (click to seek).
    lyric_rows: Vec<(u16, usize)>,
    input: Option<Input>,
    suggestions: Vec<Item>,
    scursor: Option<usize>,
    sseq: Arc<AtomicU64>,
    popup: Option<Popup>,
    toast: Option<(String, Instant)>,
    seen_toast: u64,
    quit: bool,
    stop_player: bool,
    // Layout from the last draw, for mouse hit-testing.
    main_area: Rect,
    list_top: u16,
    side_area: Rect,
    bar_area: Rect,
    last_click: Option<(u16, u16, Instant)>,
}

impl App {
    fn view(&mut self) -> &mut View {
        self.views.last_mut().expect("a view")
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn send(&self, r: Request) {
        let _ = self.player.send(r);
    }

    fn position(&self) -> f64 {
        let mut p = self.state.position;
        if self.state.current.is_some() && !self.state.paused && !self.state.loading {
            p += self.state_at.elapsed().as_secs_f64();
        }
        if self.state.duration > 0.0 { p.min(self.state.duration) } else { p }
    }

    // ----- navigation -----

    fn open(&mut self, endpoint: Value, title: &str, push: bool) {
        if !push {
            self.views.clear();
        }
        let id = self.next_view;
        self.next_view += 1;
        let mut v = View::new(id, title);
        v.endpoint = Some(endpoint.clone());
        self.views.push(v);
        self.focus = Focus::Main;
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = exec::run(&api, &endpoint, None).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Page { view: id, res });
        });
    }

    fn open_nav(&mut self, i: usize) {
        self.nav = i;
        let (title, id) = NAV[i];
        self.open(json!({"browseEndpoint": {"browseId": id}}), title, false);
        self.view().fixed_title = true;
    }

    fn refresh(&mut self) {
        let Some(ep) = self.view().endpoint.clone() else { return };
        let title = self.view().page.title.clone();
        self.views.pop();
        self.open(ep, &title, true);
    }

    fn back(&mut self) {
        if self.views.len() > 1 {
            self.views.pop();
        }
    }

    fn load_more(&mut self) {
        let Some((section, tok)) = self.view().wants_more() else { return };
        let id = self.view().id;
        self.view().loading_more = true;
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = exec::continue_page(&api, &tok).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::More { view: id, section, res });
        });
    }

    fn search(&mut self, q: &str) {
        let q = q.trim();
        if q.is_empty() {
            return;
        }
        self.open(json!({"searchEndpoint": {"query": q}}), &format!("Search: {q}"), false);
    }

    fn suggest(&mut self) {
        let Some(inp) = &self.input else { return };
        let text = inp.text.clone();
        let seq = self.sseq.fetch_add(1, Ordering::SeqCst) + 1;
        if text.trim().is_empty() {
            self.suggestions.clear();
            self.scursor = None;
            return;
        }
        let (api, tx, latest) = (self.api.clone(), self.tx.clone(), self.sseq.clone());
        tokio::spawn(async move {
            // Debounce: only the newest keystroke after a short pause asks YouTube.
            tokio::time::sleep(Duration::from_millis(110)).await;
            if latest.load(Ordering::SeqCst) != seq {
                return;
            }
            if let Ok(r) = api.suggestions(&text).await {
                let _ = tx.send(Msg::Suggest { seq, items: model::parse_suggestions(&r) });
            }
        });
    }

    // ----- actions -----

    /// Run an endpoint in the background; the outcome comes back as a message.
    fn run(&mut self, endpoint: Value, ctx: Option<Item>, toggled: Option<(u64, usize, usize)>) {
        // Rendering-only endpoints are handled locally for speed.
        if let Some((k, _)) = model::endpoint_kind(&endpoint) {
            match k {
                "watchEndpoint" | "watchPlaylistEndpoint" => {
                    self.send(Request::Play { endpoint });
                    return;
                }
                "queueAddEndpoint" => {
                    let next = endpoint.pointer("/queueAddEndpoint/queueInsertPosition").and_then(Value::as_str) == Some("INSERT_AFTER_CURRENT_VIDEO");
                    self.send(Request::Enqueue { endpoint, next });
                    return;
                }
                "browseEndpoint" | "searchEndpoint" => {
                    self.open(endpoint, "…", true);
                    return;
                }
                _ => {}
            }
        }
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = exec::run(&api, &endpoint, ctx.as_ref()).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Outcome { res, ctx, toggled });
        });
    }

    fn activate(&mut self, it: Item) {
        if it.kind == Kind::Query {
            if let Some(q) = it.primary.as_ref().and_then(|p| p.pointer("/searchEndpoint/query")).and_then(Value::as_str) {
                let q = q.to_string();
                self.search(&q);
            }
            return;
        }
        match it.primary.clone().or_else(|| it.play.clone()) {
            Some(ep) => self.run(ep, Some(it), None),
            None => self.say("Nothing to open"),
        }
    }

    fn play_item(&mut self, it: &Item) {
        if let Some(ep) = it.play.clone().or_else(|| it.playlist_id.as_ref().map(|p| json!({"watchPlaylistEndpoint": {"playlistId": p}}))) {
            self.send(Request::Play { endpoint: ep });
        } else if it.kind.playable() {
            self.activate(it.clone());
        } else {
            self.say("Open it, then pick something to play");
        }
    }

    fn queue_item(&mut self, it: &Item, next: bool) {
        let pos = if next { "INSERT_AFTER_CURRENT_VIDEO" } else { "INSERT_AT_END" };
        let ep = it.menu.iter().find(|a| a.endpoint.pointer("/queueAddEndpoint/queueInsertPosition").and_then(Value::as_str) == Some(pos)).map(|a| a.endpoint.clone()).or_else(|| {
            let target = if it.kind.playable() { json!({"videoId": it.video_id.as_ref()?}) } else { json!({"playlistId": it.playlist_id.as_ref()?}) };
            Some(json!({"queueAddEndpoint": {"queueTarget": target, "queueInsertPosition": pos}}))
        });
        match ep {
            Some(endpoint) => self.send(Request::Enqueue { endpoint, next }),
            None => self.say("That can't be queued"),
        }
    }

    fn rate_current(&mut self) {
        let Some(cur) = self.state.current.clone() else { return };
        let Some(v) = cur.video_id.clone() else { return };
        let status = if cur.like.as_deref() == Some("LIKE") { "INDIFFERENT" } else { "LIKE" };
        self.run(json!({"likeEndpoint": {"status": status, "target": {"videoId": v}}}), Some(cur), None);
    }

    fn save_to_playlist(&mut self, it: Item) {
        let ep = it.menu.iter().find(|a| a.endpoint.get("addToPlaylistEndpoint").is_some()).map(|a| a.endpoint.clone()).or_else(|| it.video_id.as_ref().map(|v| json!({"addToPlaylistEndpoint": {"videoId": v}})));
        match ep {
            Some(e) => self.run(e, Some(it), None),
            None => self.say("That can't be saved to a playlist"),
        }
    }

    fn open_menu(&mut self) {
        let (title, items, ctx, loc) = if self.focus == Focus::Side && self.side == Side::Queue {
            let Some(it) = self.queue.get(self.qcursor).cloned() else { return };
            let mut items: Vec<Action> = it.menu.iter().filter(|a| a.endpoint.get("deletePlaylistEndpoint").is_none() && a.endpoint.get("removeFromQueueEndpoint").is_none()).cloned().collect();
            items.insert(0, Action { label: "Play now".into(), endpoint: json!({"ytmx": "jump"}), toggled: None });
            items.push(Action { label: "Remove from queue".into(), endpoint: json!({"ytmx": "remove"}), toggled: None });
            items.push(Action { label: "Clear queue".into(), endpoint: json!({"ytmx": "clear"}), toggled: None });
            (it.title.clone(), items, Some(it), None)
        } else {
            let id = self.view().id;
            let Some((s, i, it)) = self.view().item() else {
                self.page_menu();
                return;
            };
            (it.title.clone(), it.menu.clone(), Some(it.clone()), Some((id, s, i)))
        };
        if items.is_empty() {
            self.say("No actions");
            return;
        }
        self.popup = Some(Popup { title, items, cursor: 0, kind: PopupKind::Menu { ctx, loc } });
    }

    fn page_menu(&mut self) {
        let v = self.views.last().expect("a view");
        let p = &v.page;
        let mut items = p.actions.clone();
        items.extend(p.tabs.iter().filter(|t| !t.endpoint.is_null()).map(|t| Action { label: format!("Tab: {}", t.label), ..t.clone() }));
        let title = if p.title.is_empty() { "Page".to_string() } else { p.title.clone() };
        // Share/report on a page refers to the page itself.
        let ctx = Some(Item {
            title: title.clone(),
            browse_id: v.endpoint.as_ref().and_then(|e| e.pointer("/browseEndpoint/browseId")).and_then(Value::as_str).map(String::from),
            playlist_id: v.endpoint.as_ref().and_then(|e| e.pointer("/browseEndpoint/browseId")).and_then(Value::as_str).and_then(|b| b.strip_prefix("VL")).map(String::from),
            ..Default::default()
        });
        if items.is_empty() {
            self.say("This page has no actions");
            return;
        }
        self.popup = Some(Popup { title, items, cursor: 0, kind: PopupKind::Menu { ctx, loc: None } });
    }

    fn switch_tab(&mut self, delta: isize) {
        let p = &self.view().page;
        if p.tabs.is_empty() {
            return;
        }
        let cur = p.selected_tab.map(|i| i as isize).unwrap_or(if delta > 0 { -1 } else { 0 });
        let n = p.tabs.len() as isize;
        let mut i = cur;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if !p.tabs[i as usize].endpoint.is_null() {
                break;
            }
        }
        let tab = p.tabs[i as usize].clone();
        if tab.endpoint.is_null() {
            return;
        }
        // Tabs replace the current page rather than stacking.
        let title = tab.label.clone();
        self.views.pop();
        self.open(tab.endpoint, &title, true);
    }

    fn popup_choose(&mut self) {
        let Some(p) = self.popup.take() else { return };
        let Some(a) = p.items.get(p.cursor).cloned() else { return };
        match p.kind {
            PopupKind::Help => {}
            PopupKind::Menu { ctx, loc } => {
                if let Some(op) = a.endpoint.get("ytmx").and_then(Value::as_str) {
                    let i = self.qcursor;
                    match op {
                        "jump" => self.send(Request::Jump { index: i }),
                        "remove" => self.send(Request::Remove { index: i }),
                        "clear" => self.send(Request::Clear),
                        _ => {}
                    }
                    return;
                }
                let toggled = if a.toggled.is_some() { loc } else { None };
                self.run(a.endpoint, ctx, toggled);
            }
            PopupKind::Pick { ctx, create } => {
                if a.endpoint.get("ytmx").is_some() {
                    if let Some(c) = create {
                        self.run(c, ctx, None);
                    }
                } else {
                    let label = a.label.clone();
                    self.run(a.endpoint, ctx, None);
                    self.say(format!("Saving to {label}…"));
                }
            }
            PopupKind::Confirm { endpoint, ctx } => {
                if a.label == "Yes" {
                    self.run(endpoint, ctx, None);
                }
            }
        }
    }

    fn on_outcome(&mut self, res: Result<Outcome, String>, ctx: Option<Item>, toggled: Option<(u64, usize, usize)>) {
        let out = match res {
            Ok(o) => o,
            Err(e) => {
                self.say(e);
                return;
            }
        };
        // A toggle ran: flip it in the item's menu so the next press undoes it.
        if let Some((vid, s, i)) = toggled
            && let Some(v) = self.views.iter_mut().find(|v| v.id == vid)
                && let Some(it) = v.page.sections.get_mut(s).and_then(|s| s.items.get_mut(i)) {
                    for a in it.menu.iter_mut() {
                        if let Some(t) = a.toggled.take() {
                            if ctx.as_ref().is_some_and(|c| c.menu.iter().any(|m| m.label == a.label)) && matches!(&out, Outcome::Toast(_) | Outcome::Nothing) {
                                let old = std::mem::replace(a, *t);
                                a.toggled = Some(Box::new(Action { toggled: None, ..old }));
                                break;
                            }
                            a.toggled = Some(t);
                        }
                    }
                }
        match out {
            Outcome::Page(mut page, ep) => {
                if page.title.is_empty() {
                    page.title = ctx.as_ref().map(|c| c.title.clone()).unwrap_or_default();
                }
                let id = self.next_view;
                self.next_view += 1;
                let mut v = View::new(id, &page.title);
                v.page = page;
                v.endpoint = Some(ep);
                v.loading = false;
                self.views.push(v);
                self.focus = Focus::Main;
            }
            Outcome::Reload(sections, cont) => {
                let v = self.view();
                v.page.sections = sections;
                v.page.continuation = cont;
                v.rows_width = 0;
                v.cursor = 0;
                v.scroll = 0;
            }
            Outcome::Play(ep) => self.send(Request::Play { endpoint: ep }),
            Outcome::Enqueue { endpoint, next } => self.send(Request::Enqueue { endpoint, next }),
            Outcome::RemoveFromQueue(v) => self.send(Request::RemoveVideo { video_id: v }),
            Outcome::Toast(t) => {
                // Ratings: keep the queue and every visible copy of the item in sync.
                if let Some(c) = &ctx
                    && let (Some(v), Some(st)) = (&c.video_id, rating_from(&t)) {
                        self.send(Request::SetLike { video_id: v.clone(), status: st.into() });
                        for view in self.views.iter_mut() {
                            for it in view.page.sections.iter_mut().flat_map(|s| s.items.iter_mut()) {
                                if it.video_id.as_deref() == Some(v.as_str()) && it.kind.playable() {
                                    model::set_like(it, st);
                                }
                            }
                        }
                    }
                self.say(t);
            }
            Outcome::Confirm { title, message, endpoint } => {
                self.popup = Some(Popup {
                    title: if message.is_empty() { title } else { format!("{title}: {message}") },
                    items: vec![Action { label: "No".into(), ..Default::default() }, Action { label: "Yes".into(), ..Default::default() }],
                    cursor: 0,
                    kind: PopupKind::Confirm { endpoint, ctx },
                });
            }
            Outcome::PickPlaylist { title, mut choices, create } => {
                if create.is_some() {
                    choices.insert(0, Action { label: "＋ New playlist…".into(), endpoint: json!({"ytmx": "create"}), toggled: None });
                }
                self.popup = Some(Popup { title: if title.is_empty() { "Save to playlist".into() } else { title }, items: choices, cursor: 0, kind: PopupKind::Pick { ctx, create } });
            }
            Outcome::CreatePlaylist { video_ids, source_playlist } => {
                self.input = Some(Input { kind: InputKind::NewPlaylist { video_ids, source: source_playlist }, text: String::new() });
            }
            Outcome::EditPlaylist { playlist_id } => {
                self.input = Some(Input { kind: InputKind::Rename { playlist_id }, text: self.view().page.title.clone() });
            }
            Outcome::Url(u) => {
                copy_osc52(&u);
                self.say(format!("Copied {u}"));
            }
            Outcome::Nothing => self.say("Done"),
        }
    }

    fn submit_input(&mut self) {
        let Some(inp) = self.input.take() else { return };
        let text = inp.text.trim().to_string();
        match inp.kind {
            InputKind::Search => {
                if let Some(i) = self.scursor.and_then(|i| self.suggestions.get(i).cloned()) {
                    self.activate(i);
                } else {
                    self.search(&text);
                }
                self.suggestions.clear();
                self.scursor = None;
            }
            InputKind::NewPlaylist { video_ids, source } => {
                if text.is_empty() {
                    return;
                }
                let (api, tx) = (self.api.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let res = exec::create_playlist(&api, &text, "", "PRIVATE", &video_ids, source.as_deref()).await.map(|_| Outcome::Toast(format!("Created playlist {text}"))).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Outcome { res, ctx: None, toggled: None });
                });
            }
            InputKind::Rename { playlist_id } => {
                if text.is_empty() {
                    return;
                }
                let (api, tx) = (self.api.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let res = exec::edit_playlist(&api, &playlist_id, Some(&text), None, None).await.map(|_| Outcome::Toast(format!("Renamed to {text}"))).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Outcome { res, ctx: None, toggled: None });
                });
            }
        }
    }

    fn fetch_lyrics(&mut self) {
        if self.side != Side::Lyrics {
            return;
        }
        let Some(v) = self.state.current.as_ref().and_then(|c| c.video_id.clone()) else { return };
        if self.lyrics.as_ref().is_some_and(|(lv, _)| *lv == v) {
            return;
        }
        self.lyrics = Some((v.clone(), None));
        self.lyrics_scroll = None;
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::spawn(async move {
            let lyrics = exec::lyrics(&api, &v).await.ok().flatten().unwrap_or_else(|| exec::Lyrics { text: "No lyrics for this track.".into(), ..Default::default() });
            let _ = tx.send(Msg::Lyrics { vid: v, lyrics });
        });
    }

    // ----- input -----

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.input.is_some() {
            self.on_input_key(k);
            return;
        }
        if self.popup.is_some() {
            self.on_popup_key(k);
            return;
        }
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('Q') => {
                self.quit = true;
                self.stop_player = true;
            }
            KeyCode::Char('/') | KeyCode::Char('s') if !ctrl => {
                self.input = Some(Input { kind: InputKind::Search, text: String::new() });
                self.suggestions.clear();
                self.scursor = None;
            }
            KeyCode::Char(c @ '1'..='5') => self.open_nav(c as usize - '1' as usize),
            KeyCode::Char('?') => {
                self.popup = Some(Popup { title: "Keys".into(), items: HELP.iter().map(|(k, d)| Action { label: format!("{k:<15} {d}"), ..Default::default() }).collect(), cursor: 0, kind: PopupKind::Help });
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if self.focus == Focus::Side {
                    self.focus = Focus::Main;
                } else {
                    self.back();
                }
            }
            KeyCode::Tab => self.focus = if self.focus == Focus::Main { Focus::Side } else { Focus::Main },
            KeyCode::Char('v') => {
                self.side = if self.side == Side::Queue { Side::Lyrics } else { Side::Queue };
                self.fetch_lyrics();
            }
            KeyCode::Char(' ') => self.send(Request::Toggle),
            KeyCode::Char('n') => self.send(Request::Next),
            KeyCode::Char('p') => self.send(Request::Prev),
            KeyCode::Left => self.send(Request::Seek { seconds: if shift { -30.0 } else { -5.0 }, relative: true }),
            KeyCode::Right => self.send(Request::Seek { seconds: if shift { 30.0 } else { 5.0 }, relative: true }),
            KeyCode::Char('+') | KeyCode::Char('=') => self.send(Request::Volume { value: 5.0, relative: true }),
            KeyCode::Char('-') => self.send(Request::Volume { value: -5.0, relative: true }),
            KeyCode::Char('0') => self.send(Request::Mute),
            KeyCode::Char('r') => self.send(Request::Repeat { mode: None }),
            KeyCode::Char('z') => self.send(Request::Shuffle),
            KeyCode::Char('o') => self.send(Request::Autoplay { on: None }),
            KeyCode::Char('l') => self.rate_current(),
            KeyCode::Char('R') => self.refresh(),
            KeyCode::Char('[') => self.switch_tab(-1),
            KeyCode::Char(']') => self.switch_tab(1),
            KeyCode::Char('M') | KeyCode::Char('.') => self.page_menu(),
            _ if self.focus == Focus::Side => self.on_side_key(k),
            _ => self.on_main_key(k),
        }
    }

    fn on_main_key(&mut self, k: KeyEvent) {
        let page = (self.main_area.height as isize - 6).max(3);
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.view().move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.view().move_by(-1),
            KeyCode::PageDown => self.view().move_by(page),
            KeyCode::PageUp => self.view().move_by(-page),
            KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => self.view().move_by(page / 2),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.view().move_by(-page / 2),
            KeyCode::Char('g') | KeyCode::Home => self.view().move_by(-100_000),
            KeyCode::Char('G') | KeyCode::End => self.view().move_by(100_000),
            KeyCode::Enter => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    self.activate(it);
                }
            }
            KeyCode::Char('m') => self.open_menu(),
            KeyCode::Char('P') => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    self.play_item(&it);
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    self.queue_item(&it, k.code == KeyCode::Char('A'));
                }
            }
            KeyCode::Char('x') => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    match it.menu.iter().find(|a| a.label == "Start mix" || a.label.contains("radio")) {
                        Some(a) => {
                            let e = a.endpoint.clone();
                            self.run(e, Some(it), None);
                        }
                        None => self.say("No mix for this item"),
                    }
                }
            }
            KeyCode::Char('L') => {
                let id = self.views.last().map(|v| v.id).unwrap_or(0);
                if let Some((s, i, it)) = self.view().item() {
                    let it = it.clone();
                    match it.menu.iter().find(|a| a.endpoint.get("likeEndpoint").is_some() && (a.label == "Like" || a.label == "Remove like")) {
                        Some(a) => {
                            let e = a.endpoint.clone();
                            self.run(e, Some(it), Some((id, s, i)));
                        }
                        None => self.say("Can't like this"),
                    }
                }
            }
            KeyCode::Char('S') => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    self.save_to_playlist(it);
                }
            }
            KeyCode::Char('y') => {
                if let Some((_, _, it)) = self.view().item() {
                    let it = it.clone();
                    self.run(json!({"shareEntityEndpoint": {}}), Some(it), None);
                }
            }
            KeyCode::Char('N') => self.input = Some(Input { kind: InputKind::NewPlaylist { video_ids: vec![], source: None }, text: String::new() }),
            _ => {}
        }
        self.load_more();
    }

    fn on_side_key(&mut self, k: KeyEvent) {
        if self.side == Side::Lyrics {
            let cur = self.lyrics_scroll.unwrap_or(0);
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => self.lyrics_scroll = Some(cur + 1),
                KeyCode::Up | KeyCode::Char('k') => self.lyrics_scroll = Some(cur.saturating_sub(1)),
                KeyCode::Char('f') => self.lyrics_scroll = None,
                _ => {}
            }
            return;
        }
        let n = self.queue.len();
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.qcursor = (self.qcursor + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.qcursor = self.qcursor.saturating_sub(1),
            KeyCode::Char('g') => self.qcursor = 0,
            KeyCode::Char('G') => self.qcursor = n.saturating_sub(1),
            KeyCode::PageDown => self.qcursor = (self.qcursor + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.qcursor = self.qcursor.saturating_sub(10),
            KeyCode::Char('c') => self.qcursor = self.state.index.unwrap_or(0),
            KeyCode::Enter => self.send(Request::Jump { index: self.qcursor }),
            KeyCode::Char('d') | KeyCode::Delete => self.send(Request::Remove { index: self.qcursor }),
            KeyCode::Char('J') if self.qcursor + 1 < n => {
                self.send(Request::Move { from: self.qcursor, to: self.qcursor + 1 });
                self.qcursor += 1;
            }
            KeyCode::Char('K') if self.qcursor > 0 => {
                self.send(Request::Move { from: self.qcursor, to: self.qcursor - 1 });
                self.qcursor -= 1;
            }
            KeyCode::Char('C') => self.send(Request::Clear),
            KeyCode::Char('m') => self.open_menu(),
            KeyCode::Char('S') => {
                if let Some(it) = self.queue.get(self.qcursor).cloned() {
                    self.save_to_playlist(it);
                }
            }
            _ => {}
        }
    }

    fn on_input_key(&mut self, k: KeyEvent) {
        let Some(inp) = self.input.as_mut() else { return };
        let is_search = matches!(inp.kind, InputKind::Search);
        match k.code {
            KeyCode::Esc => {
                self.input = None;
                self.suggestions.clear();
                self.scursor = None;
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Backspace => {
                inp.text.pop();
                self.scursor = None;
                if is_search {
                    self.suggest();
                }
            }
            KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                let t = inp.text.trim_end();
                let cut = t.rfind(' ').map(|i| i + 1).unwrap_or(0);
                inp.text.truncate(cut);
                if is_search {
                    self.suggest();
                }
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                inp.text.clear();
                if is_search {
                    self.suggest();
                }
            }
            KeyCode::Down | KeyCode::Tab if is_search && !self.suggestions.is_empty() => {
                self.scursor = Some(self.scursor.map(|c| (c + 1).min(self.suggestions.len() - 1)).unwrap_or(0));
            }
            KeyCode::Up if is_search => self.scursor = self.scursor.and_then(|c| c.checked_sub(1)),
            KeyCode::Char(c) => {
                inp.text.push(c);
                self.scursor = None;
                if is_search {
                    self.suggest();
                }
            }
            _ => {}
        }
    }

    fn on_popup_key(&mut self, k: KeyEvent) {
        let Some(p) = self.popup.as_mut() else { return };
        let n = p.items.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.popup = None,
            KeyCode::Down | KeyCode::Char('j') => p.cursor = (p.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Char('y') if matches!(p.kind, PopupKind::Confirm { .. }) => {
                p.cursor = 1;
                self.popup_choose();
            }
            KeyCode::Char('n') if matches!(p.kind, PopupKind::Confirm { .. }) => self.popup = None,
            KeyCode::Enter => {
                if matches!(p.kind, PopupKind::Help) {
                    self.popup = None;
                } else {
                    self.popup_choose();
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let i = c as usize - '1' as usize;
                if i < n && !matches!(p.kind, PopupKind::Help) {
                    p.cursor = i;
                    self.popup_choose();
                }
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let inside = |r: Rect| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                if inside(self.side_area) {
                    if self.side == Side::Queue {
                        self.qcursor = (self.qcursor as isize + d).clamp(0, self.queue.len().saturating_sub(1) as isize) as usize;
                    } else {
                        self.lyrics_scroll = Some((self.lyrics_scroll.unwrap_or(0) as isize + d).max(0) as usize);
                    }
                } else if inside(self.main_area) && self.popup.is_none() {
                    self.view().move_by(d);
                    self.load_more();
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self.last_click.is_some_and(|(c, r, t)| c == m.column && r == m.row && t.elapsed() < Duration::from_millis(400));
                self.last_click = Some((m.column, m.row, Instant::now()));
                if inside(self.bar_area) && m.row == self.bar_area.y + 2 && self.state.duration > 0.0 {
                    // Progress line: seek to the clicked fraction.
                    let x0 = self.bar_area.x + 8;
                    let w = self.bar_area.width.saturating_sub(16).max(1);
                    let frac = (m.column.saturating_sub(x0) as f64 / w as f64).clamp(0.0, 1.0);
                    self.send(Request::Seek { seconds: frac * self.state.duration, relative: false });
                } else if inside(self.side_area) && self.side == Side::Lyrics {
                    // Click a synced line to jump there and follow again.
                    let target = self.lyric_rows.iter().find(|(y, _)| *y == m.row).map(|(_, i)| *i);
                    let start = target.and_then(|i| self.lyrics.as_ref()?.1.as_ref()?.lines.get(i).map(|l| l.start_ms));
                    if let Some(ms) = start {
                        self.send(Request::Seek { seconds: ms as f64 / 1000.0, relative: false });
                        self.lyrics_scroll = None;
                    }
                } else if inside(self.side_area) && self.side == Side::Queue {
                    self.focus = Focus::Side;
                    let i = self.qscroll + m.row.saturating_sub(self.side_area.y + 1) as usize;
                    if i < self.queue.len() {
                        self.qcursor = i;
                        if double {
                            self.send(Request::Jump { index: i });
                        }
                    }
                } else if inside(self.main_area) && m.row >= self.list_top {
                    self.focus = Focus::Main;
                    let top = self.list_top;
                    let v = self.view();
                    let i = v.scroll + (m.row - top) as usize;
                    if matches!(v.rows.get(i), Some(Row::Item(..))) {
                        v.cursor = i;
                        if double
                            && let Some((_, _, it)) = self.view().item() {
                                let it = it.clone();
                                self.activate(it);
                            }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) if inside(self.main_area) => self.open_menu(),
            _ => {}
        }
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Page { view, res } => {
                let Some(v) = self.views.iter_mut().find(|v| v.id == view) else { return };
                v.loading = false;
                match res {
                    Ok(Outcome::Page(mut page, ep)) => {
                        if v.fixed_title || page.title.is_empty() {
                            page.title = std::mem::take(&mut v.page.title);
                        }
                        v.page = page;
                        v.endpoint = Some(ep);
                        v.rows_width = 0;
                    }
                    Ok(Outcome::Reload(sections, cont)) => {
                        v.page.sections = sections;
                        v.page.continuation = cont;
                        v.rows_width = 0;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        v.page.sections = vec![Section { text: Some(e), ..Default::default() }];
                        v.rows_width = 0;
                    }
                }
            }
            Msg::Outcome { res, ctx, toggled } => self.on_outcome(res, ctx, toggled),
            Msg::More { view, section, res } => {
                let Some(v) = self.views.iter_mut().find(|v| v.id == view) else { return };
                v.loading_more = false;
                match (res, section) {
                    (Ok(Cont::Items(items, c)), Some(si)) => {
                        if let Some(s) = v.page.sections.get_mut(si) {
                            s.continuation = if items.is_empty() { None } else { c };
                            s.items.extend(items);
                        }
                    }
                    (Ok(Cont::Sections(secs, c)), None) => {
                        v.page.continuation = c;
                        v.page.sections.extend(secs);
                    }
                    (Ok(Cont::Items(items, c)), None) => {
                        v.page.continuation = None;
                        if !items.is_empty() {
                            v.page.sections.push(Section { items, continuation: c, ..Default::default() });
                        }
                    }
                    (Ok(Cont::Sections(secs, _)), Some(si)) => {
                        if let Some(s) = v.page.sections.get_mut(si) {
                            s.continuation = None;
                        }
                        v.page.sections.extend(secs);
                    }
                    (Err(e), _) => {
                        // Stop retrying a broken continuation.
                        match section {
                            Some(si) => {
                                if let Some(s) = v.page.sections.get_mut(si) {
                                    s.continuation = None;
                                }
                            }
                            None => v.page.continuation = None,
                        }
                        self.toast = Some((e, Instant::now()));
                        return;
                    }
                }
                v.rows_width = 0;
            }
            Msg::Suggest { seq, items } => {
                if seq == self.sseq.load(Ordering::SeqCst) && self.input.is_some() {
                    self.suggestions = items;
                }
            }
            Msg::Lyrics { vid, lyrics } => {
                if self.lyrics.as_ref().is_some_and(|(v, _)| *v == vid) {
                    self.lyrics = Some((vid, Some(lyrics)));
                }
            }
            Msg::Player(Event::State(s)) => {
                let track_changed = s.current.as_ref().and_then(|c| c.video_id.clone()) != self.state.current.as_ref().and_then(|c| c.video_id.clone());
                if s.toast_id > self.seen_toast {
                    self.seen_toast = s.toast_id;
                    if let Some(t) = &s.toast {
                        self.toast = Some((t.clone(), Instant::now()));
                    }
                }
                if track_changed
                    && let Some(i) = s.index
                        && self.focus != Focus::Side {
                            self.qcursor = i;
                        }
                self.state = s;
                self.state_at = Instant::now();
                if track_changed {
                    self.fetch_lyrics();
                }
            }
            Msg::Player(Event::Queue { items, .. }) => {
                self.queue = items;
                self.qcursor = self.qcursor.min(self.queue.len().saturating_sub(1));
            }
            // Player replies carry state the watch stream also delivers; only errors matter here.
            Msg::Reply(Err(e)) => self.say(e),
            Msg::Reply(Ok(_)) => {}
        }
    }

    // ----- drawing -----

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        let rows = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(4)]).split(area);
        self.draw_top(f, rows[0]);
        let wide = area.width >= 100;
        let body = if wide {
            Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[1]).to_vec()
        } else {
            vec![rows[1], rows[1]]
        };
        let show_main = wide || self.focus == Focus::Main;
        let show_side = wide || self.focus == Focus::Side;
        self.main_area = if show_main { body[0] } else { Rect::default() };
        self.side_area = if show_side { body[1] } else { Rect::default() };
        if show_main {
            self.draw_main(f, body[0]);
        }
        if show_side {
            self.draw_side(f, body[1]);
        }
        self.bar_area = rows[2];
        self.draw_bar(f, rows[2]);
        if self.input.is_some() {
            self.draw_input(f, area);
        }
        if self.popup.is_some() {
            self.draw_popup(f, area);
        }
    }

    fn draw_top(&self, f: &mut ratatui::Frame, r: Rect) {
        let mut spans = vec![Span::styled(" ytmx ", Style::default().fg(Color::White).bg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" ")];
        for (i, (name, _)) in NAV.iter().enumerate() {
            let st = if i == self.nav && self.views.len() == 1 && !self.views[0].page.title.starts_with("Search") {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            spans.push(Span::styled(format!("{} {name}  ", i + 1), st));
        }
        spans.push(Span::styled("/ search   m actions   ? keys", Style::default().fg(DIM)));
        f.render_widget(Paragraph::new(Line::from(spans)), r);
    }

    fn draw_main(&mut self, f: &mut ratatui::Frame, r: Rect) {
        let focused = self.focus == Focus::Main;
        let crumbs = self.views.iter().map(|v| trunc(&v.page.title, 24)).collect::<Vec<_>>().join(" › ");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
            .title(Span::styled(format!(" {} ", trunc(&crumbs, r.width.saturating_sub(4) as usize)), Style::default().add_modifier(Modifier::BOLD)));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let current_vid = self.state.current.as_ref().and_then(|c| c.video_id.clone());
        let v = self.views.last_mut().expect("view");
        if v.rows_width != inner.width {
            v.build_rows(inner.width);
        }
        let mut header: Vec<Line> = Vec::new();
        if !v.page.subtitle.is_empty() {
            header.push(Line::from(Span::styled(trunc(&v.page.subtitle, inner.width as usize), Style::default().fg(DIM))));
        }
        if !v.page.tabs.is_empty() {
            let mut spans = Vec::new();
            for (i, t) in v.page.tabs.iter().enumerate() {
                let sel = v.page.selected_tab == Some(i);
                let st = if sel { Style::default().fg(Color::Black).bg(Color::White) } else { Style::default().fg(Color::Gray) };
                spans.push(Span::styled(format!(" {} ", t.label), st));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(" [ ]", Style::default().fg(DIM)));
            header.push(Line::from(spans));
        }
        if !v.page.actions.is_empty() {
            let acts = v.page.actions.iter().take(6).map(|a| a.label.as_str()).collect::<Vec<_>>().join(" · ");
            header.push(Line::from(vec![Span::styled(". ", Style::default().fg(ACCENT)), Span::styled(trunc(&acts, inner.width as usize - 2), Style::default().fg(DIM))]));
        }
        let hh = header.len() as u16;
        f.render_widget(Paragraph::new(header), Rect { height: hh.min(inner.height), ..inner });
        let list = Rect { y: inner.y + hh, height: inner.height.saturating_sub(hh), ..inner };
        self.list_top = list.y;
        let v = self.views.last_mut().expect("view");
        if v.loading && v.rows.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("  Loading…", Style::default().fg(DIM))), list);
            return;
        }
        let h = list.height as usize;
        if v.cursor < v.scroll {
            v.scroll = v.cursor.saturating_sub(1);
        } else if v.cursor >= v.scroll + h {
            v.scroll = v.cursor + 1 - h;
        }
        let w = list.width as usize;
        let mut lines = Vec::with_capacity(h);
        for (ri, row) in v.rows.iter().enumerate().skip(v.scroll).take(h) {
            let sel = ri == v.cursor;
            lines.push(match row {
                Row::Blank => Line::raw(""),
                Row::Section(si) => Line::from(Span::styled(trunc(&v.page.sections[*si].title, w), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
                Row::Text(t) => Line::from(Span::raw(format!("  {t}"))),
                Row::Item(si, ii) => {
                    let it = &v.page.sections[*si].items[*ii];
                    item_line(it, w, sel && focused, sel, it.video_id.is_some() && it.video_id == current_vid && it.kind.playable())
                }
            });
        }
        if v.loading_more {
            lines.push(Line::from(Span::styled("  Loading more…", Style::default().fg(DIM))));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_side(&mut self, f: &mut ratatui::Frame, r: Rect) {
        let focused = self.focus == Focus::Side;
        let title = match self.side {
            Side::Queue => format!(" Queue {}/{} ", self.state.index.map(|i| i + 1).unwrap_or(0), self.queue.len()),
            Side::Lyrics => match self.lyrics.as_ref().and_then(|(_, l)| l.as_ref()) {
                Some(l) if !l.lines.is_empty() => format!(" Lyrics · synced{} ", if self.lyrics_scroll.is_some() { " · f to follow" } else { "" }),
                _ => " Lyrics ".to_string(),
            },
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
            .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)))
            .title_bottom(Span::styled(" v queue/lyrics ", Style::default().fg(DIM)));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let h = inner.height as usize;
        let w = inner.width as usize;
        match self.side {
            Side::Queue => {
                if self.queue.is_empty() {
                    f.render_widget(Paragraph::new(Span::styled(" Nothing queued. Pick something and press Enter.", Style::default().fg(DIM))), inner);
                    return;
                }
                // Follow the playing track unless the user is browsing the queue.
                let anchor = if focused { self.qcursor } else { self.state.index.unwrap_or(0) };
                if anchor < self.qscroll + 1 {
                    self.qscroll = anchor.saturating_sub(1);
                } else if anchor + 2 > self.qscroll + h {
                    self.qscroll = (anchor + 2).saturating_sub(h);
                }
                let mut lines = Vec::new();
                for (i, it) in self.queue.iter().enumerate().skip(self.qscroll).take(h) {
                    let playing = Some(i) == self.state.index;
                    let sel = focused && i == self.qcursor;
                    let mark = if playing { if self.state.paused { "⏸ " } else { "▶ " } } else { "  " };
                    let dur = it.duration.clone();
                    let room = w.saturating_sub(dur.width() + 3);
                    let mut body = format!("{mark}{}", it.title);
                    if !it.subtitle.is_empty() {
                        body.push_str(" · ");
                        body.push_str(it.subtitle.split(" • ").next().unwrap_or(""));
                    }
                    let body = trunc(&body, room);
                    let pad = room.saturating_sub(body.width());
                    let mut st = Style::default();
                    if playing {
                        st = st.fg(PLAYING).add_modifier(Modifier::BOLD);
                    } else if Some(i) < self.state.index {
                        st = st.fg(DIM);
                    }
                    if sel {
                        st = st.bg(Color::Rgb(50, 50, 60));
                    }
                    lines.push(Line::from(vec![Span::styled(format!("{body}{} ", " ".repeat(pad)), st), Span::styled(dur, Style::default().fg(DIM))]));
                }
                f.render_widget(Paragraph::new(lines), inner);
            }
            Side::Lyrics => {
                self.lyric_rows.clear();
                let loaded = self.lyrics.as_ref().and_then(|(_, l)| l.as_ref());
                let Some(ly) = loaded.filter(|l| !l.lines.is_empty()) else {
                    let text = match &self.lyrics {
                        Some((_, Some(l))) => l.text.clone(),
                        Some((_, None)) => "Loading lyrics…".into(),
                        None => "Play something to see its lyrics.".into(),
                    };
                    let lines = wrap(&text, w.saturating_sub(1));
                    let max = lines.len().saturating_sub(h);
                    // Plain lyrics have no timing: follow playback proportionally.
                    let scroll = self.lyrics_scroll.unwrap_or_else(|| if self.state.duration > 0.0 { ((self.position() / self.state.duration) * max as f64) as usize } else { 0 }).min(max);
                    let out = lines.into_iter().skip(scroll).take(h).map(|l| vec![(l, Style::default())]).collect();
                    f.render_widget(CharLines(out), inner);
                    return;
                };
                // Synced: wrap each line, remembering which lyric each row belongs to.
                let ms = (self.position() * 1000.0) as u64;
                let cur = ly.current(ms);
                let mut rows: Vec<(usize, String)> = Vec::new();
                let mut first_row = Vec::with_capacity(ly.lines.len());
                for (i, l) in ly.lines.iter().enumerate() {
                    first_row.push(rows.len());
                    let text = if l.text.trim().is_empty() { "♪" } else { l.text.as_str() };
                    for part in wrap(text, w.saturating_sub(3)) {
                        rows.push((i, part));
                    }
                    rows.push((usize::MAX, String::new()));
                }
                if let Some(src) = &ly.source {
                    rows.push((usize::MAX, String::new()));
                    rows.push((usize::MAX - 1, src.clone()));
                }
                let max = rows.len().saturating_sub(h);
                // Keep the sung line a third of the way down, like the app.
                let follow = cur.map(|c| first_row[c].saturating_sub(h / 3)).unwrap_or(0);
                let scroll = self.lyrics_scroll.unwrap_or(follow).min(max);
                let mut out: Vec<Vec<(String, Style)>> = Vec::with_capacity(h);
                for (n, (i, text)) in rows.iter().enumerate().skip(scroll).take(h) {
                    let y = inner.y + (n - scroll) as u16;
                    out.push(match *i {
                        usize::MAX => vec![],
                        i if i == usize::MAX - 1 => vec![(format!(" {text}"), Style::default().fg(DIM).add_modifier(Modifier::ITALIC))],
                        i => {
                            self.lyric_rows.push((y, i));
                            if Some(i) == cur {
                                vec![("▍".into(), Style::default().fg(ACCENT)), (format!(" {text}"), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))]
                            } else if cur.is_some_and(|c| i < c) {
                                vec![(format!("  {text}"), Style::default().fg(Color::DarkGray))]
                            } else {
                                vec![(format!("  {text}"), Style::default().fg(DIM))]
                            }
                        }
                    });
                }
                f.render_widget(CharLines(out), inner);
            }
        }
    }

    fn draw_bar(&self, f: &mut ratatui::Frame, r: Rect) {
        let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let w = inner.width as usize;
        let s = &self.state;
        let (title, sub, liked) = match &s.current {
            Some(c) => (c.title.clone(), c.subtitle.clone(), c.like.as_deref() == Some("LIKE")),
            None => ("Nothing playing".into(), String::new(), false),
        };
        let icon = if s.current.is_none() {
            "■"
        } else if s.loading {
            "…"
        } else if s.paused {
            "⏸"
        } else {
            "▶"
        };
        // "🔊 50" is the desktop's output volume when ytmx drives the system mixer.
        let vol = if s.muted { "muted".to_string() } else { format!("{} {:.0}", if s.system_volume { "🔊" } else { "vol" }, s.volume) };
        let right = format!(
            "{}{vol}  {}  {}",
            if liked { "♥  " } else { "♡  " },
            match s.repeat {
                Repeat::Off => "repeat off",
                Repeat::All => "repeat all",
                Repeat::One => "repeat one",
            },
            if s.autoplay { "autoplay" } else { "no autoplay" }
        );
        let left_room = w.saturating_sub(right.width() + 2);
        let left = trunc(&format!("{icon} {title}"), left_room);
        let sub_t = trunc(&sub, left_room.saturating_sub(left.width() + 3));
        let pad = w.saturating_sub(left.width() + 3 + sub_t.width() + right.width());
        let line1 = Line::from(vec![
            Span::styled(left, Style::default().add_modifier(Modifier::BOLD).fg(if s.current.is_some() { Color::White } else { DIM })),
            Span::raw("   "),
            Span::styled(sub_t, Style::default().fg(DIM)),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, Style::default().fg(if liked { ACCENT } else { DIM })),
        ]);
        let pos = self.position();
        let dur = s.duration.max(0.0);
        let bar_w = w.saturating_sub(16).max(1);
        let filled = if dur > 0.0 { ((pos / dur) * bar_w as f64).round() as usize } else { 0 }.min(bar_w);
        let line2 = Line::from(vec![
            Span::styled(format!("{:>7} ", fmt_time(pos)), Style::default().fg(Color::Gray)),
            Span::styled("━".repeat(filled), Style::default().fg(ACCENT)),
            Span::styled("●", Style::default().fg(Color::White)),
            Span::styled("─".repeat(bar_w.saturating_sub(filled)), Style::default().fg(Color::DarkGray)),
            Span::styled(format!(" {}", fmt_time(dur)), Style::default().fg(Color::Gray)),
        ]);
        let hint = match &self.toast {
            Some((t, at)) if at.elapsed() < Duration::from_secs(4) => Span::styled(trunc(t, w), Style::default().fg(Color::Yellow)),
            _ => Span::styled(
                trunc("space play/pause  n/p next/prev  ←/→ seek  +/- vol  0 mute  l like  a/A queue/next  S save  m menu  . page  Tab focus  q quit", w),
                Style::default().fg(DIM),
            ),
        };
        f.render_widget(Paragraph::new(vec![line1, line2, Line::from(hint)]), inner);
    }

    fn draw_input(&self, f: &mut ratatui::Frame, area: Rect) {
        let inp = self.input.as_ref().expect("input");
        let label = match inp.kind {
            InputKind::Search => "Search",
            InputKind::NewPlaylist { .. } => "New playlist title",
            InputKind::Rename { .. } => "Rename playlist",
        };
        let w = (area.width * 3 / 5).max(40).min(area.width);
        let sugg = if matches!(inp.kind, InputKind::Search) { self.suggestions.len().min(12) as u16 } else { 0 };
        let h = 3 + if sugg > 0 { sugg + 1 } else { 0 };
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + 2, width: w, height: h.min(area.height) };
        f.render_widget(Clear, r);
        let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(ACCENT)).title(format!(" {label} "));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines = vec![Line::from(vec![Span::raw(format!("› {}", inp.text)), Span::styled("▏", Style::default().fg(ACCENT))])];
        if sugg > 0 {
            lines.push(Line::raw(""));
            for (i, it) in self.suggestions.iter().take(12).enumerate() {
                let sel = self.scursor == Some(i);
                let mut st = Style::default();
                if sel {
                    st = st.bg(Color::Rgb(50, 50, 60)).add_modifier(Modifier::BOLD);
                }
                let text = if it.subtitle.is_empty() { format!("{} {}", icon(it.kind), it.title) } else { format!("{} {}  {}", icon(it.kind), it.title, it.subtitle) };
                lines.push(Line::from(Span::styled(trunc(&text, inner.width as usize), st)));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_popup(&self, f: &mut ratatui::Frame, area: Rect) {
        let p = self.popup.as_ref().expect("popup");
        let longest = p.items.iter().map(|a| a.label.width()).max().unwrap_or(10).max(p.title.width().min(60));
        let w = (longest as u16 + 8).min(area.width.saturating_sub(4)).max(30);
        let h = (p.items.len() as u16 + 2).min(area.height.saturating_sub(4));
        let r = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + (area.height.saturating_sub(h)) / 2, width: w, height: h };
        f.render_widget(Clear, r);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(format!(" {} ", trunc(&p.title, w as usize - 4)), Style::default().add_modifier(Modifier::BOLD)));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let hgt = inner.height as usize;
        let start = p.cursor.saturating_sub(hgt.saturating_sub(1));
        let lines: Vec<Line> = p
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(hgt)
            .map(|(i, a)| {
                let help = matches!(p.kind, PopupKind::Help);
                let num = if help || i >= 9 { "  ".to_string() } else { format!("{} ", i + 1) };
                let st = if i == p.cursor && !help { Style::default().bg(ACCENT).fg(Color::White) } else { Style::default() };
                Line::from(vec![Span::styled(num, Style::default().fg(DIM)), Span::styled(trunc(&a.label, inner.width as usize - 2), st)])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn rating_from(toast: &str) -> Option<&'static str> {
    match toast {
        "Liked" => Some("LIKE"),
        "Disliked" => Some("DISLIKE"),
        "Rating removed" => Some("INDIFFERENT"),
        _ => None,
    }
}

fn item_line(it: &Item, w: usize, focused_sel: bool, sel: bool, playing: bool) -> Line<'static> {
    let dur = it.duration.clone();
    let heart = if it.like.as_deref() == Some("LIKE") { " ♥" } else { "" };
    let explicit = if it.explicit { " 🅴" } else { "" };
    let right = format!("{heart}{explicit} {dur}");
    let room = w.saturating_sub(right.width() + 4);
    let title = trunc(&it.title, room);
    let sub = if it.subtitle.is_empty() { String::new() } else { trunc(&format!("  {}", it.subtitle), room.saturating_sub(title.width())) };
    let pad = room.saturating_sub(title.width() + sub.width());
    let mut base = Style::default();
    if focused_sel {
        base = base.bg(Color::Rgb(55, 40, 60));
    } else if sel {
        base = base.bg(Color::Rgb(40, 40, 45));
    }
    let tstyle = if playing { base.fg(PLAYING).add_modifier(Modifier::BOLD) } else { base.add_modifier(Modifier::BOLD) };
    Line::from(vec![
        Span::styled(format!("{} {} ", if focused_sel { "›" } else { " " }, icon(it.kind)), base.fg(if playing { PLAYING } else { ACCENT })),
        Span::styled(title, tstyle),
        Span::styled(sub, base.fg(DIM)),
        Span::styled(" ".repeat(pad), base),
        Span::styled(right, base.fg(if heart.is_empty() { DIM } else { ACCENT })),
    ])
}

fn copy_osc52(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let b = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b}\x07");
    let _ = out.flush();
}

const HELP: &[(&str, &str)] = &[
    ("1-5", "Home, Explore, Library, Liked, History"),
    ("/ s", "search (live suggestions; ↓ to pick one)"),
    ("Enter", "open / play the selection"),
    ("P", "play an album/playlist/artist without opening it"),
    ("m  right-click", "all YouTube Music actions for the selection"),
    (". M", "page actions (play, shuffle, save, subscribe, sort, tabs…)"),
    ("[ ]", "previous / next tab or filter chip"),
    ("a A", "add to queue / play next"),
    ("x", "start a mix (radio) from the selection"),
    ("L", "like / unlike the selection"),
    ("S", "save the selection to a playlist"),
    ("N", "new playlist"),
    ("y", "copy the selection's link"),
    ("Backspace Esc", "back"),
    ("R", "refresh the page"),
    ("space", "play / pause"),
    ("n p", "next / previous"),
    ("← → (⇧)", "seek 5 s (30 s)"),
    ("+ -", "volume (the system output volume: same as the tray and media keys)"),
    ("0", "mute / unmute"),
    ("l", "like / unlike what's playing"),
    ("r", "repeat off → all → one"),
    ("z", "shuffle the upcoming queue"),
    ("o", "autoplay on/off"),
    ("v", "side pane: queue / lyrics (synced lyrics highlight the sung line)"),
    ("click a lyric", "jump to that line; j/k scroll, f follows again"),
    ("Tab", "focus the side pane (queue: Enter jump, d remove, J/K move, C clear)"),
    ("q", "quit (music keeps playing)"),
    ("Q", "quit and stop the music"),
];

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture, terminal::LeaveAlternateScreen, crossterm::cursor::Show);
}

pub async fn run() -> Result<()> {
    let api = Arc::new(Api::new().await?);
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (ptx, mut prx) = mpsc::unbounded_channel::<Request>();

    // Player commands: one connection, started (and the daemon spawned) lazily.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut client: Option<Client> = None;
            while let Some(req) = prx.recv().await {
                if client.is_none() {
                    match Client::connect().await {
                        Ok(c) => client = Some(c),
                        Err(e) => {
                            let _ = tx.send(Msg::Reply(Err(format!("{e:#}"))));
                            continue;
                        }
                    }
                }
                let r = client.as_mut().expect("client").request(&req).await;
                if r.is_err() {
                    client = None;
                }
                let _ = tx.send(Msg::Reply(r.map_err(|e| format!("{e:#}"))));
            }
        });
    }
    // Player state: a watch connection, reconnecting when the daemon restarts.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                if let Some(mut c) = Client::existing().await
                    && c.send(&Request::Watch).await.is_ok() && c.recv_line().await.is_ok() {
                        while let Ok(ev) = c.next_event().await {
                            if tx.send(Msg::Player(ev)).is_err() {
                                return;
                            }
                        }
                    }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));
    terminal::enable_raw_mode()?;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let mut app = App {
        api,
        tx,
        player: ptx,
        views: Vec::new(),
        next_view: 1,
        nav: 0,
        focus: Focus::Main,
        side: Side::Queue,
        state: State::default(),
        state_at: Instant::now(),
        queue: Vec::new(),
        qcursor: 0,
        qscroll: 0,
        lyrics: None,
        lyrics_scroll: None,
        lyric_rows: Vec::new(),
        input: None,
        suggestions: Vec::new(),
        scursor: None,
        sseq: Arc::new(AtomicU64::new(0)),
        popup: None,
        toast: None,
        seen_toast: u64::MAX,
        quit: false,
        stop_player: false,
        main_area: Rect::default(),
        list_top: 0,
        side_area: Rect::default(),
        bar_area: Rect::default(),
        last_click: None,
    };
    if !app.api.signed_in() {
        app.say("Not signed in: run `ytmx login` for your library, likes and playlists");
    }
    app.open_nav(0);

    let mut events = EventStream::new();
    // Redraw at ~30 fps while something changes or music plays; idle otherwise.
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_second = -1i64;
    let res: Result<()> = loop {
        if dirty {
            if let Err(e) = term.draw(|f| app.draw(f)) {
                break Err(e.into());
            }
            dirty = false;
        }
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(CEvent::Key(k))) => { app.on_key(k); dirty = true; }
                Some(Ok(CEvent::Mouse(m))) => { app.on_mouse(m); dirty = true; }
                Some(Ok(CEvent::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            Some(m) = rx.recv() => {
                // Seen first so the first message after a toast clears it on time.
                app.on_msg(m);
                while let Ok(m) = rx.try_recv() { app.on_msg(m); }
                dirty = true;
            }
            _ = tick.tick() => {
                // The progress bar moves once per displayed second; toasts expire.
                let sec = app.position() as i64;
                if sec != last_second { last_second = sec; dirty = true; }
                if app.toast.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4) && at.elapsed() < Duration::from_millis(4100)) { dirty = true; }
                if app.side == Side::Lyrics && app.lyrics_scroll.is_none() { dirty = true; }
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    restore_terminal();
    if app.stop_player
        && let Some(mut c) = Client::existing().await {
            let _ = c.request(&Request::Quit).await;
        }
    res
}
