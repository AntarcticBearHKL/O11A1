import React, { useEffect, useRef, useState } from 'react';
import { CoverImage } from './CoverImage';
import { JsonView } from './JsonView';
import { formatCount, formatDuration } from '../api';
import type { FishAgent, FishAgentReply, FishAgentStep } from '../types';

/// One tool / thinking row inside the process group. Expanding it prints the
/// exact request arguments and the result the tool produced, not just a
/// one-line summary.
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
  const thinking = step.kind === 'thinking';
  return (
    <div className={`agent-card agent-step ${step.kind} ${step.state} ${open ? 'open' : ''}`}>
      <button className="agent-card-head" type="button" onClick={onToggle} aria-expanded={open}>
        <span className={`agent-dot ${step.kind} ${step.state}`} />
        <span className="agent-title">{step.title}</span>
        <span className={`agent-badge ${state.tone}`}>{state.label}</span>
        <span className="agent-meta">{thinking ? step.meta : step.id}</span>
        <span className="agent-preview">{step.detail}</span>
        <span className="agent-tail">
          {step.ms ? <span className="agent-timing">{formatMs(step.ms)}</span> : null}
          {step.bytes ? <span className="agent-usage">{formatBytes(step.bytes)}</span> : null}
          {step.tokens ? <span className="agent-usage">{formatCount(step.tokens)} tok</span> : null}
        </span>
        <Chevron open={open} />
      </button>
      {open ? (
        <div className="agent-card-body agent-tool-body">
          <div className="agent-io">
            <div className="agent-io-label">{thinking ? '请求体 · request' : '调用参数'}</div>
            <JsonView text={step.arguments} empty="(没有记录参数)" />
          </div>
          <div className="agent-io">
            <div className="agent-io-label">{thinking ? '响应体 · response' : '返回结果'}</div>
            {step.output ? (
              <JsonView text={step.output} />
            ) : (
              <div className="agent-io-wait">
                {step.state === 'running' ? '等待工具返回…' : '(空)'}
              </div>
            )}
          </div>
          <div className="agent-detail-row"><span>说明</span><span>{step.detail || '—'}</span></div>
          <div className="agent-detail-row"><span>状态</span><span>{state.label}</span></div>
          <div className="agent-detail-row">
            <span>耗时 / 数据</span>
            <span>
              {[step.ms ? formatMs(step.ms) : '', step.bytes ? formatBytes(step.bytes) : '']
                .filter(Boolean)
                .join(' · ') || '—'}
            </span>
          </div>
          {step.tokens ? (
            <div className="agent-detail-row"><span>模型 tokens</span><span>{formatCount(step.tokens)}</span></div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/// System / user prompt card. The text is kept verbatim; the scroll box exists
/// because a materials prompt can be several thousand characters long.
function PromptDoc({
  title,
  meta,
  tone,
  text,
  open,
  onToggle,
}: {
  title: string;
  meta: string;
  tone: string;
  text: string;
  open: boolean;
  onToggle: () => void;
}) {
  return (
    <div className={`agent-card prompt-doc ${tone} ${open ? 'open' : ''}`}>
      <button className="agent-card-head" type="button" onClick={onToggle} aria-expanded={open}>
        <span className={`agent-dot ${tone}`} />
        <span className="agent-title">{title}</span>
        <span className="agent-meta">{meta}</span>
        <span className="agent-preview">{previewLine(text)}</span>
        <span className="agent-tail">
          <span className="agent-usage">{text.length} 字符</span>
        </span>
        <Chevron open={open} />
      </button>
      {open ? (
        <div className="agent-prompt-doc">
          <pre>{text || '（这次运行没有保存提示词）'}</pre>
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
  waiting: { label: '等待投喂', tone: 'running' },
  ok: { label: '完成', tone: 'ok' },
  skipped: { label: '跳过', tone: 'skipped' },
  failed: { label: '失败', tone: 'failed' },
  done: { label: '已完成', tone: 'ok' },
  cancelled: { label: '已取消', tone: 'skipped' },
  error: { label: '出错', tone: 'failed' },
};

function previewLine(text: string): string {
  for (const raw of String(text || '').replace(/\r\n/g, '\n').split('\n')) {
    const line = raw.trim();
    if (!line) continue;
    return line.length > 90 ? `${line.slice(0, 89)}…` : line;
  }
  return '';
}

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
/// The chat surface mirrors the yah transcript: the feed card, the exact
/// system/user prompts, one collapsible process group whose tool rows expose
/// arguments and results, the DeepSeek request/response and the final answer.
/// A prompt card or a tool row can be opened independently; the thread follows
/// the tail only while the reader stays at the bottom.
export function AgentTrace({ agent, full = false }: { agent: FishAgent | null; full?: boolean }) {
  const [processOpen, setProcessOpen] = useState(true);
  const [expanded, setExpanded] = useState<string[]>([]);
  const [systemOpen, setSystemOpen] = useState(true);
  const [userOpen, setUserOpen] = useState(true);
  const threadRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const state = agent ? STATE_LABEL[agent.state] || STATE_LABEL.running : null;
  const steps = agent?.steps || [];
  const failed = steps.filter((step) => step.state === 'failed').length;
  const skipped = steps.filter((step) => step.state === 'skipped').length;
  // A cheap signature that changes when a row's state/detail moves, so the
  // tail keeps following even when several updates land in the same second.
  const signature = steps.map((step) => `${step.id}:${step.state}:${step.detail}:${step.ms}`).join('|');

  useEffect(() => {
    if (!stick.current) return;
    const node = threadRef.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [agent?.updated_at, steps.length, agent?.state, signature]);

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
            <div className="agent-empty-hint">点「开始 agent」，自动填充器会持续投喂未看过的视频，这里实时显示完整提示词、工具调用和 DeepSeek 判定。</div>
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

            {agent.system_prompt || agent.user_prompt ? (
              <>
                <PromptDoc
                  title="系统提示词"
                  meta="system"
                  tone="system"
                  text={agent.system_prompt}
                  open={systemOpen}
                  onToggle={() => setSystemOpen((open) => !open)}
                />
                <PromptDoc
                  title="用户提示词"
                  meta="user · 材料 + 指令"
                  tone="user"
                  text={agent.user_prompt}
                  open={userOpen}
                  onToggle={() => setUserOpen((open) => !open)}
                />
              </>
            ) : null}

            {steps.length ? (
              <div className={`agent-process ${processOpen ? '' : 'collapsed'}`}>
                <button
                  className="agent-process-head"
                  type="button"
                  aria-expanded={processOpen}
                  onClick={() => setProcessOpen((open) => !open)}
                >
                  <span className={`agent-process-dot ${agent.state === 'running' || agent.state === 'waiting' ? 'live' : ''}`} />
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
            ) : agent.state === 'running' || agent.state === 'waiting' ? (
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
