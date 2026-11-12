//!  直播：分区索引、房间信息与拉流地址。

use super::*;
use serde::Serialize;

const LIVE_INDEX_URL: &str = "https://api.live.bilibili.com/xlive/web-interface/v1/index/getList";

/// The legacy 分区 list. `room/v1/area/getList` names its areas; this one lists
/// the rooms (`data` is a bare array of rooms, not `{ list: [] }`).
const LIVE_AREA_LEGACY_URL: &str = "https://api.live.bilibili.com/room/v1/area/getRoomList";

const LIVE_AREA_LEGACY_PATH: &str = "room/v1/area/getRoomList";

const LIVE_AREA_URL: &str = "https://api.live.bilibili.com/xlive/web-interface/v1/second/getList";

const LIVE_AREA_SIGNED_PATH: &str = "second/getList";

const LIVE_ROOM_URL: &str = "https://api.live.bilibili.com/room/v1/Room/get_info";

const LIVE_PLAY_URL: &str =
    "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo";

const LIVE_REFERER: &str = "https://live.bilibili.com/";

/// Rooms kept per module / area page. The module rows carry 10 each; the area
/// endpoint's own page size is 30 (`page_size` is honoured when present).
const LIVE_MODULE_MAX: usize = 10;

/// The live relay's own HTTP client (see [`Session::open_stream`]).
fn live_stream_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    headers.insert(REFERER, HeaderValue::from_static(LIVE_REFERER));
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_nodelay(true)
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

const LIVE_AREA_PAGE: usize = 30;

/// One live room, as the index cards and the player need it. `status` in the
/// index payload is a bool (`live_status` appears in other endpoints), so both
/// spellings land in [`LiveRoom::live_status`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveRoom {
    pub room_id: i64,
    pub uid: i64,
    pub title: String,
    pub uname: String,
    pub online: u64,
    pub cover: String,
    pub area_name: String,
    pub parent_area_name: String,
    pub live_status: i64,
}

/// One 推荐 module of the live homepage (关注 / 推荐直播 / 手游 / …).
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveModule {
    pub title: String,
    pub rooms: Vec<LiveRoom>,
}

/// One 分区 entrance (`area_entrance_v2`). `id` is the `area_v2_id` the
/// second-level list wants; `parent_id` is the top-level area.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveArea {
    pub id: i64,
    pub parent_id: i64,
    pub title: String,
}

/// The live homepage aggregate, parsed.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveIndex {
    pub modules: Vec<LiveModule>,
    pub areas: Vec<LiveArea>,
}

/// One page of a 分区's rooms. `code` is the API's own code (`0` = ok), `source`
/// the endpoint that answered, `elapsed_ms` the wall time of the whole request
/// (both attempts included). A non-zero code with an empty `rooms` is the
/// documented risk-control case.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveAreaPage {
    pub rooms: Vec<LiveRoom>,
    pub has_more: bool,
    pub code: i64,
    /// `room/v1/area/getRoomList` (normal) or `second/getList` (the signed
    /// fallback this client only reaches when the legacy endpoint refuses).
    pub source: &'static str,
    pub signed: bool,
    pub elapsed_ms: u128,
}

/// `room/v1/Room/get_info`'s fields the player uses.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveRoomInfo {
    pub room_id: i64,
    pub title: String,
    pub live_status: i64,
    pub online: u64,
    pub area_name: String,
    pub is_portrait: bool,
    /// The room's own 头图 (`keyframe`, else `user_cover`). The player shows it
    /// while the stream is still buffering.
    pub cover: String,
}

/// The playable URLs for one room, split by delivery so the player can order
/// them itself: FLV (`http_stream`), HLS-fmp4 and HLS-ts (`http_hls`). `hls`
/// keeps the old "ts first" choice for callers that only have one slot.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LivePlay {
    pub hls: Option<String>,
    pub hls_fmp4: Option<String>,
    pub hls_ts: Option<String>,
    pub flv: Option<String>,
    pub live_status: i64,
    pub locked: bool,
}

fn live_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn live_i64(value: &serde_json::Value, key: &str) -> i64 {
    match value.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn live_u64(value: &serde_json::Value, key: &str) -> u64 {
    match value.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// `http://` → `https://` and a size suffix for the live covers (they are
/// served without one, so the full-size JPEG would be the whole card).
fn live_pic(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return String::new();
    }
    let url = if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    };
    if url.contains('@') {
        url
    } else {
        format!("{url}@320w_180h_1c.webp")
    }
}

fn live_room_from(value: &serde_json::Value) -> Option<LiveRoom> {
    let room_id = live_i64(value, "roomid");
    if room_id <= 0 {
        return None;
    }
    let mut live_status = live_i64(value, "live_status");
    if live_status == 0 {
        live_status = match value.get("status") {
            Some(serde_json::Value::Bool(true)) => 1,
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
    }
    Some(LiveRoom {
        room_id,
        uid: live_i64(value, "uid"),
        title: live_str(value, "title"),
        uname: live_str(value, "uname"),
        online: live_u64(value, "online"),
        cover: live_pic(&live_str(value, "cover")),
        area_name: live_str(value, "area_v2_name"),
        parent_area_name: live_str(value, "area_v2_parent_name"),
        live_status,
    })
}

/// Parse one room of a 分区 list. `room/v1/area/getRoomList` omits `live_status`,
/// and every row it returns is a room it lists as live (checked against
/// `Room/get_info`: even rows whose `online` counter reads 0 answer
/// `live_status: 1`), so the 直播中 pill stays truthful on this path.
fn live_area_room_from(value: &serde_json::Value) -> Option<LiveRoom> {
    let mut room = live_room_from(value)?;
    if room.live_status == 0 && value.get("live_status").is_none() {
        room.live_status = 1;
    }
    Some(room)
}

/// The room array of a 分区 response: `data` is a bare array on the legacy
/// endpoint and `{ list: [...] }` on `second/getList`.
fn live_area_list(body: &serde_json::Value) -> Vec<serde_json::Value> {
    match body.get("data") {
        Some(serde_json::Value::Array(list)) => list.clone(),
        Some(data) => data
            .get("list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

impl Session {
    /// Open one streaming GET for the live relay. The caller owns the body and
    /// copies it to a local socket.
    ///
    /// A **separate client**: the shared one carries a 25 s total timeout, which
    /// is right for a JSON call and fatal for a live body (measured: the relay
    /// died at ~25 s with `operation timed out`). This client has no total
    /// timeout, so the stream lives as long as the room.
    pub async fn open_stream(&self, url: &str, referer: &str) -> Result<reqwest::Response> {
        let response = live_stream_client()?
            .get(url)
            .header(REFERER, referer)
            .send()
            .await?
            .error_for_status()?;
        Ok(response)
    }

    async fn live_json(&self, url: &str, params: &[(&str, String)]) -> Result<serde_json::Value> {
        self.get_query_json::<serde_json::Value>(url, params, Some(LIVE_REFERER))
            .await
    }

    /// The live homepage: the decorated module rows (each with its rooms) and
    /// the 24 area entrances.
    pub async fn live_index(&self) -> Result<LiveIndex> {
        let params = [("platform", "web".to_string())];
        let body = self.live_json(LIVE_INDEX_URL, &params.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>()).await?;
        let code = live_i64(&body, "code");
        if code != 0 {
            bail!("直播首页加载失败（code={code}）");
        }
        let data = body.get("data").cloned().unwrap_or_default();

        let mut modules: Vec<LiveModule> = Vec::new();
        if let Some(list) = data.get("room_list").and_then(|v| v.as_array()) {
            for module in list {
                let title = module
                    .get("module_info")
                    .map(|info| live_str(info, "title"))
                    .unwrap_or_default();
                let rooms: Vec<LiveRoom> = module
                    .get("list")
                    .and_then(|v| v.as_array())
                    .map(|rooms| {
                        rooms
                            .iter()
                            .filter_map(live_room_from)
                            .take(LIVE_MODULE_MAX)
                            .collect()
                    })
                    .unwrap_or_default();
                if !title.is_empty() && !rooms.is_empty() {
                    modules.push(LiveModule { title, rooms });
                }
            }
        }

        let mut areas: Vec<LiveArea> = Vec::new();
        if let Some(list) = data
            .get("area_entrance_v2")
            .and_then(|v| v.get("list"))
            .and_then(|v| v.as_array())
        {
            for area in list {
                // `id` and `area_v2_id` are the same number; prefer the former.
                let id = live_i64(area, "id").max(live_i64(area, "area_v2_id"));
                let title = live_str(area, "title");
                if id <= 0 || title.is_empty() {
                    continue;
                }
                areas.push(LiveArea {
                    id,
                    parent_id: live_i64(area, "area_v2_parent_id"),
                    title,
                });
            }
        }
        if modules.is_empty() && areas.is_empty() {
            bail!("直播首页没有返回内容");
        }
        crate::debug_log(&format!(
            "live index modules={} rooms={} areas={}",
            modules.len(),
            modules.iter().map(|m| m.rooms.len()).sum::<usize>(),
            areas.len()
        ));
        Ok(LiveIndex { modules, areas })
    }

    /// One page of a 分区's rooms.
    ///
    /// Order (measured 2026-10-28, `parent_area_id=2&area_id=86`):
    /// 1. `room/v1/area/getRoomList` — `code: 0`, 30 rooms, ~0.65–1.1 s. This is
    ///    the path the grid uses; it needs neither a cookie nor a signature.
    /// 2. `xlive/.../second/getList`, WBI-signed — only reached when (1) refuses.
    ///    That endpoint answers `-352` for this account/IP even with keys proven
    ///    good (`wbi/index/top/feed/rcmd` → `code: 0`), so the refusal is
    ///    reported with its own `code` and `source` instead of being hidden.
    ///
    /// `has_more` is derived from a full page (`>= 30`): the legacy payload has
    /// no `has_more` field (measured: page 20 of 英雄联盟 returns 6 rooms, the
    /// list's end).
    pub async fn live_area_rooms(
        &self,
        parent_area_id: i64,
        area_id: i64,
        page: i64,
    ) -> Result<LiveAreaPage> {
        let started = Instant::now();
        let page = page.max(1);
        let params: Vec<(&str, String)> = vec![
            ("platform", "web".to_string()),
            ("parent_area_id", parent_area_id.to_string()),
            ("area_id", area_id.to_string()),
            ("sort_type", "online".to_string()),
            ("page", page.to_string()),
            ("page_size", LIVE_AREA_PAGE.to_string()),
        ];
        let mut code = 0i64;
        let mut source = "";
        let mut signed = false;
        let mut list: Vec<serde_json::Value> = Vec::new();
        let mut has_more_field = false;

        match self.live_json(LIVE_AREA_LEGACY_URL, &params).await {
            Ok(body) => {
                code = live_i64(&body, "code");
                if code == 0 {
                    source = LIVE_AREA_LEGACY_PATH;
                    list = live_area_list(&body);
                } else {
                    crate::debug_log(&format!(
                        "live area legacy refused parent={parent_area_id} area={area_id} page={page} code={code}"
                    ));
                }
            }
            Err(err) => {
                crate::debug_log(&format!(
                    "live area legacy err parent={parent_area_id} area={area_id} page={page} err={err:#}"
                ));
            }
        }

        if source.is_empty() {
            let owned: Vec<(String, String)> =
                params.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
            match self
                .wbi_call_referer::<serde_json::Value>(
                    LIVE_AREA_URL,
                    &owned,
                    true,
                    Some(LIVE_REFERER),
                )
                .await
            {
                Ok((_, signed_body)) => {
                    signed = true;
                    source = LIVE_AREA_SIGNED_PATH;
                    code = live_i64(&signed_body, "code");
                    has_more_field = signed_body
                        .get("data")
                        .and_then(|data| data.get("has_more"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                        != 0;
                    list = live_area_list(&signed_body);
                }
                Err(err) => {
                    source = LIVE_AREA_SIGNED_PATH;
                    if code == 0 {
                        code = -1;
                    }
                    crate::debug_log(&format!(
                        "live area signed err parent={parent_area_id} area={area_id} page={page} err={err:#}"
                    ));
                }
            }
        }

        let rooms: Vec<LiveRoom> = list
            .iter()
            .filter_map(live_area_room_from)
            .take(LIVE_AREA_PAGE)
            .collect();
        let has_more =
            code == 0 && (has_more_field || rooms.len() >= LIVE_AREA_PAGE);
        let elapsed_ms = started.elapsed().as_millis();
        crate::debug_log(&format!(
            "live area parent={parent_area_id} area={area_id} page={page} n={} ms={elapsed_ms} source={source} code={code} signed={}",
            rooms.len(),
            u8::from(signed)
        ));
        Ok(LiveAreaPage {
            rooms,
            has_more,
            code,
            source,
            signed,
            elapsed_ms,
        })
    }

    /// One room's live status and viewer count (`uname` is not in this payload).
    pub async fn live_room_info(&self, room_id: i64) -> Result<LiveRoomInfo> {
        let params = [("room_id", room_id.to_string())];
        let body = self.live_json(LIVE_ROOM_URL, &params).await?;
        let code = live_i64(&body, "code");
        if code != 0 {
            bail!("直播间信息加载失败（code={code}）");
        }
        let data = body.get("data").cloned().unwrap_or_default();
        let cover = {
            let keyframe = live_str(&data, "keyframe");
            if keyframe.trim().is_empty() {
                live_str(&data, "user_cover")
            } else {
                keyframe
            }
        };
        Ok(LiveRoomInfo {
            room_id: live_i64(&data, "room_id").max(room_id),
            title: live_str(&data, "title"),
            live_status: live_i64(&data, "live_status"),
            online: live_i64(&data, "online").max(0) as u64,
            area_name: live_str(&data, "area_name"),
            is_portrait: data
                .get("is_portrait")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            cover,
        })
    }

    /// The playable URLs of one room: the `http_hls` stream first (ts then
    /// fmp4), `http_stream` (FLV) as the fallback. `host + base_url + extra` is
    /// the complete, signed URL — the three parts are never reassembled later.
    pub async fn live_play_url(&self, room_id: i64) -> Result<LivePlay> {
        self.live_play_url_qn(room_id, 10000).await
    }

    /// Same as [`Session::live_play_url`] with an explicit 画质 `qn`
    /// (10000 原画 / 400 蓝光 / 250 超清 / 150 高清 / 80 流畅).
    pub async fn live_play_url_qn(&self, room_id: i64, qn: u32) -> Result<LivePlay> {
        let params = [
            ("room_id", room_id.to_string()),
            ("protocol", "0,1".to_string()),
            ("format", "0,1,2".to_string()),
            ("codec", "0,1".to_string()),
            ("qn", qn.to_string()),
            ("platform", "web".to_string()),
            ("ptype", "8".to_string()),
        ];
        let body = self.live_json(LIVE_PLAY_URL, &params).await?;
        let code = live_i64(&body, "code");
        if code != 0 {
            bail!("直播地址加载失败（code={code}）");
        }
        let data = body.get("data").cloned().unwrap_or_default();
        let live_status = live_i64(&data, "live_status");
        let locked = data
            .get("is_locked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut hls: Option<String> = None;
        let mut hls_fmp4: Option<String> = None;
        let mut flv: Option<String> = None;
        if let Some(streams) = data
            .get("playurl_info")
            .and_then(|v| v.get("playurl"))
            .and_then(|v| v.get("stream"))
            .and_then(|v| v.as_array())
        {
            for stream in streams {
                let protocol = live_str(stream, "protocol_name");
                let Some(formats) = stream.get("format").and_then(|v| v.as_array()) else {
                    continue;
                };
                for format in formats {
                    let format_name = live_str(format, "format_name");
                    let Some(codecs) = format.get("codec").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    for codec in codecs {
                        let Some(url_info) = codec
                            .get("url_info")
                            .and_then(|v| v.as_array())
                            .and_then(|list| list.first())
                        else {
                            continue;
                        };
                        let url = format!(
                            "{}{}{}",
                            live_str(url_info, "host"),
                            live_str(codec, "base_url"),
                            live_str(url_info, "extra")
                        );
                        if url.len() < 12 {
                            continue;
                        }
                        if protocol == "http_hls" {
                            // fmp4 first: the ts rendition of this CDN is the
                            // one that stalls (measured: 0 bytes in 6 s while
                            // fmp4 answers 200 in ~0.8 s).
                            if format_name == "fmp4" && hls_fmp4.is_none() {
                                hls_fmp4 = Some(url);
                            } else if hls.is_none() {
                                hls = Some(url);
                            }
                        } else if flv.is_none() {
                            flv = Some(url);
                        }
                    }
                }
            }
        }
        let hls_ts = hls;
        let hls = hls_ts.clone().or_else(|| hls_fmp4.clone());
        if hls.is_none() && flv.is_none() {
            bail!("这个直播间没有可用的播放地址（live_status={live_status}）");
        }
        crate::debug_log(&format!(
            "live play room={room_id} status={live_status} locked={} hls_ts={} hls_fmp4={} flv={}",
            u8::from(locked),
            u8::from(hls_ts.is_some()),
            u8::from(hls_fmp4.is_some()),
            u8::from(flv.is_some())
        ));
        Ok(LivePlay {
            hls,
            hls_fmp4,
            hls_ts,
            flv,
            live_status,
            locked,
        })
    }
}

#[cfg(test)]
mod live_tests {
    use super::{live_area_list, live_area_room_from, live_pic, live_room_from};
    use serde_json::json;

    #[test]
    fn room_parses_bool_status_and_prefixed_ids() {
        let value = json!({
            "roomid": "22116362", "uid": 531381429, "title": "五行气功跟练",
            "uname": "武当袁师懋", "online": "1091", "status": true,
            "cover": "https://i0.hdslb.com/bfs/live/a.jpg",
            "area_v2_name": "运动体育", "area_v2_parent_name": "生活"
        });
        let room = live_room_from(&value).expect("room");
        assert_eq!(room.room_id, 22116362);
        assert_eq!(room.uid, 531381429);
        assert_eq!(room.online, 1091);
        assert_eq!(room.live_status, 1);
        assert_eq!(room.area_name, "运动体育");
        assert!(room.cover.ends_with("@320w_180h_1c.webp"));
    }

    #[test]
    fn room_without_id_is_dropped_and_http_is_upgraded() {
        assert!(live_room_from(&json!({ "roomid": 0 })).is_none());
        assert_eq!(
            live_pic("http://i0.hdslb.com/bfs/live/x.png"),
            "https://i0.hdslb.com/bfs/live/x.png@320w_180h_1c.webp"
        );
    }

    /// Captured from `room/v1/area/getRoomList?parent_area_id=2&area_id=86`:
    /// `data` is a bare array, and the row has no `live_status` — yet
    /// `Room/get_info` answers `live_status: 1` for it, so the card keeps its
    /// 直播中 pill.
    #[test]
    fn legacy_area_page_is_a_bare_array_without_live_status() {
        let body = json!({
            "code": 0, "msg": "success",
            "data": [
                {
                    "roomid": 545068, "uid": 8739477, "title": "德云色 上分如喝汤",
                    "uname": "老实憨厚的笑笑", "online": 389934,
                    "cover": "https://i0.hdslb.com/bfs/live/a.jpg",
                    "area_v2_id": 86, "area_v2_name": "英雄联盟",
                    "area_v2_parent_id": 2, "area_v2_parent_name": "网游"
                },
                { "roomid": 0 }
            ]
        });
        let list = live_area_list(&body);
        assert_eq!(list.len(), 2);
        let room = live_area_room_from(&list[0]).expect("room");
        assert_eq!(room.room_id, 545068);
        assert_eq!(room.online, 389934);
        assert_eq!(room.area_name, "英雄联盟");
        assert_eq!(room.parent_area_name, "网游");
        assert_eq!(room.live_status, 1);
        assert!(room.cover.ends_with("@320w_180h_1c.webp"));
    }

    /// `second/getList` nests the array and carries a real `live_status`; a
    /// finished room must stay 0 (no invented 直播中).
    #[test]
    fn nested_area_page_keeps_its_live_status() {
        let body = json!({
            "code": 0,
            "data": { "list": [{ "roomid": 7, "title": "t", "live_status": 0 }], "has_more": 1 }
        });
        let list = live_area_list(&body);
        let room = live_area_room_from(&list[0]).expect("room");
        assert_eq!(room.live_status, 0);
    }
}
