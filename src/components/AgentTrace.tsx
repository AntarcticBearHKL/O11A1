import React, { useEffect, useRef, useState } from 'react';
import { CoverImage } from './CoverImage';
import { formatCount, formatDuration } from '../api';
import type { FishAgent, FishAgentReply, FishAgentStep } from '../types';

/// One tool / thinking row inside the process group.
function AgentStepRow({
  step,
  open,
  onToggle,
}: {
  step: FishAgentStep;
  open: boolean;
  onToggle: () => void;
}) {
  const state = STATE_LABEL[step.state] || STATE_LABEL.running;
  return (
    <div className={`agent-card agent-step ${step.kind} ${step.state} ${open ? 'open' : ''}`}>
      <button className="agent-card-head" type="button" onClick={onToggle} aria-expanded={open}>
        <span className={`agent-dot ${step.kind} ${step.state}`} />
        <span className="agent-title">{step.title}</span>
        <span className={`agent-badge ${state.tone}`}>{state.label}</span>
        <span className="agent-preview">{step.detail}</span>
        <span className="agent-tail">
          {step.ms ? <span className="agent-timing">{formatMs(step.ms)}</span> : null}
          {step.bytes ? <span className="agent-usage">{formatBytes(step.bytes)}</span> : null}
          {step.tokens ? <span className="agent-usage">{formatCount(step.tokens)} tok</span> : null}
        </span>
        <Chevron open={open} />
      </button>
      {open ? (
        <div className="agent-card-body">
          <div className="agent-detail-row"><span>说明</span><span>{step.detail || '—'}</span></div>
          <div className="agent-detail-row"><span>状态</span><span>{state.label}</span></div>
          <div className="agent-detail-row">
            <span>耗时 / 数据</span>
            <span>{[step.ms ? formatMs(step.ms) : '', step.bytes ? formatBytes(step.bytes) : ''].filter(Boolean).join(' · ') || '—'}</span>
          </div>
          {step.tokens ? (
            <div className="agent-detail-row"><span>模型 tokens</span><span>{formatCount(step.tokens)}</span></div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/// The answer card: score, verdict, reasons, tags and the quoted line.
function AgentReplyCard({ reply, model }: { reply: FishAgentReply; model: string }) {
  return (
    <div className="agent-card reply">
      <div className="agent-card-head static">
        <span className="agent-dot reply" />
        <span className="agent-title">回复</span>
        <span className="agent-meta">{model || 'DeepSeek'}</span>
        <span className="agent-tail">
          {reply.ms ? <span className="agent-timing">{formatMs(reply.ms)}</span> : null}
          <span className="agent-usage">{formatCount(reply.tokens_in + reply.tokens_out)} tok</span>
        </span>
      </div>
      <div className="agent-reply-body">
        <div className="agent-verdict">
          <span className="agent-score">{Math.round(reply.score)}</span>
          <div className="agent-verdict-copy">
            <div className="agent-verdict-line">{reply.verdict || '也许'}<span> / 10</span></div>
            <div className="agent-one-line">{reply.one_line}</div>
          </div>
        </div>
        {reply.reasons.length ? (
          <ul className="agent-reasons">
            {reply.reasons.map((reason, index) => <li key={`${index}-${reason}`}>{reason}</li>)}
          </ul>
        ) : null}
        {reply.quote ? <div className="agent-quote">“{reply.quote}”</div> : null}
        {reply.tags.length ? (
          <div className="agent-tags">
            {reply.tags.map((tag) => <span className="chip small" key={tag}>{tag}</span>)}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function Chevron({ open }: { open: boolean }) {
  return (
    <span className={`agent-chevron ${open ? 'open' : ''}`}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 9l6 6 6-6" /></svg>
    </span>
  );
}

const STATE_LABEL: Record<string, { label: string; tone: string }> = {
  running: { label: '运行中', tone: 'running' },
  ok: { label: '完成', tone: 'ok' },
  skipped: { label: '跳过', tone: 'skipped' },
  failed: { label: '失败', tone: 'failed' },
  done: { label: '已完成', tone: 'ok' },
  cancelled: { label: '已取消', tone: 'skipped' },
  error: { label: '出错', tone: 'failed' },
};

function formatMs(ms: number): string {
  if (!ms) return '';
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.round(ms)}ms`;
}

function formatBytes(bytes: number): string {
  if (!bytes) return '';
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(2)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1_024) return `${(bytes / 1_024).toFixed(0)} KB`;
  return `${bytes} B`;
}

/// 养鱼 · Agent 看片过程.
///
/// The chat surface mirrors the yah composer logic: a prompt card, one
/// collapsible process group whose rows are the watch-plan tools plus the
/// DeepSeek thinking row, and the final answer card. Rows expand in place,
/// the thread follows the tail only while the reader stays at the bottom, and
/// a run that ends without a verdict closes into a red (error) or quiet
/// (cancelled) card instead of leaving a spinner behind.
export function AgentTrace({ agent, full = false }: { agent: FishAgent | null; full?: boolean }) {
  const [processOpen, setProcessOpen] = useState(true);
  const [expanded, setExpanded] = useState<string[]>([]);
  const threadRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const state = agent ? STATE_LABEL[agent.state] || STATE_LABEL.running : null;
  const steps = agent?.steps || [];
  const failed = steps.filter((step) => step.state === 'failed').length;
  const skipped = steps.filter((step) => step.state === 'skipped').length;

  useEffect(() => {
    if (!stick.current) return;
    const node = threadRef.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [agent?.updated_at, steps.length, agent?.state, processOpen, expanded.length]);

  const toggleStep = (id: string) => {
    setExpanded((current) => current.includes(id) ? current.filter((row) => row !== id) : [...current, id]);
  };

  return (
    <div className={`agent-chat ${full ? 'full' : ''}`}>
      <div className="agent-head">
        <div className="agent-head-main">
          <span className={`agent-live ${agent?.state || 'idle'}`} />
          <div>
            <div className="agent-head-title">Agent</div>
            <div className="agent-head-sub">
              {agent ? `DeepSeek 看片 · ${state?.label || agent.state}` : 'DeepSeek 看片 · 等待投喂'}
            </div>
          </div>
        </div>
        <div className="agent-head-stats">
          {agent?.model ? <span className="chip small">{agent.model}</span> : null}
          {agent && agent.tokens_in + agent.tokens_out > 0 ? (
            <span className="agent-head-tokens">{formatCount(agent.tokens_in + agent.tokens_out)} tok</span>
          ) : null}
          {state ? <span className={`agent-state ${state.tone}`}>{state.label}</span> : null}
        </div>
      </div>

      <div
        className="agent-thread"
        ref={threadRef}
        onScroll={() => {
          const node = threadRef.current;
          if (!node) return;
          stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
        }}
      >
        {!agent ? (
          <div className="agent-empty">
            <div className="agent-empty-title">还没有开始看片</div>
            <div className="agent-empty-hint">点「开始」或「投喂首页推荐」，这里会实时显示 DeepSeek 的看片过程。</div>
          </div>
        ) : (
          <>
            <div className="agent-card prompt">
              <div className="agent-card-head static">
                <span className="agent-dot prompt" />
                <span className="agent-title">投喂</span>
                <span className="agent-meta">你</span>
                <span className="agent-tail">
                  <span className="agent-timing">{formatDuration(agent.duration_s)}</span>
                </span>
              </div>
              <div className="agent-prompt-body">
                <CoverImage src={agent.cover} alt="" eager className="agent-cover" />
                <div className="agent-prompt-copy">
                  <div className="agent-video-title">{agent.title || agent.bvid}</div>
                  <div className="agent-video-sub">{[agent.up_name, agent.bvid].filter(Boolean).join(' · ')}</div>
                  {agent.instruction ? <div className="agent-instruction">“{agent.instruction}”</div> : null}
                </div>
              </div>
            </div>

            {steps.length ? (
              <div className={`agent-process ${processOpen ? '' : 'collapsed'}`}>
                <button
                  className="agent-process-head"
                  type="button"
                  aria-expanded={processOpen}
                  onClick={() => setProcessOpen((open) => !open)}
                >
                  <span className={`agent-process-dot ${agent.state === 'running' ? 'live' : ''}`} />
                  <span className="agent-process-label">过程</span>
                  <span className="agent-process-count">{steps.length}</span>
                  {failed ? <span className="agent-process-error"><i />{failed} 失败</span> : null}
                  {!failed && skipped ? <span className="agent-process-skip">{skipped} 跳过</span> : null}
                  <span className="agent-process-note">{agent.note}</span>
                  <Chevron open={processOpen} />
                </button>
                {processOpen ? (
                  <div className="agent-process-body">
                    {steps.map((step) => (
                      <AgentStepRow
                        key={step.id}
                        step={step}
                        open={expanded.includes(step.id)}
                        onToggle={() => toggleStep(step.id)}
                      />
                    ))}
                  </div>
                ) : null}
              </div>
            ) : null}

            {agent.reply ? (
              <AgentReplyCard reply={agent.reply} model={agent.model} />
            ) : agent.state === 'running' ? (
              <div className="agent-waiting">
                <span className="agent-dot thinking running" />
                <span className="agent-waiting-text">{agent.note || '正在思考…'}</span>
                <span className="agent-cursor" />
              </div>
            ) : (
              <div className={`agent-card ${agent.state === 'error' ? 'error' : 'cancelled'}`}>
                <div className="agent-card-head static">
                  <span className={`agent-dot ${agent.state === 'error' ? 'failed' : 'skipped'}`} />
                  <span className="agent-title">{agent.state === 'error' ? '出错' : '已取消'}</span>
                  <span className="agent-tail">
                    <span className={`agent-badge ${agent.state === 'error' ? 'failed' : 'skipped'}`}>
                      {agent.state === 'error' ? '失败' : '取消'}
                    </span>
                  </span>
                </div>
                <div className="agent-card-body"><div className="agent-detail-row"><span>说明</span><span>{agent.note || '—'}</span></div></div>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
