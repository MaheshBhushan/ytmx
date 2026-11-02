//! Execute any endpoint/command object the web client would dispatch.

use crate::api::Api;
use crate::model::{self, Action, Item, Page, Section, endpoint_kind, text, toast_of};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub enum Outcome {
    /// Navigate to a page; the endpoint is kept for refresh/history.
    Page(Page, Value),
    /// Replace the current page's sections (sort / reload).
    Reload(Vec<Section>, Option<String>),
    /// Start playback (watchEndpoint / watchPlaylistEndpoint): handled by the player.
    Play(Value),
    /// Add to the local queue (queueAddEndpoint): handled by the player.
    Enqueue { endpoint: Value, next: bool },
    RemoveFromQueue(String),
    Toast(String),
    Confirm { title: String, message: String, endpoint: Value },
    PickPlaylist { title: String, choices: Vec<Action>, create: Option<Value> },
    /// Needs a title (and optional description/privacy) from the user.
    CreatePlaylist { video_ids: Vec<String>, source_playlist: Option<String> },
    EditPlaylist { playlist_id: String },
    Url(String),
    Nothing,
}

fn share_url(item: Option<&Item>) -> Option<String> {
    let it = item?;
    if let Some(v) = &it.video_id {
        return Some(format!("https://music.youtube.com/watch?v={v}"));
    }
    if let Some(p) = &it.playlist_id {
        return Some(format!("https://music.youtube.com/playlist?list={p}"));
    }
    let b = it.browse_id.as_deref()?;
    Some(if b.starts_with("UC") { format!("https://music.youtube.com/channel/{b}") } else { format!("https://music.youtube.com/browse/{b}") })
}

pub async fn browse_endpoint(api: &Api, ep: &Value) -> Result<Outcome> {
    let v = &ep["browseEndpoint"];
    let id = v["browseId"].as_str().unwrap_or_default();
    let r = api.browse(id, v.get("params").and_then(Value::as_str)).await?;
    let page = model::parse_page(&r);
    Ok(Outcome::Page(page, json!({ "browseEndpoint": v })))
}

/// Run one endpoint. `ctx` is the item the action belongs to (for share URLs).
pub async fn run(api: &Api, ep: &Value, ctx: Option<&Item>) -> Result<Outcome> {
    let Some((kind, v)) = endpoint_kind(ep) else { return Ok(Outcome::Nothing) };
    let toast = |fallback: &str| Outcome::Toast(toast_of(v).unwrap_or_else(|| fallback.to_string()));
    Ok(match kind {
        "browseEndpoint" => browse_endpoint(api, &json!({ kind: v })).await?,
        "searchEndpoint" => {
            let q = v["query"].as_str().unwrap_or_default();
            let r = api.search(q, v.get("params").and_then(Value::as_str)).await?;
            let mut page = model::parse_page(&r);
            page.title = format!("Search: {q}");
            Outcome::Page(page, json!({ kind: v }))
        }
        "watchEndpoint" | "watchPlaylistEndpoint" => Outcome::Play(json!({ kind: v })),
        "queueAddEndpoint" => Outcome::Enqueue {
            next: v["queueInsertPosition"].as_str() == Some("INSERT_AFTER_CURRENT_VIDEO"),
            endpoint: json!({ kind: v }),
        },
        "removeFromQueueEndpoint" => Outcome::RemoveFromQueue(v["videoId"].as_str().unwrap_or_default().into()),
        "likeEndpoint" => {
            let path = match v["status"].as_str() {
                Some("LIKE") => "like/like",
                Some("DISLIKE") => "like/dislike",
                _ => "like/removelike",
            };
            let mut body = json!({ "target": v["target"] });
            if let Some(p) = v.get("likeParams").or_else(|| v.get("removeLikeParams")).or_else(|| v.get("dislikeParams")) {
                body["params"] = p.clone();
            }
            api.call(path, body).await?;
            toast(match v["status"].as_str() {
                Some("LIKE") => "Liked",
                Some("DISLIKE") => "Disliked",
                _ => "Rating removed",
            })
        }
        "feedbackEndpoint" => {
            let r = api.call("feedback", json!({ "feedbackTokens": [v["feedbackToken"]] })).await?;
            let ok = r.pointer("/feedbackResponses/0/isProcessed").and_then(Value::as_bool).unwrap_or(true);
            if !ok {
                bail!("YouTube Music did not process that action");
            }
            toast("Done")
        }
        "subscribeEndpoint" | "unsubscribeEndpoint" => {
            let path = if kind == "subscribeEndpoint" { "subscription/subscribe" } else { "subscription/unsubscribe" };
            let mut body = json!({ "channelIds": v["channelIds"] });
            if let Some(p) = v.get("params") {
                body["params"] = p.clone();
            }
            let r = api.call(path, body).await?;
            Outcome::Toast(toast_of(&r).unwrap_or_else(|| if kind == "subscribeEndpoint" { "Subscribed".into() } else { "Unsubscribed".into() }))
        }
        "addToPlaylistEndpoint" => {
            let mut body = json!({ "excludeWatchLater": true });
            if let Some(id) = v.get("videoId") {
                body["videoIds"] = json!([id]);
            }
            if let Some(id) = v.get("playlistId") {
                body["playlistId"] = id.clone();
            }
            let r = api.call("playlist/get_add_to_playlist", body).await?;
            let rr = &r["contents"][0]["addToPlaylistRenderer"];
            let choices = rr["playlists"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    let o = &p["playlistAddToOptionRenderer"];
                    let by = text(&o["shortBylineText"]);
                    let label = if by.is_empty() { text(&o["title"]) } else { format!("{} ({by})", text(&o["title"])) };
                    let (k, e) = endpoint_kind(&o["addToPlaylistServiceEndpoint"])?;
                    Some(Action { label, endpoint: json!({ k: e }), toggled: None })
                })
                .collect();
            let create = rr["actions"].as_array().into_iter().flatten().find_map(|a| {
                let e = a.pointer("/buttonRenderer/navigationEndpoint")?;
                let (k, ev) = endpoint_kind(e)?;
                Some(json!({ k: ev }))
            });
            Outcome::PickPlaylist { title: text(&rr["title"]), choices, create }
        }
        "playlistEditEndpoint" => {
            let mut body = json!({ "playlistId": v["playlistId"], "actions": v["actions"] });
            if let Some(p) = v.get("params") {
                body["params"] = p.clone();
            }
            let r = api.call("browse/edit_playlist", body).await?;
            if r["status"].as_str().is_some_and(|s| s != "STATUS_SUCCEEDED") {
                bail!("playlist edit failed: {}", r["status"]);
            }
            // Only the response's own toast ("Saved to …"); its menus carry unrelated toasts.
            Outcome::Toast(toast_of(&r["actions"]).or_else(|| toast_of(v)).unwrap_or_else(|| "Playlist updated".into()))
        }
        "createPlaylistEndpoint" | "createPlaylistServiceEndpoint" => Outcome::CreatePlaylist {
            video_ids: v["videoIds"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            source_playlist: v["sourcePlaylistId"].as_str().map(String::from),
        },
        "deletePlaylistEndpoint" => {
            api.call("playlist/delete", json!({ "playlistId": v["playlistId"] })).await?;
            toast("Playlist deleted")
        }
        "playlistEditorEndpoint" => Outcome::EditPlaylist { playlist_id: v["playlistId"].as_str().unwrap_or_default().into() },
        "confirmDialogEndpoint" => {
            let d = &v["content"]["confirmDialogRenderer"];
            let btn = &d["confirmButton"]["buttonRenderer"];
            let inner = btn.get("serviceEndpoint").or_else(|| btn.get("command")).or_else(|| btn.get("navigationEndpoint"));
            let Some((k, e)) = inner.and_then(endpoint_kind) else { return Ok(Outcome::Nothing) };
            Outcome::Confirm {
                title: text(&d["title"]),
                message: d["dialogMessages"].as_array().map(|m| m.iter().map(text).collect::<Vec<_>>().join("\n")).unwrap_or_default(),
                endpoint: json!({ k: e }),
            }
        }
        "browseSectionListReloadEndpoint" => {
            let tok = v.pointer("/continuation/reloadContinuationData/continuation").and_then(Value::as_str).unwrap_or_default();
            let r = api.continuation("browse", tok).await?;
            match model::parse_continuation(&r) {
                model::Cont::Sections(s, c) => Outcome::Reload(s, c),
                model::Cont::Items(items, c) => Outcome::Reload(vec![Section { items, continuation: c, ..Default::default() }], None),
            }
        }
        "commandExecutorCommand" => {
            // Run each command; client-only state mutations are no-ops here.
            let mut last = Outcome::Nothing;
            for c in v["commands"].as_array().into_iter().flatten() {
                let out = Box::pin(run(api, c, ctx)).await?;
                if !matches!(out, Outcome::Nothing) {
                    last = out;
                }
            }
            last
        }
        "shareEntityEndpoint" | "showDialogCommand" => share_url(ctx).map(Outcome::Url).unwrap_or(Outcome::Nothing),
        "urlEndpoint" => Outcome::Url(v["url"].as_str().unwrap_or_default().into()),
        "musicLibraryPersistLaunchNavigationCommand" | "musicCheckboxFormItemMutatedCommand" | "signalServiceEndpoint" | "addToToastAction" | "logFlowLoggingEventCommand" => Outcome::Nothing,
        "playlistFilterSearchEndpoint" => Outcome::Toast("Use / in the page to filter".into()),
        "getReportFormEndpoint" => share_url(ctx)
            .map(|u| Outcome::Url(format!("{u}  (reporting needs the web form)")))
            .unwrap_or(Outcome::Toast("Reporting needs the web form".into())),
        other => bail!("unsupported action type {other}"),
    })
}

/// Load the next page of items for a section, or more sections for a page.
pub async fn continue_page(api: &Api, token: &str) -> Result<model::Cont> {
    Ok(model::parse_continuation(&api.continuation("browse", token).await?))
}

pub async fn create_playlist(api: &Api, title: &str, description: &str, privacy: &str, video_ids: &[String], source: Option<&str>) -> Result<String> {
    let mut body = json!({ "title": title, "description": description, "privacyStatus": privacy });
    if !video_ids.is_empty() {
        body["videoIds"] = json!(video_ids);
    }
    if let Some(s) = source {
        body["sourcePlaylistId"] = json!(s);
    }
    let r = api.call("playlist/create", body).await?;
    r["playlistId"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("playlist/create returned no id"))
}

pub async fn edit_playlist(api: &Api, playlist_id: &str, title: Option<&str>, description: Option<&str>, privacy: Option<&str>) -> Result<()> {
    let mut actions = Vec::new();
    if let Some(t) = title {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": t }));
    }
    if let Some(d) = description {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_DESCRIPTION", "playlistDescription": d }));
    }
    if let Some(p) = privacy {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_PRIVACY", "playlistPrivacy": p }));
    }
    if actions.is_empty() {
        return Ok(());
    }
    api.call("browse/edit_playlist", json!({ "playlistId": playlist_id, "actions": actions })).await?;
    Ok(())
}

pub async fn playlist_add(api: &Api, playlist_id: &str, video_ids: &[String]) -> Result<usize> {
    let actions: Vec<Value> = video_ids.iter().map(|v| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": v, "dedupeOption": "DEDUPE_OPTION_SKIP" })).collect();
    let r = api.call("browse/edit_playlist", json!({ "playlistId": playlist_id, "actions": actions })).await?;
    Ok(r["playlistEditResults"].as_array().map(Vec::len).unwrap_or(0))
}

pub async fn playlist_remove(api: &Api, playlist_id: &str, items: &[(String, String)]) -> Result<()> {
    let actions: Vec<Value> = items.iter().map(|(v, s)| json!({ "action": "ACTION_REMOVE_VIDEO", "removedVideoId": v, "setVideoId": s })).collect();
    api.call("browse/edit_playlist", json!({ "playlistId": playlist_id, "actions": actions })).await?;
    Ok(())
}

/// Plain lyrics for a video via the /next lyrics tab.
pub async fn lyrics(api: &Api, video_id: &str) -> Result<Option<String>> {
    let n = model::parse_next(&api.next(json!({ "videoId": video_id })).await?);
    let Some(ep) = n.lyrics else { return Ok(None) };
    let id = ep.pointer("/browseEndpoint/browseId").and_then(Value::as_str).unwrap_or_default();
    let page = model::parse_page(&api.browse(id, None).await?);
    Ok(page.sections.into_iter().find_map(|s| s.text))
}
