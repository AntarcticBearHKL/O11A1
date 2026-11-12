import { call, cover } from './api';
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

/// After a QR login the gate reloads the recommendation once with the new
/// session, so the first screen is already the signed-in feed rather than the
/// anonymous one that was fetched while waiting for the scan.
export async function refreshFeedAfterLogin(): Promise<FeedVideo[]> {
  const feed = await call<FeedVideo[]>('feed.home', { fresh: 0 });
  if (feed[0]?.pic) {
    localStorage.setItem('o11a1:vinyl-cover', feed[0].pic);
  }
  if (feed.length) cachedFeed = feed;
  const covers = feed.slice(0, 12).map((item) => cover(item.pic)).filter(Boolean);
  await Promise.all(covers.map(waitImage));
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
      resolve();
    };
    image.onload = finish;
    image.onerror = finish;
    image.referrerPolicy = 'no-referrer';
    image.src = url;
    setTimeout(finish, 1600);
  });
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
      feed = await call<FeedVideo[]>('feed.home', { fresh: 0 });
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
  report({ value: 0.72, step: '正在加载搜索与播放记录…' });

  // Warm the first screen's covers so the grid paints complete cards, not a
  // row of grey boxes that fill in one by one.
  const urls = [
    ...feed.slice(0, 24).map((item) => cover(item.pic)),
    cover(bootstrap.account?.face),
  ].filter(Boolean);
  await Promise.all(urls.map(waitImage));
  if (feed[0]?.pic) {
    localStorage.setItem('o11a1:vinyl-cover', feed[0].pic);
  }

  cachedFeed = feed;
  cachedSearch = { hot, history };
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
