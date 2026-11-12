'use strict';

const { app, BrowserWindow, ipcMain, screen, Tray, Menu, nativeImage, shell, dialog } = require('electron');
const path = require('path');
const fs = require('fs');
const net = require('net');
const { spawn } = require('child_process');
const https = require('https');

const IS_DEV = !!process.env.O11A1_DEV_URL;
const ROOT = path.join(__dirname, '..');
const ICON = path.join(ROOT, 'assets', 'o11a1.ico');
const RELEASE_API = 'https://api.github.com/repos/AntarcticBearHKL/O11A1/releases';
const APP_VERSION = require(path.join(ROOT, 'package.json')).version;

let mainWindow = null;
let tray = null;
let playerSeq = 0;
/** playerId -> { window, playerId, options, cssRect, opened, lastFocused } */
const playerWindows = new Map();
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

function handleEngineEvent(message) {
  const data = message.data || {};
  const entry = data.id ? entryByPlayerId(String(data.id)) : null;
  if (message.event === 'player.state') {
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
    backgroundColor: '#fafafc',
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
  mainWindow.webContents.on('before-input-event', (event, input) => {
    if (input.key === 'F12' && input.type === 'keyDown') {
      event.preventDefault();
      const win = BrowserWindow.fromWebContents(event.sender);
      if (win) win.webContents.send('app:hud-toggle');
    }
  });
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

function checkUpdate() {
  return new Promise((resolve, reject) => {
    const request = https.get(RELEASE_API, { headers: { 'User-Agent': 'O11A1-Electron', Accept: 'application/vnd.github+json' } }, (response) => {
      let body = '';
      response.on('data', (chunk) => { body += chunk; });
      response.on('end', () => {
        try {
          const releases = JSON.parse(body);
          if (!Array.isArray(releases) || releases.length === 0) {
            resolve({ current: APP_VERSION, update: null });
            return;
          }
          const release = releases.find((item) => !item.draft && !item.prerelease) || releases[0];
          const version = String(release.tag_name || release.name || '').trim();
          resolve({
            current: APP_VERSION,
            latest: version,
            update: newer(version, APP_VERSION) ? { version, notes: release.body || '', url: release.html_url || '' } : null,
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

app.whenReady().then(() => {
  app.setAppUserModelId('com.o11a1.electron');
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
    return { ok: true, data: await engineCall(method, scoped) };
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

ipcMain.handle('player:launch', async (_event, options) => {
  // Use the native Rust player window with its own overlay chrome. It is the
  // exact player UI from the native client: full-window video, floating
  // toolbar and floating right sidebar. Electron no longer wraps it in a
  // BrowserWindow.
  playerSeq += 1;
  const playerId = `native-${Date.now()}-${playerSeq}`;
  await engineCall('player.open', { ...(options || {}), playerId }, 120000);
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
