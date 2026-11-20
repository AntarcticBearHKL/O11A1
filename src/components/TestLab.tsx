import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { call, formatCount, formatDuration } from '../api';
import { useApp } from '../context';
import type { FeedVideo, FishToolRun, FishToolSpec } from '../types';
import { CoverImage } from './CoverImage';
import { FrameLab } from './FrameLab';
import { ToolLab } from './ToolLab';
import { Empty } from './Common';

type SelectedVideo = {
  bvid: string;
  cid?: number;
  title?: string;
  owner?: string;
  pic?: string;
  duration?: number;
  view?: number;
};

type RelationUser = {
  mid: number;
  uname: string;
  face: string;
  sign: string;
  official: string;
};

type RelationPage = {
  total: number;
  page: number;
  has_more: boolean;
  items: RelationUser[];
};

const LAB_TABS = ['来源', '固定流程', '画面拼接', '低层工具'];

const selectFromFeed = (item: FeedVideo): SelectedVideo => ({
  bvid: item.bvid,
  cid: item.cid,
  title: item.title,
  owner: item.owner,
  pic: item.pic,
  duration: item.duration,
  view: item.view,
});

export function TestLab({ tools, seedBvid = '' }: { tools: FishToolSpec[]; seedBvid?: string }) {
  const { toast } = useApp();
  const [tab, setTab] = useState(0);
  const [selected, setSelected] = useState<SelectedVideo | null>(
    seedBvid ? { bvid: seedBvid } : null,
  );

  const pick = useCallback((video: SelectedVideo, nextTab = 1) => {
    setSelected(video);
    setTab(nextTab);
  }, []);

  return (
    <div className="test-lab">
      <div className="test-bar">
        <div className="test-tabs">
          {LAB_TABS.map((label, index) => (
            <button
              key={label}
              className={`test-tab ${tab === index ? 'active' : ''}`}
              onClick={() => setTab(index)}
            >
              {label}
            </button>
          ))}
        </div>
        {selected ? <span className="test-current-bvid">{selected.bvid}</span> : null}
      </div>

      {tab === 0 ? (
        <ContextTools selected={selected} onSelect={(video) => pick(video)} onToast={toast} />
      ) : null}

      {tab === 1 ? <WatchFlow selected={selected} onPick={pick} onOpenSources={() => setTab(0)} /> : null}

      {tab === 2 ? (
        <FrameLab key={selected?.bvid || seedBvid || 'frame-lab'} seedBvid={selected?.bvid || seedBvid} embedded />
      ) : null}

      {tab === 3 ? (
        <ToolLab
          key={selected?.bvid || seedBvid || 'tool-lab'}
          tools={tools}
          seedBvid={selected?.bvid || seedBvid}
        />
      ) : null}
    </div>
  );
}

/// Context tools are deliberately small and auditable: one feed pick, the
/// following list, one UP's newest videos, and the watched ledger. None of
/// them watches anything; the VideoFlow below is the fixed watch tool.
function ContextTools({
  selected,
  onSelect,
  onToast,
}: {
  selected: SelectedVideo | null;
  onSelect: (video: SelectedVideo) => void;
  onToast: (text: string, error?: boolean) => void;
}) {
  const [feedBusy, setFeedBusy] = useState(false);
  const [follows, setFollows] = useState<RelationUser[] | null>(null);
  const [followsBusy, setFollowsBusy] = useState(false);
  const [mid, setMid] = useState('');
  const [upName, setUpName] = useState('');
  const [upVideos, setUpVideos] = useState<FeedVideo[] | null>(null);
  const [upBusy, setUpBusy] = useState(false);
  const [watched, setWatched] = useState<boolean | null>(null);
  const [watchedBusy, setWatchedBusy] = useState(false);

  useEffect(() => {
    setWatched(null);
  }, [selected?.bvid]);

  const nextVideo = async () => {
    setFeedBusy(true);
    try {
      const item = await call<FeedVideo | null>('fish.feed.next');
      if (!item) {
        onToast('首页推荐里已经没有未看过的视频', true);
        return;
      }
      onSelect(selectFromFeed(item));
    } catch (reason: any) {
      onToast(reason?.message || String(reason), true);
    } finally {
      setFeedBusy(false);
    }
  };

  const loadFollows = async () => {
    setFollowsBusy(true);
    try {
      const page = await call<RelationPage>('fish.follows', { page: 1 });
      setFollows(page.items || []);
      if (!page.items?.length) onToast('关注列表为空');
    } catch (reason: any) {
      onToast(reason?.message || String(reason), true);
    } finally {
      setFollowsBusy(false);
    }
  };

  const loadUp = async (value: string, name = '') => {
    const id = Number(value);
    if (!Number.isFinite(id) || id <= 0) {
      onToast('请输入正确的 UP mid', true);
      return;
    }
    setUpBusy(true);
    setUpName(name);
    try {
      const page = await call<{ items: FeedVideo[]; has_more: boolean }>('fish.up.videos', {
        mid: id,
        page: 1,
      });
      setUpVideos(page.items || []);
      if (!page.items?.length) onToast('这个 UP 没有可读取的投稿');
    } catch (reason: any) {
      onToast(reason?.message || String(reason), true);
    } finally {
      setUpBusy(false);
    }
  };

  const checkWatched = async () => {
    if (!selected) return;
    setWatchedBusy(true);
    try {
      const result = await call<{ watched: boolean }>('fish.watched.check', { bvid: selected.bvid });
      setWatched(result.watched);
    } catch (reason: any) {
      onToast(reason?.message || String(reason), true);
    } finally {
      setWatchedBusy(false);
    }
  };

  const markWatched = async () => {
    if (!selected) return;
    setWatchedBusy(true);
    try {
      await call('fish.watched.mark', { bvid: selected.bvid });
      setWatched(true);
      onToast('已标记为看过');
    } catch (reason: any) {
      onToast(reason?.message || String(reason), true);
    } finally {
      setWatchedBusy(false);
    }
  };

  const videoList = (items: FeedVideo[], source: string) => (
    <div className="test-video-list">
      {items.map((item) => (
        <button className="test-video-row" key={`${source}-${item.bvid}`} onClick={() => onSelect(selectFromFeed(item))}>
          <CoverImage src={item.pic} alt="" className="test-video-cover" />
          <div className="test-video-copy">
            <div className="test-video-title">{item.title}</div>
            <div className="test-video-meta">
              <span>{item.owner}</span>
              <span>{formatDuration(item.duration)}</span>
              <span>{formatCount(item.view)} 播放</span>
              <span>{item.bvid}</span>
            </div>
          </div>
        </button>
      ))}
    </div>
  );

  return (
    <div className="context-lab">
      <div className="context-actions">
        <button className="btn sm primary" disabled={feedBusy} onClick={nextVideo}>
          {feedBusy ? '读取中…' : '换一个新视频'}
        </button>
        <button className="btn sm" disabled={followsBusy} onClick={loadFollows}>
          {followsBusy ? '读取中…' : '关注列表'}
        </button>
        <div className="context-mid">
          <input
            value={mid}
            onChange={(event) => setMid(event.target.value.replace(/\D/g, ''))}
            onKeyDown={(event) => { if (event.key === 'Enter') loadUp(mid); }}
            placeholder="UP mid"
            inputMode="numeric"
          />
          <button className="btn sm" disabled={upBusy} onClick={() => loadUp(mid)}>
            {upBusy ? '…' : '读取 UP'}
          </button>
        </div>
      </div>

      {selected ? (
        <div className="context-selected">
          <div>
            <div className="context-selected-title">{selected.title || selected.bvid}</div>
            <div className="context-selected-meta">
              {selected.owner ? <span>{selected.owner}</span> : null}
              {selected.duration ? <span>{formatDuration(selected.duration)}</span> : null}
              <span>{selected.bvid}</span>
            </div>
          </div>
          <div className="context-watched">
            {watched == null ? (
              <span className="context-watched-unknown">未检查</span>
            ) : watched ? (
              <span className="context-watched-on">已看过</span>
            ) : (
              <span className="context-watched-off">未看过</span>
            )}
            <button className="btn sm ghost" disabled={watchedBusy} onClick={checkWatched}>检查</button>
            <button className="btn sm" disabled={watchedBusy || watched === true} onClick={markWatched}>标记看过</button>
            <button className="btn sm primary" onClick={() => onSelect(selected)}>开始观看</button>
          </div>
        </div>
      ) : null}

      {follows ? (
        <section className="context-section">
          <div className="context-section-head">
            <div className="context-section-title">关注的人</div>
            <button className="btn sm ghost" onClick={() => setFollows(null)}>收起</button>
          </div>
          <div className="context-follows">
            {follows.map((user) => (
              <button className="context-follow" key={user.mid} onClick={() => { setMid(String(user.mid)); loadUp(String(user.mid), user.uname); }}>
                <CoverImage src={user.face} alt="" className="context-follow-face" />
                <span>{user.uname}</span>
              </button>
            ))}
          </div>
        </section>
      ) : null}

      {upVideos ? (
        <section className="context-section">
          <div className="context-section-head">
            <div className="context-section-title">{upName || 'UP 主'} 的最新投稿</div>
            <button className="btn sm ghost" onClick={() => setUpVideos(null)}>收起</button>
          </div>
          {videoList(upVideos, 'up')}
        </section>
      ) : null}

      {!follows && !upVideos ? (
        <div className="context-empty-line">
          {selected ? '视频已就位，去「固定流程」开始。' : '先取一个视频，或从关注列表 / UP mid 里挑。'}
        </div>
      ) : null}
    </div>
  );
}

const WATCH_STEPS: Array<{ id: string; label: string; hint: string; heavy?: boolean }> = [
  { id: 'meta', label: '读取信息', hint: '标题、UP、cid、时长和简介。' },
  { id: 'subtitle', label: '官方字幕', hint: '拉取字幕文本和来源覆盖率。' },
  { id: 'outline', label: 'AI 提纲', hint: 'B 站 conclusion/get 提纲。' },
  { id: 'comments', label: '评论样本', hint: '热门评论和弹幕峰值。' },
  { id: 'asr', label: '语音转写', hint: '本地 Whisper，长视频会慢。', heavy: true },
  { id: 'frames', label: '画面观察', hint: '按设置间隔抽帧拼图，再让 vision 逐格看。', heavy: true },
];

/// The fixed watch tool, shown as a manual stepper. A human advances one step
/// at a time; the Agent runs the same order and receives the same records.
function WatchFlow({
  selected,
  onPick,
  onOpenSources,
}: {
  selected: SelectedVideo | null;
  onPick: (video: SelectedVideo, tab?: number) => void;
  onOpenSources: () => void;
}) {
  const [results, setResults] = useState<Record<string, FishToolRun>>({});
  const [running, setRunning] = useState('');
  const [error, setError] = useState('');

  useEffect(() => {
    setResults({});
    setError('');
    setRunning('');
  }, [selected?.bvid]);

  const done = useMemo(() => WATCH_STEPS.filter((step) => results[step.id]).length, [results]);
  const complete = done >= WATCH_STEPS.length;
  const next = WATCH_STEPS.find((step) => !results[step.id]) || null;

  const runNext = async () => {
    if (!selected || !next || running) return;
    setRunning(next.id);
    setError('');
    try {
      const result = await call<FishToolRun>('fish.tool.run', {
        id: next.id,
        bvid: selected.bvid,
        cid: selected.cid || 0,
      });
      setResults((current) => ({ ...current, [next.id]: result }));
      if (result.state === 'failed') setError(result.reason || `${next.label} 失败`);
    } catch (reason: any) {
      setError(reason?.message || String(reason));
    } finally {
      setRunning('');
    }
  };

  if (!selected) {
    return (
      <div className="watch-flow-empty">
        <Empty title="还没有测试视频" hint="去「来源」取一个新视频，或选一个关注 / UP 的投稿。" />
        <button className="btn" onClick={onOpenSources}>打开来源</button>
      </div>
    );
  }

  return (
    <div className="watch-flow">
      <section className="watch-flow-head">
        <div>
          <div className="watch-flow-title">{selected.title || selected.bvid}</div>
        </div>
        <div className="watch-flow-progress">
          <b>{done}</b><span>/ {WATCH_STEPS.length}</span>
        </div>
      </section>

      <div className="watch-steps">
        {WATCH_STEPS.map((step, index) => {
          const result = results[step.id];
          const active = running === step.id;
          const isNext = !result && !active && next?.id === step.id;
          const blocked = !result && !active && next?.id !== step.id && !complete;
          return (
            <article className={`watch-step ${result ? result.state : ''} ${active ? 'running' : ''} ${isNext ? 'next' : ''}`} key={step.id}>
              <div className="watch-step-rail">
                <span className="watch-step-index">{String(index + 1).padStart(2, '0')}</span>
              </div>
              <div className="watch-step-body">
                <div className="watch-step-head">
                  <div>
                    <div className="watch-step-title">{step.label}</div>
                    <div className="watch-step-hint">{step.hint}</div>
                  </div>
                  <div className="watch-step-state">
                    {result ? (
                      <span className={`watch-state ${result.state}`}>{result.state}</span>
                    ) : active ? (
                      <span className="watch-state running">运行中</span>
                    ) : (
                      <span className="watch-state idle">待运行</span>
                    )}
                    {result ? <span className="watch-step-cost">{result.ms} ms · {result.tokens} tok</span> : null}
                  </div>
                </div>
                {result ? (
                  <>
                    <div className="watch-step-detail">{result.detail || result.reason || '已完成'}</div>
                    {result.preview ? (
                      <details className="watch-step-preview">
                        <summary>查看材料摘要</summary>
                        <pre>{result.preview}</pre>
                      </details>
                    ) : null}
                  </>
                ) : null}
              </div>
              <button
                className={`btn sm watch-step-run ${isNext ? 'primary' : ''}`}
                disabled={Boolean(running) || Boolean(result) || blocked}
                onClick={next?.id === step.id ? runNext : undefined}
              >
                {active ? '运行中…' : result ? '已完成' : blocked ? '等待上一步' : '运行这一步'}
              </button>
            </article>
          );
        })}
      </div>

      <div className="watch-flow-foot">
        {error ? <div className="watch-flow-error">{error}</div> : null}
        <div className="watch-flow-actions">
          <button className="btn primary" disabled={!next || Boolean(running)} onClick={runNext}>
            {running ? '正在运行…' : next ? `运行下一步：${next.label}` : '流程完成'}
          </button>
          {done > 0 ? (
            <button className="btn sm" disabled={Boolean(running)} onClick={() => { setResults({}); setError(''); }}>
              重新开始
            </button>
          ) : null}
          <button className="btn sm ghost" onClick={() => onPick(selected, 0)}>换一个视频</button>
        </div>
        {complete ? (
          <div className="watch-flow-done">
            <div className="watch-flow-done-title">流程走完</div>
            <div className="watch-flow-done-sub">这就是 AI 实际使用的固定看片工具；正式运行时它直接把最终结论交给 Agent。</div>
          </div>
        ) : (
          <div className="watch-flow-note">
            {next?.heavy ? '这一步可能很慢，长视频建议先完成字幕 / 评论，再单独跑语音或画面。' : '每次只跑一步，结果会留在上方。'}
          </div>
        )}
      </div>
    </div>
  );
}
