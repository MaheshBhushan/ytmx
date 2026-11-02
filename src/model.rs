//! Generic InnerTube renderer → page model.
//!
//! The web client renders every screen from ~a dozen renderer types and wires
//! every button to an "endpoint" object. We keep endpoints verbatim so the
//! executor can replay any action the browser offers, instead of hard-coding
//! one command per feature.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Action {
    pub label: String,
    pub endpoint: Value,
    /// For toggles (library, liked songs, pins): what the action becomes after it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toggled: Option<Box<Action>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Other,
    Song,
    Video,
    Episode,
    Album,
    Artist,
    Playlist,
    Podcast,
    Profile,
    Category,
    Query,
}

impl Kind {
    pub fn playable(self) -> bool {
        matches!(self, Kind::Song | Kind::Video | Kind::Episode)
    }
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Song => "song",
            Kind::Video => "video",
            Kind::Episode => "episode",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
            Kind::Podcast => "podcast",
            Kind::Profile => "profile",
            Kind::Category => "category",
            Kind::Query => "search",
            Kind::Other => "",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub duration: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_video_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browse_id: Option<String>,
    /// What Enter does: open the page, or play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<Value>,
    /// The overlay play button (plays an album/playlist without opening it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub menu: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub like: Option<String>,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Section {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    /// "More" / "See all" endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Page {
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Header buttons and header menu (play, shuffle, radio, save, subscribe, edit, delete...).
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Tabs and filter chips (search filters, library sections, mood chips).
    #[serde(default)]
    pub tabs: Vec<Action>,
    #[serde(default)]
    pub selected_tab: Option<usize>,
    pub sections: Vec<Section>,
    /// Continuation for more sections (home feed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
}

pub enum Cont {
    Sections(Vec<Section>, Option<String>),
    Items(Vec<Item>, Option<String>),
}

// ---------- helpers ----------

pub fn text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(s) = v.get("simpleText").and_then(Value::as_str) {
        return s.to_string();
    }
    v.get("runs")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect())
        .unwrap_or_default()
}

const META_KEYS: [&str; 4] = ["clickTrackingParams", "commandMetadata", "loggingUrls", "loggingContext"];

/// The endpoint's single meaningful key and its payload.
pub fn endpoint_kind(ep: &Value) -> Option<(&str, &Value)> {
    ep.as_object()?.iter().find(|(k, _)| !META_KEYS.contains(&k.as_str())).map(|(k, v)| (k.as_str(), v))
}

/// Drop tracking noise so stored endpoints stay small.
fn clean(ep: &Value) -> Option<Value> {
    let (k, v) = endpoint_kind(ep)?;
    Some(json!({ k: v }))
}

fn thumb(v: &Value) -> Option<String> {
    let mut out = None;
    walk(v, &mut |k, val| {
        if out.is_none() && k == "thumbnails" {
            out = val.as_array().and_then(|a| a.last()).and_then(|t| t.get("url")).and_then(Value::as_str).map(String::from);
        }
        out.is_none()
    });
    out
}

/// Depth-first walk; callback returns false to stop descending into a value.
fn walk(v: &Value, f: &mut dyn FnMut(&str, &Value) -> bool) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if f(k, val) {
                    walk(val, f);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
        _ => {}
    }
}

pub fn kind_of(ep: &Value) -> Kind {
    let Some((k, v)) = endpoint_kind(ep) else { return Kind::Other };
    match k {
        "watchEndpoint" => match v.pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType").and_then(Value::as_str) {
            Some("MUSIC_VIDEO_TYPE_ATV") | None => Kind::Song,
            Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE") => Kind::Episode,
            Some(_) => Kind::Video,
        },
        "watchPlaylistEndpoint" => Kind::Playlist,
        "searchEndpoint" => Kind::Query,
        "browseEndpoint" => match v.pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(Value::as_str) {
            Some("MUSIC_PAGE_TYPE_ALBUM") | Some("MUSIC_PAGE_TYPE_AUDIOBOOK") => Kind::Album,
            Some("MUSIC_PAGE_TYPE_ARTIST") | Some("MUSIC_PAGE_TYPE_LIBRARY_ARTIST") => Kind::Artist,
            Some("MUSIC_PAGE_TYPE_USER_CHANNEL") => Kind::Profile,
            Some("MUSIC_PAGE_TYPE_PLAYLIST") => Kind::Playlist,
            Some("MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE") => Kind::Podcast,
            Some("MUSIC_PAGE_TYPE_NON_MUSIC_AUDIO_TRACK_PAGE") => Kind::Episode,
            _ if v.get("params").is_some() => Kind::Category,
            _ => Kind::Other,
        },
        _ => Kind::Other,
    }
}

fn fill_ids(item: &mut Item, ep: &Value) {
    let Some((k, v)) = endpoint_kind(ep) else { return };
    let s = |p: &str| v.get(p).and_then(Value::as_str).map(String::from);
    match k {
        "watchEndpoint" => {
            item.video_id = item.video_id.take().or_else(|| s("videoId"));
            item.playlist_id = item.playlist_id.take().or_else(|| s("playlistId"));
        }
        "watchPlaylistEndpoint" => item.playlist_id = item.playlist_id.take().or_else(|| s("playlistId")),
        "browseEndpoint" => {
            let id = s("browseId");
            if let Some(id) = &id
                && let Some(p) = id.strip_prefix("VL") {
                    item.playlist_id.get_or_insert_with(|| p.to_string());
                }
            item.browse_id = item.browse_id.take().or(id);
        }
        _ => {}
    }
}

// ---------- menus ----------

fn action(label: String, ep: Option<&Value>) -> Option<Action> {
    let endpoint = clean(ep?)?;
    if label.is_empty() {
        return None;
    }
    // Offline downloads are a mobile-app feature; nothing to do on desktop.
    let k = endpoint_kind(&endpoint)?.0;
    if matches!(k, "offlineVideoEndpoint" | "getDownloadActionCommand" | "offlinePlaylistEndpoint") {
        return None;
    }
    Some(Action { label, endpoint, toggled: None })
}

fn like_actions(lb: &Value, out: &mut Vec<Action>) -> Option<String> {
    let status = lb.get("likeStatus").and_then(Value::as_str).unwrap_or("INDIFFERENT").to_string();
    let target = lb.get("target")?.clone();
    let mk = |label: &str, st: &str| Action { label: label.into(), endpoint: json!({"likeEndpoint": {"status": st, "target": target}}), toggled: None };
    match status.as_str() {
        "LIKE" => out.extend([mk("Remove like", "INDIFFERENT"), mk("Dislike", "DISLIKE")]),
        "DISLIKE" => out.extend([mk("Like", "LIKE"), mk("Remove dislike", "INDIFFERENT")]),
        _ => out.extend([mk("Like", "LIKE"), mk("Dislike", "DISLIKE")]),
    }
    Some(status)
}

/// Parse a `menu` (menuRenderer) into actions; returns like status if present.
fn parse_menu(menu: &Value, out: &mut Vec<Action>) -> Option<String> {
    let m = menu.get("menuRenderer").unwrap_or(menu);
    for it in m.get("items").and_then(Value::as_array).into_iter().flatten() {
        if let Some(r) = it.get("menuNavigationItemRenderer") {
            out.extend(action(text(&r["text"]), r.get("navigationEndpoint")));
        } else if let Some(r) = it.get("menuServiceItemRenderer") {
            out.extend(action(text(&r["text"]), r.get("serviceEndpoint")));
        } else if let Some(r) = it.get("toggleMenuServiceItemRenderer") {
            // isToggled means the toggled side is the current state, so its action is next.
            let on = r.get("isToggled").and_then(Value::as_bool).unwrap_or(false);
            let (cur, other) = if on { ("toggled", "default") } else { ("default", "toggled") };
            if let Some(mut a) = action(text(&r[format!("{cur}Text")]), r.get(format!("{cur}ServiceEndpoint"))) {
                a.toggled = action(text(&r[format!("{other}Text")]), r.get(format!("{other}ServiceEndpoint"))).map(Box::new);
                out.push(a);
            }
        }
    }
    let mut like = None;
    for b in m.get("topLevelButtons").and_then(Value::as_array).into_iter().flatten() {
        if let Some(lb) = b.get("likeButtonRenderer") {
            like = like_actions(lb, out);
        } else if let Some(br) = b.get("buttonRenderer") {
            let ep = br.get("navigationEndpoint").or_else(|| br.get("serviceEndpoint")).or_else(|| br.get("command"));
            out.extend(action(text(&br["text"]).trim().to_string(), ep));
        }
    }
    like
}

/// Collect every button-ish action under a header renderer.
fn header_actions(v: &Value, out: &mut Vec<Action>) {
    walk(v, &mut |k, val| match k {
        "menu" => {
            parse_menu(val, out);
            false
        }
        "musicPlayButtonRenderer" => {
            out.extend(action("Play".into(), val.get("playNavigationEndpoint")));
            false
        }
        "buttonRenderer" => {
            let ep = val.get("navigationEndpoint").or_else(|| val.get("serviceEndpoint")).or_else(|| val.get("command"));
            let mut label = text(&val["text"]);
            if label.is_empty() {
                label = val.pointer("/accessibilityData/accessibilityData/label").and_then(Value::as_str).unwrap_or("").to_string();
            }
            out.extend(action(label, ep));
            false
        }
        "toggleButtonRenderer" => {
            let on = val.get("isToggled").and_then(Value::as_bool).unwrap_or(false);
            let (a, b) = if on { ("toggled", "default") } else { ("default", "toggled") };
            let lbl = |p: &str| {
                let t = text(&val[format!("{p}Text")]);
                if t.is_empty() { val.pointer(&format!("/{p}Tooltip")).and_then(Value::as_str).unwrap_or("").to_string() } else { t }
            };
            if let Some(mut x) = action(lbl(a), val.get(format!("{a}ServiceEndpoint")).or_else(|| val.get(format!("{a}NavigationEndpoint")))) {
                x.toggled = action(lbl(b), val.get(format!("{b}ServiceEndpoint"))).map(Box::new);
                out.push(x);
            }
            false
        }
        "subscribeButtonRenderer" => {
            let subbed = val.get("subscribed").and_then(Value::as_bool).unwrap_or(false);
            // Two shapes: on(Un)SubscribeEndpoints, or serviceEndpoints holding the
            // subscribeEndpoint and a confirm dialog wrapping the unsubscribeEndpoint.
            let find = |key: &str| {
                let mut found = None;
                walk(val, &mut |k, v| {
                    if found.is_none() && k == key {
                        found = Some(json!({ key: v }));
                    }
                    found.is_none()
                });
                found
            };
            let ch = val.get("channelId").cloned();
            let sub = find("subscribeEndpoint")
                .or_else(|| ch.clone().map(|c| json!({"subscribeEndpoint": {"channelIds": [c]}})))
                .and_then(|e| action("Subscribe".into(), Some(&e)));
            let unsub = find("unsubscribeEndpoint")
                .or_else(|| ch.map(|c| json!({"unsubscribeEndpoint": {"channelIds": [c]}})))
                .and_then(|e| action("Unsubscribe".into(), Some(&e)));
            let (mut cur, other) = if subbed { (unsub, sub) } else { (sub, unsub) };
            if let Some(c) = cur.as_mut() {
                c.toggled = other.map(Box::new);
            }
            out.extend(cur);
            false
        }
        "likeButtonRenderer" => {
            like_actions(val, out);
            false
        }
        _ => true,
    });
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(a.label.clone()));
}

// ---------- items ----------

fn badges_explicit(r: &Value) -> bool {
    let mut e = false;
    if let Some(b) = r.get("badges").or_else(|| r.get("subtitleBadges")) {
        walk(b, &mut |k, v| {
            if k == "iconType" && v.as_str() == Some("MUSIC_EXPLICIT_BADGE") {
                e = true;
            }
            true
        });
    }
    e
}

fn play_overlay(r: &Value) -> Option<Value> {
    let mut out = None;
    for key in ["overlay", "thumbnailOverlay"] {
        if let Some(o) = r.get(key) {
            walk(o, &mut |k, v| {
                if out.is_none() && k == "playNavigationEndpoint" {
                    out = clean(v);
                }
                out.is_none()
            });
        }
    }
    out
}

fn finish(mut item: Item, r: &Value) -> Item {
    for ep in [item.primary.clone(), item.play.clone()].into_iter().flatten() {
        fill_ids(&mut item, &ep);
    }
    if item.kind == Kind::Other {
        item.kind = item.primary.as_ref().map(kind_of).unwrap_or_default();
    }
    if item.kind == Kind::Other
        && let Some(p) = &item.play {
            item.kind = kind_of(p);
        }
    if let Some(m) = r.get("menu") {
        item.like = parse_menu(m, &mut item.menu);
    }
    item.explicit = badges_explicit(r);
    item.thumbnail = r.get("thumbnail").or_else(|| r.get("thumbnailRenderer")).and_then(thumb);
    item
}

fn list_item(r: &Value) -> Item {
    let cols: Vec<&Value> = r
        .get("flexColumns")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|c| c.get("musicResponsiveListItemFlexColumnRenderer").map(|c| &c["text"])).collect())
        .unwrap_or_default();
    let title_ep = cols.first().and_then(|c| c.pointer("/runs/0/navigationEndpoint")).and_then(clean);
    let subtitle = cols.iter().skip(1).map(|c| text(c)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • ");
    let duration = r
        .get("fixedColumns")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .map(|c| text(&c["musicResponsiveListItemFixedColumnRenderer"]["text"]))
        .unwrap_or_default();
    let play = play_overlay(r);
    let nav = r.get("navigationEndpoint").and_then(clean);
    let pid = r.get("playlistItemData");
    let item = Item {
        title: cols.first().map(|c| text(c)).unwrap_or_default(),
        subtitle,
        duration,
        video_id: pid.and_then(|p| p.get("videoId")).and_then(Value::as_str).map(String::from),
        set_video_id: pid.and_then(|p| p.get("playlistSetVideoId")).and_then(Value::as_str).map(String::from),
        primary: nav.or(title_ep).or_else(|| play.clone()),
        play,
        ..Default::default()
    };
    finish(item, r)
}

fn two_row(r: &Value) -> Item {
    let item = Item {
        title: text(&r["title"]),
        subtitle: text(&r["subtitle"]),
        primary: r.get("navigationEndpoint").and_then(clean),
        play: play_overlay(r),
        ..Default::default()
    };
    finish(item, r)
}

fn panel_video(r: &Value) -> Item {
    let byline = r.get("longBylineText").or_else(|| r.get("shortBylineText")).map(text).unwrap_or_default();
    let item = Item {
        title: text(&r["title"]),
        subtitle: byline,
        duration: r.get("lengthText").map(text).unwrap_or_default(),
        video_id: r.get("videoId").and_then(Value::as_str).map(String::from),
        set_video_id: r.get("playlistSetVideoId").and_then(Value::as_str).map(String::from),
        primary: r.get("navigationEndpoint").and_then(clean),
        selected: r.get("selected").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    finish(item, r)
}

fn nav_button(r: &Value) -> Item {
    let mut item = Item { title: text(&r["buttonText"]), kind: Kind::Category, primary: r.get("clickCommand").and_then(clean), ..Default::default() };
    if let Some(p) = item.primary.clone() {
        fill_ids(&mut item, &p);
    }
    item
}

fn multi_row(r: &Value) -> Item {
    let item = Item {
        title: text(&r["title"]),
        subtitle: [text(&r["subtitle"]), r.get("playbackProgress").map(|p| text(&p["musicPlaybackProgressRenderer"]["durationText"])).unwrap_or_default()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" • "),
        primary: r.get("onTap").and_then(clean),
        play: play_overlay(r),
        ..Default::default()
    };
    finish(item, r)
}

fn suggestion(r: &Value) -> Item {
    Item { title: text(&r["suggestion"]), kind: Kind::Query, primary: r.get("navigationEndpoint").and_then(clean), ..Default::default() }
}

fn card_item(r: &Value) -> Item {
    let mut actions = Vec::new();
    if let Some(b) = r.get("buttons") {
        header_actions(b, &mut actions);
    }
    let mut item = Item {
        title: text(&r["title"]),
        subtitle: text(&r["subtitle"]),
        primary: r.pointer("/title/runs/0/navigationEndpoint").and_then(clean).or_else(|| r.get("onTap").and_then(clean)),
        play: play_overlay(r),
        ..Default::default()
    };
    item = finish(item, r);
    actions.append(&mut item.menu);
    let mut seen = std::collections::HashSet::new();
    actions.retain(|a| seen.insert(a.label.clone()));
    item.menu = actions;
    item
}

fn cont_token(v: &Value) -> Option<String> {
    let mut tok = None;
    walk(v, &mut |k, val| {
        if tok.is_none() {
            match k {
                "nextContinuationData" | "nextRadioContinuationData" => tok = val.get("continuation").and_then(Value::as_str).map(String::from),
                "continuationCommand" => tok = val.get("token").and_then(Value::as_str).map(String::from),
                _ => {}
            }
        }
        tok.is_none()
    });
    tok
}

/// Find item renderers anywhere under `v` (not inside other items).
fn collect_items(v: &Value, items: &mut Vec<Item>, cont: &mut Option<String>) {
    walk(v, &mut |k, val| match k {
        "musicResponsiveListItemRenderer" => {
            items.push(list_item(val));
            false
        }
        "musicTwoRowItemRenderer" => {
            items.push(two_row(val));
            false
        }
        "playlistPanelVideoRenderer" => {
            items.push(panel_video(val));
            false
        }
        "musicNavigationButtonRenderer" => {
            items.push(nav_button(val));
            false
        }
        "musicMultiRowListItemRenderer" => {
            items.push(multi_row(val));
            false
        }
        "searchSuggestionRenderer" => {
            items.push(suggestion(val));
            false
        }
        "continuationItemRenderer" | "continuations" => {
            if cont.is_none() {
                *cont = cont_token(val);
            }
            false
        }
        "menu" | "header" | "automixPreviewVideoRenderer" => false,
        _ => true,
    });
}

const ITEM_KEYS: [&str; 6] = [
    "musicResponsiveListItemRenderer",
    "musicTwoRowItemRenderer",
    "playlistPanelVideoRenderer",
    "musicNavigationButtonRenderer",
    "musicMultiRowListItemRenderer",
    "searchSuggestionRenderer",
];

fn parse_section(key: &str, r: &Value, out: &mut Vec<Section>) {
    match key {
        "itemSectionRenderer" => {
            // Either wraps shelves, or (search results) holds item renderers directly.
            let mut direct = Vec::new();
            for c in r.get("contents").and_then(Value::as_array).into_iter().flatten() {
                if let Some((k, v)) = c.as_object().and_then(|m| m.iter().next()) {
                    if ITEM_KEYS.contains(&k.as_str()) {
                        let mut cont = None;
                        collect_items(c, &mut direct, &mut cont);
                    } else {
                        parse_section(k, v, out);
                    }
                }
            }
            if !direct.is_empty() {
                // Consecutive headerless item sections read as one list.
                match out.last_mut() {
                    Some(prev) if prev.title.is_empty() && prev.text.is_none() && prev.continuation.is_none() => prev.items.extend(direct),
                    _ => out.push(Section { items: direct, ..Default::default() }),
                }
            }
        }
        "messageRenderer" | "musicNotifierShelfRenderer" => {
            let t = [text(&r["text"]), text(&r["subtext"]["messageSubtextRenderer"]["text"])].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            if !t.is_empty() {
                out.push(Section { text: Some(t), ..Default::default() });
            }
        }
        "musicDescriptionShelfRenderer" => {
            let mut t = text(&r["description"]);
            let footer = text(&r["footer"]);
            if !footer.is_empty() {
                t.push_str("\n\n");
                t.push_str(&footer);
            }
            out.push(Section { title: text(&r["header"]), text: Some(t), ..Default::default() });
        }
        "musicCardShelfRenderer" => {
            let mut items = vec![card_item(r)];
            let mut cont = None;
            if let Some(c) = r.get("contents") {
                collect_items(c, &mut items, &mut cont);
            }
            let title = r.pointer("/header/musicCardShelfHeaderBasicRenderer/title").map(text).unwrap_or_else(|| "Top result".into());
            out.push(Section { title, items, ..Default::default() });
        }
        "musicResponsiveHeaderRenderer" | "musicEditablePlaylistDetailHeaderRenderer" | "musicTastebuilderShelfRenderer" | "chipCloudRenderer" => {}
        _ => {
            let mut title = String::new();
            let mut more = None;
            if let Some(h) = r.get("header")
                && let Some((_, hv)) = h.as_object().and_then(|m| m.iter().next()) {
                    title = text(&hv["title"]);
                    let strap = text(&hv["strapline"]);
                    if !strap.is_empty() {
                        title = format!("{strap} · {title}");
                    }
                    more = hv.pointer("/moreContentButton/buttonRenderer/navigationEndpoint").and_then(clean);
                }
            if title.is_empty() {
                title = text(&r["title"]);
            }
            if more.is_none() {
                more = r.get("bottomEndpoint").and_then(clean);
            }
            let mut items = Vec::new();
            let mut cont = None;
            for k in ["contents", "items"] {
                if let Some(c) = r.get(k) {
                    collect_items(c, &mut items, &mut cont);
                }
            }
            if cont.is_none()
                && let Some(c) = r.get("continuations") {
                    cont = cont_token(c);
                }
            if !items.is_empty() || cont.is_some() {
                out.push(Section { title, items, continuation: cont, more, text: None });
            }
        }
    }
}

fn parse_section_list(sl: &Value, page: &mut Page) {
    // Filter chips and sort menus live in the header, sometimes nested in
    // musicSideAlignedItemRenderer (library pages).
    if let Some(h) = sl.get("header") {
        walk(h, &mut |k, v| match k {
            "chipCloudChipRenderer" => {
                if let Some(a) = action(text(&v["text"]), v.get("navigationEndpoint")) {
                    if v.get("isSelected").and_then(Value::as_bool).unwrap_or(false) {
                        page.selected_tab = Some(page.tabs.len());
                    }
                    page.tabs.push(a);
                }
                false
            }
            "musicMultiSelectMenuItemRenderer" => {
                let label = format!("Sort: {}", text(&v["title"]));
                page.actions.extend(action(label, v.get("selectedCommand")));
                false
            }
            _ => true,
        });
    }
    for s in sl.get("contents").and_then(Value::as_array).into_iter().flatten() {
        if let Some((k, v)) = s.as_object().and_then(|m| m.iter().next()) {
            if matches!(k.as_str(), "musicResponsiveHeaderRenderer" | "musicEditablePlaylistDetailHeaderRenderer") {
                parse_header(&json!({ k: v }), page);
            } else {
                parse_section(k, v, &mut page.sections);
            }
        }
    }
    if let Some(c) = sl.get("continuations") {
        page.continuation = cont_token(c);
    }
}

fn parse_header(h: &Value, page: &mut Page) {
    let Some((key, r)) = h.as_object().and_then(|m| m.iter().next()) else { return };
    if key == "musicEditablePlaylistDetailHeaderRenderer" {
        if let Some(inner) = r.get("header") {
            parse_header(inner, page);
        }
        header_actions(&r["editHeader"], &mut page.actions);
        return;
    }
    if page.title.is_empty() {
        page.title = text(&r["title"]);
    }
    let sub = ["straplineTextOne", "subtitle", "secondSubtitle"].iter().map(|k| text(&r[*k])).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • ");
    if !sub.is_empty() {
        page.subtitle = sub;
    }
    let desc = r.get("description").map(|d| if d.get("musicDescriptionShelfRenderer").is_some() { text(&d["musicDescriptionShelfRenderer"]["description"]) } else { text(d) });
    if let Some(d) = desc.filter(|d| !d.is_empty()) {
        page.description = Some(d);
    }
    header_actions(r, &mut page.actions);
}

/// Parse a browse/search response into a page.
pub fn parse_page(v: &Value) -> Page {
    let mut page = Page::default();
    if let Some(h) = v.get("header") {
        parse_header(h, &mut page);
    }
    let c = &v["contents"];
    let tabs = c
        .pointer("/singleColumnBrowseResultsRenderer/tabs")
        .or_else(|| c.pointer("/tabbedSearchResultsRenderer/tabs"))
        .or_else(|| c.pointer("/twoColumnBrowseResultsRenderer/tabs"))
        .and_then(Value::as_array);
    if let Some(tabs) = tabs {
        for t in tabs {
            let tr = &t["tabRenderer"];
            if tabs.len() > 1 {
                let label = text(&tr["title"]);
                if let Some(ep) = tr.get("endpoint") {
                    page.tabs.extend(action(label, Some(ep)));
                } else if tr.get("selected").and_then(Value::as_bool).unwrap_or(false) || tr.get("content").is_some() {
                    page.selected_tab = Some(page.tabs.len());
                    page.tabs.push(Action { label, endpoint: Value::Null, toggled: None });
                }
            }
            if let Some(sl) = tr.pointer("/content/sectionListRenderer") {
                parse_section_list(sl, &mut page);
            }
        }
    }
    if let Some(sl) = c.pointer("/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer") {
        parse_section_list(sl, &mut page);
    }
    if let Some(sl) = c.get("sectionListRenderer") {
        parse_section_list(sl, &mut page);
    }
    if page.title.is_empty() {
        page.title = page.sections.iter().find(|s| !s.title.is_empty()).map(|s| s.title.clone()).unwrap_or_default();
    }
    page
}

/// Parse a continuation response.
pub fn parse_continuation(v: &Value) -> Cont {
    // continuationContents has a single key naming the continued renderer.
    if let Some((k, r)) = v.get("continuationContents").and_then(Value::as_object).and_then(|cc| cc.iter().next()) {
        if k == "sectionListContinuation" {
            let mut page = Page::default();
            parse_section_list(r, &mut page);
            return Cont::Sections(page.sections, page.continuation);
        }
        let mut items = Vec::new();
        let mut cont = None;
        for key in ["contents", "items"] {
            if let Some(c) = r.get(key) {
                collect_items(c, &mut items, &mut cont);
            }
        }
        if cont.is_none() {
            cont = r.get("continuations").and_then(cont_token);
        }
        return Cont::Items(items, cont);
    }
    let mut items = Vec::new();
    let mut cont = None;
    if let Some(a) = v.get("onResponseReceivedActions") {
        collect_items(a, &mut items, &mut cont);
    }
    Cont::Items(items, cont)
}

pub fn parse_suggestions(v: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    let mut cont = None;
    if let Some(c) = v.get("contents") {
        collect_items(c, &mut items, &mut cont);
    }
    items
}

/// The "Up next" panel from /next.
#[derive(Debug, Default)]
pub struct WatchNext {
    pub items: Vec<Item>,
    pub continuation: Option<String>,
    pub playlist_id: Option<String>,
    pub lyrics: Option<Value>,
    pub related: Option<Value>,
}

pub fn parse_next(v: &Value) -> WatchNext {
    let mut w = WatchNext::default();
    if let Some(cc) = v.pointer("/continuationContents/playlistPanelContinuation") {
        collect_items(&cc["contents"], &mut w.items, &mut w.continuation);
        w.continuation = w.continuation.or_else(|| cc.get("continuations").and_then(cont_token));
        return w;
    }
    let tabs = v.pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs").and_then(Value::as_array);
    for (i, t) in tabs.into_iter().flatten().enumerate() {
        let tr = &t["tabRenderer"];
        if i == 0 {
            if let Some(p) = tr.pointer("/content/musicQueueRenderer/content/playlistPanelRenderer") {
                collect_items(&p["contents"], &mut w.items, &mut w.continuation);
                w.continuation = w.continuation.or_else(|| p.get("continuations").and_then(cont_token));
                w.playlist_id = p.get("playlistId").and_then(Value::as_str).map(String::from);
            }
            continue;
        }
        let Some(ep) = tr.get("endpoint").and_then(clean) else { continue };
        let pt = ep.pointer("/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(Value::as_str);
        match pt {
            Some("MUSIC_PAGE_TYPE_TRACK_LYRICS") => w.lyrics = Some(ep),
            Some("MUSIC_PAGE_TYPE_TRACK_RELATED") => w.related = Some(ep),
            _ => {}
        }
    }
    w
}

/// Toast text an endpoint carries for success (`addToToastAction`).
pub fn toast_of(v: &Value) -> Option<String> {
    let mut out = None;
    walk(v, &mut |k, val| {
        if out.is_none() {
            let t = match k {
                "successResponseText" => text(val),
                "notificationTextRenderer" if val.get("successResponseText").is_none() => text(&val["text"]),
                "notificationActionRenderer" => text(&val["responseText"]),
                _ => String::new(),
            };
            if !t.is_empty() {
                out = Some(t);
            }
        }
        out.is_none()
    });
    out
}

/// Every item renderer anywhere in a response (e.g. music/get_queue).
pub fn items_in(v: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    let mut cont = None;
    collect_items(v, &mut items, &mut cont);
    items
}

/// Rewrite an item's rating actions after its like status changed.
pub fn set_like(item: &mut Item, status: &str) {
    let target = item.menu.iter().find_map(|a| a.endpoint.pointer("/likeEndpoint/target").cloned());
    let target = target.or_else(|| item.video_id.as_ref().map(|v| json!({ "videoId": v })));
    let Some(target) = target else { return };
    item.menu.retain(|a| a.endpoint.get("likeEndpoint").is_none() || a.label.contains("liked songs"));
    for a in item.menu.iter_mut().filter(|a| a.label.contains("liked songs")) {
        let liked_now = a.label.starts_with("Remove");
        if liked_now != (status == "LIKE")
            && let Some(t) = a.toggled.take() {
                let old = std::mem::replace(a, *t);
                a.toggled = Some(Box::new(Action { toggled: None, ..old }));
            }
    }
    like_actions(&json!({ "likeStatus": status, "target": target }), &mut item.menu);
    item.like = Some(status.to_string());
}
