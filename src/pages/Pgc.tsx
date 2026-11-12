import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { PgcEpisode, PgcIndexPage, PgcSeason } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, Tabs } from '../components/Common';
import { resolvePgcSource } from '../player';

const TYPES: Array<[number, string]> = [
  [1, '番剧'], [4, '国创'], [2, '电影'], [3, '纪录片'], [5, '电视剧'], [7, '综艺'],
];

export default function Pgc() {
  const { route, navigate, launch, toast } = useApp();
  const seasonId = Number(route.query.get('season') || 0);
  const [view, setView] = useState<'daily' | 'index' | 'rank'>(route.query.get('view') === 'index' ? 'index' : route.query.get('view') === 'rank' ? 'rank' : 'daily');
  const [type, setType] = useState(1);
  const [order, setOrder] = useState(3);
  const [timeline, setTimeline] = useState<any[]>([]);
  const [index, setIndex] = useState<PgcIndexPage | null>(null);
  const [rank, setRank] = useState<any[]>([]);
  const [page, setPage] = useState(1);
  const [season, setSeason] = useState<PgcSeason | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [playing, setPlaying] = useState(0);

  const loadSeason = useCallback(async () => {
    if (!seasonId) return;
    setLoading(true);
    setError('');
    try {
      setSeason(await call<PgcSeason>('pgc.season', { seasonId }));
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [seasonId]);

  const loadView = useCallback(async () => {
    if (seasonId) { loadSeason(); return; }
    setLoading(true);
    setError('');
    try {
      if (view === 'daily') {
        setTimeline(await call<any[]>('pgc.timeline'));
      } else if (view === 'index') {
        const data = await call<PgcIndexPage>('pgc.index', { seasonType: type, page, order });
        setIndex(data);
      } else {
        setRank(await call<any[]>('pgc.rank', { seasonType: type }));
      }
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [seasonId, view, type, page, order, loadSeason]);

  useEffect(() => { loadView(); }, [loadView]);

  useEffect(() => {
    const onRefresh = () => loadView();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [loadView]);

  const playEpisode = async (ep: PgcEpisode) => {
    setPlaying(ep.ep_id);
    try {
      const source = await resolvePgcSource(ep.ep_id, ep.cid);
      await launch({
        url: source.url,
        audioUrl: source.audioUrl,
        epId: ep.ep_id,
        cid: ep.cid,
        title: `${season?.title || ''} ${ep.title} ${ep.long_title || ''}`.trim(),
        cover: season?.cover,
        episodes: (season?.episodes || []).map((row) => ({ ep_id: row.ep_id, cid: row.cid, title: row.title, long_title: row.long_title })),
      });
    } catch (reason: any) {
      toast(reason.message, true);
    } finally {
      setPlaying(0);
    }
  };

  if (seasonId) {
    return (
      <>
        <PageHead title="番剧详情" sub={season?.index_show || ''}>
          <button className="btn sm" onClick={() => navigate('pgc')}>返回索引</button>
        </PageHead>
        {loading ? <SkeletonGrid count={6} /> : error ? <ErrorBox error={error} onRetry={loadSeason} /> : season ? (
          <>
            <div className="card hero" style={{ marginBottom: 16 }}>
              <img className="hero-cover" src={cover(season.cover)} referrerPolicy="no-referrer" alt="" style={{ aspectRatio: '3/4' }} />
              <div>
                <h1 className="hero-title">{season.title}</h1>
                <div className="stats" style={{ marginBottom: 10 }}>
                  <span>评分<strong>{season.score ? season.score.toFixed(1) : '暂无'}</strong></span>
                  <span>{season.areas.join(' / ')}</span>
                  <span>{season.styles.join(' / ')}</span>
                  <span>{season.is_finish ? '已完结' : '连载中'}</span>
                </div>
                <div className="hero-desc" style={{ maxHeight: 120 }}>{season.evaluate || '暂无简介'}</div>
                <div style={{ display: 'flex', gap: 8, marginTop: 14 }}>
                  {season.episodes[0] ? <button className="btn primary" disabled={!!playing} onClick={() => playEpisode(season.episodes[0])}>▶ 播放第 1 集</button> : null}
                  <button className="btn" onClick={() => navigate(`pgc?season=${season.season_id}&view=index`)}>相关推荐</button>
                </div>
              </div>
            </div>
            <div className="card panel">
              <div className="panel-title">选集（{season.episodes.length}）</div>
              <div className="episode-grid" style={{ gridTemplateColumns: 'repeat(8, 1fr)' }}>
                {season.episodes.map((ep, index) => (
                  <button key={ep.ep_id} className={`episode ${playing === ep.ep_id ? 'active' : ''}`} title={`${ep.title} ${ep.long_title}`} onClick={() => playEpisode(ep)} disabled={!!playing}>
                    {ep.title || `第 ${index + 1} 集`}
                  </button>
                ))}
              </div>
              {season.sections.map((section) => (
                <React.Fragment key={section.title}>
                  <div className="panel-title" style={{ marginTop: 18 }}>{section.title}</div>
                  <div className="episode-grid" style={{ gridTemplateColumns: 'repeat(8, 1fr)' }}>
                    {section.episodes.map((ep, index) => (
                      <button key={ep.ep_id} className={`episode ${playing === ep.ep_id ? 'active' : ''}`} onClick={() => playEpisode(ep)} disabled={!!playing}>
                        {ep.title || `第 ${index + 1} 集`}
                      </button>
                    ))}
                  </div>
                </React.Fragment>
              ))}
            </div>
          </>
        ) : <Empty title="没有找到番剧" />}
      </>
    );
  }

  return (
    <>
      <PageHead title="番剧 / 影视" sub="每日放送 · 索引 · 排行榜">
        <Tabs tabs={['每日放送', '索引', '排行榜']} active={view === 'daily' ? 0 : view === 'index' ? 1 : 2} onChange={(next) => { setView(next === 0 ? 'daily' : next === 1 ? 'index' : 'rank'); setPage(1); }} />
      </PageHead>

      {view !== 'daily' ? (
        <div style={{ display: 'flex', gap: 8, marginBottom: 16, flexWrap: 'wrap', alignItems: 'center' }}>
          {TYPES.map(([value, label]) => (
            <button key={value} className={`chip ${type === value ? 'active' : ''}`} onClick={() => { setType(value); setPage(1); }}>{label}</button>
          ))}
          {view === 'index' ? (
            <select className="select" style={{ marginLeft: 8 }} value={order} onChange={(event) => { setOrder(Number(event.target.value)); setPage(1); }}>
              <option value={3}>追番人数</option>
              <option value={0}>更新时间</option>
              <option value={1}>播放数量</option>
              <option value={2}>评分</option>
            </select>
          ) : null}
        </div>
      ) : null}

      {loading ? <SkeletonGrid count={8} /> : error ? <ErrorBox error={error} onRetry={loadView} /> : view === 'daily' ? (
        timeline.length ? timeline.map((day: any) => (
          <div key={day.date} style={{ marginBottom: 18 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10 }}>
              <h3 style={{ margin: 0, fontSize: 15 }}>{day.date}</h3>
              {day.is_today ? <span className="chip small active">今天</span> : null}
            </div>
            <div className="pgc-grid">
              {day.episodes.map((ep: any) => (
                <div className="pgc-card" key={`${day.date}-${ep.season_id}-${ep.pub_index}`} onClick={() => navigate(`pgc?season=${ep.season_id}`)}>
                  <div className="pgc-cover">
                    <img src={cover(ep.cover)} referrerPolicy="no-referrer" alt="" />
                    {ep.pub_time ? <div className="pgc-badge">{ep.pub_time}</div> : null}
                  </div>
                  <div className="pgc-body">
                    <div className="video-title">{ep.title}</div>
                    <div className="video-meta"><span className="video-author">{ep.pub_index || '今日更新'}</span></div>
                  </div>
                </div>
              ))}
            </div>
          </div>
        )) : <Empty title="今天没有放送" />
      ) : view === 'index' ? (
        index && index.items.length ? (
          <>
            <div className="pgc-grid">
              {index.items.map((item) => {
                const badge = item.badge || (item.score ? `${item.score}分` : item.order);
                const meta = [item.index_show, item.score ? `${item.score}分` : ''].filter(Boolean).join(' · ') || item.order || '暂无信息';
                return (
                  <div className="pgc-card" key={item.season_id} onClick={() => navigate(`pgc?season=${item.season_id}`)}>
                    <div className="pgc-cover">
                      <img src={cover(item.cover)} referrerPolicy="no-referrer" alt="" />
                      {badge ? <div className="pgc-badge">{badge}</div> : null}
                    </div>
                    <div className="pgc-body">
                      <div className="video-title">{item.title}</div>
                      <div className="video-meta"><span className="video-author">{meta}</span></div>
                    </div>
                  </div>
                );
              })}
            </div>
            <div style={{ display: 'flex', justifyContent: 'center', padding: 20 }}>
              {index.has_next ? <button className="btn" onClick={() => setPage(page + 1)}>下一页</button> : <span style={{ color: 'var(--text-faint)', fontSize: 12 }}>已经到底了</span>}
            </div>
          </>
        ) : <Empty title="没有索引结果" />
      ) : (
        <div className="card panel">
          {rank.map((item: any) => (
            <div className="row" key={item.season_id} style={{ cursor: 'pointer' }} onClick={() => navigate(`pgc?season=${item.season_id}`)}>
              <div style={{ width: 32, textAlign: 'center', fontWeight: 600, color: item.rank <= 3 ? 'var(--accent)' : 'var(--text-faint)' }}>{item.rank}</div>
              <img src={cover(item.cover)} referrerPolicy="no-referrer" alt="" style={{ width: 52, aspectRatio: '3/4', objectFit: 'cover', borderRadius: 7 }} />
              <div style={{ flex: 1 }}>
                <div className="row-title">{item.title}</div>
                <div className="row-sub">{item.index_show} · {item.rating} · {formatCount(item.follow)} 追番</div>
              </div>
              <span className="chip small">{item.badge}</span>
            </div>
          ))}
          {!rank.length ? <Empty title="排行榜为空" /> : null}
        </div>
      )}
    </>
  );
}
