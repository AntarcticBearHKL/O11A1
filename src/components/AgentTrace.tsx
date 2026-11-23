import React, { useEffect, useRef, useState } from 'react';
import { call, formatCount, formatDuration } from '../api';
import { CoverImage } from './CoverImage';
import { JsonView } from './JsonView';
import type { FishAgent, FishAgentImage, FishAgentReply, FishAgentStep } from '../types';

/// Load one contact sheet only when its row is on screen. The snapshot carries
/// a key, not megabytes of base64, so a one-second poll stays small.
function AgentShot({ image }: { image: FishAgentImage }) {
  const [src, setSrc] = useState('');
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let alive = true;
    setFailed(false);
    setSrc('');
    call<{ data_url: string }>('fish.agent.image', { key: image.key })
      .then((result) => {
        if (alive) setSrc(result?.data_url || '');
      })
      .catch(() => {
        if (alive) setFailed(true);
      });
    return () => { alive = false; };
  }, [image.key]);
  return (
    <figure className="agent-shot">
      {src ? (
        <a href={src} target="_blank" rel="noreferrer" title="点开看大图">
          <img src={src} alt={image.label} loading="lazy" />
        </a>
      ) : (
        <div className="agent-shot-placeholder">
          {failed ? '画面已过期' : '正在读取拼接画面…'}
        </div>
      )}
      <figcaption>{image.label}</figcaption>
    </figure>
  );
}

function parseOutput(step: FishAgentStep): any {
  if (!step.output) return null;
  try {
    return JSON.parse(step.output);
  } catch {
    return null;
  }
}

function listOf(value: any): any[] {
  return Array.isArray(value) ? value : [];
}

function textOf(value: any, max = 220): string {
  const text = String(value ?? '').trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/// What a tool actually saw, painted inline instead of hiding it behind a JSON
/// blob. This is the part the owner reads to audit the agent.
function ToolPreview({ step }: { step: FishAgentStep }) {
  const data = parseOutput(step);
  if (!data) return null;

  if (step.tool === 'list_videos') {
    const videos = listOf(data.videos).slice(0, 10);
    if (!videos.length) {
      return <div className="agent-tool-preview agent-tool-empty">{textOf(data.note || '候选列表为空')}</div>;
    }
    return (
      <div className="agent-tool-preview">
        <div className="agent-preview-head">
          <span>候选 · {data.label || data.source || '视频列表'}</span>
          <span>{data.count ?? videos.length} 条</span>
        </div>
        <div className="agent-candidates">
          {videos.map((video: any, index: number) => (
            <div className="agent-candidate" key={video.bvid || index}>
              <span className="agent-candidate-index">{index + 1}</span>
              <div className="agent-candidate-copy">
                <div className="agent-candidate-title">{video.title}</div>
                <div className="agent-candidate-meta">
                  {[video.up_name, video.duration, video.view ? `${formatCount(video.view)} 播放` : '']
                    .filter(Boolean)
                    .join(' · ')}
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (step.tool === 'open_video') {
    const comments = listOf(data.comments).slice(0, 8);
    return (
      <div className="agent-tool-preview">
        <div className="agent-video-facts">
          <div className="agent-video-fact-main">
            <span className="agent-video-fact-title">{data.title || step.detail}</span>
            <span className="agent-video-fact-sub">
              {[
                data.up_name,
                data.duration,
                data.view ? `${formatCount(data.view)} 播放` : '',
                data.tname,
              ].filter(Boolean).join(' · ')}
            </span>
          </div>
          <span className="agent-chip">
            评论 {data.comments_total ?? comments.length} 条
          </span>
          <span className="agent-chip">
            字幕 {data.subtitle_cues ?? 0} 句
          </span>
        </div>
        {data.desc ? <div className="agent-desc">{textOf(data.desc, 360)}</div> : null}
        {comments.length ? (
          <div className="agent-comments">
            <div className="agent-evidence-label">评论 · 按点赞</div>
            {comments.map((comment: any, index: number) => (
              <div className="agent-comment" key={`${comment.user}-${index}`}>
                <span className="agent-comment-like">{formatCount(comment.like || 0)}</span>
                <div className="agent-comment-copy">
                  <div className="agent-comment-user">{comment.user || '匿名'}</div>
                  <div className="agent-comment-text">{textOf(comment.text, 180)}</div>
                </div>
              </div>
            ))}
          </div>
        ) : null}
      </div>
    );
  }

  if (step.tool === 'watch_clip') {
    const subtitles = listOf(data.subtitles);
    const danmaku = listOf(data.danmaku);
    return (
      <div className="agent-tool-preview">
        {step.images?.length ? (
          <div className="agent-shots">
            {step.images.map((image) => (
              <AgentShot image={image} key={image.key} />
            ))}
          </div>
        ) : null}
        <div className="agent-evidence-grid">
          <div className="agent-evidence-block">
            <div className="agent-evidence-label">
              字幕 · {subtitles.length} 句
              {data.subtitle_source === 'none' ? '（没有字幕）' : ''}
            </div>
            {subtitles.length ? (
              <div className="agent-subtitle-list">
                {subtitles.slice(0, 12).map((line: any, index: number) => (
                  <div className="agent-subtitle-line" key={`s-${index}`}>
                    <span>{line.at || ''}</span>
                    <p>{textOf(line.text, 140)}</p>
                  </div>
                ))}
              </div>
            ) : (
              <div className="agent-tool-empty">这一段没有字幕，只能看画面和评论。</div>
            )}
          </div>
          <div className="agent-evidence-block">
            <div className="agent-evidence-label">
              弹幕 · {data.danmaku_total ?? danmaku.length} 条
            </div>
            {danmaku.length ? (
              <div className="agent-danmaku-list">
                {danmaku.slice(0, 24).map((line: any, index: number) => (
                  <span className="agent-danmaku" key={`d-${index}`} title={line.text}>
                    <em>{line.at_s != null ? `${Math.round(Number(line.at_s))}s` : ''}</em>
                    {textOf(line.text, 60)}
                  </span>
                ))}
              </div>
            ) : (
              <div className="agent-tool-empty">这一段没有弹幕。</div>
            )}
          </div>
        </div>
        {data.focus ? <div className="agent-focus">关注：{data.focus}</div> : null}
      </div>
    );
  }

  return null;
}

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
        <span className="agent-meta">{thinking ? step.meta : step.tool || step.id}</span>
        <span className="agent-preview">{step.detail}</span>
        <span className="agent-tail">
          {step.ms ? <span className="agent-timing">{formatMs(step.ms)}</span> : null}
          {step.bytes ? <span className="agent-usage">{formatBytes(step.bytes)}</span> : null}
          {step.tokens ? <span className="agent-usage">{formatCount(step.tokens)} tok</span> : null}
        </span>
        <Chevron open={open} />
      </button>

      {thinking && !open ? (
        <div className="agent-thinking-preview">{textOf(step.thinking || step.detail, 240)}</div>
      ) : null}

      {!thinking ? <ToolPreview step={step} /> : null}

      {open ? (
        <div className={`agent-card-body ${thinking ? 'agent-thinking-body' : 'agent-tool-body'}`}>
          {thinking ? (
            <div className="agent-io">
              <div className="agent-io-label">模型原文 · reasoning / content</div>
              <pre className="agent-thinking-full">{step.thinking || step.detail || '（没有文字）'}</pre>
            </div>
          ) : null}
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
  const [systemOpen, setSystemOpen] = useState(false);
  const [userOpen, setUserOpen] = useState(false);
  const threadRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const state = agent ? STATE_LABEL[agent.state] || STATE_LABEL.running : null;
  const steps = agent?.steps || [];
  const failed = steps.filter((step) => step.state === 'failed').length;
  const skipped = steps.filter((step) => step.state === 'skipped').length;
  // A cheap signature that changes when a row's state/detail moves, so the
  // tail keeps following even when several updates land in the same second.
  const signature = steps
    .map((step) => `${step.id}:${step.state}:${step.detail}:${step.ms}:${step.images?.length || 0}`)
    .join('|');

  useEffect(() => {
    if (!stick.current) return;
    const node = threadRef.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [agent?.updated_at, steps.length, agent?.state, signature]);

  const toggleStep = (id: string) => {
    setExpanded((current) => current.includes(id) ? current.filter((row) => row !== id) : [...current, id]);
  };

  const waiting = !agent || agent.state === 'running' || agent.state === 'waiting';
  const title = agent?.title || (agent ? '正在挑选视频…' : '');

  return (
    <div className={`agent-chat ${full ? 'full' : ''}`}>
      <div className="agent-head">
        <div className="agent-head-main">
          <span className={`agent-live ${agent?.state || 'idle'}`} />
          <div>
            <div className="agent-head-title">Agent</div>
            <div className="agent-head-sub">
              {agent ? `DeepSeek 自主看片 · ${state?.label || agent.state}` : 'DeepSeek 自主看片 · 等待开始'}
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
            <div className="agent-empty-hint">点「开始 agent」，它会自己拿候选、按标题挑视频、读评论、分段看画面/字幕/弹幕，再给出看或不看的结论；做完一个会自动进入下一个。</div>
          </div>
        ) : (
          <>
            <div className="agent-card prompt">
              <div className="agent-card-head static">
                <span className="agent-dot prompt" />
                <span className="agent-title">{agent.bvid ? '投喂' : '挑选中'}</span>
                <span className="agent-meta">你</span>
                <span className="agent-tail">
                  {agent.duration_s ? <span className="agent-timing">{formatDuration(agent.duration_s)}</span> : null}
                </span>
              </div>
              <div className="agent-prompt-body">
                {agent.cover ? <CoverImage src={agent.cover} alt="" eager className="agent-cover" /> : (
                  <div className="agent-cover agent-cover-empty">AI</div>
                )}
                <div className="agent-prompt-copy">
                  <div className="agent-video-title">{title || '正在挑选视频…'}</div>
                  <div className="agent-video-sub">
                    {[agent.up_name, agent.bvid].filter(Boolean).join(' · ') || agent.note || '正在读取候选列表'}
                  </div>
                  {agent.instruction ? <div className="agent-instruction">“{agent.instruction}”</div> : null}
                </div>
              </div>
            </div>

            {agent.system_prompt || agent.user_prompt ? (
              <>
                <PromptDoc
                  title="系统提示词"
                  meta="system · 工具与规则"
                  tone="system"
                  text={agent.system_prompt}
                  open={systemOpen}
                  onToggle={() => setSystemOpen((open) => !open)}
                />
                <PromptDoc
                  title="起始消息"
                  meta="user · 看片任务"
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
            ) : waiting ? (
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
