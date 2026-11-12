import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { FeedVideo } from '../types';
import { ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';
import { takePreloadedFeed } from '../preload';

export default function Home() {
  const { launch, toast } = useApp();
  const [preloaded] = useState(() => takePreloadedFeed());
  const [items, setItems] = useState<FeedVideo[]>(() => preloaded ?? []);
  const [loading, setLoading] = useState(() => !preloaded?.length);
  const [error, setError] = useState('');
  const [fresh, setFresh] = useState(() => (preloaded?.length ? 1 : 0));
  const sentinel = useRef<HTMLDivElement>(null);
  const autoLoads = useRef(0);

  const load = useCallback(async (nextFresh: number, reset: boolean) => {
    setLoading(true);
    setError('');
    try {
      const data = await call<FeedVideo[]>('feed.home', { fresh: nextFresh });
      if (data[0]?.pic) {
        localStorage.setItem('o11a1:vinyl-cover', data[0].pic);
        window.dispatchEvent(new CustomEvent('o11a1:vinyl-cover', { detail: data[0].pic }));
      }
      setItems((current) => {
        const base = reset ? [] : current;
        const seen = new Set(base.map((item) => item.bvid));
        return [...base, ...data.filter((item) => !seen.has(item.bvid))];
      });
      setFresh(nextFresh + 1);
      if (!data.length && reset) setError('推荐流为空，请稍后重试');
    } catch (reason: any) {
      setError(reason?.message || String(reason));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!preloaded?.length) load(0, true);
  }, [load, preloaded]);

  useEffect(() => {
    const onRefresh = () => load(fresh, true);
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [fresh, load]);

  useEffect(() => {
    const node = sentinel.current;
    if (!node) return;
    const observer = new IntersectionObserver((entries) => {
      if (entries[0]?.isIntersecting && !loading && items.length > 0 && autoLoads.current < 4) {
        autoLoads.current += 1;
        load(fresh, false);
      }
    }, { rootMargin: '300px' });
    observer.observe(node);
    return () => observer.disconnect();
  }, [fresh, items.length, loading, load]);

  const play = (item: FeedVideo) => launch({
    bvid: item.bvid,
    cid: item.cid,
    aid: item.aid,
    title: item.title,
    cover: item.pic,
    owner: item.owner,
    duration: item.duration,
  }).catch((error) => toast(error.message, true));

  return (
    <>
      {error && !items.length ? <ErrorBox error={error} onRetry={() => load(fresh, true)} /> : null}
      {loading && !items.length ? <SkeletonGrid count={8} /> : null}
      {items.length ? <VideoGrid items={items} onPlay={play} /> : null}
      {items.length ? (
        <div ref={sentinel} style={{ display: 'flex', justifyContent: 'center', padding: '24px 0' }}>
          <button className="btn" disabled={loading} onClick={() => { autoLoads.current = 4; load(fresh, false); }}>{loading ? '加载中…' : '加载更多推荐'}</button>
        </div>
      ) : null}
    </>
  );
}
