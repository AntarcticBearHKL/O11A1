import React, { useCallback, useEffect, useRef, useState } from 'react';
import QRCode from 'qrcode';
import { call, cover } from './api';
import App from './App';
import { preloadAll, refreshFeedAfterLogin, type BootstrapData, type PreloadData, type PreloadProgress } from './preload';
import { IconClose, IconMax, IconMin, IconRefresh } from './components/Icons';
import type { Account, Settings } from './types';

function applyTheme(settings: Settings) {
  const dark = settings.theme === 2;
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  const alpha = Math.max(0.58, 0.62 + (1 - Number(settings.glass_transparency || 0) / 100) * 0.36);
  const rgb = dark ? '38, 40, 46' : '255, 255, 255';
  document.documentElement.style.setProperty('--card', `rgba(${rgb}, ${alpha})`);
  document.documentElement.style.setProperty('--glass-blur', `${Math.max(0, Number(settings.glass_blur || 0)) * 0.18}px`);
  document.documentElement.style.setProperty('--wallpaper-blur', `${Math.max(0, Number(settings.backdrop_blur || 0))}px`);
}

interface GateProps {
  data: PreloadData | null;
  bootstrap: BootstrapData | null;
  account: Account | null;
  progress: PreloadProgress;
  error: string;
  onAccount: (account: Account) => void;
  onEnter: () => void;
  onRetry: () => void;
}

function LoginGate({ data, bootstrap, account, progress, error, onAccount, onEnter, onRetry }: GateProps) {
  const [showQr, setShowQr] = useState(false);
  const [qr, setQr] = useState<{ url: string; key: string } | null>(null);
  const [qrState, setQrState] = useState('准备中…');
  const [qrError, setQrError] = useState('');
  const [polling, setPolling] = useState(false);
  const [autoEnter, setAutoEnter] = useState(false);
  const [feedRefreshDone, setFeedRefreshDone] = useState(true);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const pollRef = useRef<any>(null);
  const ready = !!bootstrap && progress.value >= 1;
  const closeAction = bootstrap?.settings.close_action ?? 1;

  useEffect(() => {
    if (!bootstrap) return;
    applyTheme(bootstrap.settings);
  }, [bootstrap]);

  // No saved account: the QR is the login screen, so start it immediately.
  useEffect(() => {
    if (bootstrap && !account) setShowQr(true);
  }, [bootstrap, account]);

  const loadQr = useCallback(async () => {
    setQrError('');
    setQrState('正在生成二维码…');
    setPolling(false);
    try {
      const result = await call<{ url: string; key: string }>('login.qr');
      setQr(result);
      setPolling(true);
      setQrState('请使用哔哩哔哩手机客户端扫码');
    } catch (reason: any) {
      setQrError(reason?.message || String(reason));
      setQrState('二维码生成失败');
    }
  }, []);

  useEffect(() => {
    if (showQr && bootstrap && !qr && !qrError) loadQr();
  }, [showQr, bootstrap, qr, qrError, loadQr]);

  useEffect(() => {
    if (!qr || !canvasRef.current) return;
    QRCode.toCanvas(canvasRef.current, qr.url, {
      width: 190,
      margin: 1,
      color: { dark: '#1a1a1a', light: '#ffffff' },
    }).catch((reason: any) => setQrError(reason?.message || String(reason)));
  }, [qr]);

  useEffect(() => {
    if (!polling || !qr) return;
    let stopped = false;
    let polls = 0;
    const tick = async () => {
      polls += 1;
      if (polls > 90) {
        setPolling(false);
        setQrState('二维码已过期，请刷新');
        return;
      }
      try {
        const result = await call<{ stateCode: number; account?: Account }>('login.poll', { key: qr.key });
        if (result.stateCode === 0) {
          setPolling(false);
          setQrState('登录成功，正在准备首页…');
          if (result.account) {
            onAccount(result.account);
            setAutoEnter(true);
            setShowQr(false);
            setFeedRefreshDone(false);
            const fallback = setTimeout(() => setFeedRefreshDone(true), 6000);
            refreshFeedAfterLogin()
              .catch(() => {})
              .finally(() => {
                clearTimeout(fallback);
                setFeedRefreshDone(true);
              });
          }
          return;
        }
        if (result.stateCode === 86038) {
          setPolling(false);
          setQrState('二维码已过期，请刷新');
        } else if (result.stateCode === 86090) {
          setQrState('已扫码，请在手机上确认');
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
  }, [polling, qr, onAccount]);

  useEffect(() => {
    if (!autoEnter || !ready || !feedRefreshDone) return;
    const timer = setTimeout(onEnter, 450);
    return () => clearTimeout(timer);
  }, [autoEnter, ready, feedRefreshDone, onEnter]);

  const qrBlock = showQr ? (
    <div className="login-qr">
      <div className="login-qr-frame">
        {qr ? <canvas ref={canvasRef} /> : <div className="login-qr-wait"><span className="spinner" /></div>}
      </div>
      <div className={`login-qr-state ${qrError ? 'error' : ''}`}>{qrState}</div>
      <button className="login-link" onClick={loadQr}>
        <IconRefresh size={13} /> 刷新二维码
      </button>
    </div>
  ) : null;

  return (
    <div className="login-gate">
      <div className="login-drag" />
      <div className="login-window-controls no-drag">
        <button className="icon-btn" title="最小化" onClick={() => window.o11a1.window.minimize()}><IconMin /></button>
        <button className="icon-btn" title="最大化" onClick={() => window.o11a1.window.maximize()}><IconMax /></button>
        <button
          className="icon-btn close"
          title={closeAction === 2 ? '退出程序' : '关闭到托盘'}
          onClick={() => { if (closeAction === 2) window.o11a1.window.quit(); else window.o11a1.window.close(); }}
        ><IconClose /></button>
      </div>

      <div className="login-card">
        <div className="login-brand">O11A1<span>.</span></div>
        <div className="login-sub">登录后同步收藏、历史与播放记录</div>

        {account ? (
          <div className="login-account">
            {account.face
              ? <img src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
              : <div className="login-avatar-placeholder" />}
            <div className="login-account-copy">
              <div className="login-account-name">{account.uname}</div>
              <div className="login-account-uid">UID: {account.mid}</div>
            </div>
            <span className="login-account-badge">已登录</span>
          </div>
        ) : (
          <div className="login-no-account">
            <div className="login-no-account-title">扫码登录</div>
            <div className="login-no-account-sub">打开哔哩哔哩手机客户端扫一扫</div>
          </div>
        )}

        {qrBlock}

        <div className="login-actions">
          {account ? (
            <button className="btn primary login-primary" disabled={!ready} onClick={onEnter}>
              {ready ? '进入主页' : '正在加载首页…'}
            </button>
          ) : null}
          {account ? (
            <button className="login-link" onClick={() => { setShowQr((open) => !open); setQrError(''); }}>
              {showQr ? '收起二维码' : '使用其他账号扫码'}
            </button>
          ) : (
            <button className="login-link guest" disabled={!ready} onClick={onEnter}>
              {ready ? '以游客身份进入' : '正在加载首页…'}
            </button>
          )}
        </div>

        <div className="login-loading">
          <div className="login-progress"><span style={{ width: `${Math.round(Math.min(1, progress.value) * 100)}%` }} /></div>
          <div className={`login-step ${error ? 'error' : ''}`}>{error || progress.step}</div>
          {error ? <button className="login-link" onClick={onRetry}>重新连接</button> : null}
        </div>
      </div>
    </div>
  );
}

export default function Startup() {
  const [data, setData] = useState<PreloadData | null>(null);
  const [bootstrap, setBootstrap] = useState<BootstrapData | null>(null);
  const [account, setAccount] = useState<Account | null>(null);
  const [progress, setProgress] = useState<PreloadProgress>({ value: 0.02, step: '正在启动…' });
  const [error, setError] = useState('');
  const [entered, setEntered] = useState(false);
  const [attempt, setAttempt] = useState(0);

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
      .then((result) => {
        if (cancelled) return;
        setData(result);
        setAccount(result.bootstrap.account);
        applyTheme(result.bootstrap.settings);
      })
      .catch((reason: any) => {
        if (!cancelled) setError(reason?.message || String(reason));
      });
    return () => { cancelled = true; };
  }, [attempt]);

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

  return (
    <LoginGate
      data={data}
      bootstrap={bootstrap}
      account={account}
      progress={progress}
      error={error}
      onAccount={setAccount}
      onEnter={() => setEntered(true)}
      onRetry={() => setAttempt((value) => value + 1)}
    />
  );
}
