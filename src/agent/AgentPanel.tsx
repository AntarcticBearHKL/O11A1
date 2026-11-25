import React from 'react';
import { AgentTrace } from '../components/AgentTrace';
import type { FishAgent } from '../types';

/// The Agent tab body shared by the main Fish page and the standalone
/// `agent.html` debugging entry. Keeping this component free of page chrome is
/// what lets the standalone page hot-reload the exact same transcript UI.
export function AgentPanel({
  agent,
  errorText,
  parked,
  onCopyError,
}: {
  agent: FishAgent | null;
  errorText: string;
  parked: boolean;
  onCopyError?: () => void;
}) {
  return (
    <div className="agent-page">
      {errorText ? (
        <div className="fish-error-strip" role="alert">
          <b>运行出错</b>
          <span>{errorText}</span>
          <button
            className="btn sm"
            onClick={() => {
              if (onCopyError) {
                onCopyError();
                return;
              }
              navigator.clipboard.writeText(errorText).catch(() => {});
            }}
          >
            复制错误
          </button>
        </div>
      ) : null}

      <AgentTrace key={`full-${agent?.bvid || 'empty'}`} agent={agent} full parked={parked} />
    </div>
  );
}
