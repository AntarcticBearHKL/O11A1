//!  搜索：视频/用户/番剧/直播检索、搜索建议与热搜。

use super::*;
use serde::Serialize;

impl Session {
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
        let data = self
            .search_data::<SearchData<Vec<SearchVideoItem>>>(&params)
            .await?;
        check_search_voucher(data.v_voucher.as_deref())?;
        let items = data
            .result
            .into_iter()
            .map(|item| {
                let (title, _highlights) = highlight_title(&item.title);
                SearchVideo {
                    bvid: item.bvid,
                    title,
                    description: strip_html(&item.description),
                    pic: normalize_url(&item.pic),
                    author: item.author,
                    mid: item.mid,
                    play: item.play,
                    duration: item.duration,
                    pub_ts: item.pubdate,
                }
            })
            .collect();
        Ok(SearchPage {
            page: data.page,
            num_pages: data.num_pages,
            num_results: data.num_results,
            items,
        })
    }
}

impl Session {
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
        let data = self
            .search_data::<SearchData<Vec<SearchUserItem>>>(&params)
            .await?;
        check_search_voucher(data.v_voucher.as_deref())?;
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
}

impl Session {
    /// One search tab's results, fetched from the endpoint that tab belongs to.
    ///
    /// `kind` picks `search_type`: `video` / `bili_user` are the UGC index, while
    /// 番剧 / 影视 / 直播 live in other indexes (`media_bangumi` / `media_ft` /
    /// `live`) that the video search never returns. Everything else — the WBI
    /// signature, the one retry after refreshing the keys, the `page` parameter
    /// and the paging metadata — is the same stack [`Self::search_video`] uses.
    pub async fn search_hits(
        &self,
        kind: SearchKind,
        keyword: &str,
        page: i64,
        order: &str,
        ps: i64,
    ) -> Result<SearchHitPage> {
        match kind {
            SearchKind::Video => {
                let page = self.search_video(keyword, page, order, ps).await?;
                Ok(SearchHitPage {
                    page: page.page,
                    num_pages: page.num_pages,
                    num_results: page.num_results,
                    items: page.items.into_iter().map(SearchHit::Video).collect(),
                })
            }
            SearchKind::User => {
                let page = self.search_user(keyword, page, ps).await?;
                Ok(SearchHitPage {
                    page: page.page,
                    num_pages: page.num_pages,
                    num_results: page.num_results,
                    items: page.items.into_iter().map(SearchHit::User).collect(),
                })
            }
            SearchKind::Bangumi | SearchKind::Film => {
                let mut params = vec![
                    ("search_type".to_string(), kind.search_type().to_string()),
                    ("keyword".to_string(), keyword.to_string()),
                    ("page".to_string(), page.max(1).to_string()),
                ];
                if let Some(ps) = page_size_param(ps) {
                    params.push(("page_size".to_string(), ps.to_string()));
                }
                let data = self
                    .search_data_referer::<MediaSearchData>(&params, Some(SEARCH_REFERER))
                    .await?;
                check_search_voucher(data.v_voucher.as_deref())?;
                Ok(SearchHitPage {
                    page: data.page,
                    num_pages: data.num_pages,
                    num_results: data.num_results,
                    items: data
                        .result
                        .into_iter()
                        .map(|item| SearchHit::Media(media_from_search(item)))
                        .collect(),
                })
            }
            SearchKind::Live => {
                let mut params = vec![
                    ("search_type".to_string(), kind.search_type().to_string()),
                    ("keyword".to_string(), keyword.to_string()),
                    ("page".to_string(), page.max(1).to_string()),
                ];
                if let Some(ps) = page_size_param(ps) {
                    params.push(("page_size".to_string(), ps.to_string()));
                }
                let data = self
                    .search_data_referer::<LiveSearchData>(&params, Some(SEARCH_REFERER))
                    .await?;
                check_search_voucher(data.v_voucher.as_deref())?;
                Ok(SearchHitPage {
                    page: data.page,
                    num_pages: data.num_pages,
                    num_results: data.num_results,
                    items: data
                        .result
                        .live_room
                        .into_iter()
                        .map(|item| SearchHit::Live(live_from_search(item)))
                        .collect(),
                })
            }
        }
    }
}

impl Session {
    /// WBI-signed search call with one retry after refreshing the keys.
    async fn search_data<T: DeserializeOwned>(
        &self,
        params: &[(String, String)],
    ) -> Result<T> {
        self.search_data_referer::<T>(params, None).await
    }
}

impl Session {
    /// [`Self::search_data`] with an explicit `Referer`.
    ///
    /// The PGC / 直播 indexes answer an anonymous or referer-less `search/type`
    /// with a `v_voucher` risk-control body, while the video index does not; the
    /// only difference between the two paths is this header.
    async fn search_data_referer<T: DeserializeOwned>(
        &self,
        params: &[(String, String)],
        referer: Option<&str>,
    ) -> Result<T> {
        let mut resp: SearchResp<T> = self
            .wbi_call_referer(SEARCH_URL, params, false, referer)
            .await?
            .1;
        if resp.code != 0 {
            resp = self
                .wbi_call_referer(SEARCH_URL, params, true, referer)
                .await?
                .1;
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    async fn hotword_fallback(&self) -> Result<Vec<HotSearch>> {
        let body: HotwordResp = self.get_query_json(HOTWORD_URL, &[], None).await?;
        Ok(body.list.into_iter().map(hotword_to_hot).collect())
    }
}

/// One page of search results plus the API's paging metadata.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchPage<T> {
    pub page: i64,
    pub num_pages: i64,
    pub num_results: i64,
    pub items: Vec<T>,
}

#[derive(Clone, Debug, Default, Serialize)]
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

#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchUser {
    pub mid: i64,
    pub uname: String,
    pub upic: String,
    pub fans: u64,
    pub videos: u64,
    pub usign: String,
}

// -- Unified search hits (five tabs) ----------------------------------------

/// The five search tabs. 番剧 / 影视 are PGC seasons and 直播 rooms: they are
/// **not** in the video index, which is why "名侦探柯南" found no official
/// season before this existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SearchKind {
    Video,
    Bangumi,
    Film,
    Live,
    User,
}

/// Tab index used by the UI: 0 视频, 1 番剧, 2 影视, 3 直播, 4 用户.
pub const SEARCH_TAB_VIDEO: i32 = 0;

pub const SEARCH_TAB_BANGUMI: i32 = 1;

pub const SEARCH_TAB_FILM: i32 = 2;

pub const SEARCH_TAB_LIVE: i32 = 3;

pub const SEARCH_TAB_USER: i32 = 4;

impl SearchKind {
    /// The tab a stored index means; anything out of range is 视频.
    pub fn from_tab(tab: i32) -> Self {
        match tab {
            SEARCH_TAB_BANGUMI => Self::Bangumi,
            SEARCH_TAB_FILM => Self::Film,
            SEARCH_TAB_LIVE => Self::Live,
            SEARCH_TAB_USER => Self::User,
            _ => Self::Video,
        }
    }

    pub fn tab(self) -> i32 {
        match self {
            Self::Video => SEARCH_TAB_VIDEO,
            Self::Bangumi => SEARCH_TAB_BANGUMI,
            Self::Film => SEARCH_TAB_FILM,
            Self::Live => SEARCH_TAB_LIVE,
            Self::User => SEARCH_TAB_USER,
        }
    }

    /// The endpoint's own `search_type` value.
    pub fn search_type(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Bangumi => "media_bangumi",
            Self::Film => "media_ft",
            Self::Live => "live",
            Self::User => "bili_user",
        }
    }

    /// The chip label, identical to the UI's own list.
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "视频",
            Self::Bangumi => "番剧",
            Self::Film => "影视",
            Self::Live => "直播",
            Self::User => "用户",
        }
    }

    /// 番剧 / 影视 / 直播 answer one short page list: the PGC index stops
    /// serving after a couple of pages and the live index after one, so the
    /// caller stops at `num_pages` instead of asking for page N+1.
    pub fn shallow_paging(self) -> bool {
        matches!(self, Self::Bangumi | Self::Film | Self::Live)
    }

    /// The empty-state sentence for this tab.
    pub fn empty_text(self) -> &'static str {
        match self {
            Self::Video => "没有找到相关视频，试试其它关键词",
            Self::Bangumi => "没有找到相关番剧，试试其它关键词",
            Self::Film => "没有找到相关影视，试试其它关键词",
            Self::Live => "没有找到相关直播，试试其它关键词",
            Self::User => "没有找到相关用户，试试其它关键词",
        }
    }
}

/// One search result, whatever tab produced it.
#[derive(Clone, Debug, Serialize)]
pub enum SearchHit {
    Video(SearchVideo),
    User(SearchUser),
    Media(SearchMedia),
    Live(SearchLive),
}

/// One page of [`SearchHit`]s plus the API's paging metadata.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchHitPage {
    pub page: i64,
    pub num_pages: i64,
    pub num_results: i64,
    pub items: Vec<SearchHit>,
}

impl SearchHitPage {
    /// Another page exists. `false` is what makes a shallow tab stop at its
    /// last page and the UI print 「没有更多」 instead of an arrow that does
    /// nothing.
    pub fn has_more(&self) -> bool {
        self.num_pages > 0 && self.page < self.num_pages
    }
}

/// One 番剧 / 影视 season from the PGC search index.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchMedia {
    pub season_id: i64,
    pub media_id: i64,
    /// Plain title (the `<em>` highlight markup stripped).
    pub title: String,
    /// Char ranges the site highlighted in the title, as `(start, end)`.
    pub highlights: Vec<(usize, usize)>,
    pub cover: String,
    /// The site's score, `0.0` when it has none (the API sends `"--"`).
    pub score: f64,
    /// `"9.6"` / `"暂无评分"` — ready to print.
    pub score_text: String,
    /// Episode count (`eps`), `0` when the payload omits it.
    pub eps: u64,
    /// `"全24话"` / `"更新至第12话"`.
    pub index_show: String,
    /// `"番剧"` / `"国创"` / `"电影"` / `"电视剧"` … the site's own label.
    pub season_type_name: String,
    /// `"奇幻,冒险"`.
    pub styles: String,
    /// `"日本"` … the production area.
    pub areas: String,
    /// 追番 / 追剧 count.
    pub follow: u64,
    /// `"会员专享"` / `"独播"` … corner mark, empty when there is none.
    pub badge: String,
}

/// One 直播 room from the live search index.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchLive {
    pub room_id: i64,
    pub uid: i64,
    pub title: String,
    pub uname: String,
    pub cover: String,
    pub online: u64,
    /// 分区 name (`tag_name`, else `cate_name`).
    pub area_name: String,
    pub live_status: i64,
}

impl SearchLive {
    /// The room shape [`crate::app::live::open_live_player`] takes.
    pub fn to_room(&self) -> LiveRoom {
        LiveRoom {
            room_id: self.room_id,
            uid: self.uid,
            title: self.title.clone(),
            uname: self.uname.clone(),
            online: self.online,
            cover: self.cover.clone(),
            area_name: self.area_name.clone(),
            parent_area_name: String::new(),
            live_status: self.live_status,
        }
    }
}

#[derive(Deserialize)]
struct SearchResp<T> {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

/// The video / 用户 index's `data`: one page of items.
#[derive(Deserialize)]
struct SearchData<T> {
    #[serde(default)]
    page: i64,
    #[serde(default, rename = "numResults")]
    num_results: i64,
    #[serde(default, rename = "numPages")]
    num_pages: i64,
    /// Present when the request was answered with a risk-control challenge
    /// instead of results (a session that has been hitting the index too hard).
    #[serde(default)]
    v_voucher: Option<String>,
    result: T,
}

/// The PGC index's `data` (`search_type=media_bangumi|media_ft`).
///
/// `result` is **absent** (not `[]`) when nothing matched — a region-locked
/// season is filtered out of the index for this network, which is exactly how
/// "名侦探柯南" answers `numResults: 0` — so the field defaults to an empty
/// page instead of failing the parse.
#[derive(Deserialize)]
struct MediaSearchData {
    #[serde(default)]
    page: i64,
    #[serde(default, rename = "numResults")]
    num_results: i64,
    #[serde(default, rename = "numPages")]
    num_pages: i64,
    #[serde(default)]
    v_voucher: Option<String>,
    #[serde(default)]
    result: Vec<SearchMediaItem>,
}

/// The 直播 index's `data`: `result` is a **map** of two lists
/// (`live_room` / `live_user`), not the flat array the other indexes send.
#[derive(Deserialize)]
struct LiveSearchData {
    #[serde(default)]
    page: i64,
    #[serde(default, rename = "numResults")]
    num_results: i64,
    #[serde(default, rename = "numPages")]
    num_pages: i64,
    #[serde(default)]
    v_voucher: Option<String>,
    #[serde(default)]
    result: LiveSearchResult,
}

/// A `v_voucher` body is a challenge, not an empty result set: without this the
/// PGC / 直播 tabs would print 「没有找到相关番剧」 for a request the site never
/// answered.
fn check_search_voucher(voucher: Option<&str>) -> Result<()> {
    if voucher.map(|value| !value.trim().is_empty()).unwrap_or(false) {
        bail!("搜索被 B站 风控拦截，请稍后重试（或登录后再搜）");
    }
    Ok(())
}

#[derive(Deserialize, Default)]
struct LiveSearchResult {
    #[serde(default)]
    live_room: Vec<SearchLiveItem>,
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

/// One item of `search_type=media_bangumi` / `media_ft`. The PGC index shares
/// the response envelope with the video index, so the same retry/parse path
/// reads it; only the item shape differs. Captured from the live index: the
/// episode count is `ep_size`, the score is `media_score.score`, and there is
/// no `rating` / `follow` field at all.
#[derive(Deserialize)]
struct SearchMediaItem {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    season_id: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    media_id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    cover: String,
    /// Episode count (`ep_size`), `0` when the payload omits it.
    #[serde(default, deserialize_with = "de_u64_flexible")]
    ep_size: u64,
    #[serde(default, deserialize_with = "de_null_default")]
    media_score: MediaScore,
    #[serde(default)]
    season_type_name: String,
    #[serde(default)]
    styles: String,
    #[serde(default)]
    areas: String,
    #[serde(default)]
    index_show: String,
    /// Corner marks the site draws on the poster (`会员专享` / `独播` / `出品`);
    /// `null` when there is none.
    #[serde(default, deserialize_with = "de_null_default")]
    badges: Vec<MediaBadge>,
}

#[derive(Deserialize, Default)]
struct MediaScore {
    /// `9.4` as a number; a string form is accepted too.
    #[serde(default, deserialize_with = "de_f64_flexible")]
    score: f64,
    /// How many people rated it (the poster's bottom-right line).
    #[serde(default, deserialize_with = "de_u64_flexible")]
    user_count: u64,
}

#[derive(Deserialize, Default)]
struct MediaBadge {
    #[serde(default)]
    text: String,
}

/// One item of `search_type=live`. The live index reports the room's 分区 as
/// `cate_name` (with the same `<em>` highlight), and has no `tag_name`.
#[derive(Deserialize)]
struct SearchLiveItem {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    roomid: i64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    uid: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    uname: String,
    #[serde(default)]
    cover: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    online: u64,
    #[serde(default)]
    cate_name: String,
    #[serde(default)]
    tag_name: String,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    live_status: i64,
}

/// Turn one PGC index item into the app's own row type.
fn media_from_search(item: SearchMediaItem) -> SearchMedia {
    let (title, highlights) = highlight_title(&item.title);
    let score = if item.media_score.score > 0.0 {
        item.media_score.score
    } else {
        0.0
    };
    let badge = item
        .badges
        .first()
        .map(|badge| strip_html(&badge.text))
        .unwrap_or_default();
    SearchMedia {
        season_id: item.season_id.max(0),
        media_id: item.media_id.max(0),
        title,
        highlights,
        cover: normalize_url(&item.cover),
        score,
        score_text: if score > 0.0 {
            format!("{score:.1}")
        } else {
            "暂无评分".to_string()
        },
        eps: item.ep_size,
        index_show: item.index_show,
        season_type_name: item.season_type_name,
        styles: item.styles,
        areas: item.areas,
        follow: item.media_score.user_count,
        badge,
    }
}

/// Turn one live index item into the app's own row type.
fn live_from_search(item: SearchLiveItem) -> SearchLive {
    // `cate_name` carries the same `<em class="keyword">` markup as the title.
    let tag = strip_html(&item.tag_name);
    let cate = strip_html(&item.cate_name);
    let area_name = if tag.trim().is_empty() {
        cate.trim().to_string()
    } else {
        tag.trim().to_string()
    };
    SearchLive {
        room_id: item.roomid.max(0),
        uid: item.uid.max(0),
        title: strip_html(&item.title),
        uname: item.uname,
        cover: normalize_url(&item.cover),
        online: item.online,
        area_name,
        live_status: item.live_status,
    }
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
pub(crate) struct SquareItem {
    #[serde(default)]
    pub(super) keyword: String,
    #[serde(default)]
    pub(super) show_name: String,
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

#[cfg(test)]
mod search_kind_tests {
    use super::{
        highlight_title, live_from_search, media_from_search, SearchHitPage,
        SearchKind, SearchLiveItem, SearchMediaItem, SEARCH_TAB_BANGUMI, SEARCH_TAB_FILM,
        SEARCH_TAB_LIVE, SEARCH_TAB_USER, SEARCH_TAB_VIDEO,
    };

    #[test]
    fn tabs_map_to_the_four_distinct_endpoints() {
        assert_eq!(SearchKind::from_tab(SEARCH_TAB_VIDEO), SearchKind::Video);
        assert_eq!(SearchKind::from_tab(SEARCH_TAB_BANGUMI), SearchKind::Bangumi);
        assert_eq!(SearchKind::from_tab(SEARCH_TAB_FILM), SearchKind::Film);
        assert_eq!(SearchKind::from_tab(SEARCH_TAB_LIVE), SearchKind::Live);
        assert_eq!(SearchKind::from_tab(SEARCH_TAB_USER), SearchKind::User);
        // A stored tab from a hand-edited settings.json falls back to 视频.
        assert_eq!(SearchKind::from_tab(9), SearchKind::Video);
        assert_eq!(SearchKind::from_tab(-1), SearchKind::Video);

        assert_eq!(SearchKind::Video.search_type(), "video");
        assert_eq!(SearchKind::Bangumi.search_type(), "media_bangumi");
        assert_eq!(SearchKind::Film.search_type(), "media_ft");
        assert_eq!(SearchKind::Live.search_type(), "live");
        assert_eq!(SearchKind::User.search_type(), "bili_user");

        // 番剧 / 影视 / 直播: the PGC and live indexes stop after a few pages.
        assert!(!SearchKind::Video.shallow_paging());
        assert!(!SearchKind::User.shallow_paging());
        assert!(SearchKind::Bangumi.shallow_paging());
        assert!(SearchKind::Film.shallow_paging());
        assert!(SearchKind::Live.shallow_paging());
        assert_eq!(SearchKind::Bangumi.tab(), SEARCH_TAB_BANGUMI);
        assert_eq!(SearchKind::User.label(), "用户");
    }

    /// A risk-control body (`v_voucher`) must be an error, not "no results":
    /// the PGC / 直播 tabs print their empty copy otherwise.
    #[test]
    fn risk_control_body_is_not_reported_as_empty() {
        let data: super::MediaSearchData =
            serde_json::from_str(r#"{ "page": 1, "numResults": 0, "v_voucher": "voucher_x" }"#)
                .expect("fixture parses");
        assert!(super::check_search_voucher(data.v_voucher.as_deref()).is_err());
        let data: super::MediaSearchData =
            serde_json::from_str(r#"{ "page": 1, "numResults": 0 }"#).expect("fixture parses");
        assert!(super::check_search_voucher(data.v_voucher.as_deref()).is_ok());
    }

    #[test]
    fn shallow_tabs_never_claim_another_page() {
        let last = SearchHitPage {
            page: 2,
            num_pages: 2,
            ..SearchHitPage::default()
        };
        assert!(!last.has_more());
        let more = SearchHitPage {
            page: 1,
            num_pages: 3,
            ..SearchHitPage::default()
        };
        assert!(more.has_more());
        // No paging metadata at all: one page, nothing more to ask for.
        assert!(!SearchHitPage::default().has_more());
    }

    /// The real `media_bangumi` title shape: one `<em>` per matched term.
    #[test]
    fn em_markup_is_stripped_and_ranges_survive_multibyte_text() {
        let (title, ranges) = highlight_title(
            "<em class=\"keyword\">名侦探</em>柯南 <em class=\"keyword\">剧场版</em>",
        );
        assert_eq!(title, "名侦探柯南 剧场版");
        assert_eq!(ranges, vec![(0, 3), (6, 9)]);
        // Chars, not bytes: 名 is 3 bytes in UTF-8.
        assert_eq!(&title[..0], "");
        assert_eq!(title.chars().take(3).collect::<String>(), "名侦探");

        let (plain, none) = highlight_title("孤独摇滚");
        assert_eq!(plain, "孤独摇滚");
        assert!(none.is_empty());

        // Entities decode to one char and keep the ranges aligned.
        let (escaped, ranges) = highlight_title("A &amp; B");
        assert_eq!(escaped, "A & B");
        assert!(ranges.is_empty());

        // Unterminated markup still yields the text (never a panic).
        let (broken, _) = highlight_title("<em>abc");
        assert_eq!(broken, "abc");
    }

    #[test]
    fn media_item_maps_score_episodes_and_label() {
        let item: SearchMediaItem = serde_json::from_str(
            r#"{
                "season_id": 73959, "media_id": 23774342,
                "title": "<em class=\"keyword\">名侦探</em>柯南",
                "cover": "//i0.hdslb.com/bfs/bangumi/a.jpg",
                "ep_size": 35, "media_score": { "score": 9.4, "user_count": 451 },
                "season_type_name": "番剧",
                "styles": "推理/冒险", "areas": "日本",
                "index_show": "全35话",
                "badges": [{ "text": "会员专享" }]
            }"#,
        )
        .expect("fixture parses");
        let media = media_from_search(item);
        assert_eq!(media.season_id, 73959);
        assert_eq!(media.media_id, 23774342);
        assert_eq!(media.title, "名侦探柯南");
        assert_eq!(media.highlights, vec![(0, 3)]);
        assert_eq!(media.cover, "https://i0.hdslb.com/bfs/bangumi/a.jpg");
        assert_eq!(media.score_text, "9.4");
        assert_eq!(media.eps, 35);
        assert_eq!(media.index_show, "全35话");
        assert_eq!(media.season_type_name, "番剧");
        assert_eq!(media.follow, 451);
        assert_eq!(media.badge, "会员专享");
    }

    /// A season nobody has rated sends `score: 0` (and no `badges`), while a
    /// string score is accepted too.
    #[test]
    fn unrated_season_reports_no_score() {
        let item: SearchMediaItem =
            serde_json::from_str(r#"{ "season_id": 1, "media_score": { "score": 0 } }"#)
                .expect("fixture");
        let media = media_from_search(item);
        assert_eq!(media.score_text, "暂无评分");
        assert_eq!(media.badge, "");
        let item: SearchMediaItem =
            serde_json::from_str(r#"{ "season_id": 1, "media_score": { "score": "8.5" } }"#)
                .expect("fixture");
        assert_eq!(media_from_search(item).score_text, "8.5");
    }

    /// The PGC index sends `badges: null` / `media_score: null` for a season
    /// with no corner mark and no rating: that is "none", not a bad response.
    #[test]
    fn null_corner_marks_and_score_parse_as_defaults() {
        let item: SearchMediaItem = serde_json::from_str(
            r#"{ "season_id": 7, "title": "凡人修仙传", "ep_size": null,
                 "media_score": null, "badges": null, "index_show": "全30话" }"#,
        )
        .expect("fixture parses");
        let media = media_from_search(item);
        assert_eq!(media.score_text, "暂无评分");
        assert_eq!(media.badge, "");
        assert_eq!(media.eps, 0);
        assert_eq!(media.follow, 0);
        assert_eq!(media.index_show, "全30话");
    }

    #[test]
    fn live_item_maps_room_area_and_online() {
        let item: SearchLiveItem = serde_json::from_str(
            r#"{
                "roomid": 545068, "uid": 8739477, "title": "德云色<em class=\"keyword\">上分</em>",
                "uname": "老实憨厚的笑笑", "online": 389934,
                "cover": "//i0.hdslb.com/bfs/live/a.jpg",
                "tag_name": "英雄联盟", "cate_name": "网游", "live_status": 1
            }"#,
        )
        .expect("fixture parses");
        let live = live_from_search(item);
        assert_eq!(live.room_id, 545068);
        assert_eq!(live.title, "德云色上分");
        assert_eq!(live.area_name, "英雄联盟");
        assert_eq!(live.cover, "https://i0.hdslb.com/bfs/live/a.jpg");
        let room = live.to_room();
        assert_eq!(room.room_id, 545068);
        assert_eq!(room.online, 389934);
        assert_eq!(room.area_name, "英雄联盟");
        assert_eq!(room.live_status, 1);

        // The live index's real 分区 field is `cate_name`, with highlight
        // markup; it is the one the card prints.
        let item: SearchLiveItem = serde_json::from_str(
            r#"{ "roomid": 1, "cate_name": "<em class=\"keyword\">剑网3</em>" }"#,
        )
        .expect("fixture");
        assert_eq!(live_from_search(item).area_name, "剑网3");
    }
}
