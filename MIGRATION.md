# 迁移映射与阶段计划

从当前 `bilibili-slint` 原生版迁移到 `bilibili-electron`。

> 状态：Phase 0–3 已完成；Phase 4 的验证记录见 [STATUS.md](STATUS.md)。
> `core` 与 `native_player` 已复制到 `native/`，引擎不再依赖旧项目目录。

## 1. 代码归属

| 当前 | 新位置 | 说明 |
|---|---|---|
| `core/` (`o11a1-core`) | `engine/`（Rust） | Bilibili API / session / settings / accounts / paths / danmaku / fishwatch / downloads |
| `native_player/` | `engine/player/`（Rust） | FFmpeg / D3D11 / DComp / audio / interpolation / enhance / danmaku render |
| `native_ui/` | `electron/`（TypeScript + React/Vue） | 所有窗口、页面、设置、播放器外壳 |
| `native_ui/src/pages/*` | `electron/src/pages/*` | 见下表 |
| `docs/native-rewrite.md` | 本目录 `FEATURES.md` | 功能基线 |
| `docs/native-gap.md` | 本目录 `FEATURES.md` 第 21 节 | 已知限制 |

## 2. 页面映射

| 原生文件 | Electron 页面 | 备注 |
|---|---|---|
| `pages/feed.rs` | `pages/Home` | 推荐流 / 卡片 |
| `pages/search.rs` | `pages/Search` | 搜索 morph、建议、历史 |
| `pages/detail.rs` | 播放器侧栏「信息」tab | 详情、分P、相关、评论全部迁入播放器侧栏，不再有独立详情页 |
| `pages/user.rs` | `pages/User` | UP 空间、关注、投稿 |
| `pages/favorites.rs` | `pages/Favorites` | 三 tab、Rust 卡片网格、文件夹 |
| `pages/history.rs` | `pages/History` | 条数、删除、清空 |
| `pages/regions.rs` | `pages/Regions` | 20 分区 |
| `pages/pgc.rs` | `pages/Pgc` | 番剧/影视 |
| `pages/live.rs` | `pages/Live` | 直播 |
| `pages/dynamic.rs` | `pages/Dynamic` | 动态 |
| `pages/messages.rs` | `pages/Messages` | 回复/@/赞/私信 |
| `pages/member.rs` | `pages/Member` | 账号、二维码、大会员 |
| `pages/settings.rs` | `pages/Settings` | 10 分类 |
| `pages/downloads.rs` | `pages/Downloads` | 离线下载 |
| `pages/fish.rs` | `pages/Fish` | 养鱼四页 |
| `native_player/overlay.rs` | 原生播放器窗口 | 视频全屏、悬浮工具条/侧栏、无顶栏、整窗拖动 |
| `native_player/engine.rs` | `engine/player` | 解码/音频/插帧/增强/上屏 |
| `core/src/danmaku.rs` | `engine/danmaku` | 解析/发送 |
| `core/src/fishwatch` | `engine/fish` | ASR/步骤/模型 |

## 3. 阶段计划

### Phase 0 — 外壳

- Electron + 构建工具（Vite/React 或 Vue）。
- 主窗口 1280×800、主题、壁纸、顶栏、底栏、页面路由、Toast、骨架屏。
- 托盘 / 关闭到托盘 / 开机自启 / 更新检查的 Electron 版本。
- 静态页面占位：Home / Search / Favorites / History / Regions / Pgc / Live / Dynamic / Messages / Member / Settings / Downloads / Fish。

### Phase 1 — 数据与页面

- 启动 Rust engine sidecar；本地 TCP JSON-RPC。
- 把 `core` 的 API 通过 engine 暴露给 Electron：feed/search/detail/favorites/history/regions/pgc/live/dynamic/messages/member/settings/downloads/fish。
- 逐页替换占位：Home → Search → Favorites → History → Regions → Pgc → Live → Dynamic → Messages → Member → Settings → Downloads → Fish；视频详情统一放进播放器侧栏。
- 数据持久化迁移：settings/accounts/resume/search_history/player_state/downloads/fish.db。

### Phase 2 — 播放器

- Electron 播放器窗口 UI：顶栏、侧栏六 tab、底栏、播放列表、睡眠定时、快捷键。
- 原生视频 popup：矩形同步、DPI、全屏、最小化、多显示器。
- 视频管线：FFmpeg/D3D11/DComp、音频/WASAPI、seek、画质/音轨、插帧、CAS/Anime4K。
- 弹幕原生化：拉取/调度/绘制/右键命中/历史面板/发送。
- 状态持久化、B 站心跳、3 播放器上限。

### Phase 3 — 收口

- 下载页 + ffmpeg 合流 + 托盘气泡。
- 养鱼四页 + 模型下载/ASR/队列。
- 更新/安装器/签名/FFmpeg 许可。
- 已知限制修复：DPI、IME、airspace、全屏控件。

### Phase 4 — 性能与验收

- 1080p60 基线；4K/120 目标。
- 播放器 fps/丢帧/队列/渲染耗时。
- Electron UI 帧率与内存。
- 截图验收、真实直播、真实播放、自动化测试。

## 4. 不要做的事

- 不要把视频帧通过 IPC 或 WebGL 传给 Electron。
- 不要让 Electron `<video>` 承担插帧/增强后的播放。
- 不要在 Electron DOM 里直接盖原生视频窗口（会被挡住）。
- 不要一开始就做透明覆盖窗口/全屏悬浮控件；先把控件放在视频外。
- 不要把 `core` 用 TypeScript 重写一遍；先复用 Rust。
