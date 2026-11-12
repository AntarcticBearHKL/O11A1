import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount, relativeTime } from '../api';
import { useApp } from '../context';
import type { DynamicItem, DynamicPage } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, Tabs } from '../components/Common';
import { IconHeart } from '../components/Icons';

const TAB_LABELS = ['全部', '视频', '图文'];
const TAB_TYPES = ['all', 'video', 'all'];

export default function Dynamic() {
  const { navigate, launch, toast, account } = useApp();
  const [tab, setTab] = useState(0);
  const [items, setItems] = useState<DynamicItem[]>([]);
  const [offset, setOffset] = useState('');
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

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
      <PageHead title="动态" sub="关注的人发布的视频与图文">
        <Tabs tabs={TAB_LABELS} active={tab} onChange={setTab} />
      </PageHead>

      {!account ? <div className="banner">登录后可以查看关注动态。</div> : null}
      {loading && !items.length ? <SkeletonGrid count={5} /> : error ? <ErrorBox error={error} onRetry={() => load(true, '')} /> : !items.length ? <Empty title="还没有动态" /> : items.map((item) => (
        <div className="card dynamic-card" key={item.id_str}>
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
            <div className="dynamic-images" style={{ gridTemplateColumns: item.images.length === 1 ? '1fr' : 'repeat(3, 1fr)' }}>
              {item.images.slice(0, 6).map((image, index) => <img key={index} src={cover(image)} referrerPolicy="no-referrer" loading="lazy" alt="" />)}
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
              style={{ cursor: 'pointer' }}
            >
              <div className="dynamic-archive-cover">
                <img src={cover(item.archive_cover)} referrerPolicy="no-referrer" alt="" />
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
        <div style={{ display: 'flex', justifyContent: 'center', padding: 22 }}>
          <button className="btn" disabled={loading} onClick={() => load(false, offset)}>{loading ? '加载中…' : '加载更多'}</button>
        </div>
      ) : null}
    </>
  );
}
