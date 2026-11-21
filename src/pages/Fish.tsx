import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call, formatCount } from '../api';
import { AgentTrace } from '../components/AgentTrace';
import { TestLab } from '../components/TestLab';
import { useApp } from '../context';
import { recommendFromFolder } from '../favRecommend';
import type { FavFolder, FeedVideo, FishAgent, FishState, FishToolSpec } from '../types';
import { ErrorBox, PageHead, Spinner, Tabs, Toggle } from '../components/Common';

const TABS = ['看片', '试验台', '设置'];

/// Keep at most this many rows waiting/running; the auto-filler adds one when
/// the queue drops below it.
const AUTOFILL_LOW_WATER = 2;
/// After an empty source / failed pick, wait this long before trying again so
/// a dead feed does not hammer Bilibili every poll.
const AUTOFILL_RETRY_MS = 8_000;

/// 语音识别固定使用 tiny 模型：设置里只提供下载，其余参数保持默认值。
const ASR_MODEL = 'tiny';

const formatBytes = (value: number | undefined | null) => `${(Number(value || 0) / 1048576).toFixed(1)} MB`;

function fishStateLabel(value: any): string {
  if (typeof value !== 'string') return value?.Error || 'Idle';
  switch (value) {
    case 'Running': return '运行中';
    case 'Waiting': return '等待投喂';
    case 'Paused': return '已暂停';
    case 'NeedsKey': return '需要 API Key';
    case 'Idle': return '空闲';
    case 'Error': return '出错';
    default: return value;
  }
}

export default function Fish() {
  const { settings, saveSettings, toast } = useApp();
  const [tab, setTab] = useState(0);
  const [state, setState] = useState<FishState | null>(null);
  const [models, setModels] = useState<any>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [key, setKey] = useState('');
  const [instruction, setInstruction] = useState(settings.fish_instruction || '');
  const [tools, setTools] = useState<FishToolSpec[]>([]);
  const [folders, setFolders] = useState<FavFolder[]>([]);
  const [foldersLoading, setFoldersLoading] = useState(false);
  const autofillBusy = useRef(false);
  const autofillRetryAt = useRef(0);
  const autofillWarned = useRef('');

  const load = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      setState(await call<FishState>('fish.state'));
      setError('');
    } catch (reason: any) {
      if (!quiet) setError(reason?.message || String(reason));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  const refreshModels = useCallback(() => {
    call<any>('fish.models').then(setModels).catch(() => {});
  }, []);

  useEffect(() => {
    load();
    // A live Agent transcript should feel live: poll every second while the
    // worker runs, then back off to 3 s once it is idle.
    const running = state?.snapshot?.state === 'Running' || state?.snapshot?.state === 'Waiting';
    const timer = setInterval(() => load(true), running ? 1000 : 3000);
    return () => clearInterval(timer);
  }, [load, state?.snapshot?.state]);

  useEffect(() => {
    const onRefresh = () => load();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  // 设置页只需要 tiny 模型的下载状态；下载时加快轮询，其他时候放慢。
  useEffect(() => {
    if (tab !== 2) return;
    refreshModels();
    const timer = setInterval(refreshModels, models?.running ? 1200 : 5000);
    return () => clearInterval(timer);
  }, [tab, models?.running, refreshModels]);

  // The Agent and 试验台 tabs share one static catalog; fetch it the first time
  // either is opened (the engine also uses it to enumerate the watch steps).
  useEffect(() => {
    if ((tab !== 0 && tab !== 1) || tools.length) return;
    call<FishToolSpec[]>('fish.tools')
      .then((list) => setTools(Array.isArray(list) ? list : []))
      .catch((reason) => toast(reason.message, true));
  }, [tab, tools.length, toast]);

  const snapshot = state?.snapshot;
  const snapshotState = snapshot?.state;
  const autofillSource = settings.fish_autofill_source === 'favorites' ? 'favorites' : 'home';
  const autofillFolder = folders.find((row) => row.id === Number(settings.fish_autofill_folder)) || null;

  const loadFolders = useCallback(async () => {
    setFoldersLoading(true);
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
    } catch {
      setFolders([]);
    } finally {
      setFoldersLoading(false);
    }
  }, []);

  useEffect(() => {
    if (tab !== 0 || autofillSource !== 'favorites' || folders.length) return;
    loadFolders();
  }, [tab, autofillSource, folders.length, loadFolders]);

  // A source / folder change is an explicit "try again now"; clear the
  // back-off that an empty previous source installed.
  useEffect(() => {
    autofillRetryAt.current = 0;
    autofillWarned.current = '';
  }, [settings.fish_autofill_source, settings.fish_autofill_folder]);

  const action = async (method: string, params: Record<string, any> = {}) => {
    try {
      await call(method, params);
      await load(true);
      toast('操作成功');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const startAgent = async () => {
    try {
      await call('fish.start');
      await load(true);
      toast('大肥鱼开始运行');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const saveFish = async (patch: Record<string, any>) => {
    await saveSettings(patch as any);
    // The worker snapshots settings when it starts. Refresh it in place (a
    // running worker only replaces the session, it does not abort the video)
    // so the new auto-fill switch applies without a pause.
    if ('fish_autofill' in patch || 'fish_autofill_source' in patch) {
      if (snapshotState === 'Running' || snapshotState === 'Waiting') {
        call('fish.start').catch(() => {});
      }
    }
  };

  const pickAutofill = useCallback(async (): Promise<FeedVideo | null> => {
    if (autofillSource === 'favorites') {
      if (!autofillFolder) throw new Error('请先选择一个收藏夹');
      const pool = await recommendFromFolder(autofillFolder, 12);
      if (!pool.length) return null;
      const candidates = pool.slice();
      for (let index = candidates.length - 1; index > 0; index -= 1) {
        const swap = Math.floor(Math.random() * (index + 1));
        [candidates[index], candidates[swap]] = [candidates[swap], candidates[index]];
      }
      for (const item of candidates) {
        if (!item?.bvid) continue;
        const check = await call<{ watched: boolean; queued: boolean }>('fish.watched.check', { bvid: item.bvid });
        if (!check?.watched && !check?.queued) return item;
      }
      return null;
    }
    return call<FeedVideo | null>('fish.feed.next');
  }, [autofillSource, autofillFolder]);

  const fillQueue = useCallback(async () => {
    if (autofillBusy.current) return;
    if (Date.now() < autofillRetryAt.current) return;
    autofillBusy.current = true;
    try {
      const item = await pickAutofill();
      if (!item) {
        autofillRetryAt.current = Date.now() + AUTOFILL_RETRY_MS;
        if (autofillWarned.current !== 'empty') {
          autofillWarned.current = 'empty';
          toast('自动填充没有找到未看过的视频，稍后重试', true);
        }
        return;
      }
      const result = await call<{ queued: boolean; bvid: string }>('fish.enqueue.item', { item });
      if (result?.queued) {
        autofillWarned.current = '';
        autofillRetryAt.current = 0;
        await load(true);
      } else {
        autofillRetryAt.current = Date.now() + AUTOFILL_RETRY_MS;
      }
    } catch (reason: any) {
      autofillRetryAt.current = Date.now() + AUTOFILL_RETRY_MS;
      const message = reason?.message || String(reason);
      if (autofillWarned.current !== message) {
        autofillWarned.current = message;
        toast(`自动填充失败：${message}`, true);
      }
    } finally {
      autofillBusy.current = false;
    }
  }, [pickAutofill, load, toast]);

  useEffect(() => {
    if (!settings.fish_autofill) return;
    if (snapshotState !== 'Running' && snapshotState !== 'Waiting') return;
    if ((snapshot?.queued ?? 0) >= AUTOFILL_LOW_WATER) return;
    const kick = () => {
      if ((snapshot?.queued ?? 0) < AUTOFILL_LOW_WATER) void fillQueue();
    };
    const first = window.setTimeout(kick, 300);
    // The queue count may stay at 0 while a source is empty or offline. Keep
    // retrying on a timer; `fillQueue` owns the longer 8 s back-off.
    const timer = window.setInterval(kick, 2_000);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(timer);
    };
  }, [settings.fish_autofill, snapshotState, snapshot?.queued, fillQueue]);

  const agent = (snapshot?.agent || null) as FishAgent | null;
  const toolSeed = agent?.bvid || snapshot?.current?.[0] || state?.verdicts?.[0]?.bvid || '';
  const asrTiny = (models?.items || []).find((item: any) => item.model?.name === ASR_MODEL);
  const asrReady = asrTiny?.state === 'Installed';
  const asrDownloading = Boolean(models?.running);
  const asrLine = !models
    ? '正在读取模型状态…'
    : asrReady
      ? `已下载 · ${formatBytes(asrTiny?.model?.bytes)} · 语音识别固定使用此模型`
      : asrDownloading
        ? `下载中 ${asrTiny?.pct ?? 0}% · ${formatBytes(asrTiny?.have)} / ${formatBytes(asrTiny?.model?.bytes)}`
        : asrTiny?.state === 'Partial'
          ? `未完成 ${formatBytes(asrTiny?.have)} / ${formatBytes(asrTiny?.model?.bytes)} · 点击继续下载`
          : '未下载 · 语音识别固定使用 tiny 模型，其他参数保持默认';

  const downloadAsr = async () => {
    try {
      await call('fish.models.download', { name: ASR_MODEL });
      toast('已开始下载 tiny 模型');
      refreshModels();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const isLive = snapshotState === 'Running' || snapshotState === 'Waiting';

  return (
    <>
      <PageHead eyebrow="实验" title="养鱼" sub="AI 看片 · 固定观看流程">
        <Tabs tabs={TABS} active={tab} onChange={setTab} />
      </PageHead>

      {loading && !state ? <Spinner label="读取养鱼引擎…" /> : error ? <ErrorBox error={error} onRetry={() => load()} /> : state ? (
        <>
          {tab === 0 ? (
            <div className="agent-page">
              <section className="fish-top">
                <div className="fish-status-row">
                  <span className={`fish-state ${isLive ? 'on' : ''}`}>
                    <i />
                    {fishStateLabel(snapshot.state)}
                  </span>
                  <span className="fish-meta">
                    队列 {snapshot.queued} · 完成 {snapshot.done} · 错误 {snapshot.errors} · tokens {formatCount(snapshot.tokens_in + snapshot.tokens_out)}
                  </span>
                  <div className="fish-toolbar">
                    <button className="btn sm primary" onClick={startAgent}>开始 agent</button>
                    <button className="btn sm" onClick={() => action('fish.pause')}>暂停</button>
                    <button className="btn sm danger" onClick={() => action('fish.stop')}>停止</button>
                  </div>
                </div>
                {snapshot.current ? <div className="fish-current">正在看：{snapshot.current[1] || snapshot.current[0]}</div> : null}
                <div className="fish-toolbar fish-toolbar-secondary">
                  <button className="btn sm" onClick={() => action('fish.enqueue')}>投喂首页推荐</button>
                  <button className="btn sm" onClick={() => action('fish.retry')}>重试错误</button>
                  <button className="btn sm danger" onClick={() => { if (window.confirm('清空推荐流？历史结论会保留。')) action('fish.clear'); }}>清空推荐流</button>
                </div>
                <div className="fish-autofill">
                  <span className="fish-autofill-label">自动填充</span>
                  <Toggle
                    on={settings.fish_autofill}
                    onChange={(value) => saveFish({ fish_autofill: value })}
                  />
                  <select
                    className="select"
                    value={autofillSource}
                    onChange={(event) => saveFish({ fish_autofill_source: event.target.value })}
                    aria-label="自动填充来源"
                  >
                    <option value="home">首页视频流</option>
                    <option value="favorites">收藏夹推荐流</option>
                  </select>
                  {autofillSource === 'favorites' ? (
                    <select
                      className="select"
                      value={settings.fish_autofill_folder || 0}
                      onChange={(event) => saveFish({ fish_autofill_folder: Number(event.target.value) })}
                      aria-label="自动填充收藏夹"
                    >
                      <option value={0}>{foldersLoading ? '正在读取收藏夹…' : '选择收藏夹'}</option>
                      {folders.map((folder) => (
                        <option key={folder.id} value={folder.id}>
                          {folder.name}（{formatCount(folder.count)}）
                        </option>
                      ))}
                    </select>
                  ) : null}
                  <span className="fish-autofill-note">
                    随机挑一条数据库里没有的视频投喂；队列低于 {AUTOFILL_LOW_WATER} 条时补充。
                    {autofillSource === 'favorites' && !autofillFolder ? ' 需要先选择一个收藏夹。' : ''}
                  </span>
                  {isLive && settings.fish_autofill ? <span className="fish-autofill-state">运行中</span> : null}
                </div>
              </section>

              <section className="fish-section fish-agent-head">
                <div className="fish-agent-line">
                  <div className="agent-tool-strip">
                    <span className="agent-tool-strip-label">固定步骤</span>
                    {tools.map((tool) => {
                      const step = agent?.steps.find((row) => row.id === tool.id);
                      const tone = step ? step.state : 'idle';
                      return (
                        <span className={`agent-tool-chip ${tone}`} key={tool.id} title={`${tool.description} · 上限 ${tool.timeout_s}s`}>
                          <i />
                          {tool.label}
                          <em>{step ? step.meta || step.state : '未参与'}</em>
                        </span>
                      );
                    })}
                    {!tools.length ? <span className="agent-tool-strip-empty">正在读取工具清单…</span> : null}
                  </div>
                  <button className="btn sm ghost" onClick={() => setTab(1)}>试验台</button>
                </div>
              </section>

              <AgentTrace key={`full-${agent?.bvid || 'empty'}`} agent={agent} full />
            </div>
          ) : null}

          {tab === 1 ? <TestLab tools={tools} seedBvid={toolSeed} /> : null}

          {tab === 2 ? (
            <div className="fish-settings">
              <div className="field">
                <label>DeepSeek API Key</label>
                <div style={{ display: 'flex', gap: 8 }}>
                  <input className="input" style={{ flex: 1 }} value={key} type="password" placeholder={state.settings.deepseek_key_set ? '已设置（留空保持）' : 'sk-…'} onChange={(event) => setKey(event.target.value)} />
                  <button className="btn sm primary" disabled={!key.trim()} onClick={async () => { await saveFish({ deepseek_key: key.trim() }); setKey(''); toast('Key 已保存'); load(true); }}>保存</button>
                </div>
              </div>
              <div className="field">
                <label>观看指令</label>
                <textarea className="textarea" value={instruction} onChange={(event) => setInstruction(event.target.value)} />
                <div style={{ display: 'flex', gap: 8 }}>
                  <button className="btn sm" onClick={async () => { await saveFish({ fish_instruction: instruction }); toast('已保存'); }}>保存指令</button>
                  {(state.presets || []).map((preset: any) => (
                    <button key={preset.id} className="chip small" title={preset.tagline} onClick={async () => { setInstruction(preset.body); await saveFish({ fish_instruction: preset.body }); toast(`已切换到 ${preset.name}`); }}>{preset.name}</button>
                  ))}
                </div>
              </div>
              <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 14 }}>
                <div className="field"><label>模型</label>
                  <select className="select" value={settings.fish_model} onChange={(event) => saveFish({ fish_model: event.target.value })}>
                    <option value="deepseek-flash">deepseek-flash</option>
                    <option value="deepseek-reasoner">deepseek-reasoner</option>
                  </select>
                </div>
                <div className="field"><label>预算（关闭自动填充时）</label><input className="input" type="number" min={1} max={200} value={settings.fish_budget_videos} onChange={(event) => saveFish({ fish_budget_videos: Number(event.target.value) })} /></div>
                <div className="field"><label>间隔（毫秒）</label><input className="input" type="number" min={200} max={10000} value={settings.fish_interval_ms} onChange={(event) => saveFish({ fish_interval_ms: Number(event.target.value) })} /></div>
                <div className="field"><label>自动填充来源</label><div className="fish-muted" style={{ paddingTop: 9 }}>在「看片」页顶部选择首页视频流或收藏夹推荐流</div></div>
              </div>
              <div className="settings-row"><div className="label"><div className="name">启用养鱼</div><div className="desc">允许后台定时喂食</div></div><Toggle on={settings.fish_enabled} onChange={(value) => saveFish({ fish_enabled: value })} /></div>
              <div className="settings-row"><div className="label"><div className="name">自动喂食</div><div className="desc">{settings.fish_feed_time || '20:00'} 开始</div></div><Toggle on={settings.fish_auto_feed} onChange={(value) => saveFish({ fish_auto_feed: value })} /></div>
              <div className="settings-row"><div className="label"><div className="name">自动看片截图间隔</div><div className="desc">自动 = 约 30 张 / 片；画面实验室可单独选择</div></div>
                <div className="control">
                  <select className="select" value={(settings as any).fish_frame_interval_s ?? 0} onChange={(event) => saveFish({ fish_frame_interval_s: Number(event.target.value) })}>
                    <option value={0}>自动</option>
                    <option value={5}>5 秒</option>
                    <option value={10}>10 秒</option>
                    <option value={15}>15 秒</option>
                    <option value={30}>30 秒</option>
                    <option value={60}>1 分钟</option>
                    <option value={120}>2 分钟</option>
                    <option value={300}>5 分钟</option>
                  </select>
                </div>
              </div>
              <div className="panel-title" style={{ marginTop: 16 }}>步骤</div>
              {[
                ['fish_step_meta', '元数据'],
                ['fish_step_subtitle', '字幕'],
                ['fish_step_outline', '大纲'],
                ['fish_step_comments', '评论'],
                ['fish_step_asr', 'ASR'],
                ['fish_step_frames', '画面抽帧'],
              ].map(([field, label]) => (
                <div className="settings-row" key={field}>
                  <div className="label"><div className="name">{label}</div></div>
                  <Toggle on={Boolean((settings as any)[field])} onChange={(value) => saveFish({ [field]: value })} />
                </div>
              ))}
              <div className="panel-title" style={{ marginTop: 16 }}>语音识别</div>
              <div className="settings-row">
                <div className="label">
                  <div className="name">Whisper tiny</div>
                  <div className="desc">{asrLine}</div>
                </div>
                <div className="control" style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                  {asrDownloading && asrTiny?.pct != null ? (
                    <div className="bar" style={{ width: 120, marginTop: 0 }}><div style={{ width: `${asrTiny.pct}%` }} /></div>
                  ) : null}
                  <button className="btn sm primary" disabled={asrReady || asrDownloading} onClick={downloadAsr}>
                    {asrReady ? '已下载' : asrDownloading ? '下载中' : asrTiny?.state === 'Partial' ? '继续下载' : '下载模型'}
                  </button>
                </div>
              </div>
            </div>
          ) : null}
        </>
      ) : null}
    </>
  );
}
