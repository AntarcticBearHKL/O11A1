'use strict';

// stdout/stderr are diagnostics only. A shortcut or detached launcher can
// leave them connected to a closed pipe; Node then raises EPIPE while
// `console.error` writes, which Electron reports as a modal main-process
// crash. Swallow errors on those streams instead of taking the app down.
for (const output of [process.stdout, process.stderr]) {
  if (output && typeof output.on === 'function') output.on('error', () => {});
}

const { app, BrowserWindow, ipcMain, screen, session, Tray, Menu, nativeImage, shell, dialog } = require('electron');
const path = require('path');
const fs = require('fs');
const net = require('net');
const { spawn } = require('child_process');
const https = require('https');
const updater = require('./updater.cjs');

const IS_DEV = !!process.env.O11A1_DEV_URL;
/// Desktop shortcut for the standalone Agent window launches the same app with
/// `--agent`; the shell is created hidden so the Agent window is the only one
/// the owner sees (the tray/desktop shortcut can still reveal the main shell).
const START_AGENT = process.argv.includes('--agent');
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
let agentWindow = null;
let tray = null;
let playerSeq = 0;
/** playerId -> { window, playerId, options, cssRect, opened, lastFocused } */
const playerWindows = new Map();
/** Native Rust player windows. They have no BrowserWindow entry. */
const nativePlayers = new Map();
let engineProcess = null;
let engineSocket = null;
let enginePort = 0;
let engineBuffer = '';
let engineReady = false;
let engineWaiters = [];
/** A drag owns its own engine connection so a busy feed/preload call cannot
 *  queue the move gesture behind it. */
let dragInFlight = false;
let engineSeq = 1;
let enginePending = new Map();
let engineStarting = false;
let quitting = false;
let trayHintShown = false;
let windowDrag = null;
let windowResize = null;

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
  if (ffmpeg) {
    env.BILI_FFMPEG_DIR = ffmpeg;
    // libavcodec/libavformat are linked into o11a1-engine and load at process
    // start, before main.cjs can call SetDllDirectory. The Windows loader
    // searches PATH, so put the same folder that holds ffmpeg.exe first.
    env.PATH = `${ffmpeg};${env.PATH || ''}`;
  }
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
  engineProcess.stderr.on('data', (chunk) => {
    // Drain the pipe always, but only echo it in dev or when explicitly
    // asked. A packaged app launched without a console must not write engine
    // lines into a dead stdout/stderr pipe.
    if (!IS_DEV && process.env.O11A1_ENGINE_LOG !== '1') return;
    console.error('[engine]', chunk.trim());
  });
  engineProcess.on('exit', (code) => {
    engineProcess = null;
    engineReady = false;
    engineStarting = false;
    enginePort = 0;
    dragInFlight = false;
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
  enginePort = port;
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

/// User-initiated reveal (tray, taskbar, second launch): restore a minimized
/// window before showing it. Closing a player deliberately does NOT call this;
/// the shell keeps whatever minimized/hidden state the owner left it in.
function showMainWindow() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.show();
  mainWindow.focus();
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
    // The native window can disappear between the map update and the replace
    // (closed by its own titlebar, by the OS, or by a player.closeAll). Drop
    // the dead record and open a fresh window instead of swallowing the click.
    console.error('native replace failed', error);
    nativePlayers.delete(record.playerId);
    try {
      await launchNativePlayer(options || {});
    } catch (fallback) {
      console.error('native relaunch after failed replace', fallback);
      throw fallback;
    }
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
    // The next row was chosen ahead of time (`playlist_next`: sequential next
    // or the pre-picked random row). Fall back to the old sequential lookup
    // for a settings file written before that field existed.
    let nextIndex = String(settings.playlist_next || '')
      ? playlist.findIndex((item) => item && String(item.bvid) === String(settings.playlist_next))
      : -1;
    if (nextIndex < 0 && current >= 0) nextIndex = current + 1;
    const next = nextIndex >= 0 ? playlist[nextIndex] : null;
    if (current >= 0) {
      // The finished row leaves the queue before the next one starts; the
      // surviving rows shift, so the next index has to shift with them.
      playlist.splice(current, 1);
      if (nextIndex > current) nextIndex -= 1;
      record.queueRetired = true;
      await engineCall('settings.patch', {
        playlist,
        playlist_current:
          next && next.bvid && !quitting && nextIndex >= 0 && nextIndex < playlist.length
            ? nextIndex
            : null,
      }, 15000).catch(() => {});
    }
    await engineCall('player.close', { playerId: record.playerId }).catch(() => {});
    nativePlayers.delete(record.playerId);
    if (quitting || !next || !next.bvid) {
      return;
    }
    await launchNativePlayer({
      bvid: next.bvid,
      cid: next.cid || null,
      aid: next.aid || 0,
      title: next.title || '',
      cover: next.cover || '',
      startMs: 0,
      // Playlist auto-advance is stream playback, not an explicit open:
      // never write it to the account's Bilibili history.
      reportHistory: false,
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

/// Resolve as soon as the engine's control socket is up. The startup boot
/// screen can be interactive a few hundred ms before that; a drag requested
/// there starts as soon as the socket exists if the button is still down.
function waitForEngine(timeout = 5000) {
  if (engineReady && enginePort) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      engineWaiters = engineWaiters.filter((item) => item !== waiter);
      reject(new Error('engine not ready'));
    }, timeout);
    const waiter = {
      resolve: () => {
        clearTimeout(timer);
        resolve();
      },
    };
    engineWaiters.push(waiter);
  });
}

/// Move a BrowserWindow through the engine's native cursor-follow loop on a
/// private connection. The main `engineSocket` is intentionally serial: a
/// feed/preload request in flight there would otherwise delay the drag until
/// it finishes, which is exactly what made the first home-feed moments feel
/// undraggable. Keep at most one drag gesture alive at a time.
function beginNativeDrag(hwnd) {
  if (dragInFlight) return false;
  dragInFlight = true;
  let socket = null;
  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    dragInFlight = false;
    if (socket) socket.destroy();
  };
  waitForEngine()
    .then(() => {
      if (!engineReady || !enginePort) {
        finish();
        return;
      }
      socket = net.createConnection({ host: '127.0.0.1', port: enginePort }, () => {
        socket.write(JSON.stringify({
          id: 0,
          method: 'window.drag',
          params: { hwnd },
        }) + '\n');
      });
      socket.setEncoding('utf8');
      // Every engine connection also receives broadcast events, so do not
      // release the gesture slot on the first line. The move loop ends with
      // the reply carrying our id; a close/error also releases it.
      let buffer = '';
      socket.on('data', (chunk) => {
        buffer += chunk;
        let index;
        while ((index = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, index).trim();
          buffer = buffer.slice(index + 1);
          if (!line) continue;
          try {
            const message = JSON.parse(line);
            if (message.id === 0) {
              finish();
              return;
            }
          } catch {
            /* ignore non-JSON lines */
          }
        }
      });
      socket.on('error', finish);
      socket.on('close', finish);
    })
    .catch(finish);
  return true;
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

function createWindow(showOnReady = true) {
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
  mainWindow.once('ready-to-show', () => {
    if (showOnReady) mainWindow.show();
  });
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

function loadAgentRenderer(win) {
  if (IS_DEV) {
    win.loadURL(new URL('agent.html', process.env.O11A1_DEV_URL).toString());
  } else {
    win.loadFile(path.join(ROOT, 'dist', 'agent.html'));
  }
}

/// A second, independent shell window that opens `agent.html` directly. It
/// shares the preload bridge, engine and account with the main shell but has no
/// titlebar/tab chrome of its own.
function createAgentWindow() {
  if (agentWindow && !agentWindow.isDestroyed()) {
    agentWindow.show();
    agentWindow.focus();
    return agentWindow;
  }
  agentWindow = new BrowserWindow({
    width: 980,
    height: 820,
    minWidth: 720,
    minHeight: 560,
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
  agentWindow.once('ready-to-show', () => agentWindow.show());
  agentWindow.on('closed', () => {
    agentWindow = null;
  });
  loadAgentRenderer(agentWindow);
  return agentWindow;
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
    { label: '打开 O11A1', click: () => showMainWindow() },
    { label: '打开 Agent', click: () => createAgentWindow() },
    { label: '检查更新', click: () => checkUpdate().then((result) => broadcast('update:result', result)).catch((error) => broadcast('update:result', { error: error.message })) },
    { type: 'separator' },
    { label: '退出', click: () => { quitting = true; app.quit(); } },
  ]);
  tray.setToolTip('O11A1');
  tray.setContextMenu(menu);
  tray.on('double-click', () => showMainWindow());
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
  // The feed's hover preview plays a signed Bilibili MP4 in an HTML5 video.
  // A file:// page cannot set a Referer, and the CDN rejects the request
  // without the Bilibili one; add it here, media hosts only.
  session.defaultSession.webRequest.onBeforeSendHeaders(
    { urls: ['*://*.bilivideo.com/*', '*://*.akamaized.net/*'] },
    (details, callback) => {
      details.requestHeaders.Referer = 'https://www.bilibili.com/';
      callback({ requestHeaders: details.requestHeaders });
    },
  );
  startEngine();
  // `--agent` is the desktop Agent shortcut: keep the main shell hidden in the
  // tray so the Agent window is the only surface on launch, but reuse the same
  // process, engine and account.
  createWindow(!START_AGENT);
  createTray();
  if (START_AGENT) createAgentWindow();
  try {
    const saved = JSON.parse(fs.readFileSync(path.join(dataRoot(), 'settings.json'), 'utf8'));
    app.setLoginItemSettings({ openAtLogin: !!saved.autostart, path: process.execPath, args: IS_DEV ? [path.join(ROOT, 'electron', 'main.cjs')] : [] });
  } catch {
    /* first run: settings do not exist yet */
  }

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
    else showMainWindow();
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
  app.on('second-instance', (_event, commandLine) => {
    // A second click on the Agent shortcut opens/focuses that window; the
    // normal shortcut keeps its old behaviour and raises the main shell.
    if (commandLine.includes('--agent')) createAgentWindow();
    else showMainWindow();
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
  showMainWindow();
});
function stopWindowDrag() {
  if (!windowDrag) return;
  clearInterval(windowDrag.timer);
  windowDrag = null;
}

function stopWindowResize() {
  if (!windowResize) return;
  clearInterval(windowResize.timer);
  if (windowResize.onBlur) {
    windowResize.win.removeListener('blur', windowResize.onBlur);
  }
  windowResize = null;
}

ipcMain.handle('window:begin-drag', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed() || win.isFullScreen()) return false;
  stopWindowDrag();
  stopWindowResize();
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

ipcMain.handle('window:begin-resize', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed() || win.isFullScreen()) return false;
  stopWindowDrag();
  stopWindowResize();
  if (win.isMaximized()) win.unmaximize();
  const startCursor = screen.getCursorScreenPoint();
  const bounds = win.getBounds();
  const [minWidth, minHeight] = win.getMinimumSize();
  const onBlur = () => stopWindowResize();
  win.once('blur', onBlur);
  const timer = setInterval(() => {
    if (!windowResize || windowResize.win.isDestroyed()) {
      stopWindowResize();
      return;
    }
    const cursor = screen.getCursorScreenPoint();
    const width = Math.max(minWidth, bounds.width + cursor.x - startCursor.x);
    const height = Math.max(minHeight, bounds.height + cursor.y - startCursor.y);
    try {
      windowResize.win.setBounds({ x: bounds.x, y: bounds.y, width, height }, false);
    } catch {
      stopWindowResize();
    }
  }, 10);
  windowResize = { win, timer, onBlur };
  return true;
});

ipcMain.handle('window:end-resize', () => {
  stopWindowResize();
  return true;
});

ipcMain.handle('window:start-drag', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed()) return false;
  const hwnd = Number(win.getNativeWindowHandle().readBigUInt64LE(0).toString());
  beginNativeDrag(hwnd);
  return true;
});
ipcMain.handle('app:navigate-main', (_event, route) => {
  if (!mainWindow || mainWindow.isDestroyed()) return false;
  showMainWindow();
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

// ---- clipboard share links ------------------------------------------------
// Bilibili share text arrives in several shapes: a full video URL, a b23.tv /
// bili2233.cn short link (sometimes without https://), a bare BV/av id, or any
// of those wrapped in 【title】 text. The parser accepts all of them and opens
// the resolved video in the player.
const SHARE_VIDEO_RE = /(?:https?:\/\/)?(?:www\.|m\.)?bilibili\.com\/video\/(BV[0-9A-Za-z]{10}|av(\d+))([^\s【】]*)/i;
const SHARE_SHORT_RE = /(?:https?:\/\/)?(b23\.tv|bili2233\.cn)\/([A-Za-z0-9]+)(\?[^\s【】]*)?/i;
const SHARE_BVID_QUERY_RE = /[?&]bvid=(BV[0-9A-Za-z]{10})/i;
const SHARE_BV_RE = /\bBV[0-9A-Za-z]{10}\b/;
const SHARE_AV_RE = /\bav(\d+)\b/i;
const SHARE_UA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36';

function shareQueryNumber(text, key) {
  const match = String(text || '').match(new RegExp(`[?&]${key}=(\\d+)`, 'i'));
  return match ? Number(match[1]) : 0;
}

function shareTimeMs(text) {
  const match = String(text || '').match(/[?&](?:t|start)=([^&#\s]+)/i);
  if (!match) return 0;
  const raw = decodeURIComponent(match[1]);
  if (/^\d+$/.test(raw)) return Number(raw) * 1000;
  let total = 0;
  for (const part of raw.matchAll(/(\d+)(h|m|s)/gi)) {
    const amount = Number(part[1]);
    const unit = part[2].toLowerCase();
    total += unit === 'h' ? amount * 3600 : unit === 'm' ? amount * 60 : amount;
  }
  return total * 1000;
}

function directShareTarget(text) {
  const match = String(text || '').match(SHARE_VIDEO_RE);
  if (!match) return null;
  const id = match[1] || '';
  const tail = match[3] || '';
  const part = shareQueryNumber(tail, 'p') || 1;
  const startMs = shareTimeMs(tail);
  if (/^BV/i.test(id)) return { bvid: id, part, startMs };
  const aid = Number(match[2] || id.replace(/^av/i, ''));
  return aid > 0 ? { aid, part, startMs } : null;
}

async function fetchShare(url, options = {}, timeoutMs = 12000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(url, {
      redirect: 'follow',
      ...options,
      signal: controller.signal,
      headers: {
        'User-Agent': SHARE_UA,
        Referer: 'https://www.bilibili.com/',
        ...(options.headers || {}),
      },
    });
  } finally {
    clearTimeout(timer);
  }
}

const shortShareCache = new Map();
const SHORT_CACHE_TTL = 10 * 60_000;

async function resolveShortShare(host, path, query, depth = 0) {
  const part = shareQueryNumber(query, 'p') || 1;
  const startMs = shareTimeMs(query);
  if (/^BV[0-9A-Za-z]{10}$/i.test(path)) return { bvid: path, part, startMs };
  if (/^av\d+$/i.test(path)) return { aid: Number(path.replace(/^av/i, '')), part, startMs };
  const url = `https://${host}/${path}${query || ''}`;
  const cached = shortShareCache.get(url);
  if (cached && Date.now() - cached.at < SHORT_CACHE_TTL) return cached.target;
  const remember = (target) => {
    if (target) shortShareCache.set(url, { target, at: Date.now() });
    return target;
  };
  // A b23 link is normally a 302. Read just that Location instead of
  // downloading the whole Bilibili video page, which is the slow part.
  try {
    const head = await fetchShare(url, { method: 'HEAD', redirect: 'manual' }, 8000);
    const location = head.headers.get('location');
    if (location) {
      const absolute = new URL(location, url).toString();
      const direct = directShareTarget(absolute);
      if (direct) return remember(direct);
      const queryBv = absolute.match(SHARE_BVID_QUERY_RE);
      if (queryBv) {
        return remember({
          bvid: queryBv[1],
          part: shareQueryNumber(absolute, 'p') || 1,
          startMs: shareTimeMs(absolute),
        });
      }
      const next = absolute.match(SHARE_SHORT_RE);
      if (next && depth < 3 && (next[1] !== host || next[2] !== path)) {
        const nested = await resolveShortShare(next[1], next[2], next[3] || '', depth + 1);
        if (nested) return remember(nested);
      }
    }
  } catch {
    /* fall back to a normal GET below */
  }
  const response = await fetchShare(url);
  const finalUrl = String(response.url || '');
  const direct = directShareTarget(finalUrl);
  if (direct) return remember(direct);
  // A few short links answer with an HTML jump page instead of a 302.
  const html = await response.text();
  const inline = html.match(/https?:\/\/[^"'<>\\\s]*bilibili\.com\/video\/(?:BV[0-9A-Za-z]{10}|av\d+)[^"'<>\\\s]*/i);
  return remember(inline ? directShareTarget(inline[0]) : null);
}

async function resolveShareTarget(text) {
  const raw = String(text || '').trim();
  if (!raw) return null;
  const direct = directShareTarget(raw);
  if (direct) return direct;
  const short = raw.match(SHARE_SHORT_RE);
  if (short) {
    const target = await resolveShortShare(short[1], short[2], short[3] || '');
    if (target) return target;
  }
  const queryBv = raw.match(SHARE_BVID_QUERY_RE);
  if (queryBv) {
    return {
      bvid: queryBv[1],
      part: shareQueryNumber(raw, 'p') || 1,
      startMs: shareTimeMs(raw),
    };
  }
  const bv = raw.match(SHARE_BV_RE);
  if (bv) return { bvid: bv[0], part: 1, startMs: 0 };
  const av = raw.match(SHARE_AV_RE);
  if (av) return { aid: Number(av[1]), part: 1, startMs: 0 };
  return null;
}

async function openShareText(text) {
  const target = await resolveShareTarget(text);
  if (!target) return { ok: false, error: '没有识别到 B 站视频链接' };
  let bvid = target.bvid || '';
  if (!bvid && target.aid) {
    const detail = await engineCall('video.detail.aid', { aid: target.aid }, 15000).catch(() => null);
    bvid = detail?.bvid || '';
  }
  if (!bvid) return { ok: false, error: '没有解析出视频 BV 号' };
  let cid = null;
  if ((target.part || 1) > 1) {
    const detail = await engineCall('video.detail', { bvid }, 15000).catch(() => null);
    const page = Array.isArray(detail?.pages) ? detail.pages[(target.part || 1) - 1] : null;
    cid = page?.cid || null;
  }
  const options = { bvid, cid, startMs: target.startMs || 0, reportHistory: true };
  const existing = mostRecentNativePlayer();
  if (existing) await replaceNativePlayback(existing, options);
  else await launchNativePlayer(options);
  return { ok: true, bvid, part: target.part || 1 };
}

ipcMain.handle('share:open', async (_event, text) => {
  try {
    return await openShareText(text);
  } catch (error) {
    return { ok: false, error: error?.message || String(error) };
  }
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
