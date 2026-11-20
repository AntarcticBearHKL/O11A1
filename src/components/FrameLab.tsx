import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { call, formatCount, formatDuration } from '../api';
import { useApp } from '../context';
import { CoverImage } from './CoverImage';
import { ImageViewer } from './ImageViewer';
import { ErrorBox, Spinner } from './Common';
import { IconFull } from './Icons';

type VideoMeta = {
  bvid: string;
  cid: number;
  title: string;
  up_name: string;
  cover: string;
  duration_s: number;
  view: number;
};

type LabSheet = {
  index: number;
  from_s: number;
  to_s: number;
  times: number[];
  columns: number;
  rows: number;
  width: number;
  height: number;
  image: string;
};

type LabBuild = {
  lab_id: string;
  video: VideoMeta;
  interval_s: number;
  frame_count: number;
  sheet_count: number;
  source: string;
  video_bytes: number;
  image_bytes: number;
  elapsed_ms: number;
  sheets: LabSheet[];
};

type TileObservation = {
  at: string;
  scene: string;
  action: string;
  onscreen_text: string[];
  details: string;
};

type SheetObservation = {
  index: number;
  from_s: number;
  to_s: number;
  times: number[];
  tokens: number;
  summary: string;
  flags: string[];
  tiles: TileObservation[];
};

type LabObservation = {
  lab_id: string;
  tokens: number;
  elapsed_ms: number;
  failed: number;
  sheets: SheetObservation[];
};

type LabStatus = {
  running: boolean;
  kind: string;
  stage: string;
  message: string;
  pct: number;
  current: number;
  total: number;
  elapsed_ms: number;
  error: string;
  has_result: boolean;
  has_lab: boolean;
};

const EMPTY_JOB: LabStatus = {
  running: false,
  kind: '',
  stage: '',
  message: '',
  pct: 0,
  current: 0,
  total: 0,
  elapsed_ms: 0,
  error: '',
  has_result: false,
  has_lab: false,
};

const INTERVALS = [5, 10, 15, 30, 60, 120, 300];
const MAX_FRAMES = 60;

const FOCUS_PRESETS: Array<[string, string]> = [
  ['广告线索', '重点看口播、商品、价格、二维码、恰饭提示、重复镜头和明显的硬切广告。'],
  ['教程步骤', '按时间顺序记录每一步操作、界面变化、屏幕上的参数和结果，尽量抄录原文。'],
  ['游戏画面', '记录游戏名、地图、UI、角色动作、关键战斗和数值变化。'],
  ['剧情人物', '记录人物、场景、动作、对白字幕、转场和画面风格。'],
  ['屏幕文字', '逐格抄录所有能看清的文字、数字、菜单、代码和图表标签，看不清就写看不清。'],
];

const parseBvid = (value: string) => {
  const match = value.match(/BV[0-9A-Za-z]{10}/);
  return match ? match[0] : '';
};

const mmss = (seconds: number) => {
  const total = Math.max(0, Math.floor(seconds || 0));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const rest = total % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(rest).padStart(2, '0')}`
    : `${minutes}:${String(rest).padStart(2, '0')}`;
};

const rangeLabel = (from: number, to: number) => from === to ? mmss(from) : `${mmss(from)}–${mmss(to)}`;

const formatBytes = (value: number | undefined | null) => {
  const bytes = Number(value || 0);
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024).toFixed(0)} KB`;
};

type Props = {
  seedBvid?: string;
  embedded?: boolean;
};

/// 养鱼 → 画面：one video, a user-chosen interval, a timestamped contact sheet,
/// and the vision model reading it tile by tile. Build and observe are two
/// separate server jobs so the owner can preview the sheets before spending
/// vision tokens.
export function FrameLab({ seedBvid = '', embedded = false }: Props) {
  const { toast } = useApp();
  const [input, setInput] = useState(seedBvid);
  const [video, setVideo] = useState<VideoMeta | null>(null);
  const [metaLoading, setMetaLoading] = useState(false);
  const [metaError, setMetaError] = useState('');
  const [interval, setIntervalValue] = useState(30);
  const [job, setJob] = useState<LabStatus>(EMPTY_JOB);
  const [lab, setLab] = useState<LabBuild | null>(null);
  const [observation, setObservation] = useState<LabObservation | null>(null);
  const [focus, setFocus] = useState('');
  const [lastFocus, setLastFocus] = useState('');
  const [openSheets, setOpenSheets] = useState<Record<number, boolean>>({});
  const [viewer, setViewer] = useState<{ images: string[]; index: number } | null>(null);
  const seeded = useRef(false);
  const findingsRef = useRef<HTMLElement | null>(null);

  const loadMeta = useCallback(async (raw: string) => {
    const bvid = parseBvid(raw) || raw.trim();
    if (!bvid) {
      setMetaError('请输入 BV 号或视频链接');
      return;
    }
    seeded.current = true;
    setMetaLoading(true);
    setMetaError('');
    try {
      const data = await call<VideoMeta>('fish.video.meta', { bvid });
      setVideo(data);
      setInput(data.bvid);
    } catch (reason: any) {
      setVideo(null);
      setMetaError(reason?.message || String(reason));
    } finally {
      setMetaLoading(false);
    }
  }, []);

  useEffect(() => {
    if (seeded.current || !seedBvid) return;
    seeded.current = true;
    setInput(seedBvid);
    loadMeta(seedBvid);
  }, [loadMeta, seedBvid]);

  // When the vision result arrives, bring the timeline into view instead of
  // leaving it below the fixed dock.
  useEffect(() => {
    if (!observation) return;
    const timer = window.setTimeout(() => {
      findingsRef.current?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }, 320);
    return () => window.clearTimeout(timer);
  }, [observation]);

  // Poll one background lab job. Starting a build resets the job to `running`
  // and this effect takes over; every terminal state fetches the result once.
  useEffect(() => {
    if (!job.running) return;
    let stopped = false;
    const tick = async () => {
      try {
        const next = await call<LabStatus>('fish.frames.status');
        if (stopped) return;
        setJob(next);
        if (next.running) return;
        if (next.error) {
          toast(next.error, true);
          return;
        }
        if (next.has_result) {
          const result = await call<any>('fish.frames.result');
          if (next.kind === 'build') {
            setLab(result);
            setObservation(null);
            if (result?.video) setVideo(result.video);
            toast('拼图已生成');
          } else {
            setObservation(result);
            setOpenSheets(result?.sheets?.[0] ? { [result.sheets[0].index]: true } : {});
            toast('AI 细看完成');
          }
        } else if (next.stage === 'error') {
          toast('任务失败', true);
        }
      } catch (reason: any) {
        if (!stopped) {
          toast(reason?.message || String(reason), true);
          setJob(EMPTY_JOB);
        }
      }
    };
    tick();
    const timer = window.setInterval(tick, 700);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [job.kind, job.running, toast]);

  const rawCount = video ? Math.floor(video.duration_s / Math.max(2, interval)) + 1 : 0;
  const tooMany = rawCount > MAX_FRAMES;
  const frameCount = Math.min(rawCount, MAX_FRAMES);
  const sheetCount = Math.ceil(frameCount / 6);
  const minimumInterval = video ? Math.max(2, Math.ceil(video.duration_s / (MAX_FRAMES - 1))) : 2;
  const building = job.running && job.kind === 'build';
  const observing = job.running && job.kind === 'observe';
  const busy = job.running;

  const focusLine = useMemo(() => focus.trim(), [focus]);

  const changeInterval = (value: number) => {
    const next = Math.max(2, Math.min(3600, Math.round(value || 0)));
    setIntervalValue(next);
  };

  const startBuild = async () => {
    if (!video) {
      toast('先读取一个视频', true);
      return;
    }
    if (tooMany) {
      toast(`间隔太短，最多 ${MAX_FRAMES} 张画面，请调到 ${minimumInterval} 秒以上`, true);
      return;
    }
    setLab(null);
    setObservation(null);
    setJob({ ...EMPTY_JOB, running: true, kind: 'build', stage: 'meta', message: '准备抽帧…' });
    try {
      await call('fish.frames.start', { bvid: video.bvid, interval });
    } catch (reason: any) {
      setJob(EMPTY_JOB);
      toast(reason?.message || String(reason), true);
    }
  };

  const startObserve = async () => {
    if (!lab) return;
    setObservation(null);
    setLastFocus(focusLine);
    setJob({
      ...EMPTY_JOB,
      running: true,
      kind: 'observe',
      stage: 'vision',
      message: '准备让 AI 细看拼图…',
      total: lab.sheet_count,
    });
    try {
      await call('fish.frames.observe', { labId: lab.lab_id, focus: focusLine });
    } catch (reason: any) {
      setJob(EMPTY_JOB);
      toast(reason?.message || String(reason), true);
    }
  };

  const cancel = async () => {
    try {
      await call('fish.frames.cancel');
      toast('正在取消…');
    } catch (reason: any) {
      toast(reason?.message || String(reason), true);
    }
  };

  const copyObservation = async () => {
    if (!observation) return;
    const text = observation.sheets
      .map((sheet) => {
        const header = `拼图 ${sheet.index}（${rangeLabel(sheet.from_s, sheet.to_s)}）`;
        const summary = sheet.summary ? `\n${sheet.summary}` : '';
        const tiles = sheet.tiles
          .map((tile) => {
            const parts = [tile.scene, tile.action, tile.details].filter(Boolean).join('；');
            const screen = tile.onscreen_text?.length ? `\n屏幕：${tile.onscreen_text.join(' / ')}` : '';
            return `- ${tile.at || ''} ${parts}${screen}`;
          })
          .join('\n');
        const flags = sheet.flags?.length ? `\n线索：${sheet.flags.join(' / ')}` : '';
        return `${header}${summary}\n${tiles}${flags}`;
      })
      .join('\n\n');
    try {
      await navigator.clipboard.writeText(text);
      toast('观察结果已复制');
    } catch {
      toast('复制失败', true);
    }
  };

  return (
    <div className={`frame-lab ${embedded ? 'embedded' : ''}`}>
      <header className="lab-head" style={embedded ? { display: 'none' } : undefined}>
        <div className="lab-head-copy">
          <div className="lab-kicker">画面拆解</div>
          <h2 className="lab-heading">按你的节奏看画面</h2>
          <p className="lab-heading-sub">选一个间隔，把视频拆成带时间戳的拼图，再让 AI 逐格读屏幕上的细节。</p>
        </div>
        <div className="lab-search">
          <input
            className="lab-search-input"
            value={input}
            onChange={(event) => setInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') loadMeta(input);
            }}
            placeholder="粘贴 BV 号或视频链接"
            spellCheck={false}
          />
          <button className="btn sm primary" disabled={metaLoading} onClick={() => loadMeta(input)}>
            {metaLoading ? '读取中…' : '读取'}
          </button>
        </div>
      </header>

      {metaLoading && !video ? <Spinner label="读取视频信息…" /> : null}
      {metaError ? <ErrorBox error={metaError} onRetry={() => loadMeta(input)} /> : null}
      {embedded && !video && !metaLoading && !metaError ? (
        <div className="lab-embedded-hint">先在「来源」里选一个视频，再回来生成拼图。</div>
      ) : null}

      {video ? (
        <div className="lab-video">
          <CoverImage src={video.cover} alt="" className="lab-video-cover" eager />
          <div className="lab-video-copy">
            <div className="lab-video-title">{video.title}</div>
            <div className="lab-video-meta">
              <span>{video.up_name}</span>
              <span>{formatDuration(video.duration_s)}</span>
              <span>{formatCount(video.view)} 播放</span>
              <span className="lab-video-bvid">{video.bvid}</span>
            </div>
          </div>
          {seedBvid && video.bvid !== seedBvid ? (
            <button className="btn sm ghost" onClick={() => { setInput(seedBvid); loadMeta(seedBvid); }}>
              用正在看的视频
            </button>
          ) : null}
        </div>
      ) : null}

      <div className="lab-grid">
        <div className="lab-main">
          <section className="lab-step-block">
            <div className="lab-step-head">
              <span className="lab-step-number">01</span>
              <div>
                <div className="lab-step-title">采样间隔</div>
                <div className="lab-step-sub">间隔越短，拼图越密，AI 看到的动作变化也越多。</div>
              </div>
            </div>

            <div className="lab-intervals">
              {INTERVALS.map((value) => (
                <button
                  key={value}
                  className={`lab-interval ${interval === value ? 'active' : ''}`}
                  onClick={() => changeInterval(value)}
                >
                  {value < 60 ? `${value}s` : `${value / 60}m`}
                </button>
              ))}
              <label className="lab-custom">
                <input
                  type="number"
                  min={2}
                  max={3600}
                  value={interval}
                  onChange={(event) => changeInterval(Number(event.target.value))}
                />
                <span>秒</span>
              </label>
            </div>

            <div className="lab-plan">
              <div className="lab-plan-numbers">
                <span><b>{video ? frameCount : '—'}</b> 张画面</span>
                <span><b>{video ? sheetCount : '—'}</b> 张拼图</span>
                <span className="lab-plan-fixed">每张 6 格 · 720P</span>
              </div>
              {video && tooMany ? (
                <div className="lab-warn">会超过 {MAX_FRAMES} 张上限，请把间隔调到 {minimumInterval} 秒以上。</div>
              ) : video ? (
                <div className="lab-plan-range">覆盖 0:00–{mmss(video.duration_s)} · 每格 {interval} 秒</div>
              ) : (
                <div className="lab-plan-range">读取视频后显示预计画面数量。</div>
              )}
            </div>

            <div className="lab-actions">
              <button className="btn primary" disabled={!video || busy || tooMany} onClick={startBuild}>
                {building ? '正在生成拼图…' : lab ? '重新生成拼图' : '生成拼图'}
              </button>
              {busy ? <button className="btn sm" onClick={cancel}>取消</button> : null}
              {lab ? (
                <span className="lab-stat">
                  {lab.frame_count} 帧 · {formatBytes(lab.image_bytes)} · {(lab.elapsed_ms / 1000).toFixed(1)}s
                </span>
              ) : null}
            </div>

            {job.running ? (
              <div className="lab-progress">
                <div className="lab-progress-head">
                  <span>{job.message || '处理中…'}</span>
                  <span>{job.pct}%</span>
                </div>
                <div className="lab-progress-track"><div style={{ width: `${job.pct}%` }} /></div>
              </div>
            ) : null}
            {!job.running && job.error ? <div className="lab-inline-error">{job.error}</div> : null}
            <p className="lab-cost-note">生成时会临时下载 720P 整片并自动删除；拼图越密，抽帧时间越长。</p>
          </section>

          <section className="lab-step-block lab-sheet-block">
            <div className="lab-step-head">
              <span className="lab-step-number">02</span>
              <div>
                <div className="lab-step-title">拼图预览</div>
                <div className="lab-step-sub">每格左上角是时间。点开大图看原始细节。</div>
              </div>
            </div>

            {lab?.sheets?.length ? (
              <div className="lab-sheets">
                {lab.sheets.map((sheet) => (
                  <article className="lab-sheet" key={sheet.index}>
                    <button
                      className="lab-sheet-image"
                      onClick={() => setViewer({ images: [sheet.image], index: 0 })}
                      aria-label={`查看拼图 ${sheet.index}`}
                    >
                      <img src={sheet.image} alt={`拼图 ${sheet.index}`} draggable={false} />
                      <span className="lab-sheet-zoom"><IconFull size={13} />大图</span>
                    </button>
                    <div className="lab-sheet-foot">
                      <span className="lab-sheet-name">拼图 {sheet.index}</span>
                      <span>{rangeLabel(sheet.from_s, sheet.to_s)}</span>
                      <span className="lab-sheet-times">{sheet.times.map((time) => mmss(time)).join(' · ')}</span>
                    </div>
                  </article>
                ))}
              </div>
            ) : (
              <div className="lab-empty">
                <div className="lab-empty-title">还没有拼图</div>
                <div className="lab-empty-sub">选好间隔后点「生成拼图」，这里会出现带时间戳的大图。</div>
              </div>
            )}
          </section>
        </div>

        <aside className="lab-side">
          <section className="lab-step-block lab-ai-block">
            <div className="lab-step-head">
              <span className="lab-step-number">03</span>
              <div>
                <div className="lab-step-title">让 AI 细看</div>
                <div className="lab-step-sub">每格都会被逐条描述，屏幕文字尽量原文抄录。</div>
              </div>
            </div>

            <div className="lab-focus-presets">
              {FOCUS_PRESETS.map(([label, value]) => (
                <button
                  key={label}
                  className={`lab-focus-chip ${focus === value ? 'active' : ''}`}
                  onClick={() => setFocus(focus === value ? '' : value)}
                >
                  {label}
                </button>
              ))}
            </div>
            <textarea
              className="lab-focus-input"
              value={focus}
              onChange={(event) => setFocus(event.target.value)}
              placeholder="也可以自己写观察重点，例如：重点看屏幕上的字、操作步骤、有没有广告…"
            />
            <button className="btn primary lab-observe" disabled={!lab || busy} onClick={startObserve}>
              {observing ? 'AI 正在细看…' : '让 AI 逐格细看'}
            </button>
            {job.running && job.kind === 'observe' ? (
              <div className="lab-progress">
                <div className="lab-progress-head">
                  <span>{job.message}</span>
                  <span>{job.pct}%</span>
                </div>
                <div className="lab-progress-track"><div style={{ width: `${job.pct}%` }} /></div>
              </div>
            ) : null}
            {!job.running && job.kind === 'observe' && job.error ? <div className="lab-inline-error">{job.error}</div> : null}
            {lab ? <div className="lab-ai-hint">约 {lab.sheet_count} 次视觉请求 · 预计 {Math.ceil(lab.sheet_count * 1.7)}k tok</div> : null}
            {!lab ? <div className="lab-ai-hint">先生成拼图，再让 AI 逐格看。</div> : null}
          </section>

          <section ref={findingsRef} className="lab-step-block lab-findings">
            <div className="lab-findings-head">
              <div>
                <div className="lab-step-title">逐格观察</div>
                <div className="lab-step-sub">时间轴、屏幕文字和画面事实都在这里。</div>
              </div>
              {observation ? <button className="btn sm ghost" onClick={copyObservation}>复制全文</button> : null}
            </div>

            {observation ? (
              <>
                <div className="lab-obs-meta">
                  <span>{observation.sheets.length} 张拼图</span>
                  <span>{observation.tokens} tok</span>
                  <span>{(observation.elapsed_ms / 1000).toFixed(1)}s</span>
                  {observation.failed ? <span className="lab-obs-failed">{observation.failed} 张失败</span> : null}
                  {lastFocus ? <span className="lab-obs-focus">重点：{lastFocus}</span> : null}
                </div>
                {observation.sheets.map((sheet) => (
                  <details
                    className="lab-obs-sheet"
                    key={sheet.index}
                    open={Boolean(openSheets[sheet.index])}
                    onToggle={(event) => {
                      const open = event.currentTarget.open;
                      setOpenSheets((current) => ({ ...current, [sheet.index]: open }));
                    }}
                  >
                    <summary>
                      <span className="lab-obs-index">拼图 {sheet.index}</span>
                      <span className="lab-obs-range">{rangeLabel(sheet.from_s, sheet.to_s)}</span>
                      <span className="lab-obs-tokens">{sheet.tokens} tok</span>
                    </summary>
                    {sheet.summary ? <p className="lab-obs-summary">{sheet.summary}</p> : null}
                    <div className="lab-obs-tiles">
                      {sheet.tiles.map((tile, tileIndex) => (
                        <div className="lab-obs-tile" key={`${sheet.index}-${tileIndex}`}>
                          <span className="lab-obs-time">{tile.at || mmss(sheet.times[tileIndex] || 0)}</span>
                          <div className="lab-obs-copy">
                            {(tile.scene || tile.action) ? (
                              <div className="lab-obs-line">{[tile.scene, tile.action].filter(Boolean).join(' · ')}</div>
                            ) : null}
                            {tile.onscreen_text?.length ? (
                              <div className="lab-obs-screen">
                                <span>屏幕</span>
                                {tile.onscreen_text.map((text, textIndex) => <em key={textIndex}>{text}</em>)}
                              </div>
                            ) : null}
                            {tile.details ? <div className="lab-obs-detail">{tile.details}</div> : null}
                          </div>
                        </div>
                      ))}
                    </div>
                    {sheet.flags?.length ? (
                      <div className="lab-obs-flags">
                        {sheet.flags.map((flag, flagIndex) => <span key={flagIndex}>{flag}</span>)}
                      </div>
                    ) : null}
                  </details>
                ))}
              </>
            ) : (
              <div className="lab-ai-empty">
                <div className="lab-ai-orbit"><i /><i /><i /></div>
                <div className="lab-empty-title">AI 还没开始看</div>
                <div className="lab-empty-sub">生成拼图后，选一个观察重点，AI 会按格子逐条读画面。</div>
              </div>
            )}
          </section>
        </aside>
      </div>

      {viewer ? (
        <ImageViewer
          images={viewer.images}
          index={viewer.index}
          onClose={() => setViewer(null)}
          onIndexChange={(index) => setViewer((current) => current ? { ...current, index } : current)}
        />
      ) : null}
    </div>
  );
}
