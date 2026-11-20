import { call } from './api';
import type { FavFolder, FeedVideo } from './types';

/// 收藏夹推荐: sample videos from a favourite folder, then sample their related
/// videos, and mix both into one flat feed.
///
/// The engine already caches folder pages (`favorites.items`) and related lists
/// (`video.related`) for five minutes, so repeated generations do not hammer
/// Bilibili. This module adds the one cache the engine cannot provide: a small
/// localStorage map of how often each bvid was already recommended, so a
/// refresh keeps showing *new* videos instead of the same first row.

const SEEN_KEY = 'o11a1:fav-seen:v1';
const PAGE_SIZE = 20;
/// A bvid shown twice within this window is skipped while the feed is filled.
const SEEN_TTL_MS = 30 * 60 * 1000;
const SEEN_LIMIT = 2;
/// Keep the map bounded; older rows are pruned on every save.
const SEEN_MAX = 600;
const SEEN_KEEP_MS = 2 * SEEN_TTL_MS;

type SeenRow = { n: number; at: number };
type SeenMap = Record<string, SeenRow>;

function loadSeen(): SeenMap {
  try {
    const raw = JSON.parse(localStorage.getItem(SEEN_KEY) || '{}');
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
    const clean: SeenMap = {};
    for (const [bvid, row] of Object.entries(raw as Record<string, any>)) {
      if (!bvid || !row || typeof row.n !== 'number' || typeof row.at !== 'number') continue;
      clean[bvid] = { n: Math.max(0, row.n), at: row.at };
    }
    return clean;
  } catch {
    return {};
  }
}

function saveSeen(seen: SeenMap): void {
  try {
    const now = Date.now();
    const rows = Object.entries(seen)
      .filter(([, row]) => row && now - row.at < SEEN_KEEP_MS)
      .sort((a, b) => b[1].at - a[1].at)
      .slice(0, SEEN_MAX);
    localStorage.setItem(SEEN_KEY, JSON.stringify(Object.fromEntries(rows)));
  } catch {
    /* localStorage can be full or disabled; recommendations still work. */
  }
}

function blocked(seen: SeenMap, bvid: string, limit: number): boolean {
  const row = seen[bvid];
  if (!row) return false;
  return Date.now() - row.at < SEEN_TTL_MS && row.n >= limit;
}

function markSeen(items: FeedVideo[]): void {
  if (!items.length) return;
  const seen = loadSeen();
  const now = Date.now();
  for (const item of items) {
    if (!item?.bvid) continue;
    const row = seen[item.bvid];
    seen[item.bvid] = { n: (row?.n || 0) + 1, at: now };
  }
  saveSeen(seen);
}

function shuffle<T>(items: T[]): T[] {
  const copy = items.slice();
  for (let index = copy.length - 1; index > 0; index -= 1) {
    const pick = Math.floor(Math.random() * (index + 1));
    [copy[index], copy[pick]] = [copy[pick], copy[index]];
  }
  return copy;
}

function dedupe(items: FeedVideo[]): FeedVideo[] {
  const seen = new Set<string>();
  return items.filter((item) => {
    if (!item?.bvid || seen.has(item.bvid)) return false;
    // Bilibili keeps deleted favourites in the list as a placeholder row; it
    // has a bvid but can never play, so it must not enter the feed.
    if ((item.title || '').trim() === '已失效视频') return false;
    seen.add(item.bvid);
    return true;
  });
}

/// Build one recommendation batch for `folder`.
///
/// `want` is normally seven rows at the current width (`feedBatchCount()`).
/// The related calls are bounded to six source videos; each source usually
/// returns tens of related entries, so the mix has plenty to choose from.
export async function recommendFromFolder(folder: FavFolder, want: number): Promise<FeedVideo[]> {
  if (!folder || !folder.id) return [];
  const target = Math.max(1, Math.round(want));
  const folderPages = Math.max(1, Math.ceil(Math.max(0, folder.count) / PAGE_SIZE));
  const pageCount = Math.min(3, folderPages);
  const picks = new Set<number>();
  while (picks.size < pageCount) {
    picks.add(1 + Math.floor(Math.random() * folderPages));
  }
  const pageResults = await Promise.all(
    [...picks].map((page) =>
      call<{ items: FeedVideo[] }>('favorites.items', { mediaId: folder.id, page })
        .catch(() => ({ items: [] as FeedVideo[] })),
    ),
  );
  let pool = dedupe(pageResults.flatMap((page) => page.items || []));
  if (!pool.length) {
    // The folder count can be a little stale; one unconditional first page is
    // a cheap fallback and is cached by the engine too.
    const first = await call<{ items: FeedVideo[] }>('favorites.items', { mediaId: folder.id, page: 1 })
      .catch(() => ({ items: [] as FeedVideo[] }));
    pool = dedupe(first.items || []);
  }
  if (!pool.length) return [];

  const sources = shuffle(pool).slice(0, Math.min(6, pool.length));
  const relatedLists = await Promise.all(
    sources.map((video) =>
      call<FeedVideo[]>('video.related', { bvid: video.bvid }).catch(() => [] as FeedVideo[]),
    ),
  );
  const related = shuffle(dedupe(relatedLists.flat()));

  const seen = loadSeen();
  const used = new Set<string>();
  const chosen: FeedVideo[] = [];
  const take = (video: FeedVideo | undefined, limit: number) => {
    if (!video || chosen.length >= target) return;
    if (!video.bvid || used.has(video.bvid)) return;
    if (blocked(seen, video.bvid, limit)) return;
    used.add(video.bvid);
    chosen.push(video);
  };

  const sourceQueue = shuffle(sources);
  const relatedQueue = related.slice();
  // Roughly two folder samples for every three related videos; source and
  // related are both recommendations, the ratio just keeps the feed varied.
  for (let index = 0; (sourceQueue.length || relatedQueue.length) && chosen.length < target; index += 1) {
    const useSource = index % 5 < 2;
    const primary = useSource ? sourceQueue : relatedQueue;
    const secondary = useSource ? relatedQueue : sourceQueue;
    take(primary.shift() || secondary.shift(), SEEN_LIMIT);
  }
  // If the seen cache blocked too much, relax it before showing a short feed.
  for (const limit of [SEEN_LIMIT + 1, Number.MAX_SAFE_INTEGER]) {
    if (chosen.length >= target) break;
    for (const video of shuffle([...sourceQueue, ...relatedQueue, ...pool])) {
      if (chosen.length >= target) break;
      take(video, limit);
    }
  }
  markSeen(chosen);
  return chosen;
}
