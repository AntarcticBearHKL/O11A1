import React, { useCallback, useEffect, useState } from 'react';
import { call } from '../api';
import { useApp } from '../context';
import type { CardPage, VideoCard } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid, VideoGrid } from '../components/Common';

const REGIONS: Array<[number, string]> = [
  [1, '动画'], [13, '番剧'], [167, '国创'], [3, '音乐'], [129, '舞蹈'],
  [4, '游戏'], [36, '知识'], [188, '科技'], [234, '运动'], [223, '汽车'],
  [160, '生活'], [211, '美食'], [217, '动物'], [119, '鬼畜'], [155, '时尚'],
  [202, '资讯'], [5, '娱乐'], [181, '影视'], [177, '纪录片'], [23, '电影'],
];

export default function Regions() {
  const { launch, toast } = useApp();
  const [selected, setSelected] = useState(0);
  const [mode, setMode] = useState(0);
  const [items, setItems] = useState<VideoCard[]>([]);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const load = useCallback(async (nextPage: number, append: boolean) => {
    setLoading(true);
    setError('');
    try {
      if (mode === 0) {
        const data = await call<CardPage>('regions.latest', { rid: REGIONS[selected][0], page: nextPage, ps: 24 });
        setItems((current) => append ? [...current, ...data.items] : data.items);
        setPage(data.page || nextPage);
        setHasMore(data.has_more);
      } else {
        const data = await call<VideoCard[]>('regions.ranking', { rid: REGIONS[selected][0] });
        setItems(data);
        setPage(1);
        setHasMore(false);
      }
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [mode, selected]);

  useEffect(() => { load(1, false); }, [load]);

  useEffect(() => {
    const onRefresh = () => load(1, false);
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  return (
    <>
      <PageHead title="分区" sub={REGIONS[selected][1]}>
        <div className="chips">
          <button className={`chip small ${mode === 0 ? 'active' : ''}`} onClick={() => setMode(0)}>最新发布</button>
          <button className={`chip small ${mode === 1 ? 'active' : ''}`} onClick={() => setMode(1)}>排行榜</button>
        </div>
      </PageHead>

      <div className="region-grid" style={{ marginBottom: 18 }}>
        {REGIONS.map(([rid, name], index) => (
          <button key={rid} className="region-tile" style={{ ...(index === selected ? { borderColor: 'var(--accent)', color: 'var(--accent)' } : {}) }} onClick={() => setSelected(index)}>
            {name}
          </button>
        ))}
      </div>

      {loading ? <SkeletonGrid count={8} /> : error ? <ErrorBox error={error} onRetry={() => load(1, false)} /> : items.length ? (
        <VideoGrid items={items} onPlay={(item) => launch({ bvid: item.bvid, title: item.title, cover: item.pic, owner: item.owner, duration: item.duration }).catch((error) => toast(error.message, true))} />
      ) : <Empty title="这个分区暂时没有视频" />}

      {mode === 0 && hasMore ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: 22 }}>
          <button className="btn" disabled={loading} onClick={() => load(page + 1, true)}>加载更多</button>
        </div>
      ) : null}
    </>
  );
}
