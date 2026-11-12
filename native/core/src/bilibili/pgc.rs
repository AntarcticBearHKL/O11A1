//!  番剧与影视：时间表、索引、排行、季/集详情与 AI 总结。

use super::*;
use serde::Serialize;

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Region / availability gate for one episode. Never errors on a non-zero
    /// API code - the code IS the answer, and the caller shows it.
    pub async fn pgc_gate(&self, ep_id: i64, cid: u64) -> Result<PgcGate> {
        self.pgc_playurl_with(ep_id, cid, "16").await.map(|play| PgcGate {
            code: play.code,
            message: play.message,
        })
    }
}

impl Session {
    /// One episode's PGC stream, asked with `fnval=1` (the combined-MP4 mode
    /// the native player's single-URL channel needs).
    ///
    /// Never errors on a non-zero API code - the code IS the answer, and
    /// [`pgc_reason`] names the known refusals. `result.dash` is parsed as well
    /// when the endpoint happens to include it; use [`Session::pgc_playurl_dash`]
    /// to explicitly request DASH.
    pub async fn pgc_playurl(&self, ep_id: i64, cid: u64) -> Result<PgcPlayUrl> {
        self.pgc_playurl_with(ep_id, cid, "1").await
    }

    /// The same episode in DASH mode (`fnval=4048`): separate video/audio
    /// representations, each with its own base URL and mirrors.
    pub async fn pgc_playurl_dash(&self, ep_id: i64, cid: u64) -> Result<PgcPlayUrl> {
        self.pgc_playurl_with(ep_id, cid, "4048").await
    }

    /// One `pgc/player/web/playurl` call with the session's saved cookies.
    /// `fnval` selects the payload shape (`1` = combined MP4 `durl`, `4048` =
    /// DASH); the parser accepts either and both arrays are public.
    async fn pgc_playurl_with(&self, ep_id: i64, cid: u64, fnval: &str) -> Result<PgcPlayUrl> {
        if ep_id <= 0 || cid == 0 {
            bail!("PGC 播放地址缺少 ep_id/cid（ep_id={ep_id}, cid={cid}）");
        }
        let params = [
            ("ep_id", ep_id.to_string()),
            ("cid", cid.to_string()),
            ("qn", "80".to_string()),
            ("fnval", fnval.to_string()),
            ("fnver", "0".to_string()),
            ("fourk", "1".to_string()),
            ("platform", "pc".to_string()),
            ("otype", "json".to_string()),
        ];
        let body: PgcPlayUrlResp = self
            .get_query_json(PGC_PLAYURL_URL, &params, Some("https://www.bilibili.com/"))
            .await?;
        let play = pgc_playurl_from(body);
        crate::debug_log(&format!(
            "pgc playurl ep={ep_id} cid={cid} fnval={fnval} code={} qn={} durl={} video={} audio={}",
            play.code,
            play.quality,
            play.durl.len(),
            play.video.len(),
            play.audio.len()
        ));
        Ok(play)
    }
}

/// One row of 每日放送.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct PgcTimelineDay {
    /// "10-27".
    pub date: String,
    /// 1 = 周一 … 7 = 周日 (0 is mapped to 7).
    pub day_of_week: i64,
    pub is_today: bool,
    pub episodes: Vec<PgcTimelineEpisode>,
}

/// One card of the 分区索引 list.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct PgcIndexPage {
    pub page: i64,
    pub total: i64,
    pub has_next: bool,
    pub items: Vec<PgcIndexItem>,
}

/// One row of the 排行榜 (99 rows, no paging).
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct PgcSection {
    pub title: String,
    pub episodes: Vec<PgcEpisode>,
}

/// One season's detail, as the 番剧 page and the player's 选集 need it.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct PgcGate {
    pub code: i64,
    pub message: String,
}

/// One combined-MP4 segment of a PGC episode (`result.durl[]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PgcStream {
    /// `http://` URLs are upgraded to `https://`; empty when the payload had
    /// none.
    pub url: String,
    /// The site's own mirror list, also upgraded to `https://`.
    pub backup_urls: Vec<String>,
    /// Segment length in milliseconds.
    pub length_ms: u64,
    /// Segment size in bytes (`0` when the payload omitted it).
    pub size: u64,
    /// 1-based segment order the API reports.
    pub order: i64,
}

/// One DASH representation (`result.dash.video[]` / `result.dash.audio[]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PgcDashTrack {
    /// Quality id (`video`) or audio id (`audio`).
    pub id: i64,
    /// Primary base URL, `https://`.
    pub base_url: String,
    /// Every `backupUrl`/`backup_url` mirror, `https://`.
    pub backup_urls: Vec<String>,
    /// Bits per second the API reports.
    pub bandwidth: u64,
    /// 7 = AVC, 12 = HEVC, 13 = AV1 for video; 0 for audio.
    pub codecid: i64,
    /// RFC 6381 codec string (`avc1.64001F`, `mp4a.40.2`).
    pub codecs: String,
    /// `video/mp4` / `audio/mp4`.
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second; `0.0` for audio or a missing value.
    pub frame_rate: f64,
}

/// Everything one `pgc/player/web/playurl` answer carries for playback.
///
/// Both payload shapes are accepted in one type: `durl` is the combined MP4
/// (`fnval=1`) and `video`/`audio` are the DASH arrays (`fnval=4048`). Which
/// one is populated depends only on the request mode, not on the episode.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PgcPlayUrl {
    pub code: i64,
    pub message: String,
    /// The quality actually served (see `accept_quality` on the wire).
    pub quality: i64,
    /// Episode length in milliseconds (`result.timelength`).
    pub duration_ms: u64,
    pub durl: Vec<PgcStream>,
    pub video: Vec<PgcDashTrack>,
    pub audio: Vec<PgcDashTrack>,
}

impl PgcPlayUrl {
    /// The first non-empty combined-MP4 URL, which is what the native player's
    /// single-URL channel can open. `None` for a DASH-only answer.
    pub fn single_url(&self) -> Option<&str> {
        self.durl
            .iter()
            .map(|stream| stream.url.as_str())
            .find(|url| url.len() > 12)
    }

    /// Every playable URL in the payload: every `durl` segment plus every DASH
    /// base URL, in payload order.
    pub fn urls(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for stream in &self.durl {
            if !stream.url.is_empty() {
                out.push(&stream.url);
            }
        }
        for track in self.video.iter().chain(self.audio.iter()) {
            if !track.base_url.is_empty() {
                out.push(&track.base_url);
            }
        }
        out
    }

    /// True when the gate accepted the episode (`code == 0`) and it can be
    /// played without a second request.
    pub fn is_playable(&self) -> bool {
        self.code == 0 && (self.single_url().is_some() || !self.video.is_empty())
    }
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
struct PgcPlayUrlResp {
    #[serde(default)]
    code: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    message: String,
    #[serde(default)]
    result: Option<PgcPlayUrlResultRaw>,
}

#[derive(Deserialize, Default)]
struct PgcPlayUrlResultRaw {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    quality: i64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    timelength: u64,
    #[serde(default)]
    durl: Vec<PgcStreamRaw>,
    #[serde(default)]
    dash: Option<PgcDashRaw>,
}

#[derive(Deserialize, Default)]
struct PgcStreamRaw {
    #[serde(default, deserialize_with = "de_string_lenient")]
    url: String,
    #[serde(default)]
    backup_url: Vec<String>,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    length: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    size: u64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    order: i64,
}

#[derive(Deserialize, Default)]
struct PgcDashRaw {
    #[serde(default)]
    video: Vec<PgcDashTrackRaw>,
    #[serde(default)]
    audio: Vec<PgcDashTrackRaw>,
}

#[derive(Deserialize, Default)]
struct PgcDashTrackRaw {
    #[serde(default, deserialize_with = "de_i64_flexible")]
    id: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    base_url: String,
    #[serde(rename = "baseUrl", default, deserialize_with = "de_string_lenient")]
    base_url_camel: String,
    #[serde(default)]
    backup_url: Vec<String>,
    #[serde(rename = "backupUrl", default)]
    backup_url_camel: Vec<String>,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    bandwidth: u64,
    #[serde(default, deserialize_with = "de_i64_flexible")]
    codecid: i64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    codecs: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    mime_type: String,
    #[serde(rename = "mimeType", default, deserialize_with = "de_string_lenient")]
    mime_type_camel: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    width: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    height: u64,
    #[serde(default, deserialize_with = "de_string_lenient")]
    frame_rate: String,
    #[serde(rename = "frameRate", default, deserialize_with = "de_string_lenient")]
    frame_rate_camel: String,
}

/// Fold one `pgc/player/web/playurl` body into [`PgcPlayUrl`].
///
/// The parser is deliberately tolerant: a refusal body has no `result` at all,
/// a combined-MP4 body has no `dash`, and the DASH keys appear in both camel
/// and snake case across the site's own endpoints. Missing values become
/// `0`/empty instead of failing the parse.
fn pgc_playurl_from(resp: PgcPlayUrlResp) -> PgcPlayUrl {
    let result = resp.result.unwrap_or_default();
    let durl = result
        .durl
        .into_iter()
        .filter(|stream| !stream.url.trim().is_empty())
        .map(|stream| PgcStream {
            url: https_url(&stream.url),
            backup_urls: stream
                .backup_url
                .iter()
                .map(|url| https_url(url))
                .filter(|url| !url.is_empty())
                .collect(),
            length_ms: stream.length,
            size: stream.size,
            order: stream.order,
        })
        .collect();
    let dash = result.dash.unwrap_or_default();
    let video = dash.video.into_iter().map(dash_track_from).collect();
    let audio = dash.audio.into_iter().map(dash_track_from).collect();
    PgcPlayUrl {
        code: resp.code,
        message: resp.message,
        quality: result.quality,
        duration_ms: result.timelength,
        durl,
        video,
        audio,
    }
}

fn dash_track_from(raw: PgcDashTrackRaw) -> PgcDashTrack {
    let base_url = if raw.base_url.trim().is_empty() {
        raw.base_url_camel
    } else {
        raw.base_url
    };
    let mut backup_urls: Vec<String> = Vec::new();
    for url in raw.backup_url.iter().chain(raw.backup_url_camel.iter()) {
        let url = https_url(url);
        if !url.is_empty() && !backup_urls.contains(&url) {
            backup_urls.push(url);
        }
    }
    let frame_rate = raw
        .frame_rate
        .trim()
        .parse::<f64>()
        .or_else(|_| raw.frame_rate_camel.trim().parse::<f64>())
        .unwrap_or(0.0)
        .max(0.0);
    PgcDashTrack {
        id: raw.id,
        base_url: https_url(&base_url),
        backup_urls,
        bandwidth: raw.bandwidth,
        codecid: raw.codecid,
        codecs: raw.codecs,
        mime_type: if raw.mime_type.trim().is_empty() {
            raw.mime_type_camel
        } else {
            raw.mime_type
        },
        width: raw.width.min(u32::MAX as u64) as u32,
        height: raw.height.min(u32::MAX as u64) as u32,
        frame_rate,
    }
}

/// Upgrade a protocol-relative or plain-`http` stream URL to `https://`.
/// Unlike [`normalize_url`] it never invents a host for an empty value.
fn https_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        String::new()
    } else if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
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
pub(crate) fn parse_season(raw: &serde_json::Value) -> Option<Season> {
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
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
mod playurl_tests {
    use super::{pgc_playurl_from, PgcPlayUrlResp};

    fn parse(json: &str) -> super::PgcPlayUrl {
        let resp: PgcPlayUrlResp = serde_json::from_str(json).expect("fixture parses");
        pgc_playurl_from(resp)
    }

    /// Captured `fnval=1` shape: one combined MP4 segment whose primary URL is
    /// `https` and whose mirror is protocol-relative.
    #[test]
    fn combined_mp4_durl_is_parsed_and_upgraded() {
        let json = r#"{
            "code": 0,
            "message": "success",
            "result": {
                "quality": 16,
                "timelength": 721920,
                "durl": [
                    {
                        "order": 1,
                        "length": 721920,
                        "size": 45231472,
                        "url": "http://upos.example.com/40005011296-1-16.mp4",
                        "backup_url": ["//mirror.example.com/40005011296-1-16.mp4"]
                    }
                ]
            }
        }"#;
        let play = parse(json);
        assert_eq!(play.code, 0);
        assert_eq!(play.message, "success");
        assert_eq!(play.quality, 16);
        assert_eq!(play.duration_ms, 721920);
        assert_eq!(play.durl.len(), 1);
        assert_eq!(
            play.durl[0].url,
            "https://upos.example.com/40005011296-1-16.mp4"
        );
        assert_eq!(
            play.durl[0].backup_urls,
            vec!["https://mirror.example.com/40005011296-1-16.mp4".to_string()]
        );
        assert_eq!(play.durl[0].length_ms, 721920);
        assert_eq!(play.durl[0].size, 45231472);
        assert_eq!(play.durl[0].order, 1);
        assert_eq!(
            play.single_url(),
            Some("https://upos.example.com/40005011296-1-16.mp4")
        );
        assert!(play.is_playable());
        assert!(play.video.is_empty() && play.audio.is_empty());
    }

    /// Captured `fnval=4048` shape: camelCase and snake_case keys coexist, the
    /// frame rate is a string and the mirrors must be de-duplicated.
    #[test]
    fn dash_video_and_audio_are_parsed() {
        let json = r#"{
            "code": 0,
            "message": "success",
            "result": {
                "quality": 32,
                "timelength": 721920,
                "dash": {
                    "video": [
                        {
                            "id": 32,
                            "base_url": "https://upos.example.com/v-30032.m4s",
                            "baseUrl": "https://ignored.example.com/v-30032.m4s",
                            "backup_url": ["https://mirror.example.com/v-30032.m4s"],
                            "backupUrl": ["https://mirror.example.com/v-30032.m4s"],
                            "bandwidth": 737936,
                            "codecid": 7,
                            "codecs": "avc1.64001F",
                            "mime_type": "video/mp4",
                            "width": 852,
                            "height": 480,
                            "frame_rate": "25.000",
                            "frameRate": "25.000"
                        },
                        {
                            "id": 80,
                            "baseUrl": "https://upos.example.com/v-30080.m4s",
                            "bandwidth": "1234567",
                            "codecid": "12",
                            "codecs": "hev1.1.6.L120.90",
                            "mimeType": "video/mp4",
                            "width": 1920,
                            "height": 1080,
                            "frameRate": "60"
                        }
                    ],
                    "audio": [
                        {
                            "id": 30216,
                            "base_url": "https://upos.example.com/a-30216.m4s",
                            "bandwidth": 65689,
                            "codecid": 0,
                            "codecs": "mp4a.40.2",
                            "mime_type": "audio/mp4",
                            "width": 0,
                            "height": 0
                        }
                    ]
                }
            }
        }"#;
        let play = parse(json);
        assert_eq!(play.code, 0);
        assert_eq!(play.quality, 32);
        assert_eq!(play.duration_ms, 721920);
        assert!(play.durl.is_empty());
        assert_eq!(play.video.len(), 2);
        assert_eq!(play.audio.len(), 1);
        assert_eq!(play.video[0].id, 32);
        assert_eq!(play.video[0].base_url, "https://upos.example.com/v-30032.m4s");
        assert_eq!(
            play.video[0].backup_urls,
            vec!["https://mirror.example.com/v-30032.m4s".to_string()]
        );
        assert_eq!(play.video[0].bandwidth, 737936);
        assert_eq!(play.video[0].codecid, 7);
        assert_eq!(play.video[0].codecs, "avc1.64001F");
        assert_eq!(play.video[0].mime_type, "video/mp4");
        assert_eq!(play.video[0].width, 852);
        assert_eq!(play.video[0].height, 480);
        assert!((play.video[0].frame_rate - 25.0).abs() < f64::EPSILON);
        assert_eq!(play.video[1].id, 80);
        assert_eq!(play.video[1].base_url, "https://upos.example.com/v-30080.m4s");
        assert_eq!(play.video[1].bandwidth, 1234567);
        assert_eq!(play.video[1].codecid, 12);
        assert_eq!(play.video[1].mime_type, "video/mp4");
        assert!((play.video[1].frame_rate - 60.0).abs() < f64::EPSILON);
        assert_eq!(play.audio[0].id, 30216);
        assert_eq!(play.audio[0].codecs, "mp4a.40.2");
        assert_eq!(play.audio[0].mime_type, "audio/mp4");
        assert_eq!(play.urls().len(), 3);
        assert!(play.single_url().is_none());
        assert!(play.is_playable());
    }

    /// A measured region refusal: no `result` block at all, so nothing can be
    /// invented and the caller must show the server's own code/message.
    #[test]
    fn refusal_keeps_the_exact_code_and_message() {
        let json = r#"{ "code": -10403, "message": "抱歉您所在地区不可观看！" }"#;
        let play = parse(json);
        assert_eq!(play.code, -10403);
        assert_eq!(play.message, "抱歉您所在地区不可观看！");
        assert_eq!(play.quality, 0);
        assert!(play.durl.is_empty());
        assert!(play.video.is_empty() && play.audio.is_empty());
        assert!(play.single_url().is_none());
        assert!(!play.is_playable());
        assert_eq!(
            super::pgc_reason(play.code),
            Some("本地区不可观看"),
            "the refusal code keeps its Chinese reason"
        );
    }

    /// An empty `durl` must not become a blank URL: the player needs to know
    /// that there is no single combined stream.
    #[test]
    fn empty_durl_entries_are_dropped() {
        let json = r#"{
            "code": 0,
            "message": "success",
            "result": { "quality": 0, "durl": [{ "url": "", "size": 1 }] }
        }"#;
        let play = parse(json);
        assert!(play.durl.is_empty());
        assert!(play.single_url().is_none());
        assert!(!play.is_playable());
    }
}

