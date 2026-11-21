import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount, relativeTime } from '../api';
import { CoverImage } from '../components/CoverImage';
import { ImageViewer } from '../components/ImageViewer';
import { useApp } from '../context';
import type { DynamicItem, DynamicPage } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, Tabs } from '../components/Common';
import { IconFull, IconHeart } from '../components/Icons';
import { flushCardRightClick, scheduleCardRightClick } from '../rightClickGuard';

const TAB_LABELS = ['全部', '视频', '图文'];
const TAB_TYPES = ['all', 'video', 'all'];

/// The feed tile is a 168px square (2× = 360px rendition). Dynamic draw URLs
/// normally carry no @ suffix, but strip any existing one first so the tile and
/// the fullscreen view always address the same underlying file.
function splitRendition(url: string) {
  const full = cover(url);
  const [path, query] = full.split('?');
  return {
    base: path.replace(/@[^/]+$/, ''),
    query: query ? `?${query}` : '',
    isCdn: /hdslb\.com\/bfs\//i.test(full),
  };
}

function dynamicThumb(url: string) {
  const { base, query, isCdn } = splitRendition(url);
  if (!isCdn || /\.gif$/i.test(base)) return `${base}${query}`;
  return `${base}@360w_360h_1c.webp${query}`;
}

function dynamicFull(url: string) {
  const { base, query, isCdn } = splitRendition(url);
  return isCdn ? `${base}${query}` : cover(url);
}

export default function Dynamic() {
  const { navigate, launch, toast, account } = useApp();
  const [tab, setTab] = useState(0);
  const [items, setItems] = useState<DynamicItem[]>([]);
  const [offset, setOffset] = useState('');
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [viewer, setViewer] = useState<{ images: string[]; index: number } | null>(null);

  const load = useCallback(async (reset: boolean, nextOffset: string) => {
    setLoading(true);
    setError('');
    try {
      const page = await call<DynamicPage>('dynamic.list', { offset: reset ? '' : nextOffset, type: TAB_TYPES[tab] });
      let rows = page.items;
      if (tab === 2) rows = rows.filter((item) => item.images.length > 0 || (!item.archive_bvid && item.text));
      setItems((current) => reset ? rows : [...current, ...rows]);
      setOffset(page.offset);
      setHasMore(page.has_more);
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [tab]);

  useEffect(() => { load(true, ''); }, [load]);

  useEffect(() => {
    const onRefresh = () => load(true, '');
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  const like = async (item: DynamicItem) => {
    try {
      const code = await call<number>('dynamic.like', { id: item.id_str, liked: !item.liked });
      if (code !== 0) throw new Error(`点赞失败（code=${code}）`);
      setItems((current) => current.map((row) => row.id_str === item.id_str ? { ...row, liked: !row.liked, like_count: row.like_count + (row.liked ? -1 : 1) } : row));
    } catch (reason: any) { toast(reason.message, true); }
  };

  return (
    <>
      <PageHead eyebrow="社区" title="动态" sub="关注的人发布的视频与图文">
        <Tabs tabs={TAB_LABELS} active={tab} onChange={setTab} />
      </PageHead>

      {!account ? <div className="banner">登录后可以查看关注动态。</div> : null}
      {loading && !items.length ? <SkeletonGrid count={5} /> : error ? <ErrorBox error={error} onRetry={() => load(true, '')} /> : !items.length ? <Empty title="还没有动态" /> : items.map((item, index) => (
        <div className="dynamic-card" style={{ animationDelay: `${Math.min(index, 10) * 40}ms` }} key={item.id_str}>
          <div className="head">
            <img className="avatar" src={cover(item.author_face)} referrerPolicy="no-referrer" alt="" style={{ cursor: 'pointer' }} onClick={() => navigate(`user/${item.author_mid}`)} />
            <div style={{ flex: 1 }}>
              <div style={{ fontWeight: 600, cursor: 'pointer' }} onClick={() => navigate(`user/${item.author_mid}`)}>{item.author_name}</div>
              <div style={{ fontSize: 11, color: 'var(--text-faint)' }}>{item.pub_time || relativeTime(0)}</div>
            </div>
            <button className={`btn sm ghost ${item.liked ? 'active' : ''}`} onClick={() => like(item)}><IconHeart size={13} /> {formatCount(item.like_count)}</button>
          </div>
          {item.text ? <div className="text">{item.text}</div> : null}
          {item.images.length ? (
            <div className="dynamic-images">
              {item.images.slice(0, 9).map((image, imageIndex) => (
                <button
                  type="button"
                  className="dynamic-image"
                  key={`${item.id_str}-${imageIndex}`}
                  aria-label={`查看第 ${imageIndex + 1} 张图片`}
                  onClick={() => setViewer({ images: item.images.map(dynamicFull), index: imageIndex })}
                >
                  <img src={dynamicThumb(image)} alt="" loading="lazy" referrerPolicy="no-referrer" draggable={false} />
                  <span className="dynamic-image-zoom" aria-hidden="true"><IconFull size={13} /></span>
                  {imageIndex === 8 && item.images.length > 9 ? (
                    <span className="dynamic-image-more">+{item.images.length - 9}</span>
                  ) : null}
                </button>
              ))}
            </div>
          ) : null}
          {item.archive_bvid ? (
            <div
              className="dynamic-archive"
              onClick={() => launch({
                bvid: item.archive_bvid,
                title: item.archive_title,
                cover: item.archive_cover,
                owner: item.author_name,
              }).catch((reason) => toast(reason.message, true))}
              onContextMenu={(event) => {
                event.preventDefault();
                scheduleCardRightClick(
                  () => launch({
                    bvid: item.archive_bvid,
                    title: item.archive_title,
                    cover: item.archive_cover,
                    owner: item.author_name,
                  }, true).catch((reason) => toast(reason.message, true)),
                );
              }}
              onMouseUp={(event) => {
                if (event.button === 2) flushCardRightClick();
              }}
              style={{ cursor: 'pointer' }}
            >
              <div className="dynamic-archive-cover">
                <CoverImage src={item.archive_cover} alt="" />
                <span>视频</span>
              </div>
              <div className="dynamic-archive-body">
                <div className="dynamic-archive-title">{item.archive_title}</div>
                <div className="dynamic-archive-meta">
                  <span>{item.author_name}</span>
                  {item.archive_meta ? <span>{item.archive_meta}</span> : null}
                </div>
              </div>
            </div>
          ) : null}
          <div style={{ display: 'flex', gap: 16, marginTop: 10, color: 'var(--text-faint)', fontSize: 11.5 }}>
            <span>评论 {formatCount(item.comment_count)}</span>
            <span>转发 {formatCount(item.forward_count)}</span>
          </div>
        </div>
      ))}

      {hasMore ? (
        <div className="page-more">
          <button className="btn" disabled={loading} onClick={() => load(false, offset)}>{loading ? '加载中…' : '加载更多'}</button>
        </div>
      ) : null}

      {viewer ? (
        <ImageViewer
          images={viewer.images}
          initialIndex={viewer.index}
          onClose={() => setViewer(null)}
        />
      ) : null}
    </>
  );
}
