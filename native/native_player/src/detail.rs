//! Background metadata/comments worker for the player's right strip.
//!
//! The player thread never blocks on the network. This thread reuses the
//! session that resolved the media, fetches `video_view` once, then serves the
//! 信息/评论 tabs and the like/coin/favourite actions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::log;
use crate::overlay::ImageData;

static IMAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub struct PageData {
    pub cid: u64,
    pub title: String,
    pub duration: u64,
}

#[derive(Clone, Default)]
pub struct InfoData {
    pub aid: i64,
    pub owner: String,
    pub title: String,
    pub views: u64,
    pub pubdate: i64,
    pub description: String,
    pub pages: Vec<PageData>,
    pub current_page: usize,
    pub cover: Option<ImageData>,
}

#[derive(Clone, Default)]
pub struct CommentData {
    pub user: String,
    pub message: String,
    pub likes: u64,
    pub avatar_url: String,
}

#[derive(Clone, Default)]
pub struct CommentsData {
    pub comments: Vec<CommentData>,
    pub total: i64,
    pub cursor: String,
    pub end: bool,
    pub reset: bool,
}

pub enum DetailRequest {
    Info,
    Comments {
        hot: bool,
        reset: bool,
        cursor: String,
    },
    Like,
    Coin,
    Favorite,
}

pub enum DetailEvent {
    Info(Result<InfoData, String>),
    Comments(Result<CommentsData, String>),
    Avatars(Vec<(String, ImageData)>),
    Toast {
        text: String,
        error: bool,
    },
    States {
        liked: bool,
        coined: bool,
        favorited: bool,
    },
}

fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    o11a1::bilibili::shared_runtime()
}

fn image(id: u64, (rgba, width, height): (Vec<u8>, u32, u32)) -> ImageData {
    ImageData {
        id,
        width,
        height,
        rgba: Arc::new(rgba),
    }
}

/// Spawn the worker. The returned request channel and event receiver are wired
/// into the engine; dropping the sender ends the thread.
pub fn spawn(
    session: Arc<o11a1::bilibili::Session>,
    bvid: String,
    cid: u64,
) -> (Sender<DetailRequest>, Receiver<DetailEvent>) {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("player-detail".into())
        .spawn(move || run(session, bvid, cid, request_rx, event_tx));
    (request_tx, event_rx)
}

fn run(
    session: Arc<o11a1::bilibili::Session>,
    bvid: String,
    cid: u64,
    requests: Receiver<DetailRequest>,
    events: Sender<DetailEvent>,
) {
    let mut info: Option<InfoData> = None;
    let mut liked = false;
    let mut coined = false;
    let mut favorited = false;
    while let Ok(request) = requests.recv() {
        match request {
            DetailRequest::Info => match load_info(&session, &bvid, cid) {
                Ok(data) => {
                    if let Ok(state) = runtime()
                        .ok_or_else(|| anyhow!("no runtime"))
                        .map(|runtime| {
                            runtime.block_on(async {
                                let liked = session.like_state(&bvid).await.unwrap_or(0) > 0;
                                let coined = session.coin_state(&bvid).await.unwrap_or(0) > 0;
                                let favorited = session
                                    .fav_state(data.aid)
                                    .await
                                    .map(|(fav, _)| fav)
                                    .unwrap_or(false);
                                (liked, coined, favorited)
                            })
                        })
                    {
                        let _ = events.send(DetailEvent::States {
                            liked: state.0,
                            coined: state.1,
                            favorited: state.2,
                        });
                        liked = state.0;
                        coined = state.1;
                        favorited = state.2;
                    }
                    info = Some(data.clone());
                    let _ = events.send(DetailEvent::Info(Ok(data)));
                }
                Err(error) => {
                    let _ = events.send(DetailEvent::Info(Err(format!("{error:#}"))));
                }
            },
            DetailRequest::Comments {
                hot,
                reset,
                cursor,
            } => {
                let Some(data) = info.as_ref() else {
                    let _ = events.send(DetailEvent::Comments(Err("视频信息尚未加载".into())));
                    continue;
                };
                match load_comments(&session, data.aid, hot, &cursor, reset) {
                    Ok((comments, total, next, end)) => {
                        let avatars: Vec<(String, String)> = comments
                            .iter()
                            .filter(|comment| !comment.avatar_url.is_empty())
                            .take(10)
                            .map(|comment| (comment.avatar_url.clone(), comment.user.clone()))
                            .collect();
                        let _ = events.send(DetailEvent::Comments(Ok(CommentsData {
                            comments,
                            total,
                            cursor: next,
                            end,
                            reset,
                        })));
                        if !avatars.is_empty() {
                            if let Some(runtime) = runtime() {
                                let mut loaded = Vec::new();
                                for (url, _) in avatars {
                                    if let Ok(data) = runtime.block_on(session.fetch_avatar(&url)) {
                                        let id = IMAGE_ID.fetch_add(1, Ordering::Relaxed);
                                        loaded.push((url, image(id, data)));
                                    }
                                }
                                log::line(&format!("detail avatars loaded n={}", loaded.len()));
                                if !loaded.is_empty() {
                                    let _ = events.send(DetailEvent::Avatars(loaded));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let _ =
                            events.send(DetailEvent::Comments(Err(format!("{error:#}"))));
                    }
                }
            }
            DetailRequest::Like => {
                let result = runtime()
                    .ok_or_else(|| anyhow!("no runtime"))
                    .and_then(|runtime| {
                        runtime.block_on(async {
                            let next = !liked;
                            session.like(&bvid, next).await?;
                            Ok(next)
                        })
                    });
                match result {
                    Ok(next) => {
                        liked = next;
                        let _ = events.send(DetailEvent::States {
                            liked,
                            coined,
                            favorited,
                        });
                        let _ = events.send(DetailEvent::Toast {
                            text: if next { "已点赞" } else { "已取消点赞" }.into(),
                            error: false,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(DetailEvent::Toast {
                            text: format!("点赞失败：{error}"),
                            error: true,
                        });
                    }
                }
            }
            DetailRequest::Coin => {
                let result = runtime()
                    .ok_or_else(|| anyhow!("no runtime"))
                    .and_then(|runtime| runtime.block_on(session.add_coin(&bvid, 1, true)));
                match result {
                    Ok(()) => {
                        coined = true;
                        let _ = events.send(DetailEvent::States {
                            liked: true,
                            coined,
                            favorited,
                        });
                        let _ = events.send(DetailEvent::Toast {
                            text: "已投币".into(),
                            error: false,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(DetailEvent::Toast {
                            text: format!("投币失败：{error}"),
                            error: true,
                        });
                    }
                }
            }
            DetailRequest::Favorite => {
                let Some(data) = info.as_ref() else {
                    continue;
                };
                let result = runtime()
                    .ok_or_else(|| anyhow!("no runtime"))
                    .and_then(|runtime| {
                        runtime.block_on(async {
                            let folders = session.fav_folders_all(data.aid).await?;
                            if let Some(folder) = folders.iter().find(|folder| folder.member == Some(true)) {
                                session.fav_deal(data.aid, &[], &[folder.id]).await?;
                                Ok(false)
                            } else if let Some(folder) = folders.first() {
                                session.fav_deal(data.aid, &[folder.id], &[]).await?;
                                Ok(true)
                            } else {
                                Err(anyhow!("没有可用的收藏夹"))
                            }
                        })
                    });
                match result {
                    Ok(next) => {
                        favorited = next;
                        let _ = events.send(DetailEvent::States {
                            liked,
                            coined,
                            favorited,
                        });
                        let _ = events.send(DetailEvent::Toast {
                            text: if next { "已收藏" } else { "已取消收藏" }.into(),
                            error: false,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(DetailEvent::Toast {
                            text: format!("收藏失败：{error}"),
                            error: true,
                        });
                    }
                }
            }
        }
    }
    log::line("detail worker stopped");
}

fn load_info(
    session: &o11a1::bilibili::Session,
    bvid: &str,
    cid: u64,
) -> Result<InfoData> {
    let runtime = runtime().ok_or_else(|| anyhow!("no runtime"))?;
    let detail = runtime
        .block_on(session.video_view(bvid))
        .map_err(|error| anyhow!("video_view: {error:#}"))?;
    let cover = runtime
        .block_on(session.fetch_thumb(&detail.pic))
        .ok()
        .map(|data| {
            let id = IMAGE_ID.fetch_add(1, Ordering::Relaxed);
            image(id, data)
        });
    let pages: Vec<PageData> = detail
        .pages
        .iter()
        .map(|page| PageData {
            cid: page.cid as u64,
            title: page.title.clone(),
            duration: page.duration,
        })
        .collect();
    let current_page = pages
        .iter()
        .position(|page| page.cid == cid)
        .unwrap_or(0);
    log::line(&format!(
        "detail info bvid={bvid} aid={} owner={:?} pages={} cover={}",
        detail.aid,
        detail.owner_name,
        pages.len(),
        u8::from(cover.is_some())
    ));
    Ok(InfoData {
        aid: detail.aid,
        owner: detail.owner_name,
        title: detail.title,
        views: detail.stat_view,
        pubdate: detail.pubdate,
        description: detail.description,
        pages,
        current_page,
        cover,
    })
}

fn load_comments(
    session: &o11a1::bilibili::Session,
    aid: i64,
    hot: bool,
    cursor: &str,
    _reset: bool,
) -> Result<(Vec<CommentData>, i64, String, bool)> {
    let runtime = runtime().ok_or_else(|| anyhow!("no runtime"))?;
    let page = runtime
        .block_on(session.comments(aid, hot, cursor))
        .map_err(|error| anyhow!("comments: {error:#}"))?;
    let mut comments = Vec::new();
    if let Some(top) = page.top {
        // Some payloads return a pinned shell without a member/message. An
        // empty row is worse than dropping the pin.
        if !top.user.uname.trim().is_empty() && !top.message.trim().is_empty() {
            comments.push(comment_from(top));
        }
    }
    comments.extend(page.comments.into_iter().map(comment_from));
    log::line(&format!(
        "detail comments aid={aid} hot={} n={} total={} end={}",
        u8::from(hot),
        comments.len(),
        page.total,
        u8::from(page.is_end)
    ));
    Ok((comments, page.total, page.cursor, page.is_end))
}

fn comment_from(comment: o11a1::bilibili::Comment) -> CommentData {
    let mut avatar = comment.user.avatar;
    if avatar.starts_with("//") {
        avatar.insert_str(0, "https:");
    }
    CommentData {
        user: comment.user.uname,
        message: comment.message.replace('\n', " "),
        likes: comment.like,
        avatar_url: avatar,
    }
}
