import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { HistoryItem, HistoryPage } from '../types';
import { Empty, ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';
import { IconTrash } from '../components/Icons';

/// The Bilibili history cursor returns at most 30 entries per request. A UI
/// page of 50/100/200 is filled by walking the cursor until enough entries are
/// buffered, then slicing that buffer: no page size can skip a video, and
/// "上一页" can be served from memory without another request.
const PAGE_SIZES = [50, 100, 200];

export default function History() {
  const { launch, toast } = useApp();
  const [max, setMax] = useState(100);
  const [page, setPage] = useState(0);
  const [allItems, setAllItems] = useState<HistoryItem[]>([]);
  const [apiMore, setApiMore] = useState(true);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const allRef = useRef<HistoryItem[]>([]);
  const cursorRef = useRef(0);
  const moreRef = useRef(true);
  const requestRef = useRef(0);

  const ensure = useCallback(async (pageIndex: number, size: number, reset: boolean) => {
    const token = ++requestRef.current;
    if (reset) {
      allRef.current = [];
      cursorRef.current = 0;
      moreRef.current = true;
    }
    let items = allRef.current;
    let cursor = cursorRef.current;
    let more = moreRef.current;
    const needed = (pageIndex + 1) * size;
    setLoading(true);
    setError('');
    try {
      while (items.length < needed && more) {
        const data = await call<HistoryPage>('history.list', { max: size, viewAt: cursor });
        if (token !== requestRef.current) return false;
        const seen = new Set(items.map((row) => row.kid));
        const fresh = (data.items || []).filter((row) => row.kid && !seen.has(row.kid));
        items = items.concat(fresh);
        const previousCursor = cursor;
        const nextCursor = data.cursor?.view_at ?? 0;
        cursor = nextCursor;
        // A cursor that did not advance would refetch the same 30 entries
        // forever (the old page did exactly that); stop instead.
        more = Boolean(data.has_more && data.cursor && nextCursor !== previousCursor);
        if (!(data.items || []).length) break;
      }
      allRef.current = items;
      cursorRef.current = cursor;
      moreRef.current = more;
      setAllItems(items);
      setApiMore(more);
      return true;
    } catch (reason: any) {
      if (token !== requestRef.current) return false;
      setError(reason?.message || String(reason));
      return false;
    } finally {
      if (token === requestRef.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    ensure(0, 100, true);
  }, [ensure]);

  useEffect(() => {
    const onRefresh = () => {
      setPage(0);
      setAllItems([]);
      setApiMore(true);
      ensure(0, max, true);
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [ensure, max]);

  const items = allItems.slice(page * max, page * max + max);
  const canPrev = page > 0;
  const canNext = apiMore || allItems.length > (page + 1) * max;

  const previous = () => {
    if (loading || !canPrev) return;
    setError('');
    setPage(page - 1);
  };

  const next = async () => {
    if (loading || !canNext) return;
    const target = page + 1;
    const ok = await ensure(target, max, false);
    if (ok) setPage(target);
  };

  const changeSize = (event: React.ChangeEvent<HTMLSelectElement>) => {
    const size = Number(event.target.value) || 100;
    setMax(size);
    setPage(0);
    setAllItems([]);
    setApiMore(true);
    ensure(0, size, true);
  };

  const remove = async (item: HistoryItem) => {
    try {
      await call('history.delete', { kid: item.kid });
      const nextItems = allRef.current.filter((row) => row.kid !== item.kid);
      allRef.current = nextItems;
      setAllItems(nextItems);
      toast('已删除');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const clear = async () => {
    if (!window.confirm('清空全部观看历史？此操作不可撤销。')) return;
    if (!window.confirm('再确认一次：真的要清空全部历史吗？')) return;
    try {
      await call('history.clear');
      requestRef.current += 1;
      allRef.current = [];
      cursorRef.current = 0;
      moreRef.current = false;
      setAllItems([]);
      setApiMore(false);
      setPage(0);
      setError('');
      setLoading(false);
      toast('历史已清空');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const retry = () => {
    setPage(0);
    setAllItems([]);
    setApiMore(true);
    ensure(0, max, true);
  };

  return (
    <>
      <div className="history-toolbar">
        <div className="history-pager">
          <button type="button" className="btn sm" disabled={loading || !canPrev} onClick={previous}>上一页</button>
          <button type="button" className="btn sm" disabled={loading || !canNext} onClick={next}>下一页</button>
          <span className="history-page">第 {page + 1} 页</span>
        </div>
        <label className="history-size">
          <span>每页</span>
          <select className="select" value={max} disabled={loading} onChange={changeSize}>
            {PAGE_SIZES.map((size) => <option key={size} value={size}>{size} 条</option>)}
          </select>
        </label>
        <button type="button" className="btn sm danger" disabled={!allItems.length || loading} onClick={clear}>清空历史</button>
      </div>

      {loading && !items.length ? <SkeletonGrid count={10} /> : null}

      {!loading && error && !items.length ? <ErrorBox error={error} onRetry={retry} /> : null}
      {!loading && !error && !items.length ? <Empty title="还没有观看历史" /> : null}

      {items.length ? (
        <>
          {error ? <div className="banner">{error}</div> : null}
          <VideoGrid
            items={items.map((item) => {
              const percent = item.duration > 0 && item.progress > 0
                ? Math.max(0, Math.min(100, Math.round((item.progress / item.duration) * 100)))
                : 0;
              return {
                ...item,
                pic: item.cover,
                owner: item.author,
                watchProgress: percent / 100,
                hideTime: true,
              };
            })}
            onPlay={(item, newWindow) => launch({
              bvid: item.bvid,
              title: item.title,
              cover: item.cover,
              owner: item.author,
              duration: item.duration,
              startMs: Math.max(0, Number(item.progress) || 0) * 1000,
            }, newWindow).catch((reason) => toast(reason.message, true))}
            renderAction={(item) => (
              <button type="button" className="card-remove icon history-remove" title="删除这条历史" onClick={() => remove(item)}>
                <IconTrash size={12} />
              </button>
            )}
          />
        </>
      ) : null}
    </>
  );
}
