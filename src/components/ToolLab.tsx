import React, { useEffect, useRef, useState } from 'react';
import { call, formatCount } from '../api';
import { useApp } from '../context';
import type { FishToolRun, FishToolSpec } from '../types';

type ResultState = {
  loading?: boolean;
  run?: FishToolRun;
  error?: string;
};

const STATE: Record<string, { label: string; tone: string }> = {
  ok: { label: '完成', tone: 'ok' },
  skipped: { label: '跳过', tone: 'skipped' },
  failed: { label: '失败', tone: 'failed' },
};

const KIND: Record<string, string> = {
  bilibili: 'B 站接口',
  local: '本地计算',
  'deepseek-vision': 'DeepSeek 视觉',
};

function formatMs(ms: number): string {
  if (!ms) return '—';
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
}

function formatBytes(bytes: number): string {
  if (!bytes) return '';
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(2)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1_024) return `${(bytes / 1_024).toFixed(0)} KB`;
  return `${bytes} B`;
}

/// One tool card = a manual test panel. The shared target (BV + cid) keeps the
/// six runs comparable; every run goes through the same Rust step the agent
/// executes during a real watch.
function ToolCard({
  tool,
  bvid,
  cid,
  result,
  onRun,
}: {
  tool: FishToolSpec;
  bvid: string;
  cid: string;
  result?: ResultState;
  onRun: () => void;
}) {
  const state = result?.run ? STATE[result.run.state] || STATE.failed : null;
  return (
    <div className={`tool-card ${result?.run?.state || ''}`}>
      <div className="tool-card-head">
        <span className={`agent-dot tool ${result?.run?.state || ''}`} />
        <div className="tool-card-title">
          <div className="tool-name">{tool.label}</div>
          <div className="tool-id">
            {tool.id} · {KIND[tool.kind] || tool.kind} · 上限 {tool.timeout_s}s
          </div>
        </div>
        <div className="tool-flags">
          {tool.heavy ? <span className="agent-badge skipped">耗时</span> : null}
          {tool.needs_key ? <span className="agent-badge running">需要 Key</span> : null}
        </div>
        <button
          className="btn sm primary"
          disabled={!bvid.trim() || result?.loading}
          onClick={onRun}
        >
          {result?.loading ? '调用中…' : '调用'}
        </button>
      </div>
      <div className="tool-desc">{tool.description}</div>

      {result?.run ? (
        <div className="tool-result">
          <div className="tool-result-meta">
            <span className={`agent-badge ${state?.tone || 'failed'}`}>{state?.label || result.run.state}</span>
            <span>{formatMs(result.run.ms)}</span>
            {result.run.bytes ? <span>{formatBytes(result.run.bytes)}</span> : null}
            {result.run.tokens ? <span>{formatCount(result.run.tokens)} tok</span> : null}
            <span>{result.run.bvid} · cid {result.run.cid}</span>
            {result.run.state !== 'ok' && result.run.reason ? <span className="tool-reason">{result.run.reason}</span> : null}
          </div>
          {result.run.detail ? <div className="tool-result-detail">{result.run.detail}</div> : null}
          {result.run.preview ? <pre className="tool-preview">{result.run.preview}</pre> : null}
        </div>
      ) : null}
      {result?.error ? <div className="tool-error">{result.error}</div> : null}
      {!bvid.trim() ? <div className="tool-hint">先在上方填一个 BV 号。</div> : null}
    </div>
  );
}

export function ToolLab({ tools, seedBvid }: { tools: FishToolSpec[]; seedBvid: string }) {
  const { settings, toast } = useApp();
  const [bvid, setBvid] = useState(seedBvid);
  const [cid, setCid] = useState('');
  const [results, setResults] = useState<Record<string, ResultState>>({});
  const dirty = useRef(false);
  const playlist = settings?.playlist || [];
  const currentIndex = Number.isInteger(settings?.playlist_current) ? Number(settings?.playlist_current) : -1;
  const currentEntry = currentIndex >= 0 ? playlist[currentIndex] : playlist[playlist.length - 1];
  const currentBvid = String(currentEntry?.bvid || '');
  const currentCid = currentEntry?.cid ? String(currentEntry.cid) : '';

  const usePlayingVideo = () => {
    if (!currentBvid) {
      toast('播放队列里没有正在播放的视频', true);
      return;
    }
    dirty.current = true;
    setBvid(currentBvid);
    setCid(currentCid);
    setResults({});
  };

  useEffect(() => {
    if (!dirty.current && seedBvid && seedBvid !== bvid) setBvid(seedBvid);
  }, [seedBvid, bvid]);

  const run = async (tool: FishToolSpec) => {
    setResults((current) => ({ ...current, [tool.id]: { loading: true } }));
    try {
      const result = await call<FishToolRun>('fish.tool.run', {
        id: tool.id,
        bvid: bvid.trim(),
        cid: Number(cid) || 0,
      });
      setResults((current) => ({ ...current, [tool.id]: { run: result } }));
    } catch (reason: any) {
      setResults((current) => ({
        ...current,
        [tool.id]: { error: reason?.message || String(reason) },
      }));
    }
  };

  return (
    <div className="tool-lab">
      <div className="tool-target">
        <div className="tool-target-head">
          <div>
            <div className="panel-title">工具台</div>
          </div>
          <button
            className="btn sm"
            disabled={!currentBvid}
            title={currentBvid || '播放队列为空'}
            onClick={usePlayingVideo}
          >
            填入正在播放
          </button>
        </div>
        <div className="tool-target-fields">
          <label className="field">
            <span>BV 号</span>
            <input
              className="input"
              value={bvid}
              placeholder="BV1…"
              onChange={(event) => {
                dirty.current = true;
                setBvid(event.target.value);
              }}
            />
          </label>
          <label className="field">
            <span>cid（可选）</span>
            <input
              className="input"
              value={cid}
              placeholder="留空自动读取"
              onChange={(event) => setCid(event.target.value.replace(/[^\d]/g, ''))}
            />
          </label>
        </div>
      </div>

      <div className="tool-list">
        {tools.map((tool) => (
          <ToolCard
            key={tool.id}
            tool={tool}
            bvid={bvid}
            cid={cid}
            result={results[tool.id]}
            onRun={() => run(tool)}
          />
        ))}
      </div>
    </div>
  );
}
