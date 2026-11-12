# bilibili-electron

[GitHub 仓库](https://github.com/AntarcticBearHKL/O11A1) · [Releases](https://github.com/AntarcticBearHKL/O11A1/releases)

O11A1 的 Electron 桌面版：**主窗口 UI 由 Electron 绘制，视频播放窗口使用原生 Rust 覆盖层。**

当前状态：可运行的桌面客户端，包含主窗口全部页面、原生 Rust 播放器窗口、原生弹幕、离线下载、养鱼、账号/设置与打包脚本。

## 架构

```text
Electron main (Node)
  ├─ BrowserWindow × 1   主窗口（HTML/CSS/React）
  ├─ tray / autostart / update check
  └─ local TCP JSON-RPC
        │
        └─ o11a1-engine.exe (Rust)
             ├─ core        Bilibili API / session / settings / downloads / fish
             ├─ native_player  FFmpeg + D3D11 + DComp + WASAPI + 插帧 + 增强
             └─ 原生播放器窗口（视频铺满、悬浮工具条/侧栏、整窗拖动）
```

- 视频帧**不经过 Electron**：Rust 自己解码、处理并 Present 到原生窗口。
- 点击卡片立即出现原生窗口和加载层（8 点 spinner + 正在打开视频…）；播放源在后台解析，第一帧准备好后自动换上视频。
- 播放窗口没有固定顶栏：视频铺满整个窗口，底部工具条和右侧侧栏都是悬浮覆盖层；鼠标静止 3 秒自动隐藏。
- 整窗左键拖动：画面按下移动超过 4px 拖动窗口，不移动松开仍播放/暂停。
- 关闭原生窗口会自动释放播放器额度，连续打开/关闭三个窗口后仍能继续打开。
- 原生弹幕、评论、历史、性能、画质/音轨/速度/比例/亮度/插帧/增强/设置都在原生播放器覆盖层内。
- 弹幕由 Rust 按视频时钟调度并绘制在视频层上；Electron 只负责开关、参数、发送、历史列表与点赞/举报/屏蔽操作。

详见 [ARCHITECTURE.md](ARCHITECTURE.md)。

## 运行

需要 Node.js 24+、Rust stable（MSVC）、FFmpeg。开发数据目录是 `%APPDATA%\o11a1-electron\data`，首次启动会尝试从旧安装版复制设置/账号/历史。

```powershell
npm install
# 第一次安装后，若 Electron 二进制没有下载：
node node_modules/electron/install.js
node node_modules/esbuild/install.js

npm run build:renderer
# 构建 Rust sidecar（默认复用 D:\Dev\bilibili-slint\target）
powershell -ExecutionPolicy Bypass -File scripts/build-engine.ps1
npm start
```

开发时可以用 `npm run dev`（Vite + Electron）。

## 打包

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

脚本会构建 Rust engine、复制字体/FFmpeg 到 `runtime/`、构建 renderer，并调用 `electron-builder` 生成 NSIS 安装包到 `release/`。

## 目录

| 路径 | 内容 |
|---|---|
| `electron/main.cjs` | Electron 主进程：窗口、托盘、引擎生命周期、播放器 rect 同步、更新、自启 |
| `electron/preload.cjs` | 受控的 `window.o11a1` API |
| `src/` | React UI：主窗口页面与组件 |
| `engine/` | Rust sidecar：JSON-RPC、数据接口、下载、养鱼、Player 控制 |
| `native/` | 从 `bilibili-slint` 迁入的 `core` / `native_player` 源码 |
| `runtime/` | 打包用 engine、字体、FFmpeg |
| `scripts/` | 构建与打包脚本 |
| `FEATURES.md` | 当前功能清单与 Electron/Rust 归属 |
| `STATUS.md` | 已实现/已知差异/验证记录 |
| `ARCHITECTURE.md` | 进程、窗口、IPC、视频、弹幕设计 |
| `MIGRATION.md` | 迁移映射与阶段计划 |

## 标记说明

- `[E]`：Electron 侧实现
- `[R]`：Rust 侧实现
- `[E/R]`：两边协作
