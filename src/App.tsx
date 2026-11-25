import React, { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { call, callRaw, cover } from './api';
import { AppContext, Route } from './context';
import { readLastAccount } from './components/LoginSurface';
import { useToast } from './hooks';
import type { Account, Settings } from './types';
import {
  IconActivity, IconBell, IconClock, IconClose, IconCompass, IconDownload, IconFish, IconGear,
  IconHome, IconMax, IconMin, IconRadio, IconSearch, IconStar, IconTv, IconUser,
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
import { launchPlayer } from './player';
import { CoverImage } from './components/CoverImage';
import { WindowResizeGrip } from './components/WindowResizeGrip';
import { clearPreloadedData } from './preload';

type HotSearchItem = { keyword: string; show_name: string };

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
  { key: 'home', label: '推荐', icon: IconHome, hint: '首页' },
  { key: 'favorites', label: '收藏', icon: IconStar, hint: '收藏' },
  { key: 'regions', label: '分区', icon: IconCompass, hint: '分区' },
  { key: 'pgc', label: '番剧', icon: IconTv, hint: '番剧' },
  { key: 'live', label: '直播', icon: IconRadio, hint: '直播' },
  { key: 'fish', label: 'Agent', icon: IconFish, hint: 'Agent' },
  { key: 'dynamic', label: '动态', icon: IconActivity, hint: '动态' },
  { key: 'messages', label: '消息', icon: IconBell, hint: '私信' },
] as const;

const LIBRARY = [
  { key: 'history', label: '历史', icon: IconClock },
  { key: 'downloads', label: '下载', icon: IconDownload },
  { key: 'member', label: '账户', icon: IconUser },
] as const;

export interface AppInitial {
  settings: Settings;
  account: Account | null;
  version?: string;
}

export default function App({ initial }: { initial?: AppInitial }) {
  const [route, setRoute] = useState<Route>(parseRoute);
  const [settings, setSettings] = useState<Settings | null>(initial?.settings ?? null);
  const [account, setAccount] = useState<Account | null>(initial?.account ?? null);
  // Signed out? Keep showing the user's own portrait instead of an empty disc,
  // so the account button never reads as an application icon.
  const lastAccount = useMemo(() => (account ? null : readLastAccount()), [account]);
  const avatarFace = account?.face || lastAccount?.face || '';
  const [bootError, setBootError] = useState('');
  const [notice, setNotice] = useState<any>(null);
  const [updateDownload, setUpdateDownload] = useState<any>(null);
  const [maximized, setMaximized] = useState(false);
  const [appVersion, setAppVersion] = useState(initial?.version || '0.1.0');
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchDraft, setSearchDraft] = useState('');
  const [searchSuggestions, setSearchSuggestions] = useState<string[]>([]);
  const [searchSuggestLoading, setSearchSuggestLoading] = useState(false);
  const [searchSuggestion, setSearchSuggestion] = useState(-1);
  const [searchHot, setSearchHot] = useState<HotSearchItem[]>([]);
  const [searchHotLoading, setSearchHotLoading] = useState(false);
  const searchSuggestSeq = useRef(0);
  const searchHotCache = useRef<HotSearchItem[]>([]);
  const [accountOpen, setAccountOpen] = useState(false);
  const [playlistOpen, setPlaylistOpen] = useState(false);
  const appRef = useRef<HTMLDivElement>(null);
  const [playlistScroll, setPlaylistScroll] = useState(0);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const accountRef = useRef<HTMLDivElement>(null);
  const searchWrapRef = useRef<HTMLDivElement>(null);
  const searchBtnRef = useRef<HTMLButtonElement>(null);
  const playlistRef = useRef<HTMLDivElement>(null);
  const vinylRef = useRef<HTMLButtonElement>(null);
  const bottomBarRef = useRef<HTMLDivElement>(null);
  const [bottomPill, setBottomPill] = useState({ left: 0, top: 8, size: 42, ready: false });
  const { toast, show } = useToast();

  // Windowed mode keeps the 22px transparent rim for the paper shadow. The
  // maximized window must drop it and fill the work area.
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

  // Remember who signed in here so the sign-in screen can greet them with
  // their own avatar and name after `login.logout` clears the saved session.
  useEffect(() => {
    if (!account?.mid) return;
    try {
      localStorage.setItem('o11a1:last-account', JSON.stringify({
        mid: account.mid,
        uname: account.uname,
        face: account.face,
        vip: account.vip,
      }));
    } catch {
      /* local storage is optional */
    }
  }, [account]);

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
    if (searchOpen) {
      const timer = setTimeout(() => searchInputRef.current?.focus(), 30);
      return () => clearTimeout(timer);
    }
  }, [searchOpen]);

  // Search-box suggestions: one debounced request per edit, with a generation
  // guard so a slow earlier answer can never overwrite the current term.
  useEffect(() => {
    const term = searchDraft.trim();
    if (!searchOpen || !term) {
      searchSuggestSeq.current += 1;
      setSearchSuggestions([]);
      setSearchSuggestLoading(false);
      setSearchSuggestion(-1);
      return;
    }
    const seq = ++searchSuggestSeq.current;
    setSearchSuggestions([]);
    setSearchSuggestLoading(true);
    setSearchSuggestion(-1);
    const timer = setTimeout(async () => {
      try {
        const items = await callRaw<string[]>('search.suggest', { term });
        if (seq !== searchSuggestSeq.current) return;
        setSearchSuggestions(
          Array.isArray(items)
            ? items.filter((item) => typeof item === 'string' && item.trim()).slice(0, 8)
            : [],
        );
      } catch {
        if (seq === searchSuggestSeq.current) setSearchSuggestions([]);
      } finally {
        if (seq === searchSuggestSeq.current) setSearchSuggestLoading(false);
      }
    }, 160);
    return () => clearTimeout(timer);
  }, [searchOpen, searchDraft]);

  // Empty search box: the same dropdown becomes the hot-search list instead
  // of disappearing. Cached for the app session so clearing the input is
  // instant; the first open fetches it once.
  useEffect(() => {
    if (!searchOpen) {
      setSearchHotLoading(false);
      return;
    }
    if (searchDraft.trim()) {
      setSearchHotLoading(false);
      return;
    }
    if (searchHotCache.current.length) {
      setSearchHot(searchHotCache.current);
      setSearchHotLoading(false);
      setSearchSuggestion(-1);
      return;
    }
    let cancelled = false;
    setSearchHotLoading(true);
    const timer = setTimeout(async () => {
      try {
        // The startup preloader already warms `search.hot`; use that page-cache
        // entry first and only hit the WBI endpoint when it is missing/empty.
        let items = await call<HotSearchItem[]>('search.hot').catch(() => [] as HotSearchItem[]);
        if (!items.length) {
          items = await callRaw<HotSearchItem[]>('search.hot').catch(() => [] as HotSearchItem[]);
        }
        if (cancelled) return;
        const list = Array.isArray(items)
          ? items.filter((item) => item && (item.keyword || item.show_name)).slice(0, 10)
          : [];
        searchHotCache.current = list;
        setSearchHot(list);
        setSearchSuggestion(-1);
      } catch {
        if (!cancelled) setSearchHot([]);
      } finally {
        if (!cancelled) setSearchHotLoading(false);
      }
    }, 120);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [searchOpen, searchDraft]);

  useEffect(() => {
    const onDown = (event: MouseEvent) => {
      // While the search dock is open it owns the right button: a right press
      // anywhere closes it immediately.
      if (event.button === 2 && searchOpen) {
        event.preventDefault();
        event.stopPropagation();
        setSearchOpen(false);
        return;
      }
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
    if (initial) return;
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
  }, [initial]);

  useEffect(() => {
    window.o11a1.info().then((info: any) => setAppVersion(info.version)).catch(() => {});
    const off = window.o11a1.accountChanged?.((value: Account | null) => setAccount(value));
    const offSettings = window.o11a1.settingsChanged?.((value: Settings) => setSettings(value));
    return () => { off?.(); offSettings?.(); };
  }, []);

  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if (event.key === '/' && !(event.target as HTMLElement)?.closest('input, textarea, select')) {
        event.preventDefault();
        setSearchOpen(true);
      }
    };
    window.addEventListener('keydown', listener);
    return () => window.removeEventListener('keydown', listener);
  }, []);

  useEffect(() => {
    if (!settings) return;
    const dark = settings.theme === 2;
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    const alpha = Math.max(0.58, 0.62 + (1 - Number(settings.glass_transparency || 0) / 100) * 0.36);
    const rgb = dark ? '38, 40, 46' : '255, 252, 246';
    document.documentElement.style.setProperty('--card', `rgba(${rgb}, ${alpha})`);
    document.documentElement.style.setProperty('--glass-blur', `${Math.max(0, Number(settings.glass_blur || 0)) * 0.18}px`);
    document.documentElement.style.setProperty('--wallpaper-blur', `${Math.max(0, Number(settings.backdrop_blur || 0))}px`);
  }, [settings?.theme, settings?.glass_transparency, settings?.glass_blur, settings?.backdrop_blur]);

  useEffect(() => {
    let timer: any;
    const touch = setTimeout(async () => {
      try {
        const result = await window.o11a1.update.check();
        if (result?.update) {
          setNotice(result.update);
          if (result.download) setUpdateDownload(result.download);
        }
      } catch {
        /* offline is fine */
      }
    }, 4000);
    const off = window.o11a1.update.onResult?.((result: any) => {
      if (result?.update) {
        setNotice(result.update);
        if (result.download) setUpdateDownload(result.download);
      } else if (result?.error) show(`检查更新失败：${result.error}`, true);
      else show('已经是最新版本');
    });
    // The shell downloads the installer in the background and reports its
    // progress here; the banner turns into the restart-install action.
    const offProgress = window.o11a1.update.onProgress?.((state: any) => setUpdateDownload(state));
    const offReady = window.o11a1.update.onReady?.((state: any) => setUpdateDownload(state));
    return () => {
      clearTimeout(timer);
      off?.();
      offProgress?.();
      offReady?.();
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
      clearPreloadedData();
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

  const launch = useCallback(async (options: Record<string, any>, newWindow = false) => {
    // The native shell owns the queue update: a left-click replaces the
    // focused window's row (delete old, insert new), a right-click enqueues a
    // new row. Doing it here too would fight that bookkeeping.
    // Every card/row click here is an explicit open, so it syncs history.
    await launchPlayer({ ...options, reportHistory: true }, newWindow);
  }, []);

  const dockGo = useCallback((key: string) => {
    setSearchOpen(false);
    setPlaylistOpen(false);
    setAccountOpen(false);
    navigate(key);
  }, [navigate]);

  // Ctrl+V on the shell opens a Bilibili share directly. Inputs keep their
  // normal paste behavior (search box, settings fields); a share text can use a
  // full video URL, a b23.tv / bili2233.cn short link, or a bare BV/av id.
  useEffect(() => {
    const onPaste = (event: ClipboardEvent) => {
      if (searchOpen) return;
      const target = event.target as HTMLElement | null;
      if (target instanceof Element && target.closest('input, textarea, select, [contenteditable="true"]')) return;
      const text = event.clipboardData?.getData('text') || '';
      if (!text.trim()) return;
      const hasVideo = /b23\.tv|bili2233\.cn|bilibili\.com\/video|BV[0-9A-Za-z]{10}/i.test(text)
        || /^\s*av\d+\s*$/i.test(text)
        || (/(?:bilibili|B站|哔哩)/i.test(text) && /\bav\d+\b/i.test(text));
      if (!hasVideo) return;
      event.preventDefault();
      show('正在打开分享视频…');
      window.o11a1.share?.open(text).then((result: any) => {
        if (result?.ok) show('已打开分享视频');
        else show(result?.error || '没有识别到视频', true);
      }).catch((error: any) => show(error?.message || '打开分享视频失败', true));
    };
    window.addEventListener('paste', onPaste);
    return () => window.removeEventListener('paste', onPaste);
  }, [searchOpen, show]);

  // The top and bottom chrome keep their live backdrop blur while the page
  // scrolls; the glass samples the moving content every frame instead of
  // switching to an opaque surface.



  const openSearchMorph = useCallback(() => {
    // Every open starts from an empty field, so hot searches are visible and
    // the previous query never stays behind after a submitted search.
    setSearchDraft('');
    setSearchSuggestions([]);
    setSearchSuggestion(-1);
    setSearchSuggestLoading(false);
    setPlaylistOpen(false);
    setSearchOpen(true);
  }, []);

  const submitSearch = useCallback((term?: string) => {
    const query = (term ?? searchDraft).trim();
    // An empty query is not a search: leave the dock and its hot list alone.
    if (!query) return;
    searchSuggestSeq.current += 1;
    setSearchDraft('');
    setSearchSuggestions([]);
    setSearchSuggestLoading(false);
    setSearchSuggestion(-1);
    setSearchOpen(false);
    navigate(`search?q=${encodeURIComponent(query)}`);
  }, [searchDraft, navigate]);

  const highlightSuggestion = (text: string) => {
    const query = searchDraft.trim();
    const at = query ? text.toLowerCase().indexOf(query.toLowerCase()) : -1;
    if (at < 0) return text;
    return (
      <>
        {text.slice(0, at)}
        <b>{text.slice(at, at + query.length)}</b>
        {text.slice(at + query.length)}
      </>
    );
  };

  type DockItem = { key: string; label: string; icon: React.ComponentType<{ size?: number }>; hint?: string };
  const dock = useMemo(() => {
    // Agent lives in the blue end-cap, not in the ordinary nav row.
    const items: DockItem[] = [...NAV, LIBRARY[0]].filter((item) => !(
      item.key === 'fish'
      || (item.key === 'live' && settings?.hide_live_tab)
      || (item.key === 'pgc' && settings?.hide_pgc_tab)
    ));
    // Keep the vinyl optically centred. The blue Agent cap replaces the old
    // refresh button and spans one or two slots depending on how many ordinary
    // icons actually remain after the two optional tabs are hidden.
    const showFish = !settings?.hide_fish_tab;
    const wideFish = showFish && items.length % 2 === 0;
    const fishSlots = showFish ? (wideFish ? 2 : 1) : 0;
    const slots = items.length + fishSlots;
    const leftCount = Math.floor(slots / 2);
    return {
      items,
      left: items.slice(0, leftCount),
      right: items.slice(leftCount),
      wideFish,
      showFish,
    };
  }, [settings?.hide_live_tab, settings?.hide_pgc_tab, settings?.hide_fish_tab]);
  const playlist = settings?.playlist || [];
  const playlistCurrent = settings?.playlist_current ?? null;
  const rows = Math.min(7, playlist.length);
  const playlistStart = Math.min(playlistScroll, Math.max(0, playlist.length - rows));
  const vinylArt = playlistCurrent != null ? playlist[playlistCurrent]?.cover || '' : '';

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

  // The active pill is one shared element, so switching tabs slides it under
  // the vinyl instead of swapping two unrelated backgrounds.
  useLayoutEffect(() => {
    const bar = bottomBarRef.current;
    if (!bar) return;
    const measure = () => {
      const active = bar.querySelector<HTMLButtonElement>('.bottom-item.active:not(.fish)');
      if (!active) {
        setBottomPill((pill) => (pill.ready ? { ...pill, ready: false } : pill));
        return;
      }
      // Use layout offsets, not getBoundingClientRect: the bar's entrance
      // animation scales it to 0.96, which would leave the pill permanently
      // 4% too small and a fraction of a pixel off-centre.
      const nav = active.closest<HTMLElement>('.bottom-nav');
      setBottomPill({
        left: (nav?.offsetLeft || 0) + active.offsetLeft,
        top: (nav?.offsetTop || 0) + active.offsetTop,
        size: active.offsetWidth,
        ready: true,
      });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(bar);
    const timer = window.setTimeout(measure, 80);
    window.addEventListener('resize', measure);
    return () => {
      observer.disconnect();
      window.clearTimeout(timer);
      window.removeEventListener('resize', measure);
    };
  }, [route.name, dock]);

  // Optical centering: several icon paths (TV antenna, fish tail) do not put
  // their ink at 12,12 inside the 24px viewBox. Shift each
  // SVG by the measured bbox delta so the visible strokes, not just the box,
  // sit in the exact centre of the round button.
  useLayoutEffect(() => {
    const bar = bottomBarRef.current;
    if (!bar) return;
    const center = () => {
      bar.querySelectorAll<SVGSVGElement>('.bottom-icon svg').forEach((svg) => {
        const box = svg.getBBox();
        const width = svg.getBoundingClientRect().width;
        if (!box.width || !box.height || !width) return;
        const scale = width / 24;
        const dx = (12 - (box.x + box.width / 2)) * scale;
        const dy = (12 - (box.y + box.height / 2)) * scale;
        svg.style.transform = `translate(${dx}px, ${dy}px)`;
      });
    };
    center();
    const timer = window.setTimeout(center, 60);
    return () => window.clearTimeout(timer);
  }, [route.name, dock]);

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

  const activeNav = dock.items.some((item) => item.key === route.name) ? route.name : '';
  const fishActive = dock.showFish && route.name === 'fish';
  const currentEntry = playlistCurrent != null ? playlist[playlistCurrent] : null;
  const searchTerm = searchDraft.trim();
  const searchPanelItems: Array<{ value: string; label: string; rank?: number }> = searchTerm
    ? searchSuggestions.map((item) => ({ value: item, label: item }))
    : searchHot.map((item, index) => ({
        value: item.keyword || item.show_name,
        label: item.show_name || item.keyword,
        rank: index + 1,
      }));
  const showSearchPanel = searchOpen && (
    searchTerm
      ? (searchSuggestLoading || searchSuggestions.length > 0)
      : (searchHotLoading || searchHot.length > 0)
  );

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
      <div className={`app${maximized ? ' maximized' : ''}`} ref={appRef}>

        <div className="workspace">
          <header className="topbar">
            <div className="topbar-account no-drag" ref={accountRef}>
              <button
                className={`topbar-avatar ${accountOpen ? 'open' : ''}`}
                title={
                  account
                    ? `${account.uname} · UID ${account.mid}`
                    : lastAccount
                      ? `上次登录 · ${lastAccount.uname}`
                      : '点击头像登录'
                }
                onClick={() => {
                  setSearchOpen(false);
                  setPlaylistOpen(false);
                  setAccountOpen((open) => !open);
                }}
              >
                {avatarFace
                  ? <img className="account-avatar" src={cover(avatarFace)} referrerPolicy="no-referrer" alt="" />
                  : <div className="account-avatar placeholder" />}
              </button>

              {accountOpen ? (
                <div className="account-menu topbar-account-menu">
                  <div className="account-menu-head">
                    {avatarFace
                      ? <img src={cover(avatarFace)} referrerPolicy="no-referrer" alt="" />
                      : <div className="account-avatar placeholder" />}
                    <div>
                      <div className="account-menu-name">
                        {account?.uname || (lastAccount ? `上次登录 · ${lastAccount.uname}` : '未登录')}
                      </div>
                      <div className="account-menu-uid">
                        {account?.mid
                          ? `UID ${account.mid}`
                          : lastAccount
                            ? `UID ${lastAccount.mid}`
                            : '点击下方登录'}
                      </div>
                    </div>
                  </div>
                  <div className="account-menu-rule" />
                  <button className="account-menu-row" onClick={() => accountGo('member')}><IconStar size={15} />会员 / 账户</button>
                  <button className="account-menu-row" onClick={() => accountGo('settings')}><IconGear size={15} />设置</button>
                  <button className="account-menu-row danger" disabled={!account} onClick={logout}><IconClose size={14} />退出登录</button>
                </div>
              ) : null}
            </div>

            <div ref={searchWrapRef} className={`search-area no-drag ${searchOpen ? 'open' : ''}`}>
              <div
                className={`search-dock ${searchOpen ? 'open' : ''}`}
                onClick={() => {
                  setPlaylistOpen(false);
                  if (!searchOpen) openSearchMorph();
                  else searchInputRef.current?.focus();
                }}
              >
                <button
                  ref={searchBtnRef}
                  className="search-toggle"
                  title="搜索（/）"
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    setPlaylistOpen(false);
                    if (!searchOpen) openSearchMorph();
                    else searchInputRef.current?.focus();
                  }}
                ><IconSearch size={16} /></button>
                <span className="search-placeholder">搜索视频或 UP 主</span>
                <input
                  ref={searchInputRef}
                  tabIndex={searchOpen ? 0 : -1}
                  placeholder="搜索视频或 UP 主"
                  value={searchDraft}
                  onChange={(event) => {
                    setSearchDraft(event.target.value);
                    setSearchSuggestion(-1);
                  }}
                  onKeyDown={(event) => {
                    if ((event.nativeEvent as any).isComposing) return;
                    if (event.key === 'ArrowDown') {
                      event.preventDefault();
                      setSearchSuggestion((current) => (
                        searchPanelItems.length
                          ? (current >= searchPanelItems.length - 1 ? 0 : current + 1)
                          : -1
                      ));
                    } else if (event.key === 'ArrowUp') {
                      event.preventDefault();
                      setSearchSuggestion((current) => (
                        searchPanelItems.length
                          ? (current <= 0 ? searchPanelItems.length - 1 : current - 1)
                          : -1
                      ));
                    } else if (event.key === 'Enter') {
                      event.preventDefault();
                      const picked = searchSuggestion >= 0 ? searchPanelItems[searchSuggestion]?.value : '';
                      submitSearch(picked || undefined);
                    } else if (event.key === 'Escape') {
                      setSearchOpen(false);
                    }
                  }}
                />
                <button
                  className="search-close"
                  tabIndex={searchOpen ? 0 : -1}
                  title="关闭"
                  onClick={(event) => { event.stopPropagation(); setSearchOpen(false); }}
                ><IconClose size={12} /></button>
              </div>
              {showSearchPanel ? (
                <div
                  className="search-suggest-panel"
                  role="listbox"
                  aria-label={searchTerm ? '搜索建议' : '热搜'}
                >
                  {(searchTerm ? searchSuggestLoading && searchSuggestions.length === 0 : searchHotLoading && searchHot.length === 0) ? (
                    <div className="search-suggest-status">
                      <i />{searchTerm ? '正在联想…' : '正在加载热搜…'}
                    </div>
                  ) : (
                    searchPanelItems.map((item, index) => (
                      <button
                        key={`${item.value}-${index}`}
                        className={`search-suggest-item${index === searchSuggestion ? ' active' : ''}`}
                        role="option"
                        aria-selected={index === searchSuggestion}
                        onMouseDown={(event) => event.preventDefault()}
                        onMouseEnter={() => setSearchSuggestion(index)}
                        onClick={() => submitSearch(item.value)}
                      >
                        {item.rank ? (
                          <span className={`search-suggest-rank${item.rank <= 3 ? ' top' : ''}`}>{item.rank}</span>
                        ) : (
                          <IconSearch size={14} />
                        )}
                        <span>{searchTerm ? highlightSuggestion(item.label) : item.label}</span>
                      </button>
                    ))
                  )}
                </div>
              ) : null}
            </div>

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

          <main className={`page${route.name === 'history' ? ' page-history' : ''}`}>
            <div className="page-inner" key={`${route.name}-${route.id}`}>
              {notice ? (
                <div className="banner">
                  <span>
                    发现新版本 {notice.version}（当前 {appVersion}）
                    {updateDownload?.version === notice.version && updateDownload.status === 'downloading'
                      ? ` · 正在后台下载 ${Math.round(updateDownload.percent || 0)}%`
                      : ''}
                    {updateDownload?.version === notice.version && updateDownload.status === 'ready'
                      ? ' · 安装包已就绪'
                      : ''}
                    {updateDownload?.version === notice.version && updateDownload.status === 'error'
                      ? ' · 下载失败'
                      : ''}
                  </span>
                  <span className="spacer" />
                  {updateDownload?.version === notice.version && updateDownload.status === 'ready' ? (
                    <button className="btn sm primary" onClick={() => window.o11a1.update.install()}>重启安装</button>
                  ) : (
                    <button className="btn sm primary" onClick={() => window.o11a1.update.open(notice.url)}>查看</button>
                  )}
                  <button className="btn sm ghost" onClick={() => setNotice(null)}>忽略</button>
                </div>
              ) : null}
              {page}
            </div>
          </main>

        <div className="dock">
          {playlistOpen ? (
            <div
              ref={playlistRef}
              className="playlist-popover"
              onWheel={(event) => {
                const max = Math.max(0, playlist.length - 7);
                if (max === 0) return;
                setPlaylistScroll((start) => Math.min(max, Math.max(0, start + (event.deltaY > 0 ? 1 : -1))));
              }}
            >
              <div className="queue-head">
                <strong>播放队列</strong>
              </div>
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
                          ? <CoverImage src={entry.cover} alt="" />
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
              {playlist.length ? (
                <div className="queue-foot">
                  <button className="queue-clear" onClick={clearQueue}>清空</button>
                </div>
              ) : null}
            </div>
          ) : null}

          <div className="bottom-bar no-drag" ref={bottomBarRef}>
            <span
              className={`bottom-pill ${bottomPill.ready ? 'ready' : ''}`}
              style={{
                transform: `translate(${bottomPill.left}px, ${bottomPill.top}px)`,
                width: bottomPill.size,
                height: bottomPill.size,
              }}
            />
            <nav className="bottom-nav">
              {dock.left.map((item) => {
                const Icon = item.icon;
                return (
                  <button
                    key={item.key}
                    className={`bottom-item ${activeNav === item.key ? 'active' : ''}`}
                    data-tip={item.hint || item.label}
                    aria-label={item.hint || item.label}
                    onClick={() => dockGo(item.key)}
                  >
                    <span className="bottom-icon"><Icon size={18} /></span>
                  </button>
                );
              })}
            </nav>

            <button
              ref={vinylRef}
              className={`bottom-disc ${playlistOpen ? 'open' : ''} ${currentEntry ? 'playing' : ''}`}
              data-tip="播放队列"
              aria-label="播放队列"
              onClick={() => {
                setSearchOpen(false);
                setAccountOpen(false);
                setPlaylistOpen((open) => {
                  if (!open) {
                    const max = Math.max(0, playlist.length - 7);
                    setPlaylistScroll(Math.min(Math.max(0, (playlistCurrent ?? 0) - 2), max));
                  }
                  return !open;
                });
              }}
            >
              <span className="vinyl-disc">
                {vinylArt ? <img className="vinyl-art" src={cover(vinylArt)} referrerPolicy="no-referrer" alt="" /> : null}
                <span className="vinyl-ring" />
                <span className="vinyl-hole" />
              </span>
            </button>

            <nav className="bottom-nav">
              {dock.right.map((item) => {
                const Icon = item.icon;
                return (
                  <button
                    key={item.key}
                    className={`bottom-item ${activeNav === item.key ? 'active' : ''}`}
                    data-tip={item.hint || item.label}
                    aria-label={item.hint || item.label}
                    onClick={() => dockGo(item.key)}
                  >
                    <span className="bottom-icon"><Icon size={18} /></span>
                  </button>
                );
              })}
              {dock.showFish ? (
                <button
                  className={`bottom-item fish ${dock.wideFish ? 'wide' : ''} ${fishActive ? 'active' : ''}`}
                  data-tip="Agent"
                  aria-label="Agent"
                  onClick={() => dockGo('fish')}
                >
                  <span className="bottom-icon"><IconFish size={18} /></span>
                </button>
              ) : null}
            </nav>
          </div>
        </div>

        </div>

        {toast ? <div className={`toast ${toast.error ? 'error' : ''}`}>{toast.text}</div> : null}
      </div>
      <WindowResizeGrip hidden={maximized} />
    </AppContext.Provider>
  );
}
