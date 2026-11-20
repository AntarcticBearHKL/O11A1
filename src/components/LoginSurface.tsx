import React from 'react';
import { cover } from '../api';
import type { Account } from '../types';

/// The last account that signed in on this machine. It is kept in local
/// storage (not the cookie store) so the sign-in screen can greet a returning
/// user with their own avatar even after `login.logout` cleared the session.
export const LAST_ACCOUNT_KEY = 'o11a1:last-account';

/// QR palette for the current login surface. In the light theme the quiet
/// zone is the surface colour itself, so the code sits directly on the paper
/// with no separate white card. Dark keeps a white quiet zone for contrast.
export function loginQrColors(): { dark: string; light: string } {
  if (document.documentElement.dataset.theme === 'dark') {
    return { dark: '#1a1a1a', light: '#ffffff' };
  }
  const surface = getComputedStyle(document.documentElement)
    .getPropertyValue('--surface')
    .trim();
  return {
    dark: '#1a1a1a',
    light: /^#[0-9a-f]{3,8}$/i.test(surface) ? surface : '#fffcf6',
  };
}

export function readLastAccount(): Account | null {
  try {
    const raw = localStorage.getItem(LAST_ACCOUNT_KEY);
    if (!raw) return null;
    const data = JSON.parse(raw);
    if (!data || !Number(data.mid) || !data.uname) return null;
    return {
      mid: Number(data.mid),
      uname: String(data.uname),
      face: String(data.face || ''),
      vip: Boolean(data.vip),
    };
  } catch {
    return null;
  }
}

/// Round profile portrait: a paper disc with a thin conic coral → cobalt ring
/// and a soft glow. A real avatar is shown when the machine remembers one;
/// otherwise the disc carries a quiet ink silhouette. This is the only
/// portrait used by the startup login and the in-app login, so both screens
/// show the same face.
export function LoginAvatar({
  account,
  size = 'lg',
}: {
  account: Account | null;
  size?: 'lg' | 'sm';
}) {
  return (
    <div className={`login-avatar login-avatar-${size} ${account?.face ? 'has-face' : ''}`}>
      <span className="login-avatar-glow" aria-hidden />
      <span className="login-avatar-ring">
        <span className="login-avatar-disc">
          {account?.face ? (
            <img src={cover(account.face)} referrerPolicy="no-referrer" alt={account.uname} draggable={false} />
          ) : (
            <svg className="login-avatar-glyph" viewBox="0 0 64 64" aria-hidden>
              <circle cx="32" cy="24.5" r="9.6" />
              <path d="M15.5 55.5c1.1-10.4 7.9-16 16.5-16s15.4 5.6 16.5 16" />
            </svg>
          )}
        </span>
      </span>
    </div>
  );
}

/// Blurred QR ghost shown while the real code is being generated. It uses the
/// same finder patterns and a deterministic module field, then the CSS blurs
/// it; nothing here is scannable, it only explains the empty frame.
export function LoginQrPlaceholder() {
  const modules: string[] = [];
  const finder = (x: number, y: number) => {
    modules.push(`M${x} ${y}h7v7h-7z M${x + 1} ${y + 1}h5v5h-5z`);
    modules.push(`M${x + 2} ${y + 2}h3v3h-3z`);
  };
  finder(0, 0);
  finder(22, 0);
  finder(0, 22);
  for (let y = 0; y < 29; y += 1) {
    for (let x = 0; x < 29; x += 1) {
      if ((x < 8 && y < 8) || (x > 20 && y < 8) || (x < 8 && y > 20)) continue;
      if (x === 6 || y === 6) {
        if ((x + y) % 2 === 0) modules.push(`M${x} ${y}h1v1h-1z`);
        continue;
      }
      if ((x * 13 + y * 7 + x * y * 3) % 11 < 4) {
        modules.push(`M${x} ${y}h1v1h-1z`);
      }
    }
  }
  return (
    <div className="login-qr-wait" aria-hidden>
      <svg
        className="login-qr-ghost"
        viewBox="0 0 29 29"
        shapeRendering="crispEdges"
      >
        <path d={modules.join(' ')} fillRule="evenodd" fill="currentColor" />
      </svg>
    </div>
  );
}

function LoginProgress({ value }: { value: number | null }) {
  if (value === null) {
    return (
      <div className="login-progress login-progress-indeterminate">
        <span />
      </div>
    );
  }
  const percent = Math.round(Math.max(0, Math.min(1, value)) * 100);
  return (
    <div className="login-progress">
      <span style={{ width: `${percent}%` }} />
    </div>
  );
}

/// The single login surface. It deliberately contains only three kinds of
/// objects: the user portrait / QR handshake, one loading bar and one status
/// line. No brand row, title, subtitle, chips, last-login pill or footnote.
export function LoginSurface({
  account,
  showAvatar = true,
  progress,
  status,
  tone = 'normal',
  children,
}: {
  account: Account | null;
  showAvatar?: boolean;
  /// `undefined` hides the bar, `null` is an indeterminate bar, a number is
  /// 0..1 determinate progress.
  progress?: number | null;
  status: string;
  tone?: 'normal' | 'error';
  children?: React.ReactNode;
}) {
  return (
    <div className="login-surface">
      {showAvatar ? <LoginAvatar account={account} /> : null}
      {children ? <div className="login-surface-object">{children}</div> : null}
      {progress !== undefined ? <LoginProgress value={progress} /> : null}
      <div className={`login-status ${tone === 'error' ? 'error' : ''}`}>{status}</div>
    </div>
  );
}
