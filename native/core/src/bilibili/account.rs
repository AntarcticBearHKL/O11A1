//!  账号与互动：登录态、收藏、历史、稍后再看、关注/粉丝、动态、消息、私信与评论。

use super::*;
use serde::Serialize;

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

/// Like / un-like one video comment (`x/v2/reply/action`).
const COMMENT_ACTION_URL: &str = "https://api.bilibili.com/x/v2/reply/action";

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

/// Drop one account's folder cache after a create/delete/rename so the next
/// page load sees the server state immediately.
fn fav_folders_invalidate(mid: i64) {
    if let Ok(mut guard) = fav_cache().lock() {
        guard.folders.remove(&mid);
    }
}

/// Drop the cached pages of one folder after removing videos from it.
fn fav_pages_invalidate(media_id: i64) {
    if let Ok(mut guard) = fav_cache().lock() {
        guard.pages.retain(|(id, _), _| *id != media_id);
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

pub(crate) fn fav_related_cached(bvid: &str) -> Option<Vec<FeedVideo>> {
    let guard = fav_cache().lock().ok()?;
    let (at, items) = guard.related.get(bvid)?;
    if at.elapsed() < FAV_RELATED_TTL {
        Some(items.clone())
    } else {
        None
    }
}

pub(crate) fn fav_related_store(bvid: &str, items: &[FeedVideo]) {
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

/// 进度条悬停预览要用的雪碧图描述（B站 `x/player/videoshot`）。
///
/// 一张雪碧图是 `cols x rows` 的格子（实测 10x10 = 100 格），每格
/// `cell_w x cell_h`（实测 480x270）。`image` 是 1..N 张 jpg，格子按
/// “先第 1 张、行优先、再第 2 张”连续编号；`times[k]` 是第 k 格对应的时间
/// （秒），长度不超过 N*cols*rows。`times` 已去掉 B站 `index` 数组首位的 0
/// 哨兵（实测 `[0,0,5,10,…]` → `[0,5,10,…]`）。
#[derive(Clone, Debug, Default, Serialize)]
pub struct VideoShot {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    /// 补全了 `https:` 的图片 URL（接口返回的是 `//…` 开头）。
    pub sheets: Vec<String>,
    /// 每格的时间戳（秒），单调不减。
    pub times: Vec<f32>,
}

/// `x/player/videoshot` 的响应外壳。`data` 缺失时按“没有雪碧图”处理。
#[derive(Deserialize, Default)]
pub(super) struct ShotResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) message: String,
    #[serde(default)]
    pub(super) data: Option<ShotData>,
}

#[derive(Deserialize, Default)]
pub(crate) struct ShotData {
    #[serde(default)]
    pub(super) img_x_len: u32,
    #[serde(default)]
    pub(super) img_y_len: u32,
    #[serde(default)]
    pub(super) img_x_size: u32,
    #[serde(default)]
    pub(super) img_y_size: u32,
    /// 接口字段名是单数 `image`，值是字符串数组。
    #[serde(default, rename = "image")]
    pub(super) sheets: Vec<String>,
    /// 每格的时间（秒），首位实测是重复的 0 哨兵。
    #[serde(default)]
    pub(super) index: Vec<f64>,
}

/// `ShotData` -> [`VideoShot`]：补全 URL 协议、去掉 `index` 的 0 哨兵、把时间戳
/// 截到格子数以内。纯函数，网络与 UI 之外的映射规则只有这一处。
pub(crate) fn videoshot_from(data: ShotData) -> Result<VideoShot> {
    let cols = data.img_x_len.max(1);
    let rows = data.img_y_len.max(1);
    if data.img_x_size == 0 || data.img_y_size == 0 || data.sheets.is_empty() {
        bail!("videoshot: 响应缺少雪碧图");
    }
    let sheets: Vec<String> = data
        .sheets
        .iter()
        .map(|url| {
            let url = url.trim();
            if let Some(rest) = url.strip_prefix("//") {
                format!("https://{rest}")
            } else {
                url.to_string()
            }
        })
        .collect();
    // 首位是重复的 0 哨兵：`[0,0,5,10,…]` -> `[0,5,10,…]`。没有哨兵的视频
    // （首两个时间不同）原样使用。
    let mut times: Vec<f32> = if data.index.len() >= 2 && data.index[0] == data.index[1] {
        data.index.iter().skip(1).map(|value| *value as f32).collect()
    } else {
        data.index.iter().map(|value| *value as f32).collect()
    };
    if times.is_empty() {
        bail!("videoshot: index 为空");
    }
    if times.windows(2).any(|pair| pair[0] > pair[1]) {
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    // 时间戳不可能比格子多，多出来的（接口偶尔给整段而不是切片）截掉。
    let cells = (sheets.len() as u64 * u64::from(cols) * u64::from(rows)) as usize;
    times.truncate(cells);
    Ok(VideoShot {
        cols,
        rows,
        cell_w: data.img_x_size,
        cell_h: data.img_y_size,
        sheets,
        times,
    })
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Best-effort; caller ignores failures.
    pub async fn myinfo(&self) -> Result<serde_json::Value> {
        let value: serde_json::Value = self.client.get(MYINFO_URL).send().await?.json().await?;
        Ok(value)
    }

    // -- 大会员 (VIP) --------------------------------------------------------
}

/// Public catalog of every 大会员 privilege (`x/vip/privilege/list`). It needs
/// no cookie and answers `code: 0` anonymously (probed 2026-11-10).
const VIP_PRIVILEGE_CATALOG_URL: &str = "https://api.bilibili.com/x/vip/privilege/list";

/// 大积分中心: VIP state, point balance and the real task list. Cookie
/// (SESSDATA) required; a logged-out cookie answers `code: -101`.
const VIP_POINT_URL: &str = "https://api.bilibili.com/x/vip_point/homepage/combine";

impl Session {
    /// Current VIP state plus the monthly privilege list (`privilege/my`).
    /// Kept as the compact [`VipCode`] view for the existing badge/action code;
    /// [`Session::vip_privilege_my_page`] exposes the same answer typed.
    pub async fn vip_privilege_my(&self) -> Result<VipCode> {
        let page = self.vip_privilege_my_page().await?;
        let summary = page.summary();
        Ok(VipCode {
            code: page.code,
            message: page.message,
            summary,
        })
    }

    /// The full typed `x/vip/privilege/my` answer. A non-zero API code is a
    /// normal answer (the page keeps `code`/`message` for display), so only
    /// HTTP/JSON failures are `Err`.
    pub async fn vip_privilege_my_page(&self) -> Result<VipMyPage> {
        let text = self.client.get(VIP_PRIVILEGE_URL).send().await?.text().await?;
        let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("会员状态响应解析失败：{err}"))?;
        let page = vip_my_page_from(&value);
        crate::debug_log(&format!(
            "vip privilege/my code={} vip={} list={}",
            page.code,
            page.is_vip,
            page.list.len()
        ));
        Ok(page)
    }

    /// The public 大会员 privilege catalog (`x/vip/privilege/list`), grouped
    /// exactly as the server sends it. No cookie is required.
    pub async fn vip_privilege_catalog(&self) -> Result<Vec<VipPrivilegeGroup>> {
        let body: VipCatalogResp = self
            .get_query_json(VIP_PRIVILEGE_CATALOG_URL, &[], Some("https://www.bilibili.com/"))
            .await?;
        if body.code != 0 {
            let message = body.message.trim();
            if message.is_empty() {
                bail!("获取会员特权列表失败（code={}）", body.code);
            }
            bail!("获取会员特权列表失败：{message}（code={}）", body.code);
        }
        let groups = vip_catalog_from(body.data.unwrap_or_default());
        crate::debug_log(&format!(
            "vip privilege/list groups={} items={}",
            groups.len(),
            groups.iter().map(|group| group.items.len()).sum::<usize>()
        ));
        Ok(groups)
    }

    /// The real 大会员 task list (`x/vip_point/homepage/combine`). A non-zero
    /// `code` (typically `-101` for a logged-out cookie) is returned inside
    /// [`VipTaskPage`] so the member page can show it verbatim.
    pub async fn vip_point_tasks(&self) -> Result<VipTaskPage> {
        let value: serde_json::Value = self
            .client
            .get(VIP_POINT_URL)
            .header(REFERER, "https://www.bilibili.com/")
            .send()
            .await?
            .json()
            .await?;
        let page = vip_task_page_from(&value);
        crate::debug_log(&format!(
            "vip_point homepage code={} tasks={} point={}",
            page.code,
            page.tasks.len(),
            page.point
        ));
        Ok(page)
    }
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Like (`liked = true`) or un-like one comment. The endpoint is
    /// `x/v2/reply/action` with `oid`, `type=1`, `rpid`, `action` and `csrf`.
    ///
    /// The API `code`/`message` is returned verbatim (not hidden) because this
    /// write can be risk-controlled (`-111` bad csrf, `-404` gone comment).
    pub async fn comment_like(
        &self,
        aid: i64,
        rpid: i64,
        liked: bool,
    ) -> Result<CommentLikeResult> {
        if aid <= 0 || rpid <= 0 {
            bail!("评论点赞参数无效（aid={aid}, rpid={rpid}）");
        }
        let csrf = self.csrf_cookie()?;
        let body = comment_like_body(aid, rpid, liked, &csrf);
        let referer = format!("https://www.bilibili.com/video/av{aid}");
        let (code, message) = self
            .post_form_code("comment/action", COMMENT_ACTION_URL, &referer, body)
            .await?;
        Ok(CommentLikeResult { code, message })
    }

    // -- Write helper -------------------------------------------------------
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Every favourite folder of the active account, **including** the default
    /// folder and empty ones - the player's 收藏 picker must show all of them.
    ///
    /// `rid` (`aid`, `type=2`) asks the same endpoint for the account's own
    /// membership: each row then carries the API's `fav_state`
    /// (`1` = this video is already in that folder). `rid == 0` omits the
    /// parameter and every row comes back with `member == None`, which the UI
    /// renders as **未确认** rather than a guessed check mark.
    pub async fn fav_folders_all(&self, rid: i64) -> Result<Vec<FavFolderState>> {
        let mid = self.fav_mid().await?;
        let mut query: Vec<(&str, String)> = vec![
            ("up_mid", mid.to_string()),
            ("web_location", "333.1387".to_string()),
        ];
        if rid > 0 {
            query.push(("rid", rid.to_string()));
            query.push(("type", "2".to_string()));
        }
        let resp: FavFoldersResp = self
            .client
            .get(FAV_FOLDERS_URL)
            .query(&query)
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            return Err(fav_error("获取收藏夹失败", resp.code, &resp.message));
        }
        let folders = fav_folder_states_from(resp.data);
        crate::debug_log(&format!(
            "fav picker mid={mid} rid={rid} n={} known={} member={}",
            folders.len(),
            folders.iter().filter(|folder| folder.member.is_some()).count(),
            folders.iter().filter(|folder| folder.member == Some(true)).count()
        ));
        Ok(folders)
    }
}

impl Session {
    /// Create a favourite folder (`privacy` 0 公开 / 1 私密) and return its
    /// `media_id`, which is the id [`Session::fav_deal`] takes.
    pub async fn fav_create(&self, title: &str, privacy: i64) -> Result<i64> {
        let title = title.trim();
        if title.is_empty() {
            bail!("收藏夹名称不能为空");
        }
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "title={}&privacy={}&csrf={}",
            encode_component(title),
            if privacy == 0 { 0 } else { 1 },
            encode_component(&csrf),
        );
        let response = self
            .client
            .post(FAV_FOLDER_ADD_URL)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(REFERER, "https://www.bilibili.com/")
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let resp: FavCreateResp = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|err| anyhow!("新建收藏夹响应解析失败（HTTP {status}）：{err}"))?;
        crate::debug_log(&format!(
            "fav_create http={status} code={} msg={} privacy={}",
            resp.code, resp.message, privacy
        ));
        if resp.code != 0 {
            return Err(fav_error("新建收藏夹失败", resp.code, &resp.message));
        }
        let id = resp.data.map(|data| data.id).unwrap_or(0);
        if id <= 0 {
            bail!("新建收藏夹失败：响应缺少 id");
        }
        crate::debug_log(&format!("fav_create ok id={id} title={title:?}"));
        // The folder list is cached for ten minutes; a new folder must be
        // visible immediately after the create returns.
        if let Ok(mid) = self.fav_mid().await {
            fav_folders_invalidate(mid);
        }
        Ok(id)
    }
}

impl Session {
    /// Delete one favourite folder (`media_id` must be a non-default folder).
    pub async fn fav_folder_delete(&self, media_id: i64) -> Result<()> {
        if media_id <= 0 {
            bail!("收藏夹 ID 无效");
        }
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "media_id={media_id}&platform=web&csrf={}",
            encode_component(&csrf),
        );
        let (code, message) = self
            .post_form_code(
                "fav_folder_del",
                FAV_FOLDER_DEL_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        if code != 0 {
            return Err(fav_error("删除收藏夹失败", code, &message));
        }
        crate::debug_log(&format!("fav_folder_del ok media_id={media_id}"));
        if let Ok(mid) = self.fav_mid().await {
            fav_folders_invalidate(mid);
        }
        fav_pages_invalidate(media_id);
        Ok(())
    }

    /// Rename one favourite folder, keeping its contents and privacy.
    pub async fn fav_folder_rename(&self, media_id: i64, title: &str) -> Result<()> {
        if media_id <= 0 {
            bail!("收藏夹 ID 无效");
        }
        let title = title.trim();
        if title.is_empty() {
            bail!("收藏夹名称不能为空");
        }
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "media_id={media_id}&title={}&csrf={}",
            encode_component(title),
            encode_component(&csrf),
        );
        let (code, message) = self
            .post_form_code(
                "fav_folder_edit",
                FAV_FOLDER_EDIT_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        if code != 0 {
            return Err(fav_error("重命名收藏夹失败", code, &message));
        }
        crate::debug_log(&format!(
            "fav_folder_edit ok media_id={media_id} title={title:?}"
        ));
        if let Ok(mid) = self.fav_mid().await {
            fav_folders_invalidate(mid);
        }
        Ok(())
    }

    /// Remove many videos from one folder through `resource/batch-del`.
    ///
    /// `resources` is Bilibili's `aid:type` list (`2` = video); the endpoint
    /// caps a request, so callers should pass a page-sized batch. Returns how
    /// many ids were submitted.
    pub async fn fav_batch_delete(&self, media_id: i64, aids: &[i64]) -> Result<usize> {
        if media_id <= 0 {
            bail!("收藏夹 ID 无效");
        }
        let aids: Vec<i64> = aids.iter().copied().filter(|aid| *aid > 0).collect();
        if aids.is_empty() {
            return Ok(0);
        }
        let resources = aids
            .iter()
            .map(|aid| format!("{aid}:2"))
            .collect::<Vec<_>>()
            .join(",");
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "resources={}&media_id={media_id}&platform=web&csrf={}",
            encode_component(&resources),
            encode_component(&csrf),
        );
        let (code, message) = self
            .post_form_code(
                "fav_batch_del",
                FAV_BATCH_DEL_URL,
                "https://www.bilibili.com/",
                body,
            )
            .await?;
        if code != 0 {
            return Err(fav_error("批量取消收藏失败", code, &message));
        }
        crate::debug_log(&format!(
            "fav_batch_del ok media_id={media_id} n={}",
            aids.len()
        ));
        fav_pages_invalidate(media_id);
        Ok(aids.len())
    }
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Add `rid` (an `aid`, `type=2`) to `add` folders and remove it from `del`.
    pub async fn fav_deal(&self, rid: i64, add: &[i64], del: &[i64]) -> Result<()> {
        if rid <= 0 {
            bail!("收藏失败：缺少 aid");
        }
        if add.is_empty() && del.is_empty() {
            return Ok(());
        }
        let csrf = self.csrf_cookie()?;
        let body = format!(
            "rid={rid}&type=2&add_media_ids={}&del_media_ids={}&csrf={}",
            encode_component(&join_ids(add)),
            encode_component(&join_ids(del)),
            encode_component(&csrf),
        );
        let (code, message) = self
            .post_form_code("fav_deal", FAV_DEAL_URL, "https://www.bilibili.com/", body)
            .await?;
        if code != 0 {
            return Err(fav_error("收藏操作失败", code, &message));
        }
        crate::debug_log(&format!(
            "fav_deal ok rid={rid} add={:?} del={:?}",
            add, del
        ));
        Ok(())
    }
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Remove the 稍后再看 entry for `aid`.
    pub async fn toview_del(&self, aid: i64) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!("aid={aid}&csrf={}", encode_component(&csrf));
        self.post_form(TOVIEW_DEL_URL, "https://www.bilibili.com/", body)
            .await
    }

    // -- Search -------------------------------------------------------------
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// One page of 回复我的 / @我的 / 收到的赞.
    ///
    /// `cursor` is the previous page's [`MsgPage::cursor`] (`None` for the
    /// first page). The three endpoints use their own time field name, but the
    /// cursor object is identical; the parameter builder below is the only
    /// place that names it.
    ///
    /// Pagination probe (2026-11-10): the official `message.bilibili.com`
    /// bundle calls `/x/msgfeed/reply|at|like` with the previous
    /// `data.cursor{id,time}` as `id` + `reply_time` / `at_time` / `like_time`.
    /// 收到的赞 nests both the page and its cursor under `data.total`
    /// (`data.latest` is only the "最近点赞" strip). A live read of this
    /// account returned empty lists (`is_end: true`), so the cursor loop is
    /// covered by the parser fixtures below and verified against the site's
    /// own request builder.
    pub async fn msg_page(&self, kind: MsgKind, cursor: Option<MsgCursor>) -> Result<MsgPage> {
        let url = match kind {
            MsgKind::Reply => MSG_REPLY_URL,
            MsgKind::At => MSG_AT_URL,
            MsgKind::Like => MSG_LIKE_URL,
        };
        let mut params: Vec<(&str, String)> = vec![
            ("platform", "web".to_string()),
            ("build", "0".to_string()),
            ("mobi_app", "web".to_string()),
        ];
        params.extend(msg_cursor_params(kind, cursor));
        let body: MsgFeedResp = self.get_query_json(url, &params, None).await?;
        if body.code != 0 {
            let message = body.message.trim();
            if message.is_empty() {
                bail!("获取消息列表失败（code={}）", body.code);
            }
            bail!("获取消息列表失败：{message}（code={}）", body.code);
        }
        let page = msg_page_from(kind, body.data.unwrap_or_default());
        crate::debug_log(&format!(
            "msg_page kind={:?} n={} cursor={} is_end={}",
            kind,
            page.items.len(),
            page.cursor.id,
            page.is_end
        ));
        Ok(page)
    }

    /// First page of a message list; same as [`Session::msg_page`] with no
    /// cursor.
    pub async fn msg_list(&self, kind: MsgKind) -> Result<MsgPage> {
        self.msg_page(kind, None).await
    }

    // -- 私信 (private messages, READ ONLY) --------------------------------
}

impl Session {
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
}

impl Session {
    /// The newest page of one session's messages, oldest first.
    pub async fn dm_messages(&self, talker_id: i64) -> Result<DmPage> {
        self.dm_messages_page(talker_id, None).await
    }

    /// One older page of one session's messages.
    ///
    /// `end_seqno` is the previous [`DmPage::min_seqno`]; the endpoint returns
    /// messages strictly older than that boundary (`has_more` says whether
    /// another page exists). Messages are sorted oldest → newest.
    ///
    /// Live probe 2026-11-10: passing the newest seqno of an 11-message
    /// conversation as `end_seqno` returned the 10 older messages, so the
    /// boundary is exclusive exactly as the API docs describe.
    pub async fn dm_messages_before(&self, talker_id: i64, end_seqno: i64) -> Result<DmPage> {
        if end_seqno <= 0 {
            bail!("读取更早私信失败：end_seqno 无效");
        }
        self.dm_messages_page(talker_id, Some(end_seqno)).await
    }

    async fn dm_messages_page(&self, talker_id: i64, end_seqno: Option<i64>) -> Result<DmPage> {
        let mut params: Vec<(&str, String)> = vec![
            ("talker_id", talker_id.to_string()),
            ("session_type", "1".to_string()),
            ("size", "20".to_string()),
            ("mobi_app", "web".to_string()),
            ("build", "0".to_string()),
        ];
        match end_seqno {
            Some(end_seqno) => params.push(("end_seqno", end_seqno.to_string())),
            // The web client sends an explicit empty `begin_seqno` for the
            // newest page; keeping it matches the observed request shape.
            None => params.push(("begin_seqno", String::new())),
        }
        let body: DmMessagesResp = self.get_query_json(DM_MESSAGES_URL, &params, None).await?;
        if body.code != 0 {
            let message = body.message.trim();
            if message.is_empty() {
                bail!("获取私信内容失败（code={}）", body.code);
            }
            bail!("获取私信内容失败：{message}（code={}）", body.code);
        }
        let page = dm_page_from(body);
        crate::debug_log(&format!(
            "dm_messages talker={talker_id} end={end_seqno:?} n={} has_more={} min={} max={}",
            page.messages.len(),
            page.has_more,
            page.min_seqno,
            page.max_seqno
        ));
        Ok(page)
    }

    // -- 关注 / 粉丝 (relations) -------------------------------------------
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

/// Full signed parameter set for `playurl`. The `dm_img_*` / `web_location`
/// entries are required since 2026-06 to avoid an HTTP 412 risk-control block.
/// Form body of one `x/click-interface/web/heartbeat` call.
///
/// `type=3` / `sub_type=0` / `dt=2` / `play_type` mirror what the web player
/// sends; `video_duration` keeps the history entry's progress bar proportional.
#[allow(clippy::too_many_arguments)]
pub(crate) fn heartbeat_body(
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

#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
pub enum NavOutcome {
    LoggedIn(NavUser),
    NotLoggedIn { code: i64, message: String },
}

/// Verbatim outcome of one 大会员 endpoint call. A non-zero `code` (e.g.
/// "not a member" / "already claimed") is an ordinary answer, so it is
/// returned rather than raised; `summary` carries an optional short text for
/// `privilege/my`.
#[derive(Clone, Debug, Serialize)]
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

/// One monthly privilege/coupon row of `x/vip/privilege/my`.
///
/// `kind` is the API's `type` (1 B币券, 2 会员购优惠券, … 9 每日经验);
/// [`VipMyPrivilege::label`] names the known ones. `state` is 0 未兑换,
/// 1 已兑换, 2 未完成.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct VipMyPrivilege {
    pub kind: i64,
    pub state: i64,
    pub expire_time: i64,
    pub vip_type: i64,
    pub next_receive_days: i64,
    pub period_end_unix: i64,
}

impl VipMyPrivilege {
    pub fn label(&self) -> String {
        match self.kind {
            1 => "B币券".to_string(),
            2 => "会员购优惠券".to_string(),
            3 => "漫画福利券".to_string(),
            4 => "会员购包邮券".to_string(),
            5 => "漫画商城优惠券".to_string(),
            6 => "装扮体验卡".to_string(),
            7 => "课堂优惠券".to_string(),
            8 => "每日礼盒".to_string(),
            9 => "每日经验".to_string(),
            other => format!("特权 {other}"),
        }
    }

    pub fn state_label(&self) -> &'static str {
        match self.state {
            0 => "未兑换",
            1 => "已兑换",
            2 => "未完成",
            _ => "未知状态",
        }
    }

    pub fn received(&self) -> bool {
        self.state == 1
    }

    pub fn incomplete(&self) -> bool {
        self.state == 2
    }
}

/// The typed `x/vip/privilege/my` answer: account VIP state plus every
/// monthly privilege row the server returned.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VipMyPage {
    pub code: i64,
    pub message: String,
    pub is_vip: bool,
    pub vip_type: i64,
    pub list: Vec<VipMyPrivilege>,
}

impl VipMyPage {
    pub fn summary(&self) -> String {
        if self.code != 0 {
            return String::new();
        }
        let label = if self.is_vip { "大会员" } else { "非大会员" };
        if !self.is_vip {
            // A non-member's `state=0` rows are the catalogue of what a
            // membership *would* unlock, not claimable coupons.
            return format!("{label} · 特权 {} 项", self.list.len());
        }
        let pending = self
            .list
            .iter()
            .filter(|item| !item.received() && !item.incomplete())
            .count();
        if pending > 0 {
            format!("{label} · 特权 {} 项 · 可领取 {pending}", self.list.len())
        } else {
            format!("{label} · 特权 {} 项", self.list.len())
        }
    }
}

/// One privilege catalog card of the public `x/vip/privilege/list` endpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct VipPrivilegeItem {
    pub id: i64,
    pub name: String,
    pub desc: String,
    pub icon: String,
    pub link: String,
    pub hot: bool,
    pub new: bool,
}

/// One privilege group (`内容特权`, `视听特权`, …).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct VipPrivilegeGroup {
    pub id: i64,
    pub name: String,
    pub items: Vec<VipPrivilegeItem>,
}

/// One 大积分 task of `x/vip_point/homepage/combine`.
///
/// `state`: 0 未领取, 1 未完成, 3 已完成/已领取. The endpoint is the real task
/// list; there is no `x/v2/vip/task/list` (that path answers HTTP 404).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct VipTask {
    pub code: String,
    pub state: i64,
    pub title: String,
    pub icon: String,
    pub subtitle: String,
    pub explain: String,
    pub link: String,
    pub vip_limit: bool,
    pub complete_times: i64,
    pub max_times: i64,
}

impl VipTask {
    pub fn state_label(&self) -> &'static str {
        match self.state {
            0 => "可领取",
            1 => "未完成",
            3 => "已完成",
            _ => "未知状态",
        }
    }

    pub fn done(&self) -> bool {
        self.state == 3
    }

    /// "1/1" when the API names both counters, else empty.
    pub fn progress(&self) -> String {
        if self.max_times > 0 {
            format!("{}/{}", self.complete_times, self.max_times)
        } else {
            String::new()
        }
    }
}

/// The typed 大积分 center answer: VIP/point state plus the real task list.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VipTaskPage {
    pub code: i64,
    pub message: String,
    pub is_vip: bool,
    pub vip_type: i64,
    /// Milliseconds, as the endpoint reports it.
    pub vip_due_date: i64,
    pub point: i64,
    pub signed: bool,
    pub sign_score: i64,
    pub task_count: i64,
    pub tasks: Vec<VipTask>,
}

impl VipTaskPage {
    /// True when the endpoint accepted the request; a non-zero `code` is a
    /// server answer (usually `-101` for a logged-out cookie), never hidden.
    pub fn is_ok(&self) -> bool {
        self.code == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Relation {
    pub following: i64,
    pub follower: i64,
}

#[derive(Deserialize)]
pub(super) struct LevelInfo {
    #[serde(default)]
    current_level: i64,
}

#[derive(Deserialize)]
pub(super) struct VipInfo {
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
struct VipCatalogResp {
    #[serde(default)]
    code: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    data: Option<Vec<VipGroupRaw>>,
}

#[derive(Deserialize, Default)]
struct VipGroupRaw {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    name: String,
    #[serde(default, deserialize_with = "de_null_default")]
    child_privileges: Vec<VipPrivilegeRaw>,
}

#[derive(Deserialize, Default)]
struct VipPrivilegeRaw {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    name: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    desc: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    icon_url: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    link: String,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    hot_type: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    new_type: i64,
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
pub(super) struct CodeResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) message: String,
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
    /// Only present when the request carried `rid`: 1 = that video is in this
    /// folder, 0 = it is not. Missing (`None`) means the payload did not say,
    /// which the picker shows as 未确认.
    #[serde(default)]
    fav_state: Option<i64>,
}

#[derive(Deserialize)]
struct FavCreateResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<FavCreateData>,
}

#[derive(Deserialize)]
struct FavCreateData {
    #[serde(default)]
    id: i64,
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
pub(super) struct RelatedResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) message: String,
    /// `null` when the endpoint answers without a list.
    #[serde(default)]
    pub(super) data: Option<Vec<Option<RelatedItem>>>,
}

#[derive(Deserialize)]
struct HistoryResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<HistoryData>,
}

#[derive(Deserialize)]
pub(super) struct HistoryIndexResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) message: String,
    #[serde(default)]
    pub(super) data: Option<Vec<String>>,
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
pub(super) struct SpiResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) data: Option<SpiData>,
}

#[derive(Deserialize)]
pub(super) struct SpiData {
    #[serde(default)]
    pub(super) b_3: String,
    #[serde(default)]
    pub(super) b_4: String,
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
        fav_cache, fav_folders_from, fav_page_from, fav_qa_picks, FavFolder,
        FavFoldersResp, FavListResp, RelatedResp, FAV_RELATED_MAX,
    };
    use crate::bilibili::feed::related_from;

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
#[derive(Clone, Debug, Default, Serialize)]
pub struct CommentUser {
    pub mid: i64,
    pub uname: String,
    pub avatar: String,
}

/// One comment: either a top-level comment or one of its replies.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Comment {
    pub rpid: i64,
    pub user: CommentUser,
    pub message: String,
    pub like: u64,
    /// Unix seconds.
    pub ctime: i64,
    /// Number of replies this comment has (0 for a leaf reply).
    pub rcount: i64,
    /// The API's own `action` bit (`1` = this account already liked it).
    pub liked: bool,
    /// Nested preview replies the list endpoint inlines (a few per comment).
    pub replies: Vec<Comment>,
}

/// One page of a video's comment list.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct CommentReplyPage {
    pub replies: Vec<Comment>,
    pub total: i64,
    /// 1-based page number this response is for.
    pub page: i64,
    /// Page size the endpoint used.
    pub size: i64,
    pub has_more: bool,
}

/// Verbatim answer of one comment-like write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CommentLikeResult {
    pub code: i64,
    pub message: String,
}

impl CommentLikeResult {
    pub fn is_ok(&self) -> bool {
        self.code == 0
    }
}

/// Form body of `x/v2/reply/action`. Kept pure so the parameter names are
/// pinned by a unit test (`action=1` like, `action=2` un-like).
fn comment_like_body(aid: i64, rpid: i64, liked: bool, csrf: &str) -> String {
    format!(
        "oid={aid}&type=1&rpid={rpid}&action={}&csrf={}",
        if liked { 1 } else { 2 },
        encode_component(csrf)
    )
}

// ---------------------------------------------------------------------------
// Search / detail types
// ---------------------------------------------------------------------------

/// Which order a UP主's 投稿 list is asked for.
///
/// The `order` query value is the whole difference between the two modes: the
/// endpoint returns the same `vlist` shape either way, so the page parser and
/// the cache key are shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct SpaceVideo {
    pub bvid: String,
    pub title: String,
    pub pic: String,
    pub length: String,
    pub play: u64,
    pub danmaku: u64,
}

/// One page of a UP主's投稿 list.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SpaceVideoPage {
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<SpaceVideo>,
}

/// One created favourite folder, as the 收藏推荐 stream picks from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FavFolder {
    /// The `media_id` the folder's contents are fetched with.
    pub id: i64,
    pub name: String,
    pub count: u32,
}

/// One row of the player's 收藏 picker: a folder of any size (default and empty
/// folders included) plus what the endpoint said about this video's membership.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FavFolderState {
    pub id: i64,
    pub name: String,
    pub count: u32,
    /// `Some(true)` = already in this folder, `Some(false)` = not in it,
    /// `None` = the payload did not report it (shown as 未确认, never guessed).
    pub member: Option<bool>,
}

/// One page of a favourite folder's contents.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FavListPage {
    pub title: String,
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<FavVideo>,
}

#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct HistoryPage {
    pub cursor: Option<HistoryCursor>,
    pub items: Vec<HistoryItem>,
    pub has_more: bool,
}

/// One day's historical danmaku plus which endpoint produced it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DanmakuHistory {
    pub items: Vec<crate::danmaku::Danmaku>,
    /// `"xml"` when parsed from `x/v2/dm/history`, `"protobuf"` on the fallback.
    pub source: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HistoryCursor {
    pub max: i64,
    pub view_at: i64,
    pub business: String,
}

#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct ToViewPage {
    pub count: i64,
    pub items: Vec<ToViewItem>,
}

#[derive(Clone, Debug, Default, Serialize)]
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
pub(super) struct ArchiveResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) data: Option<ArchiveData>,
}

#[derive(Deserialize, Default)]
pub(super) struct ArchiveData {
    #[serde(default)]
    pub(super) archives: Vec<CardItem>,
    #[serde(default)]
    pub(super) page: Option<ArchivePageInfo>,
}

#[derive(Deserialize, Default)]
pub(super) struct ArchivePageInfo {
    #[serde(default)]
    pub(super) num: i64,
    #[serde(default)]
    pub(super) size: i64,
}

#[derive(Deserialize)]
pub(crate) struct CardItem {
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

/// One dynamic-feed item after flattening its `modules`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DynamicItem {
    pub id_str: String,
    pub author_name: String,
    pub author_mid: i64,
    pub author_face: String,
    pub pub_time: String,
    pub text: String,
    pub archive_title: String,
    pub archive_bvid: String,
    pub archive_cover: String,
    pub archive_meta: String,
    pub images: Vec<String>,
    pub like_count: u64,
    pub liked: bool,
    pub comment_count: u64,
    pub forward_count: u64,
}

/// One page of the dynamic feed plus its pagination cursor.
#[derive(Clone, Serialize)]
pub struct DynamicPage {
    pub offset: String,
    pub has_more: bool,
    pub items: Vec<DynamicItem>,
}

/// Which message list to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum MsgKind {
    Reply,
    At,
    Like,
}

/// The forward cursor of one 回复/@/赞 list page.
///
/// The three endpoints share the same shape but name the time parameter after
/// themselves (`reply_time`, `at_time`, `like_time`); `time` is that value and
/// `id` is the last message id. Both come from `data.cursor` (for 赞:
/// `data.total.cursor`) and are sent back verbatim to load the next page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MsgCursor {
    pub id: i64,
    pub time: i64,
}

impl MsgCursor {
    /// A cursor is usable only when the server gave it a non-zero id.
    pub fn is_usable(&self) -> bool {
        self.id > 0
    }
}

/// Per-category unread counts behind the 消息 badge.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct MsgUnread {
    pub total: i64,
    pub reply: i64,
    pub at: i64,
    pub like: i64,
}

/// One 回复/@/赞 message entry.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MsgItem {
    /// The server's message id, used to de-duplicate an appended page.
    pub id: i64,
    pub nickname: String,
    pub avatar: String,
    pub title: String,
    pub detail: String,
    pub time: i64,
    pub count: i64,
}

/// One page of a message list plus its forward cursor.
///
/// `cursor` is only meaningful when `is_end` is false; [`MsgPage::has_more`]
/// combines both so a caller cannot page past the end.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MsgPage {
    pub items: Vec<MsgItem>,
    pub cursor: MsgCursor,
    pub is_end: bool,
}

impl MsgPage {
    pub fn has_more(&self) -> bool {
        !self.is_end && self.cursor.is_usable()
    }
}

/// One private-message session (READ ONLY).
#[derive(Clone, Debug, Default, Serialize)]
pub struct DmSession {
    pub talker_id: i64,
    pub at_time: i64,
    pub unread: i64,
    pub preview: String,
}

/// One private-message entry (READ ONLY).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DmMessage {
    pub sender_uid: i64,
    pub text: String,
    pub timestamp: i64,
    /// `msg_seqno`, the cursor used to page towards older messages.
    pub seqno: i64,
    /// The API's `msg_type` (`1` text, `2` image, `10` notification, …).
    pub msg_type: i64,
    /// 0 normal, 1 recalled, 2 system-recalled, 50 expired image.
    pub msg_status: i64,
}

/// One `fetch_session_msgs` page for a single conversation.
///
/// `messages` is sorted oldest → newest for rendering. `has_more` and
/// `min_seqno` are the server's own answer; pass `min_seqno` as `end_seqno` to
/// [`Session::dm_messages_before`] for the next older page.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DmPage {
    pub messages: Vec<DmMessage>,
    pub has_more: bool,
    pub min_seqno: i64,
    pub max_seqno: i64,
}

/// One 关注 / 粉丝 user.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RelationUser {
    pub mid: i64,
    pub uname: String,
    pub face: String,
    pub sign: String,
    pub official: String,
}

/// One page of a 关注 / 粉丝 list.
#[derive(Clone, Debug, Default, Serialize)]
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
    #[serde(default, deserialize_with = "de_i64_flexible")]
    mid: i64,
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
    bvid: String,
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
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    data: Option<MsgFeedData>,
}

#[derive(Deserialize, Default)]
struct MsgFeedData {
    /// 回复我的 / @我的 use this flat list; 收到的赞 leaves it empty.
    #[serde(default, deserialize_with = "de_null_default")]
    items: Vec<MsgFeedItem>,
    #[serde(default)]
    cursor: Option<MsgFeedCursor>,
    /// 收到的赞: the "最近点赞" block (only on the first page).
    #[serde(default)]
    latest: Option<MsgFeedLatest>,
    /// 收到的赞: the paginated "全部点赞" block.
    #[serde(default)]
    total: Option<MsgFeedTotal>,
}

#[derive(Clone, Deserialize, Default)]
struct MsgFeedCursor {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    id: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    time: i64,
    #[serde(default, deserialize_with = "de_flag")]
    is_end: bool,
}

#[derive(Deserialize, Default)]
struct MsgFeedLatest {
    #[serde(default, deserialize_with = "de_null_default")]
    items: Vec<MsgFeedItem>,
}

#[derive(Deserialize, Default)]
struct MsgFeedTotal {
    #[serde(default, deserialize_with = "de_null_default")]
    items: Vec<MsgFeedItem>,
    #[serde(default)]
    cursor: Option<MsgFeedCursor>,
}

#[derive(Deserialize, Default)]
struct MsgFeedItem {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    id: i64,
    #[serde(default)]
    user: Option<MsgUser>,
    /// 收到的赞 items carry every user who liked the same target.
    #[serde(default)]
    users: Vec<MsgUser>,
    #[serde(default)]
    item: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    counts: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    reply_time: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    at_time: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    like_time: i64,
}

#[derive(Clone, Deserialize, Default)]
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
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    data: Option<DmMessagesData>,
}

#[derive(Deserialize, Default)]
struct DmMessagesData {
    #[serde(default, deserialize_with = "de_null_default")]
    messages: Vec<DmMessageRaw>,
    #[serde(default, deserialize_with = "de_flag")]
    has_more: bool,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    min_seqno: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    max_seqno: i64,
}

#[derive(Deserialize, Default)]
struct DmMessageRaw {
    #[serde(default)]
    sender_uid: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    content: String,
    #[serde(default)]
    timestamp: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    msg_seqno: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    msg_type: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    msg_status: i64,
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
    /// `1` when the current account already liked this comment.
    #[serde(default, deserialize_with = "de_i64_flexible")]
    action: i64,
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

    let (archive_title, archive_bvid, archive_cover, archive_meta) = match major.archive {
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
            (
                archive.title,
                archive.bvid,
                normalize_url(&archive.cover),
                meta,
            )
        }
        None => (String::new(), String::new(), String::new(), String::new()),
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
        author_mid: author.mid,
        author_face: normalize_url(&author.face),
        pub_time: author.pub_time,
        text,
        archive_title,
        archive_bvid,
        archive_cover,
        archive_meta,
        images,
        like_count: like.count,
        liked: like.status,
        comment_count: stat.comment.map(|comment| comment.count).unwrap_or(0),
        forward_count: stat.forward.map(|forward| forward.count).unwrap_or(0),
    }
}

fn msg_item_from(kind: MsgKind, raw: MsgFeedItem) -> MsgItem {
    let users = if !raw.users.is_empty() {
        raw.users
    } else {
        raw.user.into_iter().collect()
    };
    let user = users.first().cloned().unwrap_or_default();
    let (title, detail) = raw.item.as_ref().map(msg_item_texts).unwrap_or_default();
    let time = match kind {
        MsgKind::Reply => raw.reply_time,
        MsgKind::At => raw.at_time,
        MsgKind::Like => raw.like_time,
    };
    // Older payloads name only one of the three stamps; take whichever is set
    // instead of showing an empty time.
    let time = if time > 0 {
        time
    } else {
        [raw.reply_time, raw.at_time, raw.like_time]
            .into_iter()
            .find(|value| *value > 0)
            .unwrap_or(0)
    };
    MsgItem {
        id: raw.id,
        nickname: user.nickname,
        avatar: normalize_url(&user.avatar),
        title,
        detail,
        time,
        count: raw.counts,
    }
}

/// Fold one `x/msgfeed/{reply,at,like}` payload into a typed page.
///
/// 回复/@ share `data.items` + `data.cursor`; 赞 answers `data.latest` (the
/// "最近点赞" strip) and `data.total` (the paginated list, cursor included).
/// A page ends when the server says so (`cursor.is_end`), when it supplies no
/// usable cursor, or when it returned no items.
fn msg_page_from(kind: MsgKind, data: MsgFeedData) -> MsgPage {
    let raw_cursor_is_end = match kind {
        MsgKind::Like => data
            .total
            .as_ref()
            .and_then(|total| total.cursor.as_ref())
            .or(data.cursor.as_ref())
            .map(|cursor| cursor.is_end)
            .unwrap_or(false),
        _ => data
            .cursor
            .as_ref()
            .map(|cursor| cursor.is_end)
            .unwrap_or(false),
    };
    let (items, cursor) = match kind {
        MsgKind::Like => {
            let fallback_cursor = data.cursor;
            let total = data.total.unwrap_or_default();
            let cursor = total.cursor.clone().or_else(|| fallback_cursor.clone());
            if total.items.is_empty() {
                // A brand-new account can have a "latest" strip but no total
                // page; showing it beats rendering an empty list.
                (
                    data.latest.map(|latest| latest.items).unwrap_or_default(),
                    fallback_cursor,
                )
            } else {
                (total.items, cursor)
            }
        }
        _ => (data.items, data.cursor),
    };
    let items: Vec<MsgItem> = items
        .into_iter()
        .map(|raw| msg_item_from(kind, raw))
        .collect();
    let cursor = cursor
        .map(|cursor| MsgCursor {
            id: cursor.id,
            time: cursor.time,
        })
        .unwrap_or_default();
    // Without a server cursor there is no honest way to ask for page N+1.
    let is_end = items.is_empty() || !cursor.is_usable() || raw_cursor_is_end;
    MsgPage {
        items,
        cursor,
        is_end,
    }
}

/// The cursor query parameters of one message list. The message id is `id` on
/// all three endpoints; only the time field is named after the kind. Returns an
/// empty list for the first page.
fn msg_cursor_params(kind: MsgKind, cursor: Option<MsgCursor>) -> Vec<(&'static str, String)> {
    let Some(cursor) = cursor.filter(MsgCursor::is_usable) else {
        return Vec::new();
    };
    let time_key = match kind {
        MsgKind::Reply => "reply_time",
        MsgKind::At => "at_time",
        MsgKind::Like => "like_time",
    };
    vec![
        ("id", cursor.id.to_string()),
        (time_key, cursor.time.to_string()),
    ]
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
        detail = pick("target_reply_content");
    }
    if detail.is_empty() {
        detail = pick("source_content");
    }
    if detail.is_empty() {
        detail = pick("title");
    }
    if detail.is_empty() {
        detail = pick("message");
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
            .or_else(|| value.get("content").and_then(|content| content.as_str()))
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
        seqno: raw.msg_seqno,
        msg_type: raw.msg_type,
        msg_status: raw.msg_status,
    }
}

/// Fold one `fetch_session_msgs` answer. The API returns newest first; the
/// display layer always wants oldest → newest, so the page is sorted by
/// sequence number (falling back to the timestamp for old payloads).
fn dm_page_from(resp: DmMessagesResp) -> DmPage {
    let data = resp.data.unwrap_or_default();
    let mut messages: Vec<DmMessage> = data.messages.into_iter().map(dm_message_from).collect();
    messages.sort_by_key(|message| (message.seqno, message.timestamp));
    DmPage {
        messages,
        has_more: data.has_more,
        min_seqno: data.min_seqno,
        max_seqno: data.max_seqno,
    }
}

// -- 大会员 payloads ---------------------------------------------------------

fn json_i64(value: Option<&serde_json::Value>) -> i64 {
    match value {
        Some(serde_json::Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))
            .unwrap_or(0),
        Some(serde_json::Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn json_bool(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::String(text)) => {
            matches!(text.trim(), "1" | "true" | "True")
        }
        _ => false,
    }
}

fn json_str(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn vip_my_page_from(value: &serde_json::Value) -> VipMyPage {
    let code = value
        .get("code")
        .map(|code| json_i64(Some(code)))
        .unwrap_or(-1);
    let message = json_str(value.get("message"));
    let data = value.get("data");
    let is_vip = json_bool(data.and_then(|data| data.get("is_vip")))
        || json_i64(data.and_then(|data| data.get("vip_status"))) == 1;
    let vip_type = json_i64(data.and_then(|data| data.get("vip_type")));
    let list = data
        .and_then(|data| data.get("list"))
        .and_then(|list| list.as_array())
        .map(|list| {
            list.iter()
                .map(|item| VipMyPrivilege {
                    kind: json_i64(item.get("type")),
                    state: json_i64(item.get("state")),
                    expire_time: json_i64(item.get("expire_time")),
                    vip_type: json_i64(item.get("vip_type")),
                    next_receive_days: json_i64(item.get("next_receive_days")),
                    period_end_unix: json_i64(item.get("period_end_unix")),
                })
                .collect()
        })
        .unwrap_or_default();
    VipMyPage {
        code,
        message,
        is_vip,
        vip_type,
        list,
    }
}

fn vip_catalog_from(data: Vec<VipGroupRaw>) -> Vec<VipPrivilegeGroup> {
    data.into_iter()
        .map(|group| VipPrivilegeGroup {
            id: group.id,
            name: group.name,
            items: group
                .child_privileges
                .into_iter()
                .map(|item| VipPrivilegeItem {
                    id: item.id,
                    name: strip_html(&item.name),
                    desc: strip_html(&item.desc),
                    icon: normalize_url(&item.icon_url),
                    link: item.link,
                    hot: item.hot_type != 0,
                    new: item.new_type != 0,
                })
                .collect(),
        })
        .filter(|group: &VipPrivilegeGroup| !group.name.trim().is_empty() || !group.items.is_empty())
        .collect()
}

fn vip_task_page_from(value: &serde_json::Value) -> VipTaskPage {
    let code = value
        .get("code")
        .map(|code| json_i64(Some(code)))
        .unwrap_or(-1);
    let message = json_str(value.get("message"));
    let data = value.get("data");
    let vip_info = data.and_then(|data| data.get("vip_info"));
    let point_info = data.and_then(|data| data.get("point_info"));
    let task = data.and_then(|data| data.get("task"));
    let tasks = task
        .and_then(|task| task.get("task_item"))
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| VipTask {
                    code: json_str(item.get("task_code")),
                    state: json_i64(item.get("state")),
                    title: strip_html(&json_str(item.get("title"))),
                    icon: normalize_url(&json_str(item.get("icon"))),
                    subtitle: strip_html(&json_str(item.get("subtitle"))),
                    explain: strip_html(&json_str(item.get("explain"))),
                    link: json_str(item.get("link")),
                    vip_limit: json_bool(item.get("vip_limit")),
                    complete_times: json_i64(item.get("complete_times")),
                    max_times: json_i64(item.get("max_times")),
                })
                .collect()
        })
        .unwrap_or_default();
    VipTaskPage {
        code,
        message,
        is_vip: json_i64(vip_info.and_then(|info| info.get("status"))) == 1,
        vip_type: json_i64(vip_info.and_then(|info| info.get("type"))),
        vip_due_date: json_i64(vip_info.and_then(|info| info.get("due_date"))),
        point: json_i64(point_info.and_then(|info| info.get("point"))),
        signed: json_bool(task.and_then(|task| task.get("signed"))),
        sign_score: json_i64(task.and_then(|task| task.get("score"))),
        task_count: json_i64(task.and_then(|task| task.get("task_count"))),
        tasks,
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

/// Fold one `folder/created/list-all` payload into picker rows: EVERY folder
/// (the default one and empty ones too), and each row's `fav_state` kept as
/// `Option<bool>` so "not in it" and "the payload did not say" stay distinct.
fn fav_folder_states_from(data: Option<FavFoldersData>) -> Vec<FavFolderState> {
    let Some(data) = data else {
        return Vec::new();
    };
    data.list
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.id > 0)
        .map(|item| FavFolderState {
            id: item.id,
            name: item.title,
            count: item.media_count,
            member: item.fav_state.map(|state| state != 0),
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

/// `<label>（code=<code>）` with the API message appended when there is one.
pub(crate) fn fav_error(label: &str, code: i64, message: &str) -> anyhow::Error {
    // Name the states the user can act on, in Chinese, before the endpoint's own
    // text: 未登录 / 风控 / 权限. Everything else keeps the API message.
    let hint = match code {
        -101 => Some("未登录"),
        -352 | -412 => Some("触发风控，请稍后再试"),
        -403 => Some("没有权限"),
        _ => None,
    };
    let message = message.trim();
    let mut text = String::from(label);
    if let Some(hint) = hint {
        text.push('（');
        text.push_str(hint);
        text.push('）');
    }
    if !message.is_empty() {
        text.push('：');
        text.push_str(message);
    }
    text.push_str(&format!("（code={code}）"));
    anyhow!("{text}")
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
        liked: raw.action != 0,
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

pub(crate) fn card_from(item: CardItem) -> VideoCard {
    VideoCard {
        bvid: item.bvid,
        title: item.title,
        pic: normalize_url(&item.pic),
        owner: item.owner.map(|owner| owner.name).unwrap_or_default(),
        view: item.stat.as_ref().map(|stat| stat.view).unwrap_or(0),
        duration: item.duration,
    }
}

pub(crate) fn square_to_hot(item: SquareItem) -> HotSearch {
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
mod message_pagination_tests {
    use super::{
        comment_like_body, dm_page_from, msg_cursor_params, msg_page_from, DmMessagesResp,
        MsgCursor, MsgFeedResp, MsgKind, VipCatalogResp, VipTaskPage, VIP_POINT_URL,
        VIP_PRIVILEGE_CATALOG_URL,
    };

    /// Captured 回复我的 shape: `data.items` + `data.cursor{id,time,is_end}`.
    const REPLY_PAGE: &str = r#"{
        "code": 0,
        "message": "0",
        "data": {
            "cursor": { "id": 2412345678, "time": 1700000000, "is_end": false },
            "items": [
                {
                    "id": 2412345678,
                    "user": { "mid": 42, "nickname": "小A",
                              "avatar": "//i0.hdslb.com/bfs/face/a.jpg" },
                    "item": { "business": "视频", "title": "标题", "source_content": "原评论" },
                    "counts": 2,
                    "reply_time": 1700000000
                }
            ]
        }
    }"#;

    /// @我的 names its stamp `at_time`; the cursor is still `{id,time}`.
    const AT_PAGE: &str = r#"{
        "code": 0,
        "data": {
            "cursor": { "id": 99, "time": 1699990000, "is_end": true },
            "items": [
                {
                    "id": 99,
                    "user": { "nickname": "小B", "avatar": "https://i0.hdslb.com/b.jpg" },
                    "item": { "business": "动态", "source_content": "@了你" },
                    "at_time": 1699990000
                }
            ]
        }
    }"#;

    /// 收到的赞: `data.total.items` + `data.total.cursor` and a separate
    /// `data.latest` strip.
    const LIKE_PAGE: &str = r#"{
        "code": 0,
        "data": {
            "latest": {
                "items": [
                    { "id": 7, "users": [{ "nickname": "最新赞" }],
                      "item": { "title": "最新" }, "like_time": 1700000100 }
                ]
            },
            "total": {
                "items": [
                    {
                        "id": 8,
                        "users": [
                            { "nickname": "小C", "avatar": "//i0.hdslb.com/c.jpg" },
                            { "nickname": "小D" }
                        ],
                        "item": { "title": "视频标题", "target_reply_content": "被赞的评论" },
                        "like_time": 1700000001,
                        "notice_state": 0
                    }
                ],
                "cursor": { "id": 8, "time": 1700000001, "is_end": false }
            }
        }
    }"#;

    const DM_PAGE: &str = r#"{
        "code": 0,
        "message": "0",
        "data": {
            "messages": [
                { "sender_uid": 2, "content": "{\"text\":\"新消息\"}",
                  "timestamp": 200, "msg_seqno": 20, "msg_type": 1, "msg_status": 0 },
                { "sender_uid": 1, "content": "{\"text\":\"旧消息\"}",
                  "timestamp": 100, "msg_seqno": 10, "msg_type": 1, "msg_status": 0 }
            ],
            "has_more": 1,
            "min_seqno": 10,
            "max_seqno": 20
        }
    }"#;

    #[test]
    fn reply_page_keeps_the_forward_cursor_and_time() {
        let resp: MsgFeedResp = serde_json::from_str(REPLY_PAGE).expect("fixture");
        assert_eq!(resp.code, 0);
        let page = msg_page_from(MsgKind::Reply, resp.data.unwrap_or_default());
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, 2412345678);
        assert_eq!(page.items[0].nickname, "小A");
        assert_eq!(page.items[0].avatar, "https://i0.hdslb.com/bfs/face/a.jpg");
        assert_eq!(page.items[0].time, 1700000000);
        assert_eq!(page.items[0].count, 2);
        assert_eq!(
            page.cursor,
            MsgCursor {
                id: 2412345678,
                time: 1700000000
            }
        );
        assert!(!page.is_end);
        assert!(page.has_more());
    }

    #[test]
    fn at_page_reads_at_time_and_ends_on_the_server_flag() {
        let resp: MsgFeedResp = serde_json::from_str(AT_PAGE).expect("fixture");
        let page = msg_page_from(MsgKind::At, resp.data.unwrap_or_default());
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].time, 1699990000);
        assert_eq!(page.items[0].detail, "@了你");
        assert!(page.is_end);
        assert!(!page.has_more());
    }

    #[test]
    fn like_page_reads_total_items_cursor_and_multiple_users() {
        let resp: MsgFeedResp = serde_json::from_str(LIKE_PAGE).expect("fixture");
        let page = msg_page_from(MsgKind::Like, resp.data.unwrap_or_default());
        assert_eq!(page.items.len(), 1, "latest must not duplicate total");
        assert_eq!(page.items[0].id, 8);
        assert_eq!(page.items[0].nickname, "小C");
        assert_eq!(page.items[0].avatar, "https://i0.hdslb.com/c.jpg");
        assert_eq!(page.items[0].title, "视频标题");
        assert_eq!(page.items[0].detail, "被赞的评论");
        assert_eq!(page.items[0].time, 1700000001);
        assert_eq!(
            page.cursor,
            MsgCursor {
                id: 8,
                time: 1700000001
            }
        );
        assert!(page.has_more());
    }

    #[test]
    fn like_page_falls_back_to_latest_when_total_is_empty() {
        let json = r#"{
            "code": 0,
            "data": {
                "latest": { "items": [
                    { "id": 1, "users": [{ "nickname": "A" }],
                      "item": { "title": "T" }, "like_time": 12 }
                ] }
            }
        }"#;
        let resp: MsgFeedResp = serde_json::from_str(json).expect("fixture");
        let page = msg_page_from(MsgKind::Like, resp.data.unwrap_or_default());
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].nickname, "A");
        assert_eq!(page.items[0].time, 12);
        assert!(page.is_end, "no cursor means no next page");
    }

    #[test]
    fn cursor_params_name_the_kind_specific_time_field() {
        let cursor = Some(MsgCursor { id: 11, time: 22 });
        assert_eq!(
            msg_cursor_params(MsgKind::Reply, cursor),
            vec![("id", "11".to_string()), ("reply_time", "22".to_string())]
        );
        assert_eq!(
            msg_cursor_params(MsgKind::At, cursor),
            vec![("id", "11".to_string()), ("at_time", "22".to_string())]
        );
        assert_eq!(
            msg_cursor_params(MsgKind::Like, cursor),
            vec![("id", "11".to_string()), ("like_time", "22".to_string())]
        );
        assert!(msg_cursor_params(MsgKind::Reply, None).is_empty());
        assert!(msg_cursor_params(
            MsgKind::Reply,
            Some(MsgCursor { id: 0, time: 22 })
        )
        .is_empty());
    }

    #[test]
    fn dm_page_is_sorted_oldest_first_and_carries_the_older_cursor() {
        let resp: DmMessagesResp = serde_json::from_str(DM_PAGE).expect("fixture");
        let page = dm_page_from(resp);
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].seqno, 10);
        assert_eq!(page.messages[0].text, "旧消息");
        assert_eq!(page.messages[1].seqno, 20);
        assert_eq!(page.messages[1].text, "新消息");
        assert_eq!(page.min_seqno, 10);
        assert_eq!(page.max_seqno, 20);
        assert!(page.has_more);
        assert_eq!(page.messages[1].sender_uid, 2);
        assert_eq!(page.messages[1].msg_type, 1);
    }

    #[test]
    fn dm_empty_conversation_has_no_more_and_sentinel_seqnos() {
        let resp: DmMessagesResp = serde_json::from_str(
            r#"{"code":0,"data":{"messages":null,"has_more":0,
                "min_seqno":18446744073709551615,"max_seqno":0}}"#,
        )
        .expect("fixture");
        let page = dm_page_from(resp);
        assert!(page.messages.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.max_seqno, 0);
    }

    #[test]
    fn comment_like_body_pins_the_endpoint_fields() {
        assert_eq!(
            comment_like_body(123, 456, true, "csrf value"),
            "oid=123&type=1&rpid=456&action=1&csrf=csrf%20value"
        );
        assert_eq!(
            comment_like_body(123, 456, false, "tok"),
            "oid=123&type=1&rpid=456&action=2&csrf=tok"
        );
    }

    #[test]
    fn vip_endpoints_are_the_probed_ones() {
        // `x/v2/vip/task/list` and the `x/v2/vip/*` aliases answer HTTP 404;
        // these are the routes the live site actually serves.
        assert_eq!(
            VIP_PRIVILEGE_CATALOG_URL,
            "https://api.bilibili.com/x/vip/privilege/list"
        );
        assert_eq!(
            VIP_POINT_URL,
            "https://api.bilibili.com/x/vip_point/homepage/combine"
        );
    }

    #[test]
    fn vip_task_page_is_typed_or_reports_the_server_answer() {
        let json = r#"{
            "code": 0,
            "message": "0",
            "data": {
                "vip_info": { "type": 2, "status": 1, "due_date": 1800000000000 },
                "point_info": { "point": 120 },
                "task": {
                    "task_count": 2,
                    "signed": false,
                    "score": 5,
                    "task_item": [
                        { "task_code": "ogvwatch", "state": 1, "title": "观看正片",
                          "subtitle": "<span class=\"active\">+10 大积分</span>", "explain": "看一集",
                          "link": "https://www.bilibili.com/", "vip_limit": 1,
                          "complete_times": 0, "max_times": 1 },
                        { "task_code": "privilege", "state": 3, "title": "浏览权益",
                          "complete_times": 1, "max_times": 1 }
                    ]
                }
            }
        }"#;
        let value: serde_json::Value = serde_json::from_str(json).expect("fixture");
        let page = super::vip_task_page_from(&value);
        assert!(page.is_ok());
        assert!(page.is_vip);
        assert_eq!(page.vip_type, 2);
        assert_eq!(page.vip_due_date, 1800000000000);
        assert_eq!(page.point, 120);
        assert_eq!(page.task_count, 2);
        assert!(!page.signed);
        assert_eq!(page.sign_score, 5);
        assert_eq!(page.tasks.len(), 2);
        assert_eq!(page.tasks[0].code, "ogvwatch");
        assert_eq!(page.tasks[0].state_label(), "未完成");
        assert_eq!(page.tasks[0].progress(), "0/1");
        assert_eq!(
            page.tasks[0].subtitle, "+10 大积分",
            "server HTML must not reach the native text renderer"
        );
        assert!(page.tasks[0].vip_limit);
        assert!(page.tasks[1].done());
        assert_eq!(page.tasks[1].progress(), "1/1");

        let refused: serde_json::Value =
            serde_json::from_str(r#"{"code":-101,"message":"账号未登录"}"#).expect("fixture");
        let page: VipTaskPage = super::vip_task_page_from(&refused);
        assert_eq!(page.code, -101);
        assert_eq!(page.message, "账号未登录");
        assert!(!page.is_ok());
        assert!(page.tasks.is_empty());
    }

    #[test]
    fn vip_catalog_groups_keep_hot_and_new_flags() {
        let json = r#"{
            "code": 0,
            "message": "OK",
            "data": [
                {
                    "id": 3,
                    "name": "视听特权",
                    "child_privileges": [
                        { "id": 3, "name": "超清看", "desc": "更清晰",
                          "icon_url": "//i0.hdslb.com/vip/a.png",
                          "link": "https://big.bilibili.com/",
                          "hot_type": 1, "new_type": 0 }
                    ]
                }
            ]
        }"#;
        let resp: VipCatalogResp = serde_json::from_str(json).expect("fixture");
        assert_eq!(resp.code, 0);
        let groups = super::vip_catalog_from(resp.data.unwrap_or_default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "视听特权");
        assert_eq!(groups[0].items.len(), 1);
        assert_eq!(groups[0].items[0].name, "超清看");
        assert_eq!(groups[0].items[0].icon, "https://i0.hdslb.com/vip/a.png");
        assert!(groups[0].items[0].hot);
        assert!(!groups[0].items[0].new);
    }

    #[test]
    fn vip_my_page_parses_the_monthly_privilege_rows() {
        let json = r#"{
            "code": 0,
            "message": "0",
            "data": {
                "is_vip": true,
                "vip_status": 1,
                "vip_type": 2,
                "list": [
                    { "type": 1, "state": 0, "expire_time": 1800000000,
                      "vip_type": 2, "next_receive_days": 12, "period_end_unix": 1800000000 },
                    { "type": 9, "state": 2, "expire_time": 1800000000 },
                    { "type": 8, "state": 1, "expire_time": 1800000000 }
                ]
            }
        }"#;
        let value: serde_json::Value = serde_json::from_str(json).expect("fixture");
        let page = super::vip_my_page_from(&value);
        assert_eq!(page.code, 0);
        assert!(page.is_vip);
        assert_eq!(page.vip_type, 2);
        assert_eq!(page.list.len(), 3);
        assert_eq!(page.list[0].label(), "B币券");
        assert_eq!(page.list[0].state_label(), "未兑换");
        assert!(!page.list[1].received());
        assert!(page.list[1].incomplete());
        assert_eq!(page.list[1].label(), "每日经验");
        assert!(page.list[2].received());
        assert_eq!(page.list[2].label(), "每日礼盒");
        assert_eq!(
            page.summary(),
            "大会员 · 特权 3 项 · 可领取 1",
            "state 2 (未完成) is not counted as claimable"
        );
    }

    #[test]
    fn vip_my_non_member_summary_does_not_claim_coupons_are_claimable() {
        let mut page = super::VipMyPage {
            code: 0,
            is_vip: false,
            list: vec![
                super::VipMyPrivilege {
                    kind: 1,
                    state: 0,
                    ..Default::default()
                },
                super::VipMyPrivilege {
                    kind: 2,
                    state: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(page.summary(), "非大会员 · 特权 2 项");
        page.is_vip = true;
        assert_eq!(page.summary(), "大会员 · 特权 2 项 · 可领取 2");
    }

    #[test]
    fn vip_my_refusal_keeps_code_and_message() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"code":-101,"message":"账号未登录","data":null}"#)
                .expect("fixture");
        let page = super::vip_my_page_from(&value);
        assert_eq!(page.code, -101);
        assert_eq!(page.message, "账号未登录");
        assert!(!page.is_vip);
        assert!(page.summary().is_empty());
    }

    /// Read-only live probe for the new data layer. Runs only with
    /// `BILI_CORE2_PROBE=1` (and a real `O11A1_HOME`); every line goes through
    /// `BILI_DEBUG_LOG`. It performs no write action.
    #[test]
    fn core2_live_probe() {
        if std::env::var("BILI_CORE2_PROBE").as_deref() != Ok("1") {
            return;
        }
        let Some(runtime) = super::shared_runtime() else {
            eprintln!("no runtime");
            return;
        };
        runtime.block_on(async {
            let Some(account) = crate::accounts::load() else {
                eprintln!("no account");
                return;
            };
            let token = crate::accounts::to_token(&account);
            let session = match super::Session::from_token(&token).await {
                Ok(session) => session,
                Err(error) => {
                    eprintln!("session: {error:#}");
                    return;
                }
            };
            for kind in [MsgKind::Reply, MsgKind::At, MsgKind::Like] {
                match session.msg_list(kind).await {
                    Ok(page) => {
                        crate::debug_log(&format!(
                            "probe msg_list {:?} n={} cursor={} is_end={}",
                            kind,
                            page.items.len(),
                            page.cursor.id,
                            page.is_end
                        ));
                        if let Some(first) = page.items.first() {
                            crate::debug_log(&format!(
                                "probe msg_first {:?} id={} nick={:?} time={} title={:?}",
                                kind, first.id, first.nickname, first.time, first.title
                            ));
                        }
                        if page.has_more() {
                            match session.msg_page(kind, Some(page.cursor)).await {
                                Ok(next) => crate::debug_log(&format!(
                                    "probe msg_next {:?} n={} cursor={} is_end={}",
                                    kind,
                                    next.items.len(),
                                    next.cursor.id,
                                    next.is_end
                                )),
                                Err(error) => crate::debug_log(&format!(
                                    "probe msg_next {:?} err={error:#}",
                                    kind
                                )),
                            }
                        }
                    }
                    Err(error) => {
                        crate::debug_log(&format!("probe msg_list {:?} err={error:#}", kind))
                    }
                }
            }
            match session.dm_sessions().await {
                Ok(sessions) => {
                    crate::debug_log(&format!("probe dm_sessions n={}", sessions.len()));
                    let mut older_checked = false;
                    for first in sessions.iter().take(20) {
                        match session.dm_messages(first.talker_id).await {
                            Ok(page) => {
                                crate::debug_log(&format!(
                                    "probe dm_messages talker={} n={} has_more={} min={} max={}",
                                    first.talker_id,
                                    page.messages.len(),
                                    page.has_more,
                                    page.min_seqno,
                                    page.max_seqno
                                ));
                                if page.messages.len() > 1 && !older_checked {
                                    older_checked = true;
                                    // `end_seqno` is exclusive: asking with the
                                    // newest seqno must return the older rest.
                                    match session
                                        .dm_messages_before(first.talker_id, page.max_seqno)
                                        .await
                                    {
                                        Ok(older) => crate::debug_log(&format!(
                                            "probe dm_end_seqno talker={} bound={} n={} has_more={} min={} max={}",
                                            first.talker_id,
                                            page.max_seqno,
                                            older.messages.len(),
                                            older.has_more,
                                            older.min_seqno,
                                            older.max_seqno
                                        )),
                                        Err(error) => crate::debug_log(&format!(
                                            "probe dm_end_seqno err={error:#}"
                                        )),
                                    }
                                }
                            }
                            Err(error) => crate::debug_log(&format!(
                                "probe dm_messages talker={} err={error:#}",
                                first.talker_id
                            )),
                        }
                    }
                }
                Err(error) => crate::debug_log(&format!("probe dm_sessions err={error:#}")),
            }
            match session.vip_privilege_my_page().await {
                Ok(page) => {
                    crate::debug_log(&format!(
                        "probe vip_my code={} is_vip={} vip_type={} list={}",
                        page.code,
                        page.is_vip,
                        page.vip_type,
                        page.list.len()
                    ));
                    for item in page.list.iter().take(6) {
                        crate::debug_log(&format!(
                            "probe vip_my_item kind={} label={:?} state={} expire={} next={}",
                            item.kind,
                            item.label(),
                            item.state,
                            item.expire_time,
                            item.next_receive_days
                        ));
                    }
                }
                Err(error) => crate::debug_log(&format!("probe vip_my err={error:#}")),
            }
            // Raw dump of the first fields only, so the typed view can be
            // checked against what the server really names its rows.
            if let Ok(response) = session
                .client
                .get("https://api.bilibili.com/x/vip/privilege/my")
                .send()
                .await
            {
                if let Ok(text) = response.text().await {
                    let preview: String = text.chars().take(1400).collect();
                    crate::debug_log(&format!("probe vip_my_raw {preview}"));
                }
            }
            match session.vip_privilege_catalog().await {
                Ok(groups) => {
                    crate::debug_log(&format!(
                        "probe vip_catalog groups={} items={}",
                        groups.len(),
                        groups.iter().map(|group| group.items.len()).sum::<usize>()
                    ));
                    for group in groups.iter().take(4) {
                        crate::debug_log(&format!(
                            "probe vip_group {:?} n={}",
                            group.name,
                            group.items.len()
                        ));
                    }
                }
                Err(error) => crate::debug_log(&format!("probe vip_catalog err={error:#}")),
            }
            match session.vip_point_tasks().await {
                Ok(page) => {
                    crate::debug_log(&format!(
                        "probe vip_tasks code={} is_vip={} tasks={} count={} point={} signed={}",
                        page.code,
                        page.is_vip,
                        page.tasks.len(),
                        page.task_count,
                        page.point,
                        page.signed
                    ));
                    for task in page.tasks.iter().take(6) {
                        crate::debug_log(&format!(
                            "probe vip_task code={:?} title={:?} state={} progress={:?}",
                            task.code,
                            task.title,
                            task.state,
                            task.progress()
                        ));
                    }
                }
                Err(error) => crate::debug_log(&format!("probe vip_tasks err={error:#}")),
            }
            match session.pgc_playurl(4823121, 40005011296).await {
                Ok(play) => crate::debug_log(&format!(
                    "probe pgc_playurl code={} qn={} durl={} single={} dash={}/{}",
                    play.code,
                    play.quality,
                    play.durl.len(),
                    play.single_url().is_some(),
                    play.video.len(),
                    play.audio.len()
                )),
                Err(error) => crate::debug_log(&format!("probe pgc_playurl err={error:#}")),
            }
            match session.pgc_playurl_dash(4823121, 40005011296).await {
                Ok(play) => crate::debug_log(&format!(
                    "probe pgc_dash code={} qn={} durl={} single={} dash={}/{}",
                    play.code,
                    play.quality,
                    play.durl.len(),
                    play.single_url().is_some(),
                    play.video.len(),
                    play.audio.len()
                )),
                Err(error) => crate::debug_log(&format!("probe pgc_dash err={error:#}")),
            }
            match session.related("BV1GJ411x7h7").await {
                Ok(items) => crate::debug_log(&format!("probe related n={}", items.len())),
                Err(error) => crate::debug_log(&format!("probe related err={error:#}")),
            }
        });
    }
}

