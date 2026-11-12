import React, { useCallback, useEffect, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { FavFolder, FeedVideo } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, Tabs, VideoGrid } from '../components/Common';
import { IconTrash } from '../components/Icons';
import History from './History';

export default function Favorites() {
  const { toast, launch } = useApp();
  const [tab, setTab] = useState(0);
  const [feed, setFeed] = useState<FeedVideo[]>([]);
  const [folders, setFolders] = useState<FavFolder[]>([]);
  const [activeFolder, setActiveFolder] = useState(0);
  const [items, setItems] = useState<FeedVideo[]>([]);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const loadFolders = useCallback(async () => {
    try {
      const list = await call<FavFolder[]>('favorites.folders');
      setFolders(list);
      if (!activeFolder && list.length) {
        setActiveFolder(list[0].id);
      }
    } catch (reason: any) { setError(reason?.message || String(reason)); }
  }, [activeFolder]);

  const loadFeed = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const list = await call<FeedVideo[]>('feed.home', { fresh: 0 });
      setFeed(list);
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, []);

  const loadItems = useCallback(async (folderId: number, nextPage: number, append: boolean) => {
    if (!folderId) return;
    setLoading(true);
    setError('');
    try {
      const data = await call<{ items: FeedVideo[]; has_more: boolean }>('favorites.items', { mediaId: folderId, page: nextPage });
      setItems((current) => append ? [...current, ...data.items] : data.items);
      setPage(nextPage);
      setHasMore(data.has_more);
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, []);

  useEffect(() => { loadFeed(); loadFolders(); }, [loadFeed, loadFolders]);
  useEffect(() => {
    if (tab === 1 && activeFolder) loadItems(activeFolder, 1, false);
  }, [tab, activeFolder, loadItems]);

  useEffect(() => {
    const onRefresh = () => {
      loadFeed();
      loadFolders();
      if (tab === 1 && activeFolder) loadItems(activeFolder, 1, false);
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [activeFolder, loadFeed, loadFolders, loadItems, tab]);

  const createFolder = async () => {
    const title = window.prompt('新建收藏夹名称');
    if (!title?.trim()) return;
    try {
      await call('favorites.create', { title: title.trim() });
      toast('收藏夹已创建');
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const renameFolder = async (folder: FavFolder) => {
    const title = window.prompt('重命名收藏夹', folder.name);
    if (!title?.trim()) return;
    try {
      await call('favorites.rename', { mediaId: folder.id, title: title.trim() });
      toast('已重命名');
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const deleteFolder = async (folder: FavFolder) => {
    if (!window.confirm(`删除收藏夹「${folder.name}」？`)) return;
    try {
      await call('favorites.delete', { mediaId: folder.id });
      toast('已删除');
      if (activeFolder === folder.id) { setActiveFolder(0); setItems([]); }
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const removeItem = async (item: FeedVideo) => {
    try {
      await call('favorites.remove', { mediaId: activeFolder, aids: [item.aid] });
      setItems((current) => current.filter((row) => row.bvid !== item.bvid));
      toast('已取消收藏');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const clearFolder = async () => {
    const folder = folders.find((item) => item.id === activeFolder);
    if (!folder || !window.confirm(`取消收藏夹「${folder.name}」里的全部收藏？`)) return;
    try {
      const removed = await call<number>('favorites.clear', { mediaId: activeFolder });
      toast(`已取消 ${removed} 个收藏`);
      setItems([]);
      loadFolders();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const play = (item: FeedVideo) => launch({
    bvid: item.bvid,
    cid: item.cid,
    aid: item.aid,
    title: item.title,
    cover: item.pic,
    owner: item.owner,
    duration: item.duration,
  }).catch((reason) => toast(reason.message, true));

  return (
    <>
      <PageHead title="收藏" sub="收藏推荐 / 收藏夹 / 历史">
        <Tabs tabs={['推荐流', '收藏夹', '历史']} active={tab} onChange={setTab} />
      </PageHead>

      {tab === 0 ? (
        loading ? <SkeletonGrid count={10} /> : error ? <ErrorBox error={error} onRetry={loadFeed} /> : (
          <VideoGrid items={feed} onPlay={play} />
        )
      ) : null}

      {tab === 1 ? (
        <div style={{ display: 'grid', gridTemplateColumns: '220px 1fr', gap: 16, alignItems: 'start' }}>
          <div className="card panel" style={{ padding: 10 }}>
            <div style={{ display: 'flex', alignItems: 'center', marginBottom: 8 }}>
              <div style={{ fontWeight: 600, fontSize: 13 }}>收藏夹</div>
              <div style={{ flex: 1 }} />
              <button className="btn sm primary" onClick={createFolder}>新建</button>
            </div>
            {folders.map((folder) => (
              <div
                key={folder.id}
                style={{ padding: '8px 9px', borderRadius: 8, background: activeFolder === folder.id ? 'var(--accent-soft)' : undefined, color: activeFolder === folder.id ? 'var(--accent)' : undefined, cursor: 'pointer', display: 'flex', gap: 6, alignItems: 'center' }}
                onClick={() => { setActiveFolder(folder.id); setItems([]); loadItems(folder.id, 1, false); }}
              >
                <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: 12.5 }}>{folder.name}</span>
                <span style={{ fontSize: 10.5, color: 'var(--text-faint)' }}>{folder.count}</span>
                <button className="btn sm ghost" style={{ padding: 0, fontSize: 10 }} onClick={(event) => { event.stopPropagation(); renameFolder(folder); }}>改名</button>
                <button className="btn sm ghost" style={{ padding: 0, color: 'var(--danger)' }} onClick={(event) => { event.stopPropagation(); deleteFolder(folder); }}><IconTrash size={11} /></button>
              </div>
            ))}
            {!folders.length ? <div style={{ color: 'var(--text-faint)', fontSize: 12, padding: 8 }}>还没有收藏夹</div> : null}
          </div>
          <div>
            {activeFolder ? (
              <div className="card panel">
                <div style={{ display: 'flex', alignItems: 'center', marginBottom: 8 }}>
                  <div className="panel-title" style={{ margin: 0 }}>{folders.find((item) => item.id === activeFolder)?.name}</div>
                  <div style={{ flex: 1 }} />
                  <button className="btn sm danger" onClick={clearFolder}>取消全部收藏</button>
                </div>
                {loading ? <SkeletonGrid count={4} /> : error ? <ErrorBox error={error} onRetry={() => loadItems(activeFolder, 1, false)} /> : items.length ? (
                  <VideoGrid
                    items={items}
                    onPlay={play}
                    renderAction={(item) => <button className="card-remove" title="取消收藏" onClick={() => removeItem(item)}>取消收藏</button>}
                  />
                ) : <Empty title="这个收藏夹是空的" />}
                {hasMore ? <div style={{ display: 'flex', justifyContent: 'center', paddingTop: 14 }}><button className="btn" onClick={() => loadItems(activeFolder, page + 1, true)}>加载更多</button></div> : null}
              </div>
            ) : <Empty title="选择或新建一个收藏夹" />}
          </div>
        </div>
      ) : null}

      {tab === 2 ? <History embedded /> : null}
    </>
  );
}
