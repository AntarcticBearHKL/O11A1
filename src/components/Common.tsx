import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { call, formatCount, formatDuration } from '../api';
import { CoverImage } from './CoverImage';
import { useApp } from '../context';
import { scheduleCardRightClick } from '../rightClickGuard';
import type { FeedVideo, VideoCard } from '../types';

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="empty" style={{ padding: 30 }}>
      <span className="spinner" />
      {label ? <div style={{ marginTop: 10 }}>{label}</div> : null}
    </div>
  );
}

export function Empty({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="empty">
      <span className="empty-mark" aria-hidden="true" />
      <strong>{title}</strong>
      {hint ? <span className="empty-hint">{hint}</span> : null}
    </div>
  );
}

export function ErrorBox({ error, onRetry }: { error: string; onRetry?: () => void }) {
  return (
    <div className="error">
      <span className="empty-mark" aria-hidden="true" />
      <strong>加载失败</strong>
      <div style={{ maxWidth: 620, margin: '0 auto 14px', wordBreak: 'break-word' }}>{error}</div>
      {onRetry ? <button className="btn primary" onClick={onRetry}>重试</button> : null}
    </div>
  );
}

/**
 * Top strip for page-local controls only. Editorial titles, eyebrows and
 * subtitles are deliberately not rendered: content leads, tabs/filters stay.
 * Callers may still pass title/sub for context, but they are ignored.
 */
export function PageHead({ children, className }: {
  title?: string;
  sub?: string;
  eyebrow?: string;
  children?: React.ReactNode;
  className?: string;
}) {
  if (!children) return null;
  return (
    <header className={`page-head${className ? ` ${className}` : ''}`}>
      <div className="page-actions">{children}</div>
    </header>
  );
}

export function SkeletonGrid({ count = 8 }: { count?: number }) {
  return (
    <div className="video-grid">
      {Array.from({ length: count }).map((_, index) => (
        <div className="skeleton-card" key={index} style={{ animationDelay: `${Math.min(index, 10) * 45}ms` }}>
          <div className="skeleton thumb" />
          <div className="skeleton line" style={{ width: '88%' }} />
          <div className="skeleton line" style={{ width: '55%' }} />
        </div>
      ))}
    </div>
  );
}

/// Text-only loading state for lists that are not video grids (messages,
/// replies and private conversations).
export function LoadingText({ label = '正在加载' }: { label?: string }) {
  return (
    <div className="loading-text" role="status" aria-live="polite">
      <span>{label}</span>
      <span className="loading-dots" aria-hidden="true"><i /><i /><i /></span>
    </div>
  );
}

/// Old card freshness labels: 刚刚 / N小时前 / N天前 / N周前 / N个月前 /
/// N季度前 / N年前. The 90-day quarter and 365-day year match the native UI.
export function freshness(ts: number | undefined | null): string {
  if (!ts || Number(ts) <= 0) return '';
  const age = Math.max(0, Date.now() / 1000 - Number(ts));
  const hour = 3600;
  const day = 86400;
  if (age < hour) return '刚刚';
  if (age < day) return `${Math.floor(age / hour)}小时前`;
  if (age < day * 7) return `${Math.floor(age / day)}天前`;
  if (age < day * 30) return `${Math.floor(age / (7 * day))}周前`;
  if (age < day * 90) return `${Math.floor(age / (30 * day))}个月前`;
  if (age < day * 365) return `${Math.floor(age / (90 * day))}季度前`;
  return `${Math.floor(age / (365 * day))}年前`;
}

function durationSeconds(value: number | string | undefined | null): number {
  if (typeof value === 'number') return Math.max(0, value);
  const text = String(value || '').trim();
  if (!text) return 0;
  if (!text.includes(':')) return Math.max(0, Number(text) || 0);
  return text.split(':').reduce((total, part) => total * 60 + (Number(part) || 0), 0);
}

/// Freshness tints for the publish-time chip. The scale runs warm → cool as the
/// upload ages, so the hue underlines the text instead of fighting it.
/// ≤1d coral, ≤7d amber, ≤30d teal, ≤90d blue, ≤1y violet, older slate.
export function freshnessColor(ts: number | undefined | null): string {
  if (!ts || Number(ts) <= 0) return '#8a909b';
  const age = Math.max(0, Date.now() / 1000 - Number(ts));
  const day = 86400;
  if (age < day) return '#d97862';
  if (age < day * 7) return '#c9984f';
  if (age < day * 30) return '#4f9a83';
  if (age < day * 90) return '#5c84c6';
  if (age < day * 365) return '#8a78b8';
  return '#8a909b';
}

/// Exact publish time for the publish-time tooltip: local date plus a 24h
/// clock, so the card's relative freshness chip can stay short.
function exactPublishTime(ts: number | undefined | null): string {
  if (!ts || Number(ts) <= 0) return '发布时间未知';
  const date = new Date(Number(ts) * 1000);
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${date.getFullYear()}年${date.getMonth() + 1}月${date.getDate()}日 ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

type CardTipKind = 'title' | 'trail';

interface CardTipState {
  kind: CardTipKind;
  anchor: DOMRect;
}

/**
 * Flat warm-paper hover tip for a video card. It is portal-rendered so the
 * grid's cover clipping and the title's two-line clamp cannot cut it off, and
 * it is clamped to the viewport with a flip below the anchor when the card sits
 * near the top edge. No native `title` tooltip is used for these targets.
 */
function CardTooltip({ tip, item, onClose }: {
  tip: CardTipState;
  item: any;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [box, setBox] = useState({ left: 0, top: 0, ready: false });
  const view = Number(item.view ?? item.play ?? 0);
  const fresh = item.trailing || freshness(item.pub_ts);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const width = el.offsetWidth;
    const height = el.offsetHeight;
    const margin = 12;
    const { anchor } = tip;
    const left = Math.max(
      margin,
      Math.min(anchor.left + anchor.width / 2 - width / 2, window.innerWidth - margin - width),
    );
    let top = anchor.top - height - 10;
    if (top < margin) top = Math.min(anchor.bottom + 10, window.innerHeight - margin - height);
    setBox({ left, top, ready: true });
  }, [tip]);

  useEffect(() => {
    const close = () => onClose();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('scroll', close, true);
    window.addEventListener('resize', close);
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('scroll', close, true);
      window.removeEventListener('resize', close);
      window.removeEventListener('keydown', onKey);
    };
  }, [onClose]);

  return createPortal(
    <div
      ref={ref}
      className={`card-tip card-tip-${tip.kind}`}
      role="tooltip"
      style={{
        left: box.left,
        top: box.top,
        visibility: box.ready ? 'visible' : 'hidden',
      }}
    >
      {tip.kind === 'title' ? (
        <div className="card-tip-text">{item.title}</div>
      ) : (
        <>
          <div className="card-tip-time">{exactPublishTime(item.pub_ts)}</div>
          <div className="card-tip-foot">
            <span>{view > 0 ? `${formatCount(view)} 播放` : '暂无播放数据'}</span>
            {fresh ? <span className="card-tip-dot">·</span> : null}
            {fresh ? <span>{fresh}</span> : null}
          </div>
        </>
      )}
    </div>,
    document.body,
  );
}

/// Signed preview URLs live for minutes; keep a small process-wide cache so
/// moving the pointer across a row does not request the same video twice.
const previewCache = new Map<string, { url: string; at: number }>();
const PREVIEW_TTL_MS = 5 * 60_000;
const PREVIEW_CACHE_MAX = 48;

function cachedPreview(key: string): string {
  const hit = previewCache.get(key);
  if (!hit) return '';
  if (Date.now() - hit.at > PREVIEW_TTL_MS) {
    previewCache.delete(key);
    return '';
  }
  return hit.url;
}

function rememberPreview(key: string, url: string): void {
  if (previewCache.size >= PREVIEW_CACHE_MAX) {
    const oldest = previewCache.keys().next().value;
    if (oldest) previewCache.delete(oldest);
  }
  previewCache.set(key, { url, at: Date.now() });
}

const previewWarming = new Set<string>();

/// Resolve a few preview URLs for the block that was just appended below the
/// viewport. The mobile feed does the same: by the time the next rows become
/// visible, hovering a card starts its video immediately instead of waiting
/// for a `video.preview` round-trip.
export function prewarmFeedPreviews(
  items: Array<{ bvid?: string; cid?: number }>,
  limit = 4,
): void {
  let started = 0;
  for (const item of items) {
    if (started >= limit) break;
    const bvid = String(item?.bvid || '').trim();
    const cid = Number(item?.cid || 0);
    if (!bvid || !cid) continue;
    const key = `${bvid}:${cid}`;
    if (cachedPreview(key) || previewWarming.has(key)) continue;
    previewWarming.add(key);
    started += 1;
    call<{ url: string }>('video.preview', { bvid, cid })
      .then((data) => {
        if (data?.url) rememberPreview(key, data.url);
      })
      .catch(() => {})
      .finally(() => previewWarming.delete(key));
  }
}

function VideoCardItem({ item, index, onPlay, getProgress, renderAction, navigate }: {
  item: any;
  index: number;
  onPlay: (item: any, newWindow?: boolean) => void;
  getProgress?: (item: any) => number | undefined;
  renderAction?: (item: any) => React.ReactNode;
  navigate: (to: string) => void;
}) {
  const [tip, setTip] = useState<CardTipState | null>(null);
  const closeTip = useCallback(() => setTip(null), []);
  const showTip = (kind: CardTipKind) => (event: React.MouseEvent<HTMLElement>) => {
    setTip({ kind, anchor: event.currentTarget.getBoundingClientRect() });
  };
  const hideTip = () => setTip(null);

  // Hover preview: a muted, low-quality MP4 plays over the cover after a
  // short delay. It is only a look-ahead, never history or local progress.
  const [preview, setPreview] = useState<{ url: string; key: string } | null>(null);
  const [previewOn, setPreviewOn] = useState(false);
  const [pressed, setPressed] = useState(false);
  const previewTimer = useRef<any>(null);
  const previewToken = useRef(0);
  const previewStart = useRef(0);
  const previewVideo = useRef<HTMLVideoElement | null>(null);

  const stopPreview = useCallback(() => {
    previewToken.current += 1;
    clearTimeout(previewTimer.current);
    previewTimer.current = null;
    setPreview(null);
    setPreviewOn(false);
    const video = previewVideo.current;
    if (video) {
      video.pause();
      video.removeAttribute('src');
      video.load();
    }
  }, []);

  const beginPreview = useCallback(() => {
    const bvid = String(item?.bvid || '').trim();
    const cid = Number(item?.cid || 0);
    if (!bvid || !cid) return;
    const key = `${bvid}:${cid}`;
    const cached = cachedPreview(key);
    if (cached) {
      setPreview({ url: cached, key });
      return;
    }
    const token = ++previewToken.current;
    clearTimeout(previewTimer.current);
    previewTimer.current = setTimeout(async () => {
      try {
        const data = await call<{ url: string }>('video.preview', { bvid, cid });
        if (token !== previewToken.current || !data?.url) return;
        rememberPreview(key, data.url);
        setPreview({ url: data.url, key });
      } catch {
        /* The cover remains; a missing preview is not an error. */
      }
    }, 280);
  }, [item?.bvid, item?.cid]);

  useEffect(() => () => stopPreview(), [stopPreview]);

  useEffect(() => {
    if (!preview) return;
    const onScroll = () => stopPreview();
    window.addEventListener('scroll', onScroll, true);
    return () => window.removeEventListener('scroll', onScroll, true);
  }, [preview, stopPreview]);

  const onPreviewMeta = (event: React.SyntheticEvent<HTMLVideoElement>) => {
    const video = event.currentTarget;
    const duration = Number.isFinite(video.duration) ? video.duration : 0;
    const start = duration > 20 ? Math.min(duration * 0.18, duration - 10, 45) : 1;
    previewStart.current = Math.max(0, start);
    try { video.currentTime = previewStart.current; } catch { /* metadata race */ }
    video.play().catch(() => {});
  };

  const onPreviewTime = (event: React.SyntheticEvent<HTMLVideoElement>) => {
    const video = event.currentTarget;
    if (previewStart.current > 0 && video.currentTime > previewStart.current + 8) {
      video.currentTime = previewStart.current;
    }
  };

  const author = item.owner || item.author || item.uname || '';
  const authorMid = Number(item.owner_mid || item.author_mid || item.mid || item.up_mid || 0) || 0;
  const fresh = item.fresh || freshness(item.pub_ts);
  const trailing = item.trailing || fresh;
  const view = Number(item.view ?? item.play ?? 0);
  const meta = item.meta || (view > 0 ? `${formatCount(view)} 播放` : '');
  const seconds = durationSeconds(item.duration);
  const hideTime = item.hideTime === true;
  const badge = hideTime ? '' : (item.badge || (seconds > 0 ? formatDuration(seconds) : ''));
  const progress = getProgress ? Number(getProgress(item) || 0) : 0;
  const watch = typeof item.watchProgress === 'number' && Number.isFinite(item.watchProgress)
    ? Math.max(0, Math.min(1, item.watchProgress))
    : null;
  const watchPercent = watch === null ? 0 : Math.round(watch * 100);

  return (
    <div
      className={`video-card${pressed ? ' pressed' : ''}`}
      data-feed-key={item.bvid ? `${item.bvid}:${Number(item.cid || 0)}` : undefined}
      style={{ animationDelay: `${Math.min(index, 14) * 32}ms` }}
      role="button"
      tabIndex={0}
      onMouseEnter={beginPreview}
      onMouseLeave={() => { setPressed(false); stopPreview(); }}
      onPointerDown={(event) => {
        if (event.button === 0) setPressed(true);
        stopPreview();
      }}
      onPointerUp={() => setPressed(false)}
      onPointerCancel={() => setPressed(false)}
      onClick={() => {
        setPressed(false);
        setTip(null);
        stopPreview();
        onPlay(item);
      }}
      onContextMenu={(event) => {
        // A short right click always opens a new player window. If the shell's
        // refresh ring owns this press, the pending open is cancelled when the
        // ring fires; a held menu delivered before mouseup waits for release.
        event.preventDefault();
        setTip(null);
        stopPreview();
        scheduleCardRightClick(() => onPlay(item, true));
      }}
      onKeyDown={(event) => {
        if (event.target !== event.currentTarget) return;
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          onPlay(item);
        }
      }}
    >
      <div className="cover">
        <CoverImage src={item.pic || item.cover} alt="" eager={index < 24} />
        {preview ? (
          <video
            ref={previewVideo}
            className={`card-preview ${previewOn ? 'on' : ''}`}
            src={preview.url}
            muted
            playsInline
            preload="auto"
            onLoadedMetadata={onPreviewMeta}
            onPlaying={() => setPreviewOn(true)}
            onTimeUpdate={onPreviewTime}
            onError={() => {
              previewCache.delete(preview.key);
              setPreview(null);
              setPreviewOn(false);
            }}
          />
        ) : null}
        {badge ? <div className="cover-badge">{badge}</div> : null}
        {progress > 0 ? <div className="video-progress"><span style={{ width: `${Math.min(100, progress * 100)}%` }} /></div> : null}
        {renderAction ? <div className="card-action" onClick={(event) => event.stopPropagation()}>{renderAction(item)}</div> : null}
      </div>
      <div className="body">
        <div
          className="video-title"
          onMouseEnter={showTip('title')}
          onMouseLeave={hideTip}
        >{item.title}</div>
        <div className="video-meta">
          {author ? (
            authorMid ? (
              <span
                className="video-author link"
                role="link"
                tabIndex={0}
                onClick={(event) => {
                  event.stopPropagation();
                  setTip(null);
                  navigate(`user/${authorMid}`);
                }}
                onKeyDown={(event) => {
                  if (event.key !== 'Enter' && event.key !== ' ') return;
                  event.preventDefault();
                  event.stopPropagation();
                  navigate(`user/${authorMid}`);
                }}
              >{author}</span>
            ) : (
              <span className="video-author">{author}</span>
            )
          ) : (
            <span className="video-author">{meta}</span>
          )}
          {watch !== null ? (
            <span className="video-watch" title={`已看 ${watchPercent}%`}>
              <span className="video-watch-track">
                <span className="video-watch-fill" style={{ width: `${watchPercent}%` }} />
              </span>
              <span className="video-watch-text">{watchPercent}%</span>
            </span>
          ) : author && trailing ? (
            <span
              className="video-trail"
              style={{ ['--trail' as any]: item.pub_ts ? freshnessColor(item.pub_ts) : '#8a909b' }}
              onMouseEnter={showTip('trail')}
              onMouseLeave={hideTip}
            >
              {!item.trailing && fresh ? <i /> : null}
              {trailing}
            </span>
          ) : null}
        </div>
      </div>
      {tip ? <CardTooltip tip={tip} item={item} onClose={closeTip} /> : null}
    </div>
  );
}

export function VideoGrid({ items, onPlay, getProgress, renderAction }: {
  items: Array<FeedVideo | VideoCard | Record<string, any>>;
  onPlay: (item: any, newWindow?: boolean) => void;
  getProgress?: (item: any) => number | undefined;
  renderAction?: (item: any) => React.ReactNode;
}) {
  const { navigate } = useApp();
  if (!items.length) return <Empty title="这里还没有内容" hint="试试刷新或换个关键词" />;
  return (
    <div className="video-grid">
      {items.map((item: any, index: number) => (
        <VideoCardItem
          key={`${item.bvid || item.title}-${item.cid || ''}-${index}`}
          item={item}
          index={index}
          onPlay={onPlay}
          getProgress={getProgress}
          renderAction={renderAction}
          navigate={navigate}
        />
      ))}
    </div>
  );
}

export function Toggle({ on, onChange }: { on: boolean; onChange: (on: boolean) => void }) {
  return <button className={`switch ${on ? 'on' : ''}`} aria-pressed={on} onClick={() => onChange(!on)} />;
}

export function SettingsRow({ name, desc, children }: { name: string; desc?: string; children: React.ReactNode }) {
  return (
    <div className="settings-row">
      <div className="label">
        <div className="name">{name}</div>
        {desc ? <div className="desc">{desc}</div> : null}
      </div>
      <div className="control">{children}</div>
    </div>
  );
}

/**
 * Page tabs. Every tab is its own standalone pill; there is no shared
 * segmented track, so each label reads as an individual button.
 */
export function Tabs({ tabs, active, onChange }: { tabs: string[]; active: number; onChange: (index: number) => void }) {
  return (
    <div className="tabs">
      {tabs.map((label, index) => (
        <button
          key={label}
          className={`tab ${active === index ? 'active' : ''}`}
          onClick={() => onChange(index)}
        >{label}</button>
      ))}
    </div>
  );
}

export function Pagination({ page, hasMore, loading, onMore }: { page: number; hasMore: boolean; loading: boolean; onMore: () => void }) {
  return (
    <div className="page-more">
      {hasMore ? (
        <button className="btn" disabled={loading} onClick={onMore}>{loading ? '加载中…' : `加载更多（第 ${page + 1} 页）`}</button>
      ) : (
        <span>已经到底了</span>
      )}
    </div>
  );
}
