# 养鱼「看片力度」设计稿（v1 草案）

目标：把 dsh-bili-humanism 的"多通道取证"思路搬进 O11A1 的养鱼，让**每一条鱼看一个视频花多少力气**变成用户可选的、可审计的东西。

现状（`src/fish.rs`，2834 行）：只会一种看法——官方 AI 提纲/字幕（`Conclusion`）+ 评论 → 拼一个 prompt → DeepSeek 出"看/不看"。

---

## 一、把"看"拆成可数的步骤

每个步骤都必须独立可开关、可估算、可在结果里留下证据。

| 步骤 | 做什么 | 产出 | 估算耗时 | 估算 token | 依赖 |
|---|---|---|---|---|---|
| `meta` | 标题 / UP / 时长 / 分P / 封面 | 文字 | 0（feed 已有） | 0 | 无 |
| `subtitle` (A1) | 官方字幕轨 `x/player/wbi/v2` → 字幕 json | 带秒的逐句 | 0.3–1.5 s | 原文长度 | 登录 |
| `outline` (A2) | `view/conclusion/get`（WBI 签名） | 带时间点的提纲 | 0.5–2 s | 同上 | 登录 |
| `comments` | 评论区热门前 N 条 | 文字 | 0.5–2 s | 2–6 k | 无 |
| `asr` | 下载音频 → Whisper 转写全片 | 带秒的逐句 | GPU small ≈ 30–60 s / 10 min 片；CPU ≈ 3–5 min | 原文长度 | 模型 + 引擎 |
| `frames` | 抽 ≤40 张 480p 帧 | 图片 | 下载 20–60 MB + 抽帧 3–8 s | 每张约 1–1.5 k（视觉模型） | 视觉模型 |
| `ocr` | 对抽出的帧做文字识别（本地） | 屏幕文字 + 时间点 | 0.2–1 s / 张 | 少量 | OCR 引擎 |
| `coverage` | 算"读到了整片的百分之几" | 覆盖率 + 盲区说明 | 0 | 0 | 无 |
| `verdict` | 交给模型出"看/不看/也许" + 理由 | 判定 | 3–15 s | 2–30 k | DeepSeek key |

> 所有数字都是**估算**，第一次实现后要用真实日志回填。

## 二、努力档位（就是用户要的"努力程度"）

档位只是预设的步骤组合，用户仍然可以逐项手改（改完档位自动变成"自定义"）。

| 档 | 名字 | 步骤 | 适用 |
|---|---|---|---|
| T0 | 只闻其名 | `meta` | 标题党初筛，只看标题/时长 |
| T1 | 听个响 | `meta + subtitle + comments + verdict` | 现在的能力 |
| T2 | 翻翻提纲 | T1 再加 `outline` | 没有字幕时的兜底 |
| T3 | 逐字听写 | `meta + asr + comments + verdict` | 想要逐字证据、字幕不可用时 |
| T4 | 边听边看 | T3 + `frames` + `ocr`（或视觉模型） | 最贵，带画面证据 |

**两段式（推荐）**：先 T1 粗筛，只有"也许/看"的候选再升级到 T3/T4。一天 40 个视频不会全量跑 ASR。

## 三、模块解耦（不写在一坨里）

```
src/fish/
  mod.rs        FishEngine 门面（保持现有对外 API 不变）
  store.rs      SQLite（迁移现有 fish.rs 的存储层）
  preset.rs     食谱
  prompt.rs     提示词拼装
  verdict.rs    DeepSeek 调用 + 结构化解析
  watch/
    mod.rs      WatchPlan（档位 → 步骤序列）、WatchRun（执行 + 计时）
    steps.rs    trait Step { id / estimate / run }
    transcript.rs  A1 / A2（转发 bilibili::）
    comments.rs    评论
    asr/
      mod.rs    trait Transcriber
      whispercpp.rs  进程内引擎（cuda / vulkan / cpu）
      external.rs    外部命令适配器（faster-whisper 等）
      models.rs      模型目录 / 下载 / sha256 / 进度
    frames/
      mod.rs    trait FrameSource（优先复用播放器缓存，其次整片下载）
      sample.rs 均匀抽帧（ffmpeg）
      ocr.rs    本地 OCR（可选）
    coverage.rs 覆盖率与盲区文案
    cost.rs     每个步骤的耗时 / 字节 / token 记账
```

约束：
- `watch/*` 不直接碰 UI，也不直接读 `settings`；参数通过 `WatchConfig` 传进来。
- UI 只读 `WatchRun` 的进度事件与 `WatchOutcome` 的最终数据。
- 每个步骤失败 → 记一条 `skipped(reason)`，**降级不伪装**。

## 四、Whisper 方案（本机已验证：RTX 4060 Ti 16G / 驱动 610.88 / CUDA 12.9 工具链已装 / Vulkan 可用）

- 引擎：**whisper.cpp**（Rust 绑定 `whisper-rs`），进程内跑，不依赖 Python。
- 后端三选一（设置里切换）：
  - `cuda`：本机工具链现成，最快（首选）
  - `vulkan`：不需要 CUDA 工具链，跨机器可移植（备选）
  - `cpu`：兜底，慢 3–5 倍
- 模型：GGML 格式，目录 `models/whisper/`，带 **sha256 校验 + 下载进度 + 断点续传**；提供 small / turbo / medium 三档，默认 small。
- 并发：Whisper 与播放器共用 GPU，**默认串行**（播放中不跑），并限制单次显存/线程；可在设置里放开。
- 隐私：转写文本会进 DeepSeek 请求，设置里明确标注。

## 五、每条结论都要能审计

`verdicts` 表新增几列（或旁表）：

- `channels`：实际用到的通道（`subtitle,comments,asr,frames`）
- `coverage_fraction` + `coverage_note`：读到了多少、漏了什么
- `cost_ms / cost_bytes / cost_tokens`：这一步花了多少
- `skipped`：没跑的步骤与原因

UI：判定卡下面一行小字「证据：字幕 87% · 评论 20 条 · 未跑 ASR」，点开看明细。

## 六、已知风险

| 风险 | 处理 |
|---|---|
| 视频没字幕、ASR 又没开 | 结论只能基于标题+评论 → 必须标"未取到口播" |
| PGC/番剧没有 `x/player/view` 的提纲 | A2 缺席是正常态，不报错 |
| 480p 整片下载浪费流量 | **优先复用播放器已缓存的片段**；没有再下载，且用完即删 |
| 视觉模型缺席 | `frames` 步骤直接禁用并说明，不许假装"看过画面" |
| ASR 专名错 | `coverage_note` 里标注"ASR 专名需回查"，可选接一次专名交叉 |

## 七、待定的设计问题

见对话中的 grill 清单（成本、视觉模型归属、GPU 抢占、两段式、缓存复用、隐私、模型档位、失败降级）。
