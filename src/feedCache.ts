import { call, cardCover } from './api';
import { feedBatchCount } from './feedLayout';
import type { FeedVideo } from './types';

/// The home feed is a fixed batch, not an infinite stream. Each refresh
/// consumes one pre-fetched batch and immediately refills the buffer, so two
/// more batches are always ready in the background.
const REFRESH_BUFFER = 2;

let queue: FeedVideo[][] = [];
let nextFresh = 2;
let warming: Promise<void> | null = null;
let generation = 0;

/// Warm the 480px renditions for the first seven rows of a batch so a refresh
/// swaps in complete cards rather than rows of grey boxes. Cache hits are free.
function warmCovers(batch: FeedVideo[]): void {
  for (const item of batch.slice(0, 48)) {
    const url = cardCover(item.pic);
    if (!url) continue;
    const image = new Image();
    image.referrerPolicy = 'no-referrer';
    // Release the handlers, not the URL. Assigning `src = ''` resolved to the
    // document URL, whose load failed and fired `onerror` again: every warm
    // cover became an infinite image-error loop that pinned one renderer core
    // from the moment the home feed appeared.
    const release = () => {
      image.onload = null;
      image.onerror = null;
    };
    image.onload = release;
    image.onerror = release;
    image.src = url;
  }
}

/// Consume one ready refresh batch, if the background warmer got there first.
/// A batch is only taken when it can fill the requested rows at the current
/// width; a smaller batch left over from a narrower window is skipped.
export function takeFreshFeed(minCount = 0): FeedVideo[] | null {
  const index = queue.findIndex((batch) => batch.length >= minCount);
  if (index < 0) return null;
  return queue.splice(index, 1)[0] ?? null;
}

/// Direct fetch used only when the buffer is empty (cold start / network loss).
export async function fetchFreshFeed(): Promise<FeedVideo[]> {
  const index = nextFresh;
  nextFresh += 1;
  const batch = await call<FeedVideo[]>('feed.home', {
    fresh: index,
    count: feedBatchCount(),
  });
  const items = Array.isArray(batch) ? batch : [];
  if (items.length) warmCovers(items);
  return items;
}

/// Keep `count` fresh batches queued. Safe to call repeatedly: the same
/// in-flight pass is shared, and a newer generation (logout/login) wins.
export function warmFreshFeeds(count = REFRESH_BUFFER): Promise<void> {
  if (warming) return warming;
  const gen = generation;
  const needed = feedBatchCount();
  warming = (async () => {
    try {
      const usable = () => queue.filter((batch) => batch.length >= needed).length;
      while (usable() < count && gen === generation) {
        const index = nextFresh;
        nextFresh += 1;
        const batch = await call<FeedVideo[]>('feed.home', {
          fresh: index,
          count: needed,
        });
        if (gen !== generation) return;
        if (!Array.isArray(batch) || !batch.length) break;
        queue.push(batch);
        warmCovers(batch);
      }
    } catch {
      // Leave whatever is queued; a refresh will fall back to a direct fetch.
    } finally {
      if (gen === generation) warming = null;
    }
  })();
  return warming;
}

export function resetFreshFeedCache(): void {
  generation += 1;
  queue = [];
  nextFresh = 2;
  warming = null;
}
