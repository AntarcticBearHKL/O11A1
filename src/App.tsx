import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { call, cover } from './api';
import { AppContext, Route } from './context';
import { useToast } from './hooks';
import type { Account, Settings } from './types';
import {
  IconClock, IconClose, IconDownload, IconGear, IconMax, IconMin, IconSearch, IconStar,
} from './components/Icons';
import Home from './pages/Home';
import Search from './pages/Search';
import User from './pages/User';
import Favorites from './pages/Favorites';
import History from './pages/History';
import Regions from './pages/Regions';
import Pgc from './pages/Pgc';
import Live from './pages/Live';
import Dynamic from './pages/Dynamic';
import Messages from './pages/Messages';
import Member from './pages/Member';
import SettingsPage from './pages/Settings';
import Downloads from './pages/Downloads';
import Fish from './pages/Fish';
import { enqueuePlaylist, launchPlayer } from './player';

function parseRoute(): Route {
  const raw = window.location.hash.replace(/^#\/?/, '');
  const [path, queryText = ''] = raw.split('?');
  const segments = path.split('/').filter(Boolean);
  if (segments.length === 0) return { name: 'home', id: '', query: new URLSearchParams(queryText) };
  return {
    name: segments[0] || 'home',
    id: segments[1] || '',
    query: new URLSearchParams(queryText),
  };
}

const NAV = [
  { key: 'home', label: '推荐' },
  { key: 'favorites', label: '收藏' },
  { key: 'regions', label: '分区' },
  { key: 'pgc', label: '番剧' },
  { key: 'live', label: '直播' },
  { key: 'fish', label: '养鱼' },
  { key: 'dynamic', label: '动态' },
  { key: 'messages', label: '消息' },
];

export default function App() {
  const [route, setRoute] = useState<Route>(parseRoute);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [account, setAccount] = useState<Account | null>(null);
  const [bootError, setBootError] = useState('');
  const [notice, setNotice] = useState<any>(null);
  const [maximized, setMaximized] = useState(false);
  const [appVersion, setAppVersion] = useState('0.1.0');
  const [hud, setHud] = useState(false);
  const [hudStats, setHudStats] = useState({ fps: 0, ms: 0 });
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchDraft, setSearchDraft] = useState('');
  const [accountOpen, setAccountOpen] = useState(false);
  const [playlistOpen, setPlaylistOpen] = useState(false);
  const [holdRing, setHoldRing] = useState<{ x: number; y: number; progress: number; fired: boolean } | null>(null);
  const [playlistScroll, setPlaylistScroll] = useState(0);
  const [vinylCover, setVinylCover] = useState(() => localStorage.getItem('o11a1:vinyl-cover') || '');
  const searchInputRef = useRef<HTMLInputElement>(null);
  const accountRef = useRef<HTMLDivElement>(null);
  const searchWrapRef = useRef<HTMLDivElement>(null);
  const searchBtnRef = useRef<HTMLButtonElement>(null);
  const playlistRef = useRef<HTMLDivElement>(null);
  const vinylRef = useRef<HTMLButtonElement>(null);
  const holdRef = useRef<{ start: number; x: number; y: number; raf: number; timer: any; fired: boolean } | null>(null);
  const { toast, show } = useToast();

  useEffect(() => {
    const onChange = () => {
      setRoute(parseRoute());
      setSearchOpen(false);
      setAccountOpen(false);
      setPlaylistOpen(false);
      const page = document.querySelector('.page');
      if (page) page.scrollTop = 0;
    };
    window.addEventListener('hashchange', onChange);
    return () => window.removeEventListener('hashchange', onChange);
  }, []);

  useEffect(() => {
    const onCover = (event: Event) => setVinylCover((event as CustomEvent<string>).detail || '');
    window.addEventListener('o11a1:vinyl-cover', onCover);
    return () => window.removeEventListener('o11a1:vinyl-cover', onCover);
  }, []);

  useEffect(() => {
    if (searchOpen) {
      const timer = setTimeout(() => searchInputRef.current?.focus(), 30);
      return () => clearTimeout(timer);
    }
  }, [searchOpen]);

  useEffect(() => {
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (searchOpen && !searchWrapRef.current?.contains(target) && !searchBtnRef.current?.contains(target)) {
        setSearchOpen(false);
      }
      if (accountOpen && !accountRef.current?.contains(target)) {
        setAccountOpen(false);
      }
      if (playlistOpen && !playlistRef.current?.contains(target) && !vinylRef.current?.contains(target)) {
        setPlaylistOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      setSearchOpen(false);
      setAccountOpen(false);
      setPlaylistOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('keydown', onKey);
    };
  }, [searchOpen, accountOpen, playlistOpen]);

  useEffect(() => {
    let cancelled = false;
    let timer: any;
    const boot = async () => {
      try {
        const data = await call('app.bootstrap');
        if (cancelled) return;
        setSettings(data.settings);
        setAccount(data.account);
        setBootError('');
        if (data.sessionError) setTimeout(() => call('session.retry').catch(() => {}), 2500);
      } catch (error: any) {
        if (cancelled) return;
        setBootError(error?.message || String(error));
        timer = setTimeout(boot, 800);
      }
    };
    boot();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    window.o11a1.info().then((info: any) => setAppVersion(info.version)).catch(() => {});
    const off = window.o11a1.accountChanged?.((value: Account | null) => setAccount(value));
    const offHud = window.o11a1.onHudToggle?.(() => setHud((value) => !value));
    const offSettings = window.o11a1.settingsChanged?.((value: Settings) => setSettings(value));
    return () => { off?.(); offHud?.(); offSettings?.(); };
  }, []);

  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if ((event.key === 'F12') || (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'h')) {
        event.preventDefault();
        setHud((value) => !value);
      }
      if (event.key === '/' && !(event.target as HTMLElement)?.closest('input, textarea, select')) {
        event.preventDefault();
        setSearchOpen(true);
      }
    };
    window.addEventListener('keydown', listener);
    return () => window.removeEventListener('keydown', listener);
  }, []);

  useEffect(() => {
    if (!hud) return;
    let raf = 0;
    let frames = 0;
    let started = performance.now();
    let last = performance.now();
    let ms = 0;
    const tick = (now: number) => {
      frames += 1;
      ms = ms * 0.9 + (now - last) * 0.1;
      last = now;
      if (now - started >= 500) {
        setHudStats({ fps: frames / ((now - started) / 1000), ms });
        frames = 0;
        started = now;
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [hud]);

  useEffect(() => {
    if (!settings) return;
    const dark = settings.theme === 2;
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    const alpha = Math.max(0.58, 0.62 + (1 - Number(settings.glass_transparency || 0) / 100) * 0.36);
    const rgb = dark ? '38, 40, 46' : '255, 255, 255';
    document.documentElement.style.setProperty('--card', `rgba(${rgb}, ${alpha})`);
    document.documentElement.style.setProperty('--glass-blur', `${Math.max(0, Number(settings.glass_blur || 0)) * 0.18}px`);
    document.documentElement.style.setProperty('--wallpaper-blur', `${Math.max(0, Number(settings.backdrop_blur || 0))}px`);
  }, [settings?.theme, settings?.glass_transparency, settings?.glass_blur, settings?.backdrop_blur]);

  useEffect(() => {
    let timer: any;
    const touch = setTimeout(async () => {
      try {
        const result = await window.o11a1.update.check();
        if (result?.update) setNotice(result.update);
      } catch {
        /* offline is fine */
      }
    }, 4000);
    const off = window.o11a1.update.onResult?.((result: any) => {
      if (result?.update) setNotice(result.update);
      else if (result?.error) show(`检查更新失败：${result.error}`, true);
      else show('已经是最新版本');
    });
    return () => {
      clearTimeout(timer);
      off?.();
    };
  }, [show]);

  useEffect(() => {
    const off = window.o11a1.engineStatusChanged?.((status: any) => {
      if (status?.ok) setBootError('');
    });
    return () => off?.();
  }, []);

  const refreshAccount = useCallback(async () => {
    const data = await call('app.bootstrap');
    setAccount(data.account);
    setSettings(data.settings);
  }, []);

  const saveSettings = useCallback(async (patch: Partial<Settings>) => {
    if (!settings) return;
    const optimistic = { ...settings, ...patch } as Settings;
    setSettings(optimistic);
    try {
      const next = await call<Settings>('settings.patch', patch);
      setSettings(next);
      if ('autostart' in patch) {
        await window.o11a1.autostart.set(!!next.autostart);
      }
    } catch (error: any) {
      show(error?.message || '保存设置失败', true);
      setSettings(settings);
    }
  }, [settings, show]);

  const reloadSettings = useCallback(async () => {
    const next = await call<Settings>('settings.get');
    setSettings(next);
  }, []);

  const navigate = useCallback((to: string) => {
    const target = to.startsWith('#/') ? to : `#/${to.replace(/^#?\/?/, '')}`;
    if (window.location.hash === target) {
      setRoute(parseRoute());
    } else {
      window.location.hash = target;
    }
  }, []);

  const accountGo = useCallback((to: string) => {
    setAccountOpen(false);
    navigate(to);
  }, [navigate]);

  const logout = useCallback(async () => {
    setAccountOpen(false);
    if (!window.confirm('退出当前账号？')) return;
    try {
      await call('login.logout');
      await refreshAccount();
      show('已退出登录');
    } catch (error: any) {
      show(error?.message || String(error), true);
    }
  }, [refreshAccount, show]);

  useEffect(() => {
    const off = window.o11a1.app?.onNavigate?.((route: string) => {
      if (route) navigate(route);
    });
    return () => off?.();
  }, [navigate]);

  const launch = useCallback(async (options: Record<string, any>) => {
    if (options?.bvid) {
      await enqueuePlaylist({
        bvid: options.bvid,
        cid: options.cid || 0,
        aid: options.aid || 0,
        title: options.title || '',
        owner: options.owner || '',
        duration: options.duration ? String(options.duration) : '',
        cover: options.cover || '',
      }).catch(() => {});
    }
    await launchPlayer(options);
  }, []);

  const triggerRefresh = useCallback(() => {
    window.dispatchEvent(new CustomEvent('o11a1:refresh'));
  }, []);

  // Old Slint shell gesture: hold the right mouse button for 500 ms to
  // refresh the current page. Moving more than 16 px cancels; the stale-hold
  // guard is why a left press always aborts an unfinished ring (f246).
  useEffect(() => {
    const clear = () => {
      const current = holdRef.current;
      if (!current) return;
      cancelAnimationFrame(current.raf);
      clearTimeout(current.timer);
      holdRef.current = null;
      setHoldRing(null);
    };

    const start = (event: MouseEvent) => {
      if (event.button !== 2) return;
      if ((event.target as HTMLElement)?.closest('input, textarea, select, [contenteditable="true"]')) return;
      clear();
      const current = {
        start: performance.now(),
        x: event.clientX,
        y: event.clientY,
        raf: 0,
        timer: 0,
        fired: false,
      };
      holdRef.current = current;
      setHoldRing({ x: current.x, y: current.y, progress: 0, fired: false });

      const tick = (now: number) => {
        const active = holdRef.current;
        if (!active) return;
        const progress = Math.min(1, (now - active.start) / 500);
        if (progress >= 1) {
          if (!active.fired) {
            active.fired = true;
            setHoldRing({ x: active.x, y: active.y, progress: 1, fired: true });
            triggerRefresh();
            show('已刷新');
            active.timer = setTimeout(() => {
              if (holdRef.current === active) clear();
            }, 160);
          }
          return;
        }
        setHoldRing({ x: active.x, y: active.y, progress, fired: false });
        active.raf = requestAnimationFrame(tick);
      };
      current.raf = requestAnimationFrame(tick);
    };

    const move = (event: MouseEvent) => {
      const current = holdRef.current;
      if (!current || current.fired) return;
      if (Math.hypot(event.clientX - current.x, event.clientY - current.y) > 16) clear();
    };
    const release = (event: MouseEvent) => {
      if (event.button !== 2) return;
      if (holdRef.current && !holdRef.current.fired) clear();
    };
    const leftDown = (event: MouseEvent) => {
      if (event.button === 0) clear();
    };
    const noMenu = (event: MouseEvent) => event.preventDefault();
    const blur = () => clear();

    window.addEventListener('mousedown', start);
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', release);
    window.addEventListener('mousedown', leftDown);
    window.addEventListener('contextmenu', noMenu);
    window.addEventListener('blur', blur);
    return () => {
      window.removeEventListener('mousedown', start);
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', release);
      window.removeEventListener('mousedown', leftDown);
      window.removeEventListener('contextmenu', noMenu);
      window.removeEventListener('blur', blur);
      clear();
    };
  }, [show, triggerRefresh]);

  const openSearchMorph = useCallback(() => {
    setSearchDraft(route.name === 'search' ? (route.query.get('q') || '') : '');
    setPlaylistOpen(false);
    setSearchOpen(true);
  }, [route.name, route.query]);

  const submitSearch = useCallback(() => {
    const query = searchDraft.trim();
    setSearchOpen(false);
    navigate(query ? `search?q=${encodeURIComponent(query)}` : 'search');
  }, [searchDraft, navigate]);

  const navItems = useMemo(() => NAV.filter((item) => !(item.key === 'live' && settings?.hide_live_tab)), [settings?.hide_live_tab]);
  const navLeft = NAV.slice(0, 5).filter((item) => !(item.key === 'live' && settings?.hide_live_tab));
  const navRight = NAV.slice(5);
  const playlist = settings?.playlist || [];
  const playlistCurrent = settings?.playlist_current ?? null;
  const rows = Math.min(5, playlist.length);
  const playlistStart = Math.min(playlistScroll, Math.max(0, playlist.length - rows));
  const vinylArt = (playlistCurrent != null ? playlist[playlistCurrent]?.cover : '') || vinylCover;

  const playQueueIndex = useCallback(async (index: number) => {
    const entry = playlist[index];
    if (!entry) return;
    setPlaylistOpen(false);
    await saveSettings({ playlist_current: index });
    await launch({ bvid: entry.bvid, cid: entry.cid || null, aid: entry.aid, title: entry.title, cover: entry.cover, startMs: 0 });
  }, [playlist, saveSettings, launch]);

  const removeQueueIndex = useCallback(async (index: number) => {
    const next = playlist.filter((_: any, row: number) => row !== index);
    let currentIndex = playlistCurrent;
    if (currentIndex === index) currentIndex = null;
    else if (currentIndex != null && currentIndex > index) currentIndex -= 1;
    await saveSettings({ playlist: next, playlist_current: currentIndex });
  }, [playlist, playlistCurrent, saveSettings]);

  const clearQueue = useCallback(async () => {
    await saveSettings({ playlist: [], playlist_current: null });
    setPlaylistScroll(0);
    setPlaylistOpen(false);
  }, [saveSettings]);

  const stepQueue = useCallback((direction: number) => {
    if (!playlist.length) return;
    const base = playlistCurrent ?? (direction > 0 ? -1 : 0);
    let next = base + direction;
    if (next < 0) next = playlist.length - 1;
    if (next >= playlist.length) next = 0;
    playQueueIndex(next);
  }, [playlist, playlistCurrent, playQueueIndex]);

  if (!settings) {
    return (
      <div className="app" style={{ display: 'grid', placeItems: 'center' }}>
        <div style={{ textAlign: 'center', maxWidth: 460 }}>
          <div className="brand" style={{ fontSize: 30, marginBottom: 14 }}>O11A1<span>.</span></div>
          {bootError ? (
            <>
              <div style={{ color: 'var(--text-dim)', fontSize: 13, marginBottom: 16 }}>正在连接视频引擎…</div>
              <div style={{ color: 'var(--danger)', fontSize: 11, marginBottom: 18, wordBreak: 'break-all' }}>{bootError}</div>
              <button className="btn primary" onClick={() => window.location.reload()}>重试</button>
            </>
          ) : (
            <div className="spinner" />
          )}
        </div>
      </div>
    );
  }

  const activeNav = navItems.some((item) => item.key === route.name) ? route.name : '';

  const page = (() => {
    switch (route.name) {
      case 'search': return <Search />;
      case 'user': return <User mid={Number(route.id)} />;
      case 'favorites': return <Favorites />;
      case 'history': return <History />;
      case 'regions': return <Regions />;
      case 'pgc': return <Pgc />;
      case 'live': return <Live />;
      case 'dynamic': return <Dynamic />;
      case 'messages': return <Messages />;
      case 'member': return <Member />;
      case 'settings': return <SettingsPage />;
      case 'downloads': return <Downloads />;
      case 'fish': return <Fish />;
      default: return <Home />;
    }
  })();

  return (
    <AppContext.Provider value={{ settings, account, saveSettings, reloadSettings, navigate, route, toast: show, launch, refreshAccount, openSearch: openSearchMorph }}>
      <div className="app">
        <header className="topbar drag">
          <div className="account-area no-drag" ref={accountRef}>
            <button
              className={`account-trigger ${accountOpen ? 'open' : ''}`}
              title="账户"
              onClick={() => {
                setSearchOpen(false);
                setPlaylistOpen(false);
                setAccountOpen((open) => !open);
              }}
            >
              {account?.face
                ? <img className="account-avatar" src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
                : <div className="account-avatar placeholder" />}
              <div className="account-copy">
                <div className="account-name">{account?.uname || '未登录'}</div>
                <div className="account-uid">{account?.mid ? `UID: ${account.mid}` : '点击头像登录'}</div>
              </div>
            </button>

            <button className="top-gear" title="设置" onClick={() => accountGo('settings')}><IconGear size={17} /></button>

            {accountOpen ? (
              <div className="account-menu no-drag">
                <div className="account-menu-head">
                  {account?.face
                    ? <img src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
                    : <div className="account-avatar placeholder" />}
                  <div>
                    <div className="account-menu-name">{account?.uname || '未登录'}</div>
                    <div className="account-menu-uid">{account?.mid ? `UID: ${account.mid}` : '点击下方登录'}</div>
                  </div>
                </div>
                <div className="account-menu-rule" />
                <button className="account-menu-row" onClick={() => accountGo('member')}><IconStar size={15} />会员 / 账户</button>
                <button className="account-menu-row" onClick={() => accountGo('settings')}><IconGear size={15} />设置</button>
                <button className="account-menu-row" onClick={() => accountGo('history')}><IconClock size={15} />历史</button>
                <button className="account-menu-row" onClick={() => accountGo('downloads')}><IconDownload size={15} />下载</button>
                <button className="account-menu-row danger" disabled={!account} onClick={logout}><IconClose size={14} />退出登录</button>
              </div>
            ) : null}
          </div>
          <div className="spacer" />
          <div className="window-controls no-drag">
            <button className="icon-btn" title="最小化" onClick={() => window.o11a1.window.minimize()}><IconMin /></button>
            <button className="icon-btn" title={maximized ? '还原' : '最大化'} onClick={async () => setMaximized(await window.o11a1.window.maximize())}><IconMax /></button>
            <button
              className="icon-btn close"
              title={settings.close_action === 2 ? '退出程序' : '关闭到托盘'}
              onClick={() => {
                if (settings.close_action === 2) window.o11a1.window.quit();
                else window.o11a1.window.close();
              }}
            ><IconClose /></button>
          </div>
        </header>

        <main className="page">
          <div className="page-inner" key={`${route.name}-${route.id}`}>
            {notice ? (
              <div className="banner">
                <span>发现新版本 {notice.version}（当前 {appVersion}）</span>
                <span className="spacer" />
                <button className="btn sm primary" onClick={() => window.o11a1.update.open(notice.url)}>查看</button>
                <button className="btn sm ghost" onClick={() => setNotice(null)}>忽略</button>
              </div>
            ) : null}
            {page}
          </div>
        </main>

        <nav className={`bottom ${searchOpen ? 'searching' : ''}`}>
          {navLeft.map((item) => (
            <button
              key={item.key}
              className={`nav-pill ${activeNav === item.key ? 'active' : ''}`}
              onClick={() => navigate(item.key === 'home' ? 'home' : `${item.key}`)}
            >
              {item.label}
            </button>
          ))}

          <div className="vinyl-slot">
            <button
              ref={vinylRef}
              className={`vinyl-record ${playlistOpen ? 'open' : ''}`}
              title="播放列表"
              onClick={() => {
                setSearchOpen(false);
                setPlaylistOpen((open) => {
                  if (!open) {
                    const max = Math.max(0, playlist.length - 5);
                    setPlaylistScroll(Math.min(Math.max(0, (playlistCurrent ?? 0) - 2), max));
                  }
                  return !open;
                });
              }}
            >
              {vinylArt ? <img className="vinyl-art" src={cover(vinylArt)} referrerPolicy="no-referrer" alt="" /> : <span className="vinyl-fallback">O11A1</span>}
              <span className="vinyl-ring" />
              <span className="vinyl-hole" />
            </button>
          </div>

          {navRight.map((item) => (
            <button
              key={item.key}
              className={`nav-pill ${activeNav === item.key ? 'active' : ''}`}
              onClick={() => navigate(item.key === 'home' ? 'home' : `${item.key}`)}
            >
              {item.label}
            </button>
          ))}

          <button className="refresh-pill" onClick={triggerRefresh}>刷新</button>
          <button
            ref={searchBtnRef}
            className={`search-btn ${searchOpen ? 'active' : ''}`}
            title="搜索"
            onClick={() => {
              setPlaylistOpen(false);
              if (searchOpen) setSearchOpen(false);
              else openSearchMorph();
            }}
          ><IconSearch size={16} /></button>

          <div ref={searchWrapRef} className={`search-morph ${searchOpen ? 'open' : ''}`} aria-hidden={!searchOpen}>
            <IconSearch size={15} className="search-morph-icon" />
            <input
              ref={searchInputRef}
              tabIndex={searchOpen ? 0 : -1}
              value={searchDraft}
              placeholder="输入关键词，回车搜索 · Esc 关闭"
              onChange={(event) => setSearchDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') submitSearch();
                if (event.key === 'Escape') setSearchOpen(false);
              }}
            />
            <button tabIndex={searchOpen ? 0 : -1} title="关闭" onClick={() => setSearchOpen(false)}><IconClose size={12} /></button>
          </div>

          {playlistOpen ? (
            <div
              ref={playlistRef}
              className="playlist-popover"
              onWheel={(event) => {
                const max = Math.max(0, playlist.length - 5);
                if (max === 0) return;
                setPlaylistScroll((start) => Math.min(max, Math.max(0, start + (event.deltaY > 0 ? 1 : -1))));
              }}
            >
              <div className="queue-head">
                <strong>播放列表</strong>
                <span>{playlist.length} 项</span>
              </div>
              {rows === 0 ? <div className="queue-empty">播放列表为空</div> : null}
              <div className="queue-rows">
                {playlist.slice(playlistStart, playlistStart + rows).map((entry: any, index: number) => {
                  const actual = playlistStart + index;
                  const active = playlistCurrent === actual;
                  return (
                    <div
                      key={`${entry.bvid}-${entry.cid}-${actual}`}
                      className={`queue-row ${active ? 'active' : ''}`}
                      onClick={() => playQueueIndex(actual)}
                    >
                      <div className="queue-cover">
                        {entry.cover
                          ? <img src={cover(entry.cover)} referrerPolicy="no-referrer" alt="" />
                          : <span className="queue-cover-fallback" />}
                        {active ? <span className="queue-playing"><i /><i /><i /></span> : null}
                      </div>
                      <div className="queue-meta">
                        <div className="queue-title">{entry.title || '未命名视频'}</div>
                        <div className="queue-sub">{[entry.owner, entry.duration].filter(Boolean).join(' · ') || ' '}</div>
                      </div>
                      <button
                        className="queue-remove"
                        title="移除"
                        onClick={(event) => { event.stopPropagation(); removeQueueIndex(actual); }}
                      >×</button>
                    </div>
                  );
                })}
              </div>
              <div className="queue-foot">
                <button
                  className={`queue-switch ${settings.playlist_autoplay ? 'on' : ''}`}
                  onClick={() => saveSettings({ playlist_autoplay: !settings.playlist_autoplay })}
                >
                  <span>自动播放</span>
                  <i className="queue-switch-track"><i /></i>
                </button>
                <button
                  className={`queue-switch ${settings.playlist_random ? 'on' : ''}`}
                  onClick={() => saveSettings({ playlist_random: !settings.playlist_random })}
                >
                  <span>随机播放</span>
                  <i className="queue-switch-track"><i /></i>
                </button>
                <span className="queue-foot-spacer" />
                <button className="queue-step" title="上一首" onClick={() => stepQueue(-1)}>‹</button>
                <button className="queue-step" title="下一首" onClick={() => stepQueue(1)}>›</button>
                <button className="queue-clear" onClick={clearQueue}>清空</button>
              </div>
            </div>
          ) : null}
        </nav>

        {holdRing ? (
          <div
            className={`hold-ring ${holdRing.fired ? 'fired' : ''}`}
            style={{ left: holdRing.x, top: holdRing.y }}
          >
            <svg viewBox="0 0 36 36" aria-hidden="true">
              <circle className="track" cx="18" cy="18" r="15.5" />
              <circle
                className="fill"
                cx="18"
                cy="18"
                r="15.5"
                pathLength={100}
                strokeDasharray={100}
                strokeDashoffset={100 - holdRing.progress * 100}
              />
            </svg>
          </div>
        ) : null}

        {hud ? <div className="hud">FPS {hudStats.fps.toFixed(0)} · frame {hudStats.ms.toFixed(2)} ms</div> : null}
        {toast ? <div className={`toast ${toast.error ? 'error' : ''}`}>{toast.text}</div> : null}
      </div>
    </AppContext.Provider>
  );
}
