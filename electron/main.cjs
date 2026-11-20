'use strict';

const { app, BrowserWindow, ipcMain, screen, Tray, Menu, nativeImage, shell, dialog } = require('electron');
const path = require('path');
const fs = require('fs');
const net = require('net');
const { spawn } = require('child_process');
const https = require('https');
const updater = require('./updater.cjs');

const IS_DEV = !!process.env.O11A1_DEV_URL;
const ROOT = path.join(__dirname, '..');
const ICON = path.join(ROOT, 'assets', 'o11a1.ico');
const RELEASE_API =
  process.env.O11A1_UPDATE_API ||
  'https://api.github.com/repos/AntarcticBearHKL/O11A1/releases';
const APP_VERSION = require(path.join(ROOT, 'package.json')).version;

// QA / portable-profile hook: keep an isolated userData (and therefore an
// isolated single-instance lock and update cache).
if (process.env.O11A1_USER_DATA) {
  app.setPath('userData', path.resolve(process.env.O11A1_USER_DATA));
}

let mainWindow = null;
let tray = null;
let playerSeq = 0;
/** playerId -> { window, playerId, options, cssRect, opened, lastFocused } */
const playerWindows = new Map();
/** Native Rust player windows. They have no BrowserWindow entry. */
const nativePlayers = new Map();
let engineProcess = null;
let engineSocket = null;
let engineBuffer = '';
let engineReady = false;
let engineWaiters = [];
let engineSeq = 1;
let enginePending = new Map();
let engineStarting = false;
let quitting = false;
let trayHintShown = false;
let windowDrag = null;

function dataRoot() {
  const dir = path.join(app.getPath('userData'), 'data');
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

function migrateLegacyData(dir) {
  if (fs.existsSync(path.join(dir, 'settings.json'))) return;
  const legacy = path.join(process.env.LOCALAPPDATA || '', 'Programs', 'O11A1');
  if (!fs.existsSync(legacy)) return;
  const files = ['settings.json', 'accounts.json', 'continue.json', 'resume.json', 'search_history.json', 'player_state.json', 'fish.db'];
  for (const file of files) {
    const from = path.join(legacy, file);
    if (!fs.existsSync(from)) continue;
    try {
      fs.copyFileSync(from, path.join(dir, file));
    } catch {
      /* a locked fish.db is not fatal */
    }
  }
}

function copyRuntimeAssets() {
  // The packaged app keeps assets in app.asar/runtime extraResources; asar is a
  // file, so it must never be treated as a writable directory.
  if (app.isPackaged) return;
  const assets = path.join(ROOT, 'assets');
  fs.mkdirSync(assets, { recursive: true });
  const ico = path.join(ROOT, 'assets', 'o11a1.ico');
  if (!fs.existsSync(ico)) {
    const fallback = path.join(ROOT, '..', 'bilibili-slint', 'icon', 'o11a1-before-v4.ico');
    if (fs.existsSync(fallback)) fs.copyFileSync(fallback, ico);
  }
  const fonts = path.join(assets, 'fonts');
  const sourceFonts = path.join(ROOT, 'native', 'assets', 'fonts');
  if (!fs.existsSync(fonts) && fs.existsSync(sourceFonts)) {
    fs.cpSync(sourceFonts, fonts, { recursive: true });
  }
}

function findEngine() {
  const candidates = [
    process.resourcesPath && path.join(process.resourcesPath, 'runtime', 'o11a1-engine.exe'),
    path.join(ROOT, 'runtime', 'o11a1-engine.exe'),
    path.join(ROOT, '..', 'bilibili-slint', 'target', 'release', 'o11a1-engine.exe'),
    path.join(ROOT, 'engine', 'target', 'release', 'o11a1-engine.exe'),
  ].filter(Boolean);
  return candidates.find((candidate) => fs.existsSync(candidate)) || null;
}

function findFfmpegDir() {
  const candidates = [
    process.resourcesPath && path.join(process.resourcesPath, 'runtime', 'ffmpeg', 'bin'),
    path.join(ROOT, 'runtime', 'ffmpeg', 'bin'),
    'C:\\Programs\\ffmpeg-unpack\\bin',
  ].filter(Boolean);
  return candidates.find((dir) => fs.existsSync(path.join(dir, 'ffmpeg.exe'))) || '';
}

function startEngine() {
  if (engineProcess || engineStarting) return;
  const exe = findEngine();
  if (!exe) {
    console.error('o11a1-engine.exe not found; run scripts/build-engine.ps1');
    return;
  }
  engineStarting = true;
  const home = dataRoot();
  migrateLegacyData(home);
  const env = {
    ...process.env,
    O11A1_HOME: home,
    BILI_FEED_PAGES: '1',
  };
  const ffmpeg = findFfmpegDir();
  if (ffmpeg) env.BILI_FFMPEG_DIR = ffmpeg;
  engineProcess = spawn(exe, [], {
    cwd: path.dirname(exe),
    env,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  engineProcess.stdout.setEncoding('utf8');
  engineProcess.stdout.on('data', (chunk) => {
    for (const line of chunk.split(/\r?\n/)) {
      const match = line.match(/^O11A1-ENGINE READY (\d+)/);
      if (match) {
        connectEngine(Number(match[1]));
      }
    }
  });
  engineProcess.stderr.setEncoding('utf8');
  engineProcess.stderr.on('data', (chunk) => console.error('[engine]', chunk.trim()));
  engineProcess.on('exit', (code) => {
    engineProcess = null;
    engineReady = false;
    engineStarting = false;
    if (engineSocket) engineSocket.destroy();
    engineSocket = null;
    for (const [, pending] of enginePending) pending.reject(new Error(`引擎已退出（${code}）`));
    enginePending.clear();
    for (const entry of playerEntries()) {
      if (!entry.window.isDestroyed()) entry.window.close();
    }
    if (!quitting) setTimeout(startEngine, 1200);
  });
}

function connectEngine(port) {
  const socket = net.createConnection({ host: '127.0.0.1', port }, () => {
    engineSocket = socket;
    engineReady = true;
    engineStarting = false;
    for (const waiter of engineWaiters) waiter.resolve();
    engineWaiters = [];
    broadcast('engine:status', { ok: true });
  });
  socket.setEncoding('utf8');
  socket.on('data', (chunk) => {
    engineBuffer += chunk;
    let index;
    while ((index = engineBuffer.indexOf('\n')) >= 0) {
      const line = engineBuffer.slice(0, index).trim();
      engineBuffer = engineBuffer.slice(index + 1);
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        continue;
      }
      if (message.event) {
        handleEngineEvent(message);
      } else if (message.id != null && enginePending.has(message.id)) {
        const pending = enginePending.get(message.id);
        enginePending.delete(message.id);
        clearTimeout(pending.timer);
        if (message.ok) pending.resolve(message.data);
        else pending.reject(new Error(message.error || '引擎调用失败'));
      }
    }
  });
  socket.on('error', (error) => console.error('engine socket:', error.message));
  socket.on('close', () => {
    engineReady = false;
    engineSocket = null;
  });
}

function playerEntries() {
  return [...playerWindows.values()].filter((entry) => entry.window && !entry.window.isDestroyed());
}

function entryByPlayerId(playerId) {
  const entry = playerWindows.get(playerId);
  return entry && entry.window && !entry.window.isDestroyed() ? entry : null;
}

function entryForWindow(win) {
  if (!win) return null;
  return playerEntries().find((entry) => entry.window === win) || null;
}

function entryForEvent(event) {
  return entryForWindow(BrowserWindow.fromWebContents(event.sender));
}

function sendToPlayer(entry, channel, data) {
  if (entry && entry.window && !entry.window.isDestroyed()) {
    entry.window.webContents.send(channel, data);
  }
}

async function launchNativePlayer(options) {
  playerSeq += 1;
  const playerId = `native-${Date.now()}-${playerSeq}`;
  const record = {
    playerId,
    options: { ...(options || {}) },
    endedHandled: false,
    advancing: false,
    opened: false,
    lastFocused: Date.now(),
    focusSeq: 0,
    replacing: false,
    queueRetired: false,
  };
  nativePlayers.set(playerId, record);
  try {
    await engineCall('player.open', { ...record.options, playerId }, 120000);
    record.opened = true;
    record.lastFocused = Date.now();
    await queueEnqueue(record.options).catch((error) => {
      console.error('native queue enqueue failed', error);
    });
    return playerId;
  } catch (error) {
    nativePlayers.delete(playerId);
    throw error;
  }
}

/// The native player a card left-click should take over: the live window that
/// was focused last. `null` means there is nothing to replace yet.
function mostRecentNativePlayer() {
  let best = null;
  for (const record of nativePlayers.values()) {
    if (!record || !record.opened) continue;
    if (!best || (record.lastFocused || 0) > (best.lastFocused || 0)) best = record;
  }
  return best;
}

/// Left-click: swap the media inside the most recently focused native window.
/// The window, its size and its chrome stay exactly as they are; only the
/// queue row and the playing source change.
async function replaceNativePlayback(record, options) {
  if (!record || record.replacing) return;
  record.replacing = true;
  try {
    await engineCall('player.replace', { ...(options || {}), playerId: record.playerId }, 120000);
    const oldBvid = String(record.options?.bvid || '');
    record.options = { ...(options || {}) };
    record.endedHandled = false;
    record.queueRetired = false;
    record.lastFocused = Date.now();
    await queueReplace(oldBvid, record.options).catch((error) => {
      console.error('native queue replace failed', error);
    });
  } catch (error) {
    console.error('native replace failed', error);
  } finally {
    record.replacing = false;
  }
}

function queueEntry(options) {
  return {
    bvid: String(options?.bvid || ''),
    cid: Number(options?.cid) || 0,
    aid: Number(options?.aid) || 0,
    title: String(options?.title || ''),
    owner: String(options?.owner || ''),
    duration: options?.duration ? String(options.duration) : '',
    cover: String(options?.cover || ''),
  };
}

function mergeQueueRow(base, entry) {
  const next = { ...(base || {}), bvid: entry.bvid };
  for (const key of ['title', 'owner', 'duration', 'cover']) {
    if (String(entry[key] || '').trim()) next[key] = entry[key];
  }
  if (Number(entry.cid) > 0) next.cid = entry.cid;
  if (Number(entry.aid) > 0) next.aid = entry.aid;
  return next;
}

async function readQueue() {
  const settings = await engineCall('settings.get', {}, 15000);
  const playlist = Array.isArray(settings.playlist)
    ? settings.playlist.map((row) => ({ ...row }))
    : [];
  const current = Number.isInteger(settings.playlist_current)
    ? settings.playlist_current
    : -1;
  return { playlist, current };
}

async function writeQueue(playlist, current) {
  if (playlist.length > 200) playlist.splice(0, playlist.length - 200);
  await engineCall('settings.patch', {
    playlist,
    playlist_current: current >= 0 && current < playlist.length ? current : null,
  }, 15000);
}

/// A new player window: dedupe by bvid and make its row current.
async function queueEnqueue(options) {
  const entry = queueEntry(options);
  if (!entry.bvid) return;
  const { playlist, current } = await readQueue();
  const index = playlist.findIndex((row) => row && String(row.bvid) === entry.bvid);
  let next = current;
  if (index >= 0) {
    playlist[index] = mergeQueueRow(playlist[index], entry);
    next = index;
  } else {
    playlist.push(mergeQueueRow({ bvid: entry.bvid }, entry));
    next = playlist.length - 1;
  }
  await writeQueue(playlist, next);
}

/// Left-click replacement: the row of the video that was taken over leaves the
/// queue and the new video is inserted where it was, as the current row.
async function queueReplace(oldBvid, options) {
  if (!oldBvid || oldBvid === String(options?.bvid || '')) {
    await queueEnqueue(options);
    return;
  }
  const entry = queueEntry(options);
  const { playlist, current } = await readQueue();
  let index = playlist.findIndex((row) => row && String(row.bvid) === oldBvid);
  let next = current;
  const stale = entry.bvid
    ? playlist.findIndex((row) => row && String(row.bvid) === entry.bvid)
    : -1;
  if (stale >= 0 && stale !== index) {
    playlist.splice(stale, 1);
    if (index > stale) index -= 1;
    if (next > stale) next -= 1;
  }
  if (!entry.bvid) {
    if (index >= 0) {
      playlist.splice(index, 1);
      if (next === index) next = -1;
      else if (next > index) next -= 1;
    }
    await writeQueue(playlist, next);
    return;
  }
  const row = mergeQueueRow({ bvid: entry.bvid }, entry);
  if (index >= 0) {
    playlist.splice(index, 1, row);
    next = index;
  } else {
    playlist.push(row);
    next = playlist.length - 1;
  }
  await writeQueue(playlist, next);
}

/// Drop the row a player window was launched from. Called when the video
/// finishes and when the user closes the window by hand: the queue is a
/// "what plays next" list, not a history, so a row leaves once its window is
/// done. The main window's queue popover updates through settings.changed.
/// Idempotent per player record.
async function retireNativeQueueEntry(record) {
  if (!record || record.queueRetired) return;
  record.queueRetired = true;
  const bvid = String(record.options?.bvid || '');
  if (!bvid) return;
  try {
    const settings = await engineCall('settings.get', {}, 15000);
    const playlist = Array.isArray(settings.playlist)
      ? settings.playlist.map((row) => ({ ...row }))
      : [];
    const removed = playlist.findIndex((row) => row && String(row.bvid) === bvid);
    if (removed < 0) return;
    playlist.splice(removed, 1);
    let current = Number.isInteger(settings.playlist_current) ? settings.playlist_current : -1;
    if (current === removed) current = -1;
    else if (current > removed) current -= 1;
    await engineCall('settings.patch', {
      playlist,
      playlist_current: current >= 0 && current < playlist.length ? current : null,
    }, 15000);
  } catch (error) {
    record.queueRetired = false;
    console.error('native queue retire failed', error);
  }
}

async function advanceNativePlaylist(record, loopMode) {
  if (record.advancing) return;
  record.advancing = true;
  try {
    // 0 = pause at the end (native keeps the window), 1 = loop (native never
    // reports ended), 2 = next / close. The mode comes from the player's own
    // state so a menu change is effective immediately.
    if (Number(loopMode) !== 2) {
      return;
    }
    const settings = await engineCall('settings.get', {}, 15000);
    const playlist = Array.isArray(settings.playlist)
      ? settings.playlist.map((row) => ({ ...row }))
      : [];
    const bvid = String(record.options?.bvid || '');
    let current = Number.isInteger(settings.playlist_current) ? settings.playlist_current : -1;
    if (current < 0 || !playlist[current] || String(playlist[current].bvid) !== bvid) {
      current = playlist.findIndex((item) => item && String(item.bvid) === bvid);
    }
    const next = current >= 0 ? playlist[current + 1] : null;
    if (current >= 0) {
      // The finished row leaves the queue before the next one starts; the
      // following row slides into `current`, which is also where the current
      // marker has to point for the next player.
      playlist.splice(current, 1);
      record.queueRetired = true;
      await engineCall('settings.patch', {
        playlist,
        playlist_current:
          next && next.bvid && !quitting && current < playlist.length ? current : null,
      }, 15000).catch(() => {});
    }
    await engineCall('player.close', { playerId: record.playerId }).catch(() => {});
    nativePlayers.delete(record.playerId);
    if (quitting || !next || !next.bvid) return;
    await launchNativePlayer({
      bvid: next.bvid,
      cid: next.cid || null,
      aid: next.aid || 0,
      title: next.title || '',
      cover: next.cover || '',
      startMs: 0,
    });
  } catch (error) {
    console.error('native playlist advance failed', error);
  } finally {
    record.advancing = false;
  }
}

function handleEngineEvent(message) {
  const data = message.data || {};
  const entry = data.id ? entryByPlayerId(String(data.id)) : null;
  const native = data.id ? nativePlayers.get(String(data.id)) : null;
  if (message.event === 'player.state') {
    if (native) {
      const focus = Number(data.focusSeq) || 0;
      if (focus > (native.focusSeq || 0)) {
        native.focusSeq = focus;
        native.lastFocused = Date.now();
      }
      if (data.open === false) {
        nativePlayers.delete(native.playerId);
      } else if (data.ended) {
        if (!native.endedHandled) {
          native.endedHandled = true;
          const loopMode = Number.isInteger(data.loopMode) ? data.loopMode : 2;
          void advanceNativePlaylist(native, loopMode);
        }
      } else {
        native.endedHandled = false;
      }
    }
    sendToPlayer(entry, 'player:state', data);
  } else if (message.event === 'player.opened') {
    if (entry) entry.opened = true;
    sendToPlayer(entry, 'player:opened', data);
  } else if (message.event === 'player.click') {
    sendToPlayer(entry, 'player:click', data);
  } else if (message.event === 'player.doubleclick') {
    sendToPlayer(entry, 'player:doubleclick', data);
  } else if (message.event === 'player.mousemove') {
    sendToPlayer(entry, 'player:mousemove', data);
  } else if (message.event === 'player.danmaku') {
    sendToPlayer(entry, 'player:danmaku', data);
  } else if (message.event === 'player.longpress') {
    sendToPlayer(entry, 'player:longpress', data);
  } else if (message.event === 'player.closed') {
    // The window closed itself: manually (title-bar close, Alt+F4, taskbar)
    // or it went away before the advance path picked it up. Either way its
    // queue row is done and leaves the list.
    if (native) {
      nativePlayers.delete(native.playerId);
      if (!quitting) void retireNativeQueueEntry(native);
    } else if (data.id) {
      nativePlayers.delete(String(data.id));
    }
  } else if (message.event === 'account.changed') {
    broadcast('account:changed', message.data);
  } else if (message.event === 'settings.changed') {
    broadcast('settings:changed', message.data);
  }
}

function broadcast(channel, data) {
  for (const win of BrowserWindow.getAllWindows()) {
    if (!win.isDestroyed()) win.webContents.send(channel, data);
  }
}

function engineCall(method, params = {}, timeout = 60000) {
  return new Promise((resolve, reject) => {
    if (!engineReady || !engineSocket) {
      reject(new Error('视频引擎尚未就绪'));
      return;
    }
    const id = engineSeq++;
    const timer = setTimeout(() => {
      enginePending.delete(id);
      reject(new Error(`引擎调用超时：${method}`));
    }, timeout);
    enginePending.set(id, { resolve, reject, timer });
    engineSocket.write(JSON.stringify({ id, method, params }) + '\n');
  });
}

function physicalRect(win, rect) {
  if (!win || win.isDestroyed() || !rect) return null;
  const bounds = win.getContentBounds();
  const display = screen.getDisplayMatching(bounds);
  const scale = display.scaleFactor || 1;
  return {
    x: Math.round(bounds.x + rect.x * scale),
    y: Math.round(bounds.y + rect.y * scale),
    width: Math.max(2, Math.round(rect.width * scale)),
    height: Math.max(2, Math.round(rect.height * scale)),
  };
}

function pushPlayerRect(entry) {
  if (!entry || !entry.window || entry.window.isDestroyed()) return;
  const rect = physicalRect(entry.window, entry.cssRect);
  if (!rect || !entry.opened) return;
  engineCall('player.rect', { playerId: entry.playerId, ...rect }).catch(() => {});
}

function showPlayerPopup(entry, visible) {
  if (!entry || !entry.opened) return;
  engineCall('player.visible', { playerId: entry.playerId, visible }).catch(() => {});
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1280,
    height: 800,
    minWidth: 980,
    minHeight: 640,
    frame: false,
    show: false,
    transparent: true,
    backgroundColor: '#00000000',
    icon: fs.existsSync(ICON) ? ICON : undefined,
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
      spellcheck: false,
    },
  });
  mainWindow.once('ready-to-show', () => mainWindow.show());
  mainWindow.on('closed', stopWindowDrag);
  mainWindow.on('close', (event) => {
    if (!quitting) {
      event.preventDefault();
      mainWindow.hide();
      if (!trayHintShown) {
        trayHintShown = true;
        if (tray) {
          tray.displayBalloon({
            title: 'O11A1',
            content: '已最小化到托盘，双击图标可以回来。',
            iconType: 'info',
          });
        }
      }
    }
  });
  loadRenderer(mainWindow, '');
  return mainWindow;
}

function loadRenderer(win, hash) {
  if (IS_DEV) {
    const url = process.env.O11A1_DEV_URL + (hash ? `#/${hash}` : '');
    win.loadURL(url);
  } else {
    win.loadFile(path.join(ROOT, 'dist', 'index.html'), hash ? { hash: `/${hash}` } : {});
  }
}

function createPlayerWindow(options) {
  playerSeq += 1;
  const playerId = `p${Date.now()}-${playerSeq}`;
  const cascade = (playerSeq - 1) % 3;
  const rect = { width: 1000, height: 600, x: 80 + cascade * 46, y: 60 + cascade * 46 };
  const win = new BrowserWindow({
    ...rect,
    minWidth: 720,
    minHeight: 460,
    frame: false,
    show: false,
    backgroundColor: '#101014',
    icon: fs.existsSync(ICON) ? ICON : undefined,
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
      spellcheck: false,
    },
  });
  const entry = {
    window: win,
    playerId,
    options: options || {},
    cssRect: null,
    opened: false,
    lastFocused: Date.now(),
    miniBounds: null,
  };
  playerWindows.set(playerId, entry);
  win.once('ready-to-show', () => win.show());
  win.on('focus', () => { entry.lastFocused = Date.now(); });
  win.on('resize', () => {
    pushPlayerRect(entry);
    if (!win.isDestroyed()) win.webContents.send('player:viewport', win.getContentBounds());
  });
  win.on('move', () => pushPlayerRect(entry));
  win.on('minimize', () => showPlayerPopup(entry, false));
  win.on('restore', () => showPlayerPopup(entry, true));
  win.on('close', () => {
    showPlayerPopup(entry, false);
    engineCall('player.close', { playerId }).catch(() => {});
    entry.opened = false;
  });
  win.on('closed', () => {
    playerWindows.delete(playerId);
  });
  loadRenderer(win, 'player');
  return entry;
}

function createTray() {
  if (!fs.existsSync(ICON)) return;
  tray = new Tray(ICON);
  const menu = Menu.buildFromTemplate([
    { label: '打开 O11A1', click: () => { if (mainWindow) { mainWindow.show(); mainWindow.focus(); } } },
    { label: '检查更新', click: () => checkUpdate().then((result) => broadcast('update:result', result)).catch((error) => broadcast('update:result', { error: error.message })) },
    { type: 'separator' },
    { label: '退出', click: () => { quitting = true; app.quit(); } },
  ]);
  tray.setToolTip('O11A1');
  tray.setContextMenu(menu);
  tray.on('double-click', () => { if (mainWindow) { mainWindow.show(); mainWindow.focus(); } });
}

function versionParts(value) {
  return String(value || '').replace(/^v/i, '').split('.').map((part) => parseInt(part, 10) || 0);
}

function newer(remote, local) {
  const a = versionParts(remote);
  const b = versionParts(local);
  for (let i = 0; i < Math.max(a.length, b.length); i += 1) {
    if ((a[i] || 0) > (b[i] || 0)) return true;
    if ((a[i] || 0) < (b[i] || 0)) return false;
  }
  return false;
}

/// One in-flight/completed installer download. The shell mirrors this to the
/// renderer as `update:progress` / `update:ready`.
let updateDownload = null;

function updatesDir() {
  return path.join(app.getPath('userData'), 'updates');
}

function updatePublicState() {
  if (!updateDownload) return null;
  const total = updateDownload.total || 0;
  const received = updateDownload.received || 0;
  return {
    version: updateDownload.version,
    status: updateDownload.status,
    received,
    total,
    percent: total > 0 ? Math.min(100, (received / total) * 100) : 0,
    path: updateDownload.status === 'ready' ? updateDownload.path : '',
    url: updateDownload.url || '',
    error: updateDownload.error || '',
  };
}

function broadcastUpdateProgress() {
  broadcast('update:progress', updatePublicState());
}

function pruneOldInstallers(keep) {
  try {
    for (const name of fs.readdirSync(updatesDir())) {
      const full = path.join(updatesDir(), name);
      if (full !== keep && /\.exe$/i.test(name)) fs.rmSync(full, { force: true });
    }
  } catch {
    /* the directory may not exist yet */
  }
}

/// Drop installers for versions this build already includes (the app updated,
/// but the downloaded setup stayed behind).
function cleanInstalledUpdates() {
  try {
    for (const name of fs.readdirSync(updatesDir())) {
      if (!/\.exe$/i.test(name)) continue;
      const match = name.match(/(\d+\.\d+\.\d+)/);
      if (match && !newer(`v${match[1]}`, APP_VERSION)) {
        fs.rmSync(path.join(updatesDir(), name), { force: true });
      }
    }
  } catch {
    /* first run: no updates directory */
  }
}

async function downloadUpdate(update, target) {
  try {
    const size = await updater.downloadAsset(update.asset, target, (received, total) => {
      if (!updateDownload || updateDownload.version !== update.version) return;
      updateDownload.received = received;
      if (total > 0) updateDownload.total = total;
      broadcastUpdateProgress();
    });
    if (!updateDownload || updateDownload.version !== update.version) return;
    updateDownload.received = size;
    updateDownload.total = size;
    updateDownload.status = 'ready';
    broadcast('update:ready', updatePublicState());
    broadcastUpdateProgress();
    pruneOldInstallers(target);
  } catch (error) {
    if (!updateDownload || updateDownload.version !== update.version) return;
    updateDownload.status = 'error';
    updateDownload.error = String(error.message || error);
    broadcastUpdateProgress();
  }
}

/// Start (or reuse) the background download for one release. A complete file
/// from an earlier session is picked up without another 200 MB transfer.
function ensureUpdateDownload(update) {
  if (!update || !update.asset || !update.asset.url || !update.asset.name) return;
  const version = update.version;
  if (updateDownload && updateDownload.version === version) {
    if (updateDownload.status === 'downloading' || updateDownload.status === 'ready') return;
  }
  const target = path.join(updatesDir(), update.asset.name);
  try {
    const stat = fs.statSync(target);
    if (!update.asset.size || stat.size === update.asset.size) {
      updateDownload = {
        version,
        path: target,
        received: stat.size,
        total: stat.size,
        status: 'ready',
        url: update.url,
        notes: update.notes,
        error: '',
      };
      broadcast('update:ready', updatePublicState());
      return;
    }
    fs.rmSync(target, { force: true });
  } catch {
    /* not downloaded yet */
  }
  updateDownload = {
    version,
    path: target,
    received: 0,
    total: update.asset.size || 0,
    status: 'downloading',
    url: update.url,
    notes: update.notes,
    error: '',
  };
  broadcastUpdateProgress();
  void downloadUpdate(update, target);
}

/// Quit, then let a detached helper run the silent NSIS installer and relaunch
/// the freshly installed app. The helper waits for this process (and the Rust
/// engine) to exit first, so no file is locked during the install.
function launchInstallerAfterExit(installer, relaunch) {
  const targets = [process.pid];
  if (engineProcess && engineProcess.pid) targets.push(engineProcess.pid);
  const script = updater.installerScript(installer, relaunch, targets);
  const encoded = updater.encodePowerShell(script);
  const helper = spawn(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', '-EncodedCommand', encoded],
    { detached: true, stdio: 'ignore', windowsHide: true },
  );
  helper.unref();
}

function checkUpdate() {
  return new Promise((resolve, reject) => {
    const client = new URL(RELEASE_API).protocol === 'http:' ? require('http') : https;
    const request = client.get(RELEASE_API, { headers: { 'User-Agent': 'O11A1-Electron', Accept: 'application/vnd.github+json' } }, (response) => {
      let body = '';
      response.on('data', (chunk) => { body += chunk; });
      response.on('end', () => {
        try {
          const releases = JSON.parse(body);
          if (!Array.isArray(releases) || releases.length === 0) {
            resolve({ current: APP_VERSION, update: null, download: updatePublicState() });
            return;
          }
          const release = releases.find((item) => !item.draft && !item.prerelease) || releases[0];
          const version = String(release.tag_name || release.name || '').trim();
          const installer = (release.assets || []).find(
            (asset) =>
              /\.exe$/i.test(asset.name || '') &&
              !/uninstaller/i.test(asset.name || '') &&
              Number(asset.size) > 0,
          );
          const update = newer(version, APP_VERSION)
            ? {
                version,
                notes: release.body || '',
                url: release.html_url || '',
                asset: installer
                  ? {
                      name: installer.name,
                      size: Number(installer.size) || 0,
                      url: installer.browser_download_url,
                      digest: installer.digest || '',
                    }
                  : null,
              }
            : null;
          if (update) ensureUpdateDownload(update);
          resolve({
            current: APP_VERSION,
            latest: version,
            update,
            download:
              updateDownload && update && updateDownload.version === update.version
                ? updatePublicState()
                : null,
          });
        } catch (error) {
          reject(error);
        }
      });
    });
    request.on('error', reject);
    request.setTimeout(15000, () => {
      request.destroy(new Error('检查更新超时'));
    });
  });
}

app.on('web-contents-created', (_event, contents) => {
  contents.on('context-menu', (event) => event.preventDefault());
});

app.on('before-quit', stopWindowDrag);

app.whenReady().then(() => {
  app.setAppUserModelId('com.o11a1.electron');
  cleanInstalledUpdates();
  copyRuntimeAssets();
  startEngine();
  createWindow();
  createTray();
  try {
    const saved = JSON.parse(fs.readFileSync(path.join(dataRoot(), 'settings.json'), 'utf8'));
    app.setLoginItemSettings({ openAtLogin: !!saved.autostart, path: process.execPath, args: IS_DEV ? [path.join(ROOT, 'electron', 'main.cjs')] : [] });
  } catch {
    /* first run: settings do not exist yet */
  }

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
    else if (mainWindow) mainWindow.show();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin' && quitting) app.quit();
});

app.on('before-quit', () => {
  quitting = true;
  engineCall('player.closeAll').catch(() => {});
  if (engineSocket) engineSocket.destroy();
  if (engineProcess) {
    try { engineProcess.kill(); } catch { /* already gone */ }
  }
});

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (mainWindow) {
      mainWindow.show();
      mainWindow.focus();
    }
  });
}

ipcMain.handle('engine:call', async (event, method, params) => {
  try {
    const entry = entryForEvent(event);
    const scoped = entry && params && typeof params === 'object'
      ? { ...params, playerId: entry.playerId }
      : params;
    // The tool tester may legitimately run ASR for half an hour; every other
    // call keeps the short default.
    const timeout = method === 'fish.tool.run' ? 2_000_000 : 120000;
    return { ok: true, data: await engineCall(method, scoped, timeout) };
  } catch (error) {
    return { ok: false, error: error.message };
  }
});

ipcMain.handle('engine:status', () => ({ ready: engineReady, error: engineReady ? null : '正在启动视频引擎…' }));

ipcMain.handle('app:info', () => ({
  version: APP_VERSION,
  userData: app.getPath('userData'),
  dataRoot: dataRoot(),
  platform: process.platform,
  electron: process.versions.electron,
}));

ipcMain.handle('window:minimize', (event) => {
  BrowserWindow.fromWebContents(event.sender)?.minimize();
});
ipcMain.handle('window:maximize', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win) return false;
  if (win.isMaximized()) win.unmaximize();
  else win.maximize();
  return win.isMaximized();
});
ipcMain.handle('window:isMaximized', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  return !!win && win.isMaximized();
});
ipcMain.handle('window:close', (event) => {
  BrowserWindow.fromWebContents(event.sender)?.close();
});
ipcMain.handle('window:hide', (event) => {
  BrowserWindow.fromWebContents(event.sender)?.hide();
});
ipcMain.handle('window:show', () => {
  mainWindow?.show();
  mainWindow?.focus();
});
function stopWindowDrag() {
  if (!windowDrag) return;
  clearInterval(windowDrag.timer);
  windowDrag = null;
}

ipcMain.handle('window:begin-drag', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed() || win.isFullScreen()) return false;
  stopWindowDrag();
  if (win.isMaximized()) win.unmaximize();
  const startCursor = screen.getCursorScreenPoint();
  const [startX, startY] = win.getPosition();
  const timer = setInterval(() => {
    if (!windowDrag || windowDrag.win.isDestroyed()) {
      stopWindowDrag();
      return;
    }
    const cursor = screen.getCursorScreenPoint();
    try {
      windowDrag.win.setPosition(
        startX + cursor.x - startCursor.x,
        startY + cursor.y - startCursor.y,
        false,
      );
    } catch {
      stopWindowDrag();
    }
  }, 10);
  windowDrag = { win, timer };
  return true;
});

ipcMain.handle('window:end-drag', () => {
  stopWindowDrag();
  return true;
});

ipcMain.handle('window:start-drag', async (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed()) return false;
  const hwnd = Number(win.getNativeWindowHandle().readBigUInt64LE(0).toString());
  await engineCall('window.drag', { hwnd }).catch(() => {});
  return true;
});
ipcMain.handle('app:navigate-main', (_event, route) => {
  if (!mainWindow || mainWindow.isDestroyed()) return false;
  mainWindow.show();
  mainWindow.focus();
  mainWindow.webContents.send('app:navigate', String(route || ''));
  return true;
});
ipcMain.handle('app:quit', () => {
  quitting = true;
  app.quit();
});
ipcMain.handle('window:fullscreen', (event, on) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  win?.setFullScreen(!!on);
  return !!win && win.isFullScreen();
});
ipcMain.handle('window:isFullscreen', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  return !!win && win.isFullScreen();
});
ipcMain.handle('window:topmost', (event, on) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win) return false;
  win.setAlwaysOnTop(!!on);
  return win.isAlwaysOnTop();
});
ipcMain.handle('player:mini', (event, on) => {
  const entry = entryForEvent(event);
  if (!entry) return false;
  const win = entry.window;
  if (on) {
    entry.miniBounds = win.getBounds();
    win.setAlwaysOnTop(true);
    win.setMinimumSize(360, 200);
    win.setSize(480, 270);
  } else {
    win.setAlwaysOnTop(false);
    if (entry.miniBounds) {
      win.setBounds(entry.miniBounds);
      entry.miniBounds = null;
    }
    win.setMinimumSize(720, 460);
  }
  setTimeout(() => pushPlayerRect(entry), 60);
  return !!on;
});
ipcMain.handle('shell:open', (_event, url) => {
  if (/^https?:/i.test(url)) shell.openExternal(url);
});
ipcMain.handle('shell:openPath', (_event, target) => {
  shell.openPath(target);
});
ipcMain.handle('dialog:error', (_event, title, message) => {
  dialog.showMessageBox({ type: 'error', title, message });
});

ipcMain.handle('player:launch', async (_event, options, newWindow) => {
  // Left-click: take over the most recently focused player window, keeping
  // its rectangle and chrome. Right-click asks for a new window explicitly.
  if (!newWindow) {
    const target = mostRecentNativePlayer();
    if (target) {
      await replaceNativePlayback(target, options || {});
      return true;
    }
  }
  await launchNativePlayer(options || {});
  return true;
});
ipcMain.handle('player:options', (event) => {
  const entry = entryForEvent(event);
  return entry ? entry.options || {} : {};
});
ipcMain.handle('player:open', async (event, options) => {
  const entry = entryForEvent(event);
  if (!entry) throw new Error('播放器窗口未创建');
  entry.options = { ...(entry.options || {}), ...(options || {}) };
  const rect = physicalRect(entry.window, entry.cssRect || { x: 0, y: 42, width: 1000, height: 500 });
  const host = rect
    ? {
        parent: Number(entry.window.getNativeWindowHandle().readBigUInt64LE(0).toString()),
        ...rect,
      }
    : null;
  const result = await engineCall(
    'player.open',
    { ...entry.options, playerId: entry.playerId, host },
    90000,
  );
  entry.opened = true;
  pushPlayerRect(entry);
  return result;
});
ipcMain.handle('player:rect', async (event, rect) => {
  const entry = entryForEvent(event);
  if (!entry) return false;
  entry.cssRect = rect;
  pushPlayerRect(entry);
  return true;
});
ipcMain.handle('player:debug-bounds', (event) => {
  const entry = entryForEvent(event);
  if (!entry) return { bounds: null, content: null, css: null, physical: null };
  return {
    bounds: entry.window.getBounds(),
    content: entry.window.getContentBounds(),
    css: entry.cssRect,
    physical: physicalRect(entry.window, entry.cssRect),
    playerId: entry.playerId,
  };
});
ipcMain.handle('player:visible', (event, visible) => {
  const entry = entryForEvent(event);
  showPlayerPopup(entry, !!visible);
  return true;
});
ipcMain.handle('player:command', async (event, cmd, value) => {
  const entry = entryForEvent(event);
  if (!entry) throw new Error('播放器窗口未创建');
  return engineCall('player.command', { playerId: entry.playerId, cmd, value });
});
ipcMain.handle('player:status', async (event) => {
  const entry = entryForEvent(event);
  if (!entry) return { open: false };
  return engineCall('player.status', { playerId: entry.playerId });
});
ipcMain.handle('player:close', async (event) => {
  const entry = entryForEvent(event);
  if (!entry) return;
  showPlayerPopup(entry, false);
  await engineCall('player.close', { playerId: entry.playerId }).catch(() => {});
  entry.opened = false;
  if (!entry.window.isDestroyed()) entry.window.close();
});
ipcMain.handle('player:detach', (event) => {
  entryForEvent(event)?.window.close();
});

ipcMain.handle('tray:balloon', (_event, title, content) => {
  if (tray) tray.displayBalloon({ title: title || 'O11A1', content: content || '', iconType: 'info' });
});
ipcMain.handle('update:check', () => checkUpdate());
ipcMain.handle('update:install', () => {
  const download = updateDownload;
  if (
    !download ||
    download.status !== 'ready' ||
    !download.path ||
    !fs.existsSync(download.path)
  ) {
    return { ok: false, error: '安装包尚未下载完成' };
  }
  launchInstallerAfterExit(download.path, process.execPath);
  quitting = true;
  app.quit();
  return { ok: true };
});
ipcMain.handle('update:open', (_event, url) => {
  if (/^https?:/i.test(url)) shell.openExternal(url);
});
ipcMain.handle('autostart:set', (_event, enabled) => {
  const exe = process.execPath;
  app.setLoginItemSettings({
    openAtLogin: !!enabled,
    path: exe,
    args: IS_DEV ? [path.join(ROOT, 'electron', 'main.cjs')] : [],
  });
  return app.getLoginItemSettings().openAtLogin;
});
ipcMain.handle('autostart:get', () => app.getLoginItemSettings().openAtLogin);
