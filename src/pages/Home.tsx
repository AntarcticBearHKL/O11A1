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
/// Four grid rows per mounted block. Block sizes are rounded to whole columns,
/// so an infinite feed never leaves a one-card orphan row at the bottom.
const ROWS_PER_BLOCK = 4;
/// Keep the current block, the one below it, and one block of scrollback.
const WINDOW_BLOCKS = 3;
const PREFETCH_PX = 900;
const SCROLL_IDLE_MS = 140;

/// `agent.html`-style remounts of Home should open a fresh stream. The first
/// entry in a session still consumes the fast startup batch.
let homeVisited = false;

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

/// Split loaded cards into blocks of complete grid rows. The leftover shorter
/// than one row stays in `pending` until another page can fill it, so a block
/// boundary can never paint an incomplete row.
function splitFullRows(
  items: FeedVideo[],
  columns: number,
  firstId: number,
): { blocks: FeedBlock[]; pending: FeedVideo[]; nextId: number } {
  const width = Math.max(1, columns);
  const blocks: FeedBlock[] = [];
  let id = firstId;
  let index = 0;
  const maxItems = width * ROWS_PER_BLOCK;
  while (items.length - index >= width) {
    const remaining = items.length - index;
    const take = Math.min(maxItems, Math.floor(remaining / width) * width);
    const rows = Math.floor(take / width);
    if (rows < 1) break;
    blocks.push({ id, items: items.slice(index, index + rows * width) });
    id += 1;
    index += rows * width;
  }
  return { blocks, pending: items.slice(index), nextId: id };
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
  const firstVisitRef = useRef<boolean | null>(null);
  if (firstVisitRef.current === null) {
    firstVisitRef.current = !homeVisited;
    homeVisited = true;
  }
  const firstVisit = firstVisitRef.current;
  const [initial] = useState(() => {
    const seen = new Set<string>();
    const clean = dedupe(firstVisit ? takePreloadedFeed() ?? [] : [], seen);
    return {
      clean,
      cursor: clean.length ? Math.ceil(clean.length / PAGE_SIZE) + 1 : 1,
      seen,
    };
  });
  const [blocks, setBlocks] = useState<FeedBlock[]>(() => (
    initial.clean.length ? [{ id: 0, items: initial.clean }] : []
  ));
  const [loading, setLoading] = useState(() => initial.clean.length === 0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const [error, setError] = useState('');
  const [moreError, setMoreError] = useState('');
  const feedRef = useRef<HTMLDivElement>(null);

  const blocksRef = useRef<FeedBlock[]>(blocks);
  const pendingRef = useRef<FeedVideo[]>([]);
  const columnsRef = useRef(0);
  const seen = useRef(initial.seen);
  const cursor = useRef(initial.cursor);
  const nextBlockId = useRef(blocks.length ? 1 : 0);
  const hasMoreRef = useRef(true);
  const loadingRef = useRef(false);
  const generation = useRef(0);
  const loadingToken = useRef(0);
  const anchorRef = useRef<{ key: string; top: number } | null>(null);
  const idleTimer = useRef(0);
  const prefetchAt = useRef(0);
  const warmedScroll = useRef(false);

  const applyCover = useCallback((items: FeedVideo[]) => {
    if (items[0]?.pic) rememberCover(items[0].pic);
  }, []);

  /// Move complete rows out of the pending queue and into mounted blocks.
  /// A remainder shorter than one column stays pending; it is either filled
  /// by the next page or dropped at the true end of the stream.
  const fillBlocks = useCallback(() => {
    const columns = Math.max(1, columnsRef.current || 4);
    let created = 0;
    while (pendingRef.current.length >= columns) {
      const rows = Math.min(ROWS_PER_BLOCK, Math.floor(pendingRef.current.length / columns));
      if (rows < 1) break;
      const items = pendingRef.current.splice(0, rows * columns);
      setBlocks((previous) => [...previous, { id: nextBlockId.current++, items }]);
      prewarmFeedPreviews(items);
      created += 1;
      if (created >= 3) break;
    }
    return created;
  }, []);

  /// Re-split mounted cards after a column change so every block still ends
  /// on a complete grid row.
  const reflowBlocks = useCallback((columns: number) => {
    // Keep the initial block id across the first column measurement. The
    // preloaded feed starts as one block and is re-split into full rows here;
    // if the id changed, `feed-block-initial` would be lost and the entrance
    // animation would either vanish or move to whichever block becomes first.
    const hasInitial = blocksRef.current[0]?.id === 0;
    const all = [...blocksRef.current.flatMap((block) => block.items), ...pendingRef.current];
    const split = splitFullRows(all, columns, hasInitial ? 0 : nextBlockId.current);
    nextBlockId.current = split.nextId;
    pendingRef.current = split.pending;
    blocksRef.current = split.blocks;
    setBlocks(split.blocks);
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
          pendingRef.current.push(...clean);
          if (first) applyCover(clean);
          if (fillBlocks() > 0) {
            appended = true;
            break;
          }
        }
        if (page?.has_more === false) {
          // Keep any complete final rows; a shorter-than-one-row remainder is
          // intentionally hidden so the feed never ends on a ragged row.
          if (fillBlocks() > 0) appended = true;
          break;
        }
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
    const split = splitFullRows(clean, Math.max(1, columnsRef.current || 4), 0);
    seen.current = nextSeen;
    cursor.current = clean.length ? Math.ceil(clean.length / PAGE_SIZE) + 1 : 1;
    nextBlockId.current = split.nextId;
    pendingRef.current = split.pending;
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

  const startedRef = useRef(false);
  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    if (blocksRef.current.length) return;
    if (firstVisit) void loadPage(true);
    else void refresh();
  }, [firstVisit, loadPage, refresh]);

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
    const feed = feedRef.current;
    const grid = feed?.querySelector<HTMLElement>('.video-grid');
    if (!grid) return;
    blocksRef.current = blocks;
    const measure = () => {
      const tracks = getComputedStyle(grid).gridTemplateColumns;
      const count = tracks && tracks !== 'none'
        ? tracks.split(/\s+/).filter(Boolean).length
        : 0;
      if (!count || count === columnsRef.current) return;
      columnsRef.current = count;
      reflowBlocks(count);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(grid);
    return () => observer.disconnect();
  }, [blocks.length, reflowBlocks]);

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

  // The first scroll of a freshly mounted feed can pay a one-frame
  // layerisation/backdrop-filter cost. Set a small offset and reset it in the
  // same task: no frame is painted between the two writes, but the compositor
  // builds the scroll/backdrop surfaces before the user touches the wheel.
  useEffect(() => {
    if (warmedScroll.current || !blocks.length) return;
    warmedScroll.current = true;
    const warm = (offset: number) => {
      const scroll = feedRef.current?.closest('.page') as HTMLElement | null;
      if (!scroll || scroll.scrollTop !== 0) return;
      scroll.scrollTop = offset;
      scroll.scrollTop = 0;
    };
    const first = window.setTimeout(() => warm(60), 450);
    // A second pass after covers have decoded warms the next row's backdrop
    // surface without painting an intermediate frame.
    const second = window.setTimeout(() => warm(180), 1200);
    return () => {
      window.clearTimeout(first);
      window.clearTimeout(second);
    };
  }, [blocks.length]);

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
            <div
              className={`feed-block${block.id === 0 ? ' feed-block-initial' : ''}`}
              key={block.id}
            >
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
