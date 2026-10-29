//! Bilibili web endpoints used for QR-code login and account info.
//!
//! One [`Session`] wraps a single `reqwest::Client` backed by an explicit
//! shared cookie [`Jar`], a desktop Chrome `User-Agent` and a
//! `Referer: https://www.bilibili.com/` default header.

use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{
    HeaderMap, HeaderValue, CONTENT_ENCODING, CONTENT_TYPE, RANGE, REFERER, SET_COOKIE, USER_AGENT,
};
use reqwest::Url;

/// A normal desktop Chrome user agent. `pub(crate)` so the offline-download
/// worker can attach the exact same UA to its ffmpeg inputs.
pub(crate) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

const QR_GEN_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";
const QR_POLL_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";
const NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";
const RELATION_URL: &str = "https://api.bilibili.com/x/relation/stat";
const MYINFO_URL: &str = "https://api.bilibili.com/x/space/myinfo";
const SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";
const HOME_URL: &str = "https://www.bilibili.com/";
const TICKET_URL: &str =
    "https://api.bilibili.com/bapis/bilibili.api.ticket.v1.Ticket/GenWebTicket";
/// HMAC key Bilibili's web client uses to sign `GenWebTicket` requests.
const TICKET_HMAC_KEY: &str = "XgwSnGZ1p";
const FEED_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/index/top/feed/rcmd";
const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/search/type";
const VIEW_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/view";
const PLAYURL_URL: &str = "https://api.bilibili.com/x/player/wbi/playurl";
/// 大会员卡券/特权状态 (`GET`, SESSDATA).
const VIP_PRIVILEGE_URL: &str = "https://api.bilibili.com/x/vip/privilege/my";
/// 大会员每日经验 (`POST` form `csrf`).
const VIP_EXPERIENCE_URL: &str = "https://api.bilibili.com/x/vip/experience/add";
/// 兑换大会员卡券 (`POST` form `type` + `csrf`).
const VIP_RECEIVE_URL: &str = "https://api.bilibili.com/x/vip/privilege/receive";
/// 大积分签到/会员任务 (`POST` form `csrf`).
const PGC_SIGN_URL: &str = "https://api.bilibili.com/pgc/activity/score/task/sign";
const DM_SEG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/seg.so";
/// Available history dates for one `cid` (`?type=1&oid=<cid>&month=YYYY-MM`).
const DM_HISTORY_INDEX_URL: &str = "https://api.bilibili.com/x/v2/dm/history/index";
/// One day's historical danmaku as XML (`?type=1&oid=<cid>&date=YYYY-MM-DD`).
const DM_HISTORY_URL: &str = "https://api.bilibili.com/x/v2/dm/history";
/// One day's historical danmaku as protobuf (same `DmSegMobileReply` shape as
/// [`DM_SEG_URL`]). Used as a fallback because the XML endpoint above now answers
/// a `state=2` placeholder without any `<d>` entries.
const DM_HISTORY_SEG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/history/seg.so";
/// Send a video danmaku. WBI-signed; the message is in the form body.
const DM_POST_URL: &str = "https://api.bilibili.com/x/v2/dm/post";
/// Report one danmaku.
const DM_REPORT_URL: &str = "https://api.bilibili.com/x/dm/report/add";
/// Like (`op=1`) / un-like (`op=2`) one danmaku.
const DM_THUMBUP_URL: &str = "https://api.bilibili.com/x/v2/dm/thumbup/add";
/// Server-side AI danmaku filter level. POST form with `ai_switch` + `csrf`.
const DM_AI_CONFIG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/config";
const LIKE_HAS_URL: &str = "https://api.bilibili.com/x/web-interface/archive/has/like";
const LIKE_URL: &str = "https://api.bilibili.com/x/web-interface/archive/like";
/// Playback heartbeat: the call that writes the official 历史记录 entry.
const HEARTBEAT_URL: &str = "https://api.bilibili.com/x/click-interface/web/heartbeat";
const COIN_STATE_URL: &str = "https://api.bilibili.com/x/web-interface/archive/coins";
const COIN_ADD_URL: &str = "https://api.bilibili.com/x/web-interface/coin/add";
const FAV_FOLDERS_URL: &str = "https://api.bilibili.com/x/v3/fav/folder/created/list-all";
const FAV_FAVOURED_URL: &str = "https://api.bilibili.com/x/v2/fav/video/favoured";
const FAV_DEAL_URL: &str = "https://api.bilibili.com/x/v3/fav/resource/deal";
const FAV_LIST_URL: &str = "https://api.bilibili.com/x/v3/fav/resource/list";
/// 收藏推荐: one video's related list (`data[]`, archive-shaped entries).
const RELATED_URL: &str = "https://api.bilibili.com/x/web-interface/archive/related";
const HISTORY_URL: &str = "https://api.bilibili.com/x/web-interface/history/cursor";
const HISTORY_DELETE_URL: &str = "https://api.bilibili.com/x/v2/history/delete";
const HISTORY_CLEAR_URL: &str = "https://api.bilibili.com/x/v2/history/clear";
const TOVIEW_URL: &str = "https://api.bilibili.com/x/v2/history/toview";
const TOVIEW_ADD_URL: &str = "https://api.bilibili.com/x/v2/history/toview/add";
const TOVIEW_DEL_URL: &str = "https://api.bilibili.com/x/v2/history/toview/del";
const REGION_URL: &str = "https://api.bilibili.com/x/web-interface/dynamic/region";
const REGION_NEWLIST_URL: &str = "https://api.bilibili.com/x/web-interface/newlist";
// -- 番剧 / PGC ---------------------------------------------------------------
/// 每日放送 (7-day timeline). `types=1,4,2,3,5,7` covers 番剧/国创/电影/纪录片/
/// 电视剧/综艺; `before=3&after=3` yields exactly 7 days around today.
const PGC_TIMELINE_URL: &str = "https://api.bilibili.com/pgc/web/timeline";
/// 分区索引, one page per request (`page`/`pagesize` 1-based).
///
/// `order` is the sort, and every value below was checked against the live
/// endpoint (same `season_type`, same page, 2026-10) by reading the returned
/// `order` strings down a whole page and across the page boundary:
///
/// | order | `order` field 文案        | sort                |
/// |-------|---------------------------|---------------------|
/// | 0     | `8月14日更新` / `今天17:59更新` | 更新时间 (newest first) |
/// | 1     | *(empty)*                 | 弹幕数 (from `pgc/view/web/season` stats) |
/// | 2     | `1.6亿次播放`              | 播放量               |
/// | 3     | `553.2万追番`              | 追番人数 (the site's default) |
/// | 4     | `9.9分`                    | 评分                 |
/// | 5     | `6月26日开播`              | 开播时间             |
const PGC_INDEX_URL: &str = "https://api.bilibili.com/pgc/season/index/result";
/// 排行榜 (99 rows, no paging).
const PGC_RANK_URL: &str = "https://api.bilibili.com/pgc/web/rank/list";
/// One season's detail: episodes + section groups + evaluate/rating/areas.
const PGC_SEASON_URL: &str = "https://api.bilibili.com/pgc/view/web/season";
/// PGC playurl. Only used as a REGION GATE (its non-zero code names the
/// reason); playback itself goes through [`PLAYURL_URL`]'s unsigned fallback.
const PGC_PLAYURL_URL: &str = "https://api.bilibili.com/pgc/player/web/playurl";
/// Unsigned playurl. PGC episodes have a real `bvid`+`cid`, so the plain
/// `x/player/playurl` answers for them where the WBI variant is risk-controlled.
const PLAYURL_PLAIN_URL: &str = "https://api.bilibili.com/x/player/playurl";
const RANKING_URL: &str = "https://api.bilibili.com/x/web-interface/ranking/v2";
const SUGGEST_URL: &str = "https://s.search.bilibili.com/main/suggest";
const HOTWORD_URL: &str = "https://s.search.bilibili.com/main/hotword";
const SQUARE_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/search/square";
/// UP主空间 info (WBI). Some accounts answer `-352` unless the request carries
/// the space page as `Referer`; [`Session::space_info`] retries once with it.
const SPACE_INFO_URL: &str = "https://api.bilibili.com/x/space/wbi/acc/info";
/// UP主投稿列表 (WBI).
const SPACE_ARC_URL: &str = "https://api.bilibili.com/x/space/wbi/arc/search";
/// UP主 card header (`data.card` + `data.archive_count`). Plain GET, no WBI.
const UP_CARD_URL: &str = "https://api.bilibili.com/x/web-interface/card";
/// Follow state for one user (`data.attribute`), distinct from the follower
/// counts endpoint `x/relation/stat`.
const RELATION_STATE_URL: &str = "https://api.bilibili.com/x/relation";
/// Follow / unfollow write.
const RELATION_MODIFY_URL: &str = "https://api.bilibili.com/x/relation/modify";
/// Logout. The CSRF field is named `biliCSRF`, not `csrf`.
const LOGOUT_URL: &str = "https://passport.bilibili.com/login/exit/v2";
/// `ranking/v2` is rejected (risk control `-352`) unless the request carries the
/// ranking page as `Referer`; the shared client's default `bilibili.com/` fails.
const RANK_REFERER: &str = "https://www.bilibili.com/v/popular/rank/all";

// -- 视频评论 (comments) ------------------------------------------------------
/// 评论主列表 (WBI-signed): `oid=<aid>&type=1&mode=<3 热门 | 2 时间>`, with a
/// `pagination_str={"offset":"<cursor>"}` cursor (empty on the first page).
const COMMENT_MAIN_URL: &str = "https://api.bilibili.com/x/v2/reply/wbi/main";
/// 一条评论的楼中楼 (dialog): `oid=<aid>&type=1&root=<rpid>&pn=<page>&ps=20`.
const COMMENT_REPLY_URL: &str = "https://api.bilibili.com/x/v2/reply/reply";

// -- 动态流 / 消息中心 / 私信 / 关注粉丝 ---------------------------------------
/// 动态流 (web `feed/all`): `data.items[]` + `data.offset` / `data.has_more`.
const DYNAMIC_FEED_URL: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/all";
/// 动态点赞 write. POST form `dyn_id_str` + `up` + `csrf`.
const DYNAMIC_THUMB_URL: &str = "https://api.bilibili.com/x/dynamic/feed/dyn/thumb";
/// Per-category unread counts for the 消息 badge.
const MSG_UNREAD_URL: &str = "https://api.bilibili.com/x/msgfeed/unread";
/// 回复我的 / @我的 / 收到的赞 lists (same payload shape).
const MSG_REPLY_URL: &str = "https://api.bilibili.com/x/msgfeed/reply";
const MSG_AT_URL: &str = "https://api.bilibili.com/x/msgfeed/at";
const MSG_LIKE_URL: &str = "https://api.bilibili.com/x/msgfeed/like";
/// 私信会话列表 (`session_svr`, READ ONLY).
const DM_SESSIONS_URL: &str =
    "https://api.vc.bilibili.com/session_svr/v1/session_svr/get_sessions";
/// 私信内容 (`svr_sync`, READ ONLY).
const DM_MESSAGES_URL: &str =
    "https://api.vc.bilibili.com/svr_sync/v1/svr_sync/fetch_session_msgs";
/// 关注 / 粉丝 lists.
const RELATION_FOLLOWINGS_URL: &str = "https://api.bilibili.com/x/relation/followings";
const RELATION_FANS_URL: &str = "https://api.bilibili.com/x/relation/fans";
/// Sessions shown on the 私信 tab (each needs one `space_info` call for its name).
pub const DM_SESSION_LIMIT: usize = 20;

/// WBI keys are stable for a long time; refresh at most this often.
const WBI_TTL: Duration = Duration::from_secs(30 * 60);
/// Default number of feed pages (~30 items each) to request.
const DEFAULT_FEED_PAGES: usize = 3;
/// Upper bound for `BILI_FEED_PAGES` (each page is one signed request).
const MAX_FEED_PAGES: usize = 8;
/// Bilibili's image CDN accepts a `@WxH` resize suffix; request a small webp
/// thumbnail instead of the full-size cover.
const THUMB_SUFFIX: &str = "@480w_270h_1c.webp";
/// Hard cap on thumbnails fetched for one list, so a very long view cannot
/// spawn an unbounded number of image requests.
pub const THUMB_MAX_PER_VIEW: usize = 120;

// -- 收藏推荐 caching ---------------------------------------------------------
/// Folder list cache TTL (one entry per `mid`).
const FAV_FOLDER_TTL: Duration = Duration::from_secs(10 * 60);
/// One folder page cache TTL (keyed by `(media_id, page)`).
const FAV_PAGE_TTL: Duration = Duration::from_secs(5 * 60);
/// One video's related list cache TTL (keyed by `bvid`).
const FAV_RELATED_TTL: Duration = Duration::from_secs(5 * 60);
/// Hard cap on cached related lists, so a long mixed stream cannot grow the
/// process-wide map without bound. The oldest entry is evicted on insert.
const FAV_RELATED_MAX: usize = 128;
/// 收藏推荐 asks the folder endpoint for its own page size (one screen).
const FAV_PAGE_SIZE: u32 = 20;
/// The default folder's title. Its `id` is 0 in the `list-all` response.
const DEFAULT_FAV_FOLDER: &str = "默认收藏夹";

// -- UP主 page caching --------------------------------------------------------
/// One `(mid, order, page)` 投稿 page stays cached this long.
const UP_PAGE_TTL: Duration = Duration::from_secs(5 * 60);
/// Hard cap on cached 投稿 pages, so browsing many UPs cannot grow the map
/// without bound; the oldest entry is evicted on insert.
const UP_PAGE_MAX: usize = 48;
/// One UP主 card (name / avatar / counts) stays cached this long.
const UP_CARD_TTL: Duration = Duration::from_secs(5 * 60);
/// Hard cap on cached UP主 cards.
const UP_CARD_MAX: usize = 32;
/// Page size the UP主 page asks the arc endpoint for (the work order's `ps=30`).
const UP_PAGE_SIZE: i64 = 30;

/// Counts thumbnails that had to fall back from the small CDN variant to the
/// raw URL (diagnostic only, surfaced through `BILI_DEBUG_LOG`).
static THUMB_FALLBACKS: AtomicU64 = AtomicU64::new(0);

pub fn thumb_fallbacks() -> u64 {
    THUMB_FALLBACKS.load(Ordering::Relaxed)
}

/// How many feed pages one batch spans. Overridable with `BILI_FEED_PAGES`
/// (clamped to `1..=MAX_FEED_PAGES`); defaults to [`DEFAULT_FEED_PAGES`].
pub fn feed_pages() -> usize {
    std::env::var("BILI_FEED_PAGES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|pages| pages.clamp(1, MAX_FEED_PAGES))
        .unwrap_or(DEFAULT_FEED_PAGES)
}

/// Pages a single 换一换 advances the recommendation feed by.
pub fn feed_batch() -> u64 {
    feed_pages() as u64
}

/// Largest page size the list endpoints are asked for. The paged sections fetch
/// with `ps = list_page_size` so one API page is exactly one screen; a larger
/// request (or a non-positive one) is out of range and the endpoint keeps its
/// own default instead.
pub const MAX_PAGE_SIZE: i64 = 50;

/// The `ps` value to send for a client-chosen page size: `None` when `size`
/// falls outside `1..=MAX_PAGE_SIZE`, in which case the caller omits `ps` and
/// the endpoint applies its own default.
pub fn page_size_param(size: i64) -> Option<i64> {
    (1..=MAX_PAGE_SIZE).contains(&size).then_some(size)
}

/// The one Tokio runtime every network call in the app runs on.
///
/// Keep-alive connections are driven by the runtime that opened them, so a
/// single long-lived runtime is what makes connection pooling safe again: a
/// connection opened by one worker can be reused by any other worker. The
/// runtime lives for the whole process and is never dropped.
static NET_RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();

/// Handle to the shared network runtime, or `None` if it could not be built.
pub fn shared_runtime() -> Option<&'static tokio::runtime::Runtime> {
    NET_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("bili-net")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
}

// ---------------------------------------------------------------------------
// Persisted token
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Token {
    #[serde(default)]
    pub cookies: Vec<StoredCookie>,
    #[serde(default)]
    pub refresh_token: String,
}

/// `token.json` lives in the project root regardless of the current dir.
pub fn token_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("token.json")
}

pub fn load_token() -> Option<Token> {
    let data = std::fs::read_to_string(token_path()).ok()?;
    serde_json::from_str::<Token>(&data).ok()
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub struct Session {
    client: reqwest::Client,
    raw_client: reqwest::Client,
    jar: Arc<Jar>,
    wbi: Mutex<Option<WbiKeys>>,
}

#[derive(Clone)]
struct WbiKeys {
    img_key: String,
    sub_key: String,
    fetched: Instant,
}

// ---------------------------------------------------------------------------
// 收藏推荐 caches
// ---------------------------------------------------------------------------

/// Process-wide TTL cache for the 收藏推荐 data layer.
///
/// Folder ids and bvids are global, and the payloads are the same whichever
/// window asks, so one cache serves every `Session`: folders by `mid`, pages by
/// `(media_id, page)`, related lists by `bvid` (bounded to [`FAV_RELATED_MAX`]).
/// Every helper below locks, clones what it returns and unlocks again, so no
/// lock is ever held across an `.await`.
#[derive(Default)]
struct FavCache {
    folders: std::collections::HashMap<i64, (Instant, Vec<FavFolder>)>,
    pages: std::collections::HashMap<(i64, u32), (Instant, Vec<FeedVideo>, bool)>,
    related: std::collections::HashMap<String, (Instant, Vec<FeedVideo>)>,
}

static FAV_CACHE: OnceLock<Mutex<FavCache>> = OnceLock::new();

fn fav_cache() -> &'static Mutex<FavCache> {
    FAV_CACHE.get_or_init(|| Mutex::new(FavCache::default()))
}

fn fav_folders_cached(mid: i64) -> Option<Vec<FavFolder>> {
    let guard = fav_cache().lock().ok()?;
    let (at, list) = guard.folders.get(&mid)?;
    if at.elapsed() < FAV_FOLDER_TTL {
        Some(list.clone())
    } else {
        None
    }
}

fn fav_folders_store(mid: i64, list: &[FavFolder]) {
    if let Ok(mut guard) = fav_cache().lock() {
        guard
            .folders
            .retain(|_, (at, _)| at.elapsed() < FAV_FOLDER_TTL);
        guard.folders.insert(mid, (Instant::now(), list.to_vec()));
    }
}

fn fav_page_cached(media_id: i64, page: u32) -> Option<(Vec<FeedVideo>, bool)> {
    let guard = fav_cache().lock().ok()?;
    let (at, items, has_more) = guard.pages.get(&(media_id, page))?;
    if at.elapsed() < FAV_PAGE_TTL {
        Some((items.clone(), *has_more))
    } else {
        None
    }
}

fn fav_page_store(media_id: i64, page: u32, items: &[FeedVideo], has_more: bool) {
    if let Ok(mut guard) = fav_cache().lock() {
        guard
            .pages
            .retain(|_, (at, _, _)| at.elapsed() < FAV_PAGE_TTL);
        guard
            .pages
            .insert((media_id, page), (Instant::now(), items.to_vec(), has_more));
    }
}

fn fav_related_cached(bvid: &str) -> Option<Vec<FeedVideo>> {
    let guard = fav_cache().lock().ok()?;
    let (at, items) = guard.related.get(bvid)?;
    if at.elapsed() < FAV_RELATED_TTL {
        Some(items.clone())
    } else {
        None
    }
}

fn fav_related_store(bvid: &str, items: &[FeedVideo]) {
    if let Ok(mut guard) = fav_cache().lock() {
        guard
            .related
            .retain(|_, (at, _)| at.elapsed() < FAV_RELATED_TTL);
        if !guard.related.contains_key(bvid) && guard.related.len() >= FAV_RELATED_MAX {
            let oldest = guard
                .related
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                guard.related.remove(&key);
            }
        }
        guard
            .related
            .insert(bvid.to_string(), (Instant::now(), items.to_vec()));
    }
}

// ---------------------------------------------------------------------------
// UP主 page caches (`up_videos` / `user_card`)
// ---------------------------------------------------------------------------

/// Process-wide TTL cache for one UP主's page: 投稿 pages by
/// `(mid, order, page)` and the header card by `mid`. Both maps are bounded by
/// [`UP_PAGE_MAX`] / [`UP_CARD_MAX`]; helpers clone what they return and never
/// hold a lock across an `.await`.
#[derive(Default)]
struct UpCache {
    pages: std::collections::HashMap<(i64, UpOrder, u32), (Instant, Vec<FeedVideo>, bool)>,
    cards: std::collections::HashMap<i64, (Instant, String, String, u64, u64)>,
}

static UP_CACHE: OnceLock<Mutex<UpCache>> = OnceLock::new();

fn up_cache() -> &'static Mutex<UpCache> {
    UP_CACHE.get_or_init(|| Mutex::new(UpCache::default()))
}

fn up_page_cached(mid: i64, order: UpOrder, page: u32) -> Option<(Vec<FeedVideo>, bool)> {
    let guard = up_cache().lock().ok()?;
    let (at, items, has_more) = guard.pages.get(&(mid, order, page))?;
    if at.elapsed() < UP_PAGE_TTL {
        Some((items.clone(), *has_more))
    } else {
        None
    }
}

fn up_page_store(mid: i64, order: UpOrder, page: u32, items: &[FeedVideo], has_more: bool) {
    if let Ok(mut guard) = up_cache().lock() {
        guard.pages.retain(|_, (at, _, _)| at.elapsed() < UP_PAGE_TTL);
        let key = (mid, order, page);
        if !guard.pages.contains_key(&key) && guard.pages.len() >= UP_PAGE_MAX {
            let oldest = guard
                .pages
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(key, _)| *key);
            if let Some(key) = oldest {
                guard.pages.remove(&key);
            }
        }
        guard
            .pages
            .insert(key, (Instant::now(), items.to_vec(), has_more));
    }
}

fn up_card_cached(mid: i64) -> Option<(String, String, u64, u64)> {
    let guard = up_cache().lock().ok()?;
    let (at, name, avatar, fans, videos) = guard.cards.get(&mid)?;
    if at.elapsed() < UP_CARD_TTL {
        Some((name.clone(), avatar.clone(), *fans, *videos))
    } else {
        None
    }
}

fn up_card_store(mid: i64, name: &str, avatar: &str, fans: u64, videos: u64) {
    if let Ok(mut guard) = up_cache().lock() {
        guard.cards.retain(|_, (at, _, _, _, _)| at.elapsed() < UP_CARD_TTL);
        if !guard.cards.contains_key(&mid) && guard.cards.len() >= UP_CARD_MAX {
            let oldest = guard
                .cards
                .iter()
                .min_by_key(|(_, (at, _, _, _, _))| *at)
                .map(|(key, _)| *key);
            if let Some(key) = oldest {
                guard.cards.remove(&key);
            }
        }
        guard.cards.insert(
            mid,
            (
                Instant::now(),
                name.to_string(),
                avatar.to_string(),
                fans,
                videos,
            ),
        );
    }
}

impl Session {
    /// Fresh (logged-out) session for the QR login flow.
    pub async fn new() -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let session = Self {
            client: build_client(jar.clone())?,
            raw_client: build_raw_client(jar.clone())?,
            jar,
            wbi: Mutex::new(None),
        };
        // Best-effort device fingerprint; never abort the session.
        let _ = session.ensure_fingerprints().await;
        Ok(session)
    }

    /// Session seeded with a previously saved cookie set (auto-login).
    pub async fn from_token(token: &Token) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let api_url = Url::parse("https://api.bilibili.com/")?;
        for cookie in &token.cookies {
            jar.add_cookie_str(
                &format!(
                    "{}={}; Domain=.bilibili.com; Path=/",
                    cookie.name, cookie.value
                ),
                &api_url,
            );
        }
        let session = Self {
            client: build_client(jar.clone())?,
            raw_client: build_raw_client(jar.clone())?,
            jar,
            wbi: Mutex::new(None),
        };
        // Best-effort device fingerprint; never abort the session.
        let _ = session.ensure_fingerprints().await;
        Ok(session)
    }

    /// Best-effort: install every risk-control fingerprint cookie the web client
    /// normally carries (`buvid3`/`buvid4`, `b_nut`, `bili_ticket`). Each part is
    /// idempotent and skips itself when the cookie already exists, so this may be
    /// called any number of times. Never aborts the session (callers ignore it).
    pub async fn ensure_fingerprints(&self) -> Result<()> {
        let started = Instant::now();
        let _ = self.ensure_buvid().await;
        let _ = self.ensure_b_nut().await;
        let _ = self.ensure_bili_ticket().await;
        crate::debug_log(&format!(
            "fingerprints ms={} buvid3={} buvid4={} b_nut={} bili_ticket={} ticket_exp={}",
            started.elapsed().as_millis(),
            self.cookie_value("buvid3").is_some(),
            self.cookie_value("buvid4").is_some(),
            self.cookie_value("b_nut").is_some(),
            self.cookie_value("bili_ticket").is_some(),
            self.cookie_value("bili_ticket_expires").is_some(),
        ));
        Ok(())
    }

    /// Best-effort: fetch and install the `buvid3`/`buvid4` fingerprint
    /// cookies that `api.bilibili.com` risk control expects. Never aborts the
    /// session (callers ignore the result).
    pub async fn ensure_buvid(&self) -> Result<()> {
        if self.cookie_value("buvid3").is_some() && self.cookie_value("buvid4").is_some() {
            return Ok(());
        }
        let resp: SpiResp = self.client.get(SPI_URL).send().await?.json().await?;
        if resp.code == 0 {
            if let Some(data) = resp.data {
                if !data.b_3.is_empty() {
                    self.set_cookie("buvid3", &data.b_3);
                }
                if !data.b_4.is_empty() {
                    self.set_cookie("buvid4", &data.b_4);
                }
            }
        }
        Ok(())
    }

    /// Best-effort: read the `b_nut` (unix-seconds) cookie the homepage sets.
    async fn ensure_b_nut(&self) -> Result<()> {
        if self.cookie_value("b_nut").is_some() {
            return Ok(());
        }
        let resp = self.client.get(HOME_URL).send().await?;
        for cookie in parse_set_cookie(resp.headers()) {
            if cookie.name == "b_nut" && !cookie.value.is_empty() {
                self.set_cookie("b_nut", &cookie.value);
                break;
            }
        }
        Ok(())
    }

    /// Best-effort: mint a `bili_ticket` JWT (HMAC-SHA256 `ts` signature). The
    /// ticket is cached for its ~3 day TTL via the `bili_ticket_expires` cookie.
    async fn ensure_bili_ticket(&self) -> Result<()> {
        if self.cookie_value("bili_ticket").is_some() {
            if let Some(expires) = self.cookie_value("bili_ticket_expires") {
                if expires
                    .parse::<u64>()
                    .map(|exp| exp > now_secs() + 60)
                    .unwrap_or(false)
                {
                    return Ok(());
                }
            }
        }
        let ts = now_secs();
        let mut mac = Hmac::<Sha256>::new_from_slice(TICKET_HMAC_KEY.as_bytes())
            .map_err(|err| anyhow!("ticket hmac init: {err}"))?;
        mac.update(format!("ts{ts}").as_bytes());
        let hexsign = hex::encode(mac.finalize().into_bytes());
        let csrf = self.cookie_value("bili_jct").unwrap_or_default();
        let url = format!(
            "{TICKET_URL}?key_id=ec02&hexsign={hexsign}&context[ts]={ts}&csrf={csrf}"
        );
        let resp: TicketResp = self.client.post(&url).send().await?.json().await?;
        crate::debug_log(&format!("bili_ticket code={}", resp.code));
        if resp.code == 0 {
            if let Some(data) = resp.data {
                if !data.ticket.is_empty() {
                    self.set_cookie("bili_ticket", &data.ticket);
                    self.set_cookie(
                        "bili_ticket_expires",
                        &(data.created_at + data.ttl).to_string(),
                    );
                }
            }
        }
        Ok(())
    }

    /// Add one cookie to the shared jar for every `*.bilibili.com` host.
    fn set_cookie(&self, name: &str, value: &str) {
        if let Ok(api_url) = Url::parse("https://api.bilibili.com/") {
            self.jar.add_cookie_str(
                &format!("{name}={value}; Domain=.bilibili.com; Path=/"),
                &api_url,
            );
        }
    }

    /// Best-effort: follow the SSO redirect URL from a successful poll so any
    /// extra cookies get jarred. Callers ignore failures.
    pub async fn visit(&self, url: &str) -> Result<()> {
        let _ = self.client.get(url).send().await?;
        Ok(())
    }

    // -- QR login -----------------------------------------------------------

    pub async fn qr_generate(&self) -> Result<QrGen> {
        let resp: QrGenResp = self
            .client
            .get(QR_GEN_URL)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取二维码失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取二维码失败：响应缺少 data"))?;
        Ok(QrGen {
            url: data.url,
            key: data.qrcode_key,
        })
    }

    pub async fn qr_poll(&self, qrcode_key: &str) -> Result<PollResult> {
        let resp = self
            .client
            .get(QR_POLL_URL)
            .query(&[("qrcode_key", qrcode_key)])
            .send()
            .await?
            .error_for_status()?;

        // Cookies (SESSDATA / bili_jct / DedeUserID) arrive as Set-Cookie on
        // the successful poll response. Read them before consuming the body.
        let set_cookie = parse_set_cookie(resp.headers());

        let body: PollResp = resp.json().await?;
        if body.code != 0 {
            bail!("轮询登录状态失败（code={}）", body.code);
        }
        let data = body
            .data
            .ok_or_else(|| anyhow!("轮询登录状态失败：响应缺少 data"))?;

        // Union the Set-Cookie result with whatever the shared jar now holds
        // (e.g. the buvid3/buvid4 fingerprints), preferring the jar values.
        let cookies = merge_cookies(self.jar_cookies(), set_cookie);

        Ok(PollResult {
            state_code: data.code,
            cookies,
            refresh_token: data.refresh_token,
            cross_url: data.url,
        })
    }

    // -- Account info -------------------------------------------------------

    /// Returns [`NavOutcome::NotLoggedIn`] when the cookie set is not (or no
    /// longer) logged in; this is not an error, so the caller can surface the
    /// API `code`/`message` for diagnosis.
    pub async fn nav(&self) -> Result<NavOutcome> {
        let resp: NavResp = self.client.get(NAV_URL).send().await?.json().await?;

        // The nav payload carries the WBI keys whether or not a user is logged
        // in, so capture them before the login checks.
        if let Some(data) = resp.data.as_ref() {
            if let Some((img_key, sub_key)) = extract_wbi_keys(data) {
                self.store_wbi(img_key, sub_key);
            }
        }

        if resp.code != 0 {
            return Ok(NavOutcome::NotLoggedIn {
                code: resp.code,
                message: resp.message,
            });
        }
        let Some(data) = resp.data else {
            return Ok(NavOutcome::NotLoggedIn {
                code: resp.code,
                message: resp.message,
            });
        };
        if !data.is_login {
            return Ok(NavOutcome::NotLoggedIn {
                code: resp.code,
                message: resp.message,
            });
        }

        let vip_text = match &data.vip {
            Some(v) => {
                let label = v
                    .label
                    .as_ref()
                    .map(|l| l.text.clone())
                    .unwrap_or_default();
                if !label.is_empty() {
                    label
                } else if v.status == 1 {
                    "大会员".to_string()
                } else {
                    "普通用户".to_string()
                }
            }
            None => "普通用户".to_string(),
        };
        let vip_status = data.vip.as_ref().map(|v| v.status == 1).unwrap_or(false);

        Ok(NavOutcome::LoggedIn(NavUser {
            uname: data.uname,
            face: data.face,
            mid: data.mid,
            money: data.money,
            moral: data.moral,
            level: data
                .level_info
                .as_ref()
                .map(|l| l.current_level)
                .unwrap_or(0),
            vip_text,
            vip_status,
        }))
    }

    pub async fn relation(&self, vmid: i64) -> Result<Relation> {
        let resp: RelationResp = self
            .client
            .get(RELATION_URL)
            .query(&[("vmid", vmid)])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取关注/粉丝数失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取关注/粉丝数失败：响应缺少 data"))?;
        Ok(Relation {
            following: data.following,
            follower: data.follower,
        })
    }

    /// Best-effort; caller ignores failures.
    pub async fn myinfo(&self) -> Result<serde_json::Value> {
        let value: serde_json::Value = self.client.get(MYINFO_URL).send().await?.json().await?;
        Ok(value)
    }

    // -- 大会员 (VIP) --------------------------------------------------------

    /// Current VIP state plus the monthly privilege list (`privilege/my`).
    pub async fn vip_privilege_my(&self) -> Result<VipCode> {
        let text = self.client.get(VIP_PRIVILEGE_URL).send().await?.text().await?;
        let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("会员状态响应解析失败：{err}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let summary = if code == 0 {
            summarize_vip(&value)
        } else {
            String::new()
        };
        crate::debug_log(&format!("vip privilege/my code={code}"));
        Ok(VipCode {
            code,
            message,
            summary,
        })
    }

    /// Claim the daily VIP experience (`experience/add`, form `csrf`).
    pub async fn vip_experience_add(&self) -> Result<VipCode> {
        let csrf = self.csrf_cookie()?;
        let body = format!("csrf={}", encode_component(&csrf));
        let (code, message) = self
            .post_form_code(
                "vip/experience/add",
                VIP_EXPERIENCE_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        Ok(VipCode::from_code(code, message))
    }

    /// Claim the monthly privilege coupon (`privilege/receive`, form `type=1`).
    pub async fn vip_privilege_receive(&self) -> Result<VipCode> {
        let csrf = self.csrf_cookie()?;
        let body = format!("type=1&csrf={}", encode_component(&csrf));
        let (code, message) = self
            .post_form_code(
                "vip/privilege/receive",
                VIP_RECEIVE_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        Ok(VipCode::from_code(code, message))
    }

    /// 会员签到/大积分任务 (`pgc/activity/score/task/sign`, form `csrf`).
    pub async fn vip_task_sign(&self) -> Result<VipCode> {
        let csrf = self.csrf_cookie()?;
        let body = format!("csrf={}", encode_component(&csrf));
        let (code, message) = self
            .post_form_code(
                "pgc/activity/score/task/sign",
                PGC_SIGN_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        Ok(VipCode::from_code(code, message))
    }

    // -- Like ---------------------------------------------------------------

    /// Read whether the account has liked `bvid`: `0` = not liked, `1` = liked.
    pub async fn like_state(&self, bvid: &str) -> Result<i64> {
        let resp: LikeStateResp = self
            .client
            .get(LIKE_HAS_URL)
            .query(&[("bvid", bvid)])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取点赞状态失败（code={}）", resp.code);
        }
        Ok(resp.data.unwrap_or(0))
    }

    /// Set the like state for `bvid`; `liked = true` likes, `false` un-likes.
    /// The `csrf` token is the `bili_jct` cookie value.
    pub async fn like(&self, bvid: &str, liked: bool) -> Result<()> {
        let csrf = self
            .cookie_value("bili_jct")
            .ok_or_else(|| anyhow!("缺少 csrf（bili_jct）"))?;
        let action = if liked { "1" } else { "2" };
        let body = format!(
            "bvid={}&like={}&csrf={}",
            encode_component(bvid),
            action,
            encode_component(&csrf)
        );
        let response = self
            .client
            .post(LIKE_URL)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(REFERER, format!("https://www.bilibili.com/video/{bvid}"))
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: LikeResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("点赞响应解析失败（HTTP {status}）：{err}"))?;
        if resp.code != 0 {
            let message = if resp.message.is_empty() {
                "点赞操作失败".to_string()
            } else {
                resp.message
            };
            bail!("{message}（code={}, HTTP {status}）", resp.code);
        }
        Ok(())
    }

    // -- Write helper -------------------------------------------------------

    /// The `bili_jct` cookie value used as `csrf` on every write.
    fn csrf_cookie(&self) -> Result<String> {
        self.cookie_value("bili_jct")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("缺少 csrf（bili_jct）"))
    }

    /// POST an `application/x-www-form-urlencoded` body and unwrap the standard
    /// `{code, message}` envelope, surfacing the API code and HTTP status on
    /// failure.
    async fn post_form(&self, url: &str, referer: &str, body: String) -> Result<()> {
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(REFERER, referer)
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: CodeResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("操作响应解析失败（HTTP {status}）：{err}"))?;
        crate::debug_log(&format!(
            "post_form {url} http={status} code={} msg={}",
            resp.code, resp.message
        ));
        if resp.code != 0 {
            let message = if resp.message.is_empty() {
                "操作失败".to_string()
            } else {
                resp.message
            };
            bail!("{message}（code={}, HTTP {status}）", resp.code);
        }
        Ok(())
    }

    /// Like [`Session::post_form`] but returns the API `code` and `message`
    /// verbatim instead of failing on a non-zero code, so write endpoints whose
    /// result depends on risk control can report exactly what came back.
    /// `label` names the endpoint in the debug log (the URL may carry `csrf`).
    async fn post_form_code(        &self,
        label: &str,
        url: &str,
        referer: &str,
        body: String,
    ) -> Result<(i64, String)> {
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(REFERER, referer)
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: CodeResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("{label} 响应解析失败（HTTP {status}）：{err}"))?;
        crate::debug_log(&format!(
            "{label} http={status} code={} msg={}",
            resp.code, resp.message
        ));
        Ok((resp.code, resp.message))
    }

    // -- Watch history (历史记录) -----------------------------------------

    /// Report one playback heartbeat, which is what makes a video appear in the
    /// account's **历史记录** on the official web site and app, with the position
    /// they show there.
    ///
    /// `played_s` is the position, `watched_s` the time actually spent in this
    /// session, `start_ts` the Unix second the session began. `play_type` follows
    /// the web player: 1 start, 0 playing, 2 pause, 3 resume, 4 ended. The API
    /// answers `code = 0` on success; a non-zero code is returned (not an error)
    /// so a risk-control rejection cannot break playback.
    #[allow(clippy::too_many_arguments)]
    pub async fn report_heartbeat(
        &self,
        aid: u64,
        bvid: &str,
        cid: u64,
        played_s: u64,
        watched_s: u64,
        duration_s: u64,
        start_ts: u64,
        play_type: u64,
    ) -> Result<i64> {
        let csrf = self.csrf_cookie()?;
        let body = heartbeat_body(
            aid,
            bvid,
            cid,
            played_s,
            watched_s,
            duration_s,
            start_ts,
            play_type,
            &csrf,
        );
        let referer = format!("https://www.bilibili.com/video/{bvid}");
        let (code, message) = self
            .post_form_code("heartbeat", HEARTBEAT_URL, &referer, body)
            .await?;
        if code != 0 {
            crate::debug_log(&format!(
                "heartbeat rejected bvid={bvid} code={code} msg={message}"
            ));
        }
        Ok(code)
    }

    // -- Coin (投币) -------------------------------------------------------
    /// Coins the active account has already given `bvid`: 0, 1 or 2.
    pub async fn coin_state(&self, bvid: &str) -> Result<i64> {
        let resp: CoinStateResp = self
            .client
            .get(COIN_STATE_URL)
            .query(&[("bvid", bvid)])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取投币状态失败（code={}）", resp.code);
        }
        Ok(resp.data.map(|data| data.multiply).unwrap_or(0))
    }

    /// Give `multiply` (1 or 2) coins to `bvid`, optionally also liking it.
    pub async fn add_coin(&self, bvid: &str, multiply: i64, select_like: bool) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "bvid={}&multiply={}&select_like={}&csrf={}",
            encode_component(bvid),
            multiply.clamp(1, 2),
            if select_like { 1 } else { 0 },
            encode_component(&csrf),
        );
        let referer = format!("https://www.bilibili.com/video/{bvid}");
        self.post_form(COIN_ADD_URL, &referer, body).await
    }

    // -- Favourites (收藏) -------------------------------------------------

    /// The active account's favourite folders for the 收藏推荐 stream, minus the
    /// default folder (`id == 0`, titled [`DEFAULT_FAV_FOLDER`]) and any empty
    /// folder. Cached for [`FAV_FOLDER_TTL`].
    ///
    /// The `mid` comes from the `DedeUserID` cookie the login flow jars, falling
    /// back to `nav`; the cookie keeps this to one request per cache miss.
    pub async fn fav_folders(&self) -> Result<Vec<FavFolder>> {
        let mid = self.fav_mid().await?;
        if let Some(cached) = fav_folders_cached(mid) {
            return Ok(cached);
        }
        let resp: FavFoldersResp = self
            .client
            .get(FAV_FOLDERS_URL)
            .query(&[
                ("up_mid", mid.to_string()),
                ("web_location", "333.1387".to_string()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            return Err(fav_error("获取收藏夹失败", resp.code, &resp.message));
        }
        let folders = fav_folders_from(resp.data);
        crate::debug_log(&format!(
            "fav folders mid={mid} n={} total={}",
            folders.len(),
            folders.iter().map(|folder| folder.count).sum::<u32>()
        ));
        fav_folders_store(mid, &folders);
        Ok(folders)
    }

    /// One page (`page` is 1-based) of a favourite folder for the 收藏推荐
    /// stream, plus whether the folder has more pages. Cached for
    /// [`FAV_PAGE_TTL`] per `(media_id, page)`.
    ///
    /// `order=mtime` matches the folder's own 最近收藏 order; entries deleted or
    /// otherwise broken come back as `null` and are skipped.
    pub async fn fav_items(&self, media_id: i64, page: u32) -> Result<(Vec<FeedVideo>, bool)> {
        let page = page.max(1);
        if let Some(cached) = fav_page_cached(media_id, page) {
            return Ok(cached);
        }
        let resp: FavListResp = self
            .client
            .get(FAV_LIST_URL)
            .query(&[
                ("media_id", media_id.to_string()),
                ("pn", page.to_string()),
                ("ps", FAV_PAGE_SIZE.to_string()),
                ("platform", "web".to_string()),
                ("order", "mtime".to_string()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            return Err(fav_error("获取收藏内容失败", resp.code, &resp.message));
        }
        let (items, has_more) = fav_page_from(resp.data);
        crate::debug_log(&format!(
            "fav items media={media_id} page={page} n={} has_more={has_more}",
            items.len()
        ));
        fav_page_store(media_id, page, &items, has_more);
        Ok((items, has_more))
    }

    /// One video's related list (the 相关推荐 the player shows), filtered to
    /// real archive entries: ads carry no `bvid` and/or a non-`av` `goto`.
    /// Cached for [`FAV_RELATED_TTL`] per `bvid`.
    ///
    /// Safe to call from several tasks at once: the cache is only locked for the
    /// lookup and the store, never across the request.
    pub async fn related(&self, bvid: &str) -> Result<Vec<FeedVideo>> {
        let bvid = bvid.trim();
        if bvid.is_empty() {
            bail!("获取相关视频失败：bvid 为空");
        }
        if let Some(cached) = fav_related_cached(bvid) {
            return Ok(cached);
        }
        let referer = format!("https://www.bilibili.com/video/{bvid}");
        let resp: RelatedResp = self
            .client
            .get(RELATED_URL)
            .query(&[("bvid", bvid)])
            .header(REFERER, referer)
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            return Err(fav_error("获取相关视频失败", resp.code, &resp.message));
        }
        let items = related_from(resp.data.unwrap_or_default());
        crate::debug_log(&format!("related bvid={bvid} n={}", items.len()));
        fav_related_store(bvid, &items);
        Ok(items)
    }

    /// The active account's `mid`: the jarred `DedeUserID`, else `nav`.
    async fn fav_mid(&self) -> Result<i64> {
        if let Some(mid) = self
            .cookie_value("DedeUserID")
            .and_then(|value| value.trim().parse::<i64>().ok())
            .filter(|mid| *mid > 0)
        {
            return Ok(mid);
        }
        match self.nav().await? {
            NavOutcome::LoggedIn(user) => Ok(user.mid),
            NavOutcome::NotLoggedIn { code, message } => {
                Err(fav_error("未登录，无法获取收藏夹", code, &message))
            }
        }
    }

    /// Whether `aid` is favourited, plus how many folders hold it.
    pub async fn fav_state(&self, aid: i64) -> Result<(bool, i64)> {
        let resp: FavFavouredResp = self
            .client
            .get(FAV_FAVOURED_URL)
            .query(&[("aid", aid.to_string())])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取收藏状态失败（code={}）", resp.code);
        }
        let data = resp.data.unwrap_or_default();
        Ok((data.favoured, data.count))
    }

    /// Add `rid` (an `aid`, `type=2`) to `add` folders and remove it from `del`.
    pub async fn fav_deal(&self, rid: i64, add: &[i64], del: &[i64]) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "rid={rid}&type=2&add_media_ids={}&del_media_ids={}&csrf={}",
            encode_component(&join_ids(add)),
            encode_component(&join_ids(del)),
            encode_component(&csrf),
        );
        self.post_form(FAV_DEAL_URL, "https://www.bilibili.com/", body)
            .await
    }

    /// One page (`pn` is 1-based) of a favourite folder's contents. `ps` is the
    /// requested page size; out of range (`1..=MAX_PAGE_SIZE`) the endpoint's
    /// own default (20) applies.
    pub async fn fav_list(&self, media_id: i64, pn: i64, ps: i64) -> Result<FavListPage> {
        let ps = page_size_param(ps).unwrap_or(20);
        let resp: FavListResp = self
            .client
            .get(FAV_LIST_URL)
            .query(&[
                ("media_id", media_id.to_string()),
                ("pn", pn.max(1).to_string()),
                ("ps", ps.to_string()),
                ("order", "mtime".to_string()),
                ("type", "0".to_string()),
                ("tid", "0".to_string()),
                ("platform", "web".to_string()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取收藏内容失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取收藏内容失败：响应缺少 data"))?;
        let title = data.info.map(|info| info.title).unwrap_or_default();
        let items = data
            .medias
            .into_iter()
            .flatten()
            .map(|media| FavVideo {
                bvid: media.bvid,
                title: media.title,
                cover: normalize_url(&media.cover),
                duration: media.duration,
                upper: media
                    .upper
                    .as_ref()
                    .map(|owner| owner.name.clone())
                    .unwrap_or_default(),
                owner_mid: media.upper.as_ref().map(|owner| owner.mid).unwrap_or(0),
                play: media.cnt_info.map(|info| info.play).unwrap_or(0),
            })
            .collect();
        Ok(FavListPage {
            title,
            page: pn.max(1),
            has_more: data.has_more,
            items,
        })
    }

    // -- Watch history (历史记录) ------------------------------------------

    /// One page of watch history. Feed the previous page's cursor (`max`,
    /// `view_at`, `business`) to page; the first call passes zeros.
    pub async fn history(&self, max: i64, view_at: i64, business: &str) -> Result<HistoryPage> {
        let resp: HistoryResp = self
            .client
            .get(HISTORY_URL)
            .query(&[
                ("ps", "30".to_string()),
                ("max", max.to_string()),
                ("view_at", view_at.to_string()),
                ("business", business.to_string()),
                ("type", "all".to_string()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取历史记录失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取历史记录失败：响应缺少 data"))?;
        let cursor = data.cursor.map(|cursor| HistoryCursor {
            max: cursor.max,
            view_at: cursor.view_at,
            business: cursor.business,
        });
        let items: Vec<HistoryItem> = data
            .list
            .into_iter()
            .map(|item| {
                let history = item.history.unwrap_or_default();
                let kid = if !history.business.is_empty() {
                    format!("{}_{}", history.business, history.oid)
                } else if !item.kid.is_empty() {
                    item.kid
                } else {
                    history.kid.clone()
                };
                HistoryItem {
                    title: item.title,
                    cover: normalize_url(&item.cover),
                    bvid: history.bvid,
                    kid,
                    author: item.author_name,
                    owner_mid: item.author_mid,
                    duration: item.duration,
                    progress: item.progress,
                }
            })
            .collect();
        let has_more = !items.is_empty()
            && cursor
                .as_ref()
                .map(|cursor| cursor.max != 0)
                .unwrap_or(false);
        Ok(HistoryPage {
            cursor,
            items,
            has_more,
        })
    }

    /// Delete one history entry by its `history.kid`.
    pub async fn history_delete(&self, kid: &str) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "kid={}&csrf={}",
            encode_component(kid),
            encode_component(&csrf)
        );
        self.post_form(
            HISTORY_DELETE_URL,
            "https://www.bilibili.com/account/history",
            body,
        )
        .await
    }

    /// Clear the whole watch history.
    pub async fn history_clear(&self) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!("csrf={}", encode_component(&csrf));
        self.post_form(
            HISTORY_CLEAR_URL,
            "https://www.bilibili.com/account/history",
            body,
        )
        .await
    }

    // -- Watch later (稍后再看) --------------------------------------------

    /// The account's 稍后再看 list plus its count.
    pub async fn toview(&self) -> Result<ToViewPage> {
        let resp: ToViewResp = self.client.get(TOVIEW_URL).send().await?.json().await?;
        if resp.code != 0 {
            bail!("获取稍后再看失败（code={}）", resp.code);
        }
        let data = resp.data.unwrap_or_default();
        let items = data
            .list
            .into_iter()
            .map(|item| ToViewItem {
                bvid: item.bvid,
                aid: item.aid,
                title: item.title,
                cover: normalize_url(&item.cover),
                duration: item.duration,
                owner: item.owner.map(|owner| owner.name).unwrap_or_default(),
                view: item.stat.map(|stat| stat.view).unwrap_or(0),
            })
            .collect();
        Ok(ToViewPage {
            count: data.count,
            items,
        })
    }

    /// Add `bvid` to 稍后再看.
    pub async fn toview_add(&self, bvid: &str) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "bvid={}&csrf={}",
            encode_component(bvid),
            encode_component(&csrf)
        );
        self.post_form(TOVIEW_ADD_URL, "https://www.bilibili.com/", body)
            .await
    }

    /// Remove the 稍后再看 entry for `aid`.
    pub async fn toview_del(&self, aid: i64) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!("aid={aid}&csrf={}", encode_component(&csrf));
        self.post_form(TOVIEW_DEL_URL, "https://www.bilibili.com/", body)
            .await
    }

    // -- Search -------------------------------------------------------------

    /// Search videos. `page` is 1-based; `ps` is the requested page size (out of
    /// range falls back to the endpoint's own default of 20).
    /// `order` is one of `totalrank|click|pubdate|dm|stow` (empty = default).
    pub async fn search_video(
        &self,
        keyword: &str,
        page: i64,
        order: &str,
        ps: i64,
    ) -> Result<SearchPage<SearchVideo>> {
        let mut params = vec![
            ("search_type".to_string(), "video".to_string()),
            ("keyword".to_string(), keyword.to_string()),
            ("page".to_string(), page.max(1).to_string()),
        ];
        if let Some(ps) = page_size_param(ps) {
            params.push(("page_size".to_string(), ps.to_string()));
        }
        if !order.is_empty() {
            params.push(("order".to_string(), order.to_string()));
        }
        let data = self.search_data::<Vec<SearchVideoItem>>(&params).await?;
        let items = data
            .result
            .into_iter()
            .map(|item| SearchVideo {
                bvid: item.bvid,
                title: strip_html(&item.title),
                description: strip_html(&item.description),
                pic: normalize_url(&item.pic),
                author: item.author,
                mid: item.mid,
                play: item.play,
                duration: item.duration,
                pub_ts: item.pubdate,
            })
            .collect();
        Ok(SearchPage {
            page: data.page,
            num_pages: data.num_pages,
            num_results: data.num_results,
            items,
        })
    }

    /// Search up users. `page` is 1-based; `ps` is the requested page size (out
    /// of range falls back to the endpoint's own default).
    pub async fn search_user(
        &self,
        keyword: &str,
        page: i64,
        ps: i64,
    ) -> Result<SearchPage<SearchUser>> {
        let mut params = vec![
            ("search_type".to_string(), "bili_user".to_string()),
            ("keyword".to_string(), keyword.to_string()),
            ("page".to_string(), page.max(1).to_string()),
        ];
        if let Some(ps) = page_size_param(ps) {
            params.push(("page_size".to_string(), ps.to_string()));
        }
        let data = self.search_data::<Vec<SearchUserItem>>(&params).await?;
        let items = data
            .result
            .into_iter()
            .map(|item| SearchUser {
                mid: item.mid,
                uname: item.uname,
                upic: normalize_url(&item.upic),
                fans: item.fans,
                videos: item.videos,
                usign: item.usign,
            })
            .collect();
        Ok(SearchPage {
            page: data.page,
            num_pages: data.num_pages,
            num_results: data.num_results,
            items,
        })
    }

    /// WBI-signed search call with one retry after refreshing the keys.
    async fn search_data<T: DeserializeOwned>(
        &self,
        params: &[(String, String)],
    ) -> Result<SearchData<T>> {
        let mut resp: SearchResp<T> = self.wbi_call(SEARCH_URL, params, false).await?.1;
        if resp.code != 0 {
            resp = self.wbi_call(SEARCH_URL, params, true).await?.1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("搜索失败（code={}）", resp.code);
            }
            bail!("搜索失败：{message}（code={}）", resp.code);
        }
        resp.data
            .ok_or_else(|| anyhow!("搜索失败：响应缺少 data"))
    }

    // -- Video detail -------------------------------------------------------

    /// Full metadata for one video (WBI-signed, one retry).
    pub async fn video_view(&self, bvid: &str) -> Result<VideoDetail> {
        let params = vec![("bvid".to_string(), bvid.to_string())];
        let mut resp: ViewResp = self.wbi_call(VIEW_URL, &params, false).await?.1;
        if resp.code != 0 {
            resp = self.wbi_call(VIEW_URL, &params, true).await?.1;
        }
        if resp.code != 0 {
            bail!("获取视频详情失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取视频详情失败：响应缺少 data"))?;
        // The upload's own resolution is the only evidence `playurl` cannot see
        // (it reports what this account may play, not what the source has), so
        // it is kept for the tier fallback of the very next `playurl` call.
        remember_native_height(&data.bvid, data.dimension.as_ref().map(|size| size.height).unwrap_or(0));
        Ok(detail_from_view(data))
    }

    // -- 视频评论 (comments) ------------------------------------------------

    /// One page of a video's comment list.
    ///
    /// `sort_hot` selects `mode=3` (热门, the default) or `mode=2` (按时间).
    /// `cursor` is the previous page's [`CommentPage::cursor`] (`""` for the
    /// first page). The response carries the pinned comment (if any), the
    /// top-level comments — each with a few preview replies — the next cursor
    /// and whether the list has ended.
    ///
    /// WBI-signed; a non-zero `code` (e.g. a stale key signature) triggers
    /// exactly one retry after force-refreshing the WBI keys.
    pub async fn comments(&self, aid: i64, sort_hot: bool, cursor: &str) -> Result<CommentPage> {
        let mode = if sort_hot { 3 } else { 2 };
        let pagination = serde_json::json!({ "offset": cursor }).to_string();
        let params = vec![
            ("oid".to_string(), aid.to_string()),
            ("type".to_string(), "1".to_string()),
            ("mode".to_string(), mode.to_string()),
            ("plat".to_string(), "1".to_string()),
            ("web_location".to_string(), "1315875".to_string()),
            ("pagination_str".to_string(), pagination),
        ];
        let referer = format!("https://www.bilibili.com/video/av{aid}");
        let (_, mut resp): (u16, CommentResp) = self
            .wbi_call_referer(COMMENT_MAIN_URL, &params, false, Some(&referer))
            .await?;
        if resp.code != 0 {
            resp = self
                .wbi_call_referer(COMMENT_MAIN_URL, &params, true, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取评论失败（code={}）", resp.code);
            }
            bail!("获取评论失败：{message}（code={}）", resp.code);
        }
        let page = comment_page_from(resp.data.unwrap_or_default());
        crate::debug_log(&format!(
            "comments aid={aid} mode={mode} n={} next={} is_end={} total={}",
            page.comments.len(),
            !page.cursor.is_empty(),
            page.is_end,
            page.total
        ));
        Ok(page)
    }

    /// One comment's dialog (楼中楼). `pn` is 1-based; `ps` is fixed at 20.
    ///
    /// The endpoint is called unsigned first (the shape the web client uses)
    /// and retried once WBI-signed if that is rejected.
    pub async fn comment_replies(
        &self,
        aid: i64,
        root_rpid: i64,
        pn: i64,
    ) -> Result<CommentReplyPage> {
        let pn = pn.max(1);
        let params: Vec<(&str, String)> = vec![
            ("oid", aid.to_string()),
            ("type", "1".to_string()),
            ("root", root_rpid.to_string()),
            ("pn", pn.to_string()),
            ("ps", "20".to_string()),
        ];
        let referer = format!("https://www.bilibili.com/video/av{aid}");
        let mut resp: CommentReplyResp = self
            .get_query_json(COMMENT_REPLY_URL, &params, Some(&referer))
            .await?;
        if resp.code != 0 {
            let signed: Vec<(String, String)> = params
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            resp = self
                .wbi_call_referer(COMMENT_REPLY_URL, &signed, false, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取评论回复失败（code={}）", resp.code);
            }
            bail!("获取评论回复失败：{message}（code={}）", resp.code);
        }
        let page = comment_reply_page_from(resp.data.unwrap_or_default(), pn);
        crate::debug_log(&format!(
            "comment-replies aid={aid} root={root_rpid} pn={pn} n={} total={} has_more={}",
            page.replies.len(),
            page.total,
            page.has_more
        ));
        Ok(page)
    }

    /// UP主 profile: name, avatar, sign, level, fan/friend counts, official
    /// badge and vip label. WBI-signed; a `-352` rejection is retried once with
    /// the space page as `Referer` (some accounts require it).
    pub async fn space_info(&self, mid: i64) -> Result<UpInfo> {
        let params = vec![("mid".to_string(), mid.to_string())];
        let referer = format!("https://space.bilibili.com/{mid}");
        let (_, mut resp): (u16, SpaceInfoResp) =
            self.wbi_call_referer(SPACE_INFO_URL, &params, false, None).await?;
        if resp.code == -352 {
            resp = self
                .wbi_call_referer(SPACE_INFO_URL, &params, true, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取UP主信息失败（code={}）", resp.code);
            }
            bail!("获取UP主信息失败：{message}（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取UP主信息失败：响应缺少 data"))?;
        let official = data
            .official
            .map(|official| official.desc.trim().to_string())
            .unwrap_or_default();
        let vip = match &data.vip {
            Some(vip) => {
                let label = vip
                    .label
                    .as_ref()
                    .map(|label| label.text.clone())
                    .unwrap_or_default();
                if !label.is_empty() {
                    label
                } else if vip.status == 1 {
                    "大会员".to_string()
                } else {
                    String::new()
                }
            }
            None => String::new(),
        };
        Ok(UpInfo {
            name: data.name,
            face: normalize_url(&data.face),
            sign: data.sign,
            level: data.level,
            fans: data.fans,
            friend: data.friend,
            official,
            vip,
        })
    }

    /// One page of a UP主's投稿 (`pn` is 1-based, newest first). `ps` is the
    /// requested page size (out of range keeps the endpoint's own default).
    pub async fn space_videos(&self, mid: i64, pn: i64, ps: i64) -> Result<SpaceVideoPage> {
        let pn = pn.max(1);
        let ps = page_size_param(ps).unwrap_or(30);
        let params = vec![
            ("mid".to_string(), mid.to_string()),
            ("ps".to_string(), ps.to_string()),
            ("pn".to_string(), pn.to_string()),
            ("order".to_string(), "pubdate".to_string()),
        ];
        let referer = format!("https://space.bilibili.com/{mid}");
        let (_, mut resp): (u16, SpaceArcResp) = self
            .wbi_call_referer(SPACE_ARC_URL, &params, false, Some(&referer))
            .await?;
        if resp.code != 0 {
            resp = self
                .wbi_call_referer(SPACE_ARC_URL, &params, true, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取投稿列表失败（code={}）", resp.code);
            }
            bail!("获取投稿列表失败：{message}（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取投稿列表失败：响应缺少 data"))?;
        let total = data.page.as_ref().map(|page| page.count).unwrap_or(0);
        let ps = data.page.as_ref().map(|page| page.ps).unwrap_or(30).max(1);
        let items: Vec<SpaceVideo> = data
            .list
            .vlist
            .into_iter()
            .map(|item| SpaceVideo {
                bvid: item.bvid,
                title: strip_html(&item.title),
                pic: normalize_url(&item.pic),
                length: item.length,
                play: item.play,
                danmaku: item.video_review,
            })
            .collect();
        let has_more = total > 0 && pn * ps < total;
        Ok(SpaceVideoPage {
            page: pn,
            has_more,
            items,
        })
    }

    /// One page of a UP主's投稿, ordered by [`UpOrder`], for the UP page grid.
    ///
    /// Same signed `x/space/wbi/arc/search` endpoint [`Session::space_videos`]
    /// uses, but with the order as the only variable and a 5 minute cache per
    /// `(mid, order, page)`. `page` is 1-based; the page size is the fixed
    /// [`UP_PAGE_SIZE`]. The bool is "a further page exists".
    pub async fn up_videos(
        &self,
        mid: i64,
        order: UpOrder,
        page: u32,
    ) -> Result<(Vec<FeedVideo>, bool)> {
        let page = page.max(1);
        if let Some(hit) = up_page_cached(mid, order, page) {
            crate::debug_log(&format!(
                "up videos cache=hit mid={mid} order={} page={page} n={}",
                order.param(),
                hit.0.len()
            ));
            return Ok(hit);
        }
        let params = vec![
            ("mid".to_string(), mid.to_string()),
            ("ps".to_string(), UP_PAGE_SIZE.to_string()),
            ("pn".to_string(), page.to_string()),
            ("order".to_string(), order.param().to_string()),
            ("platform".to_string(), "web".to_string()),
        ];
        let referer = format!("https://space.bilibili.com/{mid}");
        let (_, mut resp): (u16, SpaceArcResp) = self
            .wbi_call_referer(SPACE_ARC_URL, &params, false, Some(&referer))
            .await?;
        if resp.code != 0 {
            resp = self
                .wbi_call_referer(SPACE_ARC_URL, &params, true, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取UP主投稿失败（code={}）", resp.code);
            }
            bail!("获取UP主投稿失败：{message}（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取UP主投稿失败：响应缺少 data"))?;
        let total = data.page.as_ref().map(|page| page.count).unwrap_or(0);
        let ps = data
            .page
            .as_ref()
            .map(|page| page.ps)
            .unwrap_or(UP_PAGE_SIZE)
            .max(1);
        let items: Vec<FeedVideo> = data
            .list
            .vlist
            .into_iter()
            .filter(|item| !item.bvid.is_empty())
            .map(|item| FeedVideo {
                bvid: item.bvid,
                // The arc list carries no cid; `video_view` resolves it when the
                // card is actually opened (same as the discovery grids).
                cid: 0,
                aid: item.aid.max(0) as u64,
                title: strip_html(&item.title),
                pic: normalize_url(&item.pic),
                owner: String::new(),
                owner_mid: mid,
                view: item.play,
                duration: length_seconds(&item.length),
                // `created` is the arc list's publish stamp; a `pubdate` field
                // appears on some responses and wins when it is there.
                pub_ts: if item.pubdate > 0 {
                    item.pubdate
                } else {
                    item.created
                },
            })
            .collect();
        let has_more = total > 0 && i64::from(page) * ps < total;
        up_page_store(mid, order, page, &items, has_more);
        Ok((items, has_more))
    }

    /// The UP page header: `(name, avatar, fans, videos)` for `mid`.
    ///
    /// `x/web-interface/card` is a plain GET whose `data.card` carries the
    /// name/face and whose `data.archive_count` is the 投稿 count. When that
    /// endpoint refuses (risk control), `x/space/wbi/acc/info` — already used by
    /// [`Session::space_info`] — supplies the same header. Cached for
    /// [`UP_CARD_TTL`].
    pub async fn user_card(&self, mid: i64) -> Result<(String, String, u64, u64)> {
        if let Some(hit) = up_card_cached(mid) {
            return Ok(hit);
        }
        let referer = format!("https://space.bilibili.com/{mid}");
        let card: Result<UpCardResp> = match self
            .client
            .get(UP_CARD_URL)
            .query(&[("mid", mid)])
            .header(REFERER, &referer)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<UpCardResp>().await {
                Ok(body) => Ok(body),
                Err(err) => Err(anyhow!("响应解析失败：{err}")),
            },
            Err(err) => Err(err.into()),
        };
        if let Ok(resp) = &card {
            if resp.code == 0 {
                if let Some(data) = &resp.data {
                    let name = data.card.name.trim().to_string();
                    if !name.is_empty() {
                        let fans = data.card.fans;
                        let videos = data.archive_count;
                        up_card_store(mid, &name, &normalize_url(&data.card.face), fans, videos);
                        return Ok((
                            name,
                            normalize_url(&data.card.face),
                            fans,
                            videos,
                        ));
                    }
                }
            } else {
                crate::debug_log(&format!(
                    "up card fallback mid={mid} code={} message={:?}",
                    resp.code,
                    resp.message.trim()
                ));
            }
        }
        // Fallback: the space info endpoint (WBI-signed, with its own -352 retry).
        let info = self.space_info(mid).await?;
        let name = info.name;
        let avatar = info.face;
        let fans = info.fans.max(0) as u64;
        // `acc/info` has no 投稿 count; ask the card's archive count endpoint
        // through the relation-independent `nav`-free path above only. When that
        // failed the count is unknown, so report 0 rather than a guess.
        let videos = 0;
        up_card_store(mid, &name, &avatar, fans, videos);
        Ok((name, avatar, fans, videos))
    }

    /// The active account's relation to `fid`: `0` unfollowed, `2` following,
    /// `6` mutual, `128` blocked.
    pub async fn relation_state(&self, fid: i64) -> Result<i64> {
        let resp: RelationStateResp = self
            .client
            .get(RELATION_STATE_URL)
            .query(&[("fid", fid)])
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("获取关注状态失败（code={}）", resp.code);
        }
        Ok(resp.data.map(|data| data.attribute).unwrap_or(0))
    }

    /// Follow (`act = 1`) or unfollow (`act = 2`) `fid`. The API code and message
    /// are surfaced on failure, because this account can receive risk-control
    /// rejections on write actions.
    pub async fn relation_modify(&self, fid: i64, act: i64) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "fid={fid}&act={}&re_src=11&csrf={}",
            if act == 2 { 2 } else { 1 },
            encode_component(&csrf),
        );
        let referer = format!("https://space.bilibili.com/{fid}");
        self.post_form(RELATION_MODIFY_URL, &referer, body).await
    }

    // -- 动态流 (dynamic feed) ---------------------------------------------

    /// One page of the dynamic feed. `offset` is the previous page's
    /// `data.offset` (`""` for the first page); the response returns the next
    /// `offset` and whether more items exist.
    pub async fn dynamic_feed(&self, offset: &str, feed_type: &str) -> Result<DynamicPage> {
        let mut params: Vec<(&str, String)> =
            vec![("type", feed_type.to_string()), ("page", "1".to_string())];
        if !offset.is_empty() {
            params.push(("offset", offset.to_string()));
        }
        let body: DynamicFeedResp = self.get_query_json(DYNAMIC_FEED_URL, &params, None).await?;
        if body.code != 0 {
            bail!("获取动态失败（code={}）", body.code);
        }
        let data = body.data.unwrap_or_default();
        let items = data.items.into_iter().map(dynamic_from).collect();
        Ok(DynamicPage {
            offset: data.offset,
            has_more: data.has_more,
            items,
        })
    }

    /// Like (`liked = true`, `up = 1`) or un-like (`up = 2`) one dynamic.
    ///
    /// `csrf` is a query parameter and the body is JSON (`dyn_id_str` + `up`).
    /// The API `code` is returned verbatim because this write can be
    /// risk-controlled (`-111` bad csrf, `4100001` bad parameters).
    pub async fn dynamic_thumb(&self, dyn_id_str: &str, liked: bool) -> Result<i64> {
        let csrf = self.csrf_cookie()?;
        let url = format!("{DYNAMIC_THUMB_URL}?csrf={}", encode_component(&csrf));
        let up = if liked { 1 } else { 2 };
        let payload = serde_json::json!({
            "dyn_id_str": dyn_id_str,
            "up": up,
        });
        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(REFERER, "https://t.bilibili.com/")
            .body(payload.to_string())
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: CodeResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("动态点赞响应解析失败（HTTP {status}）：{err}"))?;
        crate::debug_log(&format!(
            "dynamic_thumb id={dyn_id_str} up={up} http={status} code={} msg={}",
            resp.code, resp.message
        ));
        Ok(resp.code)
    }

    // -- 消息中心 (message center) -----------------------------------------

    /// The per-category unread counts behind the 消息 badge.
    pub async fn msg_unread(&self) -> Result<MsgUnread> {
        let body: MsgUnreadResp = self
            .client
            .get(MSG_UNREAD_URL)
            .send()
            .await?
            .json()
            .await?;
        if body.code != 0 {
            bail!("获取消息未读失败（code={}）", body.code);
        }
        let map = body.data.unwrap_or_default();
        let get = |key: &str| {
            map.get(key)
                .and_then(|value| value.as_i64())
                .unwrap_or(0)
                .max(0)
        };
        let reply = get("reply");
        let at = get("at");
        let like = get("like");
        let total = reply
            + at
            + like
            + get("recv_reply")
            + get("recv_at")
            + get("recv_like")
            + get("sys_msg")
            + get("up");
        Ok(MsgUnread {
            total,
            reply,
            at,
            like,
        })
    }

    /// One page of 回复我的 / @我的 / 收到的赞.
    pub async fn msg_list(&self, kind: MsgKind) -> Result<MsgPage> {
        let url = match kind {
            MsgKind::Reply => MSG_REPLY_URL,
            MsgKind::At => MSG_AT_URL,
            MsgKind::Like => MSG_LIKE_URL,
        };
        let params = [
            ("platform", "web".to_string()),
            ("build", "0".to_string()),
            ("mobi_app", "web".to_string()),
        ];
        let body: MsgFeedResp = self.get_query_json(url, &params, None).await?;
        if body.code != 0 {
            bail!("获取消息列表失败（code={}）", body.code);
        }
        let data = body.data.unwrap_or_default();
        let items = data.items.into_iter().map(msg_item_from).collect();
        Ok(MsgPage { items })
    }

    // -- 私信 (private messages, READ ONLY) --------------------------------

    /// The private-message session list.
    pub async fn dm_sessions(&self) -> Result<Vec<DmSession>> {
        let params = [
            ("session_type", "1".to_string()),
            ("group_fold", "1".to_string()),
            ("unfollow_fold", "0".to_string()),
            ("sort_rule", "2".to_string()),
            ("build", "0".to_string()),
            ("mobi_app", "web".to_string()),
        ];
        let body: DmSessionsResp = self.get_query_json(DM_SESSIONS_URL, &params, None).await?;
        if body.code != 0 {
            bail!("获取私信会话失败（code={}）", body.code);
        }
        let list = body.data.map(|data| data.session_list).unwrap_or_default();
        Ok(list.into_iter().map(dm_session_from).collect())
    }

    /// One session's messages, oldest first.
    pub async fn dm_messages(&self, talker_id: i64) -> Result<Vec<DmMessage>> {
        let url = format!(
            "{DM_MESSAGES_URL}?talker_id={talker_id}&session_type=1&size=20&begin_seqno="
        );
        let body: DmMessagesResp = self.get_query_json(&url, &[], None).await?;
        if body.code != 0 {
            bail!("获取私信内容失败（code={}）", body.code);
        }
        let messages = body.data.map(|data| data.messages).unwrap_or_default();
        let mut out: Vec<DmMessage> = messages.into_iter().map(dm_message_from).collect();
        out.sort_by_key(|message| message.timestamp);
        Ok(out)
    }

    // -- 关注 / 粉丝 (relations) -------------------------------------------

    /// One page (`pn` 1-based, 20 per page) of an account's 关注 list.
    pub async fn followings(&self, mid: i64, pn: i64) -> Result<RelationPage> {
        let pn = pn.max(1);
        let params = [
            ("vmid", mid.to_string()),
            ("pn", pn.to_string()),
            ("ps", "20".to_string()),
            ("order", "desc".to_string()),
            ("order_type", String::new()),
        ];
        self.relation_page(RELATION_FOLLOWINGS_URL, &params, pn).await
    }

    /// One page (`pn` 1-based, 20 per page) of an account's 粉丝 list.
    pub async fn fans(&self, mid: i64, pn: i64) -> Result<RelationPage> {
        let pn = pn.max(1);
        let params = [
            ("vmid", mid.to_string()),
            ("pn", pn.to_string()),
            ("ps", "20".to_string()),
        ];
        self.relation_page(RELATION_FANS_URL, &params, pn).await
    }

    async fn relation_page(
        &self,
        url: &str,
        params: &[(&str, String)],
        pn: i64,
    ) -> Result<RelationPage> {
        let body: RelationListResp = self.get_query_json(url, params, None).await?;
        if body.code != 0 {
            bail!("获取用户列表失败（code={}）", body.code);
        }
        let data = body.data.unwrap_or_default();
        let total = data.total.max(0);
        let items = data.list.into_iter().map(relation_user_from).collect();
        let has_more = pn * 20 < total;
        Ok(RelationPage {
            total,
            page: pn,
            has_more,
            items,
        })
    }

    /// Log the active account out server-side. Callers must confirm the login is
    /// still live first; the CSRF field here is `biliCSRF`, not `csrf`.
    pub async fn logout(&self) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!("biliCSRF={}", encode_component(&csrf));
        self.post_form(LOGOUT_URL, "https://www.bilibili.com/", body)
            .await
    }

    /// Sign, GET and parse one JSON response with an optional explicit
    /// `Referer`. Retrying with fresh keys is left to the caller via `force`.
    async fn wbi_call_referer<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(String, String)],
        force: bool,
        referer: Option<&str>,
    ) -> Result<(u16, T)> {
        let signed = self.sign(params.to_vec(), force).await?;
        let full = format!("{url}?{signed}");
        let mut request = self.client.get(&full);
        if let Some(referer) = referer {
            request = request.header(REFERER, referer);
        }
        let resp = request.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        let body = serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))?;
        Ok((status, body))
    }

    /// Sign, GET and parse one JSON response. Retrying with fresh keys is left
    /// to the caller via `force`.
    async fn wbi_call<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(String, String)],
        force: bool,
    ) -> Result<(u16, T)> {
        let signed = self.sign(params.to_vec(), force).await?;
        let full = format!("{url}?{signed}");
        let resp = self.client.get(&full).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        let body = serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))?;
        Ok((status, body))
    }

    // -- Playback URLs ------------------------------------------------------

    /// Resolve the playable media URLs for one `bvid`/`cid` pair.
    ///
    /// Every query parameter is WBI-signed. A non-zero `code` (e.g. a stale key
    /// signature) triggers exactly one retry after force-refreshing the WBI
    /// keys; if it still fails the API code and HTTP status are reported.
    pub async fn playurl(&self, bvid: &str, cid: u64) -> Result<PlayInfo> {
        let params = playurl_params(bvid, cid);
        let (mut status, mut body) = match self.playurl_request(&params, false).await {
            Ok(result) => result,
            Err(err) => {
                // A PGC episode (or a risk-controlled request) can answer the
                // WBI endpoint with a non-JSON 412. The plain endpoint takes the
                // same bvid/cid and is the path 番剧 playback is verified on.
                crate::debug_log(&format!("playurl wbi failed bvid={bvid} err={err:#}"));
                self.playurl_plain_request(&params).await?
            }
        };
        if body.code != 0 {
            let (retry_status, retry_body) = self
                .playurl_request(&params, true)
                .await
                .unwrap_or((0, PlayUrlResp { code: -1, data: None }));
            if retry_body.code == 0 {
                status = retry_status;
                body = retry_body;
            }
        }
        if body.code != 0 {
            // PGC: `x/player/wbi/playurl` is refused for 番剧 episodes, while
            // the unsigned `x/player/playurl` answers with a real DASH tree.
            let (plain_status, plain_body) = self.playurl_plain_request(&params).await?;
            crate::debug_log(&format!(
                "playurl plain fallback bvid={bvid} wbi_code={} code={} http={plain_status}",
                body.code, plain_body.code
            ));
            status = plain_status;
            body = plain_body;
        }

        crate::debug_log(&format!("playurl code={} http={status}", body.code));

        if body.code != 0 {
            // Name the site's refusal in Chinese when the code has a known
            // meaning (-404 内容不存在, -10403 地区限制, 87007 大会员). The
            // player's toast shows this text verbatim.
            if let Some(reason) = pgc_reason(body.code) {
                bail!("{reason}（code={}）", body.code);
            }
            bail!("获取播放地址失败（code={}, HTTP {status}）", body.code);
        }
        let data = body
            .data
            .ok_or_else(|| anyhow!("获取播放地址失败：响应缺少 data"))?;
        // The tier lists are the whole truth about what the video offers: the
        // account's `dash.video[]` may be a strict subset (大会员 tiers are
        // dropped for a non-VIP account). Logged verbatim so a QA run can tell
        // "the video does not have 4K" from "this account may not play 4K".
        crate::debug_log(&format!(
            "playurl tiers accept_quality={:?} accept_description={:?} support_formats=[{}]",
            data.accept_quality,
            data.accept_description,
            data.support_formats
                .iter()
                .map(|format| format!(
                    "q={} new={} display={} codecs={}",
                    format.quality,
                    format.new_description,
                    format.display_desc,
                    format.codecs.join("|")
                ))
                .collect::<Vec<_>>()
                .join(",")
        ));
        let tiers = {
            // The response only names tiers this account can reach; when it names
            // nothing at all, the upload's own resolution is the remaining
            // evidence. `video_view` usually ran first and left it cached, so
            // this costs no request on the normal open path.
            let advertised =
                !data.accept_quality.is_empty() || !data.support_formats.is_empty();
            let mut native_height = native_height_for(bvid).unwrap_or(0);
            if !advertised && native_height == 0 {
                native_height = self
                    .video_view(bvid)
                    .await
                    .map(|view| view.height)
                    .unwrap_or(0);
            }
            build_quality_tiers(&data, native_height)
        };
        let info = build_play_info(data).ok_or_else(|| anyhow!("获取播放地址失败：未找到可用媒体流"))?;
        // Remember the full tier list for the 画质 menu: the playable stream
        // rows come from `info.qualities`, the locked 大会员 rows from this.
        if tiers_qa_bvid().as_deref() == Some(bvid) {
            log_tier_table(bvid, cid, &tiers);
        }
        remember_quality_tiers(bvid, cid, tiers);
        // Diagnostic only: every candidate's `qn`/`codecid`/`bandwidth`, so the
        // auto selection (smallest stream) can be checked against the others.
        crate::debug_log(&format!(
            "playurl tracks n={} list={}",
            info.video_tracks.len(),
            info.video_tracks
                .iter()
                .map(|track| format!("{}/{}/{}", track.id, track.codecid, track.bandwidth))
                .collect::<Vec<_>>()
                .join(",")
        ));
        crate::debug_log(&format!(
            "playurl qualities n={} fnval={} list={}",
            info.qualities.len(),
            quality_fnval(),
            info.qualities
                .iter()
                .map(|quality| {
                    format!(
                        "qn={} codec={} cid={} {}x{} {:.2}fps {}bps {} hdr={} dv={} 8k={} url={} backups={}",
                        quality.qn,
                        quality.codec,
                        quality.codecid,
                        quality.width,
                        quality.height,
                        quality.fps,
                        quality.bandwidth_bps,
                        quality.label,
                        quality.hdr,
                        quality.dolby_vision,
                        quality.eight_k,
                        quality.url.len(),
                        quality.backup_urls.len()
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        ));
        crate::debug_log(&format!(
            "playurl audio n={} list={}",
            info.audio_tracks.len(),
            info.audio_tracks
                .iter()
                .map(|track| format!(
                    "{}/{}/vip={}/dolby={}/hires={}",
                    track.id, track.bandwidth, track.vip, track.dolby, track.hi_res
                ))
                .collect::<Vec<_>>()
                .join(",")
        ));
        Ok(info)
    }

    async fn playurl_request(
        &self,
        params: &[(String, String)],
        force: bool,
    ) -> Result<(u16, PlayUrlResp)> {
        let signed = self.sign(params.to_vec(), force).await?;
        let url = format!("{PLAYURL_URL}?{signed}");
        let resp = self.client.get(&url).send().await?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("?")
            .to_string();
        let text = resp.text().await?;
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        match serde_json::from_str::<PlayUrlResp>(cleaned) {
            Ok(body) => Ok((status, body)),
            Err(err) => {
                crate::debug_log(&format!(
                    "playurl parse fail http={status} ct={content_type} len={} starts_json={} err={err}",
                    text.len(),
                    cleaned.starts_with('{')
                ));
                bail!("播放地址响应解析失败（HTTP {status}）")
            }
        }
    }

    /// Unsigned `x/player/playurl`. Same params, no WBI signature: the path
    /// that answers for PGC episodes (their `bvid` + `cid` are real archive
    /// ids) when the WBI endpoint risk-controls the request.
    async fn playurl_plain_request(
        &self,
        params: &[(String, String)],
    ) -> Result<(u16, PlayUrlResp)> {
        let resp = self
            .client
            .get(PLAYURL_PLAIN_URL)
            .query(params)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        match serde_json::from_str::<PlayUrlResp>(cleaned) {
            Ok(body) => Ok((status, body)),
            Err(err) => {
                crate::debug_log(&format!(
                    "playurl plain parse fail http={status} len={} err={err}",
                    text.len()
                ));
                bail!("播放地址响应解析失败（HTTP {status}）")
            }
        }
    }

    // -- Danmaku ------------------------------------------------------------

    /// Fetch one 6-minute danmaku segment over `client`.
    ///
    /// `seg.so` is the plain-cookie endpoint (no WBI signing) and answers with
    /// raw protobuf, so the body is read as bytes and decoded by the
    /// hand-written reader in [`crate::danmaku`].
    async fn danmaku_segment(
        &self,
        client: &reqwest::Client,
        cid: u64,
        segment_index: u32,
    ) -> Result<Vec<crate::danmaku::Danmaku>> {
        let oid = cid.to_string();
        let index = segment_index.to_string();
        let response = client
            .get(DM_SEG_URL)
            .query(&[
                ("type", "1"),
                ("oid", oid.as_str()),
                ("segment_index", index.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?;

        // Risk control / logged-out responses answer with a JSON error body
        // instead of protobuf; never decode those, and never log their content.
        if bytes.first() == Some(&b'{') {
            crate::debug_log(&format!(
                "danmaku seg={segment_index} json_body_len={}",
                bytes.len()
            ));
            bail!("弹幕接口返回错误响应");
        }

        Ok(crate::danmaku::decode_segment(&bytes))
    }

    /// Fetch every segment covering `duration_secs`, sorted by time.
    ///
    /// Best-effort: three consecutive segment failures stop the scan and
    /// whatever was decoded is returned.
    pub async fn danmaku_all(
        &self,
        cid: u64,
        duration_secs: u64,
    ) -> Vec<crate::danmaku::Danmaku> {
        if cid == 0 {
            return Vec::new();
        }
        let segments = (duration_secs.saturating_add(359) / 360).max(1);
        let mut all: Vec<crate::danmaku::Danmaku> = Vec::new();
        let mut consecutive_errors = 0u32;
        let mut index: u32 = 1;
        while (index as u64) <= segments {
            match self.danmaku_segment(&self.client, cid, index).await {
                Ok(list) => {
                    all.extend(list);
                    consecutive_errors = 0;
                }
                Err(err) => {
                    crate::debug_log(&format!("danmaku seg {index} failed: {err:#}"));
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        break;
                    }
                }
            }
            index += 1;
        }

        all.sort_by_key(|item| item.progress_ms);
        all
    }

    /// Best-effort server-side AI danmaku filter toggle.
    ///
    /// Returns the API `code` verbatim (never an error for a non-zero code) so
    /// the UI can show exactly what the endpoint answered. Requires a live
    /// login: the `bili_jct` cookie supplies `csrf`.
    pub async fn danmaku_ai_config(&self, ai_switch: bool, ai_level: Option<u32>) -> Result<i64> {
        let csrf = self.csrf_cookie()?;
        let mut body = format!(
            "ai_switch={}&csrf={}",
            if ai_switch { 1 } else { 0 },
            encode_component(&csrf),
        );
        if let Some(level) = ai_level {
            body.push_str(&format!("&ai_level={}", level.min(10)));
        }
        let response = self
            .client
            .post(DM_AI_CONFIG_URL)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(REFERER, "https://www.bilibili.com/")
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: CodeResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("AI 屏蔽响应解析失败（HTTP {status}）：{err}"))?;
        crate::debug_log(&format!(
            "danmaku_ai_config ai_switch={ai_switch} ai_level={ai_level:?} http={status} code={} msg={}",
            resp.code, resp.message
        ));
        Ok(resp.code)
    }

    /// The dates (`YYYY-MM-DD`) that carry historical danmaku for `cid`.
    ///
    /// `history/index` requires a `month` (`YYYY-MM`), so the current month and
    /// up to 11 earlier ones are scanned and merged newest-first. A `-400` from
    /// any month aborts with the API code/message. Scanning stops early once at
    /// least one date was found and three consecutive months came back empty.
    pub async fn danmaku_history_dates(&self, cid: u64) -> Result<Vec<String>> {
        let oid = cid.to_string();
        let (mut year, mut month) = current_year_month();
        let mut dates: Vec<String> = Vec::new();
        let mut empty_streak = 0u32;

        for _ in 0..12 {
            let month_text = format!("{year:04}-{month:02}");
            let resp: HistoryIndexResp = self
                .client
                .get(DM_HISTORY_INDEX_URL)
                .query(&[
                    ("type", "1"),
                    ("oid", oid.as_str()),
                    ("month", month_text.as_str()),
                ])
                .send()
                .await?
                .json()
                .await?;
            if resp.code != 0 {
                crate::debug_log(&format!(
                    "dm history/index month={month_text} code={} msg={}",
                    resp.code, resp.message
                ));
                bail!(
                    "获取历史弹幕日期失败（code={}）：{}",
                    resp.code,
                    resp.message
                );
            }
            let found = resp.data.unwrap_or_default();
            if found.is_empty() {
                empty_streak += 1;
                if !dates.is_empty() && empty_streak >= 3 {
                    break;
                }
            } else {
                empty_streak = 0;
                dates.extend(found);
            }
            if month == 1 {
                month = 12;
                year -= 1;
            } else {
                month -= 1;
            }
        }

        dates.sort();
        dates.dedup();
        dates.reverse();
        crate::debug_log(&format!(
            "dm history/index cid={cid} months_scanned dates={}",
            dates.len()
        ));
        Ok(dates)
    }

    /// One day's historical danmaku for `cid`.
    ///
    /// The XML endpoint is tried first; when it yields no `<d>` entries (it now
    /// answers a `state=2` "弹幕功能已关闭" placeholder) the protobuf endpoint is
    /// used instead. A hard XML failure also falls back, so one broken endpoint
    /// cannot break the feature.
    pub async fn danmaku_history(&self, cid: u64, date: &str) -> Result<DanmakuHistory> {
        match self.danmaku_history_xml(cid, date).await {
            Ok(items) if !items.is_empty() => {
                crate::debug_log(&format!(
                    "dm history date={date} source=xml items={}",
                    items.len()
                ));
                return Ok(DanmakuHistory {
                    items,
                    source: "xml",
                });
            }
            Ok(_) => {
                crate::debug_log(&format!("dm history date={date} xml_empty -> protobuf"));
            }
            Err(err) => {
                crate::debug_log(&format!("dm history date={date} xml_err={err:#} -> protobuf"));
            }
        }

        let items = self.danmaku_history_seg(cid, date).await?;
        crate::debug_log(&format!(
            "dm history date={date} source=protobuf items={}",
            items.len()
        ));
        Ok(DanmakuHistory {
            items,
            source: "protobuf",
        })
    }

    /// Fetch and parse the XML from `x/v2/dm/history`.
    ///
    /// The server always answers `Content-Encoding: deflate` (raw, which
    /// reqwest's own decoder rejects), so a decompression-disabled client reads
    /// the raw bytes and they are inflated here before parsing.
    async fn danmaku_history_xml(
        &self,
        cid: u64,
        date: &str,
    ) -> Result<Vec<crate::danmaku::Danmaku>> {
        let oid = cid.to_string();
        let response = self
            .raw_client
            .get(DM_HISTORY_URL)
            .query(&[("type", "1"), ("oid", oid.as_str()), ("date", date)])
            .send()
            .await?;
        let status = response.status().as_u16();
        let encoding = response
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = response.bytes().await?;
        let body = match encoding.as_str() {
            "" | "identity" => bytes.to_vec(),
            "deflate" => inflate_deflate(&bytes)
                .ok_or_else(|| anyhow!("历史弹幕 deflate 解码失败（HTTP {status}）"))?,
            other => bail!("历史弹幕响应编码不支持：{other}（HTTP {status}）"),
        };

        let text = String::from_utf8_lossy(&body);
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        if cleaned.starts_with('{') {
            let resp: CodeResp = serde_json::from_str(cleaned)
                .map_err(|err| anyhow!("历史弹幕响应解析失败（HTTP {status}）：{err}"))?;
            bail!(
                "获取历史弹幕失败（code={}, HTTP {status}）：{}",
                resp.code,
                resp.message
            );
        }
        Ok(crate::danmaku::parse_history_xml(cleaned))
    }

    async fn danmaku_history_seg(&self, cid: u64, date: &str) -> Result<Vec<crate::danmaku::Danmaku>> {
        let oid = cid.to_string();
        let response = self
            .client
            .get(DM_HISTORY_SEG_URL)
            .query(&[("type", "1"), ("oid", oid.as_str()), ("date", date)])
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.first() == Some(&b'{') {
            let text = String::from_utf8_lossy(&bytes);
            let cleaned = text.trim_start_matches('\u{feff}').trim();
            let resp: CodeResp = serde_json::from_str(cleaned)
                .map_err(|err| anyhow!("历史弹幕响应解析失败：{err}"))?;
            bail!("获取历史弹幕失败（code={}）：{}", resp.code, resp.message);
        }
        Ok(crate::danmaku::decode_segment(&bytes))
    }

    /// Send one video danmaku. `progress_ms` is the appearance time, `mode` 1
    /// (scroll) / 4 (bottom) / 5 (top), `fontsize` 25 is the API default.
    ///
    /// The request is WBI-signed (`w_rid`/`wts` over `web_location` + `csrf`) and
    /// the `csrf` is repeated in the form body. Returns the API `code` and
    /// `message` verbatim; a non-zero code is not an `Err` because this account
    /// can be risk-controlled on writes.
    pub async fn danmaku_post(
        &self,
        cid: u64,
        bvid: &str,
        msg: &str,
        progress_ms: u64,
        mode: u32,
        fontsize: u32,
        color: u32,
        pool: u32,
    ) -> Result<(i64, String)> {
        let csrf = self.csrf_cookie()?;
        let signed = self
            .sign(
                vec![
                    ("web_location".to_string(), "1315873".to_string()),
                    ("csrf".to_string(), csrf.clone()),
                ],
                false,
            )
            .await?;
        let url = format!("{DM_POST_URL}?{signed}");
        let body = format!(
            "type=1&oid={cid}&bvid={}&msg={}&progress={progress_ms}&mode={mode}&fontsize={fontsize}&color={color}&pool={pool}&csrf={}",
            encode_component(bvid),
            encode_component(msg),
            encode_component(&csrf),
        );
        let referer = format!("https://www.bilibili.com/video/{bvid}");
        self.post_form_code("dm/post", &url, &referer, body).await
    }

    /// Report one danmaku. `reason` is the documented report-type code
    /// (1..=12); `content` is only honoured for `reason == 11` (其他).
    pub async fn danmaku_report(
        &self,
        cid: u64,
        dmid: &str,
        reason: u32,
        content: &str,
    ) -> Result<(i64, String)> {
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "cid={cid}&dmid={}&reason={}&content={}&csrf={}",
            encode_component(dmid),
            reason,
            encode_component(content),
            encode_component(&csrf),
        );
        self.post_form_code("dm/report/add", DM_REPORT_URL, "https://www.bilibili.com/", body)
            .await
    }

    /// Like (`op = 1`) or un-like (`op = 2`) one danmaku.
    pub async fn danmaku_thumbup(&self, cid: u64, dmid: &str, op: u32) -> Result<(i64, String)> {
        let csrf = self.csrf_cookie()?;
        let op = if op == 2 { 2 } else { 1 };
        let body = format!(
            "dmid={}&oid={cid}&op={op}&platform=web_player&csrf={}",
            encode_component(dmid),
            encode_component(&csrf),
        );
        self.post_form_code("dm/thumbup/add", DM_THUMBUP_URL, "https://www.bilibili.com/", body)
            .await
    }

    // -- WBI signing --------------------------------------------------------

    fn store_wbi(&self, img_key: String, sub_key: String) {
        if img_key.is_empty() || sub_key.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.wbi.lock() {
            *guard = Some(WbiKeys {
                img_key,
                sub_key,
                fetched: Instant::now(),
            });
        }
    }

    fn cached_wbi(&self) -> Option<(String, String)> {
        let guard = self.wbi.lock().ok()?;
        let keys = guard.as_ref()?;
        if keys.fetched.elapsed() > WBI_TTL {
            return None;
        }
        Some((keys.img_key.clone(), keys.sub_key.clone()))
    }

    /// Return cached WBI keys, fetching them via `nav` when absent, stale or
    /// when `force` is set (used after a signing rejection).
    async fn wbi_keys(&self, force: bool) -> Result<(String, String)> {
        if !force {
            if let Some(keys) = self.cached_wbi() {
                return Ok(keys);
            }
        }
        let _ = self.nav().await?;
        self.cached_wbi()
            .ok_or_else(|| anyhow!("未能获取 WBI 签名密钥"))
    }

    /// Sign `params` with the current WBI keys.
    async fn sign(&self, params: Vec<(String, String)>, force: bool) -> Result<String> {
        let (img_key, sub_key) = self.wbi_keys(force).await?;
        Ok(crate::wbi::enc_wbi(params, &img_key, &sub_key))
    }

    async fn feed_page(&self, params: &[(String, String)], force: bool) -> Result<FeedResp> {
        let signed = self.sign(params.to_vec(), force).await?;
        let url = format!("{FEED_URL}?{signed}");
        let response = self.client.get(&url).send().await?;
        crate::debug_log(&format!("feed http={:?}", response.version()));
        let resp: FeedResp = response.json().await?;
        Ok(resp)
    }

    // -- Homepage recommendation feed ---------------------------------------

    /// Fetch [`feed_batch`] pages of recommendations starting at
    /// `fresh_idx_start`. Advancing the start past the pages already shown
    /// yields a fresh batch (the 换一换 path). `on_page` is called once per
    /// fetched page with `(pages_done, pages_total)` so the caller can publish
    /// the feed's refresh progress.
    pub async fn homepage_feed_from(
        &self,
        fresh_idx_start: u64,
        on_page: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<Vec<FeedVideo>> {
        let feed_started = Instant::now();
        self.wbi_keys(false).await?;

        let pages = feed_pages();
        let mut out: Vec<FeedVideo> = Vec::new();
        let mut fresh_idx: u64 = fresh_idx_start.max(1);
        let mut brush: u64 = fresh_idx_start.max(1);
        let mut rows_so_far: u64 = 0;
        let mut last_showlist = String::new();
        let mut refreshed = false;

        for page in 0..pages {
            let page_started = Instant::now();
            let params = feed_params(fresh_idx, brush, 1 + rows_so_far, &last_showlist);
            let mut resp = self.feed_page(&params, false).await?;

            if resp.code != 0 && !refreshed {
                refreshed = true;
                resp = self.feed_page(&params, true).await?;
            }
            crate::debug_log(&format!(
                "feed page={page} req_ms={} code={}",
                page_started.elapsed().as_millis(),
                resp.code
            ));

            if resp.code != 0 {
                if page == 0 {
                    bail!("获取首页推荐失败（code={}）", resp.code);
                }
                break;
            }
            let Some(data) = resp.data else { break };
            let count = data.item.len();
            if count == 0 {
                break;
            }

            let mut ids: Vec<String> = Vec::with_capacity(count);
            for item in data.item {
                ids.push(format!("av_{}", item.id));
                if item.goto != "av" {
                    continue;
                }
                let (owner, owner_mid) = match item.owner {
                    Some(owner) => (owner.name, owner.mid),
                    None => (String::new(), 0),
                };
                out.push(FeedVideo {
                    bvid: item.bvid,
                    cid: item.cid,
                    aid: item.id,
                    title: item.title,
                    pic: item.pic,
                    owner,
                    owner_mid,
                    view: item.stat.map(|s| s.view).unwrap_or(0),
                    duration: item.duration,
                    pub_ts: item.pubdate,
                });
            }

            last_showlist = ids.join("_");
            rows_so_far += count as u64;
            fresh_idx += 1;
            brush += 1;
            crate::debug_log(&format!(
                "feed page={page} items={count} total_items={}",
                out.len()
            ));
            on_page(page as u64 + 1, pages as u64);
        }

        crate::debug_log(&format!(
            "feed fetch pages={pages} total_ms={} items={}",
            feed_started.elapsed().as_millis(),
            out.len()
        ));
        Ok(out)
    }

    // -- Discovery: 分区 / 排行 / 热搜 / 搜索建议 -------------------------

    /// One page of a region's latest videos. `pn` is 1-based; `ps` is the
    /// requested page size (out of range keeps the endpoint's own default).
    ///
    /// The documented `dynamic/region` endpoint currently answers `-404` for
    /// every caller, so `newlist` (identical `archives`/`page` payload) is the
    /// working fallback; both are tried in order.
    pub async fn region_latest(&self, rid: i64, pn: i64, ps: i64) -> Result<CardPage> {
        let pn = pn.max(1);
        let ps = page_size_param(ps).unwrap_or(20);
        let params = [
            ("rid", rid.to_string()),
            ("pn", pn.to_string()),
            ("ps", ps.to_string()),
        ];
        match self.archive_page(REGION_URL, &params).await {
            Ok(page) if !page.items.is_empty() => Ok(page),
            _ => self.archive_page(REGION_NEWLIST_URL, &params).await,
        }
    }

    /// 每日放送: exactly 7 days around today (`before=3&after=3`).
    pub async fn pgc_timeline(&self) -> Result<Vec<PgcTimelineDay>> {
        let params = [
            ("types", "1,4,2,3,5,7".to_string()),
            ("before", "3".to_string()),
            ("after", "3".to_string()),
        ];
        let body: PgcTimelineResp = self.get_query_json(PGC_TIMELINE_URL, &params, None).await?;
        if body.code != 0 {
            bail!("每日放送加载失败（code={}）", body.code);
        }
        Ok(body.result.into_iter().map(timeline_day_from).collect())
    }

    /// 分区索引: one page (20 rows) for `season_type`, sorted by `order` (see
    /// [`PGC_INDEX_URL`] for the verified meaning of each value; `3` = 追番人数,
    /// the site's own default and the pre-existing behaviour here).
    pub async fn pgc_index(
        &self,
        season_type: i64,
        page: i64,
        pagesize: i64,
        order: i64,
    ) -> Result<PgcIndexPage> {
        let page = page.max(1);
        let pagesize = pagesize.clamp(1, 50);
        let order = order.clamp(0, 5);
        let params = [
            ("season_type", season_type.to_string()),
            ("page", page.to_string()),
            ("pagesize", pagesize.to_string()),
            ("order", order.to_string()),
            ("st", "1".to_string()),
            ("sort", "0".to_string()),
            ("type", "1".to_string()),
        ];
        let body: PgcIndexResp = self.get_query_json(PGC_INDEX_URL, &params, None).await?;
        if body.code != 0 {
            bail!("番剧索引加载失败（code={}）", body.code);
        }
        let data = body.data.unwrap_or_default();
        let page_no = if data.num > 0 { data.num } else { page };
        Ok(PgcIndexPage {
            page: page_no,
            total: data.total,
            has_next: data.has_next,
            items: data.list.into_iter().map(index_item_from).collect(),
        })
    }

    /// 排行榜 for one season type (99 rows, no paging).
    pub async fn pgc_rank(&self, season_type: i64, day: i64) -> Result<Vec<PgcRankItem>> {
        let params = [
            ("season_type", season_type.to_string()),
            ("day", day.max(1).to_string()),
        ];
        let body: PgcRankResp = self.get_query_json(PGC_RANK_URL, &params, None).await?;
        if body.code != 0 {
            bail!("番剧排行榜加载失败（code={}）", body.code);
        }
        Ok(body
            .result
            .unwrap_or_default()
            .list
            .into_iter()
            .map(rank_item_from)
            .collect())
    }

    /// One season's detail (episodes, section groups, evaluate, rating, areas).
    pub async fn pgc_season(&self, season_id: i64) -> Result<PgcSeason> {
        let params = [("season_id", season_id.to_string())];
        let body: PgcSeasonResp = self.get_query_json(PGC_SEASON_URL, &params, None).await?;
        if body.code != 0 {
            bail!("番剧详情加载失败（code={}）", body.code);
        }
        body.result
            .map(season_from)
            .ok_or_else(|| anyhow!("番剧详情加载失败：响应缺少 result"))
    }

    /// Region / availability gate for one episode. Never errors on a non-zero
    /// API code - the code IS the answer, and the caller shows it.
    pub async fn pgc_gate(&self, ep_id: i64, cid: u64) -> Result<PgcGate> {
        let params = [
            ("ep_id", ep_id.to_string()),
            ("cid", cid.to_string()),
            ("qn", "80".to_string()),
            ("fnval", "16".to_string()),
            ("fourk", "1".to_string()),
        ];
        let body: PgcGateResp = self.get_query_json(PGC_PLAYURL_URL, &params, None).await?;
        Ok(PgcGate {
            code: body.code,
            message: body.message,
        })
    }

    async fn archive_page(&self, url: &str, params: &[(&str, String)]) -> Result<CardPage> {
        let body: ArchiveResp = self.get_query_json(url, params, None).await?;
        if body.code != 0 {
            bail!("获取分区视频失败（code={}）", body.code);
        }
        let data = body.data.unwrap_or_default();
        let page = data.page.as_ref().map(|p| p.num).unwrap_or(1).max(1);
        let size = data.page.as_ref().map(|p| p.size).unwrap_or(0).max(1);
        let items: Vec<VideoCard> = data.archives.into_iter().map(card_from).collect();
        let has_more = size > 1 && items.len() as i64 >= size;
        Ok(CardPage {
            page,
            has_more,
            items,
        })
    }

    /// The 全站 (`rid = 0`) or one region's ranking list (single page).
    pub async fn ranking(&self, rid: i64) -> Result<Vec<VideoCard>> {
        let params = [("rid", rid.to_string()), ("type", "all".to_string())];
        let body: RankingResp = self
            .get_query_json(RANKING_URL, &params, Some(RANK_REFERER))
            .await?;
        if body.code != 0 {
            bail!("获取排行榜失败（code={}）", body.code);
        }
        Ok(body
            .data
            .unwrap_or_default()
            .list
            .into_iter()
            .map(card_from)
            .collect())
    }

    /// Search-box suggestions for `term` (up to 8), decoded from the suggest
    /// endpoint's `<em>` highlight markup.
    pub async fn search_suggest(&self, term: &str) -> Result<Vec<String>> {
        let term = term.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let params = [
            ("term", term.to_string()),
            ("main_ver", "v1".to_string()),
        ];
        let body: SuggestResp = self.get_query_json(SUGGEST_URL, &params, None).await?;
        let tags = body.result.map(|result| result.tag).unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        for tag in tags {
            let text = if !tag.name.trim().is_empty() {
                strip_html(&tag.name)
            } else {
                tag.value
            };
            let text = text.trim().to_string();
            if !text.is_empty() && !out.contains(&text) {
                out.push(text);
            }
            if out.len() >= 8 {
                break;
            }
        }
        Ok(out)
    }

    /// The 热搜 list: the WBI `square` endpoint, falling back to `hotword`.
    pub async fn hot_search(&self) -> Result<Vec<HotSearch>> {
        let params = vec![("limit".to_string(), "10".to_string())];
        let square = async {
            let mut resp: SquareResp = self.wbi_call(SQUARE_URL, &params, false).await?.1;
            if resp.code != 0 {
                resp = self.wbi_call(SQUARE_URL, &params, true).await?.1;
            }
            if resp.code != 0 {
                bail!("热搜加载失败（code={}）", resp.code);
            }
            let list: Vec<HotSearch> = resp
                .data
                .unwrap_or_default()
                .trending
                .list
                .into_iter()
                .map(square_to_hot)
                .collect();
            Ok::<Vec<HotSearch>, anyhow::Error>(list)
        }
        .await;
        match square {
            Ok(list) if !list.is_empty() => Ok(list),
            _ => self.hotword_fallback().await,
        }
    }

    async fn hotword_fallback(&self) -> Result<Vec<HotSearch>> {
        let body: HotwordResp = self.get_query_json(HOTWORD_URL, &[], None).await?;
        Ok(body.list.into_iter().map(hotword_to_hot).collect())
    }

    async fn get_query_json<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(&str, String)],
        referer: Option<&str>,
    ) -> Result<T> {
        let mut request = self.client.get(url).query(params);
        if let Some(referer) = referer {
            request = request.header(REFERER, referer);
        }
        let resp = request.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))
    }

    /// Download and decode an image URL into raw RGBA8 pixels.
    pub async fn fetch_image(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        let bytes = self.get_bytes(url).await?;
        decode_rgba_blocking(bytes).await
    }

    /// Download and decode an image URL into raw RGBA8 pixels.
    pub async fn fetch_avatar(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        self.fetch_image(url).await
    }

    /// Like [`Session::fetch_image`] but requests the CDN's small webp variant
    /// (`@480w_270h_1c.webp`) and falls back to the raw URL when the CDN rejects
    /// the suffix or the variant cannot be decoded.
    pub async fn fetch_thumb(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        if url.is_empty() {
            bail!("empty image url");
        }
        if url.contains('@') {
            return self.fetch_image(url).await;
        }
        let sized = format!("{url}{THUMB_SUFFIX}");
        if let Ok(bytes) = self.get_bytes(&sized).await {
            if !bytes.is_empty() {
                if let Ok(pixels) = decode_rgba_blocking(bytes).await {
                    return Ok(pixels);
                }
            }
        }
        THUMB_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        self.fetch_image(url).await
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    /// Snapshot the shared jar's cookies for the `api.bilibili.com` origin as
    /// `name=value` pairs (the header is `name=value; name2=value2`).
    fn jar_cookies(&self) -> Vec<StoredCookie> {
        let Some(text) = self.api_cookie_header() else {
            return Vec::new();
        };
        text.split("; ")
            .filter_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                Some(StoredCookie {
                    name: name.to_string(),
                    value: value.trim().to_string(),
                })
            })
            .collect()
    }

    /// Read one cookie value by name from the shared jar.
    fn cookie_value(&self, name: &str) -> Option<String> {
        let text = self.api_cookie_header()?;
        text.split("; ").find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key.trim() == name {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
    }

    /// The shared HTTP client for raw-bytes endpoints (auto-decompression
    /// disabled), reused by the offline-download worker for parallel range
    /// requests so it inherits the session cookies, referer and UA.
    pub fn raw_client(&self) -> &reqwest::Client {
        &self.raw_client
    }

    /// Raw `Cookie` header for the `api.bilibili.com` origin, if any.
    fn api_cookie_header(&self) -> Option<String> {
        let api_url = Url::parse("https://api.bilibili.com/").ok()?;
        let header = self.jar.cookies(&api_url)?;
        header.to_str().ok().map(|text| text.to_string())
    }
}

fn build_client(jar: Arc<Jar>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    headers.insert(REFERER, HeaderValue::from_static("https://www.bilibili.com/"));

    // Every request runs on the shared multi-thread runtime (see
    // [`shared_runtime`]), so a pooled keep-alive connection is driven by a
    // runtime that outlives any single request and can be safely reused. HTTP/2
    // is negotiated via ALPN, letting the pooled connection multiplex the many
    // parallel thumbnail requests.
    let client = reqwest::Client::builder()
        .cookie_provider(jar)
        .default_headers(headers)
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(25))
        .build()?;
    Ok(client)
}

/// A client with every automatic decompression disabled, so an endpoint that
/// always answers `Content-Encoding: deflate` (raw) can be decoded by hand
/// instead of failing inside reqwest's own decoder. Pooled and driven by the
/// shared runtime like [`build_client`].
fn build_raw_client(jar: Arc<Jar>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    headers.insert(REFERER, HeaderValue::from_static("https://www.bilibili.com/"));
    let client = reqwest::Client::builder()
        .cookie_provider(jar)
        .default_headers(headers)
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(25))
        .gzip(false)
        .brotli(false)
        .build()?;
    Ok(client)
}

/// Decode encoded image bytes (jpeg/png/webp) into raw RGBA8 pixels.
fn decode_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

/// Decode encoded image bytes on the blocking pool instead of the async workers.
///
/// Decoding is CPU-bound and can take milliseconds per image; running it inline
/// would occupy one of the shared runtime's four workers and stall the other
/// network work queued behind it while a whole list of covers decodes.
async fn decode_rgba_blocking(bytes: Vec<u8>) -> Result<(Vec<u8>, u32, u32)> {
    match tokio::task::spawn_blocking(move || decode_rgba(&bytes)).await {
        Ok(result) => result,
        Err(err) => Err(anyhow!("图片解码任务失败：{err}")),
    }
}

/// Inflate a raw-or-zlib HTTP `deflate` body, trying the zlib wrapper first.
fn inflate_deflate(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(body)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Some(out);
    }
    out.clear();
    if flate2::read::DeflateDecoder::new(body)
        .read_to_end(&mut out)
        .is_ok()
    {
        return Some(out);
    }
    None
}

// ---------------------------------------------------------------------------
// Cookie parsing (Set-Cookie -> StoredCookie)
// ---------------------------------------------------------------------------

fn parse_set_cookie(headers: &HeaderMap) -> Vec<StoredCookie> {
    let mut out = Vec::new();
    for value in headers.get_all(SET_COOKIE).iter() {
        if let Ok(text) = value.to_str() {
            if let Some(cookie) = parse_one_set_cookie(text) {
                out.push(cookie);
            }
        }
    }
    out
}

fn parse_one_set_cookie(text: &str) -> Option<StoredCookie> {
    let pair = text.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    if name.trim().is_empty() {
        return None;
    }
    Some(StoredCookie {
        name: name.trim().to_string(),
        value: value.trim().to_string(),
    })
}

/// Union two cookie lists by name, preferring entries already in `primary`.
fn merge_cookies(primary: Vec<StoredCookie>, secondary: Vec<StoredCookie>) -> Vec<StoredCookie> {
    let mut out = primary;
    for cookie in secondary {
        if !out.iter().any(|existing| existing.name == cookie.name) {
            out.push(cookie);
        }
    }
    out
}

/// Percent-encode one `application/x-www-form-urlencoded` value.
fn encode_component(value: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Join numeric ids as the comma-separated `add_media_ids` list the API wants.
fn join_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn extract_wbi_key(url: &str) -> String {    let file = url.rsplit('/').next().unwrap_or("");
    file.split('.').next().unwrap_or("").to_string()
}

fn extract_wbi_keys(data: &NavData) -> Option<(String, String)> {
    let wbi = data.wbi_img.as_ref()?;
    let img_key = extract_wbi_key(&wbi.img_url);
    let sub_key = extract_wbi_key(&wbi.sub_url);
    if img_key.is_empty() || sub_key.is_empty() {
        return None;
    }
    Some((img_key, sub_key))
}

fn feed_params(
    fresh_idx: u64,
    brush: u64,
    fetch_row: u64,
    last_showlist: &str,
) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = vec![
        ("web_location".into(), "1430650".into()),
        ("fresh_type".into(), "4".into()),
        ("ps".into(), "30".into()),
        ("fresh_idx".into(), fresh_idx.to_string()),
        ("fresh_idx_1h".into(), fresh_idx.to_string()),
        ("brush".into(), brush.to_string()),
        ("fetch_row".into(), fetch_row.to_string()),
        ("feed_version".into(), "V8".into()),
        ("homepage_ver".into(), "1".into()),
    ];
    if !last_showlist.is_empty() {
        params.push(("last_showlist".into(), last_showlist.to_string()));
    }
    params
}

/// `fnval` is a bitmask of the DASH features the client understands. Asking for
/// a feature the account may not use is harmless: the API simply omits those
/// streams, so the mask can always request the full matrix.
const FNVAL_DASH: i64 = 16; // DASH
const FNVAL_HDR: i64 = 64; // HDR
const FNVAL_4K: i64 = 128; // 4K
const FNVAL_DOLBY_AUDIO: i64 = 256; // 杜比全景声
const FNVAL_DOLBY_VISION: i64 = 512; // 杜比视界
const FNVAL_8K: i64 = 1024; // 8K
const FNVAL_AV1: i64 = 2048; // AV1

/// Every documented DASH feature at once (`4048`): DASH + HDR + 4K + Dolby
/// audio + Dolby Vision + 8K + AV1. This is what a PC web client requests, and
/// it makes the response describe every stream the account is entitled to.
pub const FNVAL_ALL: i64 = FNVAL_DASH
    | FNVAL_HDR
    | FNVAL_4K
    | FNVAL_DOLBY_AUDIO
    | FNVAL_DOLBY_VISION
    | FNVAL_8K
    | FNVAL_AV1;

/// `fnval` sent with `playurl`. `BILI_QUALITY_FNVAL=<n>` overrides the mask so
/// the QA hook can compare the returned stream set against an older request
/// (e.g. `16` = DASH only); the default is always [`FNVAL_ALL`].
pub fn quality_fnval() -> i64 {
    std::env::var("BILI_QUALITY_FNVAL")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| (0..=4095).contains(value))
        .unwrap_or(FNVAL_ALL)
}

/// Full signed parameter set for `playurl`. The `dm_img_*` / `web_location`
/// entries are required since 2026-06 to avoid an HTTP 412 risk-control block.
/// Form body of one `x/click-interface/web/heartbeat` call.
///
/// `type=3` / `sub_type=0` / `dt=2` / `play_type` mirror what the web player
/// sends; `video_duration` keeps the history entry's progress bar proportional.
#[allow(clippy::too_many_arguments)]
fn heartbeat_body(
    aid: u64,
    bvid: &str,
    cid: u64,
    played_s: u64,
    watched_s: u64,
    duration_s: u64,
    start_ts: u64,
    play_type: u64,
    csrf: &str,
) -> String {
    format!(
        "aid={aid}&bvid={}&cid={cid}&played_time={played_s}&realtime={watched_s}\
         &start_ts={start_ts}&type=3&sub_type=0&dt=2&play_type={play_type}\
         &video_duration={duration_s}&csrf={}",
        encode_component(bvid),
        encode_component(csrf),
    )
}

fn playurl_params(bvid: &str, cid: u64) -> Vec<(String, String)> {    vec![
        ("bvid".into(), bvid.to_string()),
        ("cid".into(), cid.to_string()),
        ("qn".into(), "80".into()),
        ("fnval".into(), quality_fnval().to_string()),
        ("fnver".into(), "0".into()),
        ("fourk".into(), "1".into()),
        ("platform".into(), "pc".into()),
        ("otype".into(), "json".into()),
        ("high_quality".into(), "1".into()),
        ("web_location".into(), "1315873".into()),
        ("dm_img_list".into(), "[]".into()),
        ("dm_img_str".into(), random_alnum(16, 64)),
        ("dm_cover_img_str".into(), random_alnum(16, 64)),
        ("dm_img_inter".into(), "{\"ds\":[],\"wh\":[0,0,0],\"of\":[0,0,0]}".into()),
    ]
}

/// Cheap non-cryptographic random alphanumeric string in `[min, max]` length.
/// Seeded from the wall clock; no `rand` dependency is available.
fn random_alnum(min: usize, max: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let min = min.max(1);
    let max = max.max(min);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let span = max - min + 1;
    let len = min + (splitmix64(&mut state) as usize % span);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        let index = splitmix64(&mut state) as usize % CHARS.len();
        out.push(CHARS[index] as char);
    }
    out
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Collapse the DASH audio list to one entry per `id`, keeping the highest
/// bitrate for each, and sort high→low so the menu reads 192K…64K.
fn collect_audio_tracks(tracks: &[DashTrack]) -> Vec<AudioTrack> {
    let mut best: Vec<AudioTrack> = Vec::new();
    for track in tracks {
        let urls = track_urls(track);
        if urls.is_empty() {
            continue;
        }
        let url = urls[0].clone();
        match best.iter_mut().find(|entry| entry.id == track.id) {
            Some(entry) => {
                if track.bandwidth > entry.bandwidth {
                    entry.bandwidth = track.bandwidth;
                    entry.url = url;
                    entry.urls = urls;
                }
            }
            None => best.push(AudioTrack {
                id: track.id,
                bandwidth: track.bandwidth,
                url,
                urls,
                vip: false,
                dolby: false,
                hi_res: false,
            }),
        }
    }
    best.sort_by(|left, right| right.bandwidth.cmp(&left.bandwidth));
    best
}

/// Append the premium (大会员) audio streams that live outside `dash.audio[]`:
/// `dash.flac` (Hi-Res, id 30251) and `dash.dolby` (Dolby Atmos, id 30250).
fn append_vip_audio(tracks: &mut Vec<AudioTrack>, dash: &Dash) {
    let flac = dash
        .flac
        .as_ref()
        .and_then(|flac| flac.audio.as_ref())
        .and_then(|audio| audio.first());
    if let Some(track) = flac {
        let urls = track_urls(track);
        if !urls.is_empty() {
            tracks.push(AudioTrack {
                id: if track.id != 0 { track.id } else { 30251 },
                bandwidth: track.bandwidth,
                url: urls[0].clone(),
                urls,
                vip: true,
                dolby: false,
                hi_res: true,
            });
        }
    }

    let dolby = dash
        .dolby
        .as_ref()
        .and_then(|dolby| dolby.audio.as_ref())
        .and_then(|audio| audio.first());
    if let Some(track) = dolby {
        let urls = track_urls(track);
        if !urls.is_empty() {
            tracks.push(AudioTrack {
                id: if track.id != 0 { track.id } else { 30250 },
                bandwidth: track.bandwidth,
                url: urls[0].clone(),
                urls,
                vip: true,
                dolby: true,
                hi_res: false,
            });
        }
    }
}

/// Short human summary of a `privilege/my` payload for the 会员中心 view.
fn summarize_vip(value: &serde_json::Value) -> String {
    let Some(data) = value.get("data").filter(|data| !data.is_null()) else {
        return String::new();
    };
    let is_vip = data.get("is_vip").and_then(|v| v.as_bool()).unwrap_or(false)
        || data.get("vip_status").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
    let count = data
        .get("list")
        .and_then(|v| v.as_array())
        .map(|list| list.len())
        .unwrap_or(0);
    let label = if is_vip { "大会员" } else { "非大会员" };
    format!("{label} · 特权 {count} 项")
}

/// Ordered fetch candidates for a DASH track: the primary base URL first, then
/// every backup mirror, de-duplicated.
fn track_urls(track: &DashTrack) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for candidate in [&track.base_url, &track.base_url_snake] {
        push_unique(&mut out, candidate);
    }
    for candidate in track.backup_url.iter().chain(track.backup_url_snake.iter()) {
        push_unique(&mut out, candidate);
    }
    out
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !out.iter().any(|existing| existing == value) {
        out.push(value.to_string());
    }
}

/// Native frame rate of one representation, clamped to a plausible range
/// (`30.0` when the manifest omits it or the value is unusable).
fn track_frame_rate(track: &DashTrack) -> f64 {
    track
        .frame_rate_camel
        .as_ref()
        .or(track.frame_rate_snake.as_ref())
        .map(|value| value.as_f64())
        .filter(|value| *value > 0.0 && *value <= 240.0)
        .unwrap_or(30.0)
}

/// Collapse the DASH video list to one entry per `(qn, codecid)` pair, keeping
/// the highest bitrate for each, and sort `qn` high→low then bitrate high→low
/// so the first entry per quality is the best variant of that quality.
///
/// This is the compact list the player's quality menu consumes; the full,
/// un-collapsed matrix is [`collect_quality_options`].
fn collect_video_tracks(tracks: &[DashTrack]) -> Vec<VideoTrack> {
    let mut best: Vec<VideoTrack> = Vec::new();
    for track in tracks {
        let urls = track_urls(track);
        if urls.is_empty() {
            continue;
        }
        let url = urls[0].clone();
        let frame_rate = track_frame_rate(track);
        match best
            .iter_mut()
            .find(|entry| entry.id == track.id && entry.codecid == track.codecid)
        {
            Some(entry) => {
                if track.bandwidth > entry.bandwidth {
                    entry.bandwidth = track.bandwidth;
                    entry.url = url;
                    entry.urls = urls;
                    entry.frame_rate = frame_rate;
                }
            }
            None => best.push(VideoTrack {
                id: track.id,
                codecid: track.codecid,
                codecs: track.codecs.clone(),
                width: track.width,
                height: track.height,
                bandwidth: track.bandwidth,
                frame_rate,
                url,
                urls,
            }),
        }
    }
    best.sort_by(|left, right| {
        right
            .id
            .cmp(&left.id)
            .then_with(|| right.bandwidth.cmp(&left.bandwidth))
    });
    best
}

/// One entry per raw `dash.video[]` representation, **without** collapsing by
/// `qn`: the same quality legitimately comes back several times (AVC + HEVC +
/// AV1, and more than one bitrate per codec), and every one of those streams is
/// a distinct thing the app can play. Sorted `qn` high→low, then bitrate
/// high→low, so the best variant of each quality sits first.
fn collect_quality_options(tracks: &[DashTrack]) -> Vec<QualityOption> {
    let mut out: Vec<QualityOption> = Vec::new();
    for track in tracks {
        let urls = track_urls(track);
        if urls.is_empty() {
            continue;
        }
        let (hdr, dolby_vision, eight_k) = quality_flags(track.id);
        let url = urls[0].clone();
        let backup_urls = urls.into_iter().skip(1).collect();
        out.push(QualityOption {
            qn: track.id,
            label: qn_label(track.id).to_string(),
            width: track.width,
            height: track.height,
            fps: track_frame_rate(track) as f32,
            codecid: track.codecid,
            codec: codec_label(track.codecid),
            bandwidth_bps: track.bandwidth,
            hdr,
            dolby_vision,
            eight_k,
            url,
            backup_urls,
        });
    }
    out.sort_by(|left, right| {
        right
            .qn
            .cmp(&left.qn)
            .then_with(|| right.bandwidth_bps.cmp(&left.bandwidth_bps))
    });
    out
}

/// The special tiers the API encodes only in the `qn` (there is no per-track
/// boolean in `dash.video[]`): HDR is 125 (真彩) / 129 (Vivid), Dolby Vision is
/// 126, and 8K is 127.
fn quality_flags(qn: i64) -> (bool, bool, bool) {
    (matches!(qn, 125 | 129), qn == 126, qn == 127)
}

/// Quality to start with when nothing better is known: 480P when present,
/// otherwise the lowest available.
///
/// This is only the decoder's last-resort fallback for a `qn` with no stream.
/// The quality a video actually opens on is [`crate::player::initial_quality`]:
/// the stored 默认画质 when it is playable, otherwise `min(highest playable,
/// 1080P)`.
fn default_quality(tracks: &[VideoTrack]) -> i64 {
    tracks
        .iter()
        .find(|track| track.id == 32)
        .or_else(|| tracks.iter().min_by_key(|track| track.id))
        .map(|track| track.id)
        .unwrap_or(0)
}

/// Select every video quality plus one audio URL from a DASH/DURL response.
/// Every quality tier the response advertises or returned, marking the ones
/// this account may not play.
///
/// `accept_quality` / `support_formats` name the video's tiers; `dash.video[]`
/// is the subset the signed-in account may play. A tier with no stream is a
/// locked 大会员 row. Tiers are ordered high→low, the same as the stream rows.
///
/// `native_height` is the upload's own `dimension.height` (`0` when unknown).
/// It is used only when the advertised list stops at what the account may play
/// — see [`possible_tiers`] — so a 1080p upload never gets a 4K/8K row.
fn build_quality_tiers(data: &PlayUrlData, native_height: u32) -> Vec<QualityTier> {
    let video = data
        .dash
        .as_ref()
        .map(|dash| dash.video.as_slice())
        .unwrap_or(&[]);
    let mut played: Vec<i64> = Vec::new();
    let mut fps = 0.0f64;
    for track in video {
        if !played.contains(&track.id) {
            played.push(track.id);
        }
        let rate = track_frame_rate(track);
        if rate > fps && rate < 200.0 {
            fps = rate;
        }
    }
    // The video's own frame rate for tiers whose id does not name one (4K,
    // HDR): the permitted streams are the same source and carry it.
    let observed_fps = if fps > 1.0 { fps } else { 30.0 };
    let fps = observed_fps.round() as u32;

    // Union of what the video advertises and what actually came back: a stream
    // the API did not list (e.g. `100` 智能修复) is still a playable tier.
    let mut qns: Vec<i64> = Vec::new();
    for qn in data
        .accept_quality
        .iter()
        .copied()
        .chain(data.support_formats.iter().map(|format| format.quality))
        .chain(played.iter().copied())
    {
        if qn > 0 && !qns.contains(&qn) {
            qns.push(qn);
        }
    }

    // Nothing advertised is missing from `dash.video[]`, so this response says
    // nothing about a tier the account cannot play. The upload's own resolution
    // is the remaining evidence: derive the tiers such a source can have and
    // mark them locked. Always additive — a derived `qn` that came back stays
    // playable.
    if native_height > 0 && qns.iter().all(|qn| played.contains(qn)) {
        for qn in possible_tiers(native_height, observed_fps) {
            if !qns.contains(&qn) {
                qns.push(qn);
            }
        }
    }

    let mut tiers: Vec<QualityTier> = qns
        .into_iter()
        .map(|qn| QualityTier {
            qn,
            label: tier_label(data, qn),
            detail: tier_detail(qn, fps),
            locked: !played.contains(&qn),
        })
        .collect();
    tiers.sort_by(|left, right| right.qn.cmp(&left.qn));
    tiers
}

/// Tiers a source `native_height` pixels tall can have **beyond** the ones this
/// account was allowed to play, derived only from evidence the API gives:
///
/// * `≥4320` → 8K (`127`); `≥2160` → 4K (`120`).
/// * 1080P+ (`112`) is a ≥4K derivative and only exists for a source that is not
///   already 60 fps; 1080P60 (`116`) needs a ≥50 fps source.
/// * HDR (`125` / `129`) and 杜比视界 (`126`) need a response flag the view API
///   does not carry, so they are never derived.
///
/// Everything returned here is a tier the video could actually have, which is
/// what keeps the 画质 menu from offering 8K on a 1080p upload.
fn possible_tiers(native_height: u32, fps: f64) -> Vec<i64> {
    let mut qns = Vec::new();
    if native_height >= 4320 {
        qns.push(127);
    }
    if native_height >= 2160 {
        qns.push(120);
    }
    let sixty = fps >= 50.0;
    if native_height >= 2160 && !sixty {
        qns.push(112);
    }
    if native_height >= 1080 && sixty {
        qns.push(116);
    }
    qns
}

/// The app-wide label for a tier; an id [`qn_label`] does not know falls back
/// to the API's own description instead of showing `默认`.
fn tier_label(data: &PlayUrlData, qn: i64) -> String {
    let known = qn_label(qn);
    if known != "默认" {
        return known.to_string();
    }
    if let Some(index) = data.accept_quality.iter().position(|id| *id == qn) {
        if let Some(text) = data
            .accept_description
            .get(index)
            .filter(|text| !text.is_empty())
        {
            return text.clone();
        }
    }
    data.support_formats
        .iter()
        .find(|format| format.quality == qn)
        .map(|format| {
            if format.new_description.is_empty() {
                format.display_desc.clone()
            } else {
                format.new_description.clone()
            }
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("qn {qn}"))
}

/// The expected picture of a tier: `3840×2160 · 30fps`.
///
/// 74/116 are the 60 fps tiers by definition; every other tier runs at the
/// video's own frame rate. A tier with no documented size still gets copy that
/// says why it cannot be picked.
fn tier_detail(qn: i64, video_fps: u32) -> String {
    let size = match qn {
        6 => (320, 240),
        16 => (640, 360),
        32 => (852, 480),
        64 | 74 => (1280, 720),
        80 | 100 | 112 | 116 => (1920, 1080),
        120 | 125 | 126 | 129 => (3840, 2160),
        127 => (7680, 4320),
        _ => return "大会员专享".to_string(),
    };
    let fps = match qn {
        74 | 116 => 60,
        _ => video_fps,
    };
    format!("{}×{} · {}fps", size.0, size.1, fps)
}

fn build_play_info(data: PlayUrlData) -> Option<PlayInfo> {
    let mut video_tracks = Vec::new();
    let mut qualities = Vec::new();
    let mut audio_tracks = Vec::new();
    let mut duration = 0u64;

    if let Some(dash) = data.dash.as_ref() {
        duration = dash.duration;
        video_tracks = collect_video_tracks(&dash.video);
        qualities = collect_quality_options(&dash.video);
        audio_tracks = collect_audio_tracks(&dash.audio);
        append_vip_audio(&mut audio_tracks, dash);
    }

    if duration == 0 {
        duration = data.timelength / 1000;
    }
    if video_tracks.is_empty() {
        if let Some(first) = data.durl.first() {
            if !first.url.is_empty() {
                video_tracks.push(VideoTrack {
                    id: 0,
                    codecid: 0,
                    codecs: String::new(),
                    width: 0,
                    height: 0,
                    bandwidth: 0,
                    frame_rate: 30.0,
                    url: first.url.clone(),
                    urls: vec![first.url.clone()],
                });
            }
        }
    }
    if video_tracks.is_empty() {
        return None;
    }
    let default_qn = default_quality(&video_tracks);
    Some(PlayInfo {
        video_tracks,
        qualities,
        default_qn,
        audio_tracks,
        duration,
    })
}

mod cdn_select {
    use super::*;

    // ---------------------------------------------------------------------------
    // CDN classification / probe / selection
    // ---------------------------------------------------------------------------
    //
    // Bilibili answers one play URL plus backups, and the *host* of that URL says
    // far more about throughput than the signature does: the `upsig` parameter
    // covers only the `uparams` query keys, never the hostname, so a URL can be
    // moved to another `*.bilivideo.com` edge without invalidating it.
    //
    // Sources, best → worst:
    //   * **Mirror**  `upos-{sz,hz,bstar}-mirror{provider}.bilivideo.com`
    //                 (ali / cos / hw / bd / cf / 08c …), server-side rewrite edges.
    //   * **UPOS**    `upos-sz-estg…`, object storage: mediocre but usually alive.
    //   * **BCache**  `cn-…bilivideo.{com,cn}`, self-built IDC, varies by region.
    //   * **MCDN**    host contains `mcdn` (P2P), and **IP:Port** / `/v1/resource`
    //                 forms, both worst and not rewritable at all.
    //
    // The `os=` query parameter (`ali`, `cos`, `upos`, `bcache`, `mcdn`) is used as
    // a cross-check for unknown hosts only. Known hosts win on purpose: a rewritten
    // mirror keeps the original `os=` byte-for-byte (that is what keeps the
    // signature valid), so trusting `os=` over the host would make every rewritten
    // mirror look like whatever the original source was.
    //
    // Probing is strictly opt-in (the caller has to call [`Session::pick_fastest`]);
    // there is no background timer here, so nothing competes with the player's first
    // buffer. Probes run sequentially because concurrent probes share one uplink and
    // each would measure a fraction of it.

    /// Which kind of Bilibili CDN serves one media URL.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CdnKind {
        /// `upos-*-mirror*` server-side rewrite edge (best).
        Mirror,
        /// `upos-*` object storage.
        Upos,
        /// `cn-*` self-built IDC cache.
        BCache,
        /// P2P / `mcdn` host (worst).
        Mcdn,
        /// Bare `IP:Port` or `/v1/resource` form (worst, never rewritable).
        IpPort,
        /// Anything else (unknown brand, empty host, …).
        Other,
    }

    impl CdnKind {
        /// Short lowercase label used in `BILI_DEBUG_LOG` lines.
        pub fn label(self) -> &'static str {
            match self {
                CdnKind::Mirror => "mirror",
                CdnKind::Upos => "upos",
                CdnKind::BCache => "bcache",
                CdnKind::Mcdn => "mcdn",
                CdnKind::IpPort => "ip",
                CdnKind::Other => "other",
            }
        }
    }

    /// Lower is better. Mirrors first, the two P2P/IP forms last.
    pub fn cdn_rank(kind: CdnKind) -> u8 {
        match kind {
            CdnKind::Mirror => 0,
            CdnKind::Upos => 1,
            CdnKind::BCache => 2,
            CdnKind::Other => 3,
            CdnKind::Mcdn => 4,
            CdnKind::IpPort => 5,
        }
    }

    /// Split `scheme://authority<rest>` into its three byte-exact parts.
    fn url_scheme_authority_rest(url: &str) -> Option<(&str, &str, &str)> {
        let (scheme, after) = url.split_once("://")?;
        if scheme.is_empty() {
            return None;
        }
        let end = after
            .find(|ch| ch == '/' || ch == '?' || ch == '#')
            .unwrap_or(after.len());
        let (authority, rest) = after.split_at(end);
        if authority.is_empty() {
            return None;
        }
        Some((scheme, authority, rest))
    }

    /// `authority` without `user:pass@`.
    fn url_authority(url: &str) -> Option<&str> {
        url_scheme_authority_rest(url).map(|(_, authority, _)| authority)
    }

    /// Host part of an authority: userinfo and a numeric port removed.
    fn authority_host(authority: &str) -> &str {
        let host_port = match authority.rsplit_once('@') {
            Some((_, rest)) => rest,
            None => authority,
        };
        if let Some(rest) = host_port.strip_prefix('[') {
            // IPv6 literal: `[::1]:443`.
            return match rest.split_once(']') {
                Some((host, _)) => host,
                None => host_port,
            };
        }
        match host_port.rsplit_once(':') {
            Some((host, port))
                if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                host
            }
            _ => host_port,
        }
    }

    /// Lowercased host of `url` (userinfo and port removed), for callers
    /// outside this module that must match a URL to the host a probe measured.
    pub fn url_host(url: &str) -> Option<String> {
        url_authority(url)
            .map(authority_host)
            .filter(|host| !host.is_empty())
            .map(|host| host.to_ascii_lowercase())
    }

    /// Path part of a URL (no query, no fragment); empty when unparseable.
    fn url_path(url: &str) -> &str {
        match url_scheme_authority_rest(url) {
            Some((_, _, rest)) => {
                let end = rest
                    .find(|ch| ch == '?' || ch == '#')
                    .unwrap_or(rest.len());
                &rest[..end]
            }
            None => "",
        }
    }

    /// Raw query string without the leading `?` (and without any fragment).
    fn url_query(url: &str) -> Option<&str> {
        let (_, query) = url.split_once('?')?;
        Some(match query.split_once('#') {
            Some((query, _)) => query,
            None => query,
        })
    }

    /// One query parameter by exact key name, undecoded (the keys Bilibili emits
    /// are plain ASCII, so decoding would only add failure modes).
    fn url_query_param<'a>(url: &'a str, name: &str) -> Option<&'a str> {
        for pair in url_query(url)?.split('&') {
            let (key, value) = match pair.split_once('=') {
                Some(parts) => parts,
                None => (pair, ""),
            };
            if key == name {
                return Some(value);
            }
        }
        None
    }

    fn is_ip_literal(host: &str) -> bool {
        !host.is_empty() && host.parse::<std::net::IpAddr>().is_ok()
    }

    /// `upos-(sz|hz|bstar)-mirror([0-9a-z]+)\.(bilivideo\.com|akamaized\.net)`
    fn is_mirror_host(host: &str) -> bool {
        let Some(rest) = host.strip_prefix("upos-") else {
            return false;
        };
        let Some((scope, tail)) = rest.split_once('-') else {
            return false;
        };
        if !matches!(scope, "sz" | "hz" | "bstar") {
            return false;
        }
        let Some(provider) = tail.strip_prefix("mirror") else {
            return false;
        };
        let label = match provider.strip_suffix(".bilivideo.com") {
            Some(label) => label,
            None => match provider.strip_suffix(".akamaized.net") {
                Some(label) => label,
                None => return false,
            },
        };
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    }

    /// Classify one media URL by host, with `os=` as a fallback for unknown hosts.
    pub fn classify_cdn(url: &str) -> CdnKind {
        let Some(authority) = url_authority(url) else {
            return CdnKind::Other;
        };
        let host = authority_host(authority).to_ascii_lowercase();
        if host.is_empty() {
            return CdnKind::Other;
        }
        // `IP:Port/v1/resource` is the PCDN form; it cannot be rewritten either.
        if url_path(url).starts_with("/v1/resource") || is_ip_literal(&host) {
            return CdnKind::IpPort;
        }
        if host.contains("mcdn") {
            return CdnKind::Mcdn;
        }
        if is_mirror_host(&host) {
            return CdnKind::Mirror;
        }
        let bilivideo = host.ends_with(".bilivideo.com") || host.ends_with(".bilivideo.cn");
        if bilivideo && host.starts_with("upos-") {
            return CdnKind::Upos;
        }
        if bilivideo && host.starts_with("cn-") {
            return CdnKind::BCache;
        }
        match url_query_param(url, "os").map(str::to_ascii_lowercase) {
            Some(os) if os == "mcdn" => CdnKind::Mcdn,
            Some(os) if os == "bcache" => CdnKind::BCache,
            Some(os)
                if matches!(
                    os.as_str(),
                    "upos" | "ali" | "alib" | "cos" | "cosb" | "hw" | "hwb" | "bd" | "cf"
                ) =>
            {
                CdnKind::Upos
            }
            _ => CdnKind::Other,
        }
    }

    /// Whether the host of `url` may be swapped while keeping the signature valid.
    ///
    /// Only the two PCDN shapes are refused: `IP:Port` hosts and `/v1/resource`
    /// paths, whose query carries a different token scheme.
    pub fn is_rewritable(url: &str) -> bool {
        if url.is_empty() || url_path(url).starts_with("/v1/resource") {
            return false;
        }
        let Some(authority) = url_authority(url) else {
            return false;
        };
        let host = authority_host(authority);
        !host.is_empty() && !is_ip_literal(host)
    }

    /// Replace the host of `url`, leaving path, query and fragment byte-identical.
    ///
    /// `akamaized.net` is refused as a *target*: it needs its own `hdnts` token and
    /// answers 403 for a URL signed for `bilivideo.com`.
    pub fn rewrite_host(url: &str, host: &str) -> Option<String> {
        let host = host.trim();
        if host.is_empty() || host.contains('/') || host.contains("://") || !is_rewritable(url) {
            return None;
        }
        let lower = host.to_ascii_lowercase();
        if lower.ends_with("akamaized.net") {
            return None;
        }
        let (scheme, _, rest) = url_scheme_authority_rest(url)?;
        Some(format!("{scheme}://{host}{rest}"))
    }

    /// Mirror providers seen in Bilibili's own backup lists, most reliable
    /// first. `cf` is last: it answered NXDOMAIN from this region.
    const CDN_MIRROR_PROVIDERS: &[&str] =
        &["ali", "alib", "cos", "cosb", "hw", "hwb", "bd", "08c", "cf"];

    /// Mirror hosts to try when a video's own backups do not fill the probe
    /// slots.
    ///
    /// `sz` edges come before `hz`: on 2026-10-20 every `upos-hz-*` name except
    /// `upos-hz-mirrorakam.akamaized.net` answered NXDOMAIN, so probing those
    /// first would only waste slots on dead names.
    /// `BILI_CDN_HOSTS=host1,host2` replaces the list (QA runs).
    pub fn curated_mirror_hosts() -> Vec<String> {
        if let Ok(value) = std::env::var("BILI_CDN_HOSTS") {
            let hosts: Vec<String> = value
                .split(',')
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty() && !host.ends_with("akamaized.net"))
                .collect();
            if !hosts.is_empty() {
                return hosts;
            }
        }
        let mut hosts = Vec::with_capacity(CDN_MIRROR_PROVIDERS.len() * 2);
        for scope in ["sz", "hz"] {
            for provider in CDN_MIRROR_PROVIDERS {
                hosts.push(format!("upos-{scope}-mirror{provider}.bilivideo.com"));
            }
        }
        hosts
    }

    /// Discard this much before the measurement window starts (handshake + TCP
    /// slow start).
    const PROBE_WARMUP_BYTES: u64 = 256 * 1024;
    /// …or this much wall time, whichever comes first.
    const PROBE_WARMUP_MS: u64 = 500;
    /// Measurement window length.
    const PROBE_WINDOW: Duration = Duration::from_millis(1200);
    /// Measurement window byte cap.
    const PROBE_MAX_BYTES: u64 = 4 * 1024 * 1024;
    /// Hard per-host timeout (connect + headers + body).
    const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
    /// Hosts probed per selection round.
    const PROBE_MAX_HOSTS: usize = 4;
    /// A measured host is reused for this long, so the next video starts warm.
    const CDN_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
    /// Hosts that failed hard are skipped for this long.
    const CDN_BLACKLIST_TTL: Duration = Duration::from_secs(5 * 60);
    /// Only switch when the winner is at least this much faster (noise guard).
    const CDN_SWITCH_GAIN: f64 = 1.25;

    type CdnCache = std::collections::HashMap<String, (u64, Instant)>;

    static CDN_CACHE: OnceLock<Mutex<CdnCache>> = OnceLock::new();
    static CDN_BLACKLIST: OnceLock<Mutex<std::collections::HashMap<String, Instant>>> = OnceLock::new();

    fn cdn_cache_cell() -> &'static Mutex<CdnCache> {
        CDN_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
    }

    fn cdn_blacklist_cell() -> &'static Mutex<std::collections::HashMap<String, Instant>> {
        CDN_BLACKLIST.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
    }

    fn cdn_cache_get(host: &str) -> Option<u64> {
        let guard = cdn_cache_cell().lock().ok()?;
        let (bps, at) = guard.get(host)?;
        if at.elapsed() < CDN_CACHE_TTL {
            Some(*bps)
        } else {
            None
        }
    }

    fn cdn_cache_put(host: &str, bps: u64) {
        if let Ok(mut guard) = cdn_cache_cell().lock() {
            guard.retain(|_, (_, at)| at.elapsed() < CDN_CACHE_TTL);
            guard.insert(host.to_string(), (bps, Instant::now()));
        }
    }

    fn cdn_blacklisted(host: &str) -> bool {
        match cdn_blacklist_cell().lock() {
            Ok(guard) => match guard.get(host) {
                Some(at) => at.elapsed() < CDN_BLACKLIST_TTL,
                None => false,
            },
            Err(_) => false,
        }
    }

    fn cdn_blacklist(host: &str) {
        if let Ok(mut guard) = cdn_blacklist_cell().lock() {
            guard.retain(|_, at| at.elapsed() < CDN_BLACKLIST_TTL);
            guard.insert(host.to_string(), Instant::now());
        }
    }

    /// One host's probe outcome.
    #[derive(Clone, Debug)]
    pub struct CdnMeasurement {
        pub host: String,
        pub kind: CdnKind,
        /// Sustained bytes/s over the measurement window; `None` on failure.
        pub bps: Option<u64>,
        pub error: Option<String>,
    }

    /// The stream URL the player should use, plus what the probe saw.
    #[derive(Clone, Debug)]
    pub struct CdnChoice {
        pub url: String,
        pub host: String,
        pub kind: CdnKind,
        pub measured: Vec<CdnMeasurement>,
        /// The winning bitrate came from the process-wide cache, not this round.
        pub from_cache: bool,
    }

    /// Compact reason for a probe failure. `reqwest::Error`'s own `Display`
    /// embeds the full signed URL, so only the category and the innermost
    /// source (e.g. `dns error: …`) are kept.
    fn describe_req_error(err: &reqwest::Error) -> String {
        let kind = if err.is_timeout() {
            "timeout"
        } else if err.is_connect() {
            "connect"
        } else if err.is_body() {
            "body"
        } else if err.is_decode() {
            "decode"
        } else if err.is_status() {
            "status"
        } else {
            "request"
        };
        let mut detail: Option<String> = None;
        let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
        while let Some(current) = source {
            detail = Some(current.to_string());
            source = current.source();
        }
        match detail {
            Some(detail) => format!("{kind}: {detail}"),
            None => kind.to_string(),
        }
    }

    /// Selection policy shared by the full probe and the cache-only fast path.
    ///
    /// Returns `(url, host, kind, gain)` for the source that should replace
    /// `current`, or `None` when the current source stays. Only measurements in
    /// `measured` compete; a host with `bps == None` counts as failed.
    fn decide(
        sources: &[String],
        current: &str,
        measured: &[CdnMeasurement],
    ) -> Option<(String, String, CdnKind, f64)> {
        let current_host = url_host(current).unwrap_or_default();
        let current_kind = classify_cdn(current);

        // A P2P / IP:Port source is only worth taking when the current
        // source is already one of those (or is known dead): otherwise the
        // best measured *non-P2P* host wins even when a mcdn edge probes
        // faster, because PCDN throughput collapses under real playback.
        let allow_p2p = cdn_rank(current_kind) >= cdn_rank(CdnKind::Mcdn)
            || cdn_blacklisted(&current_host);
        let best = measured
            .iter()
            .filter(|item| allow_p2p || cdn_rank(item.kind) < cdn_rank(CdnKind::Mcdn))
            .filter_map(|item| item.bps.map(|bps| (item, bps)))
            .max_by_key(|(_, bps)| *bps);

        let (winner, winner_bps) = best?;
        let winner_host = winner.host.clone();
        if winner_host == current_host {
            return None;
        }
        let mut gain = 1.0f64;
        let current_bps = measured
            .iter()
            .find(|item| item.host == current_host)
            .and_then(|item| item.bps);
        let switch = match current_bps {
            Some(bps) if bps > 0 => {
                gain = winner_bps as f64 / bps as f64;
                gain >= CDN_SWITCH_GAIN
            }
            Some(_) => false,
            // The current source could not be measured: take the proven
            // winner unless it is a worse kind than the current one, or the
            // current host was blacklisted (then any working host beats a
            // known-dead one).
            None => {
                cdn_blacklisted(&current_host) || cdn_rank(winner.kind) <= cdn_rank(current_kind)
            }
        };
        if !switch {
            return None;
        }
        let rewrite_base = sources
            .iter()
            .find(|url| {
                url_authority(url)
                    .map(authority_host)
                    .map(|host| host.eq_ignore_ascii_case(&winner_host))
                    .unwrap_or(false)
            })
            .cloned()
            .unwrap_or_else(|| current.to_string());
        let same_host = url_authority(&rewrite_base)
            .map(authority_host)
            .map(|host| host.eq_ignore_ascii_case(&winner_host))
            .unwrap_or(false);
        let url = if same_host {
            Some(rewrite_base)
        } else {
            rewrite_host(&rewrite_base, &winner_host)
        }?;
        Some((url, winner_host, winner.kind, gain))
    }

    impl Session {
        /// Probe one host with a `Range` request and return sustained bytes/s.
        ///
        /// Warmup (first 256 KiB or 500 ms) is thrown away, then the following
        /// window is timed until 1.2 s or 4 MiB, whichever comes first. `url` must
        /// already point at `host`. Hard failures (HTTP error, timeout, connect
        /// error) blacklist the host for [`CDN_BLACKLIST_TTL`].
        async fn probe_cdn_host(&self, host: &str, url: &str) -> (CdnMeasurement, bool) {
            let kind = classify_cdn(url);
            let failure = |error: String| CdnMeasurement {
                host: host.to_string(),
                kind,
                bps: None,
                error: Some(error),
            };

            if cdn_blacklisted(host) {
                return (failure("blacklisted (5m)".to_string()), false);
            }
            if let Some(bps) = cdn_cache_get(host) {
                return (
                    CdnMeasurement {
                        host: host.to_string(),
                        kind,
                        bps: Some(bps),
                        error: None,
                    },
                    true,
                );
            }

            let range_end = PROBE_MAX_BYTES + PROBE_WARMUP_BYTES - 1;
            let response = self
                .client
                .get(url)
                .header(RANGE, format!("bytes=0-{range_end}"))
                .timeout(PROBE_TIMEOUT)
                .send()
                .await;
            let mut response = match response {
                Ok(response) => response,
                Err(err) => {
                    cdn_blacklist(host);
                    return (failure(describe_req_error(&err)), false);
                }
            };
            let status = response.status();
            if !status.is_success() {
                cdn_blacklist(host);
                return (failure(format!("http {}", status.as_u16())), false);
            }

            let started = Instant::now();
            let warmup_timeout = Duration::from_millis(PROBE_WARMUP_MS);
            let mut warmup_bytes = 0u64;
            let mut warming = true;
            let mut measured_bytes = 0u64;
            let mut measure_start = started;
            let mut read_error: Option<String> = None;

            loop {
                if !warming && (measured_bytes >= PROBE_MAX_BYTES || measure_start.elapsed() >= PROBE_WINDOW)
                {
                    break;
                }
                if started.elapsed() >= PROBE_TIMEOUT {
                    read_error = Some("timeout".to_string());
                    break;
                }
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let len = chunk.len() as u64;
                        if warming {
                            warmup_bytes += len;
                            if warmup_bytes >= PROBE_WARMUP_BYTES
                                || started.elapsed() >= warmup_timeout
                            {
                                warming = false;
                                measure_start = Instant::now();
                                measured_bytes = 0;
                            }
                            continue;
                        }
                        measured_bytes += len;
                    }
                    Ok(None) => break,
                    Err(err) => {
                        read_error = Some(describe_req_error(&err));
                        break;
                    }
                }
            }

            if measured_bytes == 0 {
                // Nothing survived the warmup: a 1-byte 200 answer, an empty body,
                // or a stream that ended during warmup. Not a hard failure.
                let reason = read_error.unwrap_or_else(|| {
                    if warming {
                        "short stream (ended in warmup)".to_string()
                    } else {
                        "no bytes in window".to_string()
                    }
                });
                return (failure(reason), false);
            }

            let seconds = measure_start.elapsed().as_secs_f64().max(0.001);
            let bps = (measured_bytes as f64 / seconds) as u64;
            if bps == 0 {
                return (failure("zero rate".to_string()), false);
            }
            cdn_cache_put(host, bps);
            (
                CdnMeasurement {
                    host: host.to_string(),
                    kind,
                    bps: Some(bps),
                    error: None,
                },
                false,
            )
        }

        /// Pick the fastest usable source for `primary` / `backups`.
        ///
        /// Candidates are ranked by [`cdn_rank`] first (so a non-MCDN source is
        /// preferred even when nothing could be measured), then up to
        /// [`PROBE_MAX_HOSTS`] hosts are probed **sequentially**. The winner is only
        /// used when it is at least [`CDN_SWITCH_GAIN`]× faster than the current
        /// source, and its URL is produced by rewriting the host only, so the
        /// signature stays valid. Measurements are cached per host for
        /// [`CDN_CACHE_TTL`]. Returns the original URL unchanged when every probe
        /// failed.
        pub async fn pick_fastest(&self, primary: &str, backups: &[String]) -> CdnChoice {
            let sources = ranked_sources(primary, backups);
            let Some(current) = sources.first().cloned() else {
                let host = url_authority(primary)
                    .map(authority_host)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let kind = classify_cdn(primary);
                crate::debug_log(&format!(
                    "cdn pick from={host} kind={} to={host} kind={} gain=1.00 measured=[] cached=false",
                    kind.label(),
                    kind.label()
                ));
                return CdnChoice {
                    url: primary.to_string(),
                    host,
                    kind,
                    measured: Vec::new(),
                    from_cache: false,
                };
            };
            let current_host = url_authority(&current)
                .map(authority_host)
                .unwrap_or("")
                .to_ascii_lowercase();
            let current_kind = classify_cdn(&current);

            let plan = probe_plan(&sources, &current);
            let mut measured: Vec<CdnMeasurement> = Vec::with_capacity(plan.len());
            let mut cached_hosts: Vec<String> = Vec::new();
            for (host, url) in plan {
                let (measurement, from_cache) = self.probe_cdn_host(&host, &url).await;
                if from_cache {
                    cached_hosts.push(host.clone());
                }
                measured.push(measurement);
            }

            let (url, host, kind, gain) = match decide(&sources, &current, &measured) {
                Some(parts) => parts,
                None => (current.clone(), current_host.clone(), current_kind, 1.0),
            };
            let from_cache = cached_hosts.iter().any(|cached| cached == &host);

            let measured_log = measured
                .iter()
                .filter_map(|item| item.bps.map(|bps| format!("{}:{:.1}", item.host, bps as f64 / 1e6)))
                .collect::<Vec<_>>()
                .join(",");
            crate::debug_log(&format!(
                "cdn pick from={} kind={} to={} kind={} gain={gain:.2} measured=[{measured_log}] cached={from_cache}",
                current_host,
                current_kind.label(),
                host,
                kind.label()
            ));

            CdnChoice {
                url,
                host,
                kind,
                measured,
                from_cache,
            }
        }

        /// The choice the process-wide host cache already supports, with **no
        /// network I/O at all**. `None` when no candidate host has a cached
        /// measurement (or the cache only holds failures), which tells the
        /// caller to start on the ranked unmeasured source and probe in the
        /// background instead of blocking the click.
        ///
        /// The decision itself is [`decide`], the same policy [`Session::pick_fastest`]
        /// applies, so a warm process opens the next video on exactly the host
        /// the full probe would have chosen.
        pub fn cached_choice(&self, primary: &str, backups: &[String]) -> Option<CdnChoice> {
            let sources = ranked_sources(primary, backups);
            let current = sources.first()?;
            let current_host = url_host(current).unwrap_or_default();
            let current_kind = classify_cdn(current);
            let mut measured: Vec<CdnMeasurement> = Vec::new();
            for (host, url) in probe_plan(&sources, current) {
                let kind = classify_cdn(&url);
                if cdn_blacklisted(&host) {
                    measured.push(CdnMeasurement {
                        host,
                        kind,
                        bps: None,
                        error: Some("blacklisted (5m)".to_string()),
                    });
                } else if let Some(bps) = cdn_cache_get(&host) {
                    measured.push(CdnMeasurement {
                        host,
                        kind,
                        bps: Some(bps),
                        error: None,
                    });
                }
            }
            if !measured.iter().any(|item| item.bps.is_some()) {
                return None;
            }
            let (url, host, kind, _) = match decide(&sources, current, &measured) {
                Some(parts) => parts,
                None => (current.clone(), current_host, current_kind, 1.0),
            };
            let from_cache = measured
                .iter()
                .any(|item| item.host == host && item.bps.is_some());
            Some(CdnChoice {
                url,
                host,
                kind,
                measured,
                from_cache,
            })
        }

        /// QA hook for `BILI_CDN_TEST=<bvid>`: fetch playurl for `bvid`, classify
        /// the primary URL and every backup, probe the selection candidates, then
        /// cheaply status-probe the curated mirror list so dead hosts show up.
        ///
        /// Everything is written through `crate::debug_log` (`BILI_DEBUG_LOG`) and
        /// also returned as one text block. Intended to be called once from startup
        /// when [`cdn_qa_bvid`] is `Some`.
        pub async fn cdn_selftest(&self, bvid: &str) -> Result<String> {
            let detail = self.video_view(bvid).await?;
            let cid = if detail.cid > 0 {
                detail.cid as u64
            } else {
                detail.pages.first().map(|page| page.cid as u64).unwrap_or(0)
            };
            if cid == 0 {
                bail!("CDN QA: {bvid} 没有可用 cid");
            }
            let info = self.playurl(bvid, cid).await?;
            // Probe the richest stream: a 480P segment can end before the window,
            // which would report a burst rate instead of sustained throughput.
            let track = info
                .video_tracks
                .iter()
                .max_by_key(|track| track.bandwidth)
                .ok_or_else(|| anyhow!("CDN QA: 没有视频轨道"))?;

            let mut report = Vec::new();
            report.push(format!(
                "cdn qa bvid={bvid} cid={cid} qn={} track_urls={}",
                track.id,
                track.urls.len()
            ));
            for (index, url) in track.urls.iter().enumerate() {
                let kind = classify_cdn(url);
                let host = url_authority(url)
                    .map(authority_host)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                report.push(format!(
                    "cdn qa cand #{index} host={host} kind={} rank={} rewritable={}",
                    kind.label(),
                    cdn_rank(kind),
                    is_rewritable(url)
                ));
            }

            let primary = track.urls.first().cloned().unwrap_or_default();
            let backups: Vec<String> = track.urls.iter().skip(1).cloned().collect();
            let choice = self.pick_fastest(&primary, &backups).await;
            for item in &choice.measured {
                let rate = match item.bps {
                    Some(bps) => format!("{:.2} mbps", bps as f64 / 1e6),
                    None => item
                        .error
                        .clone()
                        .unwrap_or_else(|| "no measurement".to_string()),
                };
                report.push(format!(
                    "cdn qa probe host={} kind={} result={rate}",
                    item.host,
                    item.kind.label()
                ));
            }
            let from_source = track.urls.iter().any(|url| url == &choice.url);
            report.push(format!(
                "cdn qa pick host={} kind={} from_cache={} url_from_sources={from_source}",
                choice.host,
                choice.kind.label(),
                choice.from_cache,
            ));

            // Second call in the same process must reuse the host cache, which
            // is what makes the *next* video start with the winner instead of
            // paying for the probe again.
            let started = Instant::now();
            let recap = self.pick_fastest(&primary, &backups).await;
            report.push(format!(
                "cdn qa recap host={} cached={} same={} ms={}",
                recap.host,
                recap.from_cache,
                recap.host == choice.host,
                started.elapsed().as_millis()
            ));

            // Cheap curated sweep: `bytes=0-0` only proves 200 vs 403/dead.
            let only_hosts: std::collections::HashSet<&str> = choice
                .measured
                .iter()
                .map(|item| item.host.as_str())
                .collect();
            for host in curated_mirror_hosts() {
                if only_hosts.contains(host.as_str()) {
                    continue;
                }
                let Some(url) = rewrite_host(&primary, &host) else {
                    report.push(format!("cdn qa curated host={host} skipped=not-rewritable"));
                    continue;
                };
                let result = self
                    .client
                    .get(&url)
                    .header(RANGE, "bytes=0-0")
                    .timeout(Duration::from_secs(4))
                    .send()
                    .await;
                match result {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        if response.status().is_success() {
                            report.push(format!("cdn qa curated host={host} status={status} alive"));
                        } else {
                            report.push(format!("cdn qa curated host={host} status={status} dead"));
                        }
                    }
                    Err(err) => report.push(format!(
                        "cdn qa curated host={host} error={}",
                        describe_req_error(&err)
                    )),
                }
            }

            for line in &report {
                crate::debug_log(line);
            }
            Ok(report.join("\n"))
        }
    }

    /// `BILI_CDN_TEST=<bvid>` for the QA hook.
    pub fn cdn_qa_bvid() -> Option<String> {
        std::env::var("BILI_CDN_TEST")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Rewritable sources, best kind first (stable within one kind).
    fn ranked_sources(primary: &str, backups: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for url in std::iter::once(primary).chain(backups.iter().map(String::as_str)) {
            if url.is_empty() || !is_rewritable(url) {
                continue;
            }
            push_unique(&mut out, url);
        }
        out.sort_by_key(|url| cdn_rank(classify_cdn(url)));
        out
    }

    /// Up to [`PROBE_MAX_HOSTS`] `(host, url)` pairs: the video's own sources first,
    /// then curated mirrors rewritten from the best source.
    fn probe_plan(sources: &[String], base: &str) -> Vec<(String, String)> {
        let mut plan: Vec<(String, String)> = Vec::with_capacity(PROBE_MAX_HOSTS);
        let mut seen: Vec<String> = Vec::new();
        for url in sources {
            if plan.len() >= PROBE_MAX_HOSTS {
                break;
            }
            let Some(host) = url_authority(url).map(authority_host) else {
                continue;
            };
            let host = host.to_ascii_lowercase();
            if seen.contains(&host) {
                continue;
            }
            seen.push(host.clone());
            plan.push((host, url.clone()));
        }
        if plan.len() < PROBE_MAX_HOSTS {
            for host in curated_mirror_hosts() {
                if plan.len() >= PROBE_MAX_HOSTS {
                    break;
                }
                if seen.contains(&host) {
                    continue;
                }
                let Some(url) = rewrite_host(base, &host) else {
                    continue;
                };
                seen.push(host.clone());
                plan.push((host, url));
            }
        }
        plan
    }
}

pub use self::cdn_select::*;

// ---------------------------------------------------------------------------
// Quality QA hook
// ---------------------------------------------------------------------------

/// `BILI_QUALITY_TEST=<bvid>` for the quality QA hook. `BILI_QUALITY_FNVAL`
/// (see [`quality_fnval`]) optionally overrides the requested `fnval` so two
/// runs can be diffed.
///
/// `BILI_TIERS_TEST=<bvid>` is the narrower hook of the two and names a bvid on
/// its own (see [`tiers_qa_bvid`]): the same selftest runs, and `playurl` logs
/// the resolved tier table.
pub fn quality_qa_bvid() -> Option<String> {
    let from_env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    from_env("BILI_QUALITY_TEST").or_else(|| from_env("BILI_TIERS_TEST"))
}

/// `BILI_TIERS_TEST=<bvid>`: log the resolved tier table of that video, one row
/// per line, through [`crate::debug_log`].
///
/// Read on the normal `playurl` path, so it needs no hook of its own: the video
/// is fetched by [`Session::quality_selftest`] (which
/// [`quality_qa_bvid`] also drives) or by opening it in the player.
pub fn tiers_qa_bvid() -> Option<String> {
    std::env::var("BILI_TIERS_TEST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// One `tiers …` line per tier plus a `tiers summary …` line, for
/// `BILI_TIERS_TEST`. The table is exactly the one the 画质 menu reads, so a
/// locked 大会员 row can be told from a tier the video does not have.
fn log_tier_table(bvid: &str, cid: u64, tiers: &[QualityTier]) {
    for tier in tiers {
        crate::debug_log(&format!(
            "tiers bvid={bvid} cid={cid} qn={} label={} locked={} note={} detail={}",
            tier.qn,
            tier.label,
            tier.locked,
            tier.note(),
            tier.detail
        ));
    }
    let locked = tiers.iter().filter(|tier| tier.locked).count();
    crate::debug_log(&format!(
        "tiers summary bvid={bvid} cid={cid} total={} locked={locked}",
        tiers.len()
    ));
}

impl Session {
    /// QA hook for `BILI_QUALITY_TEST=<bvid>`: fetch `playurl` with the current
    /// `fnval` and report every raw DASH stream plus the audio matrix — one
    /// `quality …` line per stream, then a summary line with the distinct `qn`s.
    ///
    /// Run it twice (`BILI_QUALITY_FNVAL=16` vs. the default `4048`) to prove
    /// what the account can actually get. Everything goes through
    /// [`crate::debug_log`] (`BILI_DEBUG_LOG`) and is also returned as one block,
    /// like [`Session::cdn_selftest`].
    pub async fn quality_selftest(&self, bvid: &str) -> Result<String> {
        let detail = self.video_view(bvid).await?;
        let cid = if detail.cid > 0 {
            detail.cid as u64
        } else {
            detail
                .pages
                .first()
                .map(|page| page.cid as u64)
                .unwrap_or(0)
        };
        if cid == 0 {
            bail!("画质 QA: {bvid} 没有可用 cid");
        }
        let info = self.playurl(bvid, cid).await?;
        let mut report = vec![
            format!(
                "quality bvid={bvid} cid={cid} fnval={} streams={} audio={}",
                quality_fnval(),
                info.qualities.len(),
                info.audio_tracks.len()
            ),
            format!(
                "quality view bvid={bvid} width={} height={} pubdate={} cid={}",
                detail.width, detail.height, detail.pubdate, detail.cid
            ),
        ];
        for quality in &info.qualities {
            report.push(format!(
                "quality qn={} label={} {}x{} {:.2}fps codec={}({}) {:.2}Mbps hdr={} dolby={} 8k={}",
                quality.qn,
                quality.label,
                quality.width,
                quality.height,
                quality.fps,
                quality.codec,
                quality.codecid,
                quality.bandwidth_bps as f64 / 1e6,
                quality.hdr,
                quality.dolby_vision,
                quality.eight_k
            ));
        }
        let mut qns: Vec<i64> = Vec::new();
        for quality in &info.qualities {
            if !qns.contains(&quality.qn) {
                qns.push(quality.qn);
            }
        }
        report.push(format!(
            "quality total={} qns={qns:?}",
            info.qualities.len()
        ));

        for audio in &info.audio_tracks {
            report.push(format!(
                "quality audio id={} label={} {:.2}Mbps vip={} dolby={} hi_res={}",
                audio.id,
                audio_label(audio.id, audio.bandwidth),
                audio.bandwidth as f64 / 1e6,
                audio.vip,
                audio.dolby,
                audio.hi_res
            ));
        }
        let mut audio_ids: Vec<i64> = Vec::new();
        for audio in &info.audio_tracks {
            if !audio_ids.contains(&audio.id) {
                audio_ids.push(audio.id);
            }
        }
        report.push(format!(
            "quality audio total={} ids={audio_ids:?}",
            info.audio_tracks.len()
        ));

        // Listing a stream is not the same as being allowed to play it, so probe
        // one byte of the richest stream: a 403 here means the tier is visible
        // but locked for this account.
        if let Some(richest) = info
            .qualities
            .iter()
            .max_by_key(|quality| quality.bandwidth_bps)
        {
            let probe = self
                .client
                .get(&richest.url)
                .header(RANGE, "bytes=0-0")
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            report.push(match probe {
                Ok(response) => format!(
                    "quality probe qn={} codec={} status={} playable={}",
                    richest.qn,
                    richest.codec,
                    response.status().as_u16(),
                    response.status().is_success()
                ),
                Err(err) => format!(
                    "quality probe qn={} codec={} err={err}",
                    richest.qn, richest.codec
                ),
            });
        }

        for line in &report {
            crate::debug_log(line);
        }
        Ok(report.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// 收藏推荐 QA hook
// ---------------------------------------------------------------------------

/// `BILI_FAV_TEST=1` enables the 收藏推荐 hook
/// ([`Session::fav_selftest`]). Values other than `1`/`true` leave it off.
pub fn fav_qa_enabled() -> bool {
    std::env::var("BILI_FAV_TEST")
        .map(|value| {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Up to two distinct pseudo-random picks from `items`, clock-seeded so repeated
/// runs sample different videos. Never indexes out of bounds.
fn fav_qa_picks(items: &[FeedVideo]) -> Vec<&FeedVideo> {
    let mut picks = Vec::with_capacity(2);
    if items.is_empty() {
        return picks;
    }
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    let len = items.len();
    let first = (splitmix64(&mut state) % len as u64) as usize;
    picks.push(&items[first]);
    if len > 1 {
        let second = (splitmix64(&mut state) % len as u64) as usize;
        let second = if second == first {
            (second + 1) % len
        } else {
            second
        };
        picks.push(&items[second]);
    }
    picks
}

impl Session {
    /// QA hook for `BILI_FAV_TEST=1`: report the 收藏推荐 data layer as three
    /// kinds of line —
    ///
    /// * `fav folders n=<n> [id:name:count, …]` (default + empty folders excluded),
    /// * `fav items media=<id> page=1 n=<n> has_more=<bool> first=<bvid> "<title>"`,
    /// * `fav related bvid=<bvid> n=<n> ok=<n>` (`n` kept after filtering, `ok`
    ///   with a usable `cid`) for two random items of that page.
    ///
    /// A leading `fav session isLogin=…` line states whether the session is
    /// logged in, so an anonymous run is visible instead of silently empty.
    /// Everything goes through [`crate::debug_log`] and is returned as one block,
    /// like [`Session::quality_selftest`].
    pub async fn fav_selftest(&self) -> Result<String> {
        let mut report = Vec::new();
        match self.nav().await {
            Ok(NavOutcome::LoggedIn(user)) => report.push(format!(
                "fav session isLogin=true mid={} uname={}",
                user.mid, user.uname
            )),
            Ok(NavOutcome::NotLoggedIn { code, message }) => report.push(format!(
                "fav session isLogin=false code={code} msg={message}"
            )),
            Err(err) => report.push(format!("fav session err={err:#}")),
        }

        let folders = match self.fav_folders().await {
            Ok(folders) => folders,
            Err(err) => {
                report.push(format!("fav folders err={err:#}"));
                Vec::new()
            }
        };
        let listing = folders
            .iter()
            .map(|folder| format!("{}:{}:{}", folder.id, folder.name, folder.count))
            .collect::<Vec<String>>()
            .join(", ");
        report.push(format!("fav folders n={} [{listing}]", folders.len()));

        // A folder with at least two page-one items gives both related samples.
        let folder = folders
            .iter()
            .find(|folder| folder.count >= 2)
            .or_else(|| folders.first());
        let Some(folder) = folder else {
            report.push("fav items skipped reason=no-folders".to_string());
            for line in &report {
                crate::debug_log(line);
            }
            return Ok(report.join("\n"));
        };

        let items = match self.fav_items(folder.id, 1).await {
            Ok((items, has_more)) => {
                let first = items.first();
                report.push(format!(
                    "fav items media={} page=1 n={} has_more={} first={} \"{}\"",
                    folder.id,
                    items.len(),
                    has_more,
                    first.map(|item| item.bvid.as_str()).unwrap_or("-"),
                    first.map(|item| item.title.as_str()).unwrap_or("")
                ));
                items
            }
            Err(err) => {
                report.push(format!("fav items media={} page=1 err={err:#}", folder.id));
                Vec::new()
            }
        };

        for item in fav_qa_picks(&items) {
            match self.related(&item.bvid).await {
                Ok(list) => {
                    let ok = list.iter().filter(|video| video.cid > 0).count();
                    report.push(format!(
                        "fav related bvid={} n={} ok={ok}",
                        item.bvid,
                        list.len()
                    ));
                }
                Err(err) => report.push(format!(
                    "fav related bvid={} n=0 ok=0 err={err:#}",
                    item.bvid
                )),
            }
        }

        for line in &report {
            crate::debug_log(line);
        }
        Ok(report.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct QrGen {
    pub url: String,
    pub key: String,
}

pub struct PollResult {
    /// 0 = success, 86038 = expired, 86090 = scanned (unconfirmed), 86101 = not scanned.
    pub state_code: i64,
    pub cookies: Vec<StoredCookie>,
    pub refresh_token: String,
    /// Cross-domain SSO redirect URL from the successful poll (`data.url`).
    pub cross_url: String,
}

pub struct NavUser {
    pub uname: String,
    pub face: String,
    pub mid: i64,
    pub money: f64,
    pub moral: f64,
    pub level: i64,
    pub vip_text: String,
    pub vip_status: bool,
}

/// Outcome of a `nav` query: either a logged-in user or the API's own
/// rejection (so the failure stays diagnosable).
pub enum NavOutcome {
    LoggedIn(NavUser),
    NotLoggedIn { code: i64, message: String },
}

/// Verbatim outcome of one 大会员 endpoint call. A non-zero `code` (e.g.
/// "not a member" / "already claimed") is an ordinary answer, so it is
/// returned rather than raised; `summary` carries an optional short text for
/// `privilege/my`.
#[derive(Clone, Debug)]
pub struct VipCode {
    pub code: i64,
    pub message: String,
    pub summary: String,
}

impl VipCode {
    fn from_code(code: i64, message: String) -> Self {
        Self {
            code,
            message,
            summary: String::new(),
        }
    }
}

pub struct Relation {
    pub following: i64,
    pub follower: i64,
}

#[derive(Clone, Debug, Default)]
pub struct FeedVideo {
    pub bvid: String,
    pub cid: u64,
    pub aid: u64,
    pub title: String,
    pub pic: String,
    pub owner: String,
    pub owner_mid: i64,
    /// Exact play count (`stat.view` / arc `play`), unformatted.
    pub view: u64,
    pub duration: u64,
    /// Publish time as a Unix timestamp in seconds; `0` when the payload has
    /// none. The feed, favourites, related and UP主 arc paths all fill it.
    pub pub_ts: i64,
}

/// One discovery card (分区 / 排行 item).
#[derive(Clone, Debug, Default)]
pub struct VideoCard {
    pub bvid: String,
    pub title: String,
    pub pic: String,
    pub owner: String,
    pub view: u64,
    pub duration: u64,
}

/// One paged card list plus its pagination state.
pub struct CardPage {
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<VideoCard>,
}

/// One trending-search entry.
#[derive(Clone, Debug, Default)]
pub struct HotSearch {
    pub keyword: String,
    pub show_name: String,
}

// ---- 番剧 / PGC ------------------------------------------------------------
//
// Everything here is already normalised: URLs are absolute, the 追番/评分
// strings are the site's own text, and `duration_ms` is milliseconds exactly as
// the API reports it. The UI never parses an API payload.

/// One row of 每日放送.
#[derive(Clone, Debug, Default)]
pub struct PgcTimelineEpisode {
    pub season_id: i64,
    pub title: String,
    pub pub_index: String,
    pub pub_time: String,
    /// 9:00-style air time, already formatted.
    pub cover: String,
    /// "12.3万追番" when the API names a number; empty when it answers "-".
    pub follows: String,
}

/// One day of the 7-day timeline.
#[derive(Clone, Debug, Default)]
pub struct PgcTimelineDay {
    /// "10-27".
    pub date: String,
    /// 1 = 周一 … 7 = 周日 (0 is mapped to 7).
    pub day_of_week: i64,
    pub is_today: bool,
    pub episodes: Vec<PgcTimelineEpisode>,
}

/// One card of the 分区索引 list.
#[derive(Clone, Debug, Default)]
pub struct PgcIndexItem {
    pub season_id: i64,
    pub title: String,
    pub cover: String,
    /// "全11话" / "更新至第197话".
    pub index_show: String,
    /// The site's own "553.2万追番".
    pub order: String,
    /// "大会员" / "独播" / empty.
    pub badge: String,
    /// "9.7" or empty.
    pub score: String,
}

/// One page of the 分区索引 list.
#[derive(Clone, Debug, Default)]
pub struct PgcIndexPage {
    pub page: i64,
    pub total: i64,
    pub has_next: bool,
    pub items: Vec<PgcIndexItem>,
}

/// One row of the 排行榜 (99 rows, no paging).
#[derive(Clone, Debug, Default)]
pub struct PgcRankItem {
    pub rank: i64,
    pub season_id: i64,
    pub title: String,
    pub cover: String,
    /// "8.8分" or empty.
    pub rating: String,
    pub badge: String,
    pub index_show: String,
    /// Follow count as the site reports it (raw number, 0 = unknown).
    pub follow: u64,
}

/// One episode of a season. `id` in the API is the ep_id.
#[derive(Clone, Debug, Default)]
pub struct PgcEpisode {
    pub ep_id: i64,
    pub aid: i64,
    pub bvid: String,
    pub cid: u64,
    pub title: String,
    pub long_title: String,
    /// Milliseconds, as the API returns it.
    pub duration_ms: u64,
    pub pub_time: i64,
}

impl PgcEpisode {
    /// The line the player's 选集 shows: `long_title` wins, then `title`,
    /// then a stable "第 N 话" fallback.
    pub fn display_title(&self) -> String {
        if !self.long_title.trim().is_empty() {
            self.long_title.trim().to_string()
        } else if !self.title.trim().is_empty() {
            self.title.trim().to_string()
        } else {
            "正片".to_string()
        }
    }

    pub fn duration_secs(&self) -> u64 {
        self.duration_ms / 1000
    }

    /// Whether the episode is already out at `now` (unix seconds). An unknown
    /// `pub_time` (0) counts as released: the API omits it on some old seasons.
    pub fn released_at(&self, now: i64) -> bool {
        self.pub_time <= 0 || self.pub_time <= now
    }
}

/// One 花絮 / PV group of a season (the API's `section`).
#[derive(Clone, Debug, Default)]
pub struct PgcSection {
    pub title: String,
    pub episodes: Vec<PgcEpisode>,
}

/// One season's detail, as the 番剧 page and the player's 选集 need it.
#[derive(Clone, Debug, Default)]
pub struct PgcSeason {
    pub season_id: i64,
    pub title: String,
    pub cover: String,
    pub evaluate: String,
    /// 0 when the season has no score (the API sends `rating: null`).
    pub score: f64,
    pub score_count: u64,
    pub areas: Vec<String>,
    pub styles: Vec<String>,
    /// "2022年10月31日18:00" (already human readable).
    pub pub_time: String,
    pub is_finish: bool,
    /// "全11话" / "更新至第197话".
    pub index_show: String,
    pub episodes: Vec<PgcEpisode>,
    pub sections: Vec<PgcSection>,
}

impl PgcSeason {
    /// First playable episode at `now`: the first aired one, else the first
    /// listed (so a fully unreleased season still opens on episode 1).
    pub fn first_episode(&self, now: i64) -> Option<&PgcEpisode> {
        self.episodes
            .iter()
            .find(|episode| episode.released_at(now))
            .or_else(|| self.episodes.first())
    }

    /// "番剧 · 日本 · 奇幻" style classification line.
    pub fn classify(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for area in &self.areas {
            if !area.trim().is_empty() {
                parts.push(area.trim());
                break;
            }
        }
        for style in self.styles.iter().take(2) {
            if !style.trim().is_empty() {
                parts.push(style.trim());
            }
        }
        parts.join(" · ")
    }

    pub fn all_episodes(&self) -> impl Iterator<Item = &PgcEpisode> {
        self.episodes
            .iter()
            .chain(self.sections.iter().flat_map(|section| section.episodes.iter()))
    }
}

/// The gate answer for one episode. `code == 0` means the region check passed;
/// anything else is the site's own refusal, mapped by [`pgc_reason`].
#[derive(Clone, Debug, Default)]
pub struct PgcGate {
    pub code: i64,
    pub message: String,
}

/// A short Chinese reason for a PGC refusal code. `None` = the code carries no
/// specific meaning we can honestly display.
pub fn pgc_reason(code: i64) -> Option<&'static str> {
    match code {
        -10403 => Some("本地区不可观看"),
        -10402 | -404 => Some("内容不存在或已下架"),
        87007 | 87008 => Some("该内容需要大会员"),
        _ => None,
    }
}

#[derive(Deserialize)]
struct QrGenResp {
    code: i64,
    data: Option<QrGenData>,
}

#[derive(Deserialize)]
struct QrGenData {
    url: String,
    qrcode_key: String,
}

#[derive(Deserialize)]
struct PollResp {
    code: i64,
    data: Option<PollData>,
}

#[derive(Deserialize)]
struct PollData {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
struct NavResp {
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<NavData>,
}

#[derive(Deserialize)]
struct NavData {
    #[serde(rename = "isLogin", default)]
    is_login: bool,
    #[serde(default)]
    uname: String,
    #[serde(default)]
    face: String,
    #[serde(default)]
    mid: i64,
    #[serde(default)]
    money: f64,
    #[serde(default)]
    moral: f64,
    #[serde(default)]
    level_info: Option<LevelInfo>,
    #[serde(default)]
    vip: Option<VipInfo>,
    #[serde(default)]
    wbi_img: Option<WbiImg>,
}

#[derive(Deserialize)]
struct WbiImg {
    #[serde(default)]
    img_url: String,
    #[serde(default)]
    sub_url: String,
}

#[derive(Deserialize)]
struct LevelInfo {
    #[serde(default)]
    current_level: i64,
}

#[derive(Deserialize)]
struct VipInfo {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    label: Option<VipLabel>,
}

#[derive(Deserialize)]
struct VipLabel {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RelationResp {
    code: i64,
    data: Option<RelationData>,
}

#[derive(Deserialize)]
struct RelationData {
    #[serde(default)]
    following: i64,
    #[serde(default)]
    follower: i64,
}

#[derive(Deserialize)]
struct LikeStateResp {
    #[serde(default)]
    code: i64,
    data: Option<i64>,
}

#[derive(Deserialize)]
struct LikeResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct CodeResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct CoinStateResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<CoinStateData>,
}

#[derive(Deserialize, Default)]
struct CoinStateData {
    #[serde(default)]
    multiply: i64,
}

#[derive(Deserialize)]
struct FavFoldersResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<FavFoldersData>,
}

#[derive(Deserialize)]
struct FavFoldersData {
    #[serde(default)]
    list: Option<Vec<FavFolderItem>>,
}

/// One entry of `folder/created/list-all`. The live endpoint names the field
/// `title`; `name` is accepted as well because older payloads used it.
#[derive(Deserialize)]
struct FavFolderItem {
    #[serde(default)]
    id: i64,
    #[serde(default, alias = "name")]
    title: String,
    #[serde(default)]
    media_count: u32,
}

#[derive(Deserialize)]
struct FavFavouredResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<FavFavouredData>,
}

#[derive(Deserialize, Default)]
struct FavFavouredData {
    #[serde(default)]
    favoured: bool,
    #[serde(default)]
    count: i64,
}

#[derive(Deserialize)]
struct FavListResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<FavListData>,
}

#[derive(Deserialize)]
struct FavListData {
    #[serde(default)]
    info: Option<FavListInfo>,
    /// Deleted or otherwise broken entries come back as `null`.
    #[serde(default)]
    medias: Vec<Option<FavMedia>>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct FavListInfo {
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct FavMedia {
    /// The `aid`.
    #[serde(default)]
    id: i64,
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    upper: Option<FavUpper>,
    #[serde(default)]
    cnt_info: Option<FavCntInfo>,
    /// Publish stamp (Unix seconds) of the video, not of the favourite.
    #[serde(default)]
    pubtime: i64,
    /// Carries the first page's `cid` on folders the web client fills in.
    #[serde(default)]
    ugc: Option<FavUgc>,
}

#[derive(Deserialize)]
struct FavUpper {
    #[serde(default)]
    name: String,
    #[serde(default)]
    mid: i64,
}

#[derive(Deserialize)]
struct FavUgc {
    #[serde(default)]
    first_cid: u64,
}

#[derive(Deserialize)]
struct FavCntInfo {
    #[serde(default)]
    play: u64,
}

#[derive(Deserialize)]
struct RelatedResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    /// `null` when the endpoint answers without a list.
    #[serde(default)]
    data: Option<Vec<Option<RelatedItem>>>,
}

/// One `archive/related` entry. Ads are entries without a `bvid` and/or with
/// `goto != "av"`; the live payload omits `goto` for normal videos.
#[derive(Deserialize)]
struct RelatedItem {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    cid: u64,
    #[serde(default)]
    aid: i64,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pic: String,
    #[serde(default)]
    duration: u64,
    /// Publish stamp (Unix seconds); related entries carry it explicitly.
    #[serde(default)]
    pubdate: i64,
    #[serde(default)]
    goto: Option<String>,
    #[serde(default)]
    owner: Option<FeedOwner>,
    #[serde(default)]
    stat: Option<FeedStat>,
}

#[derive(Deserialize)]
struct HistoryResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<HistoryData>,
}

#[derive(Deserialize)]
struct HistoryIndexResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct HistoryData {
    #[serde(default)]
    cursor: Option<HistoryCursorData>,
    #[serde(default)]
    list: Vec<HistoryEntry>,
}

#[derive(Deserialize)]
struct HistoryCursorData {
    #[serde(default)]
    max: i64,
    #[serde(default)]
    view_at: i64,
    #[serde(default)]
    business: String,
}

#[derive(Deserialize)]
struct HistoryEntry {
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    kid: String,
    #[serde(default)]
    history: Option<HistoryMeta>,
    #[serde(default)]
    author_name: String,
    /// The uploader's mid, so a history row's UP name can open the UP page.
    #[serde(default)]
    author_mid: i64,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    progress: i64,
}

#[derive(Deserialize, Default)]
struct HistoryMeta {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    oid: i64,
    #[serde(default)]
    business: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    kid: String,
}

#[derive(Deserialize)]
struct ToViewResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<ToViewData>,
}

#[derive(Deserialize, Default)]
struct ToViewData {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    list: Vec<ToViewEntry>,
}

#[derive(Deserialize)]
struct ToViewEntry {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    aid: i64,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    owner: Option<ToViewOwner>,
    #[serde(default)]
    stat: Option<ToViewStat>,
}

#[derive(Deserialize)]
struct ToViewOwner {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct ToViewStat {
    #[serde(default)]
    view: u64,
}

#[derive(Deserialize)]
struct SpiResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<SpiData>,
}

#[derive(Deserialize)]
struct SpiData {
    #[serde(default)]
    b_3: String,
    #[serde(default)]
    b_4: String,
}

#[derive(Deserialize)]
struct FeedResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<FeedData>,
}

#[derive(Deserialize)]
struct FeedData {
    #[serde(default)]
    item: Vec<FeedItem>,
}

#[derive(Deserialize)]
struct FeedItem {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    cid: u64,
    #[serde(default)]
    goto: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pic: String,
    #[serde(default)]
    duration: u64,
    /// Publish stamp (Unix seconds) the feed item carries; `0` when absent.
    #[serde(default)]
    pubdate: i64,
    #[serde(default)]
    owner: Option<FeedOwner>,
    #[serde(default)]
    stat: Option<FeedStat>,
}

#[derive(Deserialize)]
struct FeedOwner {
    #[serde(default)]
    mid: i64,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct FeedStat {
    #[serde(default)]
    view: u64,
}

/// Resolved media streams for one video.
pub struct PlayInfo {
    /// One entry per `(qn, codecid)` pair so 编码偏好 can pick between codecs.
    /// This is the compact list the player's quality menu is built from.
    pub video_tracks: Vec<VideoTrack>,
    /// Every `dash.video[]` representation exactly as returned, one entry per
    /// stream and never collapsed by `qn` (see [`QualityOption`]).
    pub qualities: Vec<QualityOption>,
    /// Quality selected when playback starts.
    pub default_qn: i64,
    /// Every DASH audio track present, sorted high→low by bitrate. Includes the
    /// 大会员 streams when the account may play them (30250 杜比全景声, 30251 无损).
    pub audio_tracks: Vec<AudioTrack>,
    pub duration: u64,
}

/// One raw DASH video representation: a single `dash.video[]` element.
///
/// A `qn` is not unique — 1080P may arrive as AVC, HEVC and AV1, and a codec
/// may arrive at several bitrates — so this list keeps one entry per stream and
/// lets the caller see the whole matrix. [`PlayInfo::video_tracks`] is the
/// collapsed view used for playback selection.
#[derive(Clone, Debug)]
pub struct QualityOption {
    /// Quality id, identical to the `qn` request tier (80 = 1080P, 120 = 4K, …).
    pub qn: i64,
    /// Human label for `qn`, e.g. `1080P60`, `4K 超清`, `HDR 真彩`.
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    /// 7 = AVC/H.264, 12 = HEVC/H.265, 13 = AV1 (`0` = unknown).
    pub codecid: i64,
    /// [`codec_label`] for `codecid`.
    pub codec: &'static str,
    /// Declared bitrate in bit/s (`bandwidth` from the manifest).
    pub bandwidth_bps: u64,
    /// `qn` 125 / 129: HDR (真彩 / Vivid), HEVC only.
    pub hdr: bool,
    /// `qn` 126: 杜比视界, HEVC only.
    pub dolby_vision: bool,
    /// `qn` 127: 8K, no AVC variant exists.
    pub eight_k: bool,
    /// Primary media URL.
    pub url: String,
    /// Backup mirrors, in manifest order.
    pub backup_urls: Vec<String>,
}

/// One selectable DASH video representation: `qn`, codec, resolution and URL.
#[derive(Clone, Debug)]
pub struct VideoTrack {
    pub id: i64,
    /// 7 = AVC/H.264, 12 = HEVC/H.265, 13 = AV1 (`0` = unknown).
    pub codecid: i64,
    pub codecs: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    /// Native frame rate from the DASH manifest (`30.0` when omitted).
    pub frame_rate: f64,
    /// Primary media URL (the first entry of [`VideoTrack::urls`]).
    pub url: String,
    /// Every fetch candidate for this representation: the primary base URL
    /// first, then each backup mirror. Used by the offline downloader to fall
    /// back to another CDN edge.
    pub urls: Vec<String>,
}

/// One selectable DASH audio track: its `id`, bitrate and resolved URL.
#[derive(Clone, Debug)]
pub struct AudioTrack {
    pub id: i64,
    pub bandwidth: u64,
    /// Primary media URL (the first entry of [`AudioTrack::urls`]).
    pub url: String,
    /// Every fetch candidate: the primary base URL first, then each backup.
    pub urls: Vec<String>,
    /// 大会员-only stream (Hi-Res 无损 / 杜比全景声).
    pub vip: bool,
    /// `30250`: 杜比全景声 (`dash.dolby.audio[]`).
    pub dolby: bool,
    /// `30251`: Hi-Res 无损 (`dash.flac.audio`).
    pub hi_res: bool,
}

/// Human label for an audio track, e.g. `192K (30280)`.
pub fn audio_label(id: i64, bandwidth: u64) -> String {
    let base = match id {
        30216 => "64K".to_string(),
        30232 => "132K".to_string(),
        30280 => "192K".to_string(),
        30250 => "杜比全景声".to_string(),
        30251 => "无损".to_string(),
        _ => format!("{}K", bandwidth / 1000),
    };
    format!("{base} ({id})")
}

/// Whether a `qn` is a 大会员-only quality tier.
pub fn is_vip_quality(qn: i64) -> bool {
    matches!(qn, 112 | 116 | 120 | 125 | 126 | 127 | 129)
}

/// Human label for a `qn` quality id, covering every tier the API documents:
/// 6 240P, 16 360P, 32 480P, 64 720P, 74 720P60, 80 1080P, 100 智能修复,
/// 112 1080P+ 高码率, 116 1080P60, 120 4K, 125 HDR, 126 杜比视界, 127 8K,
/// 129 HDR Vivid.
///
/// Pure and allocation-free, so the QA hook and the unit tests can call it for
/// every tier; an unknown id falls back to `默认`, the API's "let the server
/// pick" tier.
pub fn qn_label(qn: i64) -> &'static str {
    match qn {
        6 => "240P",
        16 => "360P",
        32 => "480P",
        64 => "720P",
        74 => "720P60",
        80 => "1080P",
        100 => "智能修复",
        112 => "1080P+",
        116 => "1080P60",
        120 => "4K 超清",
        125 => "HDR 真彩",
        126 => "杜比视界",
        127 => "8K 超清",
        129 => "HDR Vivid",
        _ => "默认",
    }
}

/// Owned label for a `qn`, kept for the existing call sites.
pub fn quality_label(qn: i64) -> String {
    qn_label(qn).to_string()
}

/// One quality tier the video advertises in `accept_quality` /
/// `support_formats`, whether or not the signed-in account may play it.
///
/// For a non-VIP account the lists still name every tier the upload has (4K,
/// 1080P60, HDR, 8K …); `dash.video[]` is the strict subset the account may
/// actually play. `locked` is that difference: no stream came back at this
/// `qn`, so the row is a 大会员 upsell, not a playable stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualityTier {
    /// Tier id (`116` 1080P60, `120` 4K, `125` HDR, `127` 8K, …).
    pub qn: i64,
    /// The app-wide label for the tier (`1080P60`, `4K 超清`, …).
    pub label: String,
    /// Expected picture, formatted for the menu: `3840×2160 · 30fps`.
    pub detail: String,
    /// No `dash.video[]` stream was returned at this `qn` for this account.
    pub locked: bool,
}

impl QualityTier {
    /// The menu copy a locked row carries: `需要大会员` for a VIP tier this
    /// account may not play, empty for a playable row and for a locked tier
    /// that is not a 大会员 one.
    ///
    /// Derived from `qn`/`locked` instead of stored, so the two can never
    /// disagree.
    pub fn note(&self) -> &'static str {
        if self.locked && is_vip_quality(self.qn) {
            "需要大会员"
        } else {
            ""
        }
    }
}

/// The upload's native picture height (`dimension.height` of
/// `x/web-interface/view`) for the last few videos, keyed by `bvid`.
///
/// `playurl` reports what this account may play, never the source resolution,
/// so the tier fallback ([`possible_tiers`]) needs the value `video_view`
/// already fetched when the video was opened. Bounded and only a hint: a miss
/// (`None`) means "unknown", never "0 pixels".
static NATIVE_SIZE_CACHE: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

const NATIVE_SIZE_CACHE_MAX: usize = 64;

fn remember_native_height(bvid: &str, height: u32) {
    if bvid.is_empty() || height == 0 {
        return;
    }
    let Ok(mut cache) = NATIVE_SIZE_CACHE.lock() else {
        return;
    };
    cache.retain(|(cached_bvid, _)| cached_bvid != bvid);
    cache.insert(0, (bvid.to_string(), height));
    cache.truncate(NATIVE_SIZE_CACHE_MAX);
}

fn native_height_for(bvid: &str) -> Option<u32> {
    let cache = NATIVE_SIZE_CACHE.lock().ok()?;
    cache
        .iter()
        .find(|(cached_bvid, _)| cached_bvid == bvid)
        .map(|(_, height)| *height)
}

/// Last few `playurl` tier lists, keyed by the video they belong to.
///
/// The player's catalogue (`PlayInfo::qualities`) only ever holds playable
/// streams, and its structs are the player's contract, so the tier list a
/// non-VIP account cannot play is kept here from the same response that
/// produced the streams. Bounded and keyed by `(bvid, cid)` so several open
/// player windows never read another video's list.
static QUALITY_TIER_CACHE: Mutex<Vec<(String, u64, Vec<QualityTier>)>> = Mutex::new(Vec::new());

const QUALITY_TIER_CACHE_MAX: usize = 16;

fn remember_quality_tiers(bvid: &str, cid: u64, tiers: Vec<QualityTier>) {
    let Ok(mut cache) = QUALITY_TIER_CACHE.lock() else {
        return;
    };
    cache.retain(|(cached_bvid, cached_cid, _)| cached_bvid != bvid || *cached_cid != cid);
    cache.insert(0, (bvid.to_string(), cid, tiers));
    cache.truncate(QUALITY_TIER_CACHE_MAX);
}

/// The tier list `playurl` saw for `bvid`/`cid`, if this process fetched it.
/// `None` (never an invented list) when the video was not opened yet.
pub fn quality_tiers_for(bvid: &str, cid: u64) -> Option<Vec<QualityTier>> {
    let cache = QUALITY_TIER_CACHE.lock().ok()?;
    cache
        .iter()
        .find(|(cached_bvid, cached_cid, _)| cached_bvid == bvid && *cached_cid == cid)
        .map(|(_, _, tiers)| tiers.clone())
}

/// Human label for a DASH `codecid`.
pub fn codec_label(codecid: i64) -> &'static str {
    match codecid {
        7 => "AVC",
        12 => "HEVC",
        13 => "AV1",
        _ => "未知",
    }
}

impl PlayInfo {
    /// The video representation for `qn` honouring `codec_pref` (`0` = any).
    ///
    /// An explicit `codec_pref` picks the highest-bitrate variant of that codec
    /// when one exists. With `codec_pref == 0` (自动) the **lowest-bitrate**
    /// variant is chosen: at the same resolution a smaller stream is less likely
    /// to stall on a slow or bursty link. Falls back to the default quality then
    /// the first track.
    pub fn video_track_for(&self, qn: i64, codec_pref: i64) -> Option<&VideoTrack> {
        self.video_tracks_for_qn(qn, codec_pref)
            .or_else(|| self.video_tracks_for_qn(self.default_qn, codec_pref))
            .or_else(|| self.video_tracks.first())
    }

    fn video_tracks_for_qn(&self, qn: i64, codec_pref: i64) -> Option<&VideoTrack> {
        if codec_pref != 0 {
            if let Some(track) = self
                .video_tracks
                .iter()
                .filter(|track| track.id == qn && track.codecid == codec_pref)
                .max_by_key(|track| track.bandwidth)
            {
                return Some(track);
            }
        }
        // 自动: smallest stream at this quality (fewer stalls at the same
        // resolution). An explicit codec choice above always wins.
        self.video_tracks
            .iter()
            .filter(|track| track.id == qn)
            .min_by_key(|track| track.bandwidth)
    }

    /// URL for the audio track at `index`, falling back to the highest bitrate.
    pub fn audio_url_for(&self, index: usize) -> Option<String> {
        self.audio_tracks
            .get(index)
            .or_else(|| self.audio_tracks.first())
            .map(|track| track.url.clone())
    }

    /// Every fetch candidate (primary then backups) for the audio track at
    /// `index`, falling back to the highest bitrate. Empty when no audio.
    pub fn audio_urls_for(&self, index: usize) -> Vec<String> {
        self.audio_tracks
            .get(index)
            .or_else(|| self.audio_tracks.first())
            .map(|track| {
                if track.urls.is_empty() {
                    vec![track.url.clone()]
                } else {
                    track.urls.clone()
                }
            })
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct PlayUrlResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<PlayUrlData>,
}

#[derive(Deserialize)]
struct PlayUrlData {
    #[serde(default)]
    dash: Option<Dash>,
    #[serde(default)]
    durl: Vec<Durl>,
    #[serde(default)]
    timelength: u64,
    /// Every tier the video advertises (`120` 4K, `116` 1080P60, …), in the
    /// API's own order. The signed-in account may not be able to play all of
    /// them; `dash.video[]` is the authoritative list of what it may play.
    #[serde(default)]
    accept_quality: Vec<i64>,
    /// Labels parallel to `accept_quality` (`4K 超清`, `1080P 高码率`, …).
    #[serde(default)]
    accept_description: Vec<String>,
    /// The same tiers with their marketing copy and codec list.
    #[serde(default)]
    support_formats: Vec<SupportFormat>,
}

/// One `support_formats[]` entry: a tier the video offers, not necessarily one
/// this account may play.
#[derive(Clone, Deserialize)]
struct SupportFormat {
    #[serde(default)]
    quality: i64,
    #[serde(default)]
    new_description: String,
    #[serde(default)]
    display_desc: String,
    #[serde(default)]
    codecs: Vec<String>,
}

#[derive(Deserialize)]
struct Dash {
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    video: Vec<DashTrack>,
    #[serde(default)]
    audio: Vec<DashTrack>,
    #[serde(default)]
    flac: Option<FlacObject>,
    #[serde(default)]
    dolby: Option<DolbyObject>,
}

/// `dash.flac`: a single Hi-Res (无损) audio object, present only for accounts
/// that may play it.
#[derive(Deserialize)]
struct FlacObject {
    #[serde(default)]
    audio: Option<TrackList>,
}

/// `dash.dolby`: the Dolby Atmos audio object, present only for accounts that
/// may play it.
#[derive(Deserialize)]
struct DolbyObject {
    #[serde(default)]
    audio: Option<TrackList>,
}

/// Bilibili emits some nested track fields as either one object or an array
/// depending on the endpoint; accept both.
#[derive(Deserialize)]
#[serde(untagged)]
enum TrackList {
    One(DashTrack),
    Many(Vec<DashTrack>),
}

impl TrackList {
    fn first(&self) -> Option<&DashTrack> {
        match self {
            TrackList::One(track) => Some(track),
            TrackList::Many(tracks) => tracks.first(),
        }
    }
}

/// One DASH representation. Bilibili emits every media URL under both the
/// camelCase and snake_case key, so each casing is a separate field — a serde
/// `alias` would treat the two present keys as a duplicate.
#[derive(Deserialize)]
struct DashTrack {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    codecid: i64,
    #[serde(default)]
    codecs: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    bandwidth: u64,
    #[serde(default, rename = "frameRate")]
    frame_rate_camel: Option<NumOrStr>,
    #[serde(default, rename = "frame_rate")]
    frame_rate_snake: Option<NumOrStr>,
    #[serde(default, rename = "baseUrl")]
    base_url: String,
    #[serde(default, rename = "base_url")]
    base_url_snake: String,
    #[serde(default, rename = "backupUrl")]
    backup_url: Vec<String>,
    #[serde(default, rename = "backup_url")]
    backup_url_snake: Vec<String>,
}

/// Bilibili emits `frameRate` as either a JSON string (`"30.000"`) or a bare
/// number depending on the endpoint; accept both and expose a plain `f64`.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Number(f64),
    Text(String),
}

impl NumOrStr {
    fn as_f64(&self) -> f64 {
        match self {
            NumOrStr::Number(value) => *value,
            NumOrStr::Text(text) => text.trim().parse::<f64>().unwrap_or(0.0),
        }
    }
}

#[derive(Deserialize)]
struct Durl {
    #[serde(default)]
    url: String,
}

#[cfg(test)]
mod track_tests {
    use super::{PlayInfo, VideoTrack};

    fn variant(id: i64, codecid: i64, bandwidth: u64) -> VideoTrack {
        let url = format!("u{id}_{codecid}");
        VideoTrack {
            id,
            codecid,
            codecs: String::new(),
            width: 0,
            height: 0,
            bandwidth,
            frame_rate: 30.0,
            url: url.clone(),
            urls: vec![url],
        }
    }

    fn sample() -> PlayInfo {
        PlayInfo {
            video_tracks: vec![
                variant(80, 12, 900),
                variant(80, 7, 700),
                variant(80, 13, 1100),
                variant(32, 7, 300),
            ],
            qualities: Vec::new(),
            default_qn: 32,
            audio_tracks: Vec::new(),
            duration: 0,
        }
    }

    #[test]
    fn auto_prefers_lowest_bandwidth() {
        assert_eq!(sample().video_track_for(80, 0).map(|t| t.codecid), Some(7));
    }

    #[test]
    fn codec_pref_overrides_bandwidth() {
        assert_eq!(sample().video_track_for(80, 7).map(|t| t.codecid), Some(7));
        assert_eq!(sample().video_track_for(80, 12).map(|t| t.codecid), Some(12));
        assert_eq!(sample().video_track_for(80, 13).map(|t| t.codecid), Some(13));
    }

    #[test]
    fn missing_codec_falls_back_to_auto() {
        assert_eq!(sample().video_track_for(80, 99).map(|t| t.codecid), Some(7));
    }

    #[test]
    fn unknown_quality_falls_back_to_default() {
        assert_eq!(sample().video_track_for(999, 0).map(|t| t.id), Some(32));
    }

    #[test]
    fn track_urls_prefer_primary_and_dedupe_backups() {
        let track: super::DashTrack = match serde_json::from_str(
            r#"{"baseUrl":"p","base_url":"p","backupUrl":["b1","b2"],"backup_url":["b2","b3"]}"#,
        ) {
            Ok(track) => track,
            Err(_) => return assert!(false, "DashTrack did not parse"),
        };
        let got = super::track_urls(&track);
        assert_eq!(
            got.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["p", "b1", "b2", "b3"]
        );
    }

    #[test]
    fn collect_video_tracks_exposes_backup_urls() {
        let track: super::DashTrack = match serde_json::from_str(
            r#"{"id":32,"codecid":7,"bandwidth":300,"baseUrl":"p","backupUrl":["b1"]}"#,
        ) {
            Ok(track) => track,
            Err(_) => return assert!(false, "DashTrack did not parse"),
        };
        let tracks = super::collect_video_tracks(&[track]);
        assert_eq!(tracks.first().map(|t| t.url.clone()), Some("p".to_string()));
        assert_eq!(
            tracks.first().map(|t| t.urls.clone()),
            Some(vec!["p".to_string(), "b1".to_string()])
        );
    }

    #[test]
    fn frame_rate_parsed_from_string_number_and_default() {
        let parse = |json: &str| {
            let track: super::DashTrack = serde_json::from_str(json).ok()?;
            super::collect_video_tracks(&[track])
                .first()
                .map(|track| track.frame_rate)
        };
        assert_eq!(
            parse(r#"{"id":80,"codecid":7,"frameRate":"60.000","baseUrl":"u"}"#),
            Some(60.0)
        );
        assert_eq!(
            parse(r#"{"id":80,"codecid":7,"frameRate":30,"baseUrl":"u"}"#),
            Some(30.0)
        );
        assert_eq!(parse(r#"{"id":80,"codecid":7,"baseUrl":"u"}"#), Some(30.0));
    }

    #[test]
    fn qn_label_covers_every_documented_tier() {
        for (qn, label) in [
            (6, "240P"),
            (16, "360P"),
            (32, "480P"),
            (64, "720P"),
            (74, "720P60"),
            (80, "1080P"),
            (100, "智能修复"),
            (112, "1080P+"),
            (116, "1080P60"),
            (120, "4K 超清"),
            (125, "HDR 真彩"),
            (126, "杜比视界"),
            (127, "8K 超清"),
            (129, "HDR Vivid"),
        ] {
            assert_eq!(super::qn_label(qn), label, "qn {qn}");
            assert_eq!(super::quality_label(qn), label, "owned label for qn {qn}");
        }
        // Auto (0) and anything undocumented fall back to the API's own tier.
        assert_eq!(super::qn_label(0), "默认");
        assert_eq!(super::qn_label(9999), "默认");
        assert!(!super::is_vip_quality(80));
        for qn in [112, 116, 120, 125, 126, 127, 129] {
            assert!(super::is_vip_quality(qn), "qn {qn} is 大会员-only");
        }
    }

    /// One `playurl` body with the whole matrix: 8K AV1, 杜比视界, HDR,
    /// two 4K variants (AVC + HEVC), 1080P60, two plain 1080P codecs, the three
    /// regular audio tiers, Dolby audio and Hi-Res flac.
    const DASH_FIXTURE: &str = r#"{
        "code": 0,
        "data": {
            "timelength": 360000,
            "dash": {
                "duration": 360,
                "video": [
                    {"id":127,"codecid":13,"codecs":"av01.0.16M.08","width":7680,"height":4320,"bandwidth":60000000,"frameRate":"60.000","baseUrl":"https://x/8k-av1.m4s","backupUrl":[]},
                    {"id":126,"codecid":12,"codecs":"hev1.2.4.L153","width":3840,"height":2160,"bandwidth":18000000,"frameRate":"60.000","baseUrl":"https://x/dv-hevc.m4s"},
                    {"id":125,"codecid":12,"codecs":"hev1.2.4.L153","width":3840,"height":2160,"bandwidth":15000000,"frameRate":60,"baseUrl":"https://x/hdr-hevc.m4s"},
                    {"id":120,"codecid":7,"codecs":"avc1.640033","width":3840,"height":2160,"bandwidth":12000000,"frameRate":"60.000","baseUrl":"https://x/4k-avc.m4s","backupUrl":["https://b/4k-avc.m4s"]},
                    {"id":120,"codecid":12,"codecs":"hev1.2.4.L153","width":3840,"height":2160,"bandwidth":9000000,"frameRate":"60.000","baseUrl":"https://x/4k-hevc.m4s"},
                    {"id":116,"codecid":7,"codecs":"avc1.640032","width":1920,"height":1080,"bandwidth":6000000,"frameRate":"60.000","baseUrl":"https://x/1080p60-avc.m4s"},
                    {"id":80,"codecid":12,"codecs":"hev1.2.4.L120","width":1920,"height":1080,"bandwidth":3000000,"frameRate":"30.000","baseUrl":"https://x/1080p-hevc.m4s"},
                    {"id":80,"codecid":13,"codecs":"av01.0.08M.08","width":1920,"height":1080,"bandwidth":2500000,"frameRate":"30.000","baseUrl":"https://x/1080p-av1.m4s"}
                ],
                "audio": [
                    {"id":30216,"bandwidth":64000,"baseUrl":"https://x/a64.m4s"},
                    {"id":30232,"bandwidth":132000,"baseUrl":"https://x/a132.m4s"},
                    {"id":30280,"bandwidth":192000,"baseUrl":"https://x/a192.m4s","backupUrl":["https://b/a192.m4s"]}
                ],
                "dolby": {"audio": {"id":30250,"bandwidth":448000,"baseUrl":"https://x/dolby.m4s"}},
                "flac": {"audio": [{"id":30251,"bandwidth":1411200,"baseUrl":"https://x/flac.m4s"}]}
            }
        }
    }"#;

    fn parse_fixture(json: &str) -> Option<super::PlayInfo> {
        let resp: super::PlayUrlResp = serde_json::from_str(json).ok()?;
        super::build_play_info(resp.data?)
    }

    #[test]
    fn dash_fixture_exposes_every_stream_and_flag() {
        let info = match parse_fixture(DASH_FIXTURE) {
            Some(info) => info,
            None => return assert!(false, "DASH fixture did not parse"),
        };
        // One entry per raw stream: duplicate `qn` values are kept on purpose.
        assert_eq!(info.qualities.len(), 8);
        let mut sorted = true;
        for pair in info.qualities.windows(2) {
            let (left, right) = (&pair[0], &pair[1]);
            sorted &= left.qn > right.qn
                || (left.qn == right.qn && left.bandwidth_bps >= right.bandwidth_bps);
        }
        assert!(sorted, "qualities sort qn desc, then bitrate desc");

        let eight_k = match info.qualities.first() {
            Some(quality) => quality,
            None => return assert!(false, "no qualities"),
        };
        assert_eq!(eight_k.qn, 127);
        assert_eq!(eight_k.label, "8K 超清");
        assert_eq!((eight_k.width, eight_k.height), (7680, 4320));
        assert_eq!(eight_k.fps, 60.0);
        assert_eq!(eight_k.codecid, 13);
        assert_eq!(eight_k.codec, "AV1");
        assert_eq!(eight_k.bandwidth_bps, 60_000_000);
        assert!(eight_k.eight_k && !eight_k.hdr && !eight_k.dolby_vision);

        let dolby_vision = info.qualities.iter().find(|q| q.qn == 126);
        assert_eq!(
            dolby_vision.map(|q| (q.label.as_str(), q.hdr, q.dolby_vision, q.eight_k)),
            Some(("杜比视界", false, true, false))
        );
        let hdr = info.qualities.iter().find(|q| q.qn == 125);
        assert_eq!(
            hdr.map(|q| (q.label.as_str(), q.hdr, q.dolby_vision, q.eight_k)),
            Some(("HDR 真彩", true, false, false))
        );

        let four_k_avc = info.qualities.iter().find(|q| q.qn == 120 && q.codecid == 7);
        assert_eq!(
            four_k_avc.map(|q| (q.label.as_str(), q.fps, q.bandwidth_bps)),
            Some(("4K 超清", 60.0, 12_000_000))
        );
        assert_eq!(
            four_k_avc.map(|q| q.backup_urls.clone()),
            Some(vec!["https://b/4k-avc.m4s".to_string()])
        );
        let p60 = info.qualities.iter().find(|q| q.qn == 116);
        assert_eq!(p60.map(|q| q.label.as_str()), Some("1080P60"));
        assert_eq!(info.qualities.iter().filter(|q| q.qn == 80).count(), 2);

        // Playback selection is unchanged: auto = lowest bitrate at the qn.
        assert_eq!(info.video_track_for(120, 0).map(|t| t.codecid), Some(12));
        assert_eq!(info.video_track_for(120, 7).map(|t| t.bandwidth), Some(12_000_000));
        assert_eq!(info.default_qn, 80);

        // Audio matrix: the three regular tiers plus 杜比全景声 and 无损.
        assert_eq!(info.audio_tracks.len(), 5);
        let audio = |id: i64| info.audio_tracks.iter().find(|track| track.id == id);
        assert_eq!(
            audio(30250).map(|track| (track.dolby, track.hi_res, track.vip)),
            Some((true, false, true))
        );
        assert_eq!(
            audio(30251).map(|track| (track.dolby, track.hi_res, track.vip)),
            Some((false, true, true))
        );
        assert_eq!(
            audio(30280).map(|track| (track.dolby, track.hi_res, track.vip)),
            Some((false, false, false))
        );
        assert_eq!(
            audio(30280).map(|track| track.urls.len()),
            Some(2),
            "backup mirror kept"
        );
    }

    /// Old/unusual video: no `dash` object at all, only the progressive `durl`.
    const DURL_FIXTURE: &str =
        r#"{"code":0,"data":{"durl":[{"url":"https://x/durl.m4s"}],"timelength":125000}}"#;

    #[test]
    fn response_without_dash_keeps_the_old_path() {
        let info = match parse_fixture(DURL_FIXTURE) {
            Some(info) => info,
            None => return assert!(false, "durl fixture did not parse"),
        };
        assert!(info.qualities.is_empty());
        assert_eq!(info.video_tracks.len(), 1);
        assert_eq!(info.video_tracks.first().map(|t| t.url.clone()), Some("https://x/durl.m4s".to_string()));
        assert_eq!(info.video_tracks.first().map(|t| t.id), Some(0));
        assert_eq!(info.default_qn, 0);
        assert_eq!(info.duration, 125);
        assert!(info.audio_tracks.is_empty());
    }
}

#[cfg(test)]
mod tier_tests {
    use super::{
        build_quality_tiers, quality_tiers_for, remember_quality_tiers, tiers_qa_bvid, PlayUrlData,
    };

    fn data(json: &str) -> PlayUrlData {
        match serde_json::from_str(json) {
            Ok(data) => data,
            Err(err) => panic!("tier fixture did not parse: {err}"),
        }
    }

    /// The real shape for a 4K video on a non-VIP account: the tier lists name
    /// 4K/1080P60, `dash.video[]` only has 1080P and below.
    const FOUR_K_NON_VIP: &str = r#"{
        "accept_quality":[120,116,80,64],
        "accept_description":["超清 4K","高清 1080P60","高清 1080P","高清 720P"],
        "support_formats":[
            {"quality":120,"new_description":"4K 超清","display_desc":"4K","codecs":["avc1.640034"]},
            {"quality":116,"new_description":"1080P 60帧","display_desc":"1080P","codecs":["avc1.640032"]}
        ],
        "dash":{"video":[
            {"id":80,"frameRate":"29.97","baseUrl":"https://x/80"},
            {"id":64,"frameRate":"29.97","baseUrl":"https://x/64"}
        ]}
    }"#;

    #[test]
    fn advertised_tiers_include_the_locked_vip_ones() {
        let tiers = build_quality_tiers(&data(FOUR_K_NON_VIP), 0);
        assert_eq!(
            tiers.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![120, 116, 80, 64],
            "accept_quality order, high→low"
        );
        let tier = |qn: i64| tiers.iter().find(|tier| tier.qn == qn).expect("tier");
        assert!(tier(120).locked, "4K has no returned stream");
        assert!(tier(116).locked, "1080P60 has no returned stream");
        assert!(!tier(80).locked, "1080P has a returned stream");
        assert_eq!(tier(116).label, "1080P60");
        assert_eq!(tier(116).detail, "1920×1080 · 60fps");
        assert_eq!(tier(120).detail, "3840×2160 · 30fps", "video's own fps");
        assert_eq!(tier(120).note(), "需要大会员", "locked VIP tier upsell");
        assert_eq!(tier(116).note(), "需要大会员");
        assert_eq!(tier(80).note(), "", "a playable row has no note");
    }

    #[test]
    fn a_video_without_a_tier_list_invents_nothing() {
        let tiers = build_quality_tiers(
            &data(r#"{"dash":{"video":[{"id":80,"frameRate":"30"}]}}"#),
            0,
        );
        assert_eq!(
            tiers.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![80],
            "only the stream that came back, with no known native size"
        );
        assert!(!tiers[0].locked);
    }

    #[test]
    fn a_1080p_upload_never_gets_a_4k_or_8k_row() {
        let tiers = build_quality_tiers(
            &data(r#"{"dash":{"video":[{"id":80,"frameRate":"25"}]}}"#),
            1080,
        );
        assert_eq!(
            tiers.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![80],
            "a 1080p25 source has no 1080P+/4K/8K/1080P60 tier to offer"
        );
    }

    #[test]
    fn the_native_size_fills_in_the_tiers_the_api_kept_quiet_about() {
        // Nothing advertised at all, 2160p source, 29.97 fps: the two tiers a
        // 4K source is known to have are derived and locked.
        let tiers = build_quality_tiers(
            &data(r#"{"dash":{"video":[{"id":80,"frameRate":"29.97"}]}}"#),
            2160,
        );
        assert_eq!(
            tiers.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![120, 112, 80]
        );
        let tier = |qn: i64| tiers.iter().find(|tier| tier.qn == qn).expect("tier");
        assert!(tier(120).locked && tier(112).locked);
        assert_eq!(tier(120).note(), "需要大会员");
        assert_eq!(tier(112).note(), "需要大会员");
        assert!(!tier(80).locked, "the returned stream stays playable");
    }

    #[test]
    fn an_8k_source_derives_8k_and_a_1080p60_source_derives_1080p60() {
        let eight_k = build_quality_tiers(
            &data(r#"{"dash":{"video":[{"id":80,"frameRate":"25"}]}}"#),
            4320,
        );
        assert_eq!(
            eight_k.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![127, 120, 112, 80]
        );
        let sixty = build_quality_tiers(
            &data(r#"{"dash":{"video":[{"id":80,"frameRate":"59.94"}]}}"#),
            1080,
        );
        assert_eq!(
            sixty.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![116, 80],
            "1080P60 needs a 60 fps source, and is the only derived tier"
        );
        assert!(sixty[0].locked);
    }

    #[test]
    fn a_returned_stream_the_api_did_not_list_is_a_playable_tier() {
        let tiers = build_quality_tiers(
            &data(
                r#"{"accept_quality":[80],"dash":{"video":[{"id":100,"frameRate":"30"},{"id":80,"frameRate":"30"}]}}"#,
            ),
            0,
        );
        assert_eq!(
            tiers.iter().map(|tier| tier.qn).collect::<Vec<_>>(),
            vec![100, 80]
        );
        assert!(!tiers[0].locked, "it came back, so it is playable");
        assert_eq!(tiers[0].note(), "", "only locked VIP tiers carry a note");
    }

    #[test]
    fn the_tiers_hook_reads_the_env_var() {
        // Never mutate process env here (tests run in parallel); just make sure
        // the accessor does not panic and is wired to `BILI_TIERS_TEST`.
        let value = tiers_qa_bvid();
        assert!(value.is_none() || value.iter().all(|item| !item.is_empty()));
    }

    #[test]
    fn the_tier_cache_is_keyed_by_video() {
        let tiers = build_quality_tiers(&data(FOUR_K_NON_VIP), 0);
        remember_quality_tiers("BVcacheTest", 42, tiers.clone());
        assert_eq!(quality_tiers_for("BVcacheTest", 42), Some(tiers));
        assert_eq!(
            quality_tiers_for("BVcacheTest", 43),
            None,
            "another part never reads this part's list"
        );
        assert_eq!(quality_tiers_for("BVneverFetched", 1), None);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::heartbeat_body;

    #[test]
    fn heartbeat_body_carries_the_position_and_the_play_flags() {
        let body = heartbeat_body(170001, "BV1xx411c7mD", 62131, 1248, 300, 3600, 1_592_720_840, 0, "abc123");
        for field in [
            "aid=170001",
            "bvid=BV1xx411c7mD",
            "cid=62131",
            "played_time=1248",
            "realtime=300",
            "start_ts=1592720840",
            "type=3",
            "sub_type=0",
            "dt=2",
            "play_type=0",
            "video_duration=3600",
            "csrf=abc123",
        ] {
            assert!(body.contains(field), "missing {field} in {body}");
        }
    }

    #[test]
    fn heartbeat_body_encodes_the_csrf_value() {
        let body = heartbeat_body(1, "BV1", 2, 3, 4, 5, 6, 2, "a b&c");
        assert!(body.contains("csrf=a%20b%26c"), "{body}");
    }
}

#[cfg(test)]
mod cdn_tests {
    use super::{
        classify_cdn, cdn_rank, curated_mirror_hosts, cdn_qa_bvid, is_rewritable, rewrite_host,
        CdnKind,
    };

    const PATH: &str = "/upgcxcode/12/34/56789.m4s?e=1761000000&deadline=1761000600&os=cos&platform=pc&upsig=abc123&uparams=e,deadline,os,platform&uipk=5&nbs=1";

    fn url(host: &str) -> String {
        format!("https://{host}{PATH}")
    }

    #[test]
    fn mirrors_classified_for_every_provider_prefix() {
        for host in [
            "upos-sz-mirrorali.bilivideo.com",
            "upos-hz-mirrorali.bilivideo.com",
            "upos-sz-mirrorcos.bilivideo.com",
            "upos-hz-mirrorcosb.bilivideo.com",
            "upos-sz-mirrorhw.bilivideo.com",
            "upos-hz-mirrorhwb.bilivideo.com",
            "upos-sz-mirror08c.bilivideo.com",
            "upos-hz-mirrorbd.bilivideo.com",
            "upos-sz-mirrorcf.bilivideo.com",
        ] {
            assert_eq!(classify_cdn(&url(host)), CdnKind::Mirror, "{host}");
        }
        assert_eq!(cdn_rank(CdnKind::Mirror), 0);
    }

    #[test]
    fn akamaized_is_a_mirror_but_not_a_rewrite_target() {
        let akam = url("upos-hz-mirrorakam.akamaized.net");
        assert_eq!(classify_cdn(&akam), CdnKind::Mirror);
        assert!(is_rewritable(&akam));
        // It needs its own `hdnts` token: rewriting to it would 403.
        assert_eq!(
            rewrite_host(&url("upos-sz-mirrorali.bilivideo.com"), "upos-hz-mirrorakam.akamaized.net"),
            None
        );
    }

    #[test]
    fn upos_storage_and_bcache_hosts() {
        assert_eq!(
            classify_cdn(&url("upos-sz-estgoss.bilivideo.com")),
            CdnKind::Upos
        );
        assert_eq!(
            classify_cdn(&url("upos-hz-mirrorcos.bilivideo.com")),
            CdnKind::Mirror
        );
        assert_eq!(
            classify_cdn(&url("cn-hbxy-cu-01-11.bilivideo.com")),
            CdnKind::BCache
        );
        assert_eq!(
            classify_cdn(&url("cn-hbxy-cu-01-11.bilivideo.cn")),
            CdnKind::BCache
        );
        assert_eq!(
            classify_cdn(&url("upos-sz-estgoss.bilivideo.cn")),
            CdnKind::Upos
        );
    }

    #[test]
    fn mcdn_hosts_are_worst() {
        assert_eq!(
            classify_cdn(&url("xy1x2x3x4x5.mcdn.bilivideo.cn:4483")),
            CdnKind::Mcdn
        );
        assert_eq!(
            classify_cdn(&url("1-2-3-4.mcdn.bilivideo.com")),
            CdnKind::Mcdn
        );
        assert_eq!(cdn_rank(CdnKind::Mcdn), 4);
        assert!(cdn_rank(CdnKind::Mcdn) > cdn_rank(CdnKind::BCache));
    }

    #[test]
    fn ip_port_and_v1_resource_are_not_rewritable() {
        let pcdn = "http://1.2.3.4:8080/v1/resource/abc/def?sig=1";
        assert_eq!(classify_cdn(pcdn), CdnKind::IpPort);
        assert!(!is_rewritable(pcdn));
        assert_eq!(
            rewrite_host(pcdn, "upos-sz-mirrorali.bilivideo.com"),
            None
        );
        assert_eq!(cdn_rank(CdnKind::IpPort), 5);

        let bare_ip = "http://10.0.0.7:8080/upgcxcode/1/2/x.m4s?os=mcdn";
        assert_eq!(classify_cdn(bare_ip), CdnKind::IpPort);
        assert!(!is_rewritable(bare_ip));

        let resource_path = url("upos-sz-mirrorali.bilivideo.com");
        let resource_path = resource_path.replace("/upgcxcode/12/34/56789.m4s", "/v1/resource/1/2");
        assert_eq!(classify_cdn(&resource_path), CdnKind::IpPort);
        assert!(!is_rewritable(&resource_path));
    }

    #[test]
    fn os_parameter_is_used_only_for_unknown_hosts() {
        // Host wins: a rewritten mirror keeps the original `os=` byte-for-byte.
        let contradicting = format!("https://upos-sz-mirrorali.bilivideo.com{PATH}&x=1")
            .replace("os=cos", "os=mcdn");
        assert_eq!(classify_cdn(&contradicting), CdnKind::Mirror);

        let unknown = "https://media.example.com/a.m4s?os=mcdn";
        assert_eq!(classify_cdn(unknown), CdnKind::Mcdn);
        assert_eq!(
            classify_cdn("https://media.example.com/a.m4s?os=bcache"),
            CdnKind::BCache
        );
        assert_eq!(
            classify_cdn("https://media.example.com/a.m4s?os=ali"),
            CdnKind::Upos
        );
        assert_eq!(
            classify_cdn("https://media.example.com/a.m4s"),
            CdnKind::Other
        );
        assert_eq!(classify_cdn("not a url"), CdnKind::Other);
        assert_eq!(classify_cdn(""), CdnKind::Other);
    }

    #[test]
    fn rank_orders_mirror_upos_bcache_other_mcdn_ip() {
        let ranks: Vec<u8> = [
            CdnKind::Mirror,
            CdnKind::Upos,
            CdnKind::BCache,
            CdnKind::Other,
            CdnKind::Mcdn,
            CdnKind::IpPort,
        ]
        .iter()
        .map(|kind| cdn_rank(*kind))
        .collect();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn rewrite_keeps_path_and_query_byte_identical() {
        let source = url("upos-sz-mirrorali.bilivideo.com");
        let target = "upos-sz-mirrorhw.bilivideo.com";
        let rewritten = rewrite_host(&source, target).unwrap_or_default();
        assert_eq!(rewritten, format!("https://{target}{PATH}"));
        assert_eq!(
            rewritten.split_once("://").map(|(_, rest)| rest),
            Some(format!("{target}{PATH}").as_str())
        );
        // No trailing-slash or encoding drift.
        assert_eq!(
            rewritten.trim_start_matches(format!("https://{target}").as_str()),
            PATH
        );

        assert_eq!(rewrite_host(&source, ""), None);
        assert_eq!(rewrite_host(&source, "https://x.bilivideo.com/"), None);
        assert_eq!(rewrite_host("https://1.2.3.4:80/a.m4s", target), None);
        assert_eq!(
            rewrite_host("https://upos-sz-mirrorali.bilivideo.com/v1/resource/a", target),
            None
        );
    }

    #[test]
    fn rewritable_needs_a_real_host() {
        assert!(is_rewritable(&url("upos-sz-mirrorali.bilivideo.com")));
        assert!(is_rewritable(&url("cn-hbxy-cu-01-11.bilivideo.com")));
        assert!(!is_rewritable(""));
        assert!(!is_rewritable("/upgcxcode/1.m4s"));
        assert!(!is_rewritable("https:///a.m4s"));
    }

    #[test]
    fn curated_hosts_are_bilivideo_mirrors_only() {
        for host in curated_mirror_hosts() {
            assert!(host.ends_with(".bilivideo.com"), "{host}");
            assert!(host.starts_with("upos-"), "{host}");
            assert!(!host.contains("akamaized"), "{host}");
            assert_eq!(
                classify_cdn(&format!("https://{host}{PATH}")),
                CdnKind::Mirror,
                "{host}"
            );
        }
        assert!(!curated_mirror_hosts().is_empty());
    }

    #[test]
    fn qa_hook_reads_the_env_var() {
        // Never mutate process env here (tests run in parallel); just make sure
        // the accessor does not panic and is wired to `BILI_CDN_TEST`.
        let value = cdn_qa_bvid();
        assert!(value.is_none() || value.iter().all(|item| !item.is_empty()));
    }

    /// Live QA. Runs only when `BILI_CDN_TEST=<bvid>` is set, and reports through
    /// `BILI_DEBUG_LOG` (`cargo test --release cdn_qa_live -- --nocapture`).
    #[test]
    fn cdn_qa_live() {
        let Some(bvid) = cdn_qa_bvid() else {
            return;
        };
        let Some(runtime) = super::shared_runtime() else {
            return;
        };
        let ok = runtime.block_on(async {
            let session = match super::load_token() {
                Some(token) => super::Session::from_token(&token).await.ok(),
                None => None,
            };
            let session = match session {
                Some(session) => Some(session),
                None => super::Session::new().await.ok(),
            };
            let Some(session) = session else {
                return false;
            };
            session.cdn_selftest(&bvid).await.is_ok()
        });
        crate::debug_log(&format!("cdn qa done ok={ok}"));
    }

    /// Live quality QA. Runs only when `BILI_QUALITY_TEST=<bvid>` is set and
    /// writes the table through `BILI_DEBUG_LOG`; `BILI_QUALITY_FNVAL=<n>`
    /// requests a different `fnval` so two runs can be diffed:
    /// `cargo test --release quality_qa_live`.
    #[test]
    fn quality_qa_live() {
        let Some(bvid) = super::quality_qa_bvid() else {
            return;
        };
        let Some(runtime) = super::shared_runtime() else {
            return;
        };
        let error = runtime.block_on(async {
            let session = match super::load_token() {
                Some(token) => super::Session::from_token(&token).await.ok(),
                None => None,
            };
            let session = match session {
                Some(session) => Some(session),
                None => super::Session::new().await.ok(),
            };
            let Some(session) = session else {
                return Some("no-session".to_string());
            };
            session
                .quality_selftest(&bvid)
                .await
                .err()
                .map(|err| format!("{err:#}"))
        });
        match error {
            Some(err) => crate::debug_log(&format!("quality qa failed bvid={bvid} err={err}")),
            None => crate::debug_log(&format!("quality qa done bvid={bvid}")),
        }
    }
}

#[cfg(test)]
mod comment_tests {
    use super::{
        comment_page_from, comment_reply_page_from, CommentPage, CommentReplyPage, CommentReplyResp,
        CommentResp,
    };

    /// A realistic `x/v2/reply/wbi/main` body: a pinned comment, two top-level
    /// comments (the first with one inlined preview reply) and a next cursor.
    const MAIN: &str = r#"{
        "code": 0,
        "message": "0",
        "data": {
            "cursor": {
                "is_end": false,
                "all_count": 1234,
                "pagination_reply": { "next_offset": "CURSOR-NEXT" }
            },
            "top": {
                "rpid": 9001,
                "content": { "message": "置顶评论" },
                "member": { "mid": "42", "uname": "UP主", "avatar": "//i0.hdslb.com/up.png" },
                "like": 100, "ctime": 1600000000, "rcount": 3
            },
            "replies": [
                {
                    "rpid": 100,
                    "content": { "message": "第一条评论" },
                    "member": { "mid": 7, "uname": "用户A", "avatar": "http://i0.hdslb.com/a.png" },
                    "like": 5, "ctime": 1600000100, "rcount": 2,
                    "replies": [
                        {
                            "rpid": 101,
                            "content": { "message": "楼中楼" },
                            "member": { "mid": "8", "uname": "用户B", "avatar": "" },
                            "like": "1", "ctime": 1600000200, "rcount": 0
                        }
                    ]
                },
                {
                    "rpid": 200,
                    "content": { "message": "第二条" },
                    "member": { "mid": "9", "uname": "用户C", "avatar": "//i0.hdslb.com/c.png" },
                    "like": 0, "ctime": 1600000300, "rcount": 0,
                    "replies": null
                }
            ]
        }
    }"#;

    const EMPTY: &str = r#"{
        "code": 0, "message": "0",
        "data": {
            "cursor": { "is_end": true, "all_count": 0, "pagination_reply": null },
            "replies": null, "top": null
        }
    }"#;

    const NO_DATA: &str = r#"{"code": -404, "message": "啥都木有"}"#;

    const REPLIES: &str = r#"{
        "code": 0, "message": "0",
        "data": {
            "page": { "num": 1, "size": 20, "count": 25 },
            "replies": [
                {
                    "rpid": 1, "content": { "message": "a" },
                    "member": { "mid": "1", "uname": "u1", "avatar": "" },
                    "like": 0, "ctime": 1, "rcount": 0
                },
                {
                    "rpid": 2, "content": { "message": "b" },
                    "member": { "mid": 2, "uname": "u2", "avatar": "//i0.hdslb.com/b.png" },
                    "like": 3, "ctime": 2, "rcount": 0
                }
            ]
        }
    }"#;

    const REPLIES_LAST: &str = r#"{
        "code": 0, "message": "0",
        "data": {
            "page": { "num": 2, "size": 20, "count": 25 },
            "replies": null
        }
    }"#;

    fn parse_main(json: &str) -> CommentPage {
        let resp: CommentResp = match serde_json::from_str(json) {
            Ok(resp) => resp,
            Err(_) => return CommentPage::default(),
        };
        comment_page_from(resp.data.unwrap_or_default())
    }

    fn parse_replies(json: &str, pn: i64) -> CommentReplyPage {
        let resp: CommentReplyResp = match serde_json::from_str(json) {
            Ok(resp) => resp,
            Err(_) => return CommentReplyPage::default(),
        };
        comment_reply_page_from(resp.data.unwrap_or_default(), pn)
    }

    #[test]
    fn comment_page_parses_top_list_and_cursor() {
        let page = parse_main(MAIN);
        assert_eq!(page.total, 1234);
        assert!(!page.is_end);
        assert_eq!(page.cursor, "CURSOR-NEXT");

        assert_eq!(page.top.as_ref().map(|c| c.rpid), Some(9001));
        assert_eq!(
            page.top.as_ref().map(|c| c.user.uname.clone()),
            Some("UP主".to_string())
        );
        assert_eq!(
            page.top.as_ref().map(|c| c.user.avatar.clone()),
            Some("https://i0.hdslb.com/up.png".to_string())
        );

        assert_eq!(page.comments.len(), 2);
        let first = page.comments.first();
        assert_eq!(first.map(|c| c.rpid), Some(100));
        assert_eq!(first.map(|c| c.user.mid), Some(7));
        assert_eq!(first.map(|c| c.ctime), Some(1600000100));
        assert_eq!(first.map(|c| c.rcount), Some(2));
        assert_eq!(first.map(|c| c.replies.len()), Some(1));
        assert_eq!(
            first.and_then(|c| c.replies.first()).map(|c| c.rpid),
            Some(101)
        );
        // A `"mid"` sent as a string parses the same as a number.
        assert_eq!(
            first.and_then(|c| c.replies.first()).map(|c| c.user.mid),
            Some(8)
        );
        assert_eq!(page.comments.get(1).map(|c| c.replies.len()), Some(0));
    }

    #[test]
    fn comment_page_handles_nulls() {
        let page = parse_main(EMPTY);
        assert!(page.top.is_none());
        assert!(page.comments.is_empty());
        assert_eq!(page.cursor, "");
        assert!(page.is_end);
        assert_eq!(page.total, 0);
    }

    #[test]
    fn comment_page_without_data_is_empty_and_ended() {
        let page = parse_main(NO_DATA);
        assert!(page.top.is_none());
        assert!(page.comments.is_empty());
        assert!(page.is_end);
    }

    #[test]
    fn comment_reply_page_parses_pager_and_has_more() {
        let page = parse_replies(REPLIES, 1);
        assert_eq!(page.total, 25);
        assert_eq!(page.page, 1);
        assert_eq!(page.size, 20);
        assert!(page.has_more);
        assert_eq!(page.replies.len(), 2);
        assert_eq!(page.replies.first().map(|c| c.rpid), Some(1));
        assert_eq!(
            page.replies.get(1).map(|c| c.user.avatar.clone()),
            Some("https://i0.hdslb.com/b.png".to_string())
        );
    }

    #[test]
    fn comment_reply_page_last_page_has_no_more() {
        let page = parse_replies(REPLIES_LAST, 2);
        assert_eq!(page.page, 2);
        assert_eq!(page.total, 25);
        assert!(!page.has_more);
        assert!(page.replies.is_empty());
    }

    #[test]
    fn comment_reply_page_falls_back_to_requested_page() {
        // No `page` block: the requested `pn` is kept and `size` defaults to 1.
        let page = parse_replies(r#"{"code":0,"data":{"replies":null}}"#, 3);
        assert_eq!(page.page, 3);
        assert_eq!(page.size, 1);
        assert_eq!(page.total, 0);
        assert!(!page.has_more);
    }
}

#[cfg(test)]
mod fav_tests {
    use super::{
        fav_cache, fav_folders_from, fav_page_from, fav_qa_picks, related_from, FavFolder,
        FavFoldersResp, FavListResp, RelatedResp, FAV_RELATED_MAX,
    };

    /// `folder/created/list-all` with the default folder (`id` 0), an empty
    /// folder and one entry that only carries the older `name` field.
    const FOLDERS: &str = r#"{
        "code": 0,
        "message": "0",
        "data": {
            "count": 5,
            "list": [
                { "id": 0, "fid": 0, "mid": 1675575430, "attr": 0,
                  "title": "默认收藏夹", "media_count": 9, "fav_state": 0 },
                { "id": 3917874230, "fid": 39178742, "mid": 1675575430,
                  "title": "练习", "media_count": 43 },
                { "id": 4093664630, "fid": 40936646, "mid": 1675575430,
                  "title": "服装", "media_count": 1 },
                { "id": 4000000000, "fid": 40000000, "mid": 1675575430,
                  "title": "空的", "media_count": 0 },
                { "id": 4000000001, "fid": 40000001, "mid": 1675575430,
                  "name": "别名夹", "media_count": 2 }
            ],
            "season": null
        }
    }"#;

    /// One `fav/resource/list` page: a real item, a `null` (deleted) slot and an
    /// item without a `bvid`.
    const FAV_PAGE: &str = r#"{
        "code": 0,
        "message": "0",
        "data": {
            "info": { "id": 3917874230, "title": "练习" },
            "medias": [
                { "id": 117285507301644, "type": 2, "title": "真视频",
                  "cover": "http://i1.hdslb.com/bfs/archive/a.jpg",
                  "duration": 398, "bvid": "BV1RLeg6rEri",
                  "pubtime": 1690000123,
                  "upper": { "mid": 25078231, "name": "太太" },
                  "cnt_info": { "collect": 1, "play": 66170 },
                  "ugc": { "first_cid": 41969583233 } },
                null,
                { "id": 116548920412362, "type": 2, "title": "已失效",
                  "cover": "", "duration": 10, "bvid": "",
                  "upper": { "mid": 1, "name": "x" },
                  "cnt_info": { "play": 2 } }
            ],
            "has_more": true
        }
    }"#;

    /// One `archive/related` payload: a real video (the live payload has no
    /// `goto`), an ad without a `bvid` and an entry with a non-`av` `goto`.
    const RELATED: &str = r#"{
        "code": 0,
        "message": "0",
        "data": [
            { "aid": 117312719953875, "cid": 42100002644, "bvid": "BV1Nbhk6GERE",
              "title": "真视频", "pic": "http://i0.hdslb.com/bfs/archive/b.jpg",
              "duration": 553, "pubdate": 1689000456,
              "owner": { "mid": 3275073, "name": "某人" },
              "stat": { "view": 156760 } },
            { "aid": 0, "cid": 0, "bvid": "", "title": "广告",
              "goto": "ad", "duration": 0 },
            { "aid": 1, "cid": 2, "bvid": "BV1cm0000000", "title": "别的类型",
              "goto": "cm", "duration": 5, "owner": { "name": "x" },
              "stat": { "view": 1 } }
        ]
    }"#;

    fn folders(json: &str) -> Vec<FavFolder> {
        match serde_json::from_str::<FavFoldersResp>(json) {
            Ok(resp) => fav_folders_from(resp.data),
            Err(_) => Vec::new(),
        }
    }

    fn page(json: &str) -> (Vec<super::FeedVideo>, bool) {
        match serde_json::from_str::<FavListResp>(json) {
            Ok(resp) => fav_page_from(resp.data),
            Err(_) => (Vec::new(), false),
        }
    }

    fn related(json: &str) -> Vec<super::FeedVideo> {
        match serde_json::from_str::<RelatedResp>(json) {
            Ok(resp) => related_from(resp.data.unwrap_or_default()),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn folder_list_drops_the_default_and_empty_folders() {
        let folders = folders(FOLDERS);
        assert_eq!(
            folders,
            vec![
                FavFolder {
                    id: 3917874230,
                    name: "练习".to_string(),
                    count: 43,
                },
                FavFolder {
                    id: 4093664630,
                    name: "服装".to_string(),
                    count: 1,
                },
                FavFolder {
                    id: 4000000001,
                    name: "别名夹".to_string(),
                    count: 2,
                },
            ]
        );
    }

    #[test]
    fn folder_list_without_data_is_empty() {
        assert!(folders(r#"{"code":0,"data":null}"#).is_empty());
        assert!(folders(r#"{"code":0,"data":{"list":null}}"#).is_empty());
    }

    #[test]
    fn folder_page_skips_null_and_bvidless_entries() {
        let (items, has_more) = page(FAV_PAGE);
        assert!(has_more);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.bvid, "BV1RLeg6rEri");
        assert_eq!(item.aid, 117285507301644);
        assert_eq!(item.cid, 41969583233);
        assert_eq!(item.title, "真视频");
        assert_eq!(item.owner, "太太");
        assert_eq!(item.owner_mid, 25078231);
        assert_eq!(item.view, 66170);
        assert_eq!(item.duration, 398);
        assert_eq!(item.pub_ts, 1690000123);
        assert_eq!(item.pic, "http://i1.hdslb.com/bfs/archive/a.jpg");
    }

    #[test]
    fn folder_page_without_data_is_empty_and_ended() {
        let (items, has_more) = page(r#"{"code":0,"data":null}"#);
        assert!(items.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn related_skips_ads_and_non_av_entries() {
        let items = related(RELATED);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].bvid, "BV1Nbhk6GERE");
        assert_eq!(items[0].cid, 42100002644);
        assert_eq!(items[0].aid, 117312719953875);
        assert_eq!(items[0].owner, "某人");
        assert_eq!(items[0].view, 156760);
        assert_eq!(items[0].pub_ts, 1689000456);
    }

    #[test]
    fn empty_related_list_is_empty() {
        assert!(related(r#"{"code":0,"data":[]}"#).is_empty());
        assert!(related(r#"{"code":0,"data":null}"#).is_empty());
    }

    #[test]
    fn related_cache_is_bounded() {
        for index in 0..(FAV_RELATED_MAX + 40) {
            super::fav_related_store(&format!("BV1cache{index:07}"), &[]);
        }
        let len = match fav_cache().lock() {
            Ok(guard) => guard.related.len(),
            Err(_) => 0,
        };
        assert!(len <= FAV_RELATED_MAX, "related cache grew to {len}");
    }

    #[test]
    fn qa_picks_are_distinct_and_in_bounds() {
        let items: Vec<super::FeedVideo> = (0..3)
            .map(|index| super::FeedVideo {
                bvid: format!("BV1pick{index}"),
                ..super::FeedVideo::default()
            })
            .collect();
        assert!(fav_qa_picks(&[]).is_empty());
        let one = fav_qa_picks(&items[..1]);
        assert_eq!(one.len(), 1);
        let two = fav_qa_picks(&items);
        assert_eq!(two.len(), 2);
        assert_ne!(two[0].bvid, two[1].bvid);
    }

    /// Live QA. Runs only when `BILI_FAV_TEST=1` is set; the report goes through
    /// `BILI_DEBUG_LOG`:
    /// `cargo test --release fav_qa_live`.
    #[test]
    fn fav_qa_live() {
        if !super::fav_qa_enabled() {
            return;
        }
        let Some(runtime) = super::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(async {
            let session = match super::load_token() {
                Some(token) => super::Session::from_token(&token).await.ok(),
                None => None,
            };
            let session = match session {
                Some(session) => Some(session),
                None => super::Session::new().await.ok(),
            };
            let Some(session) = session else {
                return Err("no-session".to_string());
            };
            session
                .fav_selftest()
                .await
                .map_err(|err| format!("{err:#}"))
        });
        match result {
            Ok(_) => {}
            Err(err) => crate::debug_log(&format!("fav qa failed err={err}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Video comments
// ---------------------------------------------------------------------------

/// The author of a comment (the API's `member` block).
#[derive(Clone, Debug, Default)]
pub struct CommentUser {
    pub mid: i64,
    pub uname: String,
    pub avatar: String,
}

/// One comment: either a top-level comment or one of its replies.
#[derive(Clone, Debug, Default)]
pub struct Comment {
    pub rpid: i64,
    pub user: CommentUser,
    pub message: String,
    pub like: u64,
    /// Unix seconds.
    pub ctime: i64,
    /// Number of replies this comment has (0 for a leaf reply).
    pub rcount: i64,
    /// Nested preview replies the list endpoint inlines (a few per comment).
    pub replies: Vec<Comment>,
}

/// One page of a video's comment list.
#[derive(Clone, Debug, Default)]
pub struct CommentPage {
    /// The pinned/top comment, when the video has one.
    pub top: Option<Comment>,
    pub comments: Vec<Comment>,
    /// Opaque cursor for the next page (`""` when the list has ended).
    pub cursor: String,
    pub is_end: bool,
    pub total: i64,
}

/// One page of a comment's dialog (楼中楼).
#[derive(Clone, Debug, Default)]
pub struct CommentReplyPage {
    pub replies: Vec<Comment>,
    pub total: i64,
    /// 1-based page number this response is for.
    pub page: i64,
    /// Page size the endpoint used.
    pub size: i64,
    pub has_more: bool,
}

// ---------------------------------------------------------------------------
// Search / detail types
// ---------------------------------------------------------------------------

/// One page of search results plus the API's paging metadata.
pub struct SearchPage<T> {
    pub page: i64,
    pub num_pages: i64,
    pub num_results: i64,
    pub items: Vec<T>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchVideo {
    pub bvid: String,
    pub title: String,
    pub description: String,
    pub pic: String,
    pub author: String,
    /// UP主 mid (`0` when the response omits it).
    pub mid: i64,
    pub play: u64,
    /// Ready to display as `mm:ss`.
    pub duration: String,
    /// Publish stamp (Unix seconds, `0` when the payload omits it). The search
    /// result card's freshness chip reads this, exactly like a feed card.
    pub pub_ts: i64,
}

#[derive(Clone, Debug, Default)]
pub struct SearchUser {
    pub mid: i64,
    pub uname: String,
    pub upic: String,
    pub fans: u64,
    pub videos: u64,
    pub usign: String,
}

#[derive(Clone, Debug, Default)]
pub struct VideoDetail {
    pub bvid: String,
    pub aid: i64,
    pub cid: i64,
    pub title: String,
    pub pic: String,
    pub duration: u64,
    pub pubdate: i64,
    pub owner_name: String,
    pub owner_mid: i64,
    pub stat_view: u64,
    pub pages: Vec<VideoPart>,
    /// 合集 / 视频列表 (`ugc_season`), `None` when this video has no list.
    pub season: Option<Season>,
    /// Native picture size from `dimension` (`0` when the API omitted it). Used
    /// to derive which tiers a video can possibly offer.
    pub width: u32,
    pub height: u32,
}

/// One 分P of a video.
#[derive(Clone, Debug, Default)]
pub struct VideoPart {
    /// The part's own `cid` (the danmaku/playurl id).
    pub cid: i64,
    /// 1-based page number the payload reports (`P1`, `P2`, ...).
    pub page: i64,
    /// The upload's own part name (`part`); never empty (`P{n}` fallback).
    pub title: String,
    /// Part duration in seconds (`0` when the payload omitted it).
    pub duration: u64,
}

/// 合集 / 视频列表 (`data.ugc_season`). Episodes are grouped in sections;
/// `ep_count` is the site's own total (which may count episodes this parser
/// truncated away).
#[derive(Clone, Debug, Default)]
pub struct Season {
    pub title: String,
    /// The site's own total (`ep_count`); the parser may have truncated the
    /// section list, so this can be larger than the episodes kept.
    pub ep_count: u32,
    pub sections: Vec<SeasonSection>,
}

/// One section of a 合集 (`ugc_season.sections[]`).
#[derive(Clone, Debug, Default)]
pub struct SeasonSection {
    /// The section title is what the header row shows.
    pub title: String,
    pub episodes: Vec<SeasonEpisode>,
}

/// One episode of a 合集. `cid` is what the player switches to; `bvid` is the
/// episode's own video id (older episodes of a 合集 are separate videos).
#[derive(Clone, Debug, Default)]
pub struct SeasonEpisode {
    pub bvid: String,
    pub aid: i64,
    pub cid: i64,
    pub title: String,
    pub duration: u64,
}

/// Hard caps for a 合集 payload: the site allows far more, but a season list is
/// a *picker*, not a library, and the player's flat model must stay cheap.
const SEASON_MAX_SECTIONS: usize = 20;
const SEASON_MAX_EPISODES: usize = 500;

/// Which order a UP主's 投稿 list is asked for.
///
/// The `order` query value is the whole difference between the two modes: the
/// endpoint returns the same `vlist` shape either way, so the page parser and
/// the cache key are shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpOrder {
    /// 播放最多 (`order=click`).
    Plays,
    /// 最新发布 (`order=pubdate`).
    Newest,
}

impl UpOrder {
    /// The `order` query value this mode sends.
    pub fn param(self) -> &'static str {
        match self {
            UpOrder::Plays => "click",
            UpOrder::Newest => "pubdate",
        }
    }
}

/// UP主 profile from the space page.
#[derive(Clone, Debug, Default)]
pub struct UpInfo {
    pub name: String,
    pub face: String,
    pub sign: String,
    pub level: i64,
    pub fans: i64,
    pub friend: i64,
    pub official: String,
    pub vip: String,
}

/// One投稿 entry on a UP主's space page.
#[derive(Clone, Debug, Default)]
pub struct SpaceVideo {
    pub bvid: String,
    pub title: String,
    pub pic: String,
    pub length: String,
    pub play: u64,
    pub danmaku: u64,
}

/// One page of a UP主's投稿 list.
pub struct SpaceVideoPage {
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<SpaceVideo>,
}

/// One created favourite folder, as the 收藏推荐 stream picks from it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FavFolder {
    /// The `media_id` the folder's contents are fetched with.
    pub id: i64,
    pub name: String,
    pub count: u32,
}

/// One page of a favourite folder's contents.
pub struct FavListPage {
    pub title: String,
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<FavVideo>,
}

#[derive(Clone, Debug, Default)]
pub struct FavVideo {
    pub bvid: String,
    pub title: String,
    pub cover: String,
    pub duration: u64,
    pub upper: String,
    /// The uploader's mid, for the row's UP name.
    pub owner_mid: i64,
    pub play: u64,
}

/// A watch-history page plus the cursor for the next one.
pub struct HistoryPage {
    pub cursor: Option<HistoryCursor>,
    pub items: Vec<HistoryItem>,
    pub has_more: bool,
}

/// One day's historical danmaku plus which endpoint produced it.
pub struct DanmakuHistory {
    pub items: Vec<crate::danmaku::Danmaku>,
    /// `"xml"` when parsed from `x/v2/dm/history`, `"protobuf"` on the fallback.
    pub source: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryCursor {
    pub max: i64,
    pub view_at: i64,
    pub business: String,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryItem {
    pub title: String,
    pub cover: String,
    pub bvid: String,
    pub kid: String,
    pub author: String,
    /// The uploader's mid, for the row's UP name.
    pub owner_mid: i64,
    pub duration: u64,
    pub progress: i64,
}

/// The 稍后再看 list plus its count.
pub struct ToViewPage {
    pub count: i64,
    pub items: Vec<ToViewItem>,
}

#[derive(Clone, Debug, Default)]
pub struct ToViewItem {
    pub bvid: String,
    pub aid: i64,
    pub title: String,
    pub cover: String,
    pub duration: u64,
    pub owner: String,
    pub view: u64,
}

#[derive(Deserialize)]
struct TicketResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<TicketData>,
}

#[derive(Deserialize)]
struct TicketData {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    ttl: u64,
}

#[derive(Deserialize)]
struct SearchResp<T> {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<SearchData<T>>,
}

#[derive(Deserialize)]
struct SearchData<T> {
    #[serde(default)]
    page: i64,
    #[serde(default, rename = "numResults")]
    num_results: i64,
    #[serde(default, rename = "numPages")]
    num_pages: i64,
    result: T,
}

#[derive(Deserialize)]
struct SearchVideoItem {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    pic: String,
    #[serde(default)]
    author: String,
    /// UP主 mid; `0` when the payload omits it.
    #[serde(default)]
    mid: i64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    play: u64,
    #[serde(default, deserialize_with = "de_duration_flexible")]
    duration: String,
    /// Publish stamp (Unix seconds) the search item carries.
    #[serde(default)]
    pubdate: i64,
}

#[derive(Deserialize)]
struct SearchUserItem {
    #[serde(default)]
    mid: i64,
    #[serde(default)]
    uname: String,
    #[serde(default)]
    upic: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    fans: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    videos: u64,
    #[serde(default)]
    usign: String,
}

#[derive(Deserialize)]
struct OfficialVerify {
    #[serde(default)]
    desc: String,
}

/// Turn the parsed `view` payload into the [`VideoDetail`] the rest of the app
/// consumes. Split out so the season/分P rules can be unit-tested against
/// synthetic payloads without any network.
fn detail_from_view(data: ViewData) -> VideoDetail {
    let (owner_name, owner_mid) = match data.owner {
        Some(owner) => (owner.name, owner.mid),
        None => (String::new(), 0),
    };
    let stat = data.stat.unwrap_or_default();
    // 分P: every page keeps the payload's own numbering; the title falls back to
    // `P{n}` only when the upload left `part` empty.
    let pages: Vec<VideoPart> = data
        .pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let number = if page.page > 0 {
                page.page
            } else {
                index as i64 + 1
            };
            VideoPart {
                cid: page.cid.max(0),
                page: number,
                title: if page.part.is_empty() {
                    format!("P{number}")
                } else {
                    page.part.clone()
                },
                duration: page.duration,
            }
        })
        .collect();
    // 合集 / 视频列表 (ugc_season): `None` when the video has none (or the
    // payload is malformed), so "no list" is a single, explicit state.
    let season = data.ugc_season.as_ref().and_then(parse_season);
    let width = data.dimension.as_ref().map(|size| size.width).unwrap_or(0);
    let height = data.dimension.as_ref().map(|size| size.height).unwrap_or(0);
    VideoDetail {
        bvid: data.bvid,
        aid: data.aid,
        cid: data.cid,
        title: data.title,
        pic: normalize_url(&data.pic),
        duration: data.duration,
        pubdate: data.pubdate,
        owner_name,
        owner_mid,
        stat_view: stat.view,
        pages,
        season,
        width,
        height,
    }
}

#[derive(Deserialize)]
struct ViewResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<ViewData>,
}

#[derive(Deserialize)]
struct ViewData {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    aid: i64,
    #[serde(default)]
    cid: i64,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    desc: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pic: String,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    pubdate: i64,
    #[serde(default)]
    tname: String,
    #[serde(default)]
    owner: Option<ViewOwner>,
    #[serde(default)]
    stat: Option<ViewStat>,
    #[serde(default)]
    pages: Vec<ViewPage>,
    /// 合集 / 视频列表. Kept as raw JSON: every list of the payload is parsed
    /// leniently by [`parse_season`], so one wrong field can never fail the
    /// whole video (a missing list is the normal case).
    #[serde(default)]
    ugc_season: Option<serde_json::Value>,
    #[serde(default)]
    dimension: Option<ViewDimension>,
}

#[derive(Deserialize)]
struct ViewDimension {
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Deserialize)]
struct ViewOwner {
    #[serde(default)]
    mid: i64,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct ViewStat {
    #[serde(default)]
    view: u64,
}

#[derive(Deserialize)]
struct ViewPage {
    #[serde(default)]
    cid: i64,
    /// 1-based part number (`0` when the payload omitted it).
    #[serde(default)]
    page: i64,
    /// The upload's part name.
    #[serde(default, deserialize_with = "de_string_lenient")]
    part: String,
    /// Part duration in seconds.
    #[serde(default)]
    duration: u64,
}

#[derive(Deserialize)]
struct SpaceInfoResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<SpaceInfoData>,
}

#[derive(Deserialize)]
struct SpaceInfoData {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    face: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    sign: String,
    #[serde(default)]
    level: i64,
    #[serde(default)]
    fans: i64,
    #[serde(default)]
    friend: i64,
    #[serde(default)]
    official: Option<OfficialInfo>,
    #[serde(default)]
    vip: Option<SpaceVip>,
}

#[derive(Deserialize)]
struct OfficialInfo {
    #[serde(default)]
    desc: String,
}

#[derive(Deserialize)]
struct SpaceVip {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    label: Option<VipLabel>,
}

#[derive(Deserialize)]
struct SpaceArcResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<SpaceArcData>,
}

#[derive(Deserialize, Default)]
struct SpaceArcData {
    #[serde(default)]
    list: SpaceArcList,
    #[serde(default)]
    page: Option<SpacePageInfo>,
}

#[derive(Deserialize, Default)]
struct SpaceArcList {
    #[serde(default)]
    vlist: Vec<SpaceArcItem>,
}

#[derive(Deserialize, Default)]
struct SpacePageInfo {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    ps: i64,
}

#[derive(Deserialize)]
struct SpaceArcItem {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    aid: i64,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pic: String,
    #[serde(default)]
    created: i64,
    /// Some arc responses carry `pubdate` next to `created`; either is the
    /// publish stamp in Unix seconds.
    #[serde(default)]
    pubdate: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    length: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    play: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    video_review: u64,
}

/// `x/web-interface/card?mid=`: `data.card` header plus `data.archive_count`.
#[derive(Deserialize)]
struct UpCardResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<UpCardData>,
}

#[derive(Deserialize, Default)]
struct UpCardData {
    #[serde(default)]
    card: UpCardInfo,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    archive_count: u64,
}

#[derive(Deserialize, Default)]
struct UpCardInfo {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    face: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    fans: u64,
}

#[derive(Deserialize)]
struct RelationStateResp {
    #[serde(default)]
    code: i64,
    data: Option<RelationStateData>,
}
#[derive(Deserialize, Default)]
struct RelationStateData {
    #[serde(default)]
    attribute: i64,
}

#[derive(Deserialize)]
struct ArchiveResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<ArchiveData>,
}

#[derive(Deserialize, Default)]
struct ArchiveData {
    #[serde(default)]
    archives: Vec<CardItem>,
    #[serde(default)]
    page: Option<ArchivePageInfo>,
}

#[derive(Deserialize, Default)]
struct ArchivePageInfo {
    #[serde(default)]
    num: i64,
    #[serde(default)]
    size: i64,
}

#[derive(Deserialize)]
struct RankingResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<RankingData>,
}

#[derive(Deserialize, Default)]
struct RankingData {
    #[serde(default)]
    list: Vec<CardItem>,
}

#[derive(Deserialize)]
struct CardItem {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pic: String,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    owner: Option<CardOwner>,
    #[serde(default)]
    stat: Option<CardStat>,
}

#[derive(Deserialize)]
struct CardOwner {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct CardStat {
    #[serde(default)]
    view: u64,
}

#[derive(Deserialize)]
struct SuggestResp {
    #[serde(default)]
    result: Option<SuggestResult>,
}

#[derive(Deserialize, Default)]
struct SuggestResult {
    #[serde(default)]
    tag: Vec<SuggestTag>,
}

#[derive(Deserialize, Default)]
struct SuggestTag {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

#[derive(Deserialize)]
struct SquareResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<SquareData>,
}

#[derive(Deserialize, Default)]
struct SquareData {
    #[serde(default)]
    trending: SquareTrending,
}

#[derive(Deserialize, Default)]
struct SquareTrending {
    #[serde(default)]
    list: Vec<SquareItem>,
}

#[derive(Deserialize, Default)]
struct SquareItem {
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    show_name: String,
}

#[derive(Deserialize)]
struct HotwordResp {
    #[serde(default)]
    list: Vec<HotwordItem>,
}

#[derive(Deserialize, Default)]
struct HotwordItem {
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    show_name: String,
}

// ---------------------------------------------------------------------------
// 动态 / 消息 / 私信 / 关注粉丝 types
// ---------------------------------------------------------------------------

/// One dynamic-feed item after flattening its `modules`.
#[derive(Clone, Debug, Default)]
pub struct DynamicItem {
    pub id_str: String,
    pub author_name: String,
    pub author_face: String,
    pub pub_time: String,
    pub text: String,
    pub archive_title: String,
    pub archive_cover: String,
    pub archive_meta: String,
    pub images: Vec<String>,
    pub like_count: u64,
    pub liked: bool,
    pub comment_count: u64,
    pub forward_count: u64,
}

/// One page of the dynamic feed plus its pagination cursor.
pub struct DynamicPage {
    pub offset: String,
    pub has_more: bool,
    pub items: Vec<DynamicItem>,
}

/// Which message list to fetch.
#[derive(Clone, Copy)]
pub enum MsgKind {
    Reply,
    At,
    Like,
}

/// Per-category unread counts behind the 消息 badge.
#[derive(Clone, Copy, Debug, Default)]
pub struct MsgUnread {
    pub total: i64,
    pub reply: i64,
    pub at: i64,
    pub like: i64,
}

/// One 回复/@/赞 message entry.
#[derive(Clone, Debug, Default)]
pub struct MsgItem {
    pub nickname: String,
    pub avatar: String,
    pub title: String,
    pub detail: String,
    pub time: i64,
    pub count: i64,
}

/// One page of a message list.
pub struct MsgPage {
    pub items: Vec<MsgItem>,
}

/// One private-message session (READ ONLY).
#[derive(Clone, Debug, Default)]
pub struct DmSession {
    pub talker_id: i64,
    pub at_time: i64,
    pub unread: i64,
    pub preview: String,
}

/// One private-message entry (READ ONLY).
#[derive(Clone, Debug, Default)]
pub struct DmMessage {
    pub sender_uid: i64,
    pub text: String,
    pub timestamp: i64,
}

/// One 关注 / 粉丝 user.
#[derive(Clone, Debug, Default)]
pub struct RelationUser {
    pub mid: i64,
    pub uname: String,
    pub face: String,
    pub sign: String,
    pub official: String,
}

/// One page of a 关注 / 粉丝 list.
pub struct RelationPage {
    pub total: i64,
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<RelationUser>,
}

#[derive(Deserialize)]
struct DynamicFeedResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<DynamicFeedData>,
}

#[derive(Deserialize, Default)]
struct DynamicFeedData {
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    offset: String,
    #[serde(default)]
    items: Vec<DynamicRawItem>,
}

#[derive(Deserialize, Default)]
struct DynamicRawItem {
    #[serde(default)]
    id_str: String,
    #[serde(default)]
    modules: Option<DynamicModules>,
}

#[derive(Deserialize, Default)]
struct DynamicModules {
    #[serde(default)]
    module_author: Option<DynamicAuthor>,
    #[serde(default)]
    module_dynamic: Option<DynamicContent>,
    #[serde(default)]
    module_stat: Option<DynamicStat>,
}

#[derive(Deserialize, Default)]
struct DynamicAuthor {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    face: String,
    #[serde(default)]
    pub_time: String,
}

#[derive(Deserialize, Default)]
struct DynamicContent {
    #[serde(default)]
    desc: Option<DynamicDesc>,
    #[serde(default)]
    major: Option<DynamicMajor>,
}

#[derive(Deserialize, Default)]
struct DynamicDesc {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct DynamicMajor {
    #[serde(default)]
    archive: Option<DynamicArchiveRaw>,
    #[serde(default)]
    draw: Option<DynamicDraw>,
}

#[derive(Deserialize, Default)]
struct DynamicArchiveRaw {
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default)]
    duration_text: String,
    #[serde(default)]
    stat: Option<DynamicArchiveStat>,
}

#[derive(Deserialize, Default)]
struct DynamicArchiveStat {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    play: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    danmaku: u64,
}

#[derive(Deserialize, Default)]
struct DynamicDraw {
    #[serde(default)]
    items: Vec<DynamicDrawItem>,
}

#[derive(Deserialize, Default)]
struct DynamicDrawItem {
    #[serde(default, deserialize_with = "de_string_lenient")]
    src: String,
}

#[derive(Deserialize, Default)]
struct DynamicStat {
    #[serde(default)]
    like: Option<DynamicCount>,
    #[serde(default)]
    comment: Option<DynamicCount>,
    #[serde(default)]
    forward: Option<DynamicCount>,
}

#[derive(Deserialize, Default)]
struct DynamicCount {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    count: u64,
    #[serde(default)]
    status: bool,
}

#[derive(Deserialize)]
struct MsgUnreadResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct MsgFeedResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<MsgFeedData>,
}

#[derive(Deserialize, Default)]
struct MsgFeedData {
    #[serde(default)]
    items: Vec<MsgFeedItem>,
}

#[derive(Deserialize, Default)]
struct MsgFeedItem {
    #[serde(default)]
    user: Option<MsgUser>,
    #[serde(default)]
    item: Option<serde_json::Value>,
    #[serde(default)]
    counts: i64,
    #[serde(default)]
    reply_time: i64,
}

#[derive(Deserialize, Default)]
struct MsgUser {
    #[serde(default)]
    nickname: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    avatar: String,
}

#[derive(Deserialize)]
struct DmSessionsResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<DmSessionsData>,
}

#[derive(Deserialize, Default)]
struct DmSessionsData {
    #[serde(default)]
    session_list: Vec<DmSessionRaw>,
}

#[derive(Deserialize, Default)]
struct DmSessionRaw {
    #[serde(default)]
    talker_id: i64,
    #[serde(default)]
    at_time: i64,
    #[serde(default)]
    unread_count: i64,
    #[serde(default)]
    last_msg: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct DmMessagesResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<DmMessagesData>,
}

#[derive(Deserialize, Default)]
struct DmMessagesData {
    #[serde(default)]
    messages: Vec<DmMessageRaw>,
}

#[derive(Deserialize, Default)]
struct DmMessageRaw {
    #[serde(default)]
    sender_uid: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    content: String,
    #[serde(default)]
    timestamp: i64,
}

#[derive(Deserialize)]
struct RelationListResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<RelationListData>,
}

#[derive(Deserialize, Default)]
struct RelationListData {
    #[serde(default)]
    list: Vec<RelationUserRaw>,
    #[serde(default)]
    total: i64,
}

#[derive(Deserialize, Default)]
struct RelationUserRaw {
    #[serde(default)]
    mid: i64,
    #[serde(default)]
    uname: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    face: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    sign: String,
    #[serde(default)]
    official_verify: Option<OfficialVerify>,
}

// -- Video comments (wire shapes) -------------------------------------------

#[derive(Deserialize, Default)]
struct CommentResp {
    #[serde(default)]
    code: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    data: Option<CommentData>,
}

#[derive(Deserialize, Default)]
struct CommentData {
    #[serde(default)]
    cursor: Option<CommentCursor>,
    /// `null` for a video without comments.
    #[serde(default)]
    replies: Option<Vec<CommentRaw>>,
    /// The pinned comment, when the video has one.
    #[serde(default)]
    top: Option<CommentRaw>,
    /// Some responses carry the pinned comment(s) here instead of `top`.
    #[serde(default)]
    top_replies: Option<Vec<CommentRaw>>,
}

#[derive(Deserialize, Default)]
struct CommentCursor {
    #[serde(default)]
    is_end: bool,
    #[serde(default)]
    all_count: i64,
    #[serde(default)]
    pagination_reply: Option<CommentPagination>,
}

#[derive(Deserialize, Default)]
struct CommentPagination {
    #[serde(default, deserialize_with = "de_string_lenient")]
    next_offset: String,
}

#[derive(Deserialize, Default)]
struct CommentRaw {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    rpid: i64,
    #[serde(default)]
    content: Option<CommentContentRaw>,
    #[serde(default)]
    member: Option<CommentMemberRaw>,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    like: u64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    ctime: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    rcount: i64,
    /// A few preview replies, inlined by the list endpoint.
    #[serde(default)]
    replies: Option<Vec<CommentRaw>>,
}

#[derive(Deserialize, Default)]
struct CommentContentRaw {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize, Default)]
struct CommentMemberRaw {
    /// The API sends this as a string (`"12345"`).
    #[serde(default, deserialize_with = "de_string_lenient")]
    mid: String,
    #[serde(default)]
    uname: String,
    #[serde(default)]
    avatar: String,
}

#[derive(Deserialize, Default)]
struct CommentReplyResp {
    #[serde(default)]
    code: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    data: Option<CommentReplyData>,
}

#[derive(Deserialize, Default)]
struct CommentReplyData {
    #[serde(default)]
    page: Option<CommentReplyPageInfo>,
    #[serde(default)]
    replies: Option<Vec<CommentRaw>>,
}

#[derive(Deserialize, Default)]
struct CommentReplyPageInfo {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    num: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    size: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    count: i64,
}

fn dynamic_from(raw: DynamicRawItem) -> DynamicItem {
    let modules = raw.modules.unwrap_or_default();
    let author = modules.module_author.unwrap_or_default();
    let content = modules.module_dynamic.unwrap_or_default();
    let stat = modules.module_stat.unwrap_or_default();
    let text = content.desc.map(|desc| desc.text).unwrap_or_default();
    let major = content.major.unwrap_or_default();

    let (archive_title, archive_cover, archive_meta) = match major.archive {
        Some(archive) => {
            let (play, danmaku) = match archive.stat {
                Some(stat) => (stat.play, stat.danmaku),
                None => (0, 0),
            };
            let meta = format!(
                "{}播放 · {}弹幕 · {}",
                short_num(play),
                short_num(danmaku),
                archive.duration_text
            );
            (archive.title, normalize_url(&archive.cover), meta)
        }
        None => (String::new(), String::new(), String::new()),
    };

    let images: Vec<String> = major
        .draw
        .map(|draw| {
            draw.items
                .into_iter()
                .map(|item| normalize_url(&item.src))
                .collect()
        })
        .unwrap_or_default();

    let like = stat.like.unwrap_or_default();

    DynamicItem {
        id_str: raw.id_str,
        author_name: author.name,
        author_face: normalize_url(&author.face),
        pub_time: author.pub_time,
        text,
        archive_title,
        archive_cover,
        archive_meta,
        images,
        like_count: like.count,
        liked: like.status,
        comment_count: stat.comment.map(|comment| comment.count).unwrap_or(0),
        forward_count: stat.forward.map(|forward| forward.count).unwrap_or(0),
    }
}

fn msg_item_from(raw: MsgFeedItem) -> MsgItem {
    let user = raw.user.unwrap_or_default();
    let (title, detail) = raw.item.as_ref().map(msg_item_texts).unwrap_or_default();
    MsgItem {
        nickname: user.nickname,
        avatar: normalize_url(&user.avatar),
        title,
        detail,
        time: raw.reply_time,
        count: raw.counts,
    }
}

fn msg_item_texts(item: &serde_json::Value) -> (String, String) {
    if let Some(text) = item.as_str() {
        return (String::new(), text.to_string());
    }
    let pick = |key: &str| {
        item.get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut title = pick("title");
    if title.is_empty() {
        title = pick("source_content");
    }
    if title.is_empty() {
        title = pick("name");
    }
    let mut detail = pick("desc");
    if detail.is_empty() {
        detail = pick("source_content");
    }
    if detail.is_empty() {
        detail = pick("title");
    }
    (title, detail)
}

fn dm_session_from(raw: DmSessionRaw) -> DmSession {
    let preview = raw
        .last_msg
        .as_ref()
        .and_then(dm_content_text)
        .unwrap_or_default();
    DmSession {
        talker_id: raw.talker_id,
        at_time: raw.at_time,
        unread: raw.unread_count,
        preview,
    }
}

fn dm_content_text(value: &serde_json::Value) -> Option<String> {
    let content = value.get("content")?.as_str()?;
    Some(content_text(content))
}

fn content_text(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value
            .get("text")
            .and_then(|text| text.as_str())
            .or_else(|| value.get("title").and_then(|title| title.as_str()))
            .unwrap_or("")
            .to_string(),
        Err(_) => raw.to_string(),
    }
}

fn dm_message_from(raw: DmMessageRaw) -> DmMessage {
    DmMessage {
        sender_uid: raw.sender_uid,
        text: content_text(&raw.content),
        timestamp: raw.timestamp,
    }
}

fn relation_user_from(raw: RelationUserRaw) -> RelationUser {
    RelationUser {
        mid: raw.mid,
        uname: raw.uname,
        face: normalize_url(&raw.face),
        sign: raw.sign,
        official: raw
            .official_verify
            .map(|verify| verify.desc)
            .unwrap_or_default(),
    }
}

/// Fold the `folder/created/list-all` payload into the 收藏推荐 folder list:
/// the default folder (`id == 0` / [`DEFAULT_FAV_FOLDER`]) and empty folders are
/// dropped, so the stream never picks from either.
fn fav_folders_from(data: Option<FavFoldersData>) -> Vec<FavFolder> {
    let Some(data) = data else {
        return Vec::new();
    };
    data.list
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.id > 0)
        .filter(|item| item.media_count > 0)
        .filter(|item| item.title.trim() != DEFAULT_FAV_FOLDER)
        .map(|item| FavFolder {
            id: item.id,
            name: item.title,
            count: item.media_count,
        })
        .collect()
}

/// Fold one `fav/resource/list` page into feed items. `null` entries (deleted
/// videos) and entries without a `bvid` are skipped; `has_more` is the API's own
/// flag.
fn fav_page_from(data: Option<FavListData>) -> (Vec<FeedVideo>, bool) {
    let Some(data) = data else {
        return (Vec::new(), false);
    };
    let has_more = data.has_more;
    let items = data
        .medias
        .into_iter()
        .flatten()
        .filter(|media| !media.bvid.trim().is_empty())
        .map(|media| {
            let owner_mid = media.upper.as_ref().map(|upper| upper.mid).unwrap_or(0);
            FeedVideo {
                bvid: media.bvid,
                cid: media.ugc.map(|ugc| ugc.first_cid).unwrap_or(0),
                aid: media.id.max(0) as u64,
                title: media.title,
                pic: normalize_url(&media.cover),
                owner: media.upper.map(|upper| upper.name).unwrap_or_default(),
                owner_mid,
                view: media.cnt_info.map(|info| info.play).unwrap_or(0),
                duration: media.duration,
                pub_ts: media.pubtime,
            }
        })
        .collect();
    (items, has_more)
}

/// Fold one `archive/related` payload into playable feed items. Ads are entries
/// without a `bvid` or with a `goto` other than `av`; the live payload omits
/// `goto` on normal videos, so a missing/empty `goto` is a video, not an ad.
fn related_from(data: Vec<Option<RelatedItem>>) -> Vec<FeedVideo> {
    data.into_iter()
        .flatten()
        .filter(|item| !item.bvid.trim().is_empty())
        .filter(|item| match item.goto.as_deref() {
            None | Some("") | Some("av") => true,
            Some(_) => false,
        })
        .map(|item| {
            let (owner, owner_mid) = match item.owner {
                Some(owner) => (owner.name, owner.mid),
                None => (String::new(), 0),
            };
            FeedVideo {
                bvid: item.bvid,
                cid: item.cid,
                aid: item.aid.max(0) as u64,
                title: item.title,
                pic: normalize_url(&item.pic),
                owner,
                owner_mid,
                view: item.stat.map(|stat| stat.view).unwrap_or(0),
                duration: item.duration,
                pub_ts: item.pubdate,
            }
        })
        .collect()
}

/// `<label>（code=<code>）` with the API message appended when there is one.
fn fav_error(label: &str, code: i64, message: &str) -> anyhow::Error {
    let message = message.trim();
    if message.is_empty() {
        anyhow!("{label}（code={code}）")
    } else {
        anyhow!("{label}：{message}（code={code}）")
    }
}

/// Convert one wire comment (and its inlined preview replies) to a [`Comment`].
fn comment_from(raw: CommentRaw) -> Comment {
    let member = raw.member.unwrap_or_default();
    let mid = member.mid.trim().parse::<i64>().unwrap_or(0);
    let replies = raw
        .replies
        .unwrap_or_default()
        .into_iter()
        .map(comment_from)
        .collect();
    Comment {
        rpid: raw.rpid,
        user: CommentUser {
            mid,
            uname: member.uname,
            avatar: normalize_url(&member.avatar),
        },
        message: raw.content.map(|content| content.message).unwrap_or_default(),
        like: raw.like,
        ctime: raw.ctime,
        rcount: raw.rcount,
        replies,
    }
}

/// Fold the wire payload of the comment list into a [`CommentPage`].
fn comment_page_from(data: CommentData) -> CommentPage {
    let cursor = data.cursor.as_ref();
    let top = data
        .top
        .or_else(|| data.top_replies.unwrap_or_default().into_iter().next())
        .map(comment_from);
    let comments = data
        .replies
        .unwrap_or_default()
        .into_iter()
        .map(comment_from)
        .collect();
    CommentPage {
        top,
        comments,
        cursor: cursor
            .and_then(|cursor| cursor.pagination_reply.as_ref())
            .map(|reply| reply.next_offset.clone())
            .unwrap_or_default(),
        // A missing cursor block means the endpoint answered without paging
        // state; treat that as the end so the caller never loops.
        is_end: cursor.map(|cursor| cursor.is_end).unwrap_or(true),
        total: cursor.map(|cursor| cursor.all_count).unwrap_or(0),
    }
}

/// Fold the wire payload of a comment's dialog into a [`CommentReplyPage`].
fn comment_reply_page_from(data: CommentReplyData, pn: i64) -> CommentReplyPage {
    let replies = data
        .replies
        .unwrap_or_default()
        .into_iter()
        .map(comment_from)
        .collect();
    let page = data.page.unwrap_or_default();
    let page_num = if page.num > 0 { page.num } else { pn };
    let size = page.size.max(1);
    let total = page.count.max(0);
    CommentReplyPage {
        replies,
        total,
        page: page_num,
        size,
        has_more: total > 0 && page_num * size < total,
    }
}

fn short_num(value: u64) -> String {
    if value >= 100_000_000 {
        format!("{:.1}亿", value as f64 / 100_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}万", value as f64 / 10_000.0)
    } else {
        value.to_string()
    }
}

/// Seconds from an arc-list `length` string: `"mm:ss"`, `"h:mm:ss"` or the
/// plain seconds some entries carry. Unparseable input is `0`, never a panic.
fn length_seconds(length: &str) -> u64 {
    let text = length.trim();
    if text.is_empty() {
        return 0;
    }
    let mut total: u64 = 0;
    let mut parts = 0usize;
    for part in text.split(':') {
        let Ok(value) = part.trim().parse::<u64>() else {
            return 0;
        };
        total = total.saturating_mul(60).saturating_add(value);
        parts += 1;
    }
    if parts == 0 {
        0
    } else {
        total
    }
}

fn card_from(item: CardItem) -> VideoCard {
    VideoCard {
        bvid: item.bvid,
        title: item.title,
        pic: normalize_url(&item.pic),
        owner: item.owner.map(|owner| owner.name).unwrap_or_default(),
        view: item.stat.as_ref().map(|stat| stat.view).unwrap_or(0),
        duration: item.duration,
    }
}

fn square_to_hot(item: SquareItem) -> HotSearch {
    let keyword = if item.keyword.is_empty() {
        item.show_name.clone()
    } else {
        item.keyword
    };
    let show_name = if item.show_name.is_empty() {
        keyword.clone()
    } else {
        item.show_name
    };
    HotSearch {
        keyword,
        show_name,
    }
}

fn hotword_to_hot(item: HotwordItem) -> HotSearch {
    let keyword = if item.keyword.is_empty() {
        item.show_name.clone()
    } else {
        item.keyword
    };
    let show_name = if item.show_name.is_empty() {
        keyword.clone()
    } else {
        item.show_name
    };
    HotSearch {
        keyword,
        show_name,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Current year and month, used as the first `month` for `history/index`.
fn current_year_month() -> (i64, u32) {
    let secs = now_secs() as i64;
    let (year, month, _) = crate::civil_from_days(secs.div_euclid(86_400));
    (year, month as u32)
}

/// Strip the `<em class="keyword">…</em>` highlight markup search results carry.
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Bilibili image URLs may be protocol-relative (`//i0.hdslb.com/...`).
fn normalize_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else if url.is_empty() {
        String::new()
    } else if url.starts_with("http") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FlexNum {
    Int(u64),
    Float(f64),
    Text(String),
}

fn de_u64_flexible<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match FlexNum::deserialize(deserializer)? {
        FlexNum::Int(value) => value,
        FlexNum::Float(value) => value.max(0.0) as u64,
        FlexNum::Text(value) => value.trim().parse::<u64>().unwrap_or(0),
    })
}

/// Like [`de_u64_flexible`] but for signed counters that arrive as a number or
/// a string (`rpid`/`ctime`/`rcount` and the reply pager).
fn de_i64_flexible<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match FlexNum::deserialize(deserializer)? {
        FlexNum::Int(value) => value.min(i64::MAX as u64) as i64,
        FlexNum::Float(value) => value.max(0.0) as i64,
        FlexNum::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
    })
}

fn de_duration_flexible<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match FlexNum::deserialize(deserializer)? {
        FlexNum::Text(value) => value,
        FlexNum::Int(secs) => format!("{:02}:{:02}", secs / 60, secs % 60),
        FlexNum::Float(secs) => {
            let secs = secs.max(0.0) as u64;
            format!("{:02}:{:02}", secs / 60, secs % 60)
        }
    })
}

fn de_string_lenient<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Bilibili's booleans flip between `true`/`false` and `1`/`0` across
/// endpoints (and even between payload versions of the same endpoint), so a
/// flag field accepts both.
fn de_flag<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Bool(flag) => flag,
        serde_json::Value::Number(number) => number.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(text) => {
            let text = text.trim();
            text == "1" || text.eq_ignore_ascii_case("true")
        }
        _ => false,
    })
}

// -- 番剧 / PGC payloads -----------------------------------------------------

#[derive(Deserialize)]
struct PgcTimelineResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    result: Vec<PgcTimelineDayRaw>,
}

#[derive(Deserialize, Default)]
struct PgcTimelineDayRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    date: String,
    #[serde(default)]
    day_of_week: i64,
    #[serde(default, deserialize_with = "de_flag")]
    is_today: bool,
    #[serde(default)]
    episodes: Vec<PgcTimelineEpisodeRaw>,
}

#[derive(Deserialize, Default)]
struct PgcTimelineEpisodeRaw {
    #[serde(default)]
    season_id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub_index: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub_time: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    follows: String,
}

#[derive(Deserialize)]
struct PgcIndexResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<PgcIndexDataRaw>,
}

#[derive(Deserialize, Default)]
struct PgcIndexDataRaw {
    #[serde(default)]
    total: i64,
    #[serde(default)]
    num: i64,
    #[serde(default, deserialize_with = "de_flag")]
    has_next: bool,
    #[serde(default)]
    list: Vec<PgcIndexItemRaw>,
}

#[derive(Deserialize, Default)]
struct PgcIndexItemRaw {
    #[serde(default)]
    season_id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    index_show: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    order: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    badge: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    score: String,
}

#[derive(Deserialize)]
struct PgcRankResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    result: Option<PgcRankResultRaw>,
}

#[derive(Deserialize, Default)]
struct PgcRankResultRaw {
    #[serde(default)]
    list: Vec<PgcRankItemRaw>,
}

#[derive(Deserialize, Default)]
struct PgcRankItemRaw {
    #[serde(default)]
    rank: i64,
    #[serde(default)]
    season_id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    rating: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    badge: String,
    #[serde(default)]
    new_ep: Option<PgcNewEpRaw>,
    #[serde(default)]
    stat: Option<PgcRankStatRaw>,
}

#[derive(Deserialize, Default)]
struct PgcRankStatRaw {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    follow: u64,
}

#[derive(Deserialize, Default)]
struct PgcNewEpRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    index_show: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
}

#[derive(Deserialize)]
struct PgcSeasonResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    result: Option<PgcSeasonRaw>,
}

#[derive(Deserialize, Default)]
struct PgcSeasonRaw {
    #[serde(default)]
    season_id: i64,
    #[serde(default)]
    media_id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    cover: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    evaluate: String,
    #[serde(default)]
    rating: Option<PgcRatingRaw>,
    #[serde(default)]
    areas: Vec<PgcNameRaw>,
    #[serde(default)]
    styles: Vec<String>,
    #[serde(default)]
    publish: Option<PgcPublishRaw>,
    #[serde(default)]
    new_ep: Option<PgcNewEpRaw>,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    episodes: Vec<PgcEpisodeRaw>,
    #[serde(default)]
    section: Vec<PgcSectionRaw>,
}

#[derive(Deserialize, Default)]
struct PgcRatingRaw {
    #[serde(default)]
    score: f64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    count: u64,
}

#[derive(Deserialize, Default)]
struct PgcNameRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    name: String,
}

#[derive(Deserialize, Default)]
struct PgcPublishRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub_time_show: String,
    #[serde(default, deserialize_with = "de_flag")]
    is_finish: bool,
}

#[derive(Deserialize, Default)]
struct PgcEpisodeRaw {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    ep_id: i64,
    #[serde(default)]
    aid: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    bvid: String,
    #[serde(default)]
    cid: u64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    long_title: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    duration: u64,
    #[serde(default)]
    pub_time: i64,
}

#[derive(Deserialize, Default)]
struct PgcSectionRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default)]
    episodes: Vec<PgcEpisodeRaw>,
}

#[derive(Deserialize)]
struct PgcGateResp {
    #[serde(default)]
    code: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
}

fn timeline_day_from(raw: PgcTimelineDayRaw) -> PgcTimelineDay {
    let day_of_week = if raw.day_of_week <= 0 {
        7
    } else {
        raw.day_of_week
    };
    PgcTimelineDay {
        date: raw.date,
        day_of_week,
        is_today: raw.is_today,
        episodes: raw
            .episodes
            .into_iter()
            .filter(|episode| episode.season_id > 0)
            .map(|episode| PgcTimelineEpisode {
                season_id: episode.season_id,
                title: episode.title,
                pub_index: episode.pub_index,
                pub_time: episode.pub_time,
                cover: normalize_url(&episode.cover),
                follows: if episode.follows.trim() == "-" {
                    String::new()
                } else {
                    episode.follows
                },
            })
            .collect(),
    }
}

fn index_item_from(raw: PgcIndexItemRaw) -> PgcIndexItem {
    PgcIndexItem {
        season_id: raw.season_id,
        title: raw.title,
        cover: normalize_url(&raw.cover),
        index_show: raw.index_show,
        order: raw.order,
        badge: raw.badge,
        score: raw.score,
    }
}

fn rank_item_from(raw: PgcRankItemRaw) -> PgcRankItem {
    PgcRankItem {
        rank: raw.rank,
        season_id: raw.season_id,
        title: raw.title,
        cover: normalize_url(&raw.cover),
        rating: raw.rating,
        badge: raw.badge,
        index_show: raw
            .new_ep
            .map(|episode| episode.index_show)
            .unwrap_or_default(),
        follow: raw.stat.map(|stat| stat.follow).unwrap_or(0),
    }
}

fn episode_from(raw: PgcEpisodeRaw) -> PgcEpisode {
    let ep_id = if raw.ep_id > 0 { raw.ep_id } else { raw.id };
    PgcEpisode {
        ep_id,
        aid: raw.aid,
        bvid: raw.bvid,
        cid: raw.cid,
        title: raw.title,
        long_title: raw.long_title,
        duration_ms: raw.duration,
        pub_time: raw.pub_time,
    }
}

fn season_from(raw: PgcSeasonRaw) -> PgcSeason {
    let is_finish = raw.publish.as_ref().map(|p| p.is_finish).unwrap_or(false);
    let pub_time = raw
        .publish
        .map(|publish| publish.pub_time_show)
        .unwrap_or_default();
    let (new_index, new_title) = raw
        .new_ep
        .map(|episode| (episode.index_show, episode.title))
        .unwrap_or_default();
    let index_show = if !new_index.trim().is_empty() {
        new_index
    } else if is_finish && raw.total > 0 {
        format!("全{}话", raw.total)
    } else if !new_title.trim().is_empty() {
        format!("更新至第{}话", new_title.trim())
    } else {
        String::new()
    };
    PgcSeason {
        season_id: if raw.season_id > 0 {
            raw.season_id
        } else {
            raw.media_id
        },
        title: raw.title,
        cover: normalize_url(&raw.cover),
        evaluate: raw.evaluate,
        score: raw.rating.as_ref().map(|rating| rating.score).unwrap_or(0.0),
        score_count: raw.rating.map(|rating| rating.count).unwrap_or(0),
        areas: raw
            .areas
            .into_iter()
            .map(|area| area.name)
            .filter(|name| !name.trim().is_empty())
            .collect(),
        styles: raw
            .styles
            .into_iter()
            .map(|style| style.trim().to_string())
            .filter(|style| !style.is_empty())
            .collect(),
        pub_time,
        is_finish,
        index_show,
        episodes: raw.episodes.into_iter().map(episode_from).collect(),
        sections: raw
            .section
            .into_iter()
            .map(|section| PgcSection {
                title: section.title,
                episodes: section.episodes.into_iter().map(episode_from).collect(),
            })
            .collect(),
    }
}

// -- 合集 / 视频列表 (ugc_season) --------------------------------------------
//
// `video_view` keeps `data.ugc_season` as raw JSON and this parser decides what
// is usable. The shape the site returns is
//
//   data.ugc_season = { id, title, cover, mid, intro, ep_count, season_type,
//     sections: [ { id, title, episodes: [ { aid, bvid, cid, title,
//       attribute, arc: { duration, pic, title, stat },
//       page: { cid, part, duration }, pages: [...] } ] } ] }
//
// Every field is optional in practice (episodes of old seasons have no `page`,
// some accounts get no `arc`, counters arrive as strings), so each value is
// read through the helpers below: a missing/negative/non-string scalar becomes
// `0`/empty instead of failing the response.

/// `0` for missing, negative or non-numeric numbers; the payload may send the
/// same id as a string.
fn season_i64(value: Option<&serde_json::Value>) -> i64 {
    match value {
        Some(serde_json::Value::Number(number)) => {
            number.as_i64().unwrap_or(0).max(0)
        }
        Some(serde_json::Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0).max(0),
        _ => 0,
    }
}

/// `None` when the field is absent (so "fall through to the next source" is
/// distinguishable from a real `0`), `Some(0)` for a present zero.
fn season_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    match value {
        Some(serde_json::Value::Number(number)) => {
            let secs = number.as_f64().unwrap_or(0.0);
            Some(if secs > 0.0 { secs.round() as u64 } else { 0 })
        }
        Some(serde_json::Value::String(text)) => {
            Some(text.trim().parse::<f64>().unwrap_or(0.0).max(0.0).round() as u64)
        }
        _ => None,
    }
}

/// Only a string counts; anything else is empty (never `"null"`, never a
/// serialised number).
fn season_str(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn season_obj(value: Option<&serde_json::Value>) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value.and_then(serde_json::Value::as_object)
}

/// Parse `data.ugc_season` into a [`Season`].
///
/// Returns `None` when the value is not an object, carries no section array or
/// no usable episode at all — the player then treats the video as "no list".
/// Sections are capped at [`SEASON_MAX_SECTIONS`], episodes at
/// [`SEASON_MAX_EPISODES`] (truncation is logged, never silent).
fn parse_season(raw: &serde_json::Value) -> Option<Season> {
    let obj = raw.as_object()?;
    let sections_raw = obj.get("sections").and_then(serde_json::Value::as_array)?;
    if sections_raw.is_empty() {
        return None;
    }
    if sections_raw.len() > SEASON_MAX_SECTIONS {
        crate::debug_log(&format!(
            "season truncated sections={} kept={SEASON_MAX_SECTIONS}",
            sections_raw.len()
        ));
    }
    let mut total = 0usize;
    let mut sections: Vec<SeasonSection> = Vec::new();
    for raw_section in sections_raw.iter().take(SEASON_MAX_SECTIONS) {
        let Some(section_obj) = raw_section.as_object() else {
            continue;
        };
        let episodes_raw = section_obj
            .get("episodes")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut episodes: Vec<SeasonEpisode> = Vec::new();
        for raw_episode in episodes_raw {
            if total >= SEASON_MAX_EPISODES {
                break;
            }
            let Some(episode) = parse_season_episode(raw_episode) else {
                continue;
            };
            total += 1;
            episodes.push(episode);
        }
        if episodes.is_empty() {
            continue;
        }
        sections.push(SeasonSection {
            title: season_str(section_obj.get("title")),
            episodes,
        });
    }
    if sections.is_empty() {
        return None;
    }
    let listed: usize = sections_raw
        .iter()
        .take(SEASON_MAX_SECTIONS)
        .filter_map(|section| {
            section
                .get("episodes")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
        })
        .sum();
    if listed > total {
        crate::debug_log(&format!(
            "season truncated episodes={listed} kept={total} max={SEASON_MAX_EPISODES}"
        ));
    }
    Some(Season {
        title: season_str(obj.get("title")),
        ep_count: season_i64(obj.get("ep_count")).min(u32::MAX as i64) as u32,
        sections,
    })
}

/// One episode. `cid` falls back from `page.cid` to the episode's own `cid`,
/// `duration` from `page.duration` to `arc.duration`. An episode without any
/// `cid` is unusable (the player could not switch to it), so it is dropped.
fn parse_season_episode(raw: &serde_json::Value) -> Option<SeasonEpisode> {
    let obj = raw.as_object()?;
    let page = season_obj(obj.get("page"));
    let arc = season_obj(obj.get("arc"));
    let cid = page
        .and_then(|page| page.get("cid"))
        .map(|value| season_i64(Some(value)))
        .filter(|cid| *cid > 0)
        .unwrap_or_else(|| season_i64(obj.get("cid")));
    if cid <= 0 {
        return None;
    }
    let duration = page
        .and_then(|page| season_u64(page.get("duration")))
        .or_else(|| arc.and_then(|arc| season_u64(arc.get("duration"))))
        .unwrap_or(0);
    let title = {
        let own = season_str(obj.get("title"));
        if own.is_empty() {
            let arc_title = arc
                .map(|arc| season_str(arc.get("title")))
                .unwrap_or_default();
            if arc_title.is_empty() {
                let part = page
                    .map(|page| season_str(page.get("part")))
                    .unwrap_or_default();
                if part.is_empty() {
                    crate::debug_log("season episode without title");
                }
                part
            } else {
                arc_title
            }
        } else {
            own
        }
    };
    let aid = {
        let own = season_i64(obj.get("aid"));
        if own > 0 {
            own
        } else {
            arc.map(|arc| season_i64(arc.get("aid"))).unwrap_or(0)
        }
    };
    Some(SeasonEpisode {
        bvid: season_str(obj.get("bvid")),
        aid,
        cid,
        title,
        duration,
    })
}

// ---------------------------------------------------------------------------
// 养鱼: AI 摘要 + 一次性元数据
// ---------------------------------------------------------------------------

/// `x/web-interface/view/conclusion/get`：Bilibili 的 AI 摘要 / 提纲 / 字幕。
const CONCLUSION_URL: &str = "https://api.bilibili.com/x/web-interface/view/conclusion/get";

/// 一条视频的 AI 提纲小节。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutlineSection {
    /// 小节开始时间（秒，取自 `timestamp`）。
    pub timestamp: u64,
    pub title: String,
    /// 小节里的要点，每个点自带时间。
    pub points: Vec<OutlinePoint>,
}

/// AI 提纲里的一个要点。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutlinePoint {
    pub timestamp: u64,
    pub content: String,
}

/// AI 字幕的一小段（`part_subtitle[]`），时间单位是秒。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cue {
    pub start_s: u64,
    pub end_s: u64,
    pub text: String,
}

/// One video's AI conclusion: a summary, an outline and (sometimes) a full
/// transcript.
///
/// `data_code` mirrors the endpoint's own `data.code`:
/// `0` = summary + outline, `1` = no summary, `-1` = this video is not
/// supported. **`-1` is not an error**: the transcript can still be complete
/// (measured: 3 of 13 sampled videos answered `-1` with a full
/// `part_subtitle`). Callers therefore always inspect [`Conclusion::cues`].
#[derive(Clone, Debug, Default)]
pub struct Conclusion {
    pub summary: String,
    pub outline: Vec<OutlineSection>,
    pub cues: Vec<Cue>,
    pub data_code: i32,
}

/// Everything the fish engine needs about one video in a single request.
///
/// The recommendation feed already carries title/owner/duration/view, but not
/// `tname`, `desc` or the `aid` the comment endpoint wants, so the engine asks
/// `x/web-interface/wbi/view` once per video for this shape.
#[derive(Clone, Debug, Default)]
pub struct FishMeta {
    pub aid: i64,
    pub cid: u64,
    pub title: String,
    pub up_mid: u64,
    pub up_name: String,
    pub duration_s: u64,
    pub view: u64,
    /// Unix seconds; `0` when the payload has none.
    pub pub_ts: i64,
    pub cover: String,
    pub tname: String,
    /// HTML already stripped, whitespace collapsed.
    pub desc: String,
}

impl Session {
    /// One video's AI summary / outline / transcript (`conclusion/get`).
    ///
    /// WBI-signed with the session's cookies (SESSDATA is required by the
    /// endpoint) and retried once after refreshing the WBI keys, exactly like
    /// the other signed reads. A non-zero top-level `code` is an error — except
    /// that `data.code == -1` is NOT one, see [`Conclusion`].
    ///
    /// Rate limiting belongs to the caller: the fish engine keeps at least
    /// 1.5 s between calls and backs off on `-352` / `-412`.
    pub async fn conclusion(&self, bvid: &str, cid: u64, up_mid: u64) -> Result<Conclusion> {
        let params = vec![
            ("bvid".to_string(), bvid.trim().to_string()),
            ("cid".to_string(), cid.to_string()),
            ("up_mid".to_string(), up_mid.to_string()),
        ];
        let referer = format!("https://www.bilibili.com/video/{}", bvid.trim());
        let (_, mut resp): (u16, ConclusionResp) = self
            .wbi_call_referer(CONCLUSION_URL, &params, false, Some(&referer))
            .await?;
        if resp.code != 0 {
            resp = self
                .wbi_call_referer(CONCLUSION_URL, &params, true, Some(&referer))
                .await?
                .1;
        }
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("获取 AI 摘要失败（code={}）", resp.code);
            }
            bail!("获取 AI 摘要失败：{message}（code={}）", resp.code);
        }
        let conclusion = conclusion_from(resp.data.unwrap_or_default());
        crate::debug_log(&format!(
            "conclusion bvid={bvid} cid={cid} code={} cues={} outline={}",
            conclusion.data_code,
            conclusion.cues.len(),
            conclusion.outline.len()
        ));
        Ok(conclusion)
    }

    /// Full metadata for one video, shaped for the fish engine.
    pub async fn fish_meta(&self, bvid: &str) -> Result<FishMeta> {
        let params = vec![("bvid".to_string(), bvid.trim().to_string())];
        let mut resp: ViewResp = self.wbi_call(VIEW_URL, &params, false).await?.1;
        if resp.code != 0 {
            resp = self.wbi_call(VIEW_URL, &params, true).await?.1;
        }
        if resp.code != 0 {
            bail!("获取视频信息失败（code={}）", resp.code);
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("获取视频信息失败：响应缺少 data"))?;
        let (up_mid, up_name) = match data.owner {
            Some(owner) => (owner.mid.max(0) as u64, owner.name),
            None => (0, String::new()),
        };
        let desc = strip_html(&data.desc)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Ok(FishMeta {
            aid: data.aid,
            cid: data.cid.max(0) as u64,
            title: data.title,
            up_mid,
            up_name,
            duration_s: data.duration,
            view: data.stat.map(|stat| stat.view).unwrap_or(0),
            pub_ts: data.pubdate,
            cover: normalize_url(&data.pic),
            tname: data.tname,
            desc,
        })
    }
}

/// True when a Bilibili error is risk control (`-352` / `-412`).
///
/// Those two codes mean "you are going too fast" (or the fingerprint/signature
/// was rejected), not "this video has no data", so the fish engine backs off
/// and retries instead of burning the queue entry.
pub fn is_risk_control(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("-352") || text.contains("-412")
}

// -- conclusion payload ------------------------------------------------------

#[derive(Deserialize, Default)]
struct ConclusionResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<ConclusionData>,
}

#[derive(Deserialize, Default)]
struct ConclusionData {
    /// `0` summary, `1` no summary, `-1` unsupported.
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    model_result: Option<ConclusionModel>,
}

#[derive(Deserialize, Default)]
struct ConclusionModel {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    outline: Option<Vec<RawOutline>>,
    /// The web payload sends a one-element array; some responses send the
    /// object directly, so both shapes are accepted.
    #[serde(default)]
    subtitle: Option<SubtitleField>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SubtitleField {
    List(Vec<RawSubtitle>),
    One(RawSubtitle),
}

impl Default for SubtitleField {
    fn default() -> Self {
        SubtitleField::List(Vec::new())
    }
}

#[derive(Deserialize, Default)]
struct RawSubtitle {
    #[serde(default)]
    part_subtitle: Vec<RawCue>,
}

#[derive(Deserialize, Default)]
struct RawCue {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    start_timestamp: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    end_timestamp: u64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    content: String,
}

#[derive(Deserialize, Default)]
struct RawOutline {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    timestamp: u64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    title: String,
    #[serde(default)]
    part_outline: Vec<RawPoint>,
}

#[derive(Deserialize, Default)]
struct RawPoint {
    #[serde(default, deserialize_with = "de_u64_flexible")]
    timestamp: u64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    content: String,
}

/// Turn the endpoint's `data` block into a [`Conclusion`].///
/// Tolerant on purpose: every field is optional, `data.code` is trusted when
/// present (so `-1` survives as "unsupported" instead of becoming an error),
/// and transcript cues are kept even when there is no summary at all.
fn conclusion_from(data: ConclusionData) -> Conclusion {
    let model = data.model_result.unwrap_or_default();
    let summary = model
        .summary
        .unwrap_or_default()
        .trim()
        .to_string();
    let outline = model
        .outline
        .unwrap_or_default()
        .into_iter()
        .map(|section| OutlineSection {
            timestamp: section.timestamp,
            title: section.title.trim().to_string(),
            points: section
                .part_outline
                .into_iter()
                .map(|point| OutlinePoint {
                    timestamp: point.timestamp,
                    content: point.content.trim().to_string(),
                })
                .filter(|point| !point.content.is_empty())
                .collect(),
        })
        .filter(|section| !section.title.is_empty() || !section.points.is_empty())
        .collect::<Vec<_>>();
    let subtitle = model.subtitle.unwrap_or_default();
    let part = match subtitle {
        SubtitleField::List(mut list) => {
            if list.is_empty() {
                RawSubtitle::default()
            } else {
                list.remove(0)
            }
        }
        SubtitleField::One(one) => one,
    };
    let cues = part
        .part_subtitle
        .into_iter()
        .map(|cue| Cue {
            start_s: cue.start_timestamp,
            end_s: cue.end_timestamp,
            text: cue.content.trim().to_string(),
        })
        .filter(|cue| !cue.text.is_empty())
        .collect::<Vec<_>>();
    let data_code = data.code.unwrap_or(if summary.is_empty() { 1 } else { 0 });
    Conclusion {
        summary,
        outline,
        cues,
        data_code,
    }
}

// ---------------------------------------------------------------------------
// 直播 (live rooms)
//
// Three endpoints, all on `api.live.bilibili.com`:
//   * `xlive/web-interface/v1/index/getList`  - homepage aggregate: the
//     decorated module rows + the 24 area entrances (no signature, works).
//   * `room/v1/Room/get_info`                 - one room's status / online /
//     area / title (no signature, works).
//   * `xlive/web-room/v2/index/getRoomPlayInfo` - the playable FLV + HLS URLs.
//   * `room/v1/area/getRoomList`               - a 分区's rooms, the endpoint
//     the area grid uses (measured: `code: 0`, 30 rooms/page, no signature and
//     no cookie needed).
//   * `xlive/web-interface/v1/second/getList` - the newer 分区 list. It answers
//     `-352` (risk control) for this account/IP even when WBI-signed with keys
//     proven good against `wbi/index/top/feed/rcmd` (`code: 0`); kept as the
//     fallback, and its (`code`, `source`, `signed`) triple is reported so a
//     refusal is diagnosed instead of hidden.
// ---------------------------------------------------------------------------

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
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
pub struct LiveModule {
    pub title: String,
    pub rooms: Vec<LiveRoom>,
}

/// One 分区 entrance (`area_entrance_v2`). `id` is the `area_v2_id` the
/// second-level list wants; `parent_id` is the top-level area.
#[derive(Clone, Debug, Default)]
pub struct LiveArea {
    pub id: i64,
    pub parent_id: i64,
    pub title: String,
}

/// The live homepage aggregate, parsed.
#[derive(Clone, Debug, Default)]
pub struct LiveIndex {
    pub modules: Vec<LiveModule>,
    pub areas: Vec<LiveArea>,
}

/// One page of a 分区's rooms. `code` is the API's own code (`0` = ok), `source`
/// the endpoint that answered, `elapsed_ms` the wall time of the whole request
/// (both attempts included). A non-zero code with an empty `rooms` is the
/// documented risk-control case.
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
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
        let params = [
            ("room_id", room_id.to_string()),
            ("protocol", "0,1".to_string()),
            ("format", "0,1,2".to_string()),
            ("codec", "0,1".to_string()),
            ("qn", "10000".to_string()),
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

#[cfg(test)]
mod conclusion_tests {
    use super::{conclusion_from, ConclusionResp};

    fn parse(json: &str) -> super::Conclusion {
        let resp: ConclusionResp = serde_json::from_str(json).expect("fixture parses");
        assert_eq!(resp.code, 0);
        conclusion_from(resp.data.unwrap_or_default())
    }

    /// Captured shape: summary + outline + full transcript.
    #[test]
    fn summary_outline_and_transcript_are_parsed() {
        let json = r#"{
            "code": 0,
            "message": "0",
            "data": {
                "code": 0,
                "model_result": {
                    "summary": "这是一个讲 Rust 所有权的视频。",
                    "outline": [
                        {
                            "timestamp": 0,
                            "title": "所有权是什么",
                            "part_outline": [
                                { "timestamp": 12, "content": "每个值有唯一所有者" },
                                { "timestamp": 48, "content": "离开作用域就释放" }
                            ]
                        },
                        {
                            "timestamp": 120,
                            "title": "借用",
                            "part_outline": []
                        }
                    ],
                    "subtitle": [
                        {
                            "part_subtitle": [
                                { "start_timestamp": 0, "end_timestamp": 5, "content": "大家好" },
                                { "start_timestamp": 5, "end_timestamp": 11, "content": "今天我们聊所有权" },
                                { "start_timestamp": 11, "end_timestamp": 18, "content": "  " }
                            ]
                        }
                    ]
                }
            }
        }"#;
        let conclusion = parse(json);
        assert_eq!(conclusion.data_code, 0);
        assert_eq!(conclusion.summary, "这是一个讲 Rust 所有权的视频。");
        assert_eq!(conclusion.outline.len(), 2);
        assert_eq!(conclusion.outline[0].timestamp, 0);
        assert_eq!(conclusion.outline[0].title, "所有权是什么");
        assert_eq!(conclusion.outline[0].points.len(), 2);
        assert_eq!(conclusion.outline[0].points[1].timestamp, 48);
        assert_eq!(conclusion.outline[1].points.len(), 0);
        assert_eq!(conclusion.cues.len(), 2, "blank cue dropped");
        assert_eq!(conclusion.cues[1].start_s, 5);
        assert_eq!(conclusion.cues[1].end_s, 11);
        assert_eq!(conclusion.cues[1].text, "今天我们聊所有权");
    }

    /// A measured payload: `data.code == -1` (no summary) but a FULL transcript.
    /// This must not become an error and must not lose the cues.
    #[test]
    fn unsupported_code_keeps_the_full_transcript() {
        let json = r#"{
            "code": 0,
            "message": "0",
            "data": {
                "code": -1,
                "allow_summary": false,
                "model_result": {
                    "summary": "",
                    "outline": [],
                    "subtitle": [
                        {
                            "part_subtitle": [
                                { "start_timestamp": 0, "end_timestamp": 3, "content": "第一句" },
                                { "start_timestamp": 3, "end_timestamp": 9, "content": "第二句" },
                                { "start_timestamp": 9, "end_timestamp": 15, "content": "第三句" }
                            ]
                        }
                    ]
                }
            }
        }"#;
        let conclusion = parse(json);
        assert_eq!(conclusion.data_code, -1);
        assert!(conclusion.summary.is_empty());
        assert!(conclusion.outline.is_empty());
        assert_eq!(conclusion.cues.len(), 3);
        assert_eq!(conclusion.cues[2].text, "第三句");
    }

    /// `data.code == 1` with no model_result at all: no summary, no cues, and
    /// the conclusion still reports code 1 rather than failing.
    #[test]
    fn missing_model_result_is_a_plain_no_summary() {
        let json = r#"{ "code": 0, "message": "0", "data": { "code": 1 } }"#;
        let conclusion = parse(json);
        assert_eq!(conclusion.data_code, 1);
        assert!(conclusion.cues.is_empty());
    }

    /// The subtitle block may arrive as an object instead of a one-element array.
    #[test]
    fn subtitle_accepts_both_shapes() {
        let object = r#"{
            "code": 0,
            "data": {
                "code": 0,
                "model_result": {
                    "summary": "有摘要",
                    "subtitle": {
                        "part_subtitle": [
                            { "start_timestamp": 0, "end_timestamp": 2, "content": "对象形状" }
                        ]
                    }
                }
            }
        }"#;
        assert_eq!(parse(object).cues.len(), 1);
        let array = r#"{
            "code": 0,
            "data": {
                "code": 0,
                "model_result": {
                    "summary": "有摘要",
                    "subtitle": [
                        { "part_subtitle": [] },
                        { "part_subtitle": [ { "start_timestamp": 0, "end_timestamp": 2, "content": "第二段" } ] }
                    ]
                }
            }
        }"#;
        let conclusion = parse(array);
        assert!(conclusion.cues.is_empty(), "only the first part is used");
    }

    /// A risk-control envelope is an error, and the classifier sees the code.
    #[test]
    fn risk_control_is_classified() {
        let error = anyhow::anyhow!("获取 AI 摘要失败（code=-352）");
        assert!(super::is_risk_control(&error));
        let error = anyhow::anyhow!("获取 AI 摘要失败：请求被拦截（code=-412）");
        assert!(super::is_risk_control(&error));
        let error = anyhow::anyhow!("获取 AI 摘要失败（code=-404）");
        assert!(!super::is_risk_control(&error));
    }
}

#[cfg(test)]
mod up_tests {
    use super::{length_seconds, UpOrder};

    /// The two modes differ only by the `order` value the endpoint gets.
    #[test]
    fn up_order_params() {
        assert_eq!(UpOrder::Plays.param(), "click");
        assert_eq!(UpOrder::Newest.param(), "pubdate");
    }

    /// Arc-list durations arrive as `mm:ss` / `h:mm:ss` strings (some entries
    /// as plain seconds); anything unparseable is 0, never a panic.
    #[test]
    fn up_length_seconds() {
        assert_eq!(length_seconds("12:34"), 754);
        assert_eq!(length_seconds("1:02:03"), 3723);
        assert_eq!(length_seconds("90"), 90);
        assert_eq!(length_seconds(""), 0);
        assert_eq!(length_seconds("n/a"), 0);
    }
}

#[cfg(test)]
mod season_tests {
    use super::{detail_from_view, ViewData, VideoDetail, SEASON_MAX_EPISODES};

    /// Synthetic `x/web-interface/view` payloads only: placeholder ids, never a
    /// real BV号.
    fn detail(payload: &str) -> VideoDetail {
        let data: ViewData = serde_json::from_str(payload).expect("fixture parses");
        detail_from_view(data)
    }

    const ONLY_PAGES: &str = r#"{
        "bvid": "BV0000000001",
        "aid": 1001,
        "cid": 5001,
        "title": "三分P示例",
        "pages": [
            { "cid": 5001, "page": 1, "part": "第一段", "duration": 61 },
            { "cid": 5002, "page": 2, "part": "", "duration": 62 },
            { "cid": 5003, "page": 3, "part": "第三段" }
        ]
    }"#;

    const ONLY_SEASON: &str = r#"{
        "bvid": "BV0000000002",
        "aid": 1002,
        "cid": 6001,
        "title": "合集示例",
        "ugc_season": {
            "id": 9001,
            "title": "示例合集",
            "cover": "//i0.hdslb.com/season.jpg",
            "mid": 77,
            "ep_count": 3,
            "sections": [
                { "id": 1, "title": "第一话", "episodes": [
                    { "aid": 2001, "bvid": "BV0000000011", "cid": 6001,
                      "title": "第一集", "arc": { "duration": 120, "pic": "//i0.hdslb.com/e1.jpg" },
                      "page": { "cid": 6001, "part": "第一集", "duration": 121 } },
                    { "aid": 2002, "bvid": "BV0000000012", "cid": 6002,
                      "title": "第二集", "arc": { "duration": 122 } }
                ] },
                { "id": 2, "title": "第二话", "episodes": [
                    { "aid": 2003, "bvid": "BV0000000013", "cid": 6003, "title": "第三集" }
                ] }
            ]
        }
    }"#;

    #[test]
    fn pages_only_keeps_每p_metadata() {
        let detail = detail(ONLY_PAGES);
        assert!(detail.season.is_none());
        assert_eq!(detail.pages.len(), 3);
        assert_eq!(detail.pages[0].cid, 5001);
        assert_eq!(detail.pages[0].page, 1);
        assert_eq!(detail.pages[0].title, "第一段");
        assert_eq!(detail.pages[0].duration, 61);
        // An empty `part` still has a usable label, and its duration defaults
        // to 0 rather than a guess.
        assert_eq!(detail.pages[1].title, "P2");
        assert_eq!(detail.pages[1].duration, 62);
        assert_eq!(detail.pages[2].duration, 0);
    }

    #[test]
    fn season_only_is_parsed_with_fallbacks() {
        let detail = detail(ONLY_SEASON);
        let season = detail.season.expect("ugc_season is parsed");
        assert_eq!(season.title, "示例合集");
        assert_eq!(season.ep_count, 3);
        assert_eq!(season.sections.len(), 2);
        assert_eq!(season.sections[0].title, "第一话");
        let first = &season.sections[0].episodes[0];
        assert_eq!(first.cid, 6001);
        assert_eq!(first.duration, 121);
        assert_eq!(first.bvid, "BV0000000011");
        assert_eq!(first.aid, 2001);
        // `page.duration` wins over `arc.duration`; a missing `page` falls back
        // to `arc.duration` and to the season cover.
        assert_eq!(season.sections[0].episodes[1].duration, 122);
        // No `page` and no `arc` at all: duration 0, season cover.
        assert_eq!(season.sections[1].episodes[0].duration, 0);
        assert_eq!(season.sections[1].episodes[0].cid, 6003);
    }

    #[test]
    fn pages_and_season_both_are_kept() {
        let payload = r#"{
            "bvid": "BV0000000003",
            "aid": 1003,
            "cid": 7000,
            "title": "两者都有",
            "pages": [ { "cid": 7000, "page": 1, "part": "上", "duration": 10 },
                       { "cid": 7001, "page": 2, "part": "下", "duration": 20 } ],
            "ugc_season": {
                "id": 12, "title": "合集", "cover": "", "mid": 1, "ep_count": 1,
                "sections": [ { "id": 3, "title": "唯一一节", "episodes": [
                    { "aid": 3001, "bvid": "BV0000000021", "cid": 7000,
                      "page": { "cid": 7000, "part": "上", "duration": 10 } }
                ] } ]
            }
        }"#;
        let detail = detail(payload);
        assert_eq!(detail.pages.len(), 2);
        let season = detail.season.expect("season stays available");
        assert_eq!(season.sections[0].episodes.len(), 1);
        // The episode had no `arc`: the title falls back to `page.part`.
        assert_eq!(season.sections[0].episodes[0].title, "上");
    }

    #[test]
    fn empty_ugc_season_is_none() {
        let payload = r#"{
            "bvid": "BV0000000004",
            "aid": 1004,
            "cid": 8000,
            "pages": [ { "cid": 8000, "page": 1, "part": "唯一", "duration": 30 } ],
            "ugc_season": []
        }"#;
        assert!(detail(payload).season.is_none());
    }

    #[test]
    fn season_without_sections_is_none() {
        let payload = r#"{
            "bvid": "BV0000000005",
            "aid": 1005,
            "cid": 8100,
            "ugc_season": { "id": 1, "title": "空合集", "sections": [] }
        }"#;
        assert!(detail(payload).season.is_none());
    }

    #[test]
    fn episode_without_page_uses_cid_and_arc() {
        let payload = r#"{
            "bvid": "BV0000000006",
            "aid": 1006,
            "cid": 8200,
            "ugc_season": {
                "id": 4, "title": "缺 page", "cover": "//cover", "mid": 2, "ep_count": 1,
                "sections": [ { "id": 5, "title": "", "episodes": [
                    { "aid": 4001, "bvid": "BV0000000031", "cid": 8201, "title": "只有 cid",
                      "arc": { "duration": 45, "pic": "//arc-pic" } }
                ] } ]
            }
        }"#;
        let season = detail(payload).season.expect("season exists");
        let episode = &season.sections[0].episodes[0];
        assert_eq!(episode.cid, 8201);
        assert_eq!(episode.duration, 45);
        assert_eq!(episode.title, "只有 cid");
    }

    #[test]
    fn malformed_values_degrade_to_zero_or_empty() {
        let payload = r#"{
            "bvid": "BV0000000007",
            "aid": 1007,
            "cid": 8300,
            "ugc_season": {
                "id": "12", "title": 7, "cover": null, "mid": -3, "ep_count": "2",
                "sections": [ { "id": "8", "title": null, "episodes": [
                    { "aid": "4002", "bvid": 42, "cid": "8301", "title": "字符串 id",
                      "page": { "cid": -1, "duration": -5 } }
                ] }, "not-an-object" ]
            }
        }"#;
        let season = detail(payload).season.expect("string ids still parse");
        assert_eq!(season.title, "");
        assert_eq!(season.ep_count, 2);
        assert_eq!(season.sections.len(), 1);
        let episode = &season.sections[0].episodes[0];
        // `page.cid` is negative -> falls through to the episode's own string id.
        assert_eq!(episode.cid, 8301);
        assert_eq!(episode.aid, 4002);
        assert_eq!(episode.bvid, "");
        assert_eq!(episode.duration, 0);
    }

    #[test]
    fn sections_and_episodes_are_capped() {
        let mut episodes = String::new();
        for index in 0..(SEASON_MAX_EPISODES + 5) {
            if index > 0 {
                episodes.push(',');
            }
            episodes.push_str(&format!(
                r#"{{ "aid": {}, "bvid": "BV{:010}", "cid": {}, "title": "e{index}",
                     "page": {{ "cid": {}, "duration": 1 }} }}"#,
                5000 + index,
                10000 + index,
                9000 + index,
                9000 + index,
            ));
        }
        let payload = format!(
            r#"{{ "bvid": "BV0000000008", "aid": 1008, "cid": 8400,
                 "ugc_season": {{ "id": 1, "title": "超长", "cover": "", "mid": 1,
                   "ep_count": 505, "sections": [
                     {{ "id": 1, "title": "一节", "episodes": [{}] }} ] }} }}"#,
            episodes
        );
        let season = detail(&payload).season.expect("season exists");
        let total: usize = season
            .sections
            .iter()
            .map(|section| section.episodes.len())
            .sum();
        assert_eq!(total, SEASON_MAX_EPISODES);
    }
}
