import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { CoverImage } from '../components/CoverImage';
import { useApp } from '../context';
import type { SearchHitPage } from '../types';
import { Empty, ErrorBox, PageHead, Tabs, VideoGrid } from '../components/Common';

const TABS = ['视频', '番剧', '影视', '直播', '用户'];

export default function Search() {
  const { route, navigate, toast, launch } = useApp();
  const keyword = route.query.get('q') || '';
  const [tab, setTab] = useState(0);
  const [page, setPage] = useState(1);
  const [data, setData] = useState<SearchHitPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const load = useCallback(async (nextPage: number, append: boolean) => {
    if (!keyword.trim()) return;
    setLoading(true);
    setError('');
    try {
      const result = await call<SearchHitPage>('search.hits', { keyword: keyword.trim(), tab, page: nextPage });
      setPage(nextPage);
      setData((current) => append && current && nextPage > 1
        ? { ...result, items: [...current.items, ...result.items] }
        : result);
    } catch (reason: any) {
      setError(reason?.message || String(reason));
    } finally {
      setLoading(false);
    }
  }, [keyword, tab]);

  useEffect(() => {
    if (keyword.trim()) load(1, false);
    else setData(null);
  }, [keyword, tab, load]);

  useEffect(() => {
    const onRefresh = () => {
      if (keyword.trim()) load(1, false);
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [keyword, load]);

  return (
    <>
      <PageHead
        eyebrow="发现"
        title={keyword ? keyword : '搜索'}
        sub={keyword
          ? (data ? `${formatCount(data.num_results)} 个结果 · 第 ${data.page} / ${Math.max(1, data.num_pages)} 页` : '正在搜索…')
          : '找到你想看的视频、番剧与 UP 主'}
      >
        {keyword ? (
          <Tabs tabs={TABS} active={tab} onChange={(next) => { setTab(next); setPage(1); }} />
        ) : null}
      </PageHead>

      {keyword ? (
        <>
          {error ? <ErrorBox error={error} onRetry={() => load(page, false)} /> : null}
          {!error && data && data.items.length === 0 && !loading ? <Empty title="没有找到相关结果" hint="换个关键词试试" /> : null}
          {data && data.items.length > 0 ? (
            tab === 0 ? (
              <VideoGrid
                items={data.items.map((hit: any) => hit.Video).filter(Boolean).map((item: any) => ({
                  bvid: item.bvid,
                  cid: 0,
                  aid: 0,
                  title: item.title,
                  pic: item.pic,
                  owner: item.author,
                  owner_mid: item.mid,
                  view: item.play,
                  duration: Number(String(item.duration).split(':').reduce((acc: number, part: string) => acc * 60 + Number(part || 0), 0)),
                  pub_ts: item.pub_ts,
                }))}
                onPlay={(item, newWindow) => launch({ bvid: item.bvid, title: item.title, cover: item.pic, owner: item.owner, duration: item.duration }, newWindow).catch((error) => toast(error.message, true))}
              />
            ) : tab === 4 ? (
              <div className="video-grid">
                {data.items.map((hit: any, index) => {
                  const user = hit.User;
                  if (!user) return null;
                  return (
                    <div className="card panel search-hit-card" key={`${user.mid}-${index}`} style={{ cursor: 'pointer' }} onClick={() => navigate(`user/${user.mid}`)}>
                      <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
                        <img className="avatar lg" src={cover(user.upic)} referrerPolicy="no-referrer" alt="" />
                        <div style={{ minWidth: 0 }}>
                          <div style={{ fontWeight: 600 }}>{user.uname}</div>
                          <div style={{ color: 'var(--text-faint)', fontSize: 11.5, marginTop: 4 }}>
                            {formatCount(user.fans)} 粉丝 · {formatCount(user.videos)} 投稿
                          </div>
                        </div>
                      </div>
                      <div style={{ color: 'var(--text-dim)', fontSize: 12, marginTop: 10, lineHeight: 1.5, minHeight: 36 }}>{user.usign}</div>
                    </div>
                  );
                })}
              </div>
            ) : tab === 3 ? (
              <div className="video-grid">
                {data.items.map((hit: any, index) => {
                  const live = hit.Live;
                  if (!live) return null;
                  return (
                    <div className="video-card" key={`${live.room_id}-${index}`} onClick={() => navigate(`live?room=${live.room_id}`)}>
                      <div className="cover">
                        <CoverImage src={live.cover} alt="" />
                        <div className="live-badge">直播</div>
                        <div className="cover-badge">{formatCount(live.online)} 人气</div>
                      </div>
                      <div className="body">
                        <div className="video-title">{live.title}</div>
                        <div className="video-meta">
                          <span className="video-author">{live.uname}</span>
                          <span className="video-trail">{live.area_name || '直播'}</span>
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
            ) : (
              <div className="video-grid">
                {data.items.map((hit: any, index) => {
                  const media = hit.Media;
                  if (!media) return null;
                  return (
                    <div className="video-card" key={`${media.season_id}-${index}`} onClick={() => navigate(`pgc?season=${media.season_id}`)}>
                      <div className="cover">
                        <CoverImage src={media.cover} alt="" />
                        {media.season_type_name ? <div className="cover-badge">{media.season_type_name}</div> : null}
                      </div>
                      <div className="body">
                        <div className="video-title">{media.title}</div>
                        <div className="video-meta">
                          <span className="video-author">{media.season_type_name || '番剧'}</span>
                          {media.score_text ? <span className="video-trail">{media.score_text}</span> : null}
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
            )
          ) : null}
          {data && data.items.length > 0 && data.page < data.num_pages ? (
            <div className="page-more">
              <button className="btn" disabled={loading} onClick={() => load(page + 1, true)}>{loading ? '加载中…' : '加载更多'}</button>
            </div>
          ) : null}
        </>
      ) : null}
    </>
  );
}
