import React, { useCallback, useEffect, useState } from 'react';
import { call, formatDuration } from '../api';
import { useApp } from '../context';
import type { HistoryItem, HistoryPage } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, VideoGrid } from '../components/Common';
import { IconTrash } from '../components/Icons';

export default function History({ embedded = false }: { embedded?: boolean }) {
  const { launch, toast } = useApp();
  const [max, setMax] = useState(100);
  const [items, setItems] = useState<HistoryItem[]>([]);
  const [cursor, setCursor] = useState<{ id: number; time: number } | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const load = useCallback(async (nextMax: number, viewAt: number, append: boolean) => {
    setLoading(true);
    setError('');
    try {
      const page = await call<HistoryPage>('history.list', { max: nextMax, viewAt });
      setItems((current) => append ? [...current, ...page.items] : page.items);
      setCursor(page.cursor);
      setHasMore(page.has_more);
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, []);

  useEffect(() => { load(max, 0, false); }, [max, load]);

  useEffect(() => {
    const onRefresh = () => load(max, 0, false);
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load, max]);

  const more = () => {
    if (cursor) load(max, cursor.time, true);
  };

  const remove = async (item: HistoryItem) => {
    try {
      await call('history.delete', { kid: item.kid });
      setItems((current) => current.filter((row) => row.kid !== item.kid));
      toast('已删除');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const clear = async () => {
    if (!window.confirm('清空全部观看历史？此操作不可撤销。')) return;
    if (!window.confirm('再确认一次：真的要清空全部历史吗？')) return;
    try {
      await call('history.clear');
      setItems([]);
      setHasMore(false);
      toast('历史已清空');
    } catch (reason: any) { toast(reason.message, true); }
  };

  return (
    <>
      {embedded ? (
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 12 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>观看历史</div>
          <div style={{ flex: 1 }} />
          <select className="select" value={max} onChange={(event) => setMax(Number(event.target.value))}>
            <option value={50}>50 条</option>
            <option value={100}>100 条</option>
            <option value={200}>200 条</option>
            <option value={0}>全部</option>
          </select>
          <button className="btn sm danger" onClick={clear}>清空历史</button>
        </div>
      ) : (
        <PageHead title="历史" sub="继续观看与最近 50 / 100 / 200 / 全部记录">
          <select className="select" value={max} onChange={(event) => setMax(Number(event.target.value))}>
            <option value={50}>50 条</option>
            <option value={100}>100 条</option>
            <option value={200}>200 条</option>
            <option value={0}>全部</option>
          </select>
          <button className="btn sm danger" onClick={clear}>清空历史</button>
        </PageHead>
      )}

      {loading ? <SkeletonGrid count={6} /> : error ? <ErrorBox error={error} onRetry={() => load(max, 0, false)} /> : !items.length ? <Empty title="还没有观看历史" /> : (
        <VideoGrid
          items={items.map((item) => {
            const percent = item.duration > 0 ? Math.min(100, Math.round((item.progress / item.duration) * 100)) : 0;
            return {
              ...item,
              pic: item.cover,
              owner: item.author,
              trailing: `看到 ${formatDuration(item.progress)} · ${percent}%`,
            };
          })}
          onPlay={(item) => launch({
            bvid: item.bvid,
            title: item.title,
            cover: item.cover,
            owner: item.author,
            duration: item.duration,
            startMs: Math.max(0, Number(item.progress) || 0) * 1000,
          }).catch((reason) => toast(reason.message, true))}
          getProgress={(item) => (item.duration > 0 ? item.progress / item.duration : 0)}
          renderAction={(item) => <button className="card-remove icon" title="删除这条历史" onClick={() => remove(item)}><IconTrash size={12} /></button>}
        />
      )}

      {hasMore ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: 22 }}>
          <button className="btn" disabled={loading} onClick={more}>加载更多</button>
        </div>
      ) : null}
    </>
  );
}
