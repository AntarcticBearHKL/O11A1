import React, { useCallback, useEffect, useRef, useState } from 'react';
import QRCode from 'qrcode';
import { call } from './api';
import App from './App';
import { WindowResizeGrip } from './components/WindowResizeGrip';
import { LoginQrPlaceholder, LoginSurface, loginQrColors, readLastAccount } from './components/LoginSurface';
import { preloadAll, refreshFeedAfterLogin, type BootstrapData, type PreloadData, type PreloadProgress } from './preload';
import { IconClose, IconMax, IconMin } from './components/Icons';
import type { Account, Settings } from './types';

function applyTheme(settings: Settings) {
  const dark = settings.theme === 2;
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  const alpha = Math.max(0.58, 0.62 + (1 - Number(settings.glass_transparency || 0) / 100) * 0.36);
  const rgb = dark ? '38, 40, 46' : '255, 252, 246';
  document.documentElement.style.setProperty('--card', `rgba(${rgb}, ${alpha})`);
  document.documentElement.style.setProperty('--glass-blur', `${Math.max(0, Number(settings.glass_blur || 0)) * 0.18}px`);
  document.documentElement.style.setProperty('--wallpaper-blur', `${Math.max(0, Number(settings.backdrop_blur || 0))}px`);
}

function WindowControls({ closeAction }: { closeAction: number }) {
  return (
    <div className="login-window-controls no-drag">
      <button className="icon-btn" title="最小化" onClick={() => window.o11a1.window.minimize()}><IconMin /></button>
      <button className="icon-btn" title="最大化" onClick={() => window.o11a1.window.maximize()}><IconMax /></button>
      <button
        className="icon-btn close"
        title={closeAction === 2 ? '退出程序' : '关闭到托盘'}
        onClick={() => { if (closeAction === 2) window.o11a1.window.quit(); else window.o11a1.window.close(); }}
      ><IconClose /></button>
    </div>
  );
}

/// Windowed mode keeps the 22px transparent rim for the paper shadow; the
/// maximized startup window must drop it and fill the work area, exactly like
/// the main app. The login gate therefore tracks the real window state.
function useWindowMaximized() {
  const [maximized, setMaximized] = useState(false);
  useEffect(() => {
    let alive = true;
    const sync = () => {
      window.o11a1.window
        ?.isMaximized?.()
        .then((value: boolean) => {
          if (alive) setMaximized(!!value);
        })
        .catch(() => {});
    };
    sync();
    window.addEventListener('resize', sync);
    return () => {
      alive = false;
      window.removeEventListener('resize', sync);
    };
  }, []);
  return maximized;
}

/// The startup window has exactly one login surface in the middle of the home
/// wallpaper. Window buttons are chrome, not part of the sign-in content.
function LoginScreen({ closeAction, children }: { closeAction: number; children: React.ReactNode }) {
  const maximized = useWindowMaximized();
  return (
    <>
      <div
        className={`login-gate${maximized ? ' maximized' : ''}`}
        onPointerDownCapture={(event) => {
          if (event.button !== 0) return;
          const target = event.target as Element | null;
          if (target?.closest(
            'button, a, input, textarea, select, option, label, [role="button"], [role="tab"], .login-surface, .login-window-controls, .no-drag',
          )) return;
          window.o11a1.window.startDrag?.().catch?.(() => {});
        }}
      >
        <div className="login-drag" aria-hidden="true" />
        <WindowControls closeAction={closeAction} />
        <section className="login-panel">{children}</section>
      </div>
      <WindowResizeGrip hidden={maximized} />
    </>
  );
}

/// Boot / post-scan state. Same surface as the QR view: an optional user
/// portrait, one loading bar and one progress line. A missing account simply
/// means no portrait; nothing else is added.
function LoginLoading({
  closeAction,
  progress,
  step,
  error,
  account,
}: {
  closeAction: number;
  progress?: number;
  step: string;
  error?: string;
  account?: Account | null;
}) {
  return (
    <LoginScreen closeAction={closeAction}>
      <LoginSurface
        account={account ?? null}
        showAvatar={Boolean(account)}
        progress={typeof progress === 'number' ? progress : null}
        status={error || step}
        tone={error ? 'error' : 'normal'}
      />
    </LoginScreen>
  );
}

interface GateProps {
  bootstrap: BootstrapData | null;
  expired?: boolean;
  onAccount: (account: Account) => void;
  onEnter: () => void;
}

function LoginGate({ bootstrap, expired = false, onAccount, onEnter }: GateProps) {
  const [qr, setQr] = useState<{ url: string; key: string } | null>(null);
  const [qrState, setQrState] = useState(
    expired ? '登录已失效，请扫码重新登录' : '正在生成二维码…',
  );
  const [qrError, setQrError] = useState('');
  const [polling, setPolling] = useState(false);
  const [entering, setEntering] = useState(false);
  const [scanned, setScanned] = useState<Account | null>(null);
  const [last] = useState<Account | null>(() => readLastAccount());
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const pollRef = useRef<any>(null);
  const closeAction = bootstrap?.settings.close_action ?? 1;

  useEffect(() => {
    if (!bootstrap) return;
    applyTheme(bootstrap.settings);
  }, [bootstrap]);

  const loadQr = useCallback(async () => {
    setQrError('');
    setQrState('正在生成二维码…');
    setPolling(false);
    try {
      const result = await call<{ url: string; key: string }>('login.qr');
      setQr(result);
      setPolling(true);
      setQrState(
        expired ? '登录已失效，请扫码重新登录' : '打开哔哩哔哩扫一扫',
      );
    } catch {
      setQrError('二维码生成失败，点击重试');
    }
  }, [expired]);

  useEffect(() => {
    if (bootstrap && !qr && !qrError && !entering) loadQr();
  }, [bootstrap, qr, qrError, entering, loadQr, expired]);

  // The gate can mount a beat before the startup validation flips the flag;
  // keep the one status line honest once it does.
  useEffect(() => {
    if (expired) setQrState('登录已失效，请扫码重新登录');
  }, [expired]);

  useEffect(() => {
    if (!qr || !canvasRef.current) return;
    QRCode.toCanvas(canvasRef.current, qr.url, {
      width: 240,
      margin: 2,
      color: loginQrColors(),
    }).catch(() => setQrError('二维码生成失败，点击重试'));
  }, [qr]);

  useEffect(() => {
    if (!polling || !qr || entering) return;
    let stopped = false;
    let polls = 0;
    const tick = async () => {
      polls += 1;
      if (polls > 90) {
        setPolling(false);
        setQrState('二维码已过期，正在刷新…');
        setQr(null);
        setQrError('');
        return;
      }
      try {
        const result = await call<{ stateCode: number; account?: Account }>('login.poll', { key: qr.key });
        if (result.stateCode === 0) {
          setPolling(false);
          setEntering(true);
          if (result.account) {
            setScanned(result.account);
            onAccount(result.account);
            await refreshFeedAfterLogin(result.account).catch(() => {});
            onEnter();
          } else {
            setEntering(false);
            setQrError('登录成功但没有返回账号信息，点击重试');
          }
          return;
        }
        if (result.stateCode === 86038) {
          setPolling(false);
          setQrState('二维码已过期，正在刷新…');
          setQr(null);
          setQrError('');
        } else if (result.stateCode === 86090) {
          setQrState('已扫码，请在手机上确认');
        } else if (result.stateCode === 86101) {
          setQrState(
            expired ? '登录已失效，请扫码重新登录' : '打开哔哩哔哩扫一扫',
          );
        }
      } catch {
        /* transient poll failures are ignored */
      }
      if (!stopped) pollRef.current = setTimeout(tick, 2000);
    };
    pollRef.current = setTimeout(tick, 1200);
    return () => {
      stopped = true;
      clearTimeout(pollRef.current);
    };
  }, [polling, qr, entering, expired, onAccount, onEnter]);

  if (entering) {
    return (
      <LoginScreen closeAction={closeAction}>
        <LoginSurface
          account={scanned}
          progress={null}
          status="登录成功，正在进入主页…"
        />
      </LoginScreen>
    );
  }

  return (
    <LoginScreen closeAction={closeAction}>
      <LoginSurface
        account={last}
        showAvatar={false}
        progress={null}
        status={qrError || qrState}
        tone={qrError ? 'error' : 'normal'}
      >
        <div
          className={`login-qr-frame ${qrError ? 'error' : ''}`}
          onClick={() => {
            if (!polling || qrError) loadQr();
          }}
          title={qrError ? '点击重试' : '点击刷新二维码'}
        >
          {qr ? (
            <canvas ref={canvasRef} />
          ) : (
            <LoginQrPlaceholder />
          )}
        </div>
      </LoginSurface>
    </LoginScreen>
  );
}

function BootSplash({ bootstrap, progress, error }: {
  bootstrap: BootstrapData | null;
  progress: PreloadProgress;
  error: string;
}) {
  const closeAction = bootstrap?.settings.close_action ?? 1;
  return (
    <LoginLoading
      closeAction={closeAction}
      progress={progress.value}
      step={progress.step}
      error={error ? '启动失败，请重新打开应用' : ''}
      account={bootstrap?.account ?? null}
    />
  );
}

export default function Startup() {
  const [data, setData] = useState<PreloadData | null>(null);
  const [bootstrap, setBootstrap] = useState<BootstrapData | null>(null);
  const [account, setAccount] = useState<Account | null>(null);
  const [progress, setProgress] = useState<PreloadProgress>({ value: 0.02, step: '正在启动…' });
  const [error, setError] = useState('');
  const [entered, setEntered] = useState(false);
  const [expired, setExpired] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setError('');
    setProgress({ value: 0.02, step: '正在启动…' });
    preloadAll(
      (next) => { if (!cancelled) setProgress(next); },
      (next) => {
        if (cancelled) return;
        setBootstrap(next);
        setAccount(next.account);
        applyTheme(next.settings);
      },
    )
      .then(async (result) => {
        if (cancelled) return;
        // A stored account can outlive its server-side session. Keep showing
        // its avatar, but do not enter the app with a dead session: that is
        // what silently locks 720P/1080P behind a fake "大会员" state.
        if (result.bootstrap.account) {
          try {
            const nav = await call<any>('account.nav');
            if (nav?.NotLoggedIn) {
              await call('login.logout').catch(() => {});
              result.bootstrap.account = null;
              if (cancelled) return;
              setExpired(true);
            }
          } catch {
            /* transient network failure: keep the saved account */
          }
        }
        if (cancelled) return;
        setData(result);
        setBootstrap(result.bootstrap);
        setAccount(result.bootstrap.account);
        applyTheme(result.bootstrap.settings);
      })
      .catch((reason: any) => {
        if (!cancelled) setError(reason?.message || String(reason));
      });
    return () => { cancelled = true; };
  }, []);

  // A saved account never sees a login card: preload the feed, then enter.
  useEffect(() => {
    if (!data || !bootstrap?.account) return;
    const timer = setTimeout(() => setEntered(true), 180);
    return () => clearTimeout(timer);
  }, [data, bootstrap?.account]);

  if (entered && data) {
    return (
      <App
        initial={{
          settings: data.bootstrap.settings,
          account,
          version: data.bootstrap.version,
        }}
      />
    );
  }

  if (!bootstrap || bootstrap.account || entered) {
    return <BootSplash bootstrap={bootstrap} progress={progress} error={error} />;
  }

  return (
    <LoginGate
      bootstrap={bootstrap}
      expired={expired}
      onAccount={setAccount}
      onEnter={() => setEntered(true)}
    />
  );
}
