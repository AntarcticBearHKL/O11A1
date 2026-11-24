import React, { useEffect, useRef, useState } from 'react';
import { call, formatCount } from '../api';
import type { FishAgent, FishAgentImage, FishAgentStep } from '../types';

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

function parseJson(text?: string): any {
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function listOf(value: any): any[] {
  return Array.isArray(value) ? value : [];
}

function textOf(value: any, max = 260): string {
  const text = String(value ?? '').trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

function formatValue(value: any): string {
  if (value == null) return '—';
  if (typeof value === 'boolean') return value ? '是' : '否';
  if (Array.isArray(value)) {
    if (!value.length) return '—';
    if (value.every((item) => item == null || typeof item !== 'object')) {
      return value.map((item) => String(item)).join('、');
    }
    return `${value.length} 项`;
  }
  if (typeof value === 'object') return `${Object.keys(value).length} 项`;
  return textOf(value, 140);
}

const ARG_LABELS: Record<string, string> = {
  bvid: '视频',
  limit: '数量',
  start_s: '开始',
  end_s: '结束',
  interval_s: '间隔',
  focus: '关注',
  source: '来源',
  score: '评分',
  verdict: '结论',
  one_line: '一句话',
  reasons: '理由',
  tags: '标签',
  quote: '原话',
  note: '说明',
};

/// Inline emphasis for model prose: **bold** and `code`. Everything is a text
/// node, so a model reply can never inject markup.
function inlineText(text: string): React.ReactNode[] {
  return text.split(/(\*\*[^*]+\*\*|`[^`]+`)/g).map((part, index) => {
    if (part.startsWith('**') && part.endsWith('**') && part.length > 4) {
      return <strong key={index}>{part.slice(2, -2)}</strong>;
    }
    if (part.startsWith('`') && part.endsWith('`') && part.length > 2) {
      return <code key={index}>{part.slice(1, -1)}</code>;
    }
    return <React.Fragment key={index}>{part}</React.Fragment>;
  });
}

/// A small Markdown-like renderer for model reasoning and replies: paragraphs,
/// headings, bullet/numbered lists, quotes and fenced code. The layout is big
/// enough to read without leaning in, and it deliberately renders text nodes
/// instead of HTML.
function RichText({ text }: { text: string }) {
  const lines = String(text || '').replace(/\r\n/g, '\n').split('\n');
  const blocks: React.ReactNode[] = [];
  let paragraph: string[] = [];
  let list: { ordered: boolean; items: string[] } | null = null;
  let code: string[] | null = null;
  let key = 0;

  const flushParagraph = () => {
    if (!paragraph.length) return;
    blocks.push(<p key={`p-${key++}`}>{inlineText(paragraph.join('\n'))}</p>);
    paragraph = [];
  };
  const flushList = () => {
    if (!list) return;
    const items = list.items.map((item, index) => <li key={`li-${index}`}>{inlineText(item)}</li>);
    blocks.push(list.ordered
      ? <ol key={`ol-${key++}`}>{items}</ol>
      : <ul key={`ul-${key++}`}>{items}</ul>);
    list = null;
  };
  const flushCode = () => {
    if (!code) return;
    blocks.push(
      <pre className="agent-rich-code" key={`code-${key++}`}>
        <code>{code.join('\n')}</code>
      </pre>,
    );
    code = null;
  };
  const flush = () => { flushParagraph(); flushList(); };

  for (const raw of lines) {
    if (raw.trim().startsWith('```')) {
      if (code) flushCode();
      else { flush(); code = []; }
      continue;
    }
    if (code) { code.push(raw); continue; }
    const line = raw.trimEnd();
    if (!line.trim()) { flush(); continue; }
    const heading = line.match(/^#{1,6}\s+(.+)$/);
    if (heading) {
      flush();
      blocks.push(<h4 key={`h-${key++}`}>{inlineText(heading[1])}</h4>);
      continue;
    }
    const bullet = line.match(/^\s*[-*•]\s+(.+)$/);
    if (bullet) {
      flushParagraph();
      if (list && list.ordered) flushList();
      if (!list) list = { ordered: false, items: [] };
      list.items.push(bullet[1]);
      continue;
    }
    const ordered = line.match(/^\s*\d+[.)]\s+(.+)$/);
    if (ordered) {
      flushParagraph();
      if (list && !list.ordered) flushList();
      if (!list) list = { ordered: true, items: [] };
      list.items.push(ordered[1]);
      continue;
    }
    const quote = line.match(/^>\s?(.*)$/);
    if (quote) {
      flush();
      blocks.push(<blockquote key={`q-${key++}`}>{inlineText(quote[1])}</blockquote>);
      continue;
    }
    flushList();
    paragraph.push(line);
  }
  flushCode();
  flush();
  if (!blocks.length) return null;
  return <div className="agent-rich">{blocks}</div>;
}

function FactList({ data }: { data: any }) {
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    return <div className="agent-tool-empty">{textOf(data, 600)}</div>;
  }
  const entries = Object.entries(data).filter(([, value]) => value != null && value !== '');
  if (!entries.length) return null;
  return (
    <div className="agent-facts">
      {entries.map(([key, value]) => (
        <div className="agent-fact" key={key}>
          <span>{ARG_LABELS[key] || key}</span>
          <b>{formatValue(value)}</b>
        </div>
      ))}
    </div>
  );
}

/// A parsed answer to rate_video. The arguments carry the model's written
/// judgement (score, verdict, reasons, tags and quote); the result only says it
/// was accepted, so read both and paint the model's side.
function RatePreview({ step }: { step: FishAgentStep }) {
  const args = parseJson(step.arguments) || {};
  const data = parseJson(step.output) || {};
  const score = Number(args.score ?? data.score);
  const verdict = String(args.verdict ?? data.verdict ?? '也许');
  const oneLine = String(args.one_line ?? data.one_line ?? '').trim();
  const reasons = listOf(args.reasons).map((reason) => String(reason)).filter(Boolean);
  const tags = listOf(args.tags).map((tag) => String(tag)).filter(Boolean);
  const quote = String(args.quote ?? '').trim();
  const tone = verdict === '看' ? 'ok' : verdict === '不看' ? 'no' : 'maybe';
  return (
    <div className="agent-tool-preview">
      <div className={`agent-rate ${tone}`}>
        <span className="agent-rate-score">{Number.isFinite(score) ? Math.round(score) : '—'}</span>
        <div className="agent-rate-copy">
          <div className="agent-rate-verdict">
            {verdict}
            <span> / 10</span>
          </div>
          {oneLine ? <div className="agent-rate-line">{oneLine}</div> : null}
        </div>
      </div>
      {reasons.length ? (
        <ul className="agent-rate-reasons">
          {reasons.map((reason, index) => <li key={`${index}-${reason}`}>{reason}</li>)}
        </ul>
      ) : null}
      {quote ? <div className="agent-rate-quote">“{quote}”</div> : null}
      {tags.length ? (
        <div className="agent-rate-tags">
          {tags.map((tag) => <span className="agent-chip" key={tag}>{tag}</span>)}
        </div>
      ) : null}
    </div>
  );
}

/// What a tool actually saw, painted inline instead of hiding it behind a JSON
/// blob. This is the part the owner reads to audit the agent.
function ToolPreview({ step, expanded }: { step: FishAgentStep; expanded: boolean }) {
  const data = parseJson(step.output);

  if (!data) {
    if (step.state === 'running') {
      return <div className="agent-tool-preview"><div className="agent-tool-empty">等待工具返回…</div></div>;
    }
    return step.detail
      ? <div className="agent-tool-preview"><div className="agent-tool-empty">{step.detail}</div></div>
      : null;
  }

  if (data.error) {
    return (
      <div className="agent-tool-preview">
        <div className="agent-tool-error">{textOf(data.error, 600)}</div>
      </div>
    );
  }

  if (step.tool === 'list_videos') {
    const all = listOf(data.videos);
    const videos = all.slice(0, expanded ? 30 : 4);
    if (!videos.length) {
      return <div className="agent-tool-preview"><div className="agent-tool-empty">{textOf(data.note || '候选列表为空')}</div></div>;
    }
    return (
      <div className="agent-tool-preview">
        <div className="agent-preview-head">
          <span>候选 · {data.label || data.source || '视频列表'}</span>
          <span>{data.count ?? all.length} 条</span>
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
        {!expanded && all.length > videos.length ? (
          <div className="agent-expand-hint">展开查看其余 {all.length - videos.length} 条</div>
        ) : null}
      </div>
    );
  }

  if (step.tool === 'open_video') {
    const allComments = listOf(data.comments);
    const comments = allComments.slice(0, expanded ? 60 : 3);
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
            评论 {data.comments_total ?? allComments.length} 条
          </span>
          <span className="agent-chip">
            字幕 {data.subtitle_cues ?? 0} 句
          </span>
        </div>
        {data.desc ? <div className="agent-desc">{textOf(data.desc, expanded ? 1600 : 360)}</div> : null}
        {comments.length ? (
          <div className="agent-comments">
            <div className="agent-evidence-label">评论 · 按点赞</div>
            {comments.map((comment: any, index: number) => (
              <div className="agent-comment" key={`${comment.user}-${index}`}>
                <span className="agent-comment-like">{formatCount(comment.like || 0)}</span>
                <div className="agent-comment-copy">
                  <div className="agent-comment-user">{comment.user || '匿名'}</div>
                  <div className="agent-comment-text">{textOf(comment.text, expanded ? 500 : 160)}</div>
                </div>
              </div>
            ))}
          </div>
        ) : null}
        {!expanded && allComments.length > comments.length ? (
          <div className="agent-expand-hint">展开查看其余 {allComments.length - comments.length} 条评论</div>
        ) : null}
      </div>
    );
  }

  if (step.tool === 'watch_clip') {
    const subtitles = listOf(data.subtitles);
    const danmaku = listOf(data.danmaku);
    return (
      <div className="agent-tool-preview">
        <div className="agent-preview-head">
          <span>每 {data.interval_s ?? 1} 秒一帧 · {data.frame_count ?? 0} 帧</span>
          <span>{data.sheet_count ?? step.images?.length ?? 0} 张拼图</span>
        </div>
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
                {subtitles.slice(0, expanded ? 40 : 8).map((line: any, index: number) => (
                  <div className="agent-subtitle-line" key={`s-${index}`}>
                    <span>{line.at || ''}</span>
                    <p>{textOf(line.text, expanded ? 300 : 160)}</p>
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
                {danmaku.slice(0, expanded ? 60 : 16).map((line: any, index: number) => (
                  <span className="agent-danmaku" key={`d-${index}`} title={line.text}>
                    <em>{line.at_s != null ? `${Math.round(Number(line.at_s))}s` : ''}</em>
                    {textOf(line.text, 80)}
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

  if (step.tool === 'rate_video') {
    return <RatePreview step={step} />;
  }

  return (
    <div className="agent-tool-preview">
      <FactList data={data} />
    </div>
  );
}

/// The arguments of a tool call, in plain language instead of raw JSON.
function AgentArgs({ step }: { step: FishAgentStep }) {
  const data = parseJson(step.arguments);
  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  const entries = Object.entries(data)
    .filter(([, value]) => value != null && value !== '' && !(Array.isArray(value) && !value.length))
    .filter(([key]) => key !== 'bvid');
  if (!entries.length) return null;
  return (
    <div className="agent-args">
      <span className="agent-args-label">调用参数</span>
      {entries.map(([key, value]) => (
        <span className="agent-arg" key={key}>
          <em>{ARG_LABELS[key] || key}</em>
          <b>{formatValue(value)}</b>
        </span>
      ))}
    </div>
  );
}

/// One thinking or tool row. Thinking is painted as prose; a tool shows its
/// parsed result. Raw request/response JSON is never rendered.
function AgentStepRow({
  step,
  index,
  open,
  onToggle,
}: {
  step: FishAgentStep;
  index: number;
  open: boolean;
  onToggle: () => void;
}) {
  const state = STATE_LABEL[step.state] || STATE_LABEL.running;
  const thinking = step.kind === 'thinking';
  const thought = String(step.thinking || (thinking ? step.detail : '') || '').trim();
  const longThought = thought.length > 420 || thought.split('\n').length > 5;
  const collapsible = thinking ? longThought : true;
  return (
    <div className={`agent-card agent-step ${step.kind} ${step.state} ${open ? 'open' : ''}`} data-step-index={index}>
      <button
        className={`agent-card-head ${collapsible ? '' : 'static'}`}
        type="button"
        onClick={onToggle}
        aria-expanded={open}
      >
        <span className={`agent-dot ${step.kind} ${step.state}`} />
        <span className="agent-title">{step.title}</span>
        <span className={`agent-badge ${state.tone}`}>{state.label}</span>
        <span className="agent-meta">{thinking ? step.meta : step.tool || step.id}</span>
        {!thinking ? <span className="agent-preview">{step.detail}</span> : null}
        <span className="agent-tail">
          {step.ms ? <span className="agent-timing">{formatMs(step.ms)}</span> : null}
          {step.bytes ? <span className="agent-usage">{formatBytes(step.bytes)}</span> : null}
          {step.tokens ? <span className="agent-usage">{formatCount(step.tokens)} tok</span> : null}
        </span>
        {collapsible ? <Chevron open={open} /> : null}
      </button>

      {thinking ? (
        <div className={`agent-thinking-body ${longThought && !open ? 'clamped' : ''}`}>
          <RichText text={thought || '（这一轮没有文字）'} />
          {longThought ? (
            <button className="agent-more" type="button" onClick={onToggle}>
              {open ? '收起' : '展开完整思考'}
            </button>
          ) : null}
        </div>
      ) : (
        <>
          <ToolPreview step={step} expanded={open} />
          {open && step.tool !== 'rate_video' ? (
            <div className="agent-step-extra">
              <AgentArgs step={step} />
            </div>
          ) : null}
        </>
      )}
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
/// A flat transcript: each model turn is readable prose, each tool call paints
/// the evidence it returned. The thread follows the tail only while the reader
/// stays at the bottom.
export function AgentTrace({
  agent,
  full = false,
  parked = false,
}: {
  agent: FishAgent | null;
  full?: boolean;
  parked?: boolean;
}) {
  const [expanded, setExpanded] = useState<string[]>([]);
  const threadRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const seen = useRef(0);
  const parkedAt = useRef(0);

  const steps = agent?.steps || [];
  const waiting = Boolean(agent && (agent.state === 'running' || agent.state === 'waiting'));
  // A cheap signature that changes when a row's state/detail moves, so the
  // tail keeps following even when several updates land in the same second.
  const signature = steps
    .map((step) => `${step.id}:${step.state}:${step.detail}:${step.ms}:${step.images?.length || 0}`)
    .join('|');

  useEffect(() => {
    const node = threadRef.current;
    if (!node) return;
    const previous = seen.current;
    seen.current = steps.length;
    if (steps.length < previous) parkedAt.current = 0;
    // When the turn parks at the gate, anchor the transcript at that turn's
    // thinking row. The reader then starts with the model's reasoning and
    // scrolls down into the tools, instead of landing past both.
    if (parked && steps.length > parkedAt.current && steps.length > 0) {
      parkedAt.current = steps.length;
      let index = -1;
      for (let at = steps.length - 1; at >= 0; at -= 1) {
        if (steps[at].kind === 'thinking') { index = at; break; }
      }
      const row = index >= 0 ? node.querySelector<HTMLElement>(`[data-step-index="${index}"]`) : null;
      if (row) {
        const delta = row.getBoundingClientRect().top - node.getBoundingClientRect().top;
        node.scrollTop = Math.max(0, node.scrollTop + delta - 2);
        return;
      }
    }
    if (!stick.current) return;
    node.scrollTop = node.scrollHeight;
  }, [agent?.updated_at, steps.length, agent?.state, signature, parked]);

  const toggleStep = (id: string) => {
    setExpanded((current) => current.includes(id) ? current.filter((row) => row !== id) : [...current, id]);
  };

  return (
    <div className={`agent-chat ${full ? 'full' : ''}`}>
      <div
        className="agent-thread"
        ref={threadRef}
        onScroll={() => {
          const node = threadRef.current;
          if (!node) return;
          stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
        }}
      >
        {steps.map((step, index) => (
          <AgentStepRow
            key={step.id}
            step={step}
            index={index}
            open={expanded.includes(step.id)}
            onToggle={() => toggleStep(step.id)}
          />
        ))}
        {waiting ? (
          <div className="agent-waiting">
            <span className="agent-dot thinking running" />
            <span className="agent-waiting-text">{agent?.note || '正在思考…'}</span>
            <span className="agent-cursor" />
          </div>
        ) : null}
      </div>
    </div>
  );
}
