import type { FishAgent, FishState } from '../types';

function step(over: Partial<FishAgent['steps'][number]>) {
  return {
    id: '',
    kind: 'tool',
    title: '',
    meta: '',
    state: 'ok',
    detail: '',
    arguments: '',
    output: '',
    tool: '',
    thinking: '',
    images: [],
    ms: 0,
    bytes: 0,
    tokens: 0,
    ...over,
  };
}

/// A fixed transcript for the standalone UI playground (`agent.html?demo=1`).
/// It exercises every fold: prompts, thinking, tool evidence, nested lists and
/// an error row, without spending a DeepSeek request.
export function createDemoState(): FishState {
  const now = Math.floor(Date.now() / 1000);
  const videos = [
    { bvid: 'BV1demo1', title: '候选视频一：一个很长的标题用来测试菜单行省略', up_name: '测试UP', duration: '12:34', view: 123456 },
    { bvid: 'BV1demo2', title: '候选视频二', up_name: '另一个UP', duration: '03:20', view: 9876 },
    { bvid: 'BV1demo3', title: '候选视频三', up_name: '第三个UP', duration: '08:01', view: 4567 },
  ];
  const agent: FishAgent = {
    bvid: 'BV1demo1',
    title: '演示视频：Agent 菜单折叠',
    up_name: '测试UP',
    cover: '',
    duration_s: 754,
    instruction: '看看这个视频值不值得看，重点看前五分钟。',
    system_prompt: '你是一个看片 Agent。\n\n你需要先用 list_videos 选择候选，再 open_video 看视频信息，必要时 watch_clip 看片段，最后 rate_video 给出结论。\n\n输出要简洁，但证据必须可核对。',
    user_prompt: '视频：BV1demo1\nUP主：测试UP\n指令：看看这个视频值不值得看，重点看前五分钟。',
    model: 'deepseek-flash',
    state: 'done',
    started_at: now - 80,
    updated_at: now,
    note: '',
    steps: [
      step({
        id: 'think-1', kind: 'thinking', title: 'DeepSeek 请求', meta: 'deepseek-flash',
        thinking: '## 先看候选\n\n我需要先找几个候选视频，再决定打开哪一个。\n\n- 候选来源：首页推荐\n- 目标：找 5 分钟以内、信息密度高的片段\n- 重点看：标题、UP 主、时长、播放量\n\n先调用 `list_videos`。',
        ms: 840, tokens: 320,
      }),
      step({
        id: 'tool-1', title: '候选视频', tool: 'list_videos',
        detail: '找到 3 个候选', arguments: JSON.stringify({ source: 'home', limit: 3 }),
        output: JSON.stringify({ label: '首页推荐', count: videos.length, videos }), ms: 120,
      }),
      step({
        id: 'tool-2', title: '打开视频', tool: 'open_video',
        detail: '演示视频：Agent 菜单折叠', arguments: JSON.stringify({ bvid: 'BV1demo1' }),
        output: JSON.stringify({
          title: '演示视频：Agent 菜单折叠',
          up_name: '测试UP',
          duration: '12:34',
          view: 123456,
          tname: '科技',
          comments_total: 42,
          subtitle_cues: 18,
          desc: '这是一个用于检查 Agent 菜单折叠效果的演示视频。',
          comments: [
            { user: '观众A', like: 1200, text: '这个折叠菜单看起来很清楚。' },
            { user: '观众B', like: 88, text: '每个工具调用都可以单独展开，调试方便。' },
            { user: '观众C', like: 12, text: '评论也可以折叠。' },
          ],
        }),
        ms: 260,
      }),
      step({
        id: 'tool-3', title: '看片段', tool: 'watch_clip',
        detail: '00:30 - 02:30', arguments: JSON.stringify({ bvid: 'BV1demo1', start_s: 30, end_s: 150 }),
        output: JSON.stringify({
          interval_s: 1,
          frame_count: 120,
          sheet_count: 2,
          danmaku_total: 36,
          subtitle_source: 'cc',
          focus: '看看前五分钟有没有实质内容',
          subtitles: [
            { at: '00:30', text: '这是第一条字幕，用来测试字幕菜单。' },
            { at: '00:34', text: '这是第二条字幕。' },
            { at: '00:38', text: '这是第三条字幕。' },
          ],
          danmaku: [
            { at_s: 31, text: '弹幕一' },
            { at_s: 44, text: '弹幕二' },
            { at_s: 59, text: '弹幕三' },
          ],
        }),
        ms: 1800, bytes: 4500000,
      }),
      step({
        id: 'tool-4', title: '评分', tool: 'rate_video',
        detail: '值得看 · 8/10',
        arguments: JSON.stringify({ score: 8, verdict: '看', one_line: '信息密度不错', reasons: ['有具体例子', '节奏紧凑'], tags: ['科技', '评测'], quote: '这里给出一个具体结论' }),
        output: JSON.stringify({ ok: true }), ms: 90,
      }),
      step({
        id: 'error-1', kind: 'error', title: '一个失败的工具调用', state: 'error',
        detail: '模拟错误：连接超时',
        thinking: '工具调用失败：HTTP 504 Gateway Timeout\n\n重试一次仍然失败，已跳过。',
        ms: 30000,
      }),
    ],
    reply: null,
    tokens_in: 1200,
    tokens_out: 340,
    cost_ms: 4200,
    cost_bytes: 4500000,
    cost_tokens: 1540,
  };
  return {
    snapshot: { state: 'Idle', worker_alive: false, agent_waiting: false, agent },
    dashboard: null,
    verdicts: [],
    errors: [],
    presets: [],
    settings: { deepseek_key_set: true },
  } as FishState;
}
