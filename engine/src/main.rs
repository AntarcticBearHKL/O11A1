//! O11A1 Electron sidecar.
//!
//! One local TCP connection carries newline-delimited JSON. The sidecar owns
//! the Bilibili session, settings, downloads, 养鱼 engine and the native video
//! popup; Electron only sends commands and renders HTML.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use o11a1::accounts::{self, Account};
use o11a1::bilibili::{
    self as bili, CommentLikeResult, LivePlay, MsgKind, NavOutcome, SearchKind, Session, VipCode,
};
use o11a1::fish::FishEngine;
use o11a1::settings::{self, Settings};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex, Notify};

use native_player::{EnhanceMode, InterpMode, Player, PlayerConfig, PlayerHost};

mod downloads;

use downloads::Downloads;

type Rpc<T> = std::result::Result<T, String>;

fn e(error: impl std::fmt::Display) -> String {
    format!("{error:#}")
}

fn rv<T: serde::Serialize>(value: T) -> Rpc<Value> {
    serde_json::to_value(value).map_err(e)
}

fn text(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_i64(params: &Value, key: &str) -> Option<i64> {
    params.get(key).and_then(Value::as_i64)
}

fn num(params: &Value, key: &str) -> i64 {
    opt_i64(params, key).unwrap_or(0)
}

fn unum(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn flag(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn player_id(params: &Value) -> String {
    let id = text(params, "playerId");
    if id.is_empty() {
        "default".to_string()
    } else {
        id
    }
}

struct App {
    settings: Mutex<Settings>,
    account: Mutex<Option<Account>>,
    session: Mutex<Option<Arc<Session>>>,
    session_note: Notify,
    session_error: Mutex<Option<String>>,
    /// At most three native video windows, keyed by the Electron window id.
    players: Mutex<HashMap<String, Player>>,
    fish: Mutex<Option<Arc<FishEngine>>>,
    downloads: Downloads,
    events: broadcast::Sender<Value>,
}

impl App {
    async fn session(&self) -> Rpc<Arc<Session>> {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if let Some(session) = self.session.lock().await.clone() {
                return Ok(session);
            }
            if let Some(error) = self.session_error.lock().await.clone() {
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("网络会话初始化超时".to_string());
            }
            let _ = tokio::time::timeout(remaining, self.session_note.notified()).await;
        }
    }

    async fn settings_snapshot(&self) -> Settings {
        self.settings.lock().await.clone()
    }

    /// Settings safe to hand to the renderer: the DeepSeek key never leaves
    /// the sidecar, only a set/unset flag does.
    async fn public_settings(&self) -> Settings {
        let mut settings = self.settings.lock().await.clone();
        settings.deepseek_key = String::new();
        settings
    }

    async fn player_snapshot(&self, id: &str) -> Value {
        let guard = self.players.lock().await;
        match guard.get(id) {
            Some(player) => player_json(id, player),
            None => json!({ "id": id, "open": false }),
        }
    }

    async fn player_snapshots(&self) -> Vec<Value> {
        let guard = self.players.lock().await;
        guard
            .iter()
            .map(|(id, player)| player_json(id, player))
            .collect()
    }

    async fn player_danmaku_hits(&self) -> (Vec<Value>, Vec<Value>) {
        let guard = self.players.lock().await;
        let mut right = Vec::new();
        let mut long = Vec::new();
        for (id, player) in guard.iter() {
            while let Some(hit) = player.take_danmaku_hit() {
                right.push(hit_json(id, &hit));
            }
            while let Some(hit) = player.take_long_press_hit() {
                long.push(hit_json(id, &hit));
            }
        }
        (right, long)
    }

    fn emit(&self, event: &str, data: Value) {
        let _ = self.events.send(json!({ "event": event, "data": data }));
    }
}

fn hit_json(id: &str, hit: &native_player::DanmakuHit) -> Value {
    json!({
        "id": id,
        "dmid": hit.dmid,
        "midHash": hit.mid_hash,
        "text": hit.text,
        "x": hit.x,
        "y": hit.y,
        "width": hit.width,
        "height": hit.height,
    })
}

fn player_json(id: &str, player: &Player) -> Value {
    json!({
        "id": id,
        "open": player.is_open(),
        "playing": player.is_playing(),
        "position": player.position_ms(),
        "duration": player.duration_ms(),
        "ended": player.ended(),
        "fps": player.fps(),
        "danmaku": player.danmaku_count(),
        "mini": player.is_mini(),
        "audioOnly": player.is_audio_only(),
        "topmost": player.is_topmost(),
        "aspect": player.aspect().as_u8(),
        "brightness": player.brightness(),
        "volume": player.volume(),
        "muted": player.muted(),
        "speed": player.speed(),
        "interp": player.interp().as_u8(),
        "enhance": player.enhance().as_u8(),
        "clickSeq": player.click_seq(),
        "doubleClickSeq": player.double_click_seq(),
        "moveSeq": player.move_seq(),
        "longPressSeq": player.long_press_seq(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind control port")?;
    let port = listener.local_addr()?.port();
    println!("O11A1-ENGINE READY {port}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let saved = accounts::load();
    let boot_token = saved.as_ref().map(accounts::to_token);
    let settings = settings::load();
    let (events, _) = broadcast::channel(256);
    let app = Arc::new(App {
        settings: Mutex::new(settings),
        account: Mutex::new(saved),
        session: Mutex::new(None),
        session_note: Notify::new(),
        session_error: Mutex::new(None),
        players: Mutex::new(HashMap::new()),
        fish: Mutex::new(None),
        downloads: Downloads::new(),
        events,
    });

    {
        let app = app.clone();
        tokio::spawn(async move {
            let session = match boot_token {
                Some(token) => match Session::from_token(&token).await {
                    Ok(session) => Ok(session),
                    Err(first) => Session::new().await.map_err(|second| {
                        format!("恢复登录会话失败：{first:#}；匿名会话失败：{second:#}")
                    }),
                },
                None => Session::new().await.map_err(e),
            };
            match session {
                Ok(session) => {
                    *app.session.lock().await = Some(Arc::new(session));
                }
                Err(error) => {
                    *app.session_error.lock().await = Some(error);
                }
            }
            app.session_note.notify_waiters();
        });
    }

    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                if let Err(error) = auto_feed_tick(&app).await {
                    o11a1::debug_log(&format!("auto feed skipped: {error:#}"));
                }
            }
        });
    }

    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut counters: HashMap<String, (u64, u64, u64)> = HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let states = app.player_snapshots().await;
                let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
                let (right_hits, long_hits) = app.player_danmaku_hits().await;
                for hit in right_hits {
                    app.emit("player.danmaku", hit);
                }
                for hit in long_hits {
                    app.emit("player.longpress", hit);
                }
                for state in states {
                    let id = state
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_string();
                    live.insert(id.clone());
                    let click = state.get("clickSeq").and_then(Value::as_u64).unwrap_or(0);
                    let double = state
                        .get("doubleClickSeq")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let moved = state.get("moveSeq").and_then(Value::as_u64).unwrap_or(0);
                    let (last_click, last_double, last_move) =
                        counters.get(&id).copied().unwrap_or((0, 0, 0));
                    if click > last_click {
                        app.emit(
                            "player.click",
                            json!({ "id": id, "count": click - last_click }),
                        );
                    }
                    if double > last_double {
                        app.emit(
                            "player.doubleclick",
                            json!({ "id": id, "count": double - last_double }),
                        );
                    }
                    if moved > last_move {
                        app.emit(
                            "player.mousemove",
                            json!({ "id": id, "count": moved - last_move }),
                        );
                    }
                    counters.insert(id.clone(), (click, double, moved));
                    app.emit("player.state", state);
                }
                counters.retain(|id, _| live.contains(id));
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(error) = serve(stream, app).await {
                eprintln!("control connection ended: {error:#}");
            }
        });
    }
}

async fn serve(stream: TcpStream, app: Arc<App>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut events = app.events.subscribe();
    loop {
        tokio::select! {
            read = reader.read_line(&mut line) => {
                let count = read?;
                if count == 0 {
                    break;
                }
                let request: Value = match serde_json::from_str(line.trim()) {
                    Ok(value) => value,
                    Err(error) => {
                        let message = json!({"id": Value::Null, "ok": false, "error": format!("bad json: {error}")});
                        writer.write_all(format!("{message}\n").as_bytes()).await?;
                        line.clear();
                        continue;
                    }
                };
                line.clear();
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let method = request.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
                let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                let response = match dispatch(&app, &method, params).await {
                    Ok(data) => json!({"id": id, "ok": true, "data": data}),
                    Err(error) => json!({"id": id, "ok": false, "error": error}),
                };
                writer.write_all(format!("{response}\n").as_bytes()).await?;
                writer.flush().await?;
            }
            event = events.recv() => {
                match event {
                    Ok(value) => {
                        writer.write_all(format!("{value}\n").as_bytes()).await?;
                        writer.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(app: &Arc<App>, method: &str, params: Value) -> Rpc<Value> {
    match method {
        "app.ping" => Ok(json!("pong")),
        "app.bootstrap" => {
            let settings = app.public_settings().await;
            let account = app.account.lock().await.clone().map(public_account);
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "settings": settings,
                "account": account,
                "sessionReady": app.session.lock().await.is_some(),
                "sessionError": app.session_error.lock().await.clone(),
            }))
        }

        // ---- settings ------------------------------------------------------
        "settings.get" => rv(app.public_settings().await),
        "settings.save" => {
            let mut settings: Settings = serde_json::from_value(params).map_err(e)?;
            // The renderer never receives the key, so an empty field means
            // "keep the stored one".
            if settings.deepseek_key.trim().is_empty() {
                settings.deepseek_key = app.settings.lock().await.deepseek_key.clone();
            }
            settings::save(&settings).map_err(e)?;
            *app.settings.lock().await = settings.clone();
            settings.deepseek_key.clear();
            Ok(json!(settings))
        }
        "settings.patch" => {
            let mut current = app.settings.lock().await.clone();
            let mut base = serde_json::to_value(&current).map_err(e)?;
            if let (Some(target), Some(patch)) = (base.as_object_mut(), params.as_object()) {
                for (key, value) in patch {
                    target.insert(key.clone(), value.clone());
                }
            }
            let cloud_changed = params.get("ai_on").is_some() || params.get("ai_level").is_some();
            current = serde_json::from_value(base).map_err(e)?;
            settings::save(&current).map_err(e)?;
            *app.settings.lock().await = current.clone();
            if cloud_changed {
                if let Some(session) = app.session.lock().await.clone() {
                    let _ = session
                        .danmaku_ai_config(current.ai_on, Some(current.ai_level))
                        .await;
                }
            }
            let mut public = current.clone();
            public.deepseek_key.clear();
            app.emit("settings.changed", json!(public));
            current.deepseek_key.clear();
            Ok(json!(current))
        }

        // ---- account / login ----------------------------------------------
        "session.retry" => {
            *app.session_error.lock().await = None;
            *app.session.lock().await = None;
            let app = app.clone();
            tokio::spawn(async move {
                let token = accounts::load().as_ref().map(accounts::to_token);
                let session = match token {
                    Some(token) => Session::from_token(&token).await.map_err(e),
                    None => Session::new().await.map_err(e),
                };
                match session {
                    Ok(session) => {
                        *app.session.lock().await = Some(Arc::new(session));
                        app.session_note.notify_waiters();
                    }
                    Err(error) => {
                        *app.session_error.lock().await = Some(error);
                    }
                }
            });
            Ok(json!(null))
        }
        "account.nav" => {
            let session = app.session().await?;
            let outcome = session.nav().await.map_err(e)?;
            if let NavOutcome::LoggedIn(user) = &outcome {
                if let Some(account) = app.account.lock().await.as_mut() {
                    account.uname = user.uname.clone();
                    account.face = user.face.clone();
                    account.vip = user.vip_status;
                    account.mid = user.mid;
                    let _ = accounts::save(account);
                }
            }
            rv(outcome)
        }
        "login.qr" => {
            let session = app.session().await?;
            let qr = session.qr_generate().await.map_err(e)?;
            Ok(json!({ "url": qr.url, "key": qr.key }))
        }
        "login.poll" => {
            let key = text(&params, "key");
            let session = app.session().await?;
            let poll = session.qr_poll(&key).await.map_err(e)?;
            if poll.state_code == 0 {
                if !poll.cross_url.is_empty() {
                    let _ = session.visit(&poll.cross_url).await;
                }
                let user = match session.nav().await.map_err(e)? {
                    NavOutcome::LoggedIn(user) => user,
                    NavOutcome::NotLoggedIn { code, message } => {
                        return Err(format!("登录校验失败：code={code} {message}"));
                    }
                };
                let account = Account {
                    mid: user.mid,
                    uname: user.uname.clone(),
                    face: user.face.clone(),
                    cookies: poll.cookies,
                    refresh_token: poll.refresh_token,
                    added_at: accounts::now_secs(),
                    vip: user.vip_status,
                };
                accounts::save(&account).map_err(e)?;
                let token = accounts::to_token(&account);
                let new_session = Session::from_token(&token).await.map_err(e)?;
                *app.session.lock().await = Some(Arc::new(new_session));
                *app.account.lock().await = Some(account.clone());
                app.session_note.notify_waiters();
                let public = public_account(account);
                app.emit("account.changed", json!(public));
                return Ok(json!({ "stateCode": 0, "account": public }));
            }
            Ok(json!({ "stateCode": poll.state_code }))
        }
        "login.logout" => {
            let session = app.session().await?;
            let _ = session.logout().await;
            accounts::clear();
            *app.account.lock().await = None;
            let anonymous = Session::new().await.map_err(e)?;
            *app.session.lock().await = Some(Arc::new(anonymous));
            app.emit("account.changed", Value::Null);
            Ok(json!(null))
        }

        // ---- feed / search -------------------------------------------------
        "feed.home" => {
            let session = app.session().await?;
            let fresh = unum(&params, "fresh");
            let items = session
                .homepage_feed_from(fresh, &|_, _| {})
                .await
                .map_err(e)?;
            rv(items)
        }
        "search.suggest" => {
            let session = app.session().await?;
            rv(session.search_suggest(&text(&params, "term")).await.map_err(e)?)
        }
        "search.hot" => {
            let session = app.session().await?;
            rv(session.hot_search().await.map_err(e)?)
        }
        "search.hits" => {
            let session = app.session().await?;
            let kind = SearchKind::from_tab(num(&params, "tab") as i32);
            let page = session
                .search_hits(
                    kind,
                    &text(&params, "keyword"),
                    num(&params, "page").max(1),
                    "",
                    20,
                )
                .await
                .map_err(e)?;
            rv(page)
        }
        "search.history.get" => Ok(json!(load_search_history())),
        "search.history.add" => {
            let keyword = text(&params, "keyword");
            let mut history = load_search_history();
            history.retain(|item| item != &keyword);
            if !keyword.trim().is_empty() {
                history.insert(0, keyword);
            }
            history.truncate(20);
            save_search_history(&history).map_err(e)?;
            Ok(json!(history))
        }
        "search.history.clear" => {
            save_search_history(&[]).map_err(e)?;
            Ok(json!([]))
        }

        // ---- video ---------------------------------------------------------
        "video.detail" => {
            let session = app.session().await?;
            rv(session.video_view(&text(&params, "bvid")).await.map_err(e)?)
        }
        "video.related" => {
            let session = app.session().await?;
            rv(session.related(&text(&params, "bvid")).await.map_err(e)?)
        }
        "video.comments" => {
            let session = app.session().await?;
            rv(session
                .comments(
                    num(&params, "aid"),
                    params.get("hot").and_then(Value::as_bool).unwrap_or(true),
                    &text(&params, "cursor"),
                )
                .await
                .map_err(e)?)
        }
        "video.replies" => {
            let session = app.session().await?;
            rv(session
                .comment_replies(
                    num(&params, "aid"),
                    num(&params, "root"),
                    num(&params, "page").max(1),
                )
                .await
                .map_err(e)?)
        }
        "video.like" => {
            let session = app.session().await?;
            session
                .like(&text(&params, "bvid"), flag(&params, "liked"))
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "video.coin" => {
            let session = app.session().await?;
            session
                .add_coin(
                    &text(&params, "bvid"),
                    num(&params, "count").max(1),
                    flag(&params, "like"),
                )
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "video.fav" => {
            let session = app.session().await?;
            let add: Vec<i64> = params
                .get("add")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            let del: Vec<i64> = params
                .get("del")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            session
                .fav_deal(num(&params, "aid"), &add, &del)
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "video.comment.like" => {
            let session = app.session().await?;
            let result: CommentLikeResult = session
                .comment_like(
                    num(&params, "aid"),
                    num(&params, "rpid"),
                    flag(&params, "liked"),
                )
                .await
                .map_err(e)?;
            rv(result)
        }
        "video.playinfo" => {
            let session = app.session().await?;
            rv(session
                .playurl(&text(&params, "bvid"), unum(&params, "cid"))
                .await
                .map_err(e)?)
        }
        "video.shot" => {
            let session = app.session().await?;
            rv(session
                .videoshot(&text(&params, "bvid"), unum(&params, "cid"))
                .await
                .map_err(e)?)
        }
        "video.heartbeat" => {
            let session = app.session().await?;
            let code = session
                .report_heartbeat(
                    unum(&params, "aid"),
                    &text(&params, "bvid"),
                    unum(&params, "cid"),
                    unum(&params, "played"),
                    unum(&params, "watched"),
                    unum(&params, "duration"),
                    unum(&params, "start"),
                    unum(&params, "type"),
                )
                .await
                .map_err(e)?;
            Ok(json!(code))
        }
        "danmaku.dates" => {
            let session = app.session().await?;
            rv(session
                .danmaku_history_dates(unum(&params, "cid"))
                .await
                .map_err(e)?)
        }
        "danmaku.history" => {
            let session = app.session().await?;
            rv(session
                .danmaku_history(unum(&params, "cid"), &text(&params, "date"))
                .await
                .map_err(e)?)
        }
        "danmaku.like" => {
            let session = app.session().await?;
            let (code, message) = session
                .danmaku_thumbup(
                    unum(&params, "cid"),
                    &text(&params, "dmid"),
                    if flag(&params, "liked") { 1 } else { 2 },
                )
                .await
                .map_err(e)?;
            Ok(json!({ "code": code, "message": message }))
        }
        "danmaku.report" => {
            let session = app.session().await?;
            let (code, message) = session
                .danmaku_report(
                    unum(&params, "cid"),
                    &text(&params, "dmid"),
                    num(&params, "reason").max(1) as u32,
                    &text(&params, "content"),
                )
                .await
                .map_err(e)?;
            Ok(json!({ "code": code, "message": message }))
        }
        "danmaku.post" => {
            let session = app.session().await?;
            let id = player_id(&params);
            let text_value = text(&params, "text");
            let mode = num(&params, "mode").max(1) as u32;
            let (code, message) = session
                .danmaku_post(
                    unum(&params, "cid"),
                    &text(&params, "bvid"),
                    &text_value,
                    num(&params, "progress").max(0) as u64,
                    mode,
                    25,
                    0xFFFFFF,
                    0,
                )
                .await
                .map_err(e)?;
            if code == 0 {
                let guard = app.players.lock().await;
                if let Some(player) = guard.get(&id) {
                    player.send_danmaku(&text_value, mode);
                }
            }
            Ok(json!({ "code": code, "message": message }))
        }

        // ---- up space ------------------------------------------------------
        "user.space" => {
            let session = app.session().await?;
            let mid = num(&params, "mid");
            let (info, videos, relation) = tokio::join!(
                session.space_info(mid),
                session.space_videos(mid, 1, 24),
                session.relation(mid)
            );
            Ok(json!({
                "info": info.map_err(e)?,
                "videos": videos.map_err(e)?,
                "relation": relation.map_err(e)?,
            }))
        }
        "user.videos" => {
            let session = app.session().await?;
            rv(session
                .space_videos(num(&params, "mid"), num(&params, "page").max(1), 24)
                .await
                .map_err(e)?)
        }
        "user.follow" => {
            let session = app.session().await?;
            session
                .relation_modify(num(&params, "mid"), if flag(&params, "follow") { 1 } else { 2 })
                .await
                .map_err(e)?;
            Ok(json!(null))
        }

        // ---- favourites ----------------------------------------------------
        "favorites.folders" => {
            let session = app.session().await?;
            rv(session.fav_folders().await.map_err(e)?)
        }
        "favorites.list" => {
            let session = app.session().await?;
            rv(session
                .fav_list(num(&params, "mediaId"), num(&params, "page").max(1), 20)
                .await
                .map_err(e)?)
        }
        "favorites.items" => {
            let session = app.session().await?;
            let (items, has_more) = session
                .fav_items(num(&params, "mediaId"), num(&params, "page").max(1) as u32)
                .await
                .map_err(e)?;
            Ok(json!({ "items": items, "has_more": has_more }))
        }
        "favorites.create" => {
            let session = app.session().await?;
            let id = session
                .fav_create(&text(&params, "title"), 0)
                .await
                .map_err(e)?;
            Ok(json!(id))
        }
        "favorites.rename" => {
            let session = app.session().await?;
            session
                .fav_folder_rename(num(&params, "mediaId"), &text(&params, "title"))
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "favorites.delete" => {
            let session = app.session().await?;
            session
                .fav_folder_delete(num(&params, "mediaId"))
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "favorites.remove" => {
            let session = app.session().await?;
            let aids: Vec<i64> = params
                .get("aids")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            let removed = session
                .fav_batch_delete(num(&params, "mediaId"), &aids)
                .await
                .map_err(e)?;
            Ok(json!(removed))
        }
        "favorites.clear" => {
            let session = app.session().await?;
            let media_id = num(&params, "mediaId");
            let mut aids = Vec::new();
            for page in 1..=50 {
                let (items, has_more) = session.fav_items(media_id, page).await.map_err(e)?;
                if items.is_empty() {
                    break;
                }
                for video in &items {
                    if let Ok(aid) = i64::try_from(video.aid) {
                        aids.push(aid);
                    }
                }
                if !has_more {
                    break;
                }
            }
            let removed = session
                .fav_batch_delete(media_id, &aids)
                .await
                .map_err(e)?;
            Ok(json!(removed))
        }

        // ---- history -------------------------------------------------------
        "history.list" => {
            let session = app.session().await?;
            let max = num(&params, "max");
            let view_at = num(&params, "viewAt");
            rv(session
                .history(max, view_at, "")
                .await
                .map_err(e)?)
        }
        "history.delete" => {
            let session = app.session().await?;
            session
                .history_delete(&text(&params, "kid"))
                .await
                .map_err(e)?;
            Ok(json!(null))
        }
        "history.clear" => {
            let session = app.session().await?;
            session.history_clear().await.map_err(e)?;
            Ok(json!(null))
        }

        // ---- regions -------------------------------------------------------
        "regions.latest" => {
            let session = app.session().await?;
            rv(session
                .region_latest(
                    num(&params, "rid"),
                    num(&params, "page").max(1),
                    num(&params, "ps").max(1),
                )
                .await
                .map_err(e)?)
        }
        "regions.ranking" => {
            let session = app.session().await?;
            rv(session.ranking(num(&params, "rid")).await.map_err(e)?)
        }

        // ---- PGC -----------------------------------------------------------
        "pgc.timeline" => {
            let session = app.session().await?;
            rv(session.pgc_timeline().await.map_err(e)?)
        }
        "pgc.index" => {
            let session = app.session().await?;
            rv(session
                .pgc_index(
                    num(&params, "seasonType"),
                    num(&params, "page").max(1),
                    20,
                    num(&params, "order"),
                )
                .await
                .map_err(e)?)
        }
        "pgc.rank" => {
            let session = app.session().await?;
            rv(session
                .pgc_rank(num(&params, "seasonType"), 3)
                .await
                .map_err(e)?)
        }
        "pgc.season" => {
            let session = app.session().await?;
            rv(session
                .pgc_season(num(&params, "seasonId"))
                .await
                .map_err(e)?)
        }
        "pgc.playurl" => {
            let session = app.session().await?;
            rv(session
                .pgc_playurl(num(&params, "epId"), unum(&params, "cid"))
                .await
                .map_err(e)?)
        }
        "pgc.playurl_dash" => {
            let session = app.session().await?;
            rv(session
                .pgc_playurl_dash(num(&params, "epId"), unum(&params, "cid"))
                .await
                .map_err(e)?)
        }

        // ---- live ----------------------------------------------------------
        "live.index" => {
            let session = app.session().await?;
            rv(session.live_index().await.map_err(e)?)
        }
        "live.area" => {
            let session = app.session().await?;
            rv(session
                .live_area_rooms(
                    num(&params, "parent"),
                    num(&params, "area"),
                    num(&params, "page").max(1),
                )
                .await
                .map_err(e)?)
        }
        "live.room" => {
            let session = app.session().await?;
            rv(session
                .live_room_info(num(&params, "roomId"))
                .await
                .map_err(e)?)
        }
        "live.play" => {
            let session = app.session().await?;
            let play: LivePlay = session
                .live_play_url_qn(num(&params, "roomId"), num(&params, "qn").max(1) as u32)
                .await
                .map_err(e)?;
            rv(play)
        }

        // ---- dynamic -------------------------------------------------------
        "dynamic.list" => {
            let session = app.session().await?;
            rv(session
                .dynamic_feed(&text(&params, "offset"), &text(&params, "type"))
                .await
                .map_err(e)?)
        }
        "dynamic.like" => {
            let session = app.session().await?;
            let code = session
                .dynamic_thumb(&text(&params, "id"), flag(&params, "liked"))
                .await
                .map_err(e)?;
            Ok(json!(code))
        }

        // ---- messages ------------------------------------------------------
        "messages.unread" => {
            let session = app.session().await?;
            rv(session.msg_unread().await.map_err(e)?)
        }
        "messages.list" => {
            let session = app.session().await?;
            let kind = msg_kind(&text(&params, "kind"));
            rv(session.msg_list(kind).await.map_err(e)?)
        }
        "messages.page" => {
            let session = app.session().await?;
            let kind = msg_kind(&text(&params, "kind"));
            let cursor = params.get("cursor").and_then(|cursor| {
                Some(bili::MsgCursor {
                    id: cursor.get("id")?.as_i64()?,
                    time: cursor.get("time")?.as_i64()?,
                })
            });
            rv(session.msg_page(kind, cursor).await.map_err(e)?)
        }
        "messages.dm.sessions" => {
            let session = app.session().await?;
            rv(session.dm_sessions().await.map_err(e)?)
        }
        "messages.dm.messages" => {
            let session = app.session().await?;
            rv(session
                .dm_messages(num(&params, "talker"))
                .await
                .map_err(e)?)
        }
        "messages.dm.more" => {
            let session = app.session().await?;
            rv(session
                .dm_messages_before(num(&params, "talker"), num(&params, "seqno"))
                .await
                .map_err(e)?)
        }

        // ---- member --------------------------------------------------------
        "member.vip" => {
            let session = app.session().await?;
            let (page, tasks, catalog) = tokio::join!(
                session.vip_privilege_my_page(),
                session.vip_point_tasks(),
                session.vip_privilege_catalog()
            );
            Ok(json!({
                "page": page.map_err(e)?,
                "tasks": tasks.map_err(e)?,
                "catalog": catalog.map_err(e)?,
            }))
        }
        "member.vip.sign" => {
            let session = app.session().await?;
            let code: VipCode = session.vip_task_sign().await.map_err(e)?;
            rv(code)
        }
        "member.vip.privilege" => {
            let session = app.session().await?;
            let code: VipCode = session.vip_privilege_receive().await.map_err(e)?;
            rv(code)
        }
        "member.vip.experience" => {
            let session = app.session().await?;
            let code: VipCode = session.vip_experience_add().await.map_err(e)?;
            rv(code)
        }

        // ---- downloads -----------------------------------------------------
        "downloads.list" => Ok(app.downloads.list()),
        "downloads.create" => {
            let settings = app.settings_snapshot().await;
            app.downloads
                .create(app.session().await?, &params, &settings)
                .await
        }
        "downloads.cancel" => {
            app.downloads.cancel(&text(&params, "id"));
            Ok(json!(null))
        }
        "downloads.remove" => {
            app.downloads.remove(&text(&params, "id"));
            Ok(json!(null))
        }
        "downloads.open" => {
            app.downloads.open_folder(&text(&params, "path"));
            Ok(json!(null))
        }

        // ---- 养鱼 ----------------------------------------------------------
        "fish.state" => {
            let engine = fish_engine(app).await?;
            let settings = app.settings_snapshot().await;
            Ok(json!({
                "snapshot": engine.snapshot(),
                "dashboard": engine.dashboard().ok(),
                "verdicts": engine.verdicts(30).map_err(e)?,
                "errors": engine.errors(20).map_err(e)?,
                "presets": engine_presets(),
                "settings": {
                    "deepseek_key_set": !settings.deepseek_key.trim().is_empty(),
                    "fish_enabled": settings.fish_enabled,
                    "fish_auto_feed": settings.fish_auto_feed,
                    "fish_instruction": settings.fish_instruction,
                    "fish_model": settings.fish_model,
                    "fish_budget_videos": settings.fish_budget_videos,
                    "fish_interval_ms": settings.fish_interval_ms,
                    "fish_sources": settings.fish_sources,
                    "fish_tier": settings.fish_tier,
                    "fish_asr_enabled": settings.fish_asr_enabled,
                    "fish_asr_engine": settings.fish_asr_engine,
                    "fish_asr_model": settings.fish_asr_model,
                    "fish_asr_threads": settings.fish_asr_threads,
                    "fish_model_mirror": settings.fish_model_mirror,
                    "fish_vision_detail": settings.fish_vision_detail,
                    "deepseek_set": !settings.deepseek_key.trim().is_empty(),
                    "step_meta": settings.fish_step_meta,
                    "step_subtitle": settings.fish_step_subtitle,
                    "step_outline": settings.fish_step_outline,
                    "step_comments": settings.fish_step_comments,
                    "step_asr": settings.fish_step_asr,
                    "step_frames": settings.fish_step_frames,
                }
            }))
        }
        "fish.start" => {
            let engine = fish_engine(app).await?;
            engine.refresh_from_settings();
            engine.start(app.session().await?);
            Ok(json!(engine.snapshot()))
        }
        "fish.pause" => {
            let engine = fish_engine(app).await?;
            engine.pause();
            Ok(json!(engine.snapshot()))
        }
        "fish.stop" => {
            let engine = fish_engine(app).await?;
            engine.stop();
            Ok(json!(engine.snapshot()))
        }
        "fish.enqueue" => {
            let session = app.session().await?;
            let items = session.homepage_feed_from(0, &|_, _| {}).await.map_err(e)?;
            let engine = fish_engine(app).await?;
            engine.enqueue(&items);
            Ok(json!({ "queued": items.len(), "snapshot": engine.snapshot() }))
        }
        "fish.clear" => {
            let engine = fish_engine(app).await?;
            let report = engine.clear_queue().map_err(e)?;
            rv(report)
        }
        "fish.retry" => {
            let engine = fish_engine(app).await?;
            let count = engine.retry_errors().map_err(e)?;
            Ok(json!(count))
        }
        "fish.asr.status" => {
            let settings = app.settings_snapshot().await;
            let config = o11a1::fish::watch_config(&settings);
            let text = tokio::task::spawn_blocking(move || {
                o11a1::fishwatch::asr::status_text(&config)
            })
            .await
            .map_err(e)?;
            Ok(json!(text))
        }
        "fish.models" => {
            let settings = app.settings_snapshot().await;
            let root = o11a1::models_dl::root_dir(&settings.fish_asr_model_dir);
            let items = o11a1::models_dl::list(&root);
            Ok(json!({
                "root": root,
                "items": items,
                "running": o11a1::models_dl::running(),
            }))
        }
        "fish.models.download" => {
            let settings = app.settings_snapshot().await;
            let name = text(&params, "name");
            let root = o11a1::models_dl::root_dir(&settings.fish_asr_model_dir);
            let mirror = o11a1::models_dl::Mirror::from_setting(&settings.fish_model_mirror);
            std::thread::spawn(move || {
                let cancel = Arc::new(AtomicBool::new(false));
                let _ = o11a1::models_dl::download(&root, &name, mirror, cancel, &mut |_| {});
            });
            Ok(json!(null))
        }
        "fish.models.cancel" => {
            Ok(json!(o11a1::models_dl::cancel()))
        }
        "fish.models.delete" => {
            let settings = app.settings_snapshot().await;
            let root = o11a1::models_dl::root_dir(&settings.fish_asr_model_dir);
            let freed = o11a1::models_dl::delete(&root, &text(&params, "name")).map_err(e)?;
            Ok(json!(freed))
        }

        // ---- player --------------------------------------------------------
        "window.drag" => {
            let hwnd = params.get("hwnd").and_then(Value::as_i64).unwrap_or(0) as isize;
            if hwnd == 0 {
                Err("window.drag: missing hwnd".to_string())
            } else {
                native_player::drag_window(hwnd);
                Ok(json!(true))
            }
        }
        "player.open" => open_player(app, params).await,
        "player.close" => {
            let id = player_id(&params);
            let mut guard = app.players.lock().await;
            if let Some(mut player) = guard.remove(&id) {
                player.close();
            }
            drop(guard);
            app.emit("player.state", json!({ "id": id, "open": false }));
            Ok(json!(null))
        }
        "player.closeAll" => {
            let mut guard = app.players.lock().await;
            for (_, mut player) in guard.drain() {
                player.close();
            }
            Ok(json!(null))
        }
        "player.status" => {
            if params.get("playerId").is_some() {
                Ok(app.player_snapshot(&player_id(&params)).await)
            } else {
                Ok(json!({ "players": app.player_snapshots().await }))
            }
        }
        "player.command" => player_command(app, params).await,
        "player.rect" => {
            let id = player_id(&params);
            let guard = app.players.lock().await;
            if let Some(player) = guard.get(&id) {
                player.set_rect(
                    num(&params, "x") as i32,
                    num(&params, "y") as i32,
                    unum(&params, "width").max(1) as u32,
                    unum(&params, "height").max(1) as u32,
                );
            }
            Ok(json!(null))
        }
        "player.visible" => {
            let id = player_id(&params);
            let guard = app.players.lock().await;
            if let Some(player) = guard.get(&id) {
                player.set_visible(flag(&params, "visible"));
            }
            Ok(json!(null))
        }

        "window.open.downloads" => {
            let dir = o11a1::paths::root().join("downloads");
            let _ = std::fs::create_dir_all(&dir);
            Ok(json!(dir))
        }

        other => Err(format!("unknown method: {other}")),
    }
}

/// Account shape safe for the renderer: identity only, never the cookie jar or
/// refresh token.
fn public_account(account: Account) -> Value {
    json!({
        "mid": account.mid,
        "uname": account.uname,
        "face": account.face,
        "vip": account.vip,
    })
}

fn msg_kind(kind: &str) -> MsgKind {
    match kind {
        "at" => MsgKind::At,
        "like" => MsgKind::Like,
        _ => MsgKind::Reply,
    }
}

fn engine_presets() -> Value {
    Value::Array(
        o11a1::fish::presets()
            .iter()
            .map(|preset| {
                json!({
                    "id": preset.id,
                    "name": preset.name,
                    "tagline": preset.tagline,
                    "body": preset.body,
                })
            })
            .collect(),
    )
}

/// Once-a-day scheduled feed. It is opt-in (`fish_auto_feed`), only fires in
/// the configured local minute, and records the local day so a restart after
/// the minute cannot feed twice.
async fn auto_feed_tick(app: &Arc<App>) -> Result<()> {
    let settings = app.settings.lock().await.clone();
    if !settings.fish_enabled || !settings.fish_auto_feed {
        return Ok(());
    }
    let (now, offset) = o11a1::fish::local_now();
    let (hour, minute) = o11a1::fish::local_hm(now, offset);
    let target = settings.fish_feed_time.trim();
    let target = if target.is_empty() { "20:00" } else { target };
    let (target_hour, target_minute) = target
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("fish_feed_time 不是 HH:MM"))?;
    if hour != target_hour || minute != target_minute {
        return Ok(());
    }
    let day = o11a1::fish::day_key(now, offset);
    let marker = o11a1::paths::root().join("fish_auto.json");
    let already = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("day").and_then(Value::as_i64));
    if already == Some(day) {
        return Ok(());
    }
    if settings.deepseek_key.trim().is_empty() {
        return Ok(());
    }
    let session = app.session().await.map_err(anyhow::Error::msg)?;
    let items = session
        .homepage_feed_from(0, &|_, _| {})
        .await
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let engine = fish_engine(app).await.map_err(anyhow::Error::msg)?;
    engine.refresh_from_settings();
    engine.enqueue(&items);
    engine.start(session);
    if let Ok(text) = serde_json::to_string_pretty(&json!({ "day": day, "at": now })) {
        let _ = std::fs::write(marker, text);
    }
    Ok(())
}

async fn fish_engine(app: &Arc<App>) -> Rpc<Arc<FishEngine>> {
    let mut guard = app.fish.lock().await;
    if let Some(engine) = guard.as_ref() {
        return Ok(engine.clone());
    }
    let engine = FishEngine::open().map_err(e)?;
    *guard = Some(engine.clone());
    Ok(engine)
}

async fn open_player(app: &Arc<App>, params: Value) -> Rpc<Value> {
    let id = player_id(&params);
    {
        let guard = app.players.lock().await;
        if !guard.contains_key(&id) && guard.len() >= 3 {
            return Err("最多同时打开 3 个播放器窗口".to_string());
        }
    }
    let settings = app.settings_snapshot().await;
    let host = params.get("host").map(|host| PlayerHost {
        parent: host.get("parent").and_then(Value::as_i64).unwrap_or(0) as isize,
        x: host.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
        y: host.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
        width: host.get("width").and_then(Value::as_u64).unwrap_or(960) as u32,
        height: host.get("height").and_then(Value::as_u64).unwrap_or(540) as u32,
    });
    let quality = opt_i64(&params, "quality")
        .or_else(|| (settings.player_quality > 0).then_some(settings.player_quality));
    let config = PlayerConfig {
        bvid: Some(text(&params, "bvid")).filter(|value| !value.is_empty()),
        cid: params.get("cid").and_then(Value::as_u64),
        direct_url: Some(text(&params, "url")).filter(|value| !value.is_empty()),
        direct_audio_url: Some(text(&params, "audioUrl")).filter(|value| !value.is_empty()),
        title: text(&params, "title"),
        start_ms: unum(&params, "startMs"),
        quality_qn: quality,
        quality_codecid: (settings.codec_pref > 0).then_some(settings.codec_pref as i64),
        audio_index: params.get("audioIndex").and_then(Value::as_u64).map(|v| v as usize),
        host,
    };
    let player = tokio::task::spawn_blocking(move || Player::open(config))
        .await
        .map_err(e)?
        .map_err(e)?;
    if settings.interp_mode > 0 {
        player.set_interp(InterpMode::from_u8(settings.interp_mode as u8));
    }
    if settings.player_enhance > 0 {
        player.set_enhance(EnhanceMode::from_u8(settings.player_enhance as u8));
    }
    let default_speed = settings.player_speed as f64 / 1000.0;
    if (default_speed - 1.0).abs() > 0.001 {
        player.set_speed(default_speed.clamp(0.5, 2.0));
    }
    let duration = player.duration_ms();
    let title = text(&params, "title");
    {
        let mut guard = app.players.lock().await;
        if let Some(mut previous) = guard.remove(&id) {
            previous.close();
        }
        guard.insert(id.clone(), player);
    }
    app.emit(
        "player.opened",
        json!({ "id": id, "duration": duration, "title": title }),
    );
    Ok(json!({ "id": id, "duration": duration }))
}

async fn player_command(app: &Arc<App>, params: Value) -> Rpc<Value> {
    let id = player_id(&params);
    let command = text(&params, "cmd");
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let guard = app.players.lock().await;
    let Some(player) = guard.get(&id) else {
        return Err("播放器未打开".to_string());
    };
    match command.as_str() {
        "play" => player.play(),
        "pause" => player.pause(),
        "toggle" => player.toggle_play(),
        "seek" => player.seek(value.as_u64().unwrap_or(0)),
        "volume" => player.set_volume(value.as_u64().unwrap_or(80) as u8),
        "speed" => player.set_speed(value.as_f64().unwrap_or(1.0)),
        "interp" => player.set_interp(InterpMode::from_u8(value.as_u64().unwrap_or(0) as u8)),
        "enhance" => player.set_enhance(EnhanceMode::from_u8(value.as_u64().unwrap_or(0) as u8)),
        "danmaku" => player.set_danmaku_visible(value.as_bool().unwrap_or(true)),
        "aspect" => player.set_aspect(native_player::AspectMode::from_u8(
            value.as_u64().unwrap_or(0) as u8,
        )),
        "topmost" => player.set_always_on_top(value.as_bool().unwrap_or(false)),
        "audio_only" => player.set_audio_only(value.as_bool().unwrap_or(false)),
        "brightness" => player.set_brightness(value.as_f64().unwrap_or(1.0) as f32),
        "screenshot" => player.screenshot(),
        "mini" => player.toggle_mini(),
        "speed_up" => player.speed_up(),
        "speed_down" => player.speed_down(),
        "preview" => player.set_preview(value.as_u64()),
        other => return Err(format!("unknown player command: {other}")),
    }
    drop(guard);
    Ok(app.player_snapshot(&id).await)
}

fn search_history_path() -> PathBuf {
    o11a1::paths::root().join("search_history.json")
}

fn load_search_history() -> Vec<String> {
    std::fs::read_to_string(search_history_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

fn save_search_history(items: &[String]) -> Result<()> {
    let data = serde_json::to_string_pretty(items)?;
    std::fs::write(search_history_path(), data)?;
    Ok(())
}
