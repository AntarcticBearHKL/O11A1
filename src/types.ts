export interface Account {
  mid: number;
  uname: string;
  face: string;
  vip: boolean;
}

export interface Settings {
  danmaku_opacity: number;
  danmaku_font: number;
  danmaku_speed: number;
  danmaku_area: number;
  block_scroll: boolean;
  block_top: boolean;
  block_bottom: boolean;
  blocked_senders: string[];
  blocked_keywords: string[];
  blocked_regex: string[];
  player_quality: number;
  player_native_size: boolean;
  player_enhance: number;
  player_mf_decode: boolean;
  player_speed: number;
  hwaccel: boolean;
  codec_pref: number;
  ai_on: boolean;
  ai_level: number;
  theme: number;
  autostart: boolean;
  player_keys: number[];
  electron_player_keys: number[];
  glass_transparency: number;
  glass_blur: number;
  backdrop_blur: number;
  page_size: number;
  history_limit: number;
  close_action: number;
  feed_prefetch_batches: number;
  cdn_select: boolean;
  prefetch_proxy: boolean;
  seek_points: number;
  prefetch_connections: number;
  preload_enabled: boolean;
  preload_comments: number;
  preload_bandwidth_mbps: number;
  net_threads: number;
  playlist: any[];
  playlist_current: number | null;
  play_mode: number;
  playlist_autoplay: boolean;
  playlist_random: boolean;
  loop_one: boolean;
  interp_mode: number;
  deepseek_key: string;
  fish_model: string;
  fish_instruction: string;
  fish_budget_videos: number;
  fish_interval_ms: number;
  fish_enabled: boolean;
  fish_auto_feed: boolean;
  fish_feed_time: string;
  fish_sources: number;
  audio_quality: number;
  hide_live_tab: boolean;
  hide_hot_search: boolean;
  fish_tier: number;
  fish_step_meta: boolean;
  fish_step_subtitle: boolean;
  fish_step_outline: boolean;
  fish_step_comments: boolean;
  fish_step_asr: boolean;
  fish_step_frames: boolean;
  fish_asr_enabled: boolean;
  fish_asr_engine: string;
  fish_asr_device: string;
  fish_asr_model: string;
  fish_asr_threads: number;
  fish_asr_model_dir: string;
  fish_model_mirror: string;
  fish_vision_detail: string;
  pgc_sorts: Record<string, string>;
  search_tab: number;
  [key: string]: unknown;
}

export interface FeedVideo {
  bvid: string;
  cid: number;
  aid: number;
  title: string;
  pic: string;
  owner: string;
  owner_mid: number;
  view: number;
  duration: number;
  pub_ts: number;
}

export interface VideoCard {
  bvid: string;
  title: string;
  pic: string;
  owner: string;
  view: number;
  duration: number;
}

export interface CardPage {
  page: number;
  has_more: boolean;
  items: VideoCard[];
}

export interface VideoPart {
  cid: number;
  page: number;
  title: string;
  duration: number;
}

export interface VideoDetail {
  bvid: string;
  aid: number;
  cid: number;
  title: string;
  description: string;
  typename: string;
  pic: string;
  duration: number;
  pubdate: number;
  owner_name: string;
  owner_mid: number;
  owner_face: string;
  stat_view: number;
  stat_danmaku: number;
  stat_like: number;
  stat_coin: number;
  stat_favorite: number;
  stat_reply: number;
  stat_share: number;
  pages: VideoPart[];
  season?: unknown;
  width: number;
  height: number;
}

export interface SearchPage<T> {
  page: number;
  num_pages: number;
  num_results: number;
  items: T[];
}

export interface SearchHitPage {
  page: number;
  num_pages: number;
  num_results: number;
  items: Array<Record<string, any>>;
}

export interface CommentUser {
  mid: number;
  uname: string;
  avatar: string;
}

export interface Comment {
  rpid: number;
  user: CommentUser;
  message: string;
  like: number;
  ctime: number;
  rcount: number;
  liked: boolean;
  replies: Comment[];
}

export interface CommentPage {
  top: Comment | null;
  comments: Comment[];
  cursor: string;
  is_end: boolean;
  total: number;
}

export interface FavFolder {
  id: number;
  name: string;
  count: number;
}

export interface FavListPage {
  title: string;
  page: number;
  has_more: boolean;
  items: Array<{
    bvid: string;
    title: string;
    cover: string;
    duration: number;
    upper: string;
    owner_mid: number;
    play: number;
  }>;
}

export interface HistoryItem {
  title: string;
  cover: string;
  bvid: string;
  kid: string;
  author: string;
  owner_mid: number;
  duration: number;
  progress: number;
}

export interface HistoryPage {
  cursor: { id: number; time: number } | null;
  items: HistoryItem[];
  has_more: boolean;
}

export interface UpInfo {
  name: string;
  face: string;
  sign: string;
  level: number;
  fans: number;
  friend: number;
  official: string;
  vip: string;
}

export interface SpaceVideo {
  bvid: string;
  title: string;
  pic: string;
  length: string;
  play: number;
  danmaku: number;
}

export interface SpaceVideoPage {
  page: number;
  has_more: boolean;
  items: SpaceVideo[];
}

export interface LiveRoom {
  room_id: number;
  uid: number;
  title: string;
  uname: string;
  online: number;
  cover: string;
  area_name: string;
  parent_area_name: string;
  live_status: number;
}

export interface LiveIndex {
  modules: Array<{ title: string; rooms: LiveRoom[] }>;
  areas: Array<{ id: number; parent_id: number; title: string }>;
}

export interface LiveAreaPage {
  rooms: LiveRoom[];
  has_more: boolean;
  code: number;
}

export interface PgcEpisode {
  ep_id: number;
  aid: number;
  bvid: string;
  cid: number;
  title: string;
  long_title: string;
  duration_ms: number;
  pub_time: number;
}

export interface PgcSeason {
  season_id: number;
  title: string;
  cover: string;
  evaluate: string;
  score: number;
  areas: string[];
  styles: string[];
  pub_time: string;
  is_finish: boolean;
  index_show: string;
  episodes: PgcEpisode[];
  sections: Array<{ title: string; episodes: PgcEpisode[] }>;
}

export interface PgcIndexPage {
  page: number;
  total: number;
  has_next: boolean;
  items: Array<{
    season_id: number;
    title: string;
    cover: string;
    index_show: string;
    order: string;
    badge: string;
    score: string;
  }>;
}

export interface DynamicItem {
  id_str: string;
  author_name: string;
  author_mid: number;
  author_face: string;
  pub_time: string;
  text: string;
  archive_title: string;
  archive_bvid: string;
  archive_cover: string;
  archive_meta: string;
  images: string[];
  like_count: number;
  liked: boolean;
  comment_count: number;
  forward_count: number;
}

export interface DynamicPage {
  offset: string;
  has_more: boolean;
  items: DynamicItem[];
}

export interface MsgItem {
  id: number;
  nickname: string;
  avatar: string;
  title: string;
  detail: string;
  time: number;
  count: number;
}

export interface MsgPage {
  items: MsgItem[];
  cursor: { id: number; time: number };
  is_end: boolean;
}

export interface DmSession {
  talker_id: number;
  at_time: number;
  unread: number;
  preview: string;
}

export interface DmMessage {
  sender_uid: number;
  text: string;
  timestamp: number;
  seqno: number;
  msg_type: number;
}

export interface DmPage {
  messages: DmMessage[];
  has_more: boolean;
  min_seqno: number;
  max_seqno: number;
}

export interface DownloadTask {
  id: string;
  bvid: string;
  cid: number;
  title: string;
  cover: string;
  status: string;
  progress: number;
  speed: string;
  path: string;
  error: string;
  created: number;
}

export interface FishState {
  snapshot: any;
  dashboard: any;
  verdicts: any[];
  errors: any[];
  presets: any[];
  settings: Record<string, any>;
}
