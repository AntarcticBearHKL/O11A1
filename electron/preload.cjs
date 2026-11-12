'use strict';

const { contextBridge, ipcRenderer } = require('electron');

function on(channel, callback) {
  const listener = (_event, value) => callback(value);
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.removeListener(channel, listener);
}

contextBridge.exposeInMainWorld('o11a1', {
  call: (method, params) => ipcRenderer.invoke('engine:call', method, params),
  engineStatus: () => ipcRenderer.invoke('engine:status'),
  info: () => ipcRenderer.invoke('app:info'),
  on,
  window: {
    minimize: () => ipcRenderer.invoke('window:minimize'),
    maximize: () => ipcRenderer.invoke('window:maximize'),
    isMaximized: () => ipcRenderer.invoke('window:isMaximized'),
    close: () => ipcRenderer.invoke('window:close'),
    hide: () => ipcRenderer.invoke('window:hide'),
    show: () => ipcRenderer.invoke('window:show'),
    quit: () => ipcRenderer.invoke('app:quit'),
    startDrag: () => ipcRenderer.invoke('window:start-drag'),
    fullscreen: (on) => ipcRenderer.invoke('window:fullscreen', on),
    isFullscreen: () => ipcRenderer.invoke('window:isFullscreen'),
    topmost: (on) => ipcRenderer.invoke('window:topmost', on),
    mini: (on) => ipcRenderer.invoke('player:mini', on),
  },
  shell: {
    open: (url) => ipcRenderer.invoke('shell:open', url),
    openPath: (path) => ipcRenderer.invoke('shell:openPath', path),
  },
  dialog: {
    error: (title, message) => ipcRenderer.invoke('dialog:error', title, message),
  },
  player: {
    launch: (options) => ipcRenderer.invoke('player:launch', options),
    options: () => ipcRenderer.invoke('player:options'),
    open: (options) => ipcRenderer.invoke('player:open', options),
    rect: (rect) => ipcRenderer.invoke('player:rect', rect),
    visible: (visible) => ipcRenderer.invoke('player:visible', visible),
    command: (cmd, value) => ipcRenderer.invoke('player:command', cmd, value),
    close: () => ipcRenderer.invoke('player:close'),
    detach: () => ipcRenderer.invoke('player:detach'),
    onState: (callback) => on('player:state', callback),
    onOpened: (callback) => on('player:opened', callback),
    onViewport: (callback) => on('player:viewport', callback),
    onRequestOpen: (callback) => on('player:request-open', callback),
    onClick: (callback) => on('player:click', callback),
    onDoubleClick: (callback) => on('player:doubleclick', callback),
    onMouseMove: (callback) => on('player:mousemove', callback),
    onDanmaku: (callback) => on('player:danmaku', callback),
    onLongPress: (callback) => on('player:longpress', callback),
  },
  update: {
    check: () => ipcRenderer.invoke('update:check'),
    open: (url) => ipcRenderer.invoke('update:open', url),
    onResult: (callback) => on('update:result', callback),
  },
  autostart: {
    set: (enabled) => ipcRenderer.invoke('autostart:set', enabled),
    get: () => ipcRenderer.invoke('autostart:get'),
  },
  tray: {
    balloon: (title, content) => ipcRenderer.invoke('tray:balloon', title, content),
  },
  app: {
    navigateMain: (route) => ipcRenderer.invoke('app:navigate-main', route),
    onNavigate: (callback) => on('app:navigate', callback),
  },
  updateResult: (callback) => on('update:result', callback),
  accountChanged: (callback) => on('account:changed', callback),
  settingsChanged: (callback) => on('settings:changed', callback),
  engineStatusChanged: (callback) => on('engine:status', callback),
  onHudToggle: (callback) => on('app:hud-toggle', callback),
});
