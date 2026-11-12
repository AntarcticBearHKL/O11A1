//!  首页与视频：信息流、分区、排行、视频详情、播放地址(playurl)、画质档位与 CDN 选路。

use super::*;
use serde::Serialize;

/// Upper bound for `BILI_FEED_PAGES` (each page is one signed request).
const MAX_FEED_PAGES: usize = 8;

/// Bilibili's image CDN accepts a `@WxH` resize suffix; request a small webp
/// thumbnail instead of the full-size cover.
const THUMB_SUFFIX: &str = "@480w_270h_1c.webp";

/// Hard cap on thumbnails fetched for one list, so a very long view cannot
/// spawn an unbounded number of image requests.
pub const THUMB_MAX_PER_VIEW: usize = 120;

// -- 收藏推荐 caching ---------------------------------------------------------

/// Counts thumbnails that had to fall back from the small CDN variant to the
/// raw URL (diagnostic only, surfaced through `BILI_DEBUG_LOG`).
static THUMB_FALLBACKS: AtomicU64 = AtomicU64::new(0);

pub fn thumb_fallbacks() -> u64 {
    THUMB_FALLBACKS.load(Ordering::Relaxed)
}

// -- f216: cover request telemetry -------------------------------------------
//
// One counter per outcome bucket plus the timing of the whole `fetch_thumb`
// call (both CDN attempts included). The per-second QA watchdog prints the
// deltas, so "every cover stops appearing" can be told apart from "the CDN
// started answering 403" without guessing.

/// HTTP 2xx answers, HTTP 3xx, 4xx and 5xx, transport/timeout errors and
/// decode failures. `THUMB_REQ_*` sum to the requests started.
pub static THUMB_REQ_2XX: AtomicU64 = AtomicU64::new(0);

pub static THUMB_REQ_3XX: AtomicU64 = AtomicU64::new(0);

pub static THUMB_REQ_4XX: AtomicU64 = AtomicU64::new(0);

pub static THUMB_REQ_5XX: AtomicU64 = AtomicU64::new(0);

pub static THUMB_REQ_NET: AtomicU64 = AtomicU64::new(0);

pub static THUMB_DECODE_FAIL: AtomicU64 = AtomicU64::new(0);

/// Total wall time and worst single call of `fetch_thumb`, in ms.
pub static THUMB_MS_TOTAL: AtomicU64 = AtomicU64::new(0);

pub static THUMB_MS_MAX: AtomicU64 = AtomicU64::new(0);

/// `fetch_thumb` calls that returned `Ok`.
pub static THUMB_OK: AtomicU64 = AtomicU64::new(0);

fn thumb_note_status(status: u16) {
    match status {
        200..=299 => THUMB_REQ_2XX.fetch_add(1, Ordering::Relaxed),
        300..=399 => THUMB_REQ_3XX.fetch_add(1, Ordering::Relaxed),
        400..=499 => THUMB_REQ_4XX.fetch_add(1, Ordering::Relaxed),
        _ => THUMB_REQ_5XX.fetch_add(1, Ordering::Relaxed),
    };
}

/// One cover request's outcome, printed per second by the QA watchdog.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ThumbStats {
    pub ok: u64,
    pub req_2xx: u64,
    pub req_3xx: u64,
    pub req_4xx: u64,
    pub req_5xx: u64,
    pub net_err: u64,
    pub decode_fail: u64,
    pub ms_total: u64,
    pub ms_max: u64,
}

pub fn thumb_stats() -> ThumbStats {
    ThumbStats {
        ok: THUMB_OK.load(Ordering::Relaxed),
        req_2xx: THUMB_REQ_2XX.load(Ordering::Relaxed),
        req_3xx: THUMB_REQ_3XX.load(Ordering::Relaxed),
        req_4xx: THUMB_REQ_4XX.load(Ordering::Relaxed),
        req_5xx: THUMB_REQ_5XX.load(Ordering::Relaxed),
        net_err: THUMB_REQ_NET.load(Ordering::Relaxed),
        decode_fail: THUMB_DECODE_FAIL.load(Ordering::Relaxed),
        ms_total: THUMB_MS_TOTAL.load(Ordering::Relaxed),
        ms_max: THUMB_MS_MAX.load(Ordering::Relaxed),
    }
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

impl Session {
    /// One video's related list (the 相关推荐 the player shows), filtered to
    /// real archive entries: ads carry no `bvid` and/or a non-`av` `goto`.
    /// Cached for [`FAV_RELATED_TTL`] per `bvid`.
    ///
    /// **No paging**: `archive/related` ignores `pn`, `ps` and `page` (probed
    /// 2026-11-10: `?pn=2` and `?ps=50` returned the byte-identical 53 KB
    /// payload as the bare request). The endpoint answers its whole list in one
    /// response, so the 详情 page shows every item it receives and has no
    /// load-more request to make.
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
}

impl Session {
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
}

impl Session {
    /// 进度条悬停预览的雪碧图 (storyboard)。
    ///
    /// B站 web 播放器 hover 进度条时显示的那张预览图就来自这里：一张
    /// `img_x_len x img_y_len`（实测 10x10）的格子图，每格 `img_x_size x
    /// img_y_size`（实测 480x270），`image` 给出 1..N 张 jpg，`index` 给出每格
    /// 的时间戳（秒）。未登录 / 视频没有切片时 `code != 0`，这里返回 `Err`，
    /// 调用方据此降级成“只显示时间”，不向用户报错。
    pub async fn videoshot(&self, bvid: &str, cid: u64) -> Result<VideoShot> {
        if bvid.trim().is_empty() || cid == 0 {
            bail!("videoshot: 缺少 bvid/cid");
        }
        let params: Vec<(&str, String)> = vec![
            ("bvid", bvid.trim().to_string()),
            ("cid", cid.to_string()),
            ("index", "1".to_string()),
        ];
        let referer = format!("https://www.bilibili.com/video/{}", bvid.trim());
        let resp: ShotResp = self
            .get_query_json(VIDEOSHOT_URL, &params, Some(&referer))
            .await?;
        if resp.code != 0 {
            let message = resp.message.trim();
            if message.is_empty() {
                bail!("videoshot 不可用（code={}）", resp.code);
            }
            bail!("videoshot 不可用：{message}（code={}）", resp.code);
        }
        let data = resp.data.unwrap_or_default();
        let shot = videoshot_from(data)?;
        crate::debug_log(&format!(
            "videoshot ok bvid={} cid={cid} sheets={} cols={} rows={} cell={}x{} tiles={}",
            bvid.trim(),
            shot.sheets.len(),
            shot.cols,
            shot.rows,
            shot.cell_w,
            shot.cell_h,
            shot.times.len()
        ));
        Ok(shot)
    }

    // -- 视频评论 (comments) ------------------------------------------------
}

impl Session {
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
}

impl Session {
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
        crate::net::note_response_bytes(text.len() as u64);
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
}

impl Session {
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
        crate::net::note_response_bytes(text.len() as u64);
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    async fn feed_page(&self, params: &[(String, String)], force: bool) -> Result<FeedResp> {
        let signed = self.sign(params.to_vec(), force).await?;
        let url = format!("{FEED_URL}?{signed}");
        let response = self.client.get(&url).send().await?;
        crate::debug_log(&format!("feed http={:?}", response.version()));
        let resp: FeedResp = response.json().await?;
        Ok(resp)
    }

    // -- Homepage recommendation feed ---------------------------------------
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
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
}

impl Session {
    /// Download and decode an image URL into raw RGBA8 pixels.
    pub async fn fetch_image(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        let bytes = self.get_bytes(url).await?;
        decode_rgba_blocking(bytes).await
    }
}

impl Session {
    /// Download and decode an image URL into raw RGBA8 pixels.
    pub async fn fetch_avatar(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        self.fetch_image(url).await
    }
}

impl Session {
    /// Like [`Session::fetch_image`] but requests the CDN's small webp variant
    /// (`@480w_270h_1c.webp`) and falls back to the raw URL when the CDN rejects
    /// the suffix or the variant cannot be decoded. Every attempt's status is
    /// counted for the f216 cover telemetry.
    pub async fn fetch_thumb(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        let started = Instant::now();
        let result = self.fetch_thumb_inner(url).await;
        let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        THUMB_MS_TOTAL.fetch_add(ms, Ordering::Relaxed);
        THUMB_MS_MAX.fetch_max(ms, Ordering::Relaxed);
        if result.is_ok() {
            THUMB_OK.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

impl Session {
    async fn fetch_thumb_inner(&self, url: &str) -> Result<(Vec<u8>, u32, u32)> {
        if url.is_empty() {
            bail!("empty image url");
        }
        if url.contains('@') {
            let (status, bytes) = self.get_bytes_status(url).await;
            match status {
                Some(status) => {
                    thumb_note_status(status);
                }
                None => {
                    THUMB_REQ_NET.fetch_add(1, Ordering::Relaxed);
                }
            }
            let bytes = bytes?;
            return match decode_rgba_blocking(bytes).await {
                Ok(pixels) => Ok(pixels),
                Err(err) => {
                    THUMB_DECODE_FAIL.fetch_add(1, Ordering::Relaxed);
                    Err(err)
                }
            };
        }
        let sized = format!("{url}{THUMB_SUFFIX}");
        let (status, bytes) = self.get_bytes_status(&sized).await;
        match status {
            Some(status) => {
                thumb_note_status(status);
            }
            None => {
                THUMB_REQ_NET.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Ok(bytes) = bytes {
            if !bytes.is_empty() {
                match decode_rgba_blocking(bytes).await {
                    Ok(pixels) => return Ok(pixels),
                    Err(_) => {
                        THUMB_DECODE_FAIL.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        THUMB_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        let (status, bytes) = self.get_bytes_status(url).await;
        match status {
            Some(status) => {
                thumb_note_status(status);
            }
            None => {
                THUMB_REQ_NET.fetch_add(1, Ordering::Relaxed);
            }
        }
        let bytes = bytes?;
        match decode_rgba_blocking(bytes).await {
            Ok(pixels) => Ok(pixels),
            Err(err) => {
                THUMB_DECODE_FAIL.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }
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

pub(crate) fn splitmix64(state: &mut u64) -> u64 {
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
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    #[derive(Clone, Debug, Serialize)]
pub struct CdnMeasurement {
        pub host: String,
        pub kind: CdnKind,
        /// Sustained bytes/s over the measurement window; `None` on failure.
        pub bps: Option<u64>,
        pub error: Option<String>,
    }

    /// The stream URL the player should use, plus what the probe saw.
    #[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct VideoCard {
    pub bvid: String,
    pub title: String,
    pub pic: String,
    pub owner: String,
    pub view: u64,
    pub duration: u64,
}

/// One paged card list plus its pagination state.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CardPage {
    pub page: i64,
    pub has_more: bool,
    pub items: Vec<VideoCard>,
}

/// One trending-search entry.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HotSearch {
    pub keyword: String,
    pub show_name: String,
}

// ---- 番剧 / PGC ------------------------------------------------------------
//
// Everything here is already normalised: URLs are absolute, the 追番/评分
// strings are the site's own text, and `duration_ms` is milliseconds exactly as
// the API reports it. The UI never parses an API payload.

/// One `archive/related` entry. Ads are entries without a `bvid` and/or with
/// `goto != "av"`; the live payload omits `goto` for normal videos.
#[derive(Deserialize)]
pub(crate) struct RelatedItem {
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
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
mod storyboard_tests {
    use super::{videoshot_from, ShotData};

    fn data(index: Vec<f64>, sheets: usize) -> ShotData {
        ShotData {
            img_x_len: 10,
            img_y_len: 10,
            img_x_size: 480,
            img_y_size: 270,
            sheets: (1..=sheets).map(|n| format!("//x/{n}.jpg")).collect(),
            index,
        }
    }

    /// 实测 356 s 视频: `[0,0,5,…,355]`，首位是重复的 0 哨兵。
    #[test]
    fn sentinel_zero_is_dropped() {
        let mut index = vec![0.0, 0.0];
        index.extend((1..72).map(|k| f64::from(k) * 5.0));
        let shot = match videoshot_from(data(index, 1)) {
            Ok(shot) => shot,
            Err(_) => return assert!(false, "356 s storyboard did not normalise"),
        };
        assert_eq!(shot.times.len(), 72);
        assert_eq!(shot.times[0], 0.0);
        assert_eq!(shot.times[71], 355.0);
        assert!(shot.sheets[0].starts_with("https://"));
    }

    /// 没有哨兵时不动第一位。
    #[test]
    fn times_without_sentinel_are_kept() {
        let shot = match videoshot_from(data(vec![0.0, 5.0, 10.0], 1)) {
            Ok(shot) => shot,
            Err(_) => return assert!(false, "storyboard did not normalise"),
        };
        assert_eq!(shot.times, vec![0.0, 5.0, 10.0]);
    }

    /// 时间戳多于格子数时截断（每张图 10x10）。
    #[test]
    fn tiles_are_capped_by_cells() {
        let index: Vec<f64> = (0..150).map(f64::from).collect();
        let shot = match videoshot_from(data(index, 1)) {
            Ok(shot) => shot,
            Err(_) => return assert!(false, "storyboard did not normalise"),
        };
        assert_eq!(shot.times.len(), 100);
    }

    #[test]
    fn missing_sheets_is_an_error() {
        let mut broken = data(vec![0.0, 0.0, 5.0], 0);
        broken.sheets.clear();
        assert!(videoshot_from(broken).is_err());
    }
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
    use crate::bilibili::account::heartbeat_body;

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

#[derive(Clone, Debug, Default, Serialize)]
pub struct VideoDetail {
    pub bvid: String,
    pub aid: i64,
    pub cid: i64,
    pub title: String,
    /// The upload's own 简介 (`desc`), verbatim. May be empty.
    pub description: String,
    /// Human category (`tname`, e.g. 知识·科学科普); empty when omitted.
    pub typename: String,
    pub pic: String,
    pub duration: u64,
    pub pubdate: i64,
    pub owner_name: String,
    pub owner_mid: i64,
    /// UP主 avatar from the `owner.face` block; empty when omitted.
    pub owner_face: String,
    pub stat_view: u64,
    pub stat_danmaku: u64,
    pub stat_like: u64,
    pub stat_coin: u64,
    pub stat_favorite: u64,
    pub stat_reply: u64,
    pub stat_share: u64,
    pub pages: Vec<VideoPart>,
    /// 合集 / 视频列表 (`ugc_season`), `None` when this video has no list.
    pub season: Option<Season>,
    /// Native picture size from `dimension` (`0` when the API omitted it). Used
    /// to derive which tiers a video can possibly offer.
    pub width: u32,
    pub height: u32,
}

/// One 分P of a video.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct Season {
    pub title: String,
    /// The site's own total (`ep_count`); the parser may have truncated the
    /// section list, so this can be larger than the episodes kept.
    pub ep_count: u32,
    pub sections: Vec<SeasonSection>,
}

/// One section of a 合集 (`ugc_season.sections[]`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct SeasonSection {
    /// The section title is what the header row shows.
    pub title: String,
    pub episodes: Vec<SeasonEpisode>,
}

/// One episode of a 合集. `cid` is what the player switches to; `bvid` is the
/// episode's own video id (older episodes of a 合集 are separate videos).
#[derive(Clone, Debug, Default, Serialize)]
pub struct SeasonEpisode {
    pub bvid: String,
    pub aid: i64,
    pub cid: i64,
    pub title: String,
    pub duration: u64,
}

/// Hard caps for a 合集 payload: the site allows far more, but a season list is
/// a *picker*, not a library, and the player's flat model must stay cheap.
pub(super) const SEASON_MAX_SECTIONS: usize = 20;

pub(super) const SEASON_MAX_EPISODES: usize = 500;

#[derive(Deserialize)]
pub(super) struct OfficialVerify {
    #[serde(default)]
    pub(super) desc: String,
}

/// Turn the parsed `view` payload into the [`VideoDetail`] the rest of the app
/// consumes. Split out so the season/分P rules can be unit-tested against
/// synthetic payloads without any network.
fn detail_from_view(data: ViewData) -> VideoDetail {
    let (owner_name, owner_mid, owner_face) = match data.owner {
        Some(owner) => (owner.name, owner.mid, normalize_url(&owner.face)),
        None => (String::new(), 0, String::new()),
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
        description: data.desc.trim().to_string(),
        typename: data.tname.trim().to_string(),
        pic: normalize_url(&data.pic),
        duration: data.duration,
        pubdate: data.pubdate,
        owner_name,
        owner_mid,
        owner_face,
        stat_view: stat.view,
        stat_danmaku: stat.danmaku,
        stat_like: stat.like,
        stat_coin: stat.coin,
        stat_favorite: stat.favorite,
        stat_reply: stat.reply,
        stat_share: stat.share,
        pages,
        season,
        width,
        height,
    }
}

#[derive(Deserialize)]
pub(super) struct ViewResp {
    #[serde(default)]
    pub(super) code: i64,
    #[serde(default)]
    pub(super) data: Option<ViewData>,
}

#[derive(Deserialize)]
pub(super) struct ViewData {
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    pub(super) aid: i64,
    #[serde(default)]
    pub(super) cid: i64,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub(super) desc: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub(super) pic: String,
    #[serde(default)]
    pub(super) duration: u64,
    #[serde(default)]
    pub(super) pubdate: i64,
    #[serde(default)]
    pub(super) tname: String,
    #[serde(default)]
    pub(super) owner: Option<ViewOwner>,
    #[serde(default)]
    pub(super) stat: Option<ViewStat>,
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
pub(super) struct ViewOwner {
    #[serde(default)]
    pub(super) mid: i64,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub(super) face: String,
}

#[derive(Deserialize, Default)]
pub(super) struct ViewStat {
    #[serde(default)]
    pub(super) view: u64,
    #[serde(default)]
    pub(super) danmaku: u64,
    #[serde(default)]
    pub(super) like: u64,
    #[serde(default)]
    pub(super) coin: u64,
    #[serde(default)]
    pub(super) favorite: u64,
    #[serde(default)]
    pub(super) reply: u64,
    #[serde(default)]
    pub(super) share: u64,
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

/// Fold one `archive/related` payload into playable feed items. Ads are entries
/// without a `bvid` or with a `goto` other than `av`; the live payload omits
/// `goto` on normal videos, so a missing/empty `goto` is a video, not an ad.
pub(crate) fn related_from(data: Vec<Option<RelatedItem>>) -> Vec<FeedVideo> {
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

pub(crate) fn short_num(value: u64) -> String {
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
pub(crate) fn length_seconds(length: &str) -> u64 {
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
    fn description_category_owner_face_and_stats_are_exposed() {
        let detail = detail(
            r#"{
                "bvid": "BV0000000009",
                "aid": 1009,
                "cid": 5009,
                "title": "简介示例",
                "desc": "  第一行\n第二行  ",
                "tname": "知识·科学科普",
                "owner": { "mid": 77, "name": "UP", "face": "//i0.hdslb.com/face.jpg" },
                "stat": { "view": 11, "danmaku": 12, "like": 13, "coin": 14,
                          "favorite": 15, "reply": 16, "share": 17 }
            }"#,
        );
        assert_eq!(detail.description, "第一行\n第二行");
        assert_eq!(detail.typename, "知识·科学科普");
        assert_eq!(detail.owner_face, "https://i0.hdslb.com/face.jpg");
        assert_eq!(detail.stat_view, 11);
        assert_eq!(detail.stat_danmaku, 12);
        assert_eq!(detail.stat_like, 13);
        assert_eq!(detail.stat_coin, 14);
        assert_eq!(detail.stat_favorite, 15);
        assert_eq!(detail.stat_reply, 16);
        assert_eq!(detail.stat_share, 17);
    }

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
