import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { LiveAreaPage, LiveIndex, LiveRoom } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid } from '../components/Common';

const QUALITIES: Array<[number, string]> = [[10000, '原画'], [400, '蓝光'], [250, '超清'], [150, '高清'], [80, '流畅']];

export default function Live() {
  const { route, navigate, launch, toast } = useApp();
  const roomId = Number(route.query.get('room') || 0);
  const [index, setIndex] = useState<LiveIndex | null>(null);
  const [area, setArea] = useState(0);
  const [rooms, setRooms] = useState<LiveRoom[]>([]);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [room, setRoom] = useState<any>(null);
  const [quality, setQuality] = useState(10000);
  const [playing, setPlaying] = useState(false);

  const loadIndex = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const data = await call<LiveIndex>('live.index');
      setIndex(data);
      setRooms(data.modules.flatMap((module) => module.rooms));
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, []);

  const loadArea = useCallback(async (areaId: number, parentId: number, nextPage: number, append: boolean) => {
    setLoading(true);
    setError('');
    try {
      const data = await call<LiveAreaPage>('live.area', { area: areaId, parent: parentId, page: nextPage });
      setRooms((current) => append ? [...current, ...data.rooms] : data.rooms);
      setPage(nextPage);
      setHasMore(data.has_more);
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, []);

  const loadRoom = useCallback(async () => {
    if (!roomId) return;
    setLoading(true);
    setError('');
    try {
      setRoom(await call<any>('live.room', { roomId }));
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [roomId]);

  useEffect(() => {
    if (roomId) {
      loadRoom();
      const timer = setInterval(loadRoom, 30000);
      return () => clearInterval(timer);
    }
    loadIndex();
  }, [roomId, loadRoom, loadIndex]);

  useEffect(() => {
    const onRefresh = () => {
      if (roomId) {
        loadRoom();
        return;
      }
      if (area) {
        const parent = index?.areas.find((item) => item.id === area)?.parent_id || 0;
        loadArea(area, parent, 1, false);
        return;
      }
      loadIndex();
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [area, index, loadArea, loadIndex, loadRoom, roomId]);

  const play = async () => {
    setPlaying(true);
    try {
      const play = await call<any>('live.play', { roomId, qn: quality });
      if (play.live_status !== 1) throw new Error(`直播状态为 ${play.live_status}，当前不可播放`);
      const url = play.flv || play.hls_fmp4 || play.hls_ts || play.hls;
      if (!url) throw new Error('没有可用的直播流');
      await launch({ url, title: `${room?.title || '直播'} · ${room?.area_name || ''}`, cover: room?.cover });
    } catch (reason: any) { toast(reason.message, true); }
    finally { setPlaying(false); }
  };

  if (roomId) {
    if (loading && !room) return <SkeletonGrid count={2} />;
    if (error || !room) return <ErrorBox error={error || '没有找到直播间'} onRetry={loadRoom} />;
    return (
      <>
        <PageHead title={room.title} sub={`${room.area_name} · ${room.online ? `${formatCount(room.online)} 人气` : '未开播'}`}>
          <button className="btn sm" onClick={() => navigate('live')}>返回直播</button>
        </PageHead>
        <div className="card hero" style={{ marginBottom: 16 }}>
          <img className="hero-cover" src={cover(room.cover)} referrerPolicy="no-referrer" alt="" />
          <div>
            <h1 className="hero-title">{room.title}</h1>
            <div className="stats" style={{ marginBottom: 12 }}>
              <span>状态<strong>{room.live_status === 1 ? '直播中' : '未开播'}</strong></span>
              <span>分区<strong>{room.area_name}</strong></span>
              <span>人气<strong>{formatCount(room.online)}</strong></span>
            </div>
            <div className="chips" style={{ marginBottom: 14 }}>
              {QUALITIES.map(([value, label]) => (
                <button key={value} className={`chip small ${quality === value ? 'active' : ''}`} onClick={() => setQuality(value)}>{label}</button>
              ))}
            </div>
            <button className="btn primary" disabled={playing || room.live_status !== 1} onClick={play}>{playing ? '连接中…' : '▶ 观看直播'}</button>
            <div style={{ color: 'var(--text-faint)', fontSize: 11.5, marginTop: 10 }}>直播每 30 秒刷新一次状态；切换画质会重新连接直播流。</div>
          </div>
        </div>
        <div className="card panel">
          <div className="panel-title">关于这个直播间</div>
          <div className="kv">
            <div className="k">房间号</div><div className="v">{room.room_id}</div>
            <div className="k">主播 UID</div><div className="v">{room.uid || '-'}</div>
            <div className="k">直播状态</div><div className="v">{room.live_status}</div>
            <div className="k">是否竖屏</div><div className="v">{room.is_portrait ? '是' : '否'}</div>
          </div>
        </div>
      </>
    );
  }

  const areas = index?.areas || [];
  return (
    <>
      <PageHead title="直播" sub="推荐 / 分区 / 房间">
        <button className="btn sm" onClick={() => { setArea(0); setRooms(index?.modules.flatMap((m) => m.rooms) || []); }}>推荐</button>
      </PageHead>

      {areas.length ? (
        <div className="chips" style={{ marginBottom: 16 }}>
          {areas.map((item) => (
            <button
              key={item.id}
              className={`chip small ${area === item.id ? 'active' : ''}`}
              onClick={() => { setArea(item.id); loadArea(item.id, item.parent_id, 1, false); }}
            >
              {item.title}
            </button>
          ))}
        </div>
      ) : null}

      {loading ? <SkeletonGrid count={8} /> : error ? <ErrorBox error={error} onRetry={() => area ? loadArea(area, areas.find((item) => item.id === area)?.parent_id || 0, 1, false) : loadIndex()} /> : rooms.length ? (
        <div className="video-grid">
          {rooms.map((item, index) => (
            <div className="video-card" key={`${item.room_id}-${index}`} onClick={() => navigate(`live?room=${item.room_id}`)}>
              <div className="cover">
                <img src={cover(item.cover)} referrerPolicy="no-referrer" alt="" />
                {item.live_status === 1 ? <div className="live-badge">直播中</div> : null}
                <div className="cover-badge">{formatCount(item.online)} 人气</div>
              </div>
              <div className="body">
                <div className="video-title">{item.title}</div>
                <div className="video-meta">
                  <span className="video-author">{item.uname}</span>
                  {item.area_name ? <span className="video-trail">{item.area_name}</span> : null}
                </div>
              </div>
            </div>
          ))}
        </div>
      ) : <Empty title="这里暂时没有直播" />}

      {area && hasMore ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: 22 }}>
          <button className="btn" onClick={() => loadArea(area, areas.find((item) => item.id === area)?.parent_id || 0, page + 1, true)}>加载更多</button>
        </div>
      ) : null}
    </>
  );
}
