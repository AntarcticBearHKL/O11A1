# 架构设计 — Electron UI + Rust 原生视频层（方案 A）

## 1. 结论

采用 **方案 A**：

- Electron 负责主窗口 UI：首页、搜索、收藏、设置、弹幕输入等。
- 播放窗口改用 `native_player` 自带的 Rust 覆盖层：视频铺满窗口，工具条与右侧侧栏悬浮在视频之上，无固定顶栏；整窗左键拖动，单击仍播放/暂停。
- 主窗口顶栏按原 Rust `App::chrome`：左侧账户头像/昵称/UID，紧跟一个设置齿轮；账户块打开玻璃下拉（会员/账户、设置、历史、下载、退出登录）；右侧只保留窗口控制。
- 主窗口保留原 Slint 版的右键长按手势：右键按住 500ms 刷新当前页，移动 >16px 取消，光标处绘制进度环；触发后通过 `o11a1:refresh` 事件让各页面重新加载自己的数据。
- 主窗口底栏按原 Rust 版布局：左侧 5 个 tab、中央旋转黑胶（打开播放列表浮层）、右侧 3 个 tab、刷新 pill、搜索放大镜；搜索框从放大镜 morph 展开。
- 所有视频卡片按原 Rust `ui_kit::Tile` 绘制：白底圆角 10、16:9 封面、20px 时长角标、60px 正文（标题 14.2/600 单行，作者 12/500，新鲜度 11/400 + 5px 稀有度圆点）。卡片左键单击直接开播，不经过详情页。
- 独立视频详情页已删除；封面、标题、UP、播放/弹幕/日期、点赞/投币/收藏、简介、分P 与评论都在播放器侧栏（原生覆盖层）。
- Rust 负责所有视频相关：FFmpeg 解码、插帧、CAS/Anime4K、音频、音视频同步、弹幕调度与绘制、最终上屏。
- Rust 把视频、弹幕、播放器工具条和侧栏画到同一个**原生播放器窗口**里。
- **视频帧不进入 Electron**：不过 IPC、不过 WebGL、不转成图片。

## 2. 进程模型

推荐 **Electron 主进程 + 独立 Rust 引擎进程**：

```text
Electron main (Node)
  ├─ BrowserWindow × 1 ← 主窗口，HTML/React
  ├─ tray / updater / autostart
  └─ JSON-RPC（127.0.0.1 本地 TCP，一行一条 JSON）
        │
        └─ o11a1-engine.exe (Rust)
             ├─ core: Bilibili API / session / settings / downloads / fish
             ├─ native_player: FFmpeg / D3D11 / DComp / WASAPI / 插帧 / 增强
             └─ native standalone player windows (WS_POPUP)
                  ├─ 视频铺满整个窗口
                  ├─ 悬浮底部工具条（3 秒自动隐藏）
                  ├─ 悬浮右侧侧栏（圆角面板，不挤压视频）
                  └─ 右上角仅 3 个悬浮窗口按钮，无顶栏
```

理由：

- 原生引擎崩溃不会带崩 Electron UI；Electron 可以重启引擎。
- 不需要 napi ABI 与 Electron 版本绑定。
- 引擎可以独立 QA、独立运行、单独看日志。
- 以后拆分 `core` 和 `player` 也容易。

如果更在意“窗口必须严格是 Electron 的子窗口”，也可以把引擎做成 napi-rs addon，
在 Electron 主进程里创建 `WS_CHILD`。代价是原生崩溃会带崩整个应用，且每次
Electron ABI 升级要重编。v1 建议先用独立进程 + owned popup。

## 3. 播放器窗口布局

播放窗口不再由 Electron 包裹，也不再创建视频占位矩形。Electron 收到播放请求后，直接让
engine 打开一个 `native_player` 独立窗口：

```text
┌──────────────────────────────────────────────┐
│                                   [—][□][×]  │  ← 仅悬浮按钮，无顶栏
│                                              │
│              视频铺满整个窗口                 │
│         （弹幕、插帧、增强都在同一层）         │
│                              ┌─────────────┐ │
│                              │ 悬浮侧栏     │ │
│                              │ 信息/选集/   │ │
│                              │ 评论/历史/   │ │
│                              │ 性能/设置    │ │
│                              └─────────────┘ │
│        ┌──────────────────────────────┐      │
│        │ 悬浮工具条：进度/播放/音量…   │      │
│        └──────────────────────────────┘      │
└──────────────────────────────────────────────┘
```

- 视频视觉始终 `width = window width`，侧栏不再 `video_w = width - side_w`。
- 侧栏是右侧圆角悬浮面板，覆盖视频；可从工具条的侧栏按钮开关。
- 工具条是底部居中圆角胶囊；鼠标静止 3 秒后工具条和右上角按钮一起隐藏，移动恢复。
- 顶栏背景、标题、画质徽标全部移除，只保留右上角三个悬浮窗口按钮。
- 整窗左键拖动：画面按下后移动超过 4px 调用原生 `WM_NCLBUTTONDOWN/HTCAPTION`
  移动整个窗口；未移动直接松开仍切换播放/暂停。
- 多个播放器窗口按 46px 级联摆放；视频、弹幕和控制条都在同一个原生窗口内，
  不存在 Electron 与原生窗口互相遮挡的问题。
- 窗口创建后做一次 topmost → not-topmost 的 z-order bump，保证从 Electron 点击卡片
  打开时，播放器直接出现在主窗口上方（后台进程无法可靠调用 `SetForegroundWindow`）。

### 打开时序（点击即出窗口）

```text
点击卡片
  → engine player.open
  → native_player 创建窗口 + 覆盖层（约 0.05–0.5 s）
  → 覆盖层画黑色加载底 + 8 点 spinner + 正在打开视频…
  → 后台线程 source::resolve（video_view + playurl）
  → Command 通道送回 DeferredOpen
  → 换入真实视频层、启动解码/音频、拉取弹幕与详情
  → 第一帧呈现前保持加载层，第一帧后自动消失
```

- `Player::open` 只等待窗口和覆盖层就绪，不再等待网络。
- 解析失败时窗口保留，覆盖层显示错误卡片；“重试”重新跑解析线程。
- 关闭原生窗口后，引擎在 150 ms 的状态轮询里发现 `Player::is_open() == false`
  并移除该播放器，三个窗口的额度随之释放。

### 全屏

- 原生播放器自己进入全屏，视频铺满显示器；`Esc` / `F` 退出。
- 全屏时覆盖层只由原生窗口绘制，不涉及 Electron 透明窗口。

## 4. 视频管线

```text
FFmpeg (libavformat/libavcodec)
  → D3D11VA 硬解 → NV12/P010 纹理
  → 插帧（RIFE / ONNX DirectML 或光流 shader）
  → 画质增强（CAS / Anime4K compute shader）
  → D3D11 视频处理器 / 色彩空间
  → DComp waitable swap chain
  → 原生 popup
```

- 帧级处理不要 `spawn ffmpeg.exe` 再读 rawvideo 管道；直接链接 libav* 或 Media Foundation。
- 音频用 WASAPI；音频时钟是主时钟，视频按音频时钟 present。
- 3 个播放器窗口上限；每个窗口独立 swap chain。
- 4K/120 是后续目标；v1 保持当前 1080p 能力。

## 5. 弹幕

弹幕由 Rust 画在视频层上（方案 A）：

- Rust：拉取/解析/调度、滚动/顶部/底部、轨道避让、透明度/字号/速度/区域、
  关键词/正则/用户屏蔽、AI 云屏蔽、历史弹幕。
- Electron：弹幕开关、设置面板、输入框、历史日期列表、右键菜单的 UI 外壳。
- 输入框在 Electron 底栏（视频矩形外），提交后 IPC 给 Rust，Rust 立刻上屏并发送。
- 右键/长按命中由 Rust 做（它知道每条弹幕的屏幕矩形），Electron 只负责弹菜单。
- 弹幕字体继续用 Chiron + GDI 掩码；和 Electron UI 的 Chromium 文本不共享渲染器。

如果你以后要 Electron 画弹幕（CSS/emoji/富文本），再引入**透明覆盖窗口**：

- `BrowserWindow({ transparent: true, frame: false, hasShadow: false, parent: mainWindow })`
- 覆盖视频矩形，`setIgnoreMouseEvents(true, { forward: true })` 点击穿透。
- Canvas/WebGL 画弹幕，时间由 Rust 每 100ms 发一次，前端插值。
- 代价：全屏、多屏、拖动、z-order、合成性能都要额外处理。

## 6. IPC 契约（草案）

传输：Rust 绑定 `127.0.0.1:0`，在 stdout 打印 `O11A1-ENGINE READY <port>`；Electron 连接后
一行一条 JSON（请求 `{id,method,params}`，响应 `{id,ok,data,error}`，事件 `{event,data}`）。
不要传视频帧。

### Electron → Engine

```json
{"cmd":"player.open","bvid":"BV...","cid":123,"quality":80}
{"cmd":"player.play"}
{"cmd":"player.pause"}
{"cmd":"player.seek","ms":12345}
{"cmd":"player.setVolume","value":80}
{"cmd":"player.setSpeed","value":1.0}
{"cmd":"player.setVideoRect","x":0,"y":48,"w":880,"h":495,"dpi":1.0}
{"cmd":"player.setFullscreen","on":true}
{"cmd":"danmaku.setConfig","opacity":85,"font":1,"speed":100,"area":100}
{"cmd":"danmaku.send","text":"..."}
{"cmd":"window.close"}
```

### Engine → Electron

```json
{"event":"player.opened","durationMs":499000}
{"event":"player.time","ms":12345}
{"event":"player.state","state":"playing|paused|buffering|error"}
{"event":"player.error","message":"..."}
{"event":"player.stats","fps":60,"dropped":0,"decodeMs":2.1,"renderMs":1.4}
{"event":"danmaku.list","items":[...]}
{"event":"danmaku.hit","id":"...","x":120,"y":300}
```

## 7. Airspace / 控件覆盖

原生 popup 永远在 Electron DOM 之上。规则：

1. 所有 Electron UI 默认放在视频矩形之外（顶栏/侧栏/底栏）。
2. 需要悬浮在视频上的控件：
   - 优先：控件出现时缩小视频矩形（视频让位），不做 overlay。
   - 其次：第二个透明 Electron 窗口，覆盖视频矩形，点击穿透；需要交互时临时关闭穿透。
   - 不推荐：把控件画进原生窗口（UI 双轨，维护成本高）。

## 8. 文本 / 字体

- Electron UI 文本由 Chromium 渲染（Windows 上是 DirectWrite/Skia），**不是**当前原生版的
  GDI 5x + sharpen 管线。Chiron 在 Chromium 下的观感会和原生版不同，这是 Electron UI 的代价。
- 原生视频窗口里的弹幕继续用 Rust 的 GDI 文本管线，可以保持当前清晰度。
- 如果要求 UI 和弹幕字体完全一致，需要接受 Chromium 的字体渲染，或把 UI 关键文字也做成原生
  overlay（不推荐）。
- 字体仍只打包 Chiron GoRound TC Regular/SemiBold；缺字体时 UI 用 CSS fallback 还是禁用文字，
  需要明确（原生版是直接不画）。

## 9. 打包

- Electron app（asar）+ `runtime/o11a1-engine.exe` + `runtime/assets/fonts` + `runtime/ffmpeg/bin`。
- Electron 启动 sidecar 时设置 `O11A1_HOME=<userData>/data` 和 `BILI_FFMPEG_DIR=<runtime>/ffmpeg/bin`；
  引擎退出后自动重启。
- 安装器：`scripts/package.ps1` 调用 electron-builder 生成 NSIS；桌面/开始菜单快捷方式。
- FFmpeg 许可（LGPL/GPL）与 DLL 分发要写进 `THIRD_PARTY`。

## 10. 风险清单

- [ ] 跨进程窗口 owner/z-order 在最大化、全屏、多显示器下的稳定性。
- [ ] DPI：CSS 像素 × devicePixelRatio 的取整误差；窗口边框/阴影。
- [ ] 视频 popup 与 Electron 窗口的移动/缩放不同步，出现 1 帧错位。
- [ ] 透明覆盖窗口在 4K/120 下的合成性能。
- [ ] Electron UI 文本（Chromium）与原生弹幕（GDI）观感不一致。
- [ ] 引擎崩溃/重启时播放状态恢复。
- [ ] FFmpeg 硬解在不同 GPU/驱动上的兼容性。
- [ ] 原生 popup 在 Electron 窗口最小化/隐藏时没有正确隐藏。
- [ ] 多窗口（3 个播放器）时 IPC 会话隔离。
