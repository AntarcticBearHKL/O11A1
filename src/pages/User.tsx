import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { FeedVideo, SpaceVideoPage, UpInfo } from '../types';
import { ErrorBox, SkeletonGrid, Tabs, VideoGrid } from '../components/Common';

type VideoOrder = 'newest' | 'plays';

const ORDER_TABS: Array<{ key: VideoOrder; label: string }> = [
  { key: 'newest', label: '最新发布' },
  { key: 'plays', label: '播放最多' },
];

export default function User({ mid }: { mid: number }) {
  const { launch, toast } = useApp();
  const [info, setInfo] = useState<UpInfo | null>(null);
  const [relation, setRelation] = useState<{ following: number; follower: number } | null>(null);
  const [videos, setVideos] = useState<FeedVideo[]>([]);
  const [order, setOrder] = useState<VideoOrder>('newest');
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState('');
  const sentinelRef = useRef<HTMLDivElement>(null);
  const requestRef = useRef(0);
  const pageRef = useRef(1);
  const hasMoreRef = useRef(false);
  const moreBusy = useRef(false);

  const load = useCallback(async () => {
    if (!mid) return;
    const token = ++requestRef.current;
    pageRef.current = 1;
    hasMoreRef.current = false;
    moreBusy.current = false;
    setLoading(true);
    setError('');
    setVideos([]);
    setHasMore(false);
    // A fresh order starts at the profile card; otherwise the old scroll
    // position can land on the new grid's bottom and immediately auto-load
    // another page before the first row is even seen.
    const pageEl = document.querySelector('.page') as HTMLElement | null;
    if (pageEl) pageEl.scrollTop = 0;
    try {
      const data = await call<any>('user.space', { mid, order });
      if (token !== requestRef.current) return;
      setInfo(data.info);
      setRelation(data.relation);
      setVideos(data.videos?.items || []);
      hasMoreRef.current = !!data.videos?.has_more;
      setHasMore(hasMoreRef.current);
    } catch (reason: any) {
      if (token === requestRef.current) setError(reason?.message || String(reason));
    } finally {
      if (token === requestRef.current) setLoading(false);
    }
  }, [mid, order]);

  useEffect(() => { load(); }, [load]);

  useEffect(() => {
    const onRefresh = () => load();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  const more = useCallback(async () => {
    if (loading || moreBusy.current || !hasMoreRef.current) return;
    moreBusy.current = true;
    setLoadingMore(true);
    const nextPage = pageRef.current + 1;
    try {
      const next: SpaceVideoPage = await call('user.videos', {
        mid,
        page: nextPage,
        order,
      });
      setVideos((current) => {
        const seen = new Set(current.map((item) => item.bvid));
        const additions = (next.items || []).filter(
          (item) => item.bvid && !seen.has(item.bvid),
        );
        return additions.length ? [...current, ...additions] : current;
      });
      pageRef.current = nextPage;
      hasMoreRef.current = !!next.has_more;
      setHasMore(hasMoreRef.current);
    } catch (reason: any) {
      toast(reason?.message || String(reason), true);
    } finally {
      moreBusy.current = false;
      setLoadingMore(false);
    }
  }, [loading, mid, order, toast]);

  // The grid grows by pages, but the owner never sees a button: the marker
  // after the last row enters view and the next page is appended.
  useEffect(() => {
    const node = sentinelRef.current;
    if (!node || !hasMore) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void more();
      },
      { rootMargin: '600px 0px' },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [more, hasMore]);

  const follow = async () => {
    try {
      await call('user.follow', { mid, follow: true });
      toast('已关注');
    } catch (reason: any) {
      toast(reason?.message || String(reason), true);
    }
  };

  if (loading && !info) return <SkeletonGrid count={8} />;
  if (!info) return <ErrorBox error={error || '没有找到 UP 主'} onRetry={load} />;

  const items = videos.map((item) => ({
    ...item,
    owner: item.owner || info.name,
    owner_mid: mid,
  }));

  return (
    <>
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

      <div className="user-video-tabs">
        <Tabs
          tabs={ORDER_TABS.map((tab) => tab.label)}
          active={order === 'newest' ? 0 : 1}
          onChange={(index) => setOrder(ORDER_TABS[index]?.key || 'newest')}
        />
      </div>

      {error ? <div className="banner">{error}</div> : null}

      {loading && !videos.length ? (
        <SkeletonGrid count={8} />
      ) : videos.length ? (
        <>
          <VideoGrid
            items={items}
            onPlay={(item, newWindow) => launch({
              bvid: item.bvid,
              cid: item.cid || null,
              aid: item.aid,
              title: item.title,
              cover: item.pic,
              owner: item.owner,
              duration: item.duration,
            }, newWindow).catch((reason) => toast(reason.message, true))}
          />
          {hasMore ? <div ref={sentinelRef} className="user-load-sentinel" aria-hidden="true" /> : null}
          {loadingMore ? (
            <div className="feed-tail">
              <span className="spinner" aria-hidden="true" />
              <span>正在加载下一行…</span>
            </div>
          ) : null}
        </>
      ) : (
        <div className="empty">还没有投稿</div>
      )}
    </>
  );
}
