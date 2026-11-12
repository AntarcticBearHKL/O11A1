import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { SpaceVideoPage, UpInfo } from '../types';
import { ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';

export default function User({ mid }: { mid: number }) {
  const { launch, toast } = useApp();
  const [info, setInfo] = useState<UpInfo | null>(null);
  const [relation, setRelation] = useState<{ following: number; follower: number } | null>(null);
  const [videos, setVideos] = useState<any[]>([]);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    if (!mid) return;
    setLoading(true);
    setError('');
    try {
      const data = await call<any>('user.space', { mid });
      setInfo(data.info);
      setRelation(data.relation);
      setVideos(data.videos?.items || []);
      setHasMore(!!data.videos?.has_more);
      setPage(1);
    } catch (reason: any) {
      setError(reason?.message || String(reason));
    } finally {
      setLoading(false);
    }
  }, [mid]);

  useEffect(() => { load(); }, [load]);

  useEffect(() => {
    const onRefresh = () => load();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  const more = async () => {
    try {
      const next: SpaceVideoPage = await call('user.videos', { mid, page: page + 1 });
      setVideos((current) => [...current, ...next.items]);
      setPage(page + 1);
      setHasMore(next.has_more);
    } catch (reason: any) { toast(reason.message, true); }
  };

  const follow = async () => {
    try {
      await call('user.follow', { mid, follow: true });
      toast('已关注');
    } catch (reason: any) { toast(reason.message, true); }
  };

  if (loading) return <SkeletonGrid count={8} />;
  if (error || !info) return <ErrorBox error={error || '没有找到 UP 主'} onRetry={load} />;

  return (
    <>
      <div className="card panel" style={{ display: 'flex', gap: 18, alignItems: 'center', marginBottom: 18 }}>
        <img className="avatar lg" style={{ width: 82, height: 82 }} src={cover(info.face)} referrerPolicy="no-referrer" alt="" />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <h1 style={{ margin: 0, fontSize: 24 }}>{info.name}</h1>
            <span className="chip small">LV{info.level}</span>
            {info.vip ? <span className="chip small" style={{ color: 'var(--accent)' }}>{info.vip}</span> : null}
          </div>
          <div style={{ color: 'var(--text-dim)', fontSize: 12.5, marginTop: 8, lineHeight: 1.55 }}>{info.sign || '这个人很神秘，什么都没有写'}</div>
          <div style={{ display: 'flex', gap: 18, marginTop: 10, color: 'var(--text-faint)', fontSize: 12 }}>
            <span>{formatCount(info.fans)} 粉丝</span>
            <span>{formatCount(info.friend)} 关注</span>
            {relation ? <span>{formatCount(relation.follower)} 关注者</span> : null}
          </div>
        </div>
        <button className="btn primary" onClick={follow}>关注</button>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', marginBottom: 14 }}>
        <h2 style={{ margin: 0, fontSize: 18 }}>投稿</h2>
        <span style={{ marginLeft: 10, color: 'var(--text-faint)', fontSize: 12 }}>{videos.length} 个视频</span>
      </div>
      {videos.length ? (
        <VideoGrid
          items={videos.map((item) => ({
            bvid: item.bvid,
            cid: 0,
            aid: 0,
            title: item.title,
            pic: item.pic,
            owner: info.name,
            owner_mid: mid,
            view: item.play,
            duration: item.length,
            pub_ts: 0,
          }))}
          onPlay={(item) => launch({ bvid: item.bvid, title: item.title, cover: item.pic, owner: info.name, duration: item.duration }).catch((error) => toast(error.message, true))}
        />
      ) : <div className="empty">还没有投稿</div>}
      {hasMore ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: 22 }}>
          <button className="btn" onClick={more}>加载更多</button>
        </div>
      ) : null}
    </>
  );
}
