import React, { useState } from 'react';

function parseJson(text: string): unknown {
  const raw = String(text ?? '').trim();
  if (!raw || (raw[0] !== '{' && raw[0] !== '[')) return undefined;
  try {
    return JSON.parse(raw) as unknown;
  } catch {
    return undefined;
  }
}

function isContainer(value: unknown): value is Record<string, unknown> | unknown[] {
  return Boolean(value) && typeof value === 'object';
}

function valueText(value: unknown): string {
  if (typeof value === 'string') return JSON.stringify(value);
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  return String(value);
}

function valueTone(value: unknown): string {
  if (typeof value === 'string') return 'string';
  if (typeof value === 'number') return 'number';
  if (typeof value === 'boolean') return 'boolean';
  return 'null';
}

function JsonNode({ name, value, depth }: { name?: string; value: unknown; depth: number }) {
  const container = isContainer(value);
  const [open, setOpen] = useState(depth < 2);

  if (!container) {
    return (
      <div className="agent-json-row">
        {name !== undefined ? <span className="agent-json-key">{name}: </span> : null}
        <span className={`agent-json-value ${valueTone(value)}`}>{valueText(value)}</span>
      </div>
    );
  }

  const entries = Array.isArray(value)
    ? value.map((item, index) => [String(index), item] as const)
    : Object.entries(value);
  const openChar = Array.isArray(value) ? '[' : '{';
  const closeChar = Array.isArray(value) ? ']' : '}';

  return (
    <div className="agent-json-node">
      <button
        type="button"
        className="agent-json-row toggle"
        onClick={() => setOpen((current) => !current)}
        aria-expanded={open}
      >
        <span className={`agent-json-caret ${open ? 'open' : ''}`}>▸</span>
        {name !== undefined ? <span className="agent-json-key">{name}: </span> : null}
        <span className="agent-json-bracket">{openChar}</span>
        {!open ? (
          <>
            <span className="agent-json-count">
              {entries.length} {Array.isArray(value) ? 'items' : 'keys'}
            </span>
            <span className="agent-json-bracket">{closeChar}</span>
          </>
        ) : null}
      </button>
      {open ? (
        <div className="agent-json-children">
          {entries.map(([key, item]) => (
            <JsonNode key={key} name={key} value={item} depth={depth + 1} />
          ))}
          <div className="agent-json-row"><span className="agent-json-bracket">{closeChar}</span></div>
        </div>
      ) : null}
    </div>
  );
}

/// Collapsible JSON rendering for tool arguments and results. Non-JSON text
/// falls back to a plain preformatted block, so a clipped or malformed payload
/// is still readable instead of disappearing.
export function JsonView({ text, empty = '(空)' }: { text: string; empty?: string }) {
  const parsed = parseJson(text);
  if (parsed === undefined) {
    return <pre className="agent-code">{String(text || '').trim() || empty}</pre>;
  }
  return (
    <div className="agent-json">
      <JsonNode value={parsed} depth={0} />
    </div>
  );
}
