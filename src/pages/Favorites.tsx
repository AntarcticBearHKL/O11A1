import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call, formatCount } from '../api';
import { useApp } from '../context';
import type { FavFolder, FeedVideo } from '../types';
import { Empty, ErrorBox, SkeletonGrid, VideoGrid } from '../components/Common';
import { feedBatchCount } from '../feedLayout';
import { recommendFromFolder } from '../favRecommend';

const MODES = ['收藏夹推荐', '收藏夹'];

/// 收藏 is a full-window feed, not a rail + detail page. Row one switches
/// between the recommendation stream and the folder itself; row two selects
/// one of *every* folder the account owns. The recommendation stream samples
/// folder entries and their related videos (see `favRecommend.ts`), while the
/// folder mode is the plain contents page.
export default function Favorites() {
  const { toast, launch } = useApp();
  const [mode, setMode] = useState(0);
  const [folders, setFolders] = useState<FavFolder[]>([]);
  const [foldersLoading, setFoldersLoading] = useState(true);
  const [foldersError, setFoldersError] = useState('');
  const [activeFolder, setActiveFolder] = useState(0);
  const [feed, setFeed] = useState<FeedVideo[]>([]);
  const [note, setNote] = useState('');
  const [items, setItems] = useState<FeedVideo[]>([]);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const folder = folders.find((row) => row.id === activeFolder) || null;
  // The effect only reruns on a mode / folder-id change; a count update after
  // removing one video must not discard the pages already loaded below.
  const folderRef = useRef<FavFolder | null>(null);
  folderRef.current = folder;
  // A slow generation must not overwrite the folder the reader switched to.
  const requestRef = useRef(0);

  const loadFolders = useCallback(async () => {
    setFoldersLoading(true);
    setFoldersError('');
    try {
      const list = await call<Array<FavFolder & { member?: boolean | null }>>('favorites.folders.all');
      const clean = (Array.isArray(list) ? list : [])
        .filter((row) => row && Number(row.id) > 0)
        .map((row) => ({
          id: Number(row.id),
          name: String(row.name || '未命名收藏夹'),
          count: Math.max(0, Number(row.count) || 0),
        }));
      setFolders(clean);
      setActiveFolder((current) => {
        if (current && clean.some((row) => row.id === current)) return current;
        const withVideos = clean.find((row) => row.count > 0);
        return (withVideos || clean[0])?.id || 0;
      });
    } catch (reason: any) {
      setFoldersError(reason?.message || String(reason));
      setFolders([]);
      setActiveFolder(0);
    } finally {
      setFoldersLoading(false);
    }
  }, []);

  const generate = useCallback(async () => {
    const target = folderRef.current;
    if (!target) {
      setFeed([]);
      return;
    }
    const token = ++requestRef.current;
    setLoading(true);
    setError('');
    try {
      const next = await recommendFromFolder(target, feedBatchCount());
      if (token !== requestRef.current) return;
      setFeed(next);
      setNote(
        next.length
          ? `随机抽样自「${target.name}」+ 相关视频 · 已避开最近推荐过的视频`
          : '',
      );
    } catch (reason: any) {
      if (token !== requestRef.current) return;
      setError(reason?.message || String(reason));
      setFeed([]);
    } finally {
      if (token === requestRef.current) setLoading(false);
    }
  }, []);

  const loadItems = useCallback(async (folderId: number, nextPage: number, append: boolean) => {
    if (!folderId) return;
    const token = ++requestRef.current;
    setLoading(true);
    setError('');
    try {
      const data = await call<{ items: FeedVideo[]; has_more: boolean }>('favorites.items', {
        mediaId: folderId,
        page: nextPage,
      });
      if (token !== requestRef.current) return;
      const list = (data.items || []).filter((item) => item && item.bvid && item.title.trim() !== '已失效视频');
      setItems((current) => append ? [...current, ...list] : list);
      setPage(nextPage);
      setHasMore(Boolean(data.has_more));
    } catch (reason: any) {
      if (token !== requestRef.current) return;
      setError(reason?.message || String(reason));
    } finally {
      if (token === requestRef.current) setLoading(false);
    }
  }, []);

  useEffect(() => { loadFolders(); }, [loadFolders]);

  useEffect(() => {
    if (!activeFolder) {
      requestRef.current += 1;
      setFeed([]);
      setItems([]);
      return;
    }
    if (mode === 0) {
      generate();
    } else {
      setItems([]);
      loadItems(activeFolder, 1, false);
    }
  }, [mode, activeFolder, generate, loadItems]);

  useEffect(() => {
    const onRefresh = () => {
      loadFolders();
      // The folder-id effect skips a refresh when the id did not change, so
      // rebuild the current stream explicitly as well.
      if (mode === 0) generate();
      else if (activeFolder) loadItems(activeFolder, 1, false);
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [activeFolder, generate, loadFolders, loadItems, mode]);

  const createFolder = async () => {
    const title = window.prompt('新建收藏夹名称');
    if (!title?.trim()) return;
    try {
      await call('favorites.create', { title: title.trim() });
      toast('收藏夹已创建');
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const renameFolder = async (target: FavFolder) => {
    const title = window.prompt('重命名收藏夹', target.name);
    if (!title?.trim()) return;
    try {
      await call('favorites.rename', { mediaId: target.id, title: title.trim() });
      toast('已重命名');
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const deleteFolder = async (target: FavFolder) => {
    if (!window.confirm(`删除收藏夹「${target.name}」？`)) return;
    try {
      await call('favorites.delete', { mediaId: target.id });
      toast('已删除');
      setActiveFolder(0);
      setFeed([]);
      setItems([]);
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const clearFolder = async () => {
    if (!folder || !window.confirm(`取消收藏夹「${folder.name}」里的全部收藏？`)) return;
    try {
      const removed = await call<number>('favorites.clear', { mediaId: folder.id });
      toast(`已取消 ${removed} 个收藏`);
      setItems([]);
      setFeed([]);
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const removeItem = async (item: FeedVideo) => {
    if (!folder) return;
    try {
      await call('favorites.remove', { mediaId: folder.id, aids: [item.aid] });
      setItems((current) => current.filter((row) => row.bvid !== item.bvid));
      setFolders((current) => current.map((row) => row.id === folder.id
        ? { ...row, count: Math.max(0, row.count - 1) }
        : row));
      toast('已取消收藏');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const play = (item: FeedVideo, newWindow = false) => launch({
    bvid: item.bvid,
    cid: item.cid,
    aid: item.aid,
    title: item.title,
    cover: item.pic,
    owner: item.owner,
    duration: item.duration,
  }, newWindow).catch((reason) => toast(reason.message, true));

  if (foldersError && !folders.length) {
    return <ErrorBox error={foldersError} onRetry={loadFolders} />;
  }

  const showSkeleton = (foldersLoading && !folders.length)
    || (mode === 0 ? loading && !feed.length : loading && !items.length);

  return (
    <div className="fav-page">
      <div className="fav-filter">
        <div className="fav-row">
          <div className="fav-row-scroll">
            {MODES.map((label, index) => (
              <button
                key={label}
                type="button"
                className={`fav-toggle fav-mode ${mode === index ? 'active' : ''}`}
                aria-pressed={mode === index}
                onClick={() => setMode(index)}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="fav-row-tail">
            {mode === 0 ? (
              <button
                type="button"
                className="fav-toggle"
                disabled={loading || !folder}
                onClick={generate}
              >
                换一批
              </button>
            ) : null}
            {mode === 1 && folder ? (
              <>
                <button type="button" className="fav-toggle" onClick={() => renameFolder(folder)}>改名</button>
                <button type="button" className="fav-toggle danger" onClick={() => deleteFolder(folder)}>删除</button>
              </>
            ) : null}
          </div>
        </div>
        <div className="fav-row">
          <div className="fav-row-scroll">
            {folders.map((row) => (
              <button
                key={row.id}
                type="button"
                className={`fav-toggle fav-folder ${row.id === activeFolder ? 'active' : ''}`}
                aria-pressed={row.id === activeFolder}
                title={`${row.name} · ${row.count} 个视频`}
                onClick={() => setActiveFolder(row.id)}
              >
                {row.name}
                <span className="fav-count">{formatCount(row.count)}</span>
              </button>
            ))}
            {foldersLoading && !folders.length ? <span className="fav-inline">正在读取收藏夹…</span> : null}
            {!foldersLoading && !folders.length ? <span className="fav-inline">还没有收藏夹</span> : null}
            <button type="button" className="fav-toggle fav-new" onClick={createFolder}>+ 新建收藏夹</button>
          </div>
        </div>
        {mode === 1 && folder ? (
          <div className="fav-folder-actions">
            <span>{folder.name} · {formatCount(folder.count)} 个视频</span>
            <span className="spacer" />
            <button type="button" className="fav-toggle danger" onClick={clearFolder}>取消全部收藏</button>
          </div>
        ) : null}
      </div>

      {showSkeleton ? <SkeletonGrid count={18} /> : null}

      {!showSkeleton && mode === 0 ? (
        error && !feed.length ? (
          <ErrorBox error={error} onRetry={generate} />
        ) : feed.length ? (
          <>
            <VideoGrid items={feed} onPlay={play} />
            <div className="fav-end">
              <span>{note}</span>
              <button type="button" className="fav-toggle" disabled={loading} onClick={generate}>
                换一批
              </button>
            </div>
          </>
        ) : (
          <Empty title="这个收藏夹还没有推荐" hint="换一个收藏夹，或先在视频卡片上收藏一些视频" />
        )
      ) : null}

      {!showSkeleton && mode === 1 ? (
        !folder ? (
          <Empty title="选择或新建一个收藏夹" hint="上方第二行是账号里的全部收藏夹" />
        ) : error && !items.length ? (
          <ErrorBox error={error} onRetry={() => loadItems(folder.id, 1, false)} />
        ) : items.length ? (
          <>
            <VideoGrid
              items={items}
              onPlay={play}
              renderAction={(item) => (
                <button type="button" className="card-remove fav-remove" onClick={() => removeItem(item)}>取消收藏</button>
              )}
            />
            {hasMore ? (
              <div className="page-more">
                <button
                  type="button"
                  className="fav-toggle"
                  disabled={loading}
                  onClick={() => loadItems(folder.id, page + 1, true)}
                >
                  加载更多
                </button>
              </div>
            ) : null}
          </>
        ) : (
          <Empty title="这个收藏夹是空的" hint="换一个收藏夹，或在视频卡片上点收藏" />
        )
      ) : null}
    </div>
  );
}
