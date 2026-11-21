import { call, callRaw, cardCover } from './api';
import { clearPageCache, putPageCache } from './pageCache';
import { resetFreshFeedCache, warmFreshFeeds } from './feedCache';
import { feedBatchCount } from './feedLayout';
import type { Account, FeedVideo, Settings } from './types';

export interface BootstrapData {
  version?: string;
  settings: Settings;
  account: Account | null;
  sessionReady?: boolean;
  sessionError?: string;
}

export interface PreloadData {
  bootstrap: BootstrapData;
  feed: FeedVideo[];
  hot: Array<{ keyword: string; show_name: string }>;
  history: string[];
  feedError: string;
}

export interface PreloadProgress {
  value: number;
  step: string;
}

let cachedFeed: FeedVideo[] | null = null;
let cachedSearch: {
  hot: Array<{ keyword: string; show_name: string }>;
  history: string[];
} | null = null;

/// The home page consumes the gate's feed exactly once, so entering the shell
/// never shows a skeleton or a second network round-trip. The cache is kept for
/// the lifetime of the window so StrictMode's double mount (dev) is harmless.
export function takePreloadedFeed(): FeedVideo[] | null {
  return cachedFeed;
}

export function takePreloadedSearch() {
  return cachedSearch;
}

/// Drop every preloaded snapshot when the account changes. Page data is
/// one-shot and memory-only, but the shell-level feed/search snapshots can also
/// outlive a logout, so clear those too.
export function clearPreloadedData(): void {
  cachedFeed = null;
  cachedSearch = null;
  resetFreshFeedCache();
  clearPageCache();
}

/// After a QR login the gate reloads the recommendation once with the new
/// session, so the first screen is already the signed-in feed rather than the
/// anonymous one that was fetched while waiting for the scan.
export async function refreshFeedAfterLogin(account?: Account | null): Promise<FeedVideo[]> {
  // Always fetch a fresh signed-in feed; a page-cache entry from the anonymous
  // preload must not be served here.
  resetFreshFeedCache();
  const feed = await callRaw<FeedVideo[]>('feed.home', { fresh: 0, count: feedBatchCount() });
  if (feed[0]?.pic) {
    localStorage.setItem('o11a1:vinyl-cover', feed[0].pic);
  }
  if (feed.length) cachedFeed = feed;
  const covers = feed.slice(0, 12).map((item) => cardCover(item.pic)).filter(Boolean);
  await Promise.all(covers.map(waitImage));
  if (feed.length) warmFreshFeeds(2).catch(() => {});
  // Account-only pages need this second pass; run it in the background so the
  // entering state is not held up by data the user has not opened yet.
  preloadPages(account ?? null).catch(() => {});
  return feed;
}

function waitImage(url: string): Promise<void> {
  if (!url) return Promise.resolve();
  return new Promise((resolve) => {
    const image = new Image();
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      image.onload = null;
      image.onerror = null;
      resolve();
    };
    image.onload = finish;
    image.onerror = finish;
    image.referrerPolicy = 'no-referrer';
    image.src = url;
    setTimeout(finish, 1600);
  });
}

const COVER_KEYS = ['pic', 'cover', 'face', 'avatar', 'images', 'image', 'keyframe'] as const;

/// Pull a small number of image URLs out of a page response. Only known image
/// fields are followed, so titles, descriptions and video URLs are never
/// mistaken for covers.
function collectCovers(value: any, out: string[], cap: number): void {
  const seen = new Set<any>();
  const walk = (node: any, depth: number) => {
    if (!node || out.length >= cap || depth > 5) return;
    if (typeof node === 'string') {
      if (/^https?:\/\//i.test(node)) out.push(node);
      return;
    }
    if (Array.isArray(node)) {
      for (const row of node.slice(0, 6)) {
        walk(row, depth + 1);
        if (out.length >= cap) return;
      }
      return;
    }
    if (typeof node !== 'object' || seen.has(node)) return;
    seen.add(node);
    for (const key of COVER_KEYS) {
      if (key in node) walk(node[key], depth + 1);
      if (out.length >= cap) return;
    }
  };
  walk(value, 0);
}

interface PreloadJob {
  method: string;
  params: Record<string, any>;
  timeout?: number;
}

async function prefetchPage(job: PreloadJob): Promise<any | null> {
  try {
    const data = await withTimeout<any>(
      callRaw<any>(job.method, job.params),
      job.timeout ?? 5000,
      null,
    );
    if (data === null || data === undefined) return null;
    putPageCache(job.method, job.params, data);
    return data;
  } catch {
    return null;
  }
}

/// Warm every page's default first view while the login / boot panel is still
/// on screen. Data is put into the one-shot page cache; images are warmed for
/// the first cards so the first paint is complete instead of filling in.
export async function preloadPages(
  account: Account | null,
  onProgress?: (done: number, total: number) => void,
): Promise<void> {
  const jobs: PreloadJob[] = [];
  if (account) {
    jobs.push(
      { method: 'favorites.folders.all', params: {} },
      { method: 'history.list', params: { max: 100, viewAt: 0 } },
      { method: 'downloads.list', params: {} },
      { method: 'dynamic.list', params: { offset: '', type: 'all' } },
      { method: 'messages.unread', params: {} },
      { method: 'messages.list', params: { kind: 'reply' } },
    );
  } else {
    jobs.push({ method: 'downloads.list', params: {} });
  }
  jobs.push(
    { method: 'regions.latest', params: { rid: 1, page: 1, ps: 24 } },
    { method: 'pgc.timeline', params: {} },
    { method: 'live.index', params: {} },
    { method: 'fish.state', params: {} },
  );
  if (account) {
    jobs.push(
      { method: 'messages.dm.sessions', params: {} },
      { method: 'account.nav', params: {} },
      { method: 'member.vip', params: {}, timeout: 6000 },
    );
  }
  let done = 0;
  const warmed: any[] = [];
  await Promise.all(jobs.map(async (job) => {
    const data = await prefetchPage(job);
    if (data !== null) warmed.push(data);
    done += 1;
    onProgress?.(done, jobs.length);
  }));

  const urls: string[] = [];
  for (const data of warmed) collectCovers(data, urls, 6);
  const covers = [...new Set(urls)].slice(0, 64).map((url) => cardCover(url)).filter(Boolean);
  await Promise.all(covers.map(waitImage));
}

function withTimeout<T>(promise: Promise<T>, ms: number, fallback: T): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((resolve) => setTimeout(() => resolve(fallback), ms)),
  ]);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function bootstrapWithRetry(report: (progress: PreloadProgress) => void): Promise<BootstrapData> {
  let lastError: any = null;
  for (let attempt = 0; attempt < 40; attempt += 1) {
    try {
      return await call<BootstrapData>('app.bootstrap');
    } catch (error: any) {
      lastError = error;
      report({
        value: Math.min(0.18, 0.04 + attempt * 0.004),
        step: '正在启动视频引擎…',
      });
      await delay(500);
    }
  }
  throw lastError || new Error('视频引擎启动超时');
}

/// Load everything the home page needs before the login card lets the user in.
/// The greeting/progress is real: every await is one of the actual startup
/// requests, not a decorative timer.
export async function preloadAll(
  report: (progress: PreloadProgress) => void,
  onBootstrap?: (bootstrap: BootstrapData) => void,
): Promise<PreloadData> {
  report({ value: 0.04, step: '正在连接视频引擎…' });
  const bootstrap = await bootstrapWithRetry(report);
  onBootstrap?.(bootstrap);
  report({
    value: 0.24,
    step: bootstrap.account ? `正在恢复 ${bootstrap.account.uname} 的账号…` : '正在准备登录…',
  });

  let feed: FeedVideo[] = [];
  let feedError = '';
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      feed = await callRaw<FeedVideo[]>('feed.home', { fresh: 0, count: feedBatchCount() });
      if (feed.length) break;
    } catch (error: any) {
      feedError = error?.message || String(error);
    }
  }
  report({ value: 0.52, step: '正在加载首页推荐…' });

  const [hot, history] = await Promise.all([
    withTimeout(call<Array<{ keyword: string; show_name: string }>>('search.hot').catch(() => []), 4000, []),
    withTimeout(call<string[]>('search.history.get').catch(() => []), 4000, []),
  ]);
  putPageCache('search.hot', {}, hot);
  putPageCache('search.history.get', {}, history);
  report({ value: 0.72, step: '正在加载搜索与播放记录…' });

  // Warm the first screen's covers so the grid paints complete cards, not a
  // row of grey boxes that fill in one by one.
  const urls = [
    ...feed.slice(0, 24).map((item) => cardCover(item.pic)),
    cardCover(bootstrap.account?.face),
  ].filter(Boolean);
  report({ value: 0.78, step: '正在预载其他页面…' });
  let warming = true;
  const pageWarm = preloadPages(bootstrap.account, (done, total) => {
    if (!warming) return;
    report({
      value: 0.78 + 0.18 * (done / Math.max(1, total)),
      step: '正在预载其他页面…',
    });
  }).catch(() => {});
  // Give the background pass a short head start, then enter. Any slow page
  // keeps warming and lands in the one-shot cache for its first visit; boot
  // must not sit on the spinner waiting for the slowest endpoint.
  await Promise.race([
    Promise.all([Promise.all(urls.map(waitImage)), pageWarm]),
    delay(2500),
  ]);
  warming = false;
  report({ value: 0.97, step: '正在进入主页…' });
  if (feed[0]?.pic) {
    localStorage.setItem('o11a1:vinyl-cover', feed[0].pic);
  }

  cachedFeed = feed;
  cachedSearch = { hot, history };
  // Two refresh batches are fetched in the background; the home page never
  // waits for a network round-trip when the user hits refresh.
  if (feed.length) warmFreshFeeds(2).catch(() => {});
  report({
    value: 1,
    step: feed.length
      ? '首页已就绪'
      : feedError
        ? '首页加载失败，可重试或以游客进入'
        : '首页暂时没有内容',
  });
  return { bootstrap, feed, hot, history, feedError };
}
