//! One-shot commands. Listings are numbered; the numbers are refs for the
//! next command (stored in the cache dir), so every menu action is scriptable.

use crate::api::Api;
use crate::auth::Session;
use crate::exec::{self, Outcome};
use crate::ipc::{Client, Repeat, Request, State};
use crate::model::{self, Action, Item, Kind, Page};
use crate::{Cmd, PlaylistCmd};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::IsTerminal;

// ---------- output ----------

struct Style(bool);
impl Style {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn num(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    fn head(&self, s: &str) -> String {
        self.wrap("1;35", s)
    }
}

fn style() -> Style {
    Style(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn item_line(st: &Style, n: usize, it: &Item) -> String {
    let mut s = format!("{:>4}  {}", st.num(&n.to_string()), st.bold(&it.title));
    if it.explicit {
        s.push_str(" 🅴");
    }
    if !it.subtitle.is_empty() {
        s.push_str(&st.dim(&format!("  {}", it.subtitle)));
    }
    if !it.duration.is_empty() {
        s.push_str(&format!("  {}", it.duration));
    }
    let tag = it.kind.tag();
    if !tag.is_empty() {
        s.push_str(&st.dim(&format!("  [{tag}]")));
    }
    if it.like.as_deref() == Some("LIKE") {
        s.push_str(" ♥");
    }
    s
}

#[derive(Serialize, Deserialize, Default)]
struct Last {
    title: String,
    actions: Vec<Action>,
    tabs: Vec<Action>,
    items: Vec<Item>,
}

fn last_path() -> std::path::PathBuf {
    crate::paths::cache_dir().join("last.json")
}

fn load_last() -> Last {
    std::fs::read(last_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_last(l: &Last) {
    let _ = std::fs::write(last_path(), serde_json::to_vec(l).unwrap_or_default());
}

fn print_page(page: &Page, limit: usize, json_out: bool) {
    let mut last = Last { title: page.title.clone(), actions: page.actions.clone(), tabs: page.tabs.clone(), items: Vec::new() };
    let mut sections = Vec::new();
    for s in &page.sections {
        let shown: Vec<Item> = s.items.iter().take(limit).cloned().collect();
        let start = last.items.len() + 1;
        last.items.extend(shown.iter().cloned());
        sections.push((s, start, shown));
    }
    save_last(&last);
    if json_out {
        println!("{}", serde_json::to_string_pretty(page).unwrap_or_default());
        return;
    }
    let st = style();
    if !page.title.is_empty() {
        println!("{}", st.head(&page.title));
    }
    if !page.subtitle.is_empty() {
        println!("{}", st.dim(&page.subtitle));
    }
    if let Some(d) = &page.description {
        let d: String = d.chars().take(400).collect();
        println!("{}", st.dim(&d));
    }
    if !page.actions.is_empty() {
        println!("{} {}", st.dim("actions (ytmx do page …):"), page.actions.iter().map(|a| a.label.as_str()).collect::<Vec<_>>().join(" · "));
    }
    if !page.tabs.is_empty() {
        let tabs: Vec<String> = page
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let l = format!("t{} {}", i + 1, t.label);
                if page.selected_tab == Some(i) { st.bold(&format!("[{l}]")) } else { l }
            })
            .collect();
        println!("{} {}", st.dim("tabs (ytmx open tN):"), tabs.join("  "));
    }
    for (s, start, shown) in sections {
        println!();
        if !s.title.is_empty() {
            println!("{}", st.head(&s.title));
        }
        if let Some(t) = &s.text {
            println!("{t}");
        }
        for (i, it) in shown.iter().enumerate() {
            println!("{}", item_line(&st, start + i, it));
        }
        let hidden = s.items.len().saturating_sub(shown.len());
        if hidden > 0 || s.continuation.is_some() {
            println!("{}", st.dim(&format!("      … {}more (-n / --all)", if hidden > 0 { format!("{hidden} ") } else { String::new() })));
        }
    }
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn print_state(s: &State, json_out: bool) {
    if json_out {
        println!("{}", serde_json::to_string_pretty(s).unwrap_or_default());
        return;
    }
    let st = style();
    match &s.current {
        Some(c) => {
            let icon = if s.loading { "…" } else if s.paused { "⏸" } else { "▶" };
            println!("{icon} {}  {}", st.bold(&c.title), st.dim(&c.subtitle));
            println!(
                "  {} / {}   {}   repeat {:?}   autoplay {}   track {}/{}",
                fmt_time(s.position),
                fmt_time(s.duration),
                if s.muted { "muted".to_string() } else { format!("vol {:.0}{}", s.volume, if s.system_volume { " (system)" } else { "" }) },
                s.repeat,
                if s.autoplay { "on" } else { "off" },
                s.index.map(|i| i + 1).unwrap_or(0),
                s.queue_len
            );
        }
        None => println!("Nothing playing ({} queued)", s.queue_len),
    }
    if let Some(e) = &s.error {
        println!("{}", st.dim(&format!("last error: {e}")));
    }
}

// ---------- refs ----------

enum Target {
    Item(Item),
    Page(Last),
    Current,
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn video_item(id: &str) -> Item {
    Item { title: id.into(), kind: Kind::Song, video_id: Some(id.into()), primary: Some(json!({"watchEndpoint": {"videoId": id}})), ..Default::default() }
}

fn browse_item(id: &str) -> Item {
    let mut it = Item { title: id.into(), browse_id: Some(id.into()), primary: Some(json!({"browseEndpoint": {"browseId": id}})), ..Default::default() };
    it.kind = if id.starts_with("UC") {
        Kind::Artist
    } else if id.starts_with("MPRE") {
        Kind::Album
    } else if let Some(p) = id.strip_prefix("VL") {
        it.playlist_id = Some(p.into());
        Kind::Playlist
    } else {
        Kind::Other
    };
    it
}

fn named_page(name: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match name {
        "home" => ("FEmusic_home", None),
        "explore" => ("FEmusic_explore", None),
        "library" => ("FEmusic_library_landing", None),
        "liked" | "likes" => ("VLLM", None),
        "history" => ("FEmusic_history", None),
        "charts" => ("FEmusic_charts", None),
        "moods" | "genres" => ("FEmusic_moods_and_genres", None),
        "new" | "new-releases" => ("FEmusic_new_releases", None),
        "podcasts" => ("FEmusic_library_non_music_audio_list", None),
        "episodes" | "later" => ("VLSE", None),
        "playlists" => ("FEmusic_liked_playlists", None),
        "songs" => ("FEmusic_liked_videos", None),
        "albums" => ("FEmusic_liked_albums", None),
        "artists" => ("FEmusic_library_corpus_track_artists", None),
        "subscriptions" | "subs" => ("FEmusic_library_corpus_artists", None),
        "profiles" => ("FEmusic_library_user_profile_channels_list", Some("ggMCCAc%3D")),
        "uploads" => ("FEmusic_library_privately_owned_tracks", None),
        "uploaded-albums" => ("FEmusic_library_privately_owned_releases", None),
        _ => return None,
    })
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}="))).map(|v| v.split('#').next().unwrap_or(v).to_string())
}

fn parse_target(s: &str) -> Option<Target> {
    let s = s.trim();
    if s.is_empty() || s == "current" || s == "now" {
        return Some(Target::Current);
    }
    if s == "page" {
        return Some(Target::Page(load_last()));
    }
    if let Ok(n) = s.parse::<usize>() {
        return load_last().items.into_iter().nth(n.checked_sub(1)?).map(Target::Item);
    }
    if s.contains("youtube.com") || s.contains("youtu.be") {
        if let Some(v) = query_param(s, "v") {
            return Some(Target::Item(video_item(&v)));
        }
        if let Some(l) = query_param(s, "list") {
            return Some(Target::Item(browse_item(&format!("VL{l}"))));
        }
        if let Some(rest) = s.split("youtu.be/").nth(1) {
            return Some(Target::Item(video_item(&rest[..rest.len().min(11)])));
        }
        for marker in ["/channel/", "/browse/"] {
            if let Some(rest) = s.split(marker).nth(1) {
                return Some(Target::Item(browse_item(rest.split(['?', '/']).next().unwrap_or(rest))));
            }
        }
    }
    if s.starts_with("UC") && s.len() == 24 || s.starts_with("MPRE") || s.starts_with("VL") || s.starts_with("MPLA") {
        return Some(Target::Item(browse_item(s)));
    }
    if ["PL", "OLAK5uy", "RD", "LR"].iter().any(|p| s.starts_with(p)) && s.len() > 12 || s == "LM" {
        return Some(Target::Item(browse_item(&format!("VL{s}"))));
    }
    if is_video_id(s) && s.chars().any(|c| c.is_ascii_digit() || c == '-' || c == '_' || c.is_ascii_uppercase()) {
        return Some(Target::Item(video_item(s)));
    }
    None
}

async fn current_item() -> Result<Item> {
    let mut c = Client::existing().await.ok_or_else(|| anyhow!("nothing is playing"))?;
    c.request(&Request::State).await?.state.and_then(|s| s.current).ok_or_else(|| anyhow!("nothing is playing"))
}

/// A ref, or a free-text query resolved to its top song.
async fn resolve_item(api: &Api, s: &str) -> Result<Item> {
    match parse_target(s) {
        Some(Target::Item(i)) => Ok(i),
        Some(Target::Current) => current_item().await,
        Some(Target::Page(_)) => bail!("`page` only works with `ytmx do`/`actions`"),
        None if s.chars().all(|c| c.is_ascii_digit()) => bail!("no item {s} in the last listing"),
        None => {
            let r = api.search(s, Some(SONGS)).await?;
            let page = model::parse_page(&r);
            page.sections.into_iter().flat_map(|s| s.items).find(|i| i.kind.playable()).ok_or_else(|| anyhow!("no song found for {s:?}"))
        }
    }
}

const SONGS: &str = "EgWKAQIIAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D";

fn filter_params(f: &str) -> Result<&'static str> {
    Ok(match f {
        "songs" | "song" => SONGS,
        "videos" | "video" => "EgWKAQIQAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        "albums" | "album" => "EgWKAQIYAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        "artists" | "artist" => "EgWKAQIgAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        "playlists" | "community" => "EgeKAQQoAEABahAQBRADEAQQChAJEBUQDhAQ",
        "featured" => "EgeKAQQoADgBahAQBRADEAQQChAJEBUQDhAQ",
        "episodes" => "EgWKAQJIAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        "podcasts" => "EgWKAQJQAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        "profiles" => "EgWKAQJYAWoQEAUQAxAEEAoQCRAVEA4QEA%3D%3D",
        other => bail!("unknown filter {other:?}: songs, videos, albums, artists, playlists, featured, episodes, podcasts, profiles"),
    })
}

/// What "play" means for an item.
async fn play_endpoint(api: &Api, it: &Item) -> Result<Value> {
    if it.kind.playable() {
        if let Some(p) = &it.primary
            && model::endpoint_kind(p).is_some_and(|(k, _)| k == "watchEndpoint") {
                return Ok(p.clone());
            }
        if let Some(v) = &it.video_id {
            return Ok(json!({"watchEndpoint": {"videoId": v}}));
        }
    }
    if let Some(p) = &it.play {
        return Ok(p.clone());
    }
    if let Some(pl) = &it.playlist_id {
        return Ok(json!({"watchPlaylistEndpoint": {"playlistId": pl}}));
    }
    if let Some(p) = &it.primary {
        // Artist / album / anything else: open it and use its play/shuffle/mix button.
        if let Outcome::Page(page, _) = exec::run(api, p, Some(it)).await? {
            if let Some(a) = page.actions.iter().find(|a| model::endpoint_kind(&a.endpoint).is_some_and(|(k, _)| k.starts_with("watch"))) {
                return Ok(a.endpoint.clone());
            }
            if let Some(i) = page.sections.iter().flat_map(|s| &s.items).find(|i| i.kind.playable()) {
                return Box::pin(play_endpoint(api, i)).await;
            }
        }
    }
    bail!("{} has nothing to play", it.title)
}

fn queue_endpoint(it: &Item, next: bool) -> Option<Value> {
    let pos = if next { "INSERT_AFTER_CURRENT_VIDEO" } else { "INSERT_AT_END" };
    if let Some(a) = it.menu.iter().find(|a| a.endpoint.pointer("/queueAddEndpoint/queueInsertPosition").and_then(Value::as_str) == Some(pos)) {
        return Some(a.endpoint.clone());
    }
    let target = if it.kind.playable() {
        json!({"videoId": it.video_id.as_ref()?})
    } else {
        json!({"playlistId": it.playlist_id.as_ref()?})
    };
    Some(json!({"queueAddEndpoint": {"queueTarget": target, "queueInsertPosition": pos}}))
}

async fn player(spawn: bool) -> Result<Client> {
    if spawn {
        Client::connect().await
    } else {
        Client::existing().await.ok_or_else(|| anyhow!("the player is not running (start something with `ytmx play`)"))
    }
}

async fn send(req: Request, spawn: bool, json_out: bool) -> Result<()> {
    let mut c = player(spawn).await?;
    let r = c.request(&req).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    } else if let Some(m) = &r.message {
        println!("{m}");
    } else if let Some(s) = &r.state {
        print_state(s, false);
    }
    Ok(())
}

fn pick<'a>(actions: &'a [Action], want: &str) -> Result<&'a Action> {
    if let Ok(n) = want.parse::<usize>() {
        return actions.get(n.wrapping_sub(1)).ok_or_else(|| anyhow!("no action {n}"));
    }
    let w = want.to_lowercase();
    actions
        .iter()
        .find(|a| a.label.to_lowercase() == w)
        .or_else(|| actions.iter().find(|a| a.label.to_lowercase().starts_with(&w)))
        .or_else(|| actions.iter().find(|a| a.label.to_lowercase().contains(&w)))
        .ok_or_else(|| anyhow!("no action matching {want:?}; available: {}", actions.iter().map(|a| a.label.as_str()).collect::<Vec<_>>().join(", ")))
}

struct DoOpts {
    to: Option<String>,
    yes: bool,
    title: Option<String>,
}

/// Handle whatever an action produced, CLI-style.
async fn settle(api: &Api, out: Outcome, ctx: Option<&Item>, opts: &DoOpts, json_out: bool, limit: usize) -> Result<()> {
    match out {
        Outcome::Page(page, _) => print_page(&page, limit, json_out),
        Outcome::Reload(sections, _) => {
            let page = Page { sections, ..Default::default() };
            print_page(&page, limit, json_out);
        }
        Outcome::Play(ep) => send(Request::Play { endpoint: ep }, true, json_out).await?,
        Outcome::Enqueue { endpoint, next } => send(Request::Enqueue { endpoint, next }, true, json_out).await?,
        Outcome::RemoveFromQueue(v) => send(Request::RemoveVideo { video_id: v }, false, json_out).await?,
        Outcome::Toast(t) => println!("{t}"),
        Outcome::Url(u) => println!("{u}"),
        Outcome::Nothing => println!("Done"),
        Outcome::Confirm { title, message, endpoint } => {
            if !opts.yes {
                bail!("{title}: {message} (re-run with --yes)");
            }
            let out = exec::run(api, &endpoint, ctx).await?;
            Box::pin(settle(api, out, ctx, opts, json_out, limit)).await?;
        }
        Outcome::PickPlaylist { choices, create, .. } => {
            let Some(to) = &opts.to else {
                bail!("pick a playlist with --to: {}", choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", "));
            };
            match pick(&choices, to) {
                Ok(c) => {
                    let out = exec::run(api, &c.endpoint, ctx).await?;
                    Box::pin(settle(api, out, ctx, opts, json_out, limit)).await?;
                }
                Err(e) => {
                    let Some(create) = create else { return Err(e) };
                    // Not an existing playlist: create it with this item.
                    let out = exec::run(api, &create, ctx).await?;
                    let opts2 = DoOpts { title: Some(to.clone()), to: None, yes: opts.yes };
                    Box::pin(settle(api, out, ctx, &opts2, json_out, limit)).await?;
                }
            }
        }
        Outcome::CreatePlaylist { video_ids, source_playlist } => {
            let title = opts.title.clone().ok_or_else(|| anyhow!("creating a playlist needs --title"))?;
            let id = exec::create_playlist(api, &title, "", "PRIVATE", &video_ids, source_playlist.as_deref()).await?;
            println!("Created playlist {title} ({id})");
        }
        Outcome::EditPlaylist { playlist_id } => println!("Use `ytmx playlist edit {playlist_id} --title … --description … --privacy …`"),
    }
    Ok(())
}

async fn rate(api: &Api, target: Option<String>, status: &str) -> Result<()> {
    let it = match target {
        Some(t) => resolve_item(api, &t).await?,
        None => current_item().await?,
    };
    let tgt = match (&it.video_id, &it.playlist_id) {
        (Some(v), _) if it.kind.playable() || it.playlist_id.is_none() => json!({"videoId": v}),
        (_, Some(p)) => json!({"playlistId": p}),
        (Some(v), None) => json!({"videoId": v}),
        _ => bail!("{} can't be rated", it.title),
    };
    let out = exec::run(api, &json!({"likeEndpoint": {"status": status, "target": tgt}}), Some(&it)).await?;
    if let (Some(v), Some(mut c)) = (&it.video_id, Client::existing().await) {
        let _ = c.request(&Request::SetLike { video_id: v.clone(), status: status.into() }).await;
    }
    if let Outcome::Toast(t) = out {
        println!("{t}: {}", it.title);
    }
    Ok(())
}

async fn browse_page(api: &Api, id: &str, params: Option<&str>) -> Result<Page> {
    match exec::browse_endpoint(api, &json!({"browseEndpoint": {"browseId": id, "params": params}})).await? {
        Outcome::Page(p, _) => Ok(p),
        _ => unreachable!(),
    }
}

/// Follow section and page continuations.
async fn load_all(api: &Api, page: &mut Page) -> Result<()> {
    for s in page.sections.iter_mut() {
        while let Some(tok) = s.continuation.take() {
            if let model::Cont::Items(items, c) = exec::continue_page(api, &tok).await? {
                if items.is_empty() {
                    break;
                }
                s.items.extend(items);
                s.continuation = c;
            }
        }
    }
    while let Some(tok) = page.continuation.take() {
        if let model::Cont::Sections(secs, c) = exec::continue_page(api, &tok).await? {
            page.sections.extend(secs);
            page.continuation = c;
        }
    }
    Ok(())
}

pub async fn run(cmd: Cmd, json_out: bool) -> Result<()> {
    // Player-only commands don't need the API client.
    let ctl = |r: Request| send(r, false, json_out);
    match cmd {
        Cmd::Pause => return ctl(Request::Pause).await,
        Cmd::Resume => return ctl(Request::Resume).await,
        Cmd::Toggle => return ctl(Request::Toggle).await,
        Cmd::Mute => return send(Request::Mute, true, json_out).await,
        Cmd::Next => return ctl(Request::Next).await,
        Cmd::Prev => return ctl(Request::Prev).await,
        Cmd::Stop => return ctl(Request::Stop).await,
        Cmd::Clear => return ctl(Request::Clear).await,
        Cmd::Shuffle => return ctl(Request::Shuffle).await,
        Cmd::Jump { index } => return ctl(Request::Jump { index: index.saturating_sub(1) }).await,
        Cmd::Remove { index } => return ctl(Request::Remove { index: index.saturating_sub(1) }).await,
        Cmd::Move { from, to } => return ctl(Request::Move { from: from.saturating_sub(1), to: to.saturating_sub(1) }).await,
        Cmd::Quit => {
            return match Client::existing().await {
                Some(mut c) => {
                    c.request(&Request::Quit).await?;
                    println!("Stopped");
                    Ok(())
                }
                None => {
                    println!("Not running");
                    Ok(())
                }
            };
        }
        Cmd::Status => {
            return match Client::existing().await {
                Some(mut c) => {
                    print_state(&c.request(&Request::State).await?.state.unwrap_or_default(), json_out);
                    Ok(())
                }
                None if json_out => {
                    println!("null");
                    Ok(())
                }
                None => {
                    println!("Not running");
                    Ok(())
                }
            };
        }
        Cmd::Queue => {
            let mut c = player(false).await?;
            let r = c.request(&Request::Queue).await?;
            let items = r.queue.unwrap_or_default();
            if json_out {
                println!("{}", serde_json::to_string_pretty(&items)?);
                return Ok(());
            }
            let st = style();
            let cur = r.state.and_then(|s| s.index);
            for (i, it) in items.iter().enumerate() {
                let line = item_line(&st, i + 1, it);
                println!("{}{line}", if Some(i) == cur { "▶" } else { " " });
            }
            return Ok(());
        }
        Cmd::Seek { position, to } => {
            let (rel, secs) = parse_seek(&position, to)?;
            return ctl(Request::Seek { seconds: secs, relative: rel }).await;
        }
        Cmd::Volume { value } => {
            let rel = value.starts_with('+') || value.starts_with('-');
            let v: f64 = value.trim_start_matches('+').parse().context("volume is a number")?;
            return send(Request::Volume { value: v, relative: rel }, true, json_out).await;
        }
        Cmd::Repeat { mode } => {
            let m = match mode.as_deref() {
                None => None,
                Some("off") => Some(Repeat::Off),
                Some("all") => Some(Repeat::All),
                Some("one") => Some(Repeat::One),
                Some(o) => bail!("repeat mode is off, all or one, not {o:?}"),
            };
            return send(Request::Repeat { mode: m }, true, json_out).await;
        }
        Cmd::Autoplay { mode } => {
            let on = match mode.as_deref() {
                None => None,
                Some("on") => Some(true),
                Some("off") => Some(false),
                Some(o) => bail!("autoplay is on or off, not {o:?}"),
            };
            return send(Request::Autoplay { on }, true, json_out).await;
        }
        Cmd::Logout => {
            println!("{}", if Session::delete()? { "Signed out (local session removed)" } else { "Not signed in" });
            let _ = std::fs::remove_file(crate::paths::cache_dir().join("cookies.txt"));
            return Ok(());
        }
        Cmd::Login { relay, relay_url, browser, profile, list_profiles, cookie, cookie_file, authuser } => {
            if list_profiles {
                for p in crate::auth::list_profiles(browser.as_deref().unwrap_or("chrome"))? {
                    println!("{p}");
                }
                return Ok(());
            }
            let url = relay_url.unwrap_or_else(crate::relay::url);
            let mut s = match (cookie, cookie_file, browser) {
                (Some(c), _, _) => Session::from_cookie_header(&c, "pasted")?,
                (None, Some(f), _) => Session::from_cookie_header(&std::fs::read_to_string(&f)?, "file")?,
                (None, None, Some(b)) => Session::import_browser(&b, profile.as_deref())?.0,
                (None, None, None) if relay || profile.is_none() && crate::relay::available(&url).await => {
                    let (header, tab) = crate::relay::cookies(&url).await?;
                    Session::from_cookie_header(&header, &format!("omp browser relay: {tab}")).context("the YouTube tab behind the relay is not signed in")?
                }
                (None, None, None) => {
                    eprintln!("No omp browser relay at {url}; reading Chrome's cookie files instead.");
                    Session::import_browser("chrome", profile.as_deref())?.0
                }
            };
            s.authuser = authuser;
            s.save()?;
            let api = Api::new().await?;
            let who = whoami(&api).await.context("cookies imported, but YouTube Music rejected them")?;
            println!("Signed in as {who} (from {})", s.source);
            if Client::existing().await.is_some() {
                println!("Restart the player to use the new session: ytmx quit");
            }
            return Ok(());
        }
        _ => {}
    }

    let api = Api::new().await?;
    match cmd {
        Cmd::Whoami => {
            if !api.signed_in() {
                println!("Not signed in (run `ytmx login`)");
            } else {
                println!("{}", whoami(&api).await?);
            }
        }
        Cmd::Search { query, filter, library, limit } => {
            let q = query.join(" ");
            if q.is_empty() {
                bail!("search for what?");
            }
            let mut page = if library {
                model::parse_page(&api.search(&q, Some("agIYBA%3D%3D")).await?)
            } else {
                model::parse_page(&api.search(&q, filter.as_deref().map(filter_params).transpose()?).await?)
            };
            if library
                && let Some(f) = filter {
                    let tab = pick(&page.tabs, &f)?.endpoint.clone();
                    if let Outcome::Page(p, _) = exec::run(&api, &tab, None).await? {
                        page = p;
                    }
                }
            page.title = format!("Search: {q}");
            print_page(&page, limit, json_out);
        }
        Cmd::Suggest { query } => {
            let items = model::parse_suggestions(&api.suggestions(&query.join(" ")).await?);
            print_page(&Page { sections: vec![model::Section { items, ..Default::default() }], ..Default::default() }, 50, json_out);
        }
        Cmd::Open { target, limit, all } => {
            let t = target.join(" ");
            let t = if t.is_empty() { "home".to_string() } else { t };
            let mut page = if let Some((id, params)) = named_page(&t) {
                browse_page(&api, id, params).await?
            } else if let Some(n) = t.strip_prefix('t').and_then(|n| n.parse::<usize>().ok()) {
                let last = load_last();
                let tab = last.tabs.get(n.wrapping_sub(1)).ok_or_else(|| anyhow!("no tab {n} on the last page"))?;
                match exec::run(&api, &tab.endpoint, None).await? {
                    Outcome::Page(p, _) => p,
                    Outcome::Reload(sections, _) => Page { title: last.title, sections, ..Default::default() },
                    _ => bail!("tab {n} is not a page"),
                }
            } else {
                let it = resolve_item(&api, &t).await?;
                let ep = it.primary.clone().ok_or_else(|| anyhow!("{} has nothing to open", it.title))?;
                match exec::run(&api, &ep, Some(&it)).await? {
                    Outcome::Page(p, _) => p,
                    Outcome::Play(_) => bail!("{} is a track; use `ytmx play`", it.title),
                    _ => bail!("{} did not open a page", it.title),
                }
            };
            if all {
                load_all(&api, &mut page).await?;
            }
            print_page(&page, if all { usize::MAX } else { limit }, json_out);
        }
        Cmd::Actions { target } => {
            let actions = match parse_target(target.as_deref().unwrap_or("current")) {
                Some(Target::Page(l)) => l.actions,
                Some(Target::Item(i)) => i.menu,
                Some(Target::Current) => current_item().await?.menu,
                None => resolve_item(&api, target.as_deref().unwrap_or_default()).await?.menu,
            };
            if json_out {
                println!("{}", serde_json::to_string_pretty(&actions)?);
            } else {
                let st = style();
                for (i, a) in actions.iter().enumerate() {
                    println!("{:>4}  {}", st.num(&(i + 1).to_string()), a.label);
                }
            }
        }
        Cmd::Do { target, action, to, yes, title } => {
            let (actions, ctx) = match parse_target(&target) {
                Some(Target::Page(l)) => (l.actions, None),
                Some(Target::Item(i)) => (i.menu.clone(), Some(i)),
                Some(Target::Current) => {
                    let i = current_item().await?;
                    (i.menu.clone(), Some(i))
                }
                None => bail!("unknown ref {target:?} (a number from the last listing, `page` or `current`)"),
            };
            // Toggles can be addressed by either side ("save to library" / "remove from library").
            let mut all = actions.clone();
            all.extend(actions.iter().filter_map(|a| a.toggled.as_deref().cloned()));
            let a = pick(&all, &action.join(" "))?.clone();
            let out = exec::run(&api, &a.endpoint, ctx.as_ref()).await?;
            if let (Some(st), Some(it)) = (a.endpoint.pointer("/likeEndpoint/status").and_then(Value::as_str), &ctx)
                && let (Some(v), Some(mut c)) = (&it.video_id, Client::existing().await) {
                    let _ = c.request(&Request::SetLike { video_id: v.clone(), status: st.into() }).await;
                }
            settle(&api, out, ctx.as_ref(), &DoOpts { to, yes, title }, json_out, 25).await?;
        }
        Cmd::Play { target } => {
            if target.is_empty() {
                return send(Request::Resume, false, json_out).await;
            }
            let it = resolve_item(&api, &target.join(" ")).await?;
            let ep = play_endpoint(&api, &it).await?;
            send(Request::Play { endpoint: ep }, true, json_out).await?;
        }
        Cmd::Radio { target } => match target {
            None => send(Request::Radio, false, json_out).await?,
            Some(t) => {
                let it = resolve_item(&api, &t).await?;
                let ep = match (&it.video_id, &it.playlist_id) {
                    (Some(v), _) if it.kind.playable() => json!({"watchEndpoint": {"videoId": v, "playlistId": format!("RDAMVM{v}"), "params": "wAEB"}}),
                    _ => it
                        .menu
                        .iter()
                        .find(|a| a.label.contains("mix") || a.label.contains("radio"))
                        .map(|a| a.endpoint.clone())
                        .ok_or_else(|| anyhow!("{} has no radio", it.title))?,
                };
                send(Request::Play { endpoint: ep }, true, json_out).await?;
            }
        },
        Cmd::Add { targets, next } => {
            if targets.is_empty() {
                bail!("add what?");
            }
            for t in &targets {
                let it = resolve_item(&api, t).await?;
                let ep = queue_endpoint(&it, next).ok_or_else(|| anyhow!("{} can't be queued", it.title))?;
                send(Request::Enqueue { endpoint: ep, next }, true, json_out).await?;
            }
        }
        Cmd::Like { target } => rate(&api, target, "LIKE").await?,
        Cmd::Dislike { target } => rate(&api, target, "DISLIKE").await?,
        Cmd::Unlike { target } => rate(&api, target, "INDIFFERENT").await?,
        Cmd::Save { target, to } => {
            let it = match target {
                Some(t) => resolve_item(&api, &t).await?,
                None => current_item().await?,
            };
            let ep = it
                .menu
                .iter()
                .find(|a| a.endpoint.get("addToPlaylistEndpoint").is_some())
                .map(|a| a.endpoint.clone())
                .or_else(|| it.video_id.as_ref().map(|v| json!({"addToPlaylistEndpoint": {"videoId": v}})))
                .ok_or_else(|| anyhow!("{} can't be saved to a playlist", it.title))?;
            let out = exec::run(&api, &ep, Some(&it)).await?;
            settle(&api, out, Some(&it), &DoOpts { to: Some(to), yes: false, title: None }, json_out, 25).await?;
        }
        Cmd::Lyrics { target, timed } => {
            let it = match target {
                Some(t) => resolve_item(&api, &t).await?,
                None => current_item().await?,
            };
            let v = it.video_id.clone().ok_or_else(|| anyhow!("{} is not a track", it.title))?;
            match exec::lyrics(&api, &v).await? {
                Some(l) if json_out => println!("{}", json!({ "title": it.title, "lyrics": l.text, "lines": l.lines, "source": l.source })),
                Some(l) => {
                    println!("{}\n", style().head(&it.title));
                    if timed && !l.lines.is_empty() {
                        for line in &l.lines {
                            let cs = line.start_ms / 10;
                            println!("[{:02}:{:02}.{:02}] {}", cs / 6000, cs / 100 % 60, cs % 100, line.text);
                        }
                    } else {
                        if timed {
                            eprintln!("(no synced lyrics for this song; plain text below)");
                        }
                        println!("{}", l.text);
                    }
                    if let Some(s) = l.source.filter(|_| !l.lines.is_empty()) {
                        println!("\n{}", style().dim(&s));
                    }
                }
                None => println!("No lyrics for {}", it.title),
            }
        }
        Cmd::Related { target } => {
            let it = match target {
                Some(t) => resolve_item(&api, &t).await?,
                None => current_item().await?,
            };
            let v = it.video_id.clone().ok_or_else(|| anyhow!("{} is not a track", it.title))?;
            let w = model::parse_next(&api.next(json!({ "videoId": v })).await?);
            let ep = w.related.ok_or_else(|| anyhow!("no related tab for {}", it.title))?;
            if let Outcome::Page(mut p, _) = exec::run(&api, &ep, None).await? {
                p.title = format!("Related to {}", it.title);
                print_page(&p, 10, json_out);
            }
        }
        Cmd::Subscribe { ref target } | Cmd::Unsubscribe { ref target } => {
            let want = if matches!(cmd, Cmd::Subscribe { .. }) { "Subscribe" } else { "Unsubscribe" };
            let target = target.clone();
            let it = resolve_item(&api, &target).await?;
            let mut actions = it.menu.clone();
            if !actions.iter().any(|a| a.label == want)
                && let Some(p) = &it.primary
                    && let Outcome::Page(page, _) = exec::run(&api, p, Some(&it)).await? {
                        actions = page.actions;
                    }
            let a = actions.iter().find(|a| a.label == want || a.toggled.as_ref().is_some_and(|t| t.label == want));
            let a = a.ok_or_else(|| anyhow!("{} has no {want} button", it.title))?;
            // Pages lag a few seconds behind subscription changes, so run the side
            // that was asked for rather than trusting the page's current state.
            let ep = if a.label == want { a.endpoint.clone() } else { a.toggled.as_ref().map(|t| t.endpoint.clone()).unwrap_or_default() };
            let out = exec::run(&api, &ep, Some(&it)).await?;
            settle(&api, out, Some(&it), &DoOpts { to: None, yes: true, title: None }, json_out, 25).await?;
        }
        Cmd::Playlist(pc) => playlist_cmd(&api, pc).await?,
        Cmd::Raw { endpoint, body } => {
            let b: Value = match body {
                Some(b) => serde_json::from_str(&b).context("body must be JSON")?,
                None => json!({}),
            };
            println!("{}", serde_json::to_string_pretty(&api.call(&endpoint, b).await?)?);
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn parse_seek(s: &str, absolute: bool) -> Result<(bool, f64)> {
    let rel = !absolute && (s.starts_with('+') || s.starts_with('-') || !s.contains(':'));
    let neg = s.starts_with('-');
    let body = s.trim_start_matches(['+', '-']);
    let secs = body.split(':').try_fold(0.0, |acc, p| p.parse::<f64>().map(|v| acc * 60.0 + v)).context("seek takes seconds or m:ss")?;
    let secs = if neg { -secs } else { secs };
    // A bare positive number without --to is relative forward ("seek 30").
    Ok((rel, secs))
}

async fn whoami(api: &Api) -> Result<String> {
    let r = api.call("account/account_menu", json!({})).await?;
    let mut name = None;
    let mut handle = None;
    fn walk(v: &Value, name: &mut Option<String>, handle: &mut Option<String>) {
        match v {
            Value::Object(m) => {
                if let Some(h) = m.get("activeAccountHeaderRenderer") {
                    *name = Some(model::text(&h["accountName"]));
                    *handle = Some(model::text(&h["channelHandle"]));
                    return;
                }
                m.values().for_each(|x| walk(x, name, handle));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, name, handle)),
            _ => {}
        }
    }
    walk(&r, &mut name, &mut handle);
    let name = name.filter(|n| !n.is_empty()).ok_or_else(|| anyhow!("not signed in: YouTube Music returned no account (cookies expired? run `ytmx login`)"))?;
    Ok(match handle.filter(|h| !h.is_empty()) {
        Some(h) => format!("{name} ({h})"),
        None => name,
    })
}

fn privacy(p: &str) -> Result<&'static str> {
    Ok(match p.to_lowercase().as_str() {
        "public" => "PUBLIC",
        "unlisted" => "UNLISTED",
        "private" => "PRIVATE",
        o => bail!("privacy is public, unlisted or private, not {o:?}"),
    })
}

/// Playlist id from a ref, id or URL.
async fn playlist_id(api: &Api, s: &str) -> Result<String> {
    let it = resolve_item(api, s).await?;
    it.playlist_id.or_else(|| it.browse_id.and_then(|b| b.strip_prefix("VL").map(String::from))).ok_or_else(|| anyhow!("{s} is not a playlist"))
}

async fn video_ids(api: &Api, refs: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for r in refs {
        let it = resolve_item(api, r).await?;
        out.push(it.video_id.ok_or_else(|| anyhow!("{} is not a track", it.title))?);
    }
    Ok(out)
}

async fn playlist_cmd(api: &Api, pc: PlaylistCmd) -> Result<()> {
    match pc {
        PlaylistCmd::Create { title, description, privacy: p, refs } => {
            let ids = video_ids(api, &refs).await?;
            let id = exec::create_playlist(api, &title, &description, privacy(&p)?, &ids, None).await?;
            println!("Created {title}: https://music.youtube.com/playlist?list={id}");
        }
        PlaylistCmd::Edit { playlist, title, description, privacy: p } => {
            let id = playlist_id(api, &playlist).await?;
            let pv = p.as_deref().map(privacy).transpose()?;
            exec::edit_playlist(api, &id, title.as_deref(), description.as_deref(), pv).await?;
            println!("Updated {id}");
        }
        PlaylistCmd::Delete { playlist, yes } => {
            let id = playlist_id(api, &playlist).await?;
            if !yes {
                bail!("deleting {id} can't be undone; re-run with --yes");
            }
            api.call("playlist/delete", json!({ "playlistId": id })).await?;
            println!("Deleted {id}");
        }
        PlaylistCmd::Add { playlist, refs } => {
            let id = playlist_id(api, &playlist).await?;
            let ids = video_ids(api, &refs).await?;
            let n = exec::playlist_add(api, &id, &ids).await?;
            println!("Added {n} track(s) to {id}");
        }
        PlaylistCmd::Remove { playlist, refs } => {
            let id = playlist_id(api, &playlist).await?;
            let mut items = Vec::new();
            for r in &refs {
                let it = resolve_item(api, r).await?;
                match (it.video_id, it.set_video_id) {
                    (Some(v), Some(s)) => items.push((v, s)),
                    _ => bail!("{r}: list the playlist first (`ytmx open {playlist}`) and use its numbers"),
                }
            }
            exec::playlist_remove(api, &id, &items).await?;
            println!("Removed {} track(s) from {id}", items.len());
        }
    }
    Ok(())
}
