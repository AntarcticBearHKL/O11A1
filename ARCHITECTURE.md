# 架构设计 — Electron UI + Rust 原生视频层（方案 A）

## 1. 结论

采用 **方案 A**：

- Electron 负责主窗口 UI：启动登录/加载页、首页、搜索、收藏、设置、弹幕输入等。登录页是首页暖纸 + 光晕 + 点阵上居中的 392px 暖纸小卡（24px 圆角），扫码页只允许二维码（二维码直接嵌在表面，没有独立白卡 / 边框 / 阴影 / hover 抬起；浅色主题静区与表面同色）、6px 加载条和一行状态文字（对象之间 22px 行距，状态行高 1.75）；加载 / 登录成功态才在上方显示头像。启动页与应用内账户页复用同一个 `LoginSurface`。
- 启动分流：无账号扫码页、启动加载/进入页、应用内账户页退出后的扫码页共用 `LoginSurface`——扫码页只显示 216px 二维码（无独立白卡，浅色主题静区取表面色，直接嵌在表面）+ 6px 加载条 + 一行状态文字（对象之间 22px 行距，状态行高 1.75），**不显示头像**；加载 / 登录成功态才在顶部显示 104px 圆形头像（上次登录真实 face，缓存在 `localStorage` 的 `o11a1:last-account`；无记录时默认剪影）。没有品牌行、标题、副标题、上次登录胶囊、SIGN IN、接口说明或按钮；二维码生成中在同一位置显示模糊的二维码幽灵（同一套寻像图形 + 模块场，CSS 模糊并轻微呼吸），不再显示独立小进度条；过期自动换码。启动时保存账号会先做 `account.nav` 校验；-101 时清除失效账号并停在扫码页，避免以未登录会话进入主界面。
- 登录页同时预载 `app.bootstrap`、`feed.home`、`search.hot`、`search.history.get` 和首页前 48 张封面；首页读取同一份缓存，进入时不再出现骨架屏。
- **全页面后台预载**：启动/登录期间并行暖好每个页面的默认首屏命令——`favorites.folders`、`history.list`、`downloads.list`、`regions.latest(1)`、`pgc.timeline`、`live.index`、`dynamic.list(all)`、`messages.unread` / `messages.list(reply)` / `messages.dm.sessions`、`account.nav`、`member.vip`、`fish.state`，并预热每个页面前 6 张封面。结果写入一次性内存缓存，`call()` 首次命中同一命令 + 参数时直接返回并从缓存删除；主页面优先，启动最多等 2.5 秒，慢请求在进入主页后继续完成。二维码登录成功后补跑账户页预载；退出登录清空缓存。
- 播放窗口改用 `native_player` 自带的 Rust 覆盖层：视频按源比例直接铺在窗口里（16px 小圆角），比例不一致时只在一个方向补首页暖纸 + 光晕 + 点阵并贴住另一方向的两条边，不再有 14px 四周画框；482×86 悬浮工具条（距窗口底部 26px）使用首页暖纸 + 径向光晕 + 点阵的不透明面板（静音按钮只切换 on/off，滚轮直接调音量，调节时视频右侧中间显示一条灰色轨道 + 蓝色填充的竖向音量细条（单条控件，无纸面外框/白边）；循环与其它控件同为实底 chip，点击展开结束行为四档菜单；侧栏 tab / 弹幕浮层 / 静音激活时只在原 chip 上显示蓝色淡底并让图标转蓝，没有描边、外圈光晕或底部短横线；所有按钮悬停显示名称/状态 label；弹幕设置浮层打开时，左键或右键点击画面只关闭浮层，不暂停或触发其它操作，滚轮调节时视频右侧中间显示竖向音量细条（纸面底 + 蓝色填充，900ms）；画面任意位置滚轮按 5%/格调音量（向上加、向下减），`wheel_volume_step` 把高分辨率滚轮不足一格的增量累积起来，滚动时工具条自动出现并让音量胶囊停留 900ms），加载提示卡使用首页暖纸 + 光晕 + 点阵并在视频画面区居中，暂停时在整窗四周绘制 4px 蓝色描边 + 1px 内发丝线（不再画内侧淡蓝光晕带）（窗口化时描边外缘与窗口 16px 圆角同心重合、四角无缝隙，全屏/最大化方角，chrome 隐藏后仍保留，不再显示居中卡）；播放结束与暂停同逻辑，只保留整窗蓝色描边、没有居中卡和状态卡，弹窗使用 88% 半透明玻璃面板（弹幕设置首行与分布密度都是左标签 + 右对齐 54×28 空心开关，只变边框颜色，无背景填充；开关右缘与下方数值右缘对齐），360px 全高侧栏车道（首页暖纸 + 径向光晕 + 点阵），视频右上角一枚暖纸关闭按钮，按钮下方常驻暖纸 Debug FPS；整窗左键拖动，单击仍播放/暂停。播放器窗口创建后立即用 `WM_SETICON` 设置 32px / 16px 的 `o11a1.ico` 图标；`native_player` 和 `o11a1-engine` 两个 crate 的 `build.rs` 用 `rc.exe` 把同一 ICO 编进可执行文件，因此任务栏、Alt-Tab 和 Explorer 都不会落到默认程序图标。单击先按系统双击间隔挂起：WM_LBUTTONDBLCLK 到来时直接吞掉第一击，只切换全屏/还原；间隔结束才提交播放/暂停，已提交的单击不能再和下一击拼成双击。
- 主窗口为**界面 2.0**：左侧边栏完全删除，内容区占满整窗；窗口外沿用 yah 同款的 22px 透明留边（`--window-inset`）+ 24px 圆角 + 1px 发丝边 + 三层 `--window-shadow`，最大化时 `.app.maximized` / `.login-gate.maximized` 把 `--window-inset`、边框和阴影归零；深色主题单独覆盖为低亮冷色边（8% 白）和 3% 内高光，外阴影改用纯黑，避免窗口边缘出现白色内圈；`.login-gate` 只负责表面（背景、点阵、内容），不得再声明 `position` / `inset` / `border-radius`，否则相对定位会被 `inset` 整体推偏，只剩左上角圆角可见；底部工具栏 `bottom: calc(var(--window-inset) + 22px)`，所以窗口化（22px 留边 + 22px 缝隙）和最大化（0 + 22px）距离可见纸张底边始终是 22px；原生播放器在 16px GDI region（DWM 圆角与默认边框关闭）之外另建 `WS_EX_LAYERED | WS_EX_TRANSPARENT` 阴影窗口，跟随 `WM_MOVE` / `WM_SIZE` 重绘软阴影，最大化、全屏和关闭时隐藏。顶部栏是绝对定位的透明磨砂条，左侧是 40px 用户头像，中间 320px 搜索框（默认与展开态都显示浅灰 `搜索视频或 UP 主` placeholder，无快捷键提示，展开铺满头像与窗口钮之间；打开时右键任意位置关闭，不触发页面长按刷新），右侧窗口钮，页面滚动时内容从顶栏下方经过；底部是居中磨砂浮动工具栏，左侧「推荐 / 收藏 / 分区 / 番剧 / 直播」，中间 84px 单独黑胶（有播放时旋转，无播放时中间为空，不显示数字角标），右侧「养鱼 / 动态 / 消息 / 历史 / 刷新」（刷新为常驻蓝色圆钮）。切换 tab 时一枚共享深色圆片在工具栏上滑动，跨过中央黑胶时从其后方穿过；工具图标按实际笔画 bbox 平移居中。下载入口完全隐藏。
- 左上角头像展开玻璃下拉（会员/账户、设置、退出登录）；点击外部或 Esc 关闭。
- 每页的 PageHead 只负责页内 tabs/筛选 chip，不再渲染 eyebrow、大标题、说明或顶部操作按钮；首页没有页头，直接进入视频网格，也不再提供「换一批」按钮。历史页的数量选择/清空放在页面最顶端的工具栏（上一页 / 下一页 · 第 N 页 → 每页 50/100/200 条 → 清空历史），账户操作移入资料卡，直播间/番剧的返回按钮移入内容卡。页内 tabs 每项是独立药丸按钮，没有共享底槽和滑动指示器；不再只靠导航 tab 标识页面。
- 首页推荐按加载时的视口宽度计算列数，固定渲染 7 行（2–6 列，8 列保护）；`feed.home` 带 `count = 7×列数`，Rust 端 `homepage_feed_count` 拉取足够整页后按 count 截断，避免小屏多取 / 大屏缺行。不提供无限滚动或「加载更多」；刷新事件先把 `.page` 滚回顶部，再从 `feedCache` 取一批后台预载的 `feed.home` 数据替换，缓冲区始终保持 2 批。登录 / 退出会重置 fresh 游标与缓存。
- 主窗口保留右键长按手势：页面任意位置（包括视频卡和卡片悬停控件）按住 **400ms** 刷新当前页，只有文本输入、养鱼（大肥鱼）页和设置页不启动。指示器是一圈以指针为中心的 44px 极简进度环（3px 轨道 + 3.8px 蓝色弧从 12 点顺时针填充），没有图标、文字或卡片；16px 内移动跟随指针，>16px 取消。进度通过 SVG circle 的 `strokeDashoffset` 直接写 DOM，按住期间不重渲染整页。触发后通过 `o11a1:refresh` 事件让各页面重新加载自己的数据。
- 播放队列位于**底部工具栏中央**：84px 单独黑胶，有当前视频时旋转，没有播放时中间为空，不显示数字角标。点击向上展开居中 440px、高度随内容（最大 620px）、最多 7 行滚动列表；空队列只保留「播放队列」标题，底部仅有「清空」，没有自动播放 / 随机 / 上一首 / 下一首。视频结束默认顺序打开队列下一条：主进程先把已播完的一行从 settings.playlist 删除并把 playlist_current 指向滑入同一位置的下一行，再关闭旧窗口、打开新窗口；最后一条播完先移除再关闭播放器窗口。手动关闭播放器窗口（标题栏 / Alt+F4 / 任务栏）由引擎 150ms 状态轮询发出的 player.closed 触发同一条移除逻辑；应用整体退出时 closeAll 不发事件，队列保持不变。播放结束行为由工具条循环菜单控制：循环 / 继续 / 下一分P / 暂停。`loop_mode` 是 0/1/2/3 四档（暂停=0，循环=1，继续=2，下一分P=3），主进程从 `player.state.loopMode` 读取当前播放器的选择：0 和 1 不推进队列，2 才推进，3 由原生播放器直接 switch_page 到下一分P；旧 `loop_one` 在设置加载时迁移为 1，保存时与新值同步。
- 底栏入口按一条统一有序列表排布，不再固定左五右五：防干扰隐藏直播 / 番剧 / 大肥鱼后，剩余入口按原顺序自动前移补位，`slots` 分成左右两半（黑胶居中）；剩余入口数为偶数时槽位为奇数，刷新按钮加 `wide` 类以 0.34s 弹簧缓动从 42px 展开到 87px（两格 + 中间 3px gap），让左右两组严格等宽；移动端 36px 槽位对应 75px。
- 分区页保留 Rust 版横向 chip 轨道：20 个一级分区单行横向滚动，最新发布/排行榜是页头唯一控件；副标题与底栏提示改成「按分区推荐」。
- 历史页是翻页列表：顶部工具栏依次是上一页 / 下一页 + 第 N 页、每页 50/100/200 条、清空历史。B 站 `history/list` 固定 `ps=30`，前端从 `cursor.view_at=0` 连续请求直到缓冲到 `(页号+1)×每页条数`，再按页切片；上一页直接读已缓冲数组，不发请求。旧 UI 把游标读成不存在的 `cursor.time`，导致「加载更多」永远回到第一页，本次一并修正。卡片右下角只保留 64×5 进度条 + 百分比（负进度夹到 0%），封面进度条和时长角标移除。
- 番剧排行榜不再套 `card panel`：`.pgc-rank-list` 直接铺在页面背景上，行只有底部 1px 发丝线，没有外层面板背景 / 圆角 / 阴影。番剧索引的类型 chips 与排序下拉共用一条 flex 行：下拉 `margin-left:auto` 贴右边缘，宽 144 / 高 30，与 30px chips 同一行居中；只有窗口窄到放不下时才换行。番剧每日放送改用视频流卡片骨架：`.pgc-daily-grid` 与 `.video-grid` 同为 210px 起、46px 行距、22px 列距，封面 16:10 / 20px 圆角，标题 13.4px 且不预留两行高度，第几话紧跟标题 5px；封面时间角标删除。索引评分角标补 `left:auto`，修掉 legacy 样式 `left:7` 与 redesign `right:7` 同时生效把角标拉成整条黑底的问题。
- 番剧集不再用 PGC playurl 的直接 URL 启动播放器。季详情里的每集都带真实 `bvid` / `cid` / `aid`，`Pgc.tsx` 直接按普通视频的载荷 `launch({ bvid, cid, aid, ... })`，于是 `source::resolve` 走同一条 `video_view` + `playurl` 路径：所有 DASH 视频轨进入 `MediaSource::video_tracks`，画质菜单显示 1080P / 720P / 480P / 360P 的 AVC / HEVC / AV1 以及大会员锁定档，切画质复用 `resolve_track_switch`；`MediaSource::cid` 与 session 同时就绪，`danmaku::fetch_with_session` 按 cid 分段拉取弹幕。只有旧数据没有 bvid 时才回退到 `resolvePgcSource` 的 durl / DASH 直链，那条路径没有画质菜单和弹幕。
- 分区 / 排行榜卡片直接复用首页 `VideoGrid`：Rust `VideoCard` 增加 `cid` / `aid` / `pub_ts`，`card_from` 从区域与排行响应的 `aid` / `cid` / `pubdate` 填充；Regions 的启动载荷也带 `cid` / `aid`。因此卡片的封面比例、UP 名、发布时间胶囊、时长角标、标题提示、悬停上浮 / 封面缩放和点击行为与首页完全一致。
- 动态页完全扁平：`.dynamic-card` 不套 `.card`，不设背景 / 圆角 / 阴影，只用底部 1px 发丝线分行；`.dynamic-archive` 视频附件去掉底色、边框和内边距，图片网格直接铺在页面上。动态图片统一 168×168 缩略图（`@360w_360h_1c.webp`），点击经 `ImageViewer` portal 到 `.app`，以 `inset: var(--window-inset)` 全屏查看原图；`原始大小` 把原图按自然尺寸放进可滚动舞台。
- 消息 / 私信页完全扁平：不套 `.card`，每行没有背景底板，只有 1px 发丝分隔；私信会话用 40px 圆形头像 + 纯蓝未读数字；会话详情没有气泡，按发送者左右对齐的纯文本，同一发送者只在首条显示名字。悬停只改名字颜色，不产生灰色/白色框。首次加载不套 `SkeletonGrid`，用 `LoadingText` 的单行文字 + 三点节奏占位，避免消息页出现视频卡骨架。
- 收藏页改成全窗口扁平视频流：无 PageHead、无左侧栏，顶部仅两行无背景切换器（收藏夹推荐 / 收藏夹；第二行为 `favorites.folders.all` 返回的全部收藏夹，含默认与空夹）。推荐模式由 `favRecommend.ts` 随机抽 1–3 个 `favorites.items` 页，取 6 条内容再并发请求 `video.related`，按约 2:3 混合；localStorage `o11a1:fav-seen:v1` 记录 bvid 的近 30 分钟出现次数，达到 2 次先跳过，长度不足时放宽到 3 次、再不限；Rust 侧 `favorites.items` / `video.related` 自带 5 分钟 TTL，避免重复请求。
- 视频卡片改为无底板：16:10 稍高封面（圆角 20）+ 两行标题 + 底部左侧 UP 名、右侧时间胶囊（按发布时间暖→冷分级，8px 圆角芯片 + 渐变着色 + 圆点光晕）；标题不预留第二行空白，一行标题直接收紧，标题到信息行 7px、信息行到卡片底部 10px，网格行距 46px、列距 22px。UP 名为 12px / 600 / 正文色，`VideoGrid` 从 `owner_mid`（兼容 `author_mid` / `mid`）拼出 `#/user/<mid>`，点击名字进入 UP 主页且不触发卡片播放，hover 变蓝并出现下划线；分区 / 排行卡片的 `VideoCard` 也已补上 `owner_mid`。UP 主页顶部有「返回」回到上一页。胶囊背景为 12%→4% 渐变、描边 24%、文字与正文色 30% 混合、6px 实色圆点 + 2px 光晕；hover 提升到 16% / 6% 并放大光晕。封面只用很轻的中性阴影，hover 上移 4px、封面 1.045 倍、阴影轻微加深，不再显示播放圆钮；单击直接开播，不经过详情页。网格封面统一走 `cardCover()` 的 480px CDN 缩略图，图片 `loading=lazy` + `decoding=async`。所有视频封面使用 `CoverImage` 容错链：480px 缩略图 → 原图 → 带 cache-bust 的原图重试 → 中性占位卡；空 URL、缩略图 404、CDN 失败都不会留空框。首屏前 24 张 `eager`，其余 `lazy`。卡片本身不挂原生 `title`，因此整卡、封面和空白处不会出现系统全名提示；只有标题和发布时间胶囊触发 portal 渲染的暖纸浮层：标题显示完整标题，时间胶囊显示「YYYY年M月D日 HH:mm」、格式化播放量与相对时间；UP 名悬停不显示提示浮层。浮层最大 400px，1px 发丝边，无左侧竖线，按视口左右夹取，上方空间不足时翻到锚点下方，滚动 / 缩放 / Esc 时关闭。卡片和封面本身悬停只做上浮 / 封面缩放，鼠标指针保持默认箭头；只有 UP 名等真实链接使用手型。
- **滚动性能**：顶栏和底部工具栏都是独立合成层，全程保留 `blur(26px) saturate(1.35)` 的实时 backdrop blur，滚动时不再切换成不透明底；页面内容直接从玻璃下方经过。顶栏绝对定位、页面用 102px 顶部内边距避让，底栏固定在底部。全屏纹理不再使用 mix-blend；卡片入场动画只保留前 14 张。实测 120Hz 下带实时毛玻璃的常速滚动约 119 FPS。Debug HUD 由独立组件直接写 DOM，不会因 FPS 刷新重渲染页面；F12 / Ctrl+Shift+H / 头像菜单切换并持久化。底栏图标悬停标签是按钮的 `::after` 墨色胶囊（纸色文字、无三角），居中悬在图标正上方并与底栏留出约 17px 间距；悬停 `0.4s` 后才渐显，移开后 `0.2s` 渐隐，不使用原生 `title`；刷新只触发页面刷新和图标旋转，不写 toast。
- 独立视频详情页已删除；封面、标题、UP、播放/弹幕/日期、点赞/投币/收藏、简介、分P 与评论都在播放器侧栏（原生覆盖层）。
- **工具条首页背景**：工具条直接用与侧栏相同的暖纸底、三组低饱和径向光晕和 3px 点阵，使用 `FillRoundedRectangle` 裁成 12px 圆角并加 1px 发丝边；面板完全不透明，视频不会透出，也不依赖 DirectComposition 模糊效果。
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
                  ├─ 悬浮底部工具条（静止 2 秒自动隐藏）
                  ├─ 全高右侧侧栏车道（窗口化向外扩展，全屏向内让位）
                  └─ 右上角一枚暖纸关闭按钮，无顶栏
```

理由：

- 原生引擎崩溃不会带崩 Electron UI；Electron 可以重启引擎。
- 不需要 napi ABI 与 Electron 版本绑定。
- 引擎可以独立 QA、独立运行、单独看日志。
- 以后拆分 `core` 和 `player` 也容易。
- 养鱼 Agent：`FishEngine` 在一次看片 run 里同时维护 `FishProgressView`（步骤清单）和 `AgentView`（聊天记录）。`AgentView` 定义在 `fishwatch::agent`，包含投喂信息、逐工具步骤、DeepSeek 思考行与回复卡，随 `FishSnapshot.agent` 返回；`fish.rs` 在 `apply_watch_progress` 合并工具事件，在请求前后写入思考行，完成时附上 verdict。渲染层 `AgentTrace.tsx` 按 yah 的卡片逻辑绘制投喂 / 过程组 / 回复，运行中 1s 轮询。工具台由 `fishwatch::tool::tool_catalog()` 提供静态目录（与 `make_step` 同一工厂，标签 / 上限不会漂移），手工调用走 `fish::run_tool`：先 `fish_meta` 拿 cid，再用只启用一个步骤的 `StepSelection::only` 起一次 `WatchRun`，返回 `StepRecord` 加按工具裁剪的材料摘要；`fish.tools` / `fish.tool.run` 由 sidecar 暴露，手工调用在 Electron 侧放宽到 2000s 以容纳 ASR。
- 养鱼页全扁平：三个 tab（看片 / 试验台 / 设置），默认看片；Agent 顶部是开始 / 暂停 / 停止 / 投喂 / 重试 / 清空和完整流程，不套 `.card` / `.panel`，`AgentTrace` 去掉 `card panel` 根节点、展开/回复/等待行底色和引用框；试验台内分 来源 / 固定流程 / 画面拼接 / 低层工具，来源工具操作 watched 台账，固定流程按同一组步骤一次推进一格，工具台保留单步测试段，设置去掉卡片外框。ASR 固定 `tiny`：`settings.rs` 的 `default_fish_asr_model()` 返回 tiny、`normalise_fish_asr_model()` 无条件折回 tiny，前端只在设置里提供 tiny 模型下载，不再暴露引擎 / 设备 / 线程 / 镜像 / 启用开关。 试验台来源工具由 `fish.feed.next` / `fish.follows` / `fish.up.videos` / `fish.watched.*` 提供；`watched` 表与 `videos` 分离，Agent 成功观看时写入 `source=fish`，手动测试写入 `source=manual`。试验台不设大标题/说明卡，只保留细 tab 行、扁平来源动作行和扁平步骤/工具行；低层工具台的「填入正在播放」从 settings.playlist_current 取当前队列视频，无 current 时取队尾。
- 画面实验室走 `fishwatch::frames::lab`：`build_with_meta` 临时下载 720P、按用户间隔抽帧，用 `image` crate 拼成带时间戳的 6 格大图（960×540 横屏 / 540×960 竖屏，末张自动收行），再按张调用 `detail=high` 的 DeepSeek vision；`fish.frames.start/status/result/observe` 由 sidecar 暴露，后台任务状态带 generation 防止旧任务覆盖新任务。Agent 的 `FramesStep` 与画面实验室共用这套 builder，`fish_frame_interval_s` 控制自动看片间隔。

如果更在意“窗口必须严格是 Electron 的子窗口”，也可以把引擎做成 napi-rs addon，
在 Electron 主进程里创建 `WS_CHILD`。代价是原生崩溃会带崩整个应用，且每次
Electron ABI 升级要重编。v1 建议先用独立进程 + owned popup。

### 2.1 后台更新

自动检查只在启动 4 秒后跑一次（托盘菜单 / 设置页可手动再查），只发一个 GitHub API 请求。发现更高版本后由主进程直接下载，不阻塞 UI：

```text
GET /repos/AntarcticBearHKL/O11A1/releases
  → 最新非 draft / prerelease，取第一个 .exe 资产（排除 uninstaller）
  → 后台下载到 <userData>/updates/<asset>.exe
  → sha256 对比 API 的 digest；大小必须等于 asset.size
  → update:progress（约 4 次/秒）→ update:ready
  → 横幅 / 设置页显示「重启安装」
  → update:install
       detached PowerShell 等 Electron 与 o11a1-engine PID 退出
       → Start-Process <installer> -ArgumentList '/S','--updated' -Wait
       → Start-Process <app exe> 重启
```

- 下载器在 `electron/updater.cjs`，不依赖 Electron：跟随 302 到 CDN、写 `.part` 后原子改名、失败删除半成品。
- `<userData>/updates` 里同版本的完整文件会直接复用；启动时 `cleanInstalledUpdates()` 删除不高于当前版本的旧安装包。
- `--updated` 让 NSIS 在检测到旧进程时短暂等待 / 强杀，并跳过完成页；`/S` 保证全程无窗口。安装目录沿用上次写入的 `InstallLocation`（默认当前用户 `HKCU\Software\<APP_GUID>`）。
- 不接 electron-updater，因此不需要 `latest.yml` / `.blockmap`，也不支持差分更新。

## 3. 播放器窗口布局

播放窗口不再由 Electron 包裹，也不再创建视频占位矩形。Electron 收到播放请求后，直接让
engine 打开一个 `native_player` 独立窗口：

```text
┌──────────────────────────────────────────────┐
│                                   [—][□][×]  │  ← 仅悬浮按钮
│                                              │
│              视频按源比例铺满画面区           │
│         （弹幕、插帧、增强都在同一层）         │
│                                        ┌─────┤
│                                        │ 信息 │
│                                        │ 选集 │
│                                        │ 评论 │
│                                        │ 性能 │
│                                        │ 设置 │
│        ┌──────────────────────────────┐│      │
│        │ 482×86 工具条：进度/循环/进阶进度…││ 360  │
│        └──────────────────────────────┘│      │
└────────────────────────────────────────┴─────┘
```

- 底部工具条为 482×86 药丸：10px 上边距 + 22px 进度带 + 8px 间距 + 36px 控件行 + 10px
  下边距；顺序为 循环 / 进阶进度条 / 播放速度｜静音 / 弹幕 / 信息 / 评论 / 选集｜性能 / 设置；进阶进度条按钮、播放速度按钮和右键进度条都能打开独立 626px 宽卡片，卡片以 360ms 宽高联动 morph 展开。
- **播放速度面板**与精准定位共用同一套 morph：`open_speed` 只把 `panel=2`，`precision_mix` 驱动同一个 482×86 工具条矩形直接变成扁平横向滚轮（`speed_panel = layout.panel`，`speed_wheel = layout.panel`），卡片用 `draw_home_panel_plain` 绘制。顶部没有「速度」标题，只有正中 17.5px 蓝色数字 + 11.5px 次级「倍」的当前值，两者共用基线；下方是一条带发丝边和底部基线的刻度槽：`speed::TICKS` 定义 21 个等距档位（`0.1倍`–`0.9倍` 每 0.1 一格，之后 `1.0倍 / 1.25倍 / 1.5倍 / 2.0倍 / 3.0倍 … 10.0倍`），`WHEEL_GAP` 是相邻档位的固定像素间距；每个档位一根刻度，整数档蓝色加高并带刻度帽，其余短刻度立在基线上，9.5px 档位标签位于刻度上方并随拖动滚动。正中一条 2.5px 圆头红色瞄准线贯穿刻度槽决定当前档位，中央档位标签留空，所以红线不会切断文字；`index_of` / `from_index` / `stepped` / `snap` 只允许落在这些档位上。`drag_speed` 用 `steps = (start_x - x) / WHEEL_GAP` 计算档位偏移（向左为加速），`speed_preview` 实时吸附到最近档位，`tuner_offset = -steps * gap` 让刻度与标签平滑滚动；滚轮一格把 `speed_base` 指到新档位并给 `tuner_offset` 一整档初值，主循环用 42% 缓动把它拉回中心，所以旧档位留在中心、新档位从一侧滑入而不是瞬移。松手由 `commit_speed_preview` 同步 `speed_base` 并让目标档位回到红线正中；停 260ms 后由 `poll_pending_speed` 提交，避免每个 notch 重建音频 / 视频会话。`speed` 范围由 `speed::clamp` 统一到 0.1–10.0，音频 `atempo` 用多级链覆盖两端，视频 reader 的时间戳步长同样按新范围计算。
- 右键进度条打开**精准定位面板**：工具条本身向上展开到 180px，关闭时缩回；没有标题和关闭按钮（再次右键或 Esc 关闭），卡片高度、内容透明度与刻度带位移用 360ms 时间缓动同步驱动。左右统一 20px 内边距：30px 顶行是左右对称的排版读数、没有播放键（空格控制播放暂停，单击画面不切换）：左侧 11.5px 弱色「时间」标签 + 16px 半粗时间码（毫秒尾 13px 次级色）+ `/ 总时长`，右侧「帧」标签 + 13px 半粗当前帧 + `/ 总帧数`；标签与斜杠最弱、总数次级、当前值最深，全部共用一条基线，没有胶囊底板；30px 满宽弹幕密度热力图；A/B 占主宽度、`清空` 为右侧至少 72px 的淡红按钮。`danger_soft` / `danger_fill` / `danger_line` 三个画笔的颜色先放进具名局部变量再调用 `SetColor`；此前把 `&color(...)` 临时引用直接传给 `SetColor`，release 构建里画笔颜色会变成无效值，导致 `清空` 按钮整颗不可见。底行满宽移动式帧数标尺（12px 齿距、每 4 格蓝色主齿带顶部刻度帽和相对帧数标签、固定中心基准窗 + 一条贯穿整条刻度的红色瞄准线，标尺自身在中心不再重复画 T）加 92×38 帧步下拉选框（与标尺同高），选项在右侧浮层展开 1/2/5/10 帧。滚轮只改预览：`nudge_frames` 更新 `drag_preview` 并重置 280ms 空闲计时，`poll_pending_wheel_seek` 在最后一个 notch 后只发一次 `seek`；拖动刻度用 `tuner_drag` 记住按下位置和起始时间，整个拖动过程只更新 `drag_preview` 与 `tuner_offset`（标尺连续跟随手指），`LeftUp` 时才调用一次 `seek`；不足 12px 的抖动不累计帧数，刻度也不会在每个 notch 归零。`tuner_offset` 不夹在面板宽度内，绘制时由 `tuner_tick_range` 根据无限偏移计算可见刻度，所以标尺可以一直拖、刻度持续延伸；`drag_preview` 仍由 `clamp_position` 夹在视频总时长内。A/B 循环时 `poll_ab_prefetch` 从进入 A–B 就用 `VideoDecoder::spawn` 预热下一次 A，不再等 B 前 1.2s，也删掉了 1.6s 最短循环限制。≤2s（且不超过 256MB）的 A–B 由 `cache_ab_frame` 在第一遍播放时缓存解码帧，`start_ab_cache_loop` 之后直接从 `AbCache` 回放，`fill_next` 也从缓存取帧，所以短循环不再启动解码器或重新加载；>2s 或缓存超限时由 `enforce_ab_loop` 在边界换入预热 decoder。音频仍走 `AudioCommand::Seek`。向左拖动 / 向下滚轮为前进。
- 图标按 Rust Slint 的 16px path 重画（不是字体图标），36×36 格、10px 圆角、8px/组内间距、
  12px/组间间距、10px 两侧分割线空气。
- 右侧侧栏是 360px 全高车道：紧贴画面右边，高度与视频窗口一致。窗口化打开时窗口向右扩展 360px，画面保持原尺寸；全屏/最大化不能扩展窗口，车道向内占用 360px，画面收窄；关闭时窗口化恢复原宽。车道与视频补边共用 `layout.stage` 作为背景采样原点（暖纸 + 三组径向光晕 + 3px 点阵），不再为车道单独采样，补边到车道没有接缝。顶部五等分 12.5px tab（选中项为滑动墨色胶囊，首页底栏同款动效；选中与未选中项共用 Chill Round Gothic 的 GDI 字族、行高和垂直居中），**没有自己的关闭按钮**；点击画面不会关闭；右键画面任意位置或侧栏任意位置、当前 tab、同一工具条按钮或 Esc 都会关闭；画面右键在精准定位 / 弹幕菜单 / 工具条浮层之后才处理，先关浮层再关车道。拖动车道空白处调用窗口拖动。视频右上角只保留一枚暖纸关闭按钮。五个 tab：信息 / 选集 / 评论 / 性能 / 设置；各分区标题在 52px 头部槽内居中，选集 / 性能 / 设置行与评论、历史、排序、内联菜单文字均按行框垂直居中。 打开 / 关闭在移动窗口前按目标客户区宽度重排：打开时先把 360px 车道画进按显示器尺寸一次性分配的 UI 交换链，再向右扩展窗口；关闭时先缩窗口再清除车道。这样既不触发 `ResizeBuffers`（避免空白合成帧），也不会出现窗口已变宽而布局仍按旧宽度再扣一次车道、画面瞬间缩小的帧。
- 侧栏标签行 36px，内容从 48px 基线开始；所有侧栏正文、标签、section 标题统一 12.5px（用字重和颜色分级）。信息面板是一条可滚动的纵向信息流：自适应宽幅封面、16px 半粗标题、UP 图标与名称、三等分指标、视频 ID 复制行、互动胶囊与 13.5px/1.55 行高简介；性能面板使用固定两列表格。
- 评论区：头部在 `sidebar_content.top` 的 48px 槽内，作为列表第一块随评论流滚动（绘制和排序命中都叠加 `-scroll`）；左侧「评论 / 共 N 条」，右侧 116×28 的「最热 / 最新」两段式控件，墨色胶囊按 `comments_sort_mix` 在 28px 控件内滑动（约 200ms 时间常数），列表在新页到达时以 0→1 的 220ms 上移淡入。排序只用点赞（API `mode=3`）和发送时间（`mode=2`）；旧页在新页返回前保留，响应里带回请求的 `hot`，与当前排序不符就直接丢弃，排序期间再次点击只置 `comments_reload_pending`，避免乱序覆盖。每行高度由 `ensure_comment_layout` 在 generation / 宽度 / 缩放变化时重新测量：GDI `wrap` 用与光栅化相同的字体和宽度把正文拆成单行，缓存行 top/height、回复行和可点击链接；绘制直接用这些行，所以高度和文字永不裁切。行版式：36px 圆形头像、13.5px 半粗用户名、11.5px 相对时间、右上赞数、14px / 21px 正文，行首 14px / 行尾 14px 留白，底部 1px 分隔线；hover 只覆盖顶层评论块（上 10px、下 8px、左右 6px 内边距，10px 圆角），不覆盖回复和链接。回复在正文下缩进 12px，不画灰底气泡：20px 头像、12px 作者、13px / 18.5px 正文、赞数/时间、左侧 2px 线程竖线；默认折叠、不显示任何回复，展开后按页加载，收起后不保留预览；链接按 `rpid` 命中。评论页不设加载按钮：`poll_comments_autoload` 在滚动目标距底部 220px 内，或首屏未填满时请求下一页；测量缓存未跟上当前 generation 时不会误判。楼中楼通过 `comment_replies` 按 20 条一页拉取，按 rpid 去重合并。头像 URL 从顶层和预览回复一起收集，交给独立线程下载，避免 CDN 请求阻塞排序/分页。
- 插帧挡位是用户的持久选择：`interp_mode` 负责启停，`interp_hz` 选 0（自动，跟随窗口所在显示器）/ 60 / 90 / 120；播放器设置页的插帧行像画质一样展开 关闭 / 自动 / 60 / 90 / 120 内联菜单，高于窗口所在显示器刷新率的固定挡位不会出现，键盘 I 也只在可用挡位间循环。`set_interp_hz` 对合法目标原样接受，菜单层负责拒绝超屏挡位；掉帧统计不会改写模式或挡位。pacer 每轮最多一个 tick，迟到重置网格、不补帧；上屏仍由 DWM / 显示器节奏决定。 原生菜单与设置页的选择由 `select_interp_level` / `select_enhance` 写回 `settings.json` 并自增 `SharedState.settings_revision`；engine 轮询到 revision 后重新读取文件、把新值推给其他已开窗口并广播 `settings.changed`；每次 `player.open` 和 `apply_switched_source` / `apply_deferred_source` 都调用 `reload_player_prefs` 重新应用文件中的插帧与增强。
- 刷新率按窗口所在显示器查询：`MonitorFromWindow` 取 HMONITOR，`GetMonitorInfoW` 取设备名，再对该设备 `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)`；结果缓存 2 秒。`EnumDisplaySettingsW(NULL)` 在部分双屏驱动上返回失败并把主屏也固定回 60Hz，不能作为主屏查询。`tick_monitor_refresh` 每秒比较一次，跨屏或改刷新率后重算 `interp_out_hz`、挡位菜单和弹幕节拍；固定挡位高于新屏时退回自动。
- 性能 tab 直接读 `UiModel`：没有固定头部，也没有「性能 实时 · FPS」标题；顶部只有 `右上角性能显示` 开关（`toggle_stats` → `PlayerState.stats` → `player_state.json`），控制 `draw_fps_hud`，其余数值都在 HUD，不在 tab 里重复。下方网络分组显示播放位置 / 时长、已播放百分比、已加载（`mediacache::progress` 的 `loaded_bytes / total_bytes`；本地文件固定 100%，直链显示边下边播）、缓冲秒数，以及 `MediaSource` 选中 DASH 轨的 `video_bitrate` / `audio_bitrate`（直链为 0）算出的总码率、视频 / 音频码率、总数据量、下载线程、下载速度。下载线程 = 当前视频 decoder + audio engine + A/B 预热 decoder。24×6 = 144 格热力图每格固定占 1/144 时间线：已播放 `heat`、已加载 `cache_fill`、当前位置 `heat_hot`、未加载 `chip`，下方图例逐项标注；`available_end_ms` 对本地文件返回整段时长、对磁盘缓存按真实进度、对直链回退到读取领先量。工具条进度条复用同一个 `available_end_ms` 画浅蓝缓存带。
  设置行垂直居中，标签左对齐、值右对齐。
- 信息 tab 的排版是一条可滚动的纵向流：整宽封面按内容高度取 16:9 与宽幅之间的比例（`cover_source_rect` 中心裁切，不拉伸），下方 16px 半粗标题、24px 用户图标 + 13.5px 半粗 UP 名 + 右对齐「UP主」、三等分指标（10.5px 弱标签 + 13.5px 半粗值）、整行可点的「视频 ID」胶囊（等宽半粗 BV/av 号 + 复制图标，点击 `copy_video_id` 写入剪贴板并 toast），再用发丝线分出「互动」「简介」；互动按钮写入详情服务并即时回显“已点赞/已投币/已收藏”。简介 13.5px、1.55 行高，信息列随滚轮滚动（`sidebar_max_scroll` 使用缓存的 `info_description_height`），长简介不会被硬切。
- 评论支持最热/最新、自动连续加载、楼中楼回复和滚轮；设置 tab 没有标题行，画质置顶。当前画质/默认画质/音轨行在自身下方直接展开内联列表（40px 行、与设置行同套 12.5px 字/右对齐值/hover/当前项蓝点，无浮层、标题、边框或 Esc 提示），展开高度把后续行整体下移并计入滚动范围；速度直接切换；插帧 / 画质增强与画质一样展开内联菜单；置顶 / 纯音频直接切换；默认画质可选最高 / 2160P / 1440P / 1080P / 720P / 480P / 360P，默认最高，打开新视频时按所选高度取可用最高流；比例、亮度、弹幕开关、弹幕设置、睡眠定时和快捷键不再出现在设置页（弹幕由工具条按钮控制）。弹幕设置（透明度 / 字号 / 速度 / 区域 / 屏蔽方向）走 `DanmakuModel::reconfigure` 的实时重排：保留当前 active 弹幕，只重算字号、行高、轨道和速度后立即重绘，不调用 `danmaku_layer.clear()`；新的字号列表同步到 `DanmakuOverlay::set_fonts`，透明度同步到 `set_opacity`，当前屏幕上的弹幕直接换字号 / 透明度。每次重排同时把值写回全局 `settings.json`；`SharedState.settings_revision` 在保存成功后自增，engine 的播放器轮询发现新 revision 后重新 `settings::load()` 并广播 `settings.changed`，React 设置页与播放器侧栏因此保持同步；`apply_switched_source` / `apply_deferred_source` 打开下一个视频前调用 `reload_danmaku_settings()` 再读一次文件，同一窗口也不会沿用旧值。
- 直播以 `live: true` 打开，`PlayerConfig.is_live` 让原生播放器强制 `interp=Off` / `enhance=Off`，sidecar 也不再套用设置里的插帧/增强；直播设置页只显示 直播画质（只读）、置顶、仅音频。
- 默认画质存 `settings.player_quality_height`（`0` = 最高）。每次 `player.open` 都从 `settings.json` 重新读取，避免原生播放器直接保存设置后 sidecar 内存仍是旧值；`source::resolve` 在 `video_tracks` 里取 `height <= 目标` 的最大高度（同高度优先当前编码、再按码率），目标为 0 时直接取最高；视频没有低于目标高度的流时回落到最低可用流。
- **无画框比例适配**：`Layout.stage` 等于整个画面区（侧栏打开时减去 360px），不再四周
  内缩。视频按源比例 fit 在 stage 里，贴住一个方向的两条边，只在另一方向留下补边；覆盖层
  用不透明的暖纸/墨色填满补边，并用四个圆角遮罩把视频修成 12px 小圆角。没有 14px 画框、
  没有 1px 描边，也不会透出桌面。`stage_video_bounds()` 同时给视频变换和覆盖层使用，
  视频洞与圆角不会错位。
- 视频锁定源宽高比：每次窗口尺寸、侧栏开合、最大化/还原、换源/换分P 都按实时
  `Layout.stage` 做等比变换；不拉伸、不裁切。
- 右上角窗口控制是一枚 36×36 的暖纸关闭按钮（`Layout.top_close`），10px 圆角 + 1px 发丝边，悬停珊瑚红；不再有最小化 / 最大化热区。
  共享外描边与两条发丝分隔线；悬停格内缩 3px 填充实色圆角块（最小化 / 最大化蓝、关闭珊瑚红），
  图标转白并放大 8%，胶囊底色与描边同步加深。它常驻且不随工具条隐藏；底部工具条在指针进入时显示，
  指针离开窗口或静止 2 秒后隐藏（窗口化与全屏一致），侧栏保持可见。
- 整窗左键拖动与四边/四角缩放都由窗口过程直接处理：按下时 `SetCapture`，随后在
  `WM_MOUSEMOVE` 里用 `SetWindowPos` 移动/改尺寸，松开 `WM_LBUTTONUP` 结束。
  不再发送 `WM_NCLBUTTONDOWN/HTCAPTION` 进入 Windows 模态移动循环，因此拖动和缩放
  期间引擎线程继续出帧；拖动结束的松开事件被吞掉，不会误触播放/暂停。缩放时若按住
  Shift，窗口过程按 `WindowEvents.video_aspect`（当前源宽高）把 client 锁定为同一比例，
  扣除抗锯齿边框后反推外框尺寸，视频铺满、补边消失；松开 Shift 即回到自由缩放。
  手动缩放结束把 client 尺寸按 DPI 折算成 96-DPI 逻辑尺寸，交给 `PlayerState`
  防抖写入 `player_state.json`；下次创建窗口时优先使用该尺寸。
- 多个播放器窗口按 46px 级联摆放；视频、弹幕和控制条都在同一个原生窗口内，
  不存在 Electron 与原生窗口互相遮挡的问题。
- 卡片左键不再新开窗口：主进程按原生窗口上报的 `focusSeq`（每次激活 +1）挑出
  最近获得焦点的播放器，发 `player.replace` 让该窗口原地换源；窗口矩形、尺寸、
  工具栏和侧栏状态全部保留。右键卡片才走 `player.open` 新开窗口。顶替时主进程
  把队列里旧视频那行删掉，新视频插到同一位置并设为当前。
- 窗口创建后做一次 topmost → not-topmost 的 z-order bump，保证从 Electron 点击卡片
  打开时，播放器直接出现在主窗口上方（后台进程无法可靠调用 `SetForegroundWindow`）。

### 打开时序（点击即出窗口）

```text
点击卡片
  → engine player.open
  → native_player 隐藏创建窗口 + 覆盖层（约 0.05–0.5 s）
  → 覆盖层画首页背景加载卡 + 旋转圆环 + 正在加载视频…
  → 再画一帧确保首帧完整，然后 ShowWindow
  → 后台线程 source::resolve（video_view + playurl）
  → Command 通道送回 DeferredOpen
  → 换入真实视频层、启动解码/音频、拉取弹幕与详情
  → 第一帧呈现前保持加载层，第一帧后自动消失
  → 关闭：点击关闭格立即 SW_HIDE，再 stop_sessions / persist_state / destroy
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
FFmpeg (独立进程, NVDEC)
  → h264_cuvid / hevc_cuvid / av1_cuvid / vp9_cuvid 解码（不允许软解）
  → hwdownload → NV12 rawvideo 管道（12.4 MB/帧 @4K，64 KB 分块读）
  → R8 (Y) + R8G8 (UV) 动态平面 → nv12_main 转 BGRA（GPU）
  → blend_main 插帧（两纹理，GPU）
  → CAS / Anime4K（GPU，直接采样 BGRA 纹理）
  → DComp waitable swap chain
  → 原生 popup
```

- 解码只走 GPU：`decode::VideoDecoder` 固定传 `-hwaccel cuda -hwaccel_output_format cuda -c:v <cuvid>`；`hwdecode` 在首次解析时用内嵌样片逐编码真实探测，探测失败的流从 `video_tracks` / 画质菜单 / 切换目标里删除，不做软解回退。
- `ffmpeg.exe` 仍是独立进程，`hwdownload` 是唯一无法避免的回读；但 NV12 只有 1.5 byte/px（4K 12.4 MB），比原 BGRA 33 MB 少 62%，CPU 不再做 `NV12→BGRA` 或任何全帧混合。
- 帧缓冲由 `decode::FrameQueue` 的 `Arc<Vec<u8>>` 池复用；读取按 64 KB 分块（Windows 管道一次 12 MB 的 ReadFile 会把吞吐压到 ~10 fps，64 KB 可达 ~1.1 GB/s）。
- `gpu::FramePool` 4 槽环形：每槽 NV12 双平面 + BGRA 画面；`VideoSurface::present_frame` / `present_blend` 直接以纹理为源。插帧权重按播放位置计算，场景切换用上传时采样的 32×18 亮度指纹判断。
- 音频用 WASAPI；音频时钟是主时钟，视频按音频时钟 present。
- 3 个播放器窗口上限；每个窗口独立 swap chain。
- 4K60 实测：4K60 AVC + 插帧 + CAS 输出 103–121 fps（帧队列 6，丢弃 0）；关闭插帧时稳定 60 fps。解码+回读约 60–65 fps 是这台机器的上限，插帧余量来自 100 ms 左右的队列提前量。
- 当前原生播放器仍启动独立 `ffmpeg.exe`。`mediacache` 先以 `prefetch_connections` 个 Range 请求把 DASH 视频/音频写入 `<root>/cache/media` 的稀疏文件（512KB 分块、`.state` 已完成块、`.ok` 完整标记），再由绑定 127.0.0.1 的只读代理按请求范围阻塞等待对应块并喂给 ffmpeg。代理每次读取都夹在当前分块边界内，未下载部分不会以空洞（全 0）返回。播放开始只依赖第 0 块；当前视频缓存完成后，`start_playlist_prefetch` 按队列顺序解析并完整缓存后续条目。A/V 同步以音频设备时钟为主：`maybe_enable_audio_master` 在差值 >300ms 时重锚墙钟；`poll_picture` 在视频队列干涸 650ms 时暂停音频、进入缓冲，帧到达后恢复；`AV_RESYNC_MS` 对持续 800ms 的落后做 8 秒节流的一次硬重同步。关闭插帧时按队列顺序取到期帧（音频时钟是阶梯式更新的，取最新一帧会每秒丢 10 多帧）。流结束按最后一帧时间判定（DASH 轨常短于元数据时长），分P解析有 25 秒超时。

## 5. 弹幕

弹幕由 Rust 画在视频层上（方案 A）：

- Rust：拉取/解析/调度、滚动/顶部/底部、轨道避让、透明度/字号/速度/区域、
  关键词/正则/用户屏蔽、AI 云屏蔽、历史弹幕。
- Electron：弹幕开关、设置面板、输入框、历史日期列表、右键菜单的 UI 外壳。
- 输入框在 Electron 底栏（视频矩形外），提交后 IPC 给 Rust，Rust 立刻上屏并发送。
- 右键/长按命中由 Rust 做（它知道每条弹幕的屏幕矩形）。独立原生播放器的弹幕菜单也由 Rust 绘制：右键按下打开菜单并记录起点，拖动超过 5px 后松开即执行指针下的菜单项，举报原因子菜单同理；普通右键单击仍保留菜单供后续点击。
- 弹幕字体用 Chill Round Gothic；字体以 FR_NOT_ENUM 注册后 GDI 与 DirectWrite 都能解析，和 Electron UI 的 Chromium 文本共享同一套字体文件，光栅化器不同。

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

- 全应用统一使用 **Chill Round Gothic（寒蝉圆黑体）** 400 / 500 / 700。Electron UI 通过
  `@font-face` 从 `dist/fonts` 加载；原生播放器用 `AddFontResourceExW` 从
  `runtime/assets/fonts` 以 `FR_NOT_ENUM` 注册到系统字体表（native player runner 退出时用 `RemoveFontResourceExW` 清理；engine sidecar 内保留到进程结束）；GDI 和 DirectWrite 都解析同一个 family 字符串 `Chill Round Gothic`，因此顶栏、帧号和弹幕不会回退到 YaHei。
- Electron UI 文本由 Chromium 渲染（Windows 上是 DirectWrite/Skia），原生播放器 UI 和弹幕走
  Rust 的 GDI/DirectWrite 管线。两侧字体轮廓和字重一致，光栅化器不同；不再为两套 UI 维护
  两套字体。
- 字体是思源黑体 SC 的圆角化版本，覆盖常用简体与「巽」等少见字；弹幕通过
  `danmaku_font_family()` 使用同一字族，不再打包 Chiron 回退。
- 字体文件在 `public/fonts`（renderer 源）、`runtime/assets/fonts`（原生播放器）各一份；
  `assets/fonts` 只保留开发用副本和 OFL 许可证，打包时用 `!assets/fonts/*.ttf` 排除，避免
  asar 里再存第三份。
- 播放器右上角的 Debug FPS 胶囊使用同一 Chill Round Gothic 圆体 10.5px，默认关闭，由侧栏性能 tab 的开关控制（`PlayerState.stats`），分五行显示画面 FPS / 弹幕 FPS / 读取 bps / 码率 bps / 缓冲秒数 / UI FPS；弹幕值取自独立弹幕层最近 1 秒的实际绘制帧数，没有可见弹幕时为 0。UI FPS 取自 `Overlay::presents()`，统计整个覆盖层（工具栏、侧栏、HUD、浮层和动画）成功 Present 的次数。整个覆盖层使用 `next_ui_draw` 绝对截止时间网格按显示器刷新率绘制，并让 `Present(1, 0)` 与合成器同拍；固定网格消除了 1–2ms 定时抖动把 120Hz 拖成 ~104Hz 的问题。侧栏信息页的简介掩码按完整文本高度测量，滚动只改变 y 并由车道裁剪，不再因为视口高度变化而每帧重新栅格化整段文字。全屏 / 窗口尺寸变化走 `reflow`，保留 active 弹幕并按新 band 重新投影，不清空重来。
- 缺少字体文件时：renderer 退回 `Microsoft YaHei UI` / `Segoe UI`；原生播放器退回系统字体，
  文字照常绘制。

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
- [ ] FFmpeg 硬解在不同 GPU/驱动上的兼容性：当前策略是激进的 GPU-only，探测失败的编码会直接从画质菜单消失；老卡（无 AV1 硬解）将看不到 AV1 流，只保留可硬解的编码。
- [ ] 原生 popup 在 Electron 窗口最小化/隐藏时没有正确隐藏。
- [ ] 多窗口（3 个播放器）时 IPC 会话隔离。
