import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { SpaceVideoPage, UpInfo } from '../types';
import { ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';
import { IconBack } from '../components/Icons';

export default function User({ mid }: { mid: number }) {
  const { launch, toast, navigate } = useApp();
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

  const goBack = () => {
    if (window.history.length > 1) window.history.back();
    else navigate('home');
  };

  return (
    <>
      <div className="page-back">
        <button className="btn ghost sm" onClick={goBack} title="返回上一页"><IconBack size={13} />返回</button>
      </div>
      <section className="profile-hero">
        <div className="profile-avatar-wrap">
          <img className="profile-avatar" src={cover(info.face)} referrerPolicy="no-referrer" alt="" />
        </div>
        <div className="profile-copy">
          <div className="profile-name">
            <h1>{info.name}</h1>
            <span className="chip small">LV{info.level}</span>
            {info.vip ? <span className="chip small accent-chip">{info.vip}</span> : null}
          </div>
          <p className="profile-sign">{info.sign || '这个人很神秘，什么都没有写'}</p>
          <div className="profile-stats">
            <span><b>{formatCount(info.fans)}</b>粉丝</span>
            <span><b>{formatCount(info.friend)}</b>关注</span>
            {relation ? <span><b>{formatCount(relation.follower)}</b>关注者</span> : null}
          </div>
        </div>
        <button className="btn primary profile-follow" onClick={follow}>+ 关注</button>
      </section>
      <div className="section-head">
        <div><span className="section-kicker">作品</span><h2>投稿</h2></div>
        <span className="section-meta">{videos.length} 个视频</span>
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
          onPlay={(item, newWindow) => launch({ bvid: item.bvid, title: item.title, cover: item.pic, owner: info.name, duration: item.duration }, newWindow).catch((error) => toast(error.message, true))}
        />
      ) : <div className="empty">还没有投稿</div>}
      {hasMore ? (
        <div className="page-more">
          <button className="btn" onClick={more}>加载更多</button>
        </div>
      ) : null}
    </>
  );
}
