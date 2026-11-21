import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { FeedVideo } from '../types';
import { ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';
import { takePreloadedFeed } from '../preload';
import { fetchFreshFeed, takeFreshFeed, warmFreshFeeds } from '../feedCache';
import { FEED_ROWS, MAX_FEED_COLUMNS, feedBatchCount } from '../feedLayout';

/// The home feed is a fixed batch, not an infinite stream: seven rows at the
/// width measured when the batch is loaded, never appended on scroll. Refresh
/// scrolls back to the top and swaps in the next pre-fetched batch.

export default function Home() {
  const { launch, toast } = useApp();
  const [preloaded] = useState(() => takePreloadedFeed());
  const [items, setItems] = useState<FeedVideo[]>(() => preloaded ?? []);
  const [loading, setLoading] = useState(() => !preloaded?.length);
  const [error, setError] = useState('');
  const [columns, setColumns] = useState(0);
  const feedRef = useRef<HTMLDivElement>(null);

  const applyFeed = useCallback((data: FeedVideo[]) => {
    if (!data.length) return;
    if (data[0]?.pic) {
      localStorage.setItem('o11a1:vinyl-cover', data[0].pic);
      window.dispatchEvent(new CustomEvent('o11a1:vinyl-cover', { detail: data[0].pic }));
    }
    setItems(data);
    setError('');
  }, []);

  const loadInitial = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const data = await call<FeedVideo[]>('feed.home', { fresh: 0, count: feedBatchCount() });
      applyFeed(data);
      if (!data.length) setError('推荐流为空，请稍后重试');
    } catch (reason: any) {
      setError(reason?.message || String(reason));
    } finally {
      setLoading(false);
    }
  }, [applyFeed]);

  useEffect(() => {
    if (!preloaded?.length) loadInitial();
    // Two refresh batches are kept warm for the rest of the session.
    warmFreshFeeds(2).catch(() => {});
  }, [loadInitial, preloaded]);

  const refresh = useCallback(() => {
    const needed = feedBatchCount();
    const cached = takeFreshFeed(needed);
    if (cached?.length) {
      applyFeed(cached);
      warmFreshFeeds(2).catch(() => {});
      return;
    }
    setLoading(true);
    fetchFreshFeed()
      .then((data) => {
        if (data.length) applyFeed(data);
      })
      .catch((reason) => setError(reason?.message || String(reason)))
      .finally(() => {
        setLoading(false);
        warmFreshFeeds(2).catch(() => {});
      });
  }, [applyFeed]);

  useEffect(() => {
    const onRefresh = () => refresh();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [refresh]);

  // The grid uses auto-fill columns, so "seven rows" depends on the current
  // width. Measure the first row after layout and render exactly that many.
  useLayoutEffect(() => {
    const grid = feedRef.current?.querySelector<HTMLElement>('.video-grid');
    if (!grid) return;
    const measure = () => {
      const first = grid.firstElementChild as HTMLElement | null;
      if (!first) return;
      const top = first.offsetTop;
      let count = 0;
      for (const child of Array.from(grid.children) as HTMLElement[]) {
        if (child.offsetTop !== top) break;
        count += 1;
      }
      if (count && count !== columns) setColumns(count);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(grid);
    return () => observer.disconnect();
  }, [items, columns]);

  const shown = items.slice(0, Math.max(1, columns || MAX_FEED_COLUMNS) * FEED_ROWS);

  const play = (item: FeedVideo, newWindow = false) => launch({
    bvid: item.bvid,
    cid: item.cid,
    aid: item.aid,
    title: item.title,
    cover: item.pic,
    owner: item.owner,
    duration: item.duration,
  }, newWindow).catch((error) => toast(error.message, true));

  return (
    <>
      {error && !items.length ? <ErrorBox error={error} onRetry={loadInitial} /> : null}
      {loading && !items.length ? <SkeletonGrid count={FEED_ROWS * 5} /> : null}
      {items.length ? (
        <div ref={feedRef} className="home-feed">
          <VideoGrid items={shown} onPlay={play} />
          <div className="feed-end">我是有底线的</div>
        </div>
      ) : null}
    </>
  );
}
