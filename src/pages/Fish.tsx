import React, { useCallback, useEffect, useState } from 'react';
import { call } from '../api';
import { AgentPanel } from '../agent/AgentPanel';
import { TestLab } from '../components/TestLab';
import { useApp } from '../context';
import type { FishAgent, FishState, FishToolSpec } from '../types';
import { ErrorBox, PageHead, Spinner, Tabs, Toggle } from '../components/Common';

const TABS = ['Agent', '试验台', '设置'];

/// 语音识别固定使用 tiny 模型：设置里只提供下载，其余参数保持默认值。
const ASR_MODEL = 'tiny';

const formatBytes = (value: number | undefined | null) => `${(Number(value || 0) / 1048576).toFixed(1)} MB`;

export default function Fish() {
  const { settings, saveSettings, toast } = useApp();
  const [tab, setTab] = useState(0);
  const [state, setState] = useState<FishState | null>(null);
  const [models, setModels] = useState<any>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [key, setKey] = useState('');
  const [chatText, setChatText] = useState('');
  const [sending, setSending] = useState(false);
  const [tools, setTools] = useState<FishToolSpec[]>([]);

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
    // A live Agent transcript should feel live: poll every second while a
    // chat turn runs, then back off to 3 s once it is idle.
    const agentState = state?.snapshot?.agent?.state;
    const running = state?.snapshot?.chat_busy
      || agentState === 'running'
      || state?.snapshot?.state === 'Running'
      || state?.snapshot?.state === 'Waiting';
    const timer = setInterval(() => load(true), running ? 1000 : 3000);
    return () => clearInterval(timer);
  }, [load, state?.snapshot?.chat_busy, state?.snapshot?.agent?.state, state?.snapshot?.state]);

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

  // 工具台 uses the old fixed watch-step catalog.
  useEffect(() => {
    if (tab !== 1 || tools.length) return;
    call<FishToolSpec[]>('fish.tools')
      .then((list) => setTools(Array.isArray(list) ? list : []))
      .catch((reason) => toast(reason.message, true));
  }, [tab, tools.length, toast]);

  const snapshot = state?.snapshot;
  const snapshotState = snapshot?.state;

  const sendChat = async () => {
    const text = chatText.trim();
    if (!text || sending) return;
    setSending(true);
    try {
      await call('fish.agent.chat', { text });
      setChatText('');
      await load(true);
    } catch (reason: any) {
      toast(reason.message || String(reason), true);
    } finally {
      setSending(false);
    }
  };

  const resetChat = async () => {
    try {
      await call('fish.agent.reset');
      setChatText('');
      await load(true);
    } catch (reason: any) { toast(reason.message || String(reason), true); }
  };

  const cancelChat = async () => {
    try {
      await call('fish.agent.cancel');
      await load(true);
    } catch (reason: any) { toast(reason.message || String(reason), true); }
  };

  const saveFish = async (patch: Record<string, any>) => {
    await saveSettings(patch as any);
  };

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

  const stateError = snapshotState && typeof snapshotState === 'object'
    ? String((snapshotState as any).Error || '')
    : '';
  const agentState = agent?.state || '';
  const agentBusy = Boolean(snapshot?.chat_busy || agentState === 'running');
  const errorText = stateError || (agentState === 'error' ? agent?.note || '' : '');

  return (
    <>
      <PageHead eyebrow="实验" title="Agent" sub="AI 看片 · 对话" className="fish-page-head">
        <Tabs tabs={TABS} active={tab} onChange={setTab} />
        {tab === 0 ? (
          <button
            className="btn sm fish-new"
            disabled={agentBusy || !agent?.steps?.length}
            onClick={resetChat}
          >
            新对话
          </button>
        ) : null}
      </PageHead>

      {loading && !state ? <Spinner label="读取 Agent…" /> : error ? <ErrorBox error={error} onRetry={() => load()} /> : state ? (
        <>
          {tab === 0 ? (
            <AgentPanel
              agent={agent}
              errorText={errorText}
              parked={false}
              onCopyError={() => navigator.clipboard.writeText(errorText).then(() => toast('错误已复制')).catch(() => toast('复制失败', true))}
            >
              <div className="agent-composer">
                <textarea
                  className="agent-composer-input"
                  value={chatText}
                  rows={2}
                  placeholder={agentBusy ? 'Agent 正在回复…' : '跟 Agent 说你想看什么…（Enter 发送，Shift+Enter 换行）'}
                  onChange={(event) => setChatText(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key !== 'Enter' || event.shiftKey) return;
                    // Enter while an IME is composing commits the Chinese
                    // candidate, it must not also send the message.
                    if (event.nativeEvent.isComposing) return;
                    event.preventDefault();
                    if (!agentBusy) void sendChat();
                  }}
                />
                {agentBusy ? (
                  <button className="btn sm" onClick={cancelChat}>停止</button>
                ) : (
                  <button className="btn sm primary" disabled={!chatText.trim() || sending} onClick={sendChat}>发送</button>
                )}
              </div>
            </AgentPanel>
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
              <div className="field"><label>模型</label>
                <select className="select" value={settings.fish_model} onChange={(event) => saveFish({ fish_model: event.target.value })}>
                  <option value="deepseek-flash">deepseek-flash</option>
                  <option value="deepseek-reasoner">deepseek-reasoner</option>
                </select>
              </div>
              <div className="settings-row">
                <div className="label"><div className="name">Agent 模式</div><div className="desc">只在对话里工作；不会自动刷首页或自动看片</div></div>
              </div>
              <div className="settings-row"><div className="label"><div className="name">画面实验室抽帧间隔</div><div className="desc">仅手动画面实验室/旧流程使用；Agent 会自己选 1/5/10/20/30/60 秒</div></div>
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
