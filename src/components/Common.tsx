import React from 'react';
import { cover, formatCount, formatDuration } from '../api';
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
      <strong>{title}</strong>
      {hint || null}
    </div>
  );
}

export function ErrorBox({ error, onRetry }: { error: string; onRetry?: () => void }) {
  return (
    <div className="error">
      <strong>加载失败</strong>
      <div style={{ maxWidth: 620, margin: '0 auto 14px', wordBreak: 'break-word' }}>{error}</div>
      {onRetry ? <button className="btn primary" onClick={onRetry}>重试</button> : null}
    </div>
  );
}

export function PageHead({ children }: { title?: string; sub?: string; children?: React.ReactNode }) {
  // The old native shell identifies the page by the active bottom tab only:
  // no per-page title strip. Keep the functional row (tabs / chips / actions)
  // when a page actually needs one.
  if (!children) return null;
  return <div className="page-tools">{children}</div>;
}

export function SkeletonGrid({ count = 8 }: { count?: number }) {
  return (
    <div className="video-grid">
      {Array.from({ length: count }).map((_, index) => (
        <div className="skeleton-card" key={index}>
          <div className="skeleton thumb" />
          <div className="skeleton line" style={{ width: '88%' }} />
          <div className="skeleton line" style={{ width: '55%' }} />
        </div>
      ))}
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

/// Play-count rarity dot: gray ≤9.9k, green 10k, blue 100k, purple 500k,
/// orange ≥1M. The number itself stays hidden, as on the old home cards.
export function rarityColor(view: number | undefined | null): string {
  const value = Number(view || 0);
  if (value >= 1_000_000) return 'rgba(235, 122, 77, 0.88)';
  if (value >= 500_000) return 'rgba(148, 107, 225, 0.82)';
  if (value >= 100_000) return 'rgba(71, 140, 235, 0.82)';
  if (value >= 10_000) return 'rgba(51, 168, 115, 0.78)';
  return 'rgba(140, 158, 184, 0.52)';
}

function durationSeconds(value: number | string | undefined | null): number {
  if (typeof value === 'number') return Math.max(0, value);
  const text = String(value || '').trim();
  if (!text) return 0;
  if (!text.includes(':')) return Math.max(0, Number(text) || 0);
  return text.split(':').reduce((total, part) => total * 60 + (Number(part) || 0), 0);
}

export function VideoGrid({ items, onPlay, getProgress, renderAction }: {
  items: Array<FeedVideo | VideoCard | Record<string, any>>;
  onPlay: (item: any) => void;
  getProgress?: (item: any) => number | undefined;
  renderAction?: (item: any) => React.ReactNode;
}) {
  if (!items.length) return <Empty title="这里还没有内容" hint="试试刷新或换个关键词" />;
  return (
    <div className="video-grid">
      {items.map((item: any, index: number) => {
        const author = item.owner || item.author || item.uname || '';
        const fresh = item.fresh || freshness(item.pub_ts);
        const trailing = item.trailing || fresh;
        const view = Number(item.view ?? item.play ?? 0);
        const meta = item.meta || (view > 0 ? `${formatCount(view)} 播放` : '');
        const seconds = durationSeconds(item.duration);
        const badge = item.badge || (seconds > 0 ? formatDuration(seconds) : '');
        const progress = getProgress ? Number(getProgress(item) || 0) : 0;
        return (
          <div
            className="video-card"
            key={`${item.bvid || item.title}-${item.cid || ''}-${index}`}
            role="button"
            tabIndex={0}
            title={item.title}
            onClick={() => onPlay(item)}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) return;
              if (event.key === 'Enter' || event.key === ' ') {
                event.preventDefault();
                onPlay(item);
              }
            }}
          >
            <div className="cover">
              <img src={cover(item.pic || item.cover)} alt="" loading="lazy" referrerPolicy="no-referrer" />
              {badge ? <div className="cover-badge">{badge}</div> : null}
              {progress > 0 ? <div className="video-progress"><span style={{ width: `${Math.min(100, progress * 100)}%` }} /></div> : null}
              {renderAction ? <div className="card-action" onClick={(event) => event.stopPropagation()}>{renderAction(item)}</div> : null}
            </div>
            <div className="body">
              <div className="video-title">{item.title}</div>
              <div className="video-meta">
                <span className="video-author">{author || meta}</span>
                {author && trailing ? (
                  <span className="video-trail">
                    {!item.trailing && fresh ? <i style={{ background: rarityColor(view) }} /> : null}
                    {trailing}
                  </span>
                ) : null}
              </div>
            </div>
          </div>
        );
      })}
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

export function Tabs({ tabs, active, onChange }: { tabs: string[]; active: number; onChange: (index: number) => void }) {
  return (
    <div className="tabs">
      {tabs.map((label, index) => (
        <button key={label} className={`tab ${active === index ? 'active' : ''}`} onClick={() => onChange(index)}>{label}</button>
      ))}
    </div>
  );
}

export function Pagination({ page, hasMore, loading, onMore }: { page: number; hasMore: boolean; loading: boolean; onMore: () => void }) {
  return (
    <div style={{ display: 'flex', justifyContent: 'center', padding: '22px 0' }}>
      {hasMore ? (
        <button className="btn" disabled={loading} onClick={onMore}>{loading ? '加载中…' : `加载更多（第 ${page + 1} 页）`}</button>
      ) : (
        <span style={{ color: 'var(--text-faint)', fontSize: 12 }}>已经到底了</span>
      )}
    </div>
  );
}
