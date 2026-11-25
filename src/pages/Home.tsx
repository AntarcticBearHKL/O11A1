import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { FeedVideo } from '../types';
import { ErrorBox, SkeletonGrid, VideoGrid, prewarmFeedPreviews } from '../components/Common';
import { takePreloadedFeed } from '../preload';
import { fetchFreshFeed, takeFreshFeed, warmFreshFeeds } from '../feedCache';
import { FEED_ROWS, feedBatchCount } from '../feedLayout';

/// The desktop home feed is now a sliding window, ported from the mobile
/// feed: one recommendation page is one block, the next block is appended
/// before the viewport reaches it, and the oldest block is released only after
/// the first visible card has moved into a newer block. Releasing a block while
/// it still contains the viewport anchor is what used to make an infinite feed
/// jump to a different place.

const PAGE_SIZE = 30;
/// Keep the current block, the one below it, and one block of scrollback.
const WINDOW_BLOCKS = 3;
const PREFETCH_PX = 900;
const SCROLL_IDLE_MS = 140;

type FeedBlock = { id: number; items: FeedVideo[] };
type FeedPage = { items: FeedVideo[]; next: number; has_more: boolean };

function feedKey(item: FeedVideo): string {
  return `${item.bvid}:${Number(item.cid || 0)}`;
}

function dedupe(items: FeedVideo[], seen: Set<string>): FeedVideo[] {
  const out: FeedVideo[] = [];
  for (const item of items) {
    if (!item?.bvid) continue;
    const key = feedKey(item);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(item);
  }
  return out;
}

function splitIntoBlocks(items: FeedVideo[], firstId: number): { blocks: FeedBlock[]; nextId: number } {
  const blocks: FeedBlock[] = [];
  let id = firstId;
  for (let index = 0; index < items.length; index += PAGE_SIZE) {
    const slice = items.slice(index, index + PAGE_SIZE);
    if (!slice.length) continue;
    blocks.push({ id, items: slice });
    id += 1;
  }
  return { blocks, nextId: id };
}

function rememberCover(pic: string): void {
  if (!pic) return;
  localStorage.setItem('o11a1:vinyl-cover', pic);
  window.dispatchEvent(new CustomEvent('o11a1:vinyl-cover', { detail: pic }));
}

function cardsIn(feed: HTMLElement, scroll: HTMLElement): HTMLElement[] {
  const bounds = scroll.getBoundingClientRect();
  const cards: HTMLElement[] = [];
  for (const card of Array.from(feed.querySelectorAll<HTMLElement>('.video-card'))) {
    const rect = card.getBoundingClientRect();
    if (rect.bottom > bounds.top + 2 && rect.top < bounds.bottom - 2) {
      cards.push(card);
    }
  }
  return cards;
}

function topVisibleCard(feed: HTMLElement, scroll: HTMLElement): HTMLElement | null {
  return cardsIn(feed, scroll)[0] ?? null;
}

function focusCard(feed: HTMLElement, scroll: HTMLElement): HTMLElement | null {
  const cards = cardsIn(feed, scroll);
  if (!cards.length) return null;
  const bounds = scroll.getBoundingClientRect();
  const middle = (bounds.top + bounds.bottom) * 0.5;
  let best: HTMLElement | null = null;
  let bestDistance = Infinity;
  for (const card of cards) {
    const rect = card.getBoundingClientRect();
    const distance = Math.abs((rect.top + rect.bottom) * 0.5 - middle);
    if (distance < bestDistance) {
      bestDistance = distance;
      best = card;
    }
  }
  return best;
}

export default function Home() {
  const { launch, toast } = useApp();
  const [preloaded] = useState(() => takePreloadedFeed());
  const [initial] = useState(() => {
    const seen = new Set<string>();
    const clean = dedupe(preloaded ?? [], seen);
    const split = splitIntoBlocks(clean, 0);
    return {
      blocks: split.blocks,
      nextId: split.nextId,
      cursor: clean.length ? Math.ceil(clean.length / PAGE_SIZE) + 1 : 1,
      seen,
    };
  });
  const [blocks, setBlocks] = useState<FeedBlock[]>(initial.blocks);
  const [loading, setLoading] = useState(() => initial.blocks.length === 0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const [error, setError] = useState('');
  const [moreError, setMoreError] = useState('');
  const feedRef = useRef<HTMLDivElement>(null);

  const blocksRef = useRef<FeedBlock[]>(initial.blocks);
  const seen = useRef(initial.seen);
  const cursor = useRef(initial.cursor);
  const nextBlockId = useRef(initial.nextId);
  const hasMoreRef = useRef(true);
  const loadingRef = useRef(false);
  const generation = useRef(0);
  const loadingToken = useRef(0);
  const anchorRef = useRef<{ key: string; top: number } | null>(null);
  const idleTimer = useRef(0);
  const prefetchAt = useRef(0);

  const applyCover = useCallback((items: FeedVideo[]) => {
    if (items[0]?.pic) rememberCover(items[0].pic);
  }, []);

  const loadPage = useCallback(async (first = false) => {
    if (loadingRef.current || !hasMoreRef.current) return;
    const stream = generation.current;
    const token = ++loadingToken.current;
    loadingRef.current = true;
    if (first) {
      setLoading(true);
      setError('');
    } else {
      setLoadingMore(true);
    }
    setMoreError('');
    try {
      let appended = false;
      // A recommendation page can repeat a video the previous page already
      // showed. Skip those and try the next page instead of showing a short
      // block or ending the stream early.
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const page = await call<FeedPage>('feed.page', { fresh: cursor.current });
        if (stream !== generation.current) return;
        const next = Number(page?.next || 0);
        cursor.current = next > cursor.current ? next : cursor.current + 1;
        const clean = dedupe(Array.isArray(page?.items) ? page.items : [], seen.current);
        if (page?.has_more === false) {
          hasMoreRef.current = false;
          setHasMore(false);
        }
        if (clean.length) {
          const block = { id: nextBlockId.current, items: clean };
          nextBlockId.current += 1;
          setBlocks((previous) => [...previous, block]);
          if (first) applyCover(clean);
          prewarmFeedPreviews(clean);
          appended = true;
          break;
        }
        if (page?.has_more === false) break;
      }
      if (!appended && first) setError('推荐流为空，请稍后重试');
      if (!appended && !first) setMoreError('暂时没有更多新内容');
    } catch (reason: any) {
      if (stream === generation.current) {
        const message = reason?.message || String(reason);
        if (first || !blocksRef.current.length) setError(message);
        else setMoreError(message);
      }
    } finally {
      if (stream === generation.current && token === loadingToken.current) {
        loadingRef.current = false;
        setLoading(false);
        setLoadingMore(false);
      }
    }
  }, [applyCover]);

  const trimFront = useCallback(() => {
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (!feed || !scroll) return;
    if (blocksRef.current.length <= WINDOW_BLOCKS) return;
    const blockEls = Array.from(feed.children).filter(
      (node): node is HTMLElement => node instanceof HTMLElement && node.classList.contains('feed-block'),
    );
    if (blockEls.length <= WINDOW_BLOCKS) return;
    const top = topVisibleCard(feed, scroll);
    const focus = focusCard(feed, scroll);
    if (!top || !focus) return;
    // Never release a block that still contains the first visible card or the
    // card nearest the viewport centre. This is the guard that keeps an older
    // block from being unloaded while the user is still looking at it.
    let remove = 0;
    const limit = blockEls.length - WINDOW_BLOCKS;
    while (remove < limit) {
      const candidate = blockEls[remove];
      if (!candidate) break;
      if (candidate.contains(top) || candidate.contains(focus)) break;
      // A hover preview owns a live <video>; unmounting it would flash the
      // cover and interrupt the preview the user is watching.
      if (candidate.querySelector('.video-card:hover, video.card-preview')) break;
      remove += 1;
    }
    if (!remove) return;
    const key = top.dataset.feedKey;
    if (!key) return;
    anchorRef.current = { key, top: top.getBoundingClientRect().top };
    setBlocks((previous) => (previous.length > WINDOW_BLOCKS ? previous.slice(remove) : previous));
  }, []);

  const onScroll = useCallback(() => {
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (!feed || !scroll) return;
    const now = Date.now();
    const nearBottom =
      scroll.scrollTop + scroll.clientHeight >= scroll.scrollHeight - PREFETCH_PX;
    if (nearBottom && now - prefetchAt.current > 250 && hasMoreRef.current && !loadingRef.current) {
      prefetchAt.current = now;
      void loadPage(false);
    }
    window.clearTimeout(idleTimer.current);
    idleTimer.current = window.setTimeout(() => {
      idleTimer.current = 0;
      trimFront();
    }, SCROLL_IDLE_MS);
  }, [loadPage, trimFront]);

  const resetStream = useCallback((batch: FeedVideo[]) => {
    generation.current += 1;
    loadingToken.current += 1;
    loadingRef.current = false;
    const nextSeen = new Set<string>();
    const clean = dedupe(batch, nextSeen);
    const split = splitIntoBlocks(clean, 0);
    seen.current = nextSeen;
    cursor.current = clean.length ? Math.ceil(clean.length / PAGE_SIZE) + 1 : 1;
    nextBlockId.current = split.nextId;
    hasMoreRef.current = true;
    setHasMore(true);
    setBlocks(split.blocks);
    setLoading(false);
    setLoadingMore(false);
    setError('');
    setMoreError('');
    if (clean.length) applyCover(clean);
    prewarmFeedPreviews(clean);
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (scroll) scroll.scrollTop = 0;
  }, [applyCover]);

  const refresh = useCallback(() => {
    const needed = feedBatchCount();
    const cached = takeFreshFeed(needed);
    if (cached?.length) {
      resetStream(cached);
      warmFreshFeeds(2).catch(() => {});
      return;
    }
    setLoading(true);
    setError('');
    fetchFreshFeed()
      .then((data) => {
        if (data.length) resetStream(data);
        else setError('推荐流为空，请稍后重试');
      })
      .catch((reason) => setError(reason?.message || String(reason)))
      .finally(() => {
        setLoading(false);
        warmFreshFeeds(2).catch(() => {});
      });
  }, [resetStream]);

  useEffect(() => {
    if (!blocks.length && !loadingRef.current) void loadPage(true);
  }, [blocks.length, loadPage]);

  // Keep enough blocks mounted to cover the viewport and one screen below it.
  useEffect(() => {
    if (!blocks.length) return;
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (!scroll) return;
    const short = scroll.scrollHeight <= scroll.clientHeight + 240;
    const nearBottom =
      scroll.scrollTop + scroll.clientHeight >= scroll.scrollHeight - PREFETCH_PX;
    if ((blocks.length < 2 || short || nearBottom) && hasMoreRef.current && !loadingRef.current) {
      void loadPage(false);
    }
  }, [blocks, loadPage]);

  useLayoutEffect(() => {
    blocksRef.current = blocks;
    const anchor = anchorRef.current;
    if (!anchor) return;
    anchorRef.current = null;
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (!feed || !scroll) return;
    const target = Array.from(feed.querySelectorAll<HTMLElement>('[data-feed-key]')).find(
      (node) => node.dataset.feedKey === anchor.key,
    );
    if (!target) return;
    // The layout effect runs before paint. Re-align the same card to the
    // viewport position it had before the first block was unloaded, so the
    // feed never visibly jumps to a different place.
    const delta = target.getBoundingClientRect().top - anchor.top;
    if (Math.abs(delta) > 0.5) scroll.scrollTop += delta;
  }, [blocks]);

  useEffect(() => {
    const feed = feedRef.current;
    const scroll = feed?.closest('.page') as HTMLElement | null;
    if (!scroll) return;
    scroll.addEventListener('scroll', onScroll, { passive: true });
    return () => {
      scroll.removeEventListener('scroll', onScroll);
      window.clearTimeout(idleTimer.current);
      idleTimer.current = 0;
    };
  }, [onScroll, blocks.length > 0]);

  useEffect(() => {
    const onRefresh = () => refresh();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [refresh]);

  useEffect(() => {
    // Preload the first few hover previews of the block below the viewport.
    const last = blocks[blocks.length - 1];
    if (last) prewarmFeedPreviews(last.items);
    applyCover(blocks[0]?.items ?? []);
    // Only for the initial preloaded batch; later blocks are warmed when they
    // are appended by `loadPage`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const retryInitial = useCallback(() => {
    setError('');
    void loadPage(true);
  }, [loadPage]);

  const retryMore = useCallback(() => {
    setMoreError('');
    void loadPage(false);
  }, [loadPage]);

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
      {error && !blocks.length ? <ErrorBox error={error} onRetry={retryInitial} /> : null}
      {loading && !blocks.length ? <SkeletonGrid count={FEED_ROWS * 5} /> : null}
      {blocks.length ? (
        <div ref={feedRef} className="home-feed">
          {blocks.map((block) => (
            <div className="feed-block" key={block.id}>
              <VideoGrid items={block.items} onPlay={play} />
            </div>
          ))}
          {loadingMore ? (
            <div className="feed-tail">
              <span className="spinner" aria-hidden="true" />
              <span>正在加载下一段…</span>
            </div>
          ) : null}
          {moreError && !loadingMore ? (
            <div className="feed-tail error">
              <span>{moreError}</span>
              <button className="btn sm" onClick={retryMore}>重试</button>
            </div>
          ) : null}
          {!hasMore && !loadingMore ? <div className="feed-end">我是有底线的</div> : null}
        </div>
      ) : null}
    </>
  );
}
