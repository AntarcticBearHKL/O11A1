import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { call, cover, formatCount, formatDate, formatDuration, formatTime, relativeTime } from './api';
import { addToPlaylist, enqueuePlaylist, resolvePgcSource } from './player';
import { useToast } from './hooks';
import type { Comment, CommentPage, FeedVideo, Settings, VideoDetail } from './types';
import {
  IconDanmaku, IconDown, IconDownload, IconExitFull, IconFull, IconHeart, IconList,
  IconMax, IconMin, IconMute, IconNext, IconPause, IconPanel, IconPlay, IconPrev,
  IconSettings, IconClose, IconVolume, IconBack, IconCoin, IconStar,
} from './components/Icons';

const SIDE_TABS = ['信息', '选集', '评论', '历史', '性能', '设置'];
const QUALITY_LABEL: Record<number, string> = { 127: '8K', 126: '杜比视界', 125: 'HDR', 120: '4K', 116: '1080P60', 80: '1080P', 74: '720P60', 64: '720P', 32: '480P', 16: '360P' };

const DEFAULT_KEYS = [0x20, 0x25, 0x27, 0x26, 0x28, 0x4d, 0x46, 0x1b, 0x44, 0x4c, 0x53];

function keyFor(event: KeyboardEvent): number {
  return event.keyCode || event.which;
}

function effectiveKeys(stored: number[] | undefined): number[] {
  return DEFAULT_KEYS.map((fallback, index) => (stored?.[index] && stored[index] >= 0x20 ? stored[index] : fallback));
}

export default function PlayerApp() {
  useEffect(() => {
    document.documentElement.classList.add('player-mode');
    return () => document.documentElement.classList.remove('player-mode');
  }, []);
  const { toast, show } = useToast();
  const [options, setOptions] = useState<any>(null);
  const [state, setState] = useState<any>({ open: false, position: 0, duration: 0, playing: false, fps: 0 });
  const [settings, setSettings] = useState<Settings | null>(null);
  const [detail, setDetail] = useState<VideoDetail | null>(null);
  const activeCid = Number(options?.cid) || Number(detail?.cid) || 0;
  const [related, setRelated] = useState<FeedVideo[]>([]);
  const [comments, setComments] = useState<Comment[]>([]);
  const [commentTop, setCommentTop] = useState<Comment | null>(null);
  const [expandedReplies, setExpandedReplies] = useState<Record<number, Comment[]>>({});
  const [commentCursor, setCommentCursor] = useState('');
  const [commentEnd, setCommentEnd] = useState(false);
  const [commentHot, setCommentHot] = useState(true);
  const [commentsLoading, setCommentsLoading] = useState(false);
  const [descOpen, setDescOpen] = useState(false);
  const [detailError, setDetailError] = useState('');
  const [folders, setFolders] = useState<Array<{ id: number; name: string; count: number }> | null>(null);
  const [showFolders, setShowFolders] = useState(false);
  const [followed, setFollowed] = useState(false);
  const [side, setSide] = useState(0);
  const [fullscreen, setFullscreen] = useState(false);
  const [fullscreenChrome, setFullscreenChrome] = useState(false);
  const [mini, setMini] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [showQuality, setShowQuality] = useState(false);
  const [showPlaylist, setShowPlaylist] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [showDanmakuSettings, setShowDanmakuSettings] = useState(false);
  const [qualities, setQualities] = useState<any[]>([]);
  const [audioTracks, setAudioTracks] = useState<any[]>([]);
  const [volume, setVolume] = useState(80);
  const [muted, setMuted] = useState(false);
  const [danmakuOn, setDanmakuOn] = useState(true);
  const [danmakuDraft, setDanmakuDraft] = useState('');
  const [sleepMinutes, setSleepMinutes] = useState(0);
  const [history, setHistory] = useState<any[]>([]);
  const [danmakuDates, setDanmakuDates] = useState<string[]>([]);
  const [danmakuHistory, setDanmakuHistory] = useState<any[]>([]);
  const [historyDate, setHistoryDate] = useState('');
  const [historyLoading, setHistoryLoading] = useState(false);
  const [duration, setDuration] = useState(0);
  const [openError, setOpenError] = useState('');
  const [position, setPosition] = useState(0);
  const [scrubbing, setScrubbing] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [hoverTime, setHoverTime] = useState<number | null>(null);
  const [dmMenu, setDmMenu] = useState<any>(null);
  const [undoBlock, setUndoBlock] = useState<{ previous: string[]; text: string } | null>(null);
  const [showControls, setShowControls] = useState(true);
  const [danmakuStats, setDanmakuStats] = useState(0);
  const videoRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<{ x: number; y: number; moved: boolean; pointerId: number } | null>(null);
  const progressRef = useRef<HTMLDivElement>(null);
  const stateRef = useRef(state);
  const openRef = useRef(false);
  const clickTimer = useRef<any>(null);
  const hideTimer = useRef<any>(null);
  const sleepTimer = useRef<any>(null);
  const lastPreview = useRef(0);
  const optionsRef = useRef<any>(null);
  const hydrated = useRef(false);
  const togglePlayRef = useRef<() => void>(() => {});
  const toggleFullscreenRef = useRef<() => void>(() => {});
  const exitMiniRef = useRef<() => void>(() => {});
  const miniRef = useRef(false);
  const fullscreenRef = useRef(false);
  const chromeTimer = useRef<any>(null);
  const undoTimer = useRef<any>(null);

  stateRef.current = state;
  optionsRef.current = options;

  const saveSettings = useCallback(async (patch: Partial<Settings>) => {
    try {
      const next = await call<Settings>('settings.patch', patch);
      setSettings(next);
      return next;
    } catch (error: any) {
      show(error.message, true);
      return null;
    }
  }, [show]);

  const sendRect = useCallback(() => {
    const element = videoRef.current;
    if (!element) return;
    const rect = element.getBoundingClientRect();
    if (rect.width < 4 || rect.height < 4) return;
    window.o11a1.player.rect({ x: rect.left, y: rect.top, width: rect.width, height: rect.height }).catch(() => {});
  }, []);

  const openPlayer = useCallback(async (nextOptions: any, replace = false) => {
    const merged = replace
      ? { ...nextOptions }
      : { ...(optionsRef.current || {}), ...nextOptions };
    if (!replace && merged.bvid) {
      await enqueuePlaylist({
        bvid: merged.bvid,
        cid: merged.cid || 0,
        aid: merged.aid || 0,
        title: merged.title || '',
        owner: merged.owner || '',
        duration: merged.duration ? String(merged.duration) : '',
        cover: merged.cover || '',
      }).catch(() => {});
    }
    setOptions(merged);
    optionsRef.current = merged;
    try {
      await window.o11a1.player.open(merged);
      openRef.current = true;
      setOpenError('');
      setDuration(Number(merged.startMs || 0));
      setPosition(Number(merged.startMs || 0));
      sendRect();
    } catch (error: any) {
      setOpenError(error.message || '播放器打开失败');
      show(error.message || '播放器打开失败', true);
    }
  }, [sendRect, show]);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const [loadedOptions, loadedSettings] = await Promise.all([
        window.o11a1.player.options(),
        call<Settings>('settings.get'),
      ]);
      if (cancelled) return;
      setSettings(loadedSettings);
      setOptions(loadedOptions);
      optionsRef.current = loadedOptions;
      setVolume(loadedSettings.player_speed ? 80 : 80);
      setDanmakuOn(true);
      requestAnimationFrame(async () => {
        sendRect();
        await openPlayer(loadedOptions, true);
      });
    })().catch((error) => show(error.message, true));
    return () => { cancelled = true; };
  }, [openPlayer, sendRect, show]);

  useEffect(() => {
    const off = window.o11a1.settingsChanged?.((value: Settings) => setSettings(value));
    return () => off?.();
  }, []);

  const offState = useRef<(() => void) | null>(null);
  useEffect(() => {
    offState.current = window.o11a1.player.onState((next: any) => {
      setState(next);
      if (!scrubbing) {
        setPosition(Number(next.position || 0));
      }
      if (Number(next.duration || 0) > 0) setDuration(Number(next.duration));
      if (typeof next.danmaku === 'number') setDanmakuStats(Number(next.danmaku));
      if (typeof next.playing === 'boolean' && next.playing) openRef.current = true;
      if (!hydrated.current && typeof next.volume === 'number') {
        hydrated.current = true;
        setVolume(Number(next.volume));
        setMuted(Boolean(next.muted));
      }
    });
    const offOpened = window.o11a1.player.onOpened((data: any) => {
      if (Number(data?.duration || 0) > 0) setDuration(Number(data.duration));
    });
    const offRequest = window.o11a1.player.onRequestOpen?.((next: any) => {
      hydrated.current = false;
      openPlayer(next, true);
    });
    const offNativeClick = window.o11a1.player.onClick?.(() => {
      setDmMenu(null);
      if (clickTimer.current) return;
      clickTimer.current = setTimeout(() => {
        clickTimer.current = null;
        togglePlayRef.current();
      }, 240);
    });
    const offNativeDouble = window.o11a1.player.onDoubleClick?.(() => {
      clearTimeout(clickTimer.current);
      clickTimer.current = null;
      if (miniRef.current) exitMiniRef.current();
      else toggleFullscreenRef.current();
    });
    const offNativeMove = window.o11a1.player.onMouseMove?.(() => showFullscreenChrome());
    const offNativeDanmaku = window.o11a1.player.onDanmaku?.((hit: any) => {
      const slot = videoRef.current?.getBoundingClientRect();
      const scale = window.devicePixelRatio || 1;
      const left = Math.min((slot?.left || 0) + hit.x / scale, window.innerWidth - 185);
      const top = Math.min((slot?.top || 0) + hit.y / scale, window.innerHeight - 190);
      setDmMenu({ ...hit, left: Math.max(6, left), top: Math.max(48, top) });
    });
    const offNativeLongPress = window.o11a1.player.onLongPress?.((hit: any) => {
      if (!hit.midHash) return;
      const previous = Array.isArray(settings?.blocked_senders) ? settings!.blocked_senders.slice() : [];
      if (previous.includes(hit.midHash)) return;
      const next = [...previous, hit.midHash];
      saveSettings({ blocked_senders: next });
      setUndoBlock({ previous, text: hit.text || '该用户' });
      clearTimeout(undoTimer.current);
      undoTimer.current = setTimeout(() => setUndoBlock(null), 3000);
    });
    return () => { offState.current?.(); offOpened?.(); offRequest?.(); offNativeClick?.(); offNativeDouble?.(); offNativeMove?.(); offNativeDanmaku?.(); offNativeLongPress?.(); };
  }, [scrubbing, settings]);

  useEffect(() => {
    const observer = new ResizeObserver(() => sendRect());
    if (videoRef.current) observer.observe(videoRef.current);
    const onResize = () => sendRect();
    window.addEventListener('resize', onResize);
    const onViewport = window.o11a1.player.onViewport?.(() => sendRect());
    // eslint-disable-next-line no-undef
    const timer = setInterval(sendRect, 2000);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', onResize);
      onViewport?.();
      clearInterval(timer);
    };
  }, [sendRect]);

  useEffect(() => {
    if (!options?.bvid) return;
    let cancelled = false;
    setDetail(null);
    setRelated([]);
    setComments([]);
    setCommentTop(null);
    setExpandedReplies({});
    setCommentCursor('');
    setCommentEnd(false);
    setCommentHot(true);
    setDescOpen(false);
    setDetailError('');
    setFollowed(false);
    setShowFolders(false);
    (async () => {
      try {
        const info = await call<VideoDetail>('video.detail', { bvid: options.bvid });
        if (!cancelled) {
          setDetail(info);
          // Cards launch with just a bvid; hydrate the part list here so the
          // side panel can switch P and autoplay the next one, exactly like
          // the removed detail page did.
          if ((info.pages || []).length > 1) {
            const parts = info.pages.map((page, index) => ({ cid: page.cid, title: page.title, index }));
            const wanted = Number(optionsRef.current?.cid || 0);
            const partIndex = Math.max(0, parts.findIndex((part) => part.cid === wanted));
            setOptions((current: any) => {
              if (!current || (current.parts || []).length) return current;
              const next = { ...current, aid: current.aid || info.aid, parts, partIndex };
              optionsRef.current = next;
              return next;
            });
          }
        }
      } catch (error: any) {
        if (!cancelled) setDetailError(error.message || String(error));
      }
      try {
        const rel = await call<FeedVideo[]>('video.related', { bvid: options.bvid });
        if (!cancelled) setRelated(rel);
      } catch {}
    })();
    return () => { cancelled = true; };
  }, [options?.bvid]);

  // Track lists depend on the resolved cid, but must not reset the detail
  // effect: doing that would loop when detail.cid feeds activeCid back.
  useEffect(() => {
    if (!options?.bvid) return;
    let cancelled = false;
    (async () => {
      try {
        const play = await call<any>('video.playinfo', { bvid: options.bvid, cid: activeCid });
        if (!cancelled) {
          setQualities(play.video_tracks || []);
          setAudioTracks(play.audio_tracks || []);
        }
      } catch {}
    })();
    return () => { cancelled = true; };
  }, [options?.bvid, activeCid]);

  const loadComments = useCallback(async (reset: boolean, hot = commentHot) => {
    if (!detail || commentsLoading) return;
    setCommentsLoading(true);
    try {
      const page = await call<CommentPage>('video.comments', { aid: detail.aid, hot, cursor: reset ? '' : commentCursor });
      const pinned = page.top && (page.top.message || page.top.user?.uname) ? page.top : null;
      if (reset) setCommentTop(pinned);
      setComments((current) => reset ? page.comments : [...current, ...page.comments]);
      setCommentCursor(page.cursor);
      setCommentEnd(page.is_end || !page.cursor);
    } catch (error: any) { show(error.message, true); }
    finally { setCommentsLoading(false); }
  }, [detail, commentHot, commentCursor, commentsLoading, show]);

  useEffect(() => {
    if (side === 2 && detail && !comments.length && !commentsLoading) loadComments(true);
  }, [side, detail, comments.length, commentsLoading, loadComments]);

  useEffect(() => {
    if (side !== 3) return;
    call('history.list', { max: 50, viewAt: 0 }).then((page: any) => setHistory(page.items || [])).catch(() => {});
    if (activeCid) {
      setHistoryLoading(true);
      call<string[]>('danmaku.dates', { cid: activeCid })
        .then((dates) => setDanmakuDates(dates || []))
        .catch(() => {})
        .finally(() => setHistoryLoading(false));
    }
  }, [side, activeCid]);

  const loadDanmakuHistory = useCallback(async (date: string) => {
    if (!activeCid) return;
    setHistoryDate(date);
    setHistoryLoading(true);
    try {
      const data = await call<any>('danmaku.history', { cid: activeCid, date });
      setDanmakuHistory(data.items || []);
    } catch (error: any) { show(error.message, true); }
    finally { setHistoryLoading(false); }
  }, [activeCid, show]);

  const likeDanmaku = async (item: any) => {
    if (!activeCid) return;
    try {
      const result = await call<any>('danmaku.like', { cid: activeCid, dmid: item.dmid, liked: true });
      if (result.code !== 0) throw new Error(result.message || `code=${result.code}`);
      show('已点赞弹幕');
    } catch (error: any) { show(error.message, true); }
  };

  const blockSender = async (item: any) => {
    if (!item.mid_hash) return;
    const list = Array.isArray(settings?.blocked_senders) ? settings!.blocked_senders.slice() : [];
    if (!list.includes(item.mid_hash)) list.push(item.mid_hash);
    await saveSettings({ blocked_senders: list });
    show('已屏蔽该用户，下次打开播放器生效');
  };

  const command = useCallback(async (cmd: string, value?: any) => {
    try {
      const next = await window.o11a1.player.command(cmd, value);
      if (next) setState(next);
      return next;
    } catch (error: any) {
      show(error.message, true);
      return null;
    }
  }, [show]);

  const togglePlay = () => command('toggle');
  const seek = (ms: number) => {
    const clamped = Math.max(0, Math.min(duration || Infinity, ms));
    setPosition(clamped);
    command('seek', Math.floor(clamped));
  };

  const timeAtProgress = (clientX: number): number | null => {
    const track = progressRef.current;
    if (!track || !duration) return null;
    const rect = track.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    return ratio * duration;
  };

  const onProgressMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const time = timeAtProgress(event.clientX);
    if (time == null) return;
    setHoverTime(time);
    if (dragging) setPosition(time);
    if (Math.abs(time - lastPreview.current) >= 500) {
      lastPreview.current = time;
      command('preview', Math.round(time));
    }
  };

  const onProgressLeave = () => {
    if (dragging) return;
    setHoverTime(null);
    lastPreview.current = 0;
    command('preview', null);
  };

  const onProgressDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    setDragging(true);
    setScrubbing(true);
    const time = timeAtProgress(event.clientX);
    if (time != null) {
      setHoverTime(time);
      setPosition(time);
      command('preview', Math.round(time));
    }
  };

  const onProgressUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!dragging) return;
    const time = timeAtProgress(event.clientX);
    if (time != null) seek(time);
    setDragging(false);
    setScrubbing(false);
    setHoverTime(null);
    lastPreview.current = 0;
    command('preview', null);
    try { event.currentTarget.releasePointerCapture(event.pointerId); } catch { /* already released */ }
  };

  const onProgressCancel = () => {
    setDragging(false);
    setScrubbing(false);
    setHoverTime(null);
    lastPreview.current = 0;
    command('preview', null);
  };

  const reopenAt = useCallback(async (patch: any) => {
    const positionNow = Number(stateRef.current.position || 0);
    await openPlayer({ ...patch, startMs: positionNow });
  }, [openPlayer]);

  const changeQuality = async (qn: number) => {
    setShowQuality(false);
    await saveSettings({ player_quality: qn });
    await reopenAt({ quality: qn });
    show(`已切换到 ${QUALITY_LABEL[qn] || qn}`);
  };

  const changeAudio = async (index: number) => {
    setShowQuality(false);
    await reopenAt({ audioIndex: index });
    show('已切换音轨');
  };

  const applyDanmakuSettings = async (patch: Partial<Settings>) => {
    const next = await saveSettings(patch);
    if (next) await reopenAt({});
  };

  const showFullscreenChrome = useCallback(() => {
    if (!fullscreenRef.current) return;
    setFullscreenChrome(true);
    clearTimeout(chromeTimer.current);
    chromeTimer.current = setTimeout(() => setFullscreenChrome(false), 2600);
  }, []);

  const toggleFullscreen = async () => {
    const next = !fullscreen;
    setFullscreen(next);
    fullscreenRef.current = next;
    setFullscreenChrome(next);
    clearTimeout(chromeTimer.current);
    if (next) chromeTimer.current = setTimeout(() => setFullscreenChrome(false), 2600);
    await window.o11a1.window.fullscreen(next);
    setTimeout(sendRect, 80);
    setTimeout(sendRect, 320);
  };

  const toggleMute = () => {
    const next = !muted;
    setMuted(next);
    command('volume', next ? 0 : volume);
  };

  const enterMini = async () => {
    const next = !mini;
    setMini(next);
    setShowSettings(false);
    await window.o11a1.window.mini(next);
    setTimeout(sendRect, 100);
  };

  const exitMini = async () => {
    if (!mini) return;
    setMini(false);
    await window.o11a1.window.mini(false);
    setTimeout(sendRect, 100);
  };

  togglePlayRef.current = togglePlay;
  toggleFullscreenRef.current = toggleFullscreen;
  exitMiniRef.current = exitMini;
  miniRef.current = mini;
  fullscreenRef.current = fullscreen;

  const toggleTopmost = async () => {
    const next = !state.topmost;
    await command('topmost', next);
    await window.o11a1.window.topmost(next);
  };

  const screenshot = async () => {
    await command('screenshot');
    show('截图已保存到 screenshots 目录');
  };

  const playPlaylistIndex = async (index: number) => {
    const playlist = settings?.playlist || [];
    const item = playlist[index];
    if (!item) return;
    await saveSettings({ playlist_current: index });
    await openPlayer({ bvid: item.bvid, cid: item.cid || null, aid: item.aid, title: item.title, cover: item.cover, startMs: 0, url: null, audioUrl: null, epId: null });
    setShowPlaylist(false);
  };

  const nextPlaylist = () => {
    const playlist = settings?.playlist || [];
    if (!playlist.length) return;
    const current = settings?.playlist_current ?? -1;
    const next = settings?.playlist_random
      ? Math.floor(Math.random() * playlist.length)
      : (current + 1) % playlist.length;
    playPlaylistIndex(next);
  };

  const prevPlaylist = () => {
    const playlist = settings?.playlist || [];
    if (!playlist.length) return;
    const current = settings?.playlist_current ?? 0;
    const next = (current - 1 + playlist.length) % playlist.length;
    playPlaylistIndex(next);
  };

  useEffect(() => {
    if (!state.ended) return;
    const playlist = settings?.playlist || [];
    if (settings?.playlist_autoplay && playlist.length > 1) {
      if (settings.loop_one) {
        seek(0);
        command('play');
      } else {
        nextPlaylist();
      }
    } else if (!settings?.playlist_autoplay && settings?.playlist_current != null) {
      const current = playlist[settings.playlist_current];
      if (current && (!options?.bvid || current.bvid === options.bvid)) {
        saveSettings({ playlist: playlist.filter((_: any, index: number) => index !== settings.playlist_current), playlist_current: null });
      }
    } else if (options?.parts?.length > 1 && Number(options.partIndex || 0) < options.parts.length - 1) {
      const next = Number(options.partIndex || 0) + 1;
      const part = options.parts[next];
      openPlayer({ ...options, cid: part.cid, partIndex: next, startMs: 0 });
    }
  }, [state.ended]);

  useEffect(() => {
    clearTimeout(sleepTimer.current);
    if (!sleepMinutes) return;
    sleepTimer.current = setTimeout(() => {
      command('pause');
      show(`睡眠定时 ${sleepMinutes} 分钟已到，已暂停`);
      setSleepMinutes(0);
    }, sleepMinutes * 60 * 1000);
    return () => clearTimeout(sleepTimer.current);
  }, [sleepMinutes]);

  useEffect(() => () => {
    clearTimeout(undoTimer.current);
    clearTimeout(chromeTimer.current);
    clearTimeout(clickTimer.current);
  }, []);

  useEffect(() => {
    const listener = async (event: KeyboardEvent) => {
      const tag = (event.target as HTMLElement)?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
      const code = keyFor(event);
      const keys = effectiveKeys(settings?.electron_player_keys);
      const action = keys.indexOf(code);
      if (action >= 0) {
        event.preventDefault();
        if (action === 0) togglePlay();
        else if (action === 1) seek(position - 5000);
        else if (action === 2) seek(position + 5000);
        else if (action === 3) { const next = Math.min(100, volume + 5); setVolume(next); command('volume', next); }
        else if (action === 4) { const next = Math.max(0, volume - 5); setVolume(next); command('volume', next); }
        else if (action === 5) toggleMute();
        else if (action === 6 && !fullscreen) toggleFullscreen();
        else if (action === 7 && fullscreen) toggleFullscreen();
        else if (action === 8) { setDanmakuOn((value) => { command('danmaku', !value); return !value; }); }
        else if (action === 9 && options?.bvid) {
          try { await call('video.like', { bvid: options.bvid, liked: true }); show('已点赞'); }
          catch (error: any) { show(error.message, true); }
        } else if (action === 10) screenshot();
        return;
      }
      switch (event.key) {
        case ' ': event.preventDefault(); togglePlay(); break;
        case 'ArrowLeft': event.preventDefault(); seek(position - 5000); break;
        case 'ArrowRight': event.preventDefault(); seek(position + 5000); break;
        case 'ArrowUp': event.preventDefault(); { const next = Math.min(100, volume + 5); setVolume(next); command('volume', next); } break;
        case 'ArrowDown': event.preventDefault(); { const next = Math.max(0, volume - 5); setVolume(next); command('volume', next); } break;
        case 'f': case 'F': if (!fullscreen) toggleFullscreen(); break;
        case 'Escape': if (fullscreen) toggleFullscreen(); break;
        case 'd': case 'D': setDanmakuOn((value) => { command('danmaku', !value); return !value; }); break;
        case 'm': case 'M': toggleMute(); break;
        default: break;
      }
    };
    window.addEventListener('keydown', listener);
    return () => window.removeEventListener('keydown', listener);
  }, [settings, position, volume, fullscreen, options?.bvid]);

  const onVideoClick = () => {
    if (clickTimer.current) return;
    clickTimer.current = setTimeout(() => {
      clickTimer.current = null;
      togglePlay();
    }, 220);
  };
  const onVideoDouble = () => {
    clearTimeout(clickTimer.current);
    clickTimer.current = null;
    if (mini) exitMini();
    else toggleFullscreen();
  };

  // The video HWND now sits behind the transparent Electron window, so the
  // DOM owns mouse input. A 5 px movement starts the native move loop; a
  // clean click still toggles playback.
  const onStagePointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement;
    if (target.closest('button, input, textarea, select, .no-drag, .player-error, .side, .player-bottom, .player-popover, .player-window-controls')) return;
    dragRef.current = { x: event.screenX, y: event.screenY, moved: false, pointerId: event.pointerId };
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* ignore */ }
  };

  const onStagePointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag || drag.moved) return;
    if (Math.hypot(event.screenX - drag.x, event.screenY - drag.y) < 5) return;
    drag.moved = true;
    try { event.currentTarget.releasePointerCapture(drag.pointerId); } catch { /* ignore */ }
    window.o11a1.window.startDrag?.().catch(() => {});
  };

  const onStagePointerUp = (event: React.PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    dragRef.current = null;
    if (!drag) return;
    try { event.currentTarget.releasePointerCapture(drag.pointerId); } catch { /* ignore */ }
    if (!drag.moved && event.button === 0) onVideoClick();
  };

  const onStagePointerCancel = () => {
    dragRef.current = null;
  };

  const danmakuSubmit = async () => {
    const text = danmakuDraft.trim();
    if (!text || !options?.bvid || !activeCid) {
      if (!options?.bvid) show('直播暂不支持发送弹幕', true);
      return;
    }
    try {
      const result = await call<any>('danmaku.post', { bvid: options.bvid, cid: activeCid, text, progress: Math.floor(position), mode: 1 });
      if (result?.code && result.code !== 0) throw new Error(result.message || `code=${result.code}`);
      setDanmakuDraft('');
      show('弹幕已发送');
    } catch (error: any) { show(error.message, true); }
  };

  const playVideoNow = useCallback(async (entry: any) => {
    const next = {
      bvid: entry.bvid,
      cid: entry.cid || null,
      aid: entry.aid || 0,
      title: entry.title || '',
      cover: entry.cover || entry.pic || '',
      owner: entry.owner || entry.author || '',
      duration: entry.duration || 0,
      startMs: Number(entry.startMs || 0),
      url: null,
      audioUrl: null,
      epId: null,
      episodes: [],
      parts: entry.parts || [],
      partIndex: entry.partIndex || 0,
    };
    await enqueuePlaylist(next).catch(() => {});
    await openPlayer(next, true);
    setSide(0);
  }, [openPlayer]);

  const playPart = useCallback(async (part: any, index: number) => {
    if (!detail) return;
    await openPlayer({
      ...options,
      cid: part.cid,
      partIndex: index,
      title: `${detail.title} · ${part.title}`,
      startMs: 0,
    });
  }, [detail, openPlayer, options]);

  const playPgcEpisode = useCallback(async (ep: any) => {
    try {
      const source = await resolvePgcSource(ep.ep_id, ep.cid);
      await openPlayer({
        ...options,
        epId: ep.ep_id,
        cid: ep.cid,
        url: source.url,
        audioUrl: source.audioUrl || '',
        title: `${ep.title || ''} ${ep.long_title || ''}`.trim() || options.title,
        startMs: 0,
      });
    } catch (error: any) { show(error.message, true); }
  }, [openPlayer, options, show]);

  const downloadVideo = async () => {
    if (!detail) return;
    try {
      const partIndex = Number(options.partIndex || 0);
      await call('downloads.create', {
        bvid: detail.bvid,
        cid: activeCid,
        title: detail.pages.length > 1 ? `${detail.title} P${partIndex + 1}` : detail.title,
        cover: detail.pic,
      });
      show('已加入下载队列');
    } catch (error: any) { show(error.message, true); }
  };

  const likeVideo = async () => {
    if (!detail) return;
    try {
      await call('video.like', { bvid: detail.bvid, liked: true });
      show('已点赞');
    } catch (error: any) { show(error.message, true); }
  };

  const coinVideo = async () => {
    if (!detail) return;
    try {
      await call('video.coin', { bvid: detail.bvid, count: 1, like: true });
      show('投币成功');
    } catch (error: any) { show(error.message, true); }
  };

  const followOwner = async () => {
    if (!detail || followed) return;
    try {
      await call('user.follow', { mid: detail.owner_mid, follow: true });
      setFollowed(true);
      show('已关注');
    } catch (error: any) { show(error.message, true); }
  };

  const openFolders = async () => {
    if (folders) { setShowFolders((open) => !open); return; }
    try {
      const list = await call<Array<{ id: number; name: string; count: number }>>('favorites.folders');
      setFolders(list);
      setShowFolders(true);
    } catch (error: any) { show(error.message, true); }
  };

  const addFavorite = async (mediaId: number) => {
    if (!detail) return;
    try {
      await call('video.fav', { aid: detail.aid, add: [mediaId], del: [] });
      setShowFolders(false);
      show('已收藏');
    } catch (error: any) { show(error.message, true); }
  };

  const addQueue = async () => {
    if (!detail) return;
    try {
      await addToPlaylist({
        bvid: detail.bvid,
        cid: activeCid,
        aid: detail.aid,
        title: detail.title,
        owner: detail.owner_name,
        duration: String(detail.duration),
        cover: detail.pic,
      });
      show('已加入播放列表');
    } catch (error: any) { show(error.message, true); }
  };

  const openReplies = async (comment: Comment) => {
    if (!detail) return;
    if (expandedReplies[comment.rpid]) {
      setExpandedReplies((current) => {
        const next = { ...current };
        delete next[comment.rpid];
        return next;
      });
      return;
    }
    try {
      const page = await call<{ replies: Comment[] }>('video.replies', { aid: detail.aid, root: comment.rpid, page: 1 });
      setExpandedReplies((current) => ({ ...current, [comment.rpid]: page.replies || [] }));
    } catch (error: any) { show(error.message, true); }
  };

  const likeComment = async (comment: Comment) => {
    if (!detail) return;
    try {
      const result = await call<{ code: number; message: string }>('video.comment.like', { aid: detail.aid, rpid: comment.rpid, liked: !comment.liked });
      if (result.code !== 0) throw new Error(result.message || `code=${result.code}`);
      const patch = (row: Comment) => row.rpid === comment.rpid
        ? { ...row, liked: !row.liked, like: row.like + (row.liked ? -1 : 1) }
        : row;
      setComments((rows) => rows.map(patch));
      setCommentTop((row) => (row ? patch(row) : row));
    } catch (error: any) { show(error.message, true); }
  };

  const title = options?.title || detail?.title || 'O11A1';
  const percent = duration > 0 ? Math.min(100, (position / duration) * 100) : 0;
  const playlist = settings?.playlist || [];

  if (!options) {
    return <div className="player-shell" style={{ display: 'grid', placeItems: 'center' }}><div className="player-empty"><span className="spinner" /></div></div>;
  }

  return (
    <div className={`player-shell ${fullscreen ? 'fullscreen' : ''} ${fullscreen && fullscreenChrome ? 'chrome' : ''} ${mini ? 'mini' : ''}`} onMouseMove={() => showFullscreenChrome()}>
      <div className="player-window-controls no-drag">
        <button className="win-btn" title="最小化" onClick={() => window.o11a1.window.minimize()}><IconMin /></button>
        <button className="win-btn" title="最大化" onClick={() => window.o11a1.window.maximize()}><IconMax /></button>
        <button className="win-btn close" title="关闭" onClick={() => window.o11a1.player.close()}><IconClose /></button>
      </div>

      <div
        className="player-stage"
        ref={stageRef}
        onPointerDown={onStagePointerDown}
        onPointerMove={onStagePointerMove}
        onPointerUp={onStagePointerUp}
        onPointerCancel={onStagePointerCancel}
        onDoubleClick={onVideoDouble}
        onMouseEnter={() => setShowControls(true)}
      >
        <div className="video-slot" ref={videoRef}>
          {openError ? (
            <div className="player-error" onClick={(event) => event.stopPropagation()}>
              <strong>播放失败</strong>
              <div>{openError}</div>
              <button className="btn sm primary" onClick={() => openPlayer(optionsRef.current, true)}>重试</button>
            </div>
          ) : <div className="hint">{state.open ? '' : '正在打开视频…'}</div>}
        </div>
      </div>

      {!fullscreen && sidebarOpen ? (
        <aside className="side">
          <div className="side-tabs">
            {SIDE_TABS.map((label, index) => (
              <button key={label} className={`side-tab ${side === index ? 'active' : ''}`} onClick={() => setSide(index)}>{label}</button>
            ))}
            <span className="side-tabs-spacer" />
            <button className="side-close" title="关闭侧栏" onClick={() => setSidebarOpen(false)}><IconClose size={12} /></button>
          </div>
            <div className="side-body">
              {side === 0 ? (
                <>
                  {detail ? (
                    <>
                      <div className="side-cover" role="button" tabIndex={0} title="播放 / 暂停" onClick={togglePlay}>
                        <img src={cover(detail.pic)} referrerPolicy="no-referrer" alt="" />
                        <span>{formatDuration(detail.duration)}</span>
                      </div>
                      <div className="side-info-title">{detail.title}</div>
                      <div className="side-info-sub">{[detail.typename, formatDate(detail.pubdate), `${formatCount(detail.stat_reply)} 条评论`].filter(Boolean).join(' · ')}</div>
                      <div className="side-stat-grid">
                        <div><b>{formatCount(detail.stat_view)}</b><span>播放</span></div>
                        <div><b>{formatCount(detail.stat_danmaku)}</b><span>弹幕</span></div>
                        <div><b>{formatCount(detail.stat_like)}</b><span>点赞</span></div>
                        <div><b>{formatCount(detail.stat_coin)}</b><span>投币</span></div>
                        <div><b>{formatCount(detail.stat_favorite)}</b><span>收藏</span></div>
                        <div><b>{formatCount(detail.stat_share)}</b><span>分享</span></div>
                      </div>
                      <div className="side-up">
                        <img className="avatar" src={cover(detail.owner_face)} referrerPolicy="no-referrer" alt="" style={{ cursor: 'pointer' }} onClick={() => window.o11a1.app?.navigateMain?.(`user/${detail.owner_mid}`)} />
                        <div className="side-up-meta">
                          <div style={{ cursor: 'pointer' }} onClick={() => window.o11a1.app?.navigateMain?.(`user/${detail.owner_mid}`)}>{detail.owner_name}</div>
                          <span>UID {detail.owner_mid}</span>
                        </div>
                        <button className={`btn sm ${followed ? '' : 'primary'}`} disabled={followed} onClick={followOwner}>{followed ? '已关注' : '关注'}</button>
                      </div>
                      <div className={`side-desc ${descOpen ? 'open' : ''}`}>{(detail.description || '暂无简介').trim()}</div>
                      {(detail.description || '').length > 40 ? (
                        <button className="side-link" onClick={() => setDescOpen((open) => !open)}>{descOpen ? '收起简介' : '展开完整简介'}</button>
                      ) : null}
                      <div className="side-actions">
                        <button onClick={downloadVideo}><IconDownload size={13} />下载</button>
                        <button onClick={likeVideo}><IconHeart size={13} />点赞</button>
                        <button onClick={coinVideo}><IconCoin size={13} />投币</button>
                        <button onClick={openFolders}><IconStar size={13} />收藏</button>
                        <button onClick={addQueue}><IconList size={13} />列表</button>
                      </div>
                      {showFolders ? (
                        <div className="side-folders">
                          {folders?.length ? folders.map((folder) => (
                            <button key={folder.id} onClick={() => addFavorite(folder.id)}>
                              <span>{folder.name}</span><i>{folder.count}</i>
                            </button>
                          )) : <div>没有收藏夹</div>}
                        </div>
                      ) : null}
                      {(options.parts || []).length > 1 ? (
                        <>
                          <div className="side-title" style={{ marginTop: 16 }}>分P（{options.parts.length}）</div>
                          <div className="episode-grid">
                            {(options.parts || []).map((part: any, index: number) => (
                              <button
                                key={part.cid}
                                className={`episode ${options.partIndex === index ? 'active' : ''}`}
                                title={part.title}
                                onClick={() => playPart(part, index)}
                              >{`P${index + 1} ${part.title || ''}`}</button>
                            ))}
                          </div>
                        </>
                      ) : null}
                    </>
                  ) : (
                    <>
                      {options.cover ? (
                        <div className="side-cover">
                          <img src={cover(options.cover)} referrerPolicy="no-referrer" alt="" />
                        </div>
                      ) : null}
                      <div className="side-info-title">{options.title || title}</div>
                      <div className="side-info-sub">
                        {options.bvid
                          ? (detailError ? `详情加载失败：${detailError}` : '信息加载中…')
                          : '直播 / 番剧直链播放'}
                      </div>
                      {options.bvid && !detailError ? <div className="side-loading"><span className="spinner" /> 正在读取视频信息…</div> : null}
                    </>
                  )}
                  <div className="side-title" style={{ marginTop: 18 }}>相关推荐</div>
                  {related.slice(0, 8).map((item) => (
                    <div className="side-related" key={item.bvid} onClick={() => playVideoNow(item)}>
                      <div className="side-related-cover">
                        <img src={cover(item.pic)} referrerPolicy="no-referrer" alt="" />
                        {item.duration > 0 ? <span>{formatDuration(item.duration)}</span> : null}
                      </div>
                      <div className="side-related-body">
                        <div className="side-related-title">{item.title}</div>
                        <div className="side-related-meta">{item.owner} · {formatCount(item.view)} 播放</div>
                      </div>
                    </div>
                  ))}
                  {detail && !related.length ? <div className="side-empty">暂无相关推荐</div> : null}
                </>
              ) : null}

              {side === 1 ? (
                <>
                  {(options.parts || []).length ? (
                    <>
                      <div className="side-title">分P</div>
                      <div className="episode-grid">
                        {(options.parts || []).map((part: any, index: number) => (
                          <button key={part.cid} className={`episode ${options.partIndex === index ? 'active' : ''}`} onClick={() => openPlayer({ ...options, cid: part.cid, partIndex: index, title: `${detail?.title || options.title} · ${part.title}`, startMs: 0 })}>P{index + 1}</button>
                        ))}
                      </div>
                    </>
                  ) : null}
                  {(options.episodes || []).length ? (
                    <>
                      <div className="side-title">选集</div>
                      <div className="episode-grid">
                        {(options.episodes || []).map((ep: any, index: number) => (
                          <button key={`${ep.ep_id}-${index}`} className={`episode ${options.epId === ep.ep_id ? 'active' : ''}`} onClick={() => playPgcEpisode(ep)}>{ep.title || `第 ${index + 1} 集`}</button>
                        ))}
                      </div>
                    </>
                  ) : null}
                  {!(options.parts || []).length && !(options.episodes || []).length ? <div style={{ color: '#8a8d92', fontSize: 12 }}>这个视频没有分P。</div> : null}
                </>
              ) : null}

              {side === 2 ? (
                <>
                  <div className="side-comments-head">
                    <div className="side-title" style={{ margin: 0 }}>评论 {detail ? `· ${formatCount(detail.stat_reply)}` : ''}</div>
                    <span style={{ flex: 1 }} />
                    <button className={`chip small ${commentHot ? 'active' : ''}`} onClick={() => { setCommentHot(true); setComments([]); setCommentTop(null); setCommentCursor(''); loadComments(true, true); }}>热门</button>
                    <button className={`chip small ${!commentHot ? 'active' : ''}`} onClick={() => { setCommentHot(false); setComments([]); setCommentTop(null); setCommentCursor(''); loadComments(true, false); }}>最新</button>
                  </div>
                  {commentTop && (commentTop.message || commentTop.user?.uname) ? <SideComment comment={commentTop} pinned replies={expandedReplies[commentTop.rpid]} onLike={likeComment} onReplies={openReplies} /> : null}
                  {comments.map((comment) => (
                    <SideComment key={comment.rpid} comment={comment} replies={expandedReplies[comment.rpid]} onLike={likeComment} onReplies={openReplies} />
                  ))}
                  {!comments.length && !commentTop && commentsLoading ? <div style={{ display: 'grid', placeItems: 'center', padding: 20 }}><span className="spinner" /></div> : null}
                  {!comments.length && !commentTop && !commentsLoading ? <div className="side-empty">还没有评论</div> : null}
                  {!commentEnd && (comments.length > 0 || commentTop) ? (
                    <button className="btn sm" style={{ width: '100%', marginTop: 10 }} disabled={commentsLoading} onClick={() => loadComments(false)}>{commentsLoading ? '加载中…' : '加载更多评论'}</button>
                  ) : commentEnd && (comments.length > 0 || commentTop) ? <div style={{ textAlign: 'center', color: '#8a8d92', fontSize: 11, padding: 10 }}>没有更多评论</div> : null}
                </>
              ) : null}

              {side === 3 ? (
                <>
                  <div className="side-title">弹幕历史</div>
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginBottom: 8 }}>
                    <button className={`chip small ${historyDate === '' ? 'active' : ''}`} onClick={() => loadDanmakuHistory('')}>实时</button>
                    {danmakuDates.map((date) => (
                      <button key={date} className={`chip small ${historyDate === date ? 'active' : ''}`} onClick={() => loadDanmakuHistory(date)}>{date.slice(5)}</button>
                    ))}
                  </div>
                  {historyLoading ? <div style={{ display: 'grid', placeItems: 'center', padding: 10 }}><span className="spinner" /></div> : null}
                  {(historyDate ? danmakuHistory : []).map((item: any, index: number) => (
                    <div key={`${item.dmid}-${index}`} style={{ padding: '6px 0', borderBottom: '1px solid rgba(0,0,0,.06)' }}>
                      <div style={{ fontSize: 10.5, color: '#8a8d92' }}>{formatTime(item.progress_ms)} · {item.mode === 5 ? '顶部' : item.mode === 4 ? '底部' : '滚动'}</div>
                      <div style={{ fontSize: 12, lineHeight: 1.4, marginTop: 2 }}>{item.text}</div>
                      <div style={{ display: 'flex', gap: 8, marginTop: 3, flexWrap: 'wrap' }}>
                        <button className="btn sm ghost" style={{ padding: 0, fontSize: 10 }} onClick={() => likeDanmaku(item)}>点赞</button>
                        <button className="btn sm ghost" style={{ padding: 0, fontSize: 10 }} onClick={() => blockSender(item)}>屏蔽用户</button>
                        <button className="btn sm ghost" style={{ padding: 0, fontSize: 10 }} onClick={() => navigator.clipboard?.writeText(item.text)}>复制</button>
                        <button className="btn sm ghost" style={{ padding: 0, fontSize: 10 }} onClick={async () => {
                          const reason = Number(window.prompt('举报原因（1–12，11 = 其他）', '1') || 0);
                          if (!reason || reason < 1 || reason > 12) return;
                          try {
                            const result = await call<any>('danmaku.report', { cid: activeCid, dmid: item.dmid, reason, content: reason === 11 ? item.text : '' });
                            if (result.code !== 0) throw new Error(result.message || `code=${result.code}`);
                            show('已提交举报');
                          } catch (error: any) { show(error.message, true); }
                        }}>举报</button>
                      </div>
                    </div>
                  ))}
                  {historyDate && !danmakuHistory.length && !historyLoading ? <div style={{ color: '#8a8d92', fontSize: 12 }}>这一天没有弹幕历史</div> : null}
                  {!historyDate ? <div style={{ color: '#8a8d92', fontSize: 11.5, marginTop: 4 }}>选择日期查看历史弹幕；「实时」是当前播放的列表。</div> : null}
                  <div className="side-title" style={{ marginTop: 16 }}>最近观看</div>
                  {history.slice(0, 5).map((item: any) => (
                    <div key={item.kid} style={{ padding: '5px 0', cursor: 'pointer' }} onClick={() => openPlayer({ bvid: item.bvid, title: item.title, cover: item.cover, startMs: item.progress * 1000, url: null, audioUrl: null, epId: null, cid: null })}>
                      <div style={{ fontSize: 11, fontWeight: 600 }}>{item.title}</div>
                      <div style={{ fontSize: 10, color: '#8a8d92' }}>{item.author} · {formatTime(item.progress * 1000)}</div>
                    </div>
                  ))}
                  {!history.length ? <div style={{ color: '#8a8d92', fontSize: 11 }}>没有观看历史</div> : null}
                </>
              ) : null}

              {side === 4 ? (
                <>
                  <div className="side-title">播放性能</div>
                  <div className="kv" style={{ gridTemplateColumns: '100px 1fr', fontSize: 11.5 }}>
                    <div className="k">FPS</div><div className="v">{(state.fps || 0).toFixed(1)}</div>
                    <div className="k">位置</div><div className="v">{formatTime(position)} / {formatTime(duration)}</div>
                    <div className="k">弹幕</div><div className="v">{danmakuStats} 条</div>
                    <div className="k">声音</div><div className="v">{muted ? '静音' : `${volume}%`}</div>
                    <div className="k">插帧</div><div className="v">{Number(settings?.interp_mode) ? '开启' : '关闭'}</div>
                    <div className="k">增强</div><div className="v">{Number(settings?.player_enhance) ? '开启' : '关闭'}</div>
                    <div className="k">解码</div><div className="v">ffmpeg + D3D11</div>
                  </div>
                  <div style={{ marginTop: 12, fontSize: 11, color: '#8a8d92' }}>视频与弹幕由 Rust 原生窗口渲染，Electron 只负责这层 UI。</div>
                </>
              ) : null}

              {side === 5 && settings ? (
                <>
                  <div className="side-title">弹幕</div>
                  <label style={{ fontSize: 11.5 }}>不透明度 {settings.danmaku_opacity}%</label>
                  <input type="range" min={0} max={100} style={{ width: '100%' }} value={settings.danmaku_opacity} onChange={(event) => saveSettings({ danmaku_opacity: Number(event.target.value) })} onMouseUp={() => reopenAt({})} />
                  <label style={{ fontSize: 11.5, display: 'block', marginTop: 8 }}>速度 {settings.danmaku_speed}%</label>
                  <input type="range" min={50} max={200} style={{ width: '100%' }} value={settings.danmaku_speed} onChange={(event) => saveSettings({ danmaku_speed: Number(event.target.value) })} onMouseUp={() => reopenAt({})} />
                  <div style={{ display: 'flex', gap: 6, marginTop: 8 }}>
                    {['小', '标准', '大'].map((label, index) => (
                      <button key={label} className={`chip small ${settings.danmaku_font === index ? 'active' : ''}`} onClick={() => applyDanmakuSettings({ danmaku_font: index })}>{label}</button>
                    ))}
                  </div>
                  <div style={{ display: 'flex', gap: 6, marginTop: 6 }}>
                    {[25, 50, 75, 100].map((area) => (
                      <button key={area} className={`chip small ${settings.danmaku_area === area ? 'active' : ''}`} onClick={() => applyDanmakuSettings({ danmaku_area: area })}>{area}%</button>
                    ))}
                  </div>
                  <div className="side-title" style={{ marginTop: 18 }}>画面</div>
                  <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
                    <button className={`chip small ${Number(settings.interp_mode) ? 'active' : ''}`} onClick={async () => { const next = Number(settings.interp_mode) ? 0 : 1; await saveSettings({ interp_mode: next }); command('interp', next); }}>插帧</button>
                    <button className={`chip small ${Number(settings.player_enhance) ? 'active' : ''}`} onClick={async () => { const next = Number(settings.player_enhance) ? 0 : 1; await saveSettings({ player_enhance: next }); command('enhance', next); }}>画质增强</button>
                    <button className={`chip small ${state.topmost ? 'active' : ''}`} onClick={toggleTopmost}>置顶</button>
                    <button className={`chip small ${state.audioOnly ? 'active' : ''}`} onClick={() => command('audio_only', !state.audioOnly)}>仅音频</button>
                  </div>
                  <div className="side-title" style={{ marginTop: 18 }}>比例</div>
                  <div style={{ display: 'flex', gap: 6 }}>
                    {['适应', '填充', '拉伸'].map((label, index) => (
                      <button key={label} className="chip small" onClick={() => command('aspect', index)}>{label}</button>
                    ))}
                  </div>
                  <div className="side-title" style={{ marginTop: 18 }}>亮度</div>
                  <input type="range" min={40} max={200} value={Math.round((state.brightness || 1) * 100)} style={{ width: '100%' }} onChange={(event) => command('brightness', Number(event.target.value) / 100)} />
                  <div className="side-title" style={{ marginTop: 18 }}>播放速度</div>
                  <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
                    {[0.5, 0.75, 1, 1.25, 1.5, 2].map((speed) => (
                      <button key={speed} className="chip small" onClick={() => command('speed', speed)}>{speed}×</button>
                    ))}
                  </div>
                </>
              ) : null}
            </div>
        </aside>
      ) : null}

      <div className="player-bottom" style={fullscreen && !fullscreenChrome ? { display: 'none' } : undefined}>
        <div
          className="progress-line"
          ref={progressRef}
          onPointerDown={onProgressDown}
          onPointerMove={onProgressMove}
          onPointerUp={onProgressUp}
          onPointerCancel={onProgressCancel}
          onPointerLeave={onProgressLeave}
        >
          <div className="progress-track"><div className="progress-fill" style={{ width: `${percent}%` }} /></div>
          {hoverTime != null ? (
            <div className="seek-hint" style={{ left: `${duration > 0 ? (hoverTime / duration) * 100 : 0}%` }}>{formatTime(hoverTime)}</div>
          ) : null}
        </div>
        <div className="controls">
          <button className="ctrl play" onClick={togglePlay} title="播放/暂停">{state.playing ? <IconPause size={18} /> : <IconPlay size={18} />}</button>
          <button className="ctrl" onClick={prevPlaylist} title="上一首"><IconPrev /></button>
          <button className="ctrl" onClick={nextPlaylist} title="下一首"><IconNext /></button>
          <div className="time">{formatTime(position)} / {formatTime(duration)}</div>
          <div className="volume">
            <button className="ctrl" onClick={toggleMute}>{muted || volume === 0 ? <IconMute /> : <IconVolume />}</button>
            <input type="range" min={0} max={100} value={muted ? 0 : volume} onChange={(event) => { const next = Number(event.target.value); setVolume(next); setMuted(next === 0); command('volume', next); }} />
          </div>
          <div className="spacer" />
          <div className="danmaku-input">
            <button className={`ctrl ${danmakuOn ? 'active' : ''}`} onClick={() => { setDanmakuOn(!danmakuOn); command('danmaku', !danmakuOn); }} title="弹幕开关"><IconDanmaku /></button>
            <input value={danmakuDraft} onChange={(event) => setDanmakuDraft(event.target.value)} onKeyDown={(event) => { if (event.key === 'Enter') danmakuSubmit(); }} placeholder="发个弹幕…" />
            <button onClick={danmakuSubmit}>发送</button>
          </div>
          <div className="spacer" />
          <button className="ctrl" onClick={() => { setShowQuality(!showQuality); setShowPlaylist(false); setShowSettings(false); }} title="画质"><span style={{ fontSize: 11 }}>{QUALITY_LABEL[Number(options.quality)] || '画质'}</span></button>
          <button className="ctrl" onClick={() => { setShowPlaylist(!showPlaylist); setShowQuality(false); setShowSettings(false); }} title="播放列表"><IconList /></button>
          <button className={`ctrl ${sidebarOpen ? 'active' : ''}`} onClick={() => setSidebarOpen((open) => !open)} title="侧边栏"><IconPanel /></button>
          <button className="ctrl" onClick={() => { setShowSettings(!showSettings); setShowQuality(false); setShowPlaylist(false); }} title="设置"><IconSettings /></button>
          <button className="ctrl" onClick={screenshot} title="截图"><IconDownload size={15} /></button>
          <button className="ctrl" onClick={toggleFullscreen} title="全屏">{fullscreen ? <IconExitFull /> : <IconFull />}</button>
        </div>
      </div>

      {showQuality ? (
        <div className="player-popover">
          <h4>画质 {options.url ? '（直播/直链）' : ''}</h4>
          {options.url ? <div style={{ fontSize: 12, color: '#6f7278', marginBottom: 8 }}>直链播放无法在这里切换画质，请返回页面重选。</div> : null}
          {qualities.map((track: any) => (
            <div className="quality-row" key={`${track.id}-${track.codecid}`} onClick={() => changeQuality(track.id)}>
              <span style={{ flex: 1 }}>{QUALITY_LABEL[track.id] || track.id} · {track.width}×{track.height}</span>
              <span style={{ fontSize: 10.5, color: '#8a8d92' }}>{track.codecid === 7 ? 'AVC' : track.codecid === 12 ? 'HEVC' : track.codecid === 13 ? 'AV1' : ''}</span>
            </div>
          ))}
          {audioTracks.length ? <><h4 style={{ marginTop: 12 }}>音轨</h4>{audioTracks.map((track: any, index: number) => (
            <div className="quality-row" key={track.id} onClick={() => changeAudio(index)}>
              <span style={{ flex: 1 }}>{track.id === 30250 ? '杜比全景声' : track.id === 30251 ? 'Hi-Res 无损' : `${Math.round(track.bandwidth / 1000)} kbps`}</span>
              {track.vip ? <span className="chip small">大会员</span> : null}
            </div>
          ))}</> : null}
          {!qualities.length && !options.url ? <div style={{ fontSize: 12, color: '#8a8d92' }}>正在读取画质列表…</div> : null}
        </div>
      ) : null}

      {showPlaylist ? (
        <div className="player-popover playlist-pop" onWheel={(event) => { event.currentTarget.scrollTop += event.deltaY; }}>
          <h4>播放列表（{playlist.length}）</h4>
          {playlist.map((item: any, index: number) => (
            <div className={`playlist-row ${settings?.playlist_current === index ? 'active' : ''}`} key={`${item.bvid}-${item.cid}-${index}`} onClick={() => playPlaylistIndex(index)}>
              <span style={{ width: 18 }}>{index + 1}</span>
              <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{item.title}</span>
              <button className="btn sm ghost" onClick={(event) => { event.stopPropagation(); saveSettings({ playlist: playlist.filter((_: any, i: number) => i !== index), playlist_current: null }); }}>移除</button>
            </div>
          ))}
          {!playlist.length ? <div style={{ fontSize: 12, color: '#8a8d92' }}>播放列表是空的。播放任意视频会自动加入。</div> : null}
          <div style={{ display: 'flex', gap: 6, marginTop: 10 }}>
            <button className={`chip small ${settings?.playlist_autoplay ? 'active' : ''}`} onClick={() => saveSettings({ playlist_autoplay: !settings?.playlist_autoplay })}>自动连播</button>
            <button className={`chip small ${settings?.playlist_random ? 'active' : ''}`} onClick={() => saveSettings({ playlist_random: !settings?.playlist_random })}>随机</button>
            <button className={`chip small ${settings?.loop_one ? 'active' : ''}`} onClick={() => saveSettings({ loop_one: !settings?.loop_one })}>单曲循环</button>
            <button className="btn sm danger" style={{ marginLeft: 'auto' }} onClick={() => saveSettings({ playlist: [], playlist_current: null })}>清空</button>
          </div>
        </div>
      ) : null}

      {showSettings ? (
        <div className="player-popover">
          <h4>播放器</h4>
          <div className="row" style={{ padding: '7px 0' }}><span style={{ flex: 1 }}>睡眠定时</span>
            <select className="select" value={sleepMinutes} onChange={(event) => setSleepMinutes(Number(event.target.value))}>
              <option value={0}>关闭</option>
              <option value={15}>15 分钟</option>
              <option value={30}>30 分钟</option>
              <option value={60}>60 分钟</option>
              <option value={90}>90 分钟</option>
            </select>
          </div>
          <div className="row" style={{ padding: '7px 0' }}><span style={{ flex: 1 }}>迷你窗</span><button className="btn sm" onClick={enterMini}>切换</button></div>
          <div className="row" style={{ padding: '7px 0' }}><span style={{ flex: 1 }}>快捷键</span><span style={{ fontSize: 11, color: '#8a8d92' }}>Space · ←/→ · ↑/↓ · F · D</span></div>
          <div className="row" style={{ padding: '7px 0' }}><span style={{ flex: 1 }}>播放器设置</span><button className="btn sm" onClick={() => { setSide(5); setShowSettings(false); }}>打开侧栏</button></div>
        </div>
      ) : null}

      {undoBlock ? (
        <div className="undo-toast">
          <span>已屏蔽该用户</span>
          <button onClick={async () => {
            clearTimeout(undoTimer.current);
            await saveSettings({ blocked_senders: undoBlock.previous });
            setUndoBlock(null);
            show('已撤销屏蔽');
          }}>撤销</button>
        </div>
      ) : null}
      {dmMenu ? (
        <div className="dm-menu" style={{ left: dmMenu.left, top: dmMenu.top }} onClick={(event) => event.stopPropagation()}>
          <div className="dm-menu-text">{dmMenu.text}</div>
          <button onClick={async () => {
            try {
              const result = await call<any>('danmaku.like', { cid: activeCid, dmid: dmMenu.dmid, liked: true });
              if (result.code !== 0) throw new Error(result.message || `code=${result.code}`);
              show('已点赞弹幕');
            } catch (error: any) { show(error.message, true); }
            setDmMenu(null);
          }}>点赞</button>
          <button onClick={async () => {
            if (!dmMenu.midHash) { show('这条弹幕没有发送者信息', true); setDmMenu(null); return; }
            const list = Array.isArray(settings?.blocked_senders) ? settings!.blocked_senders.slice() : [];
            if (!list.includes(dmMenu.midHash)) list.push(dmMenu.midHash);
            await saveSettings({ blocked_senders: list });
            setDmMenu(null);
            show('已屏蔽该用户，3 秒内可在设置里撤销');
          }}>屏蔽用户</button>
          <button onClick={() => { navigator.clipboard?.writeText(dmMenu.text); setDmMenu(null); }}>复制</button>
          <button onClick={async () => {
            const reason = Number(window.prompt('举报原因（1–12，11 = 其他）', '1') || 0);
            if (!reason || reason < 1 || reason > 12) { setDmMenu(null); return; }
            try {
              const result = await call<any>('danmaku.report', { cid: activeCid, dmid: dmMenu.dmid, reason, content: reason === 11 ? dmMenu.text : '' });
              if (result.code !== 0) throw new Error(result.message || `code=${result.code}`);
              show('已提交举报');
            } catch (error: any) { show(error.message, true); }
            setDmMenu(null);
          }}>举报</button>
          <button className="dm-menu-close" onClick={() => setDmMenu(null)}>关闭</button>
        </div>
      ) : null}
      {toast ? <div className={`toast ${toast.error ? 'error' : ''}`}>{toast.text}</div> : null}
    </div>
  );
}

function SideComment({ comment, pinned, replies, onLike, onReplies }: {
  comment: Comment;
  pinned?: boolean;
  replies?: Comment[];
  onLike: (comment: Comment) => void;
  onReplies: (comment: Comment) => void;
}) {
  return (
    <div className="side-comment">
      <img className="avatar sm" src={cover(comment.user.avatar)} referrerPolicy="no-referrer" alt="" />
      <div className="side-comment-body">
        <div className="side-comment-name">{comment.user.uname}{pinned ? ' · 置顶' : ''}</div>
        <div className="side-comment-text">{comment.message}</div>
        <div className="side-comment-meta">
          <span>{relativeTime(comment.ctime)}</span>
          <button className={comment.liked ? 'liked' : ''} onClick={() => onLike(comment)}><IconHeart size={11} /> {comment.like || ''} {comment.liked ? '已赞' : ''}</button>
          {comment.rcount > 0 ? <button onClick={() => onReplies(comment)}>{replies ? '收起回复' : `${comment.rcount} 条回复`}</button> : null}
        </div>
        {replies?.length ? (
          <div className="side-replies">
            {replies.map((reply) => (
              <div key={reply.rpid}><b>{reply.user.uname}</b><span>{reply.message}</span></div>
            ))}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function IconUpSmall() {
  return <svg width={14} height={14} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round"><path d="M12 19V5M6 11l6-6 6 6" /></svg>;
}
