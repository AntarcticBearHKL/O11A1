//!  会话与 HTTP 基础设施：client/cookie/jar、WBI 签名、wbi 调用、通用反序列化工具与接口常量。

use super::*;

/// A normal desktop Chrome user agent. `pub(crate)` so the offline-download
/// worker can attach the exact same UA to its ffmpeg inputs.
pub(crate) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

const QR_GEN_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";

const QR_POLL_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";

pub(super) const NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";

pub(super) const RELATION_URL: &str = "https://api.bilibili.com/x/relation/stat";

pub(super) const MYINFO_URL: &str = "https://api.bilibili.com/x/space/myinfo";

const SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";

const HOME_URL: &str = "https://www.bilibili.com/";

const TICKET_URL: &str =
    "https://api.bilibili.com/bapis/bilibili.api.ticket.v1.Ticket/GenWebTicket";

/// HMAC key Bilibili's web client uses to sign `GenWebTicket` requests.
const TICKET_HMAC_KEY: &str = "XgwSnGZ1p";

pub(super) const FEED_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/index/top/feed/rcmd";

pub(super) const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/search/type";

/// `Referer` the 番剧 / 影视 / 直播 search indexes expect. The policy is applied
/// per `search_type` by the endpoint: without it these three answer a
/// `v_voucher` risk-control body, while the video/user index does not care.
pub(super) const SEARCH_REFERER: &str = "https://search.bilibili.com/";

pub(super) const VIEW_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/view";

/// 进度条悬停预览的雪碧图 (storyboard) 描述，B站 web 播放器的同一个端点。
/// 需要登录 cookie（未登录 code≠0，调用方据此降级为“只显示时间”）。
pub(super) const VIDEOSHOT_URL: &str = "https://api.bilibili.com/x/player/videoshot";

pub(super) const PLAYURL_URL: &str = "https://api.bilibili.com/x/player/wbi/playurl";

/// 大会员卡券/特权状态 (`GET`, SESSDATA).
pub(super) const VIP_PRIVILEGE_URL: &str = "https://api.bilibili.com/x/vip/privilege/my";

/// 大会员每日经验 (`POST` form `csrf`).
pub(super) const VIP_EXPERIENCE_URL: &str = "https://api.bilibili.com/x/vip/experience/add";

/// 兑换大会员卡券 (`POST` form `type` + `csrf`).
pub(super) const VIP_RECEIVE_URL: &str = "https://api.bilibili.com/x/vip/privilege/receive";

/// 大积分签到/会员任务 (`POST` form `csrf`).
pub(super) const PGC_SIGN_URL: &str = "https://api.bilibili.com/pgc/activity/score/task/sign";

pub(super) const DM_SEG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/seg.so";

/// Available history dates for one `cid` (`?type=1&oid=<cid>&month=YYYY-MM`).
pub(super) const DM_HISTORY_INDEX_URL: &str = "https://api.bilibili.com/x/v2/dm/history/index";

/// One day's historical danmaku as XML (`?type=1&oid=<cid>&date=YYYY-MM-DD`).
pub(super) const DM_HISTORY_URL: &str = "https://api.bilibili.com/x/v2/dm/history";

/// One day's historical danmaku as protobuf (same `DmSegMobileReply` shape as
/// [`DM_SEG_URL`]). Used as a fallback because the XML endpoint above now answers
/// a `state=2` placeholder without any `<d>` entries.
pub(super) const DM_HISTORY_SEG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/history/seg.so";

/// Send a video danmaku. WBI-signed; the message is in the form body.
pub(super) const DM_POST_URL: &str = "https://api.bilibili.com/x/v2/dm/post";

/// Report one danmaku.
pub(super) const DM_REPORT_URL: &str = "https://api.bilibili.com/x/dm/report/add";

/// Like (`op=1`) / un-like (`op=2`) one danmaku.
pub(super) const DM_THUMBUP_URL: &str = "https://api.bilibili.com/x/v2/dm/thumbup/add";

/// Server-side AI danmaku filter level. POST form with `ai_switch` + `csrf`.
pub(super) const DM_AI_CONFIG_URL: &str = "https://api.bilibili.com/x/v2/dm/web/config";

pub(super) const LIKE_HAS_URL: &str = "https://api.bilibili.com/x/web-interface/archive/has/like";

pub(super) const LIKE_URL: &str = "https://api.bilibili.com/x/web-interface/archive/like";

/// Playback heartbeat: the call that writes the official 历史记录 entry.
pub(super) const HEARTBEAT_URL: &str = "https://api.bilibili.com/x/click-interface/web/heartbeat";

pub(super) const COIN_STATE_URL: &str = "https://api.bilibili.com/x/web-interface/archive/coins";

pub(super) const COIN_ADD_URL: &str = "https://api.bilibili.com/x/web-interface/coin/add";

pub(super) const FAV_FOLDERS_URL: &str = "https://api.bilibili.com/x/v3/fav/folder/created/list-all";

pub(super) const FAV_FOLDER_ADD_URL: &str = "https://api.bilibili.com/x/v3/fav/folder/add";

pub(super) const FAV_FOLDER_DEL_URL: &str = "https://api.bilibili.com/x/v3/fav/folder/del";

pub(super) const FAV_FOLDER_EDIT_URL: &str = "https://api.bilibili.com/x/v3/fav/folder/edit";

pub(super) const FAV_BATCH_DEL_URL: &str = "https://api.bilibili.com/x/v3/fav/resource/batch-del";

pub(super) const FAV_FAVOURED_URL: &str = "https://api.bilibili.com/x/v2/fav/video/favoured";

pub(super) const FAV_DEAL_URL: &str = "https://api.bilibili.com/x/v3/fav/resource/deal";

pub(super) const FAV_LIST_URL: &str = "https://api.bilibili.com/x/v3/fav/resource/list";

/// 收藏推荐: one video's related list (`data[]`, archive-shaped entries).
pub(super) const RELATED_URL: &str = "https://api.bilibili.com/x/web-interface/archive/related";

pub(super) const HISTORY_URL: &str = "https://api.bilibili.com/x/web-interface/history/cursor";

pub(super) const HISTORY_DELETE_URL: &str = "https://api.bilibili.com/x/v2/history/delete";

pub(super) const HISTORY_CLEAR_URL: &str = "https://api.bilibili.com/x/v2/history/clear";

pub(super) const TOVIEW_URL: &str = "https://api.bilibili.com/x/v2/history/toview";

pub(super) const TOVIEW_DEL_URL: &str = "https://api.bilibili.com/x/v2/history/toview/del";

pub(super) const REGION_URL: &str = "https://api.bilibili.com/x/web-interface/dynamic/region";

pub(super) const REGION_NEWLIST_URL: &str = "https://api.bilibili.com/x/web-interface/newlist";
// -- 番剧 / PGC ---------------------------------------------------------------

/// 每日放送 (7-day timeline). `types=1,4,2,3,5,7` covers 番剧/国创/电影/纪录片/
/// 电视剧/综艺; `before=3&after=3` yields exactly 7 days around today.
pub(super) const PGC_TIMELINE_URL: &str = "https://api.bilibili.com/pgc/web/timeline";

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
pub(super) const PGC_INDEX_URL: &str = "https://api.bilibili.com/pgc/season/index/result";

/// 排行榜 (99 rows, no paging).
pub(super) const PGC_RANK_URL: &str = "https://api.bilibili.com/pgc/web/rank/list";

/// One season's detail: episodes + section groups + evaluate/rating/areas.
pub(super) const PGC_SEASON_URL: &str = "https://api.bilibili.com/pgc/view/web/season";

/// PGC playurl. Only used as a REGION GATE (its non-zero code names the
/// reason); playback itself goes through [`PLAYURL_URL`]'s unsigned fallback.
pub(super) const PGC_PLAYURL_URL: &str = "https://api.bilibili.com/pgc/player/web/playurl";

/// Unsigned playurl. PGC episodes have a real `bvid`+`cid`, so the plain
/// `x/player/playurl` answers for them where the WBI variant is risk-controlled.
pub(super) const PLAYURL_PLAIN_URL: &str = "https://api.bilibili.com/x/player/playurl";

pub(super) const RANKING_URL: &str = "https://api.bilibili.com/x/web-interface/ranking/v2";

pub(super) const SUGGEST_URL: &str = "https://s.search.bilibili.com/main/suggest";

pub(super) const HOTWORD_URL: &str = "https://s.search.bilibili.com/main/hotword";

pub(super) const SQUARE_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/search/square";

/// UP主空间 info (WBI). Some accounts answer `-352` unless the request carries
/// the space page as `Referer`; [`Session::space_info`] retries once with it.
pub(super) const SPACE_INFO_URL: &str = "https://api.bilibili.com/x/space/wbi/acc/info";

/// UP主投稿列表 (WBI).
pub(super) const SPACE_ARC_URL: &str = "https://api.bilibili.com/x/space/wbi/arc/search";

/// UP主 card header (`data.card` + `data.archive_count`). Plain GET, no WBI.
pub(super) const UP_CARD_URL: &str = "https://api.bilibili.com/x/web-interface/card";

/// Follow state for one user (`data.attribute`), distinct from the follower
/// counts endpoint `x/relation/stat`.
pub(super) const RELATION_STATE_URL: &str = "https://api.bilibili.com/x/relation";

/// Follow / unfollow write.
pub(super) const RELATION_MODIFY_URL: &str = "https://api.bilibili.com/x/relation/modify";

/// Logout. The CSRF field is named `biliCSRF`, not `csrf`.
const LOGOUT_URL: &str = "https://passport.bilibili.com/login/exit/v2";

/// `ranking/v2` is rejected (risk control `-352`) unless the request carries the
/// ranking page as `Referer`; the shared client's default `bilibili.com/` fails.
pub(super) const RANK_REFERER: &str = "https://www.bilibili.com/v/popular/rank/all";

// -- 视频评论 (comments) ------------------------------------------------------

/// 评论主列表 (WBI-signed): `oid=<aid>&type=1&mode=<3 热门 | 2 时间>`, with a
/// `pagination_str={"offset":"<cursor>"}` cursor (empty on the first page).
pub(super) const COMMENT_MAIN_URL: &str = "https://api.bilibili.com/x/v2/reply/wbi/main";

/// 一条评论的楼中楼 (dialog): `oid=<aid>&type=1&root=<rpid>&pn=<page>&ps=20`.
pub(super) const COMMENT_REPLY_URL: &str = "https://api.bilibili.com/x/v2/reply/reply";

// -- 动态流 / 消息中心 / 私信 / 关注粉丝 ---------------------------------------

/// 动态流 (web `feed/all`): `data.items[]` + `data.offset` / `data.has_more`.
pub(super) const DYNAMIC_FEED_URL: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/all";

/// 动态点赞 write. POST form `dyn_id_str` + `up` + `csrf`.
pub(super) const DYNAMIC_THUMB_URL: &str = "https://api.bilibili.com/x/dynamic/feed/dyn/thumb";

/// Per-category unread counts for the 消息 badge.
pub(super) const MSG_UNREAD_URL: &str = "https://api.bilibili.com/x/msgfeed/unread";

/// 回复我的 / @我的 / 收到的赞 lists (same payload shape).
pub(super) const MSG_REPLY_URL: &str = "https://api.bilibili.com/x/msgfeed/reply";

pub(super) const MSG_AT_URL: &str = "https://api.bilibili.com/x/msgfeed/at";

pub(super) const MSG_LIKE_URL: &str = "https://api.bilibili.com/x/msgfeed/like";

/// 私信会话列表 (`session_svr`, READ ONLY).
pub(super) const DM_SESSIONS_URL: &str =
    "https://api.vc.bilibili.com/session_svr/v1/session_svr/get_sessions";

/// 私信内容 (`svr_sync`, READ ONLY).
pub(super) const DM_MESSAGES_URL: &str =
    "https://api.vc.bilibili.com/svr_sync/v1/svr_sync/fetch_session_msgs";

/// 关注 / 粉丝 lists.
pub(super) const RELATION_FOLLOWINGS_URL: &str = "https://api.bilibili.com/x/relation/followings";

pub(super) const RELATION_FANS_URL: &str = "https://api.bilibili.com/x/relation/fans";

/// Sessions shown on the 私信 tab (each needs one `space_info` call for its name).
pub const DM_SESSION_LIMIT: usize = 20;

/// WBI keys are stable for a long time; refresh at most this often.
const WBI_TTL: Duration = Duration::from_secs(30 * 60);

/// Default number of feed pages (~30 items each) to request.
pub(super) const DEFAULT_FEED_PAGES: usize = 3;

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
    crate::paths::root().join("token.json")
}

pub fn load_token() -> Option<Token> {
    let data = std::fs::read_to_string(token_path()).ok()?;
    serde_json::from_str::<Token>(&data).ok()
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub struct Session {
    pub(super) client: reqwest::Client,
    pub(super) raw_client: reqwest::Client,
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Add one cookie to the shared jar for every `*.bilibili.com` host.
    fn set_cookie(&self, name: &str, value: &str) {
        if let Ok(api_url) = Url::parse("https://api.bilibili.com/") {
            self.jar.add_cookie_str(
                &format!("{name}={value}; Domain=.bilibili.com; Path=/"),
                &api_url,
            );
        }
    }
}

impl Session {
    /// Best-effort: follow the SSO redirect URL from a successful poll so any
    /// extra cookies get jarred. Callers ignore failures.
    pub async fn visit(&self, url: &str) -> Result<()> {
        let _ = self.client.get(url).send().await?;
        Ok(())
    }

    // -- QR login -----------------------------------------------------------
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// The `bili_jct` cookie value used as `csrf` on every write.
    pub(super) fn csrf_cookie(&self) -> Result<String> {
        self.cookie_value("bili_jct")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("缺少 csrf（bili_jct）"))
    }
}

impl Session {
    /// POST an `application/x-www-form-urlencoded` body and unwrap the standard
    /// `{code, message}` envelope, surfacing the API code and HTTP status on
    /// failure.
    pub(super) async fn post_form(&self, url: &str, referer: &str, body: String) -> Result<()> {
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
}

impl Session {
    /// Like [`Session::post_form`] but returns the API `code` and `message`
    /// verbatim instead of failing on a non-zero code, so write endpoints whose
    /// result depends on risk control can report exactly what came back.
    /// `label` names the endpoint in the debug log (the URL may carry `csrf`).
    pub(super) async fn post_form_code(        &self,
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
}

impl Session {
    /// Log the active account out server-side. Callers must confirm the login is
    /// still live first; the CSRF field here is `biliCSRF`, not `csrf`.
    pub async fn logout(&self) -> Result<()> {
        let csrf = self.csrf_cookie()?;
        let body = format!("biliCSRF={}", encode_component(&csrf));
        self.post_form(LOGOUT_URL, "https://www.bilibili.com/", body)
            .await
    }
}

impl Session {
    /// Sign, GET and parse one JSON response with an optional explicit
    /// `Referer`. Retrying with fresh keys is left to the caller via `force`.
    pub(super) async fn wbi_call_referer<T: DeserializeOwned>(
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
        crate::net::note_response_bytes(text.len() as u64);
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        let body = serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))?;
        Ok((status, body))
    }
}

impl Session {
    /// Sign, GET and parse one JSON response. Retrying with fresh keys is left
    /// to the caller via `force`.
    pub(super) async fn wbi_call<T: DeserializeOwned>(
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
        crate::net::note_response_bytes(text.len() as u64);
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        let body = serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))?;
        Ok((status, body))
    }

    // -- Playback URLs ------------------------------------------------------
}

impl Session {
    pub(super) fn store_wbi(&self, img_key: String, sub_key: String) {
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
}

impl Session {
    fn cached_wbi(&self) -> Option<(String, String)> {
        let guard = self.wbi.lock().ok()?;
        let keys = guard.as_ref()?;
        if keys.fetched.elapsed() > WBI_TTL {
            return None;
        }
        Some((keys.img_key.clone(), keys.sub_key.clone()))
    }
}

impl Session {
    /// Return cached WBI keys, fetching them via `nav` when absent, stale or
    /// when `force` is set (used after a signing rejection).
    pub(super) async fn wbi_keys(&self, force: bool) -> Result<(String, String)> {
        if !force {
            if let Some(keys) = self.cached_wbi() {
                return Ok(keys);
            }
        }
        let _ = self.nav().await?;
        self.cached_wbi()
            .ok_or_else(|| anyhow!("未能获取 WBI 签名密钥"))
    }
}

impl Session {
    /// Sign `params` with the current WBI keys.
    pub(super) async fn sign(&self, params: Vec<(String, String)>, force: bool) -> Result<String> {
        let (img_key, sub_key) = self.wbi_keys(force).await?;
        Ok(crate::wbi::enc_wbi(params, &img_key, &sub_key))
    }
}

impl Session {
    pub(super) async fn get_query_json<T: DeserializeOwned>(
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
        crate::net::note_response_bytes(text.len() as u64);
        let cleaned = text.trim_start_matches('\u{feff}').trim();
        serde_json::from_str::<T>(cleaned)
            .map_err(|err| anyhow!("响应解析失败（HTTP {status}）：{err}"))
    }
}

impl Session {
    pub(super) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        crate::net::note_response_bytes(bytes.len() as u64);
        Ok(bytes.to_vec())
    }
}

impl Session {
    /// [`Session::get_bytes`] that also reports the HTTP status, including for
    /// the status that `error_for_status` turns into an error.
    pub(super) async fn get_bytes_status(&self, url: &str) -> (Option<u16>, Result<Vec<u8>>) {
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                crate::debug_log(&format!("covers http err={err} url={url}"));
                return (None, Err(err.into()));
            }
        };
        let status = response.status().as_u16();
        match response.error_for_status() {
            Ok(response) => match response.bytes().await {
                Ok(bytes) => {
                    crate::net::note_response_bytes(bytes.len() as u64);
                    (Some(status), Ok(bytes.to_vec()))
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "covers http body status={status} err={err} url={url}"
                    ));
                    (None, Err(err.into()))
                }
            },
            Err(err) => {
                crate::debug_log(&format!("covers http status={status} url={url}"));
                (Some(status), Err(err.into()))
            }
        }
    }
}

impl Session {
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
}

impl Session {
    /// Read one cookie value by name from the shared jar.
    pub(super) fn cookie_value(&self, name: &str) -> Option<String> {
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
}

impl Session {
    /// The shared HTTP client for raw-bytes endpoints (auto-decompression
    /// disabled), reused by the offline-download worker for parallel range
    /// requests so it inherits the session cookies, referer and UA.
    pub fn raw_client(&self) -> &reqwest::Client {
        &self.raw_client
    }

    /// Headers a native media pipeline must send when it fetches a DASH
    /// segment directly (ffmpeg/ffplay): `User-Agent`, `Referer` and the
    /// session `Cookie` when logged in. The CDN rejects a bare request.
    pub fn media_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("User-Agent".to_string(), UA.to_string()),
            (
                "Referer".to_string(),
                "https://www.bilibili.com/".to_string(),
            ),
        ];
        if let Some(cookie) = self.api_cookie_header() {
            headers.push(("Cookie".to_string(), cookie));
        }
        headers
    }
}

impl Session {
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
pub(crate) async fn decode_rgba_blocking(bytes: Vec<u8>) -> Result<(Vec<u8>, u32, u32)> {
    match tokio::task::spawn_blocking(move || decode_rgba(&bytes)).await {
        Ok(result) => result,
        Err(err) => Err(anyhow!("图片解码任务失败：{err}")),
    }
}

/// Inflate a raw-or-zlib HTTP `deflate` body, trying the zlib wrapper first.
pub(crate) fn inflate_deflate(body: &[u8]) -> Option<Vec<u8>> {
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
pub(crate) fn encode_component(value: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Join numeric ids as the comma-separated `add_media_ids` list the API wants.
pub(crate) fn join_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn extract_wbi_key(url: &str) -> String {    let file = url.rsplit('/').next().unwrap_or("");
    file.split('.').next().unwrap_or("").to_string()
}

pub(crate) fn extract_wbi_keys(data: &NavData) -> Option<(String, String)> {
    let wbi = data.wbi_img.as_ref()?;
    let img_key = extract_wbi_key(&wbi.img_url);
    let sub_key = extract_wbi_key(&wbi.sub_url);
    if img_key.is_empty() || sub_key.is_empty() {
        return None;
    }
    Some((img_key, sub_key))
}

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
pub(super) struct NavResp {
    pub(super) code: i64,
    #[serde(default)]
    pub(super) message: String,
    pub(super) data: Option<NavData>,
}

#[derive(Deserialize)]
pub(crate) struct NavData {
    #[serde(rename = "isLogin", default)]
    pub(super) is_login: bool,
    #[serde(default)]
    pub(super) uname: String,
    #[serde(default)]
    pub(super) face: String,
    #[serde(default)]
    pub(super) mid: i64,
    #[serde(default)]
    pub(super) money: f64,
    #[serde(default)]
    pub(super) moral: f64,
    #[serde(default)]
    pub(super) level_info: Option<LevelInfo>,
    #[serde(default)]
    pub(super) vip: Option<VipInfo>,
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Current year and month, used as the first `month` for `history/index`.
pub(crate) fn current_year_month() -> (i64, u32) {
    let secs = now_secs() as i64;
    let (year, month, _) = crate::civil_from_days(secs.div_euclid(86_400));
    (year, month as u32)
}

/// Strip the `<em class="keyword">…</em>` highlight markup search results carry.
pub(crate) fn strip_html(input: &str) -> String {
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

/// One highlighted span of a search title: char offsets into the plain text.
pub type Highlight = (usize, usize);

/// Split the site's `<em class="keyword">…</em>` markup out of a search title.
///
/// The sites' search payloads wrap every matched term in that tag (one wrap per
/// term). The plain text is what the card prints; the char ranges are kept so a
/// card can emphasise the same terms without re-parsing the markup. Entities
/// are decoded as one char each, so the ranges always index the returned string.
pub(crate) fn highlight_title(raw: &str) -> (String, Vec<Highlight>) {
    const ENTITIES: [(&str, char); 5] = [
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&amp;", '&'),
    ];
    let mut out = String::with_capacity(raw.len());
    let mut ranges: Vec<Highlight> = Vec::new();
    let mut count = 0usize;
    let mut em_start: Option<usize> = None;
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('<') {
            let (tag, after) = match tail.find('>') {
                Some(end) => (&tail[..end], &tail[end + 1..]),
                // An unterminated `<` is text, exactly as `strip_html` treats it.
                None => (tail, ""),
            };
            let name = tag.trim().to_ascii_lowercase();
            if name == "em" || name.starts_with("em ") {
                em_start = Some(count);
            } else if name == "/em" {
                if let Some(start) = em_start.take() {
                    if count > start {
                        ranges.push((start, count));
                    }
                }
            }
            rest = after;
            continue;
        }
        if let Some((name, ch)) = ENTITIES.iter().find(|(name, _)| rest.starts_with(name)) {
            out.push(*ch);
            count += 1;
            rest = &rest[name.len()..];
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        count += 1;
        rest = &rest[ch.len_utf8()..];
    }
    // A title that ends inside `<em>` still keeps the highlighted tail.
    if let Some(start) = em_start {
        if count > start {
            ranges.push((start, count));
        }
    }
    (out, ranges)
}

/// Bilibili image URLs may be protocol-relative (`//i0.hdslb.com/...`).
pub(crate) fn normalize_url(url: &str) -> String {
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

pub(crate) fn de_u64_flexible<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // `null` is common in the PGC index (an unrated season, a missing count):
    // it means "0", not a parse failure.
    Ok(match Option::<FlexNum>::deserialize(deserializer)? {
        Some(FlexNum::Int(value)) => value,
        Some(FlexNum::Float(value)) => value.max(0.0) as u64,
        Some(FlexNum::Text(value)) => value.trim().parse::<u64>().unwrap_or(0),
        None => 0,
    })
}

/// Like [`de_u64_flexible`] but for signed counters that arrive as a number or
/// a string (`rpid`/`ctime`/`rcount` and the reply pager).
pub(crate) fn de_i64_flexible<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<FlexNum>::deserialize(deserializer)? {
        Some(FlexNum::Int(value)) => value.min(i64::MAX as u64) as i64,
        Some(FlexNum::Float(value)) => value.max(0.0) as i64,
        Some(FlexNum::Text(value)) => value.trim().parse::<i64>().unwrap_or(0),
        None => 0,
    })
}

/// A score that arrives as a number, a numeric string or `"--"` (the PGC index
/// sends `"--"` for a season nobody has rated).
pub(crate) fn de_f64_flexible<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<FlexNum>::deserialize(deserializer)? {
        Some(FlexNum::Int(value)) => value as f64,
        Some(FlexNum::Float(value)) => value,
        Some(FlexNum::Text(value)) => value.trim().parse::<f64>().unwrap_or(0.0),
        None => 0.0,
    })
}

/// `null` (or a missing field) becomes `T::default()`.
///
/// The PGC index sends `badges: null` / `media_score: null` for a season with
/// no corner mark and no rating; that is "none", not a malformed response.
pub(crate) fn de_null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub(crate) fn de_duration_flexible<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<FlexNum>::deserialize(deserializer)? {
        Some(FlexNum::Text(value)) => value,
        Some(FlexNum::Int(secs)) => format!("{:02}:{:02}", secs / 60, secs % 60),
        Some(FlexNum::Float(secs)) => {
            let secs = secs.max(0.0) as u64;
            format!("{:02}:{:02}", secs / 60, secs % 60)
        }
        None => String::new(),
    })
}

pub(crate) fn de_string_lenient<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
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
pub(crate) fn de_flag<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
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
