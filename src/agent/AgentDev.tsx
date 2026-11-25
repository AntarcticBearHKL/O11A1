import React, { useCallback, useEffect, useRef, useState } from 'react';
import { call } from '../api';
import { AgentPanel } from './AgentPanel';
import { ErrorBox, Spinner } from '../components/Common';
import { IconClose, IconMax, IconMin, IconRefresh } from '../components/Icons';
import { useToast } from '../hooks';
import type { FishAgent, FishState } from '../types';

function applyTheme(settings: any) {
  const dark = settings?.theme === 2;
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  const alpha = Math.max(0.58, 0.62 + (1 - Number(settings?.glass_transparency || 0) / 100) * 0.36);
  const rgb = dark ? '38, 40, 46' : '255, 252, 246';
  document.documentElement.style.setProperty('--card', `rgba(${rgb}, ${alpha})`);
  document.documentElement.style.setProperty('--glass-blur', `${Math.max(0, Number(settings?.glass_blur || 0)) * 0.18}px`);
  document.documentElement.style.setProperty('--wallpaper-blur', `${Math.max(0, Number(settings?.backdrop_blur || 0))}px`);
}

function stateLabel(value: any): string {
  if (typeof value === 'string') return value;
  if (value && typeof value === 'object') return Object.keys(value)[0] || '';
  return '';
}

function stateError(value: any): string {
  if (!value || typeof value !== 'object') return '';
  return String(value.Error || '');
}

/// Standalone credential-free Agent view used by `agent.html`.
///
/// This is the same AgentPanel and AgentTrace the main Fish page renders. It
/// talks to the running sidecar through the normal preload bridge, so a Vite
/// edit hot-reloads the transcript without rebuilding the renderer or packaging
/// the app.
export default function AgentDev() {
  const { toast, show } = useToast();
  const [state, setState] = useState<FishState | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [connected, setConnected] = useState(Boolean((window as any).o11a1?.call));
  const themeApplied = useRef(false);

  const load = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      setState(await call<FishState>('fish.state'));
      setError('');
      setConnected(true);
      if (!themeApplied.current) {
        themeApplied.current = true;
        call<any>('settings.get')
          .then(applyTheme)
          .catch(() => { themeApplied.current = false; });
      }
    } catch (reason: any) {
      if (!quiet) setError(reason?.message || String(reason));
      setConnected(false);
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    const running = state?.snapshot?.state === 'Running' || state?.snapshot?.state === 'Waiting';
    const timer = setInterval(() => load(true), running ? 1000 : 3000);
    return () => clearInterval(timer);
  }, [load, state?.snapshot?.state]);

  const snapshot = state?.snapshot;
  const agent = (snapshot?.agent || null) as FishAgent | null;
  const name = stateLabel(snapshot?.state);
  const workerAlive = Boolean(snapshot?.worker_alive);
  const isLive = workerAlive && (name === 'Running' || name === 'Waiting');
  const agentWaiting = Boolean(snapshot?.agent_waiting);
  const errorText = stateError(snapshot?.state) || (agent?.state === 'error' ? agent?.note || '' : '');

  const next = async () => {
    try {
      await call('fish.agent.step');
      await load(true);
      show('已推进一步');
    } catch (reason: any) {
      show(reason?.message || String(reason), true);
    }
  };

  return (
    <div className="app agent-dev-app">
      <div className="agent-dev-shell">
        <header className="agent-dev-head">
          <div className="agent-dev-copy">
            <div className="agent-dev-eyebrow">独立调试 · Agent</div>
            <div className="agent-dev-title">Agent</div>
            <div className="agent-dev-sub">
              同一个 AgentPanel / AgentTrace，Vite 热更新，不需要重编主体。
              {' '}
              <span className={`agent-dev-connection ${connected ? 'ok' : 'bad'}`}>
                {connected ? '引擎已连接' : '未连接引擎'}
              </span>
            </div>
          </div>

          <div className="agent-dev-actions no-drag">
            <span className={`fish-state ${isLive ? 'on' : ''}`}>
              <i />
              {name || '空闲'}
            </span>
            <button className="btn sm" onClick={() => load()} disabled={loading}>
              <IconRefresh size={13} />
              刷新
            </button>
            <button
              className="btn sm primary"
              disabled={isLive && !agentWaiting}
              onClick={next}
              title={isLive && !agentWaiting ? '当前这一轮还在运行' : '执行下一次 DeepSeek 请求'}
            >
              下一步
            </button>
            <div className="window-controls">
              <button className="icon-btn" title="最小化" onClick={() => (window as any).o11a1?.window?.minimize?.()}>
                <IconMin />
              </button>
              <button className="icon-btn" title="最大化" onClick={() => (window as any).o11a1?.window?.maximize?.()}>
                <IconMax />
              </button>
              <button className="icon-btn close" title="退出调试" onClick={() => (window as any).o11a1?.window?.quit?.()}>
                <IconClose />
              </button>
            </div>
          </div>
        </header>

        <div className="agent-dev-meta">
          <span>状态 {name || '-'}</span>
          <span>Worker {workerAlive ? '在线' : '离线'}</span>
          <span>等待下一步 {agentWaiting ? '是' : '否'}</span>
          <span>视频 {agent?.bvid || '-'}</span>
          <span>模型 {agent?.model || '-'}</span>
          <span>Tokens {Number(agent?.tokens_in || 0) + Number(agent?.tokens_out || 0)}</span>
        </div>

        <div className="agent-dev-body">
          {loading && !state ? <Spinner label="读取 Agent…" /> : error ? (
            <ErrorBox error={error} onRetry={() => load()} />
          ) : state ? (
            <AgentPanel
              agent={agent}
              errorText={errorText}
              parked={agentWaiting}
              onCopyError={() => navigator.clipboard.writeText(errorText).then(() => show('错误已复制')).catch(() => show('复制失败', true))}
            />
          ) : (
            <div className="agent-dev-empty">
              未连接到引擎。请用 <code>npm run agent:dev</code> 启动 Electron 调试窗口。
            </div>
          )}
        </div>
      </div>

      {toast ? <div className={`toast ${toast.error ? 'error' : ''}`}>{toast.text}</div> : null}
    </div>
  );
}
