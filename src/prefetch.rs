//! Local loopback prefetch proxy for the player.
//!
//! `player.rs` runs ffmpeg against a CDN URL and reads raw frames from its
//! stdout. ffmpeg's HTTP client is single-connection and reports no byte
//! counters. This module puts a tiny HTTP/1.1 server on `127.0.0.1:<random>`
//! between ffmpeg and the CDN:
//!
//! * ffmpeg reads the local URL; every `Range: bytes=N-` it sends is served
//!   from a read-ahead window that up to [`PrefetchConfig::connections`]
//!   (default 16, max 32, `0` = auto) worker tasks fill.
//! * the engine counts exactly how many bytes it pulled from upstream, per
//!   host, per worker and per second, which is what the profiler panel shows.
//!
//! **Whole-file, priority-ordered scheduling, bounded by a media horizon.**
//! The queue covers the file in [`PrefetchConfig::chunk_bytes`] chunks, up to
//! [`PREFETCH_AHEAD_SECONDS`] of media beyond the playhead (an hour, i.e. the
//! whole file for anything a person watches in one sitting). The queue used to
//! stop at [`PLAYBACK_AHEAD_SECONDS`] (90 s), which made "整片缓存完成" take as
//! long as watching the video; only the *retained* window is still 90 s, bytes
//! past it are persisted and dropped from memory. A worker that finishes picks,
//! in order: (1) the chunk(s) the client reads now and the next
//! [`HOT_AHEAD_SECONDS`] of playback, (2) a range that failed its retry route,
//! (3) the seek warm points, (4) the rest of the file. For a file above 64 MiB
//! the claimable bytes are split into one part per connection; an exhausted
//! part steals sub-chunks from the largest remaining part, so no worker sits
//! idle while claimable bytes exist.
//!
//! **The speculative queue is rate limited, but never below what the live
//! connections can pull.** Warm points and the rest of the file share one token
//! bucket refilled at [`WARM_RATE_FACTOR`]× (while points are pending) or
//! [`SPECULATIVE_RATE_FACTOR`]× the stream bitrate — `total / duration`, or the
//! session-average serve rate when the caller does not know the duration —
//! raised to [`MIN_SPECULATIVE_CONNECTION_BPS`] per live connection and capped
//! at [`SPECULATIVE_CEILING_BPS`]. The old fixed 2× bitrate was 460 kB/s on a
//! 1080p stream: a 4 MiB claim reserved the whole bucket, so **one or two of
//! the sixteen connections ever fetched** and a 145 MB file needed five
//! minutes. The burst is one chunk per live connection, so every configured
//! worker can be in flight at once; playback bytes are never limited.
//!
//! **The connection count follows the measured knee, not the setting.** Bilibili
//! mirrors throttle per stream: measured on one mirror, 1 connection pulled
//! 22 MB/s, 4 pulled 19 MB/s and 16 pulled 8–10 MB/s. So the session starts at
//! [`STARTUP_CONNECTIONS`], then a governor doubles/halves the live count every
//! [`GOVERNOR_TICK`] while the aggregate throughput keeps improving, and settles
//! on the count that measured best (bounded by the configured value, never below
//! two). The knee is only measurable if the bucket is not the smaller of the
//! two: with the old 16 MiB/s ceiling two streams filled it, the governor saw no
//! gain at four and parked there. The ceiling is now 48 MiB/s, and a session on
//! this box ramps 2→4→8→16 and pulls 40–52 MB/s (see the QA table below). Only a
//! serve wait that cost the client real time ([`SERVE_WAIT_SLOW_MS`]) halves the
//! count; the ordinary 5–14 ms miss of a healthy reader used to do it on every
//! seek and buffer refill. The line
//! `prefetch fetch active=N inflight=M live=K configured=C agg=…/s per=[…]
//! waits=W slow=S` carries the whole picture once per second — `active`/`inflight`
//! are what is really transferring, so "16 configured, two working" is visible.
//!
//! **Network reads are batched into disk-sized blocks.** reqwest hands out one
//! HTTP/2 frame (16 KiB) per `chunk()`, and every read used to take the state
//! lock, copy its bytes, publish the whole download map and queue a disk write.
//! [`READ_BATCH_BYTES`] accumulates 256 KiB (one disk block) per push, except the
//! first read of a response, which is pushed alone so a serve wait never waits
//! for the batch.
//!
//! **The serve path has absolute priority over speculation.** A rate-limited
//! queue still lets a speculative burst own every connection at the moment the
//! client needs a byte: measured `serve wait pos=3670016 waited_ms=6029` while
//! 117 MB of seek points were being fetched. Now a miss in the serve path opens
//! a *serve wait* ([`Engine::open_serve_wait`]): while it is open no worker may
//! claim a `Warm` or `Rest` chunk, and every speculative claim already in
//! flight is cancelled at its next network read so its slot can take the
//! missing hot chunk. The speculative queue resumes the moment the bytes land
//! (or the body ends) — this changes *priority*, never throughput or the
//! [`PREFETCH_AHEAD_SECONDS`] horizon.
//!
//! The wait is instrumented and bounded. Once per second it logs
//! `serve wait pos=… waited_ms=… hot_inflight=… warm_inflight=… rest_inflight=…`,
//! and it closes with `serve wait end pos=… waited_ms=… reason=available` when
//! bytes land or `reason=timeout` when the body ends without them. The wait also
//! registers its **exact offset** as an urgent claim, so the next worker starts
//! its range at the blocked byte instead of at the chunk boundary — on a 4 MiB
//! chunk that prefix is seconds of media the client cannot use yet. A hot claim
//! that covers the missing offset but has produced no byte for [`HOT_STUCK_MS`]
//! is logged as `hot stuck … switching host`, marked slow (the existing
//! one-shot retry), its route degraded, and a duplicate raced on another mirror
//! — the same per-host retry/race path as the five-second health watchdog, only
//! 4.5 s earlier.
//!
//! [`PrefetchConfig::warm_points`] evenly spaced positions (`i / count` of the
//! total) go through the same worker pool into a bounded **point cache**
//! (80 MiB). A request whose offset is in a cached point or in the buffered
//! window is answered from memory — a seek to either never goes upstream. The
//! window (32 MiB by default, up to 256 MiB, and never more than
//! [`PLAYBACK_AHEAD_SECONDS`] of media) is the only retained data: chunks
//! outside it are counted, persisted, then discarded, so the whole-file queue
//! costs cache file, not process memory.
//!
//! **Per-worker health.** Throughput is measured over a five-second window.
//! Zero bytes for 5 s (or below 20 % of the median worker for 15 s) marks a
//! worker slow: its request is cancelled, its route is degraded for a cooldown,
//! and the same range is retried once on another mirror. If an idle worker
//! exists, a duplicate request for the slow range races on a different host and
//! the first finisher wins (`worker race … winner=…`).
//!
//! **The newest request always wins.** ffmpeg stops reading an open-ended
//! `bytes=N-` response as soon as it seeks, and opens a second connection for
//! the new position. A `206` like that promises the rest of the file, so the
//! response still writing it used to block in `write_all` (the client no longer
//! reads) while holding the single-response lock: the seek then waited 10 s,
//! got no answer, and ffmpeg restarted — the `partial file` restart loop.
//! Every accepted request now bumps [`Engine::serve_epoch`], the abandoned
//! handler notices at its next write poll (20 ms) and drops its body, so the
//! seek is served in tens of milliseconds.
//!
//! There is no size probe in front of playback. The workers start real ranged
//! GETs as soon as the proxy is bound, and the first client request is answered
//! from those in-flight responses: their `Content-Range` / `Content-Length`
//! supply the total length and content type every later `206` needs. ffmpeg
//! therefore waits for one upstream round trip, not for a probe plus a fetch.
//!
//! Only loopback is bound, no authentication is added, and any request whose
//! path+query differs from the configured media URL is rejected: the server is
//! a bridge for one URL, not an open proxy. The upstream path and query are
//! forwarded unchanged to the chosen mirror host (Bilibili signatures do not
//! cover the host); URLs under `/v1/resource` keep their original host.
//!
//! **The session cache file is the truth for "downloaded".** Every fetched byte
//! is also written to a sparse per-session file
//! (`%LOCALAPPDATA%\o11a1\cache\<key>.part`, capped at [`CACHE_CAP_BYTES`]) and
//! tracked as an exact list of present ranges. [`PrefetchHandle::cached_ranges`]
//! — and therefore the 下载分布 panel, which reads
//! [`PrefetchStats::downloaded`] — reports the *file*, not the 32 MiB memory
//! window. The window is a hot cache in front of it, so a seek, a window trim or
//! the whole-file queue passing a region can no longer make the map forget bytes
//! that still exist locally.
//! Serving order is memory → disk → upstream; a disk hit never goes upstream.
//!
//! **The advertised length is authoritative.** A ranged GET that stops before
//! its end is a truncated range, never a shorter file: the chunk goes back to
//! the queue (and is retired only after [`SHORT_BODY_LIMIT`] attempts). It used
//! to rewrite the total, which made the serving body close at that offset —
//! ffmpeg's `Stream ends prematurely at N, should be M` — and the player then
//! ran its "playback ended" path mid-video. The same rule applies to a mirror
//! that fails: one transient body error cools the route down
//! ([`HOST_FAILURE_LIMIT`] consecutive errors retire it) instead of ending the
//! only stream the user has.
//!
//! Shutdown is drop-based: dropping [`PrefetchHandle`] signals every task,
//! aborts any connection that is still streaming, closes the listener, and
//! deletes the session cache file.
//!
//! **Playlist preload.** [`PrefetchHandle::preload`] (URLs) and
//! [`PrefetchHandle::preload_keys`] (`(cache key, URL)` pairs) fetch the tracks
//! that follow the current one into their own cache files, in playlist order, at
//! background priority: one sequential range reader that yields between chunks
//! and stops on [`PrefetchHandle::cancel_preload`], on drop, or at
//! [`CACHE_CAP_BYTES`]. Nothing is wired into `main.rs` on purpose — the UI
//! session owns that file; the exact call site is documented on
//! `PlaybackControl::preload_playlist` in `player.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener as StdTcpListener};
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    RANGE, REFERER,
};
use reqwest::StatusCode;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// Default read-ahead window (32 MiB), the amount buffered beyond the cursor.
pub const DEFAULT_READ_AHEAD_BYTES: u64 = 32 << 20;
/// Default size of one upstream range request (4 MiB).
pub const DEFAULT_CHUNK_BYTES: u64 = 4 << 20;
/// Default number of parallel upstream fetchers. `0` in a config means this
/// value (see [`PrefetchConfig::normalized`]).
pub const DEFAULT_CONNECTIONS: usize = 16;
/// Default bytes cached per warmed seek point (4 MiB).
pub const DEFAULT_POINT_BYTES: u64 = 4 << 20;
/// Hard cap on the whole point cache (80 MiB). `count * chunk` is clamped to
/// this, so warming can never grow the process without bound.
pub const MAX_POINT_CACHE_BYTES: u64 = 80 << 20;
/// Most seek points that can be warmed at once.
pub const MAX_WARM_POINTS: usize = 20;
/// Upper bound on [`PrefetchStats::downloaded`] entries. Past this, the ranges
/// across the smallest gaps are merged, so the UI never receives a fragmented
/// list (and a 128 MiB window of 256 KiB chunks still costs one small vector).
pub const MAX_DOWNLOADED_RANGES: usize = 64;

/// Granularity of the test fixtures for the presence map (a network read and a
/// media block are both around this size); the map itself is exact.
#[cfg(test)]
const CACHE_BLOCK_BYTES: u64 = 256 << 10;
/// Hard cap on one session cache file: 2 GiB. Past it the file simply stops
/// growing (one `cache cap reached` line) and the memory path keeps working.
pub const CACHE_CAP_BYTES: u64 = 2 << 30;

// ---------------------------------------------------------------------------
// The one disk writer
//
// Every fetch worker used to call into `DiskCache` itself: one mutex, one
// `seek_write` per network read, sixteen threads, random offsets, one file. On
// an NVMe box that is invisible; on a machine whose filesystem serialises
// positional writes (or where a filter driver sits in the path) it is the
// "everything freezes while a video plays" report.
//
// Now a session has exactly **one** thread that touches the file. Workers send
// `(offset, bytes)` on a bounded channel and never wait for the disk:
// - a full queue drops the block (the map only records successful writes, so a
//   dropped block simply reads as "not cached");
// - the writer merges adjacent blocks that arrive close together into one run
//   (up to [`DISK_MERGE_MAX`]), so the file is written in large, mostly
//   sequential calls instead of many small interleaved ones;
// - `set_len` and file creation stay on the writer (creation happens once, when
//   the session opens, before any worker exists).
// ---------------------------------------------------------------------------

/// Blocks a queue may hold before workers drop theirs. 256 × one read (tens of
/// KiB) is a few MiB of slack: deep enough that a burst of sixteen streams never
/// drops, shallow enough that a stuck disk cannot grow the process.
const DISK_QUEUE_DEPTH: usize = 256;
/// Largest single write the coalescer issues (4 MiB).
const DISK_MERGE_MAX: usize = 4 << 20;
/// Bytes the coalescer may hold across all open runs. Bounds the writer's own
/// memory: it can never grow with the cache file.
const DISK_BUFFER_BUDGET: usize = 16 << 20;
/// Open runs kept at once. Each in-flight upstream range owns one, so this
/// tracks [`MAX_CONNECTIONS`]: at eight, the ninth concurrent range forced a
/// flush of somebody else's run on the writer thread.
const DISK_RUNS_MAX: usize = MAX_CONNECTIONS;
/// A run that stopped growing this long is flushed; it is what turns a stream
/// of network reads into one large write without waiting for the range to end.
const DISK_RUN_IDLE: Duration = Duration::from_millis(20);
/// How often the writer logs its own call pattern (calls/s, average call size).
const DISK_WRITER_TRACE: Duration = Duration::from_secs(5);

const MIN_READ_AHEAD_BYTES: u64 = 8 << 20;
const MAX_READ_AHEAD_BYTES: u64 = 256 << 20;
const MIN_CHUNK_BYTES: u64 = 256 << 10;
const MAX_CHUNK_BYTES: u64 = 16 << 20;
/// Connections a config may ask for (`0` = auto).
const MAX_CONNECTIONS: usize = 32;
/// Files larger than this get the dynamic split of the remaining bytes across
/// the configured connections (whole-file scheduling).
const DYNAMIC_SPLIT_THRESHOLD: u64 = 64 << 20;
/// Health window over which a worker's throughput is measured.
const HEALTH_WINDOW: Duration = Duration::from_secs(5);
/// Zero bytes for this long marks a worker slow.
const HEALTH_SLOW_AFTER: Duration = Duration::from_secs(5);
/// Below 20 % of the median worker for this long marks a worker slow.
const HEALTH_LOW_AFTER: Duration = Duration::from_secs(15);
/// Poll interval of the per-worker health watchdog.
const HEALTH_POLL: Duration = Duration::from_millis(1000);
/// How often the connection governor measures aggregate throughput and logs the
/// `prefetch fetch active=…` line. One second is the QA cadence; the count
/// itself is stepped every second *decision* (two ticks), because a 4 MiB chunk
/// at the mirrors' real rate takes longer than one second and a faster loop
/// would act on half-finished transfers.
const GOVERNOR_TICK: Duration = Duration::from_secs(1);
/// A governor step only counts as an improvement past this factor. Below it the
/// two counts are treated as the same speed and the smaller one wins: on a
/// mirror that throttles per stream the knee is one or two connections, and
/// every extra stream costs CPU (TLS state, syscalls, context switches) the
/// decoder would rather have.
const GOVERNOR_IMPROVE: f64 = 1.25;
/// Aggregate throughput that marks a fresh measurement as collapsed (a mirror
/// started throttling, or the link is shared): the governor re-explores.
const GOVERNOR_COLLAPSE: f64 = 0.55;
/// Never step below two connections: one stream cannot fill the first window
/// while a seek is being served on the same mirror.
const GOVERNOR_MIN_CONNECTIONS: usize = 2;
/// How long after the first byte reaches the client the speculative queue stays
/// on the old, small budget ([`SPECULATIVE_RATE_FACTOR`] × bitrate, two chunks).
///
/// The opening is the only moment where the prefetch can cost frames: the
/// decoder is starting, the interpolator is warming up, and the ring is still
/// shallow. Five seconds of near-silence costs one 4 MiB chunk on a 1080p
/// stream — the hot window is never limited, so there is no rebuffering — and
/// the whole-file burst starts right after, when the player is already steady.
const SPECULATIVE_WARMUP: Duration = Duration::from_secs(5);
/// How often a worker above the live connection count checks whether the pool
/// was raised again (cheap: no state lock while it waits).
const IDLE_CONNECTION_POLL: Duration = Duration::from_millis(20);
/// A worker learns that more connections were asked for at this interval.

/// How often the accept loop and the workers re-check the shutdown flag /
/// for newly available window space.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const WAIT_POLL: Duration = Duration::from_millis(2);
/// How long a serve handler waits before trying to take the response lock
/// again. Short, because the current holder is preempted by the same token.
const SERVE_POLL: Duration = Duration::from_millis(5);
/// Poll interval of a body write: long enough to cost nothing while a slow
/// (paused) reader keeps its connection, short enough that a superseded
/// response lets go of the lock within a few tens of milliseconds.
const WRITE_POLL: Duration = Duration::from_millis(20);
/// Upper bound on waiting for the response lock. Only reachable if a handler
/// is stuck outside its own write path; normally the preempting token releases
/// it within [`WRITE_POLL`].
const SERVE_TIMEOUT: Duration = Duration::from_secs(10);
/// A serve wait must not outlast this while the network is healthy. Past it the
/// wait is escalated: every speculative fetch is cancelled and the hot range's
/// route is cancelled and raced (see [`Engine::enforce_serve_priority`]).
///
/// This is an SLO, not a hard abort: the body keeps waiting (closing it would
/// end playback), but the log names the breach and the scheduler moves the bytes.
const SERVE_WAIT_BUDGET_MS: u64 = 1500;
/// A serve wait shorter than this is the ordinary "the client asked for a byte
/// the buffer has not reached yet" of a healthy session (measured 5–14 ms on a
/// 1080p stream). Only a wait at least this long counts against the live
/// connection count: seven missed frames at 30 fps.
const SERVE_WAIT_SLOW_MS: u64 = 250;
/// A hot chunk that has produced no byte for this long while a serve wait is
/// open is stuck. It is cancelled and re-routed now instead of waiting for the
/// five-second [`HEALTH_SLOW_AFTER`] timer, which is thirty times a frame.
const HOT_STUCK_MS: u64 = 500;
/// How often, while a serve wait is open, the scheduler enforces hot priority
/// (preempt speculative claims, watchdog a silent hot range).
const SERVE_WAIT_ENFORCE_POLL: Duration = Duration::from_millis(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the first client request waits for the first upstream response
/// headers (which carry the total length) before failing with a `504`.
const FIRST_METADATA_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// One second of traffic history is what `bytes_per_sec` averages over.
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Read-ahead a session starts with (see [`STARTUP_WINDOW`]); the configured
/// window (32 MiB by default) takes over once frames are flowing.
///
/// 2 MiB is ~5 s of a 3.3 Mbps stream and several times what the pre-roll gate
/// waits for, so the first frame never waits on it — but it is a quarter of the
/// I/O, TLS and disk work the old 8 MiB window did while the app was still
/// opening (measured 0.6–0.9 cores for the first seconds on a slow CDN).
pub const STARTUP_READ_AHEAD_BYTES: u64 = 2 << 20;
/// Connections a gated session starts with. Two is the measured knee on a
/// mirror that throttles per stream (1 conn 22 MB/s, 4 conns 19 MB/s, 16 conns
/// 8–10 MB/s), and it is enough to fill the startup window in well under the
/// gate's budget: the governor owns the count from there.
const STARTUP_CONNECTIONS: usize = 2;
/// How long a session keeps the smaller [`STARTUP_READ_AHEAD_BYTES`] window
/// before the configured value takes over.
const STARTUP_WINDOW: Duration = Duration::from_secs(3);
/// Upper bound on waiting for the first client byte before the window is
/// widened anyway (a proxy with no reader must not stay small forever).
const STARTUP_WINDOW_MAX: Duration = Duration::from_secs(15);
/// Poll interval of the startup-gate task.
const STARTUP_POLL: Duration = Duration::from_millis(100);

/// Media seconds of read-ahead the proxy may hold ahead of the playhead.
///
/// The configured window (`read_ahead_bytes`, 32 MiB by default, up to 256)
/// stays the hard memory bound, but it is also clamped to this many seconds of
/// the stream: a 400 kbit/s 720P stream would otherwise buffer ten minutes of
/// video, and every byte of it is memory, disk write and trim work. 90 s is
/// what the owner asked for and is far more than a seek inside the window
/// needs.
pub const PLAYBACK_AHEAD_SECONDS: u64 = 90;
/// Media seconds the whole-file queue may run ahead of the playhead.
///
/// This used to be [`PLAYBACK_AHEAD_SECONDS`]: the queue stopped 90 s ahead, so
/// "整片缓存完成" could never be faster than watching the video, and the 下载分布
/// panel crawled at the stream bitrate. The queue now covers the whole file (an
/// hour is longer than any single 视频/番剧 this player opens); the *retained*
/// window is still [`PLAYBACK_AHEAD_SECONDS`], so memory did not move — bytes
/// past the window go to the session cache file and are dropped from memory.
pub const PREFETCH_AHEAD_SECONDS: u64 = 3600;
/// Media seconds of read-ahead the hot path keeps in flight. The rest of the
/// window is filled by the background queue at the speculative rate below.
const HOT_AHEAD_SECONDS: u64 = 12;
/// Speculative (whole-file) bytes per second, as a multiple of the measured
/// media bitrate. This is a *floor*, not the whole story: see
/// [`MIN_SPECULATIVE_CONNECTION_BPS`].
const SPECULATIVE_RATE_FACTOR: u64 = 2;
/// Absolute floor for one live connection's share of the speculative budget.
///
/// The old policy used [`SPECULATIVE_RATE_FACTOR`] × bitrate and nothing else,
/// which on a 1080p stream (1853 kbit/s ≈ 231 kB/s) is 462 kB/s for the whole
/// session: one 4 MiB claim reserved ~9 s of budget, so at most two of the
/// sixteen configured connections ever fetched and the rest sat idle while the
/// file crawled in at 0.45 MB/s. A connection that is allowed to run must be
/// allowed to run at a real rate; 2 MiB/s is a fifth of what one measured
/// mirror stream gives, so the bucket stops being the scheduler.
const MIN_SPECULATIVE_CONNECTION_BPS: u64 = 2 << 20;
/// Hard ceiling on speculative throughput. Playback is never limited, but the
/// whole-file queue must not become "download the internet": it bounds the TLS
/// and disk work one session may add.
///
/// **This is a parallelism bound, not just a speed limit.** In steady state
/// [`speculative_rate`] returns exactly this value, so the token bucket, not the
/// CDN, decides how many workers get bytes: at 16 MiB/s two streams already
/// saturated it, every other worker had no tokens, the governor measured "no
/// improvement" at four and settled on two — the user's "16 线程但只有一两个在
/// 干活", only with a fast pair instead of a slow one. 48 MiB/s leaves room for
/// 6–8 four-MiB streams on a link that can take it; the governor still trims the
/// count to the measured knee and a serve wait still halves it immediately.
/// `BILI_PREFETCH_CEILING_BPS` pins it for an A/B run (see [`spec_ceiling`]).
const SPECULATIVE_CEILING_BPS: u64 = 48 << 20;
/// Refill factor while seek points are still being warmed: 6× the bitrate.
/// The whole warm plan is a few dozen MiB, and at this rate it is one or two
/// streams instead of sixteen racing the decoder.
const WARM_RATE_FACTOR: u64 = 6;
/// Burst of the speculative token bucket, in chunks per live connection: one
/// in-flight claim per connection is exactly the concurrency the configuration
/// asks for, and a two-chunk burst (the old value) serialised the pool down to
/// two workers no matter how many were configured.
const SPECULATIVE_BURST_CHUNKS_PER_CONNECTION: u64 = 1;
/// Network reads are accumulated into blocks of this size before they meet the
/// state lock / publish / disk queue (see `run_claim`). 256 KiB is the disk
/// block size, so one push becomes one disk block and one publish instead of
/// sixteen 16 KiB frames doing it sixteen times. The first read of a response is
/// never batched, so a serve wait still sees its bytes immediately.
const READ_BATCH_BYTES: u64 = 256 << 10;
/// How many short upstream bodies one chunk may produce before it is retired.
/// A mirror that keeps truncating the same range must not loop forever; the
/// serve path then reports the range as unavailable instead of silently
/// claiming EOF.
const SHORT_BODY_LIMIT: u32 = 4;
/// Consecutive failed range attempts before a mirror is retired for the
/// session. Below it the route only cools down (1 s, then 2 s, 3 s): a single
/// `error decoding response body` on the one configured host used to end
/// playback, because the range it held was the one the client was reading.
const HOST_FAILURE_LIMIT: u32 = 3;
/// Attempts one range gets, across hosts and identical retries, before it is
/// retired. A body that dies mid-range is normally transient (a CDN edge
/// resetting one connection), and the retry resumes exactly at `cursor`.
const RANGE_ATTEMPTS: u32 = 3;
/// Pause before retrying the same range on the same route when no other host
/// is available.
const RETRY_BACKOFF: Duration = Duration::from_millis(300);
/// Bounds of the fallback bitrate estimate when the caller supplied no duration
/// (128 kbit/s .. 16 Mbit/s). Inside them the session average is used as is.
const FALLBACK_BITRATE_MIN: u64 = 16 * 1024;
const FALLBACK_BITRATE_MAX: u64 = 2 << 20;
/// Wait of a worker with nothing to claim. Nothing is due for it: the hot
/// window is filled, the token bucket is empty and no seek point is pending.
/// Two milliseconds of polling × sixteen workers was 8 000 state-lock wakeups
/// per second of pure overhead.
const IDLE_WAIT: Duration = Duration::from_millis(15);

// ---------------------------------------------------------------------------
// Public configuration and stats
// ---------------------------------------------------------------------------

/// `BILI_STARTUP_GATE=0` restores the pre-gate schedule: the read-ahead window
/// is the configured size from the first byte, the connection count is the
/// configured one, feed background work runs while a player is open, and seek
/// points are warmed as soon as the first byte is served. Only the
/// before/after QA runs use it; the gate is on by default.
pub fn startup_gate_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_STARTUP_GATE")
            .map(|value| value.trim() != "0")
            .unwrap_or(true)
    })
}

/// Tuning for one proxy instance. [`PrefetchConfig::normalized`] clamps every
/// field, so callers may pass anything.
#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Bytes to keep buffered beyond the read cursor (8..=256 MiB). This is
    /// the whole memory bound of the window: speculative whole-file bytes that
    /// fall outside it are counted but not retained.
    pub read_ahead_bytes: u64,
    /// Size of one upstream range request (256 KiB..=16 MiB, at most the
    /// read-ahead window).
    pub chunk_bytes: u64,
    /// Parallel upstream fetchers (1..=32, `0` = [`DEFAULT_CONNECTIONS`]).
    /// Every one of them may be in flight at the same time.
    pub connections: usize,
    /// Seek positions warmed in the background once playback started
    /// (0 = no warming, 1..=[`MAX_WARM_POINTS`]). Each point caches at most
    /// [`PrefetchConfig::point_bytes`], and the whole point cache is capped at
    /// [`MAX_POINT_CACHE_BYTES`].
    pub warm_points: usize,
    /// Bytes cached per warmed seek point (at most 4 MiB, and never more than
    /// the spacing between two points).
    pub point_bytes: u64,
    /// Mirror hosts, best first. Each entry is a bare host (`host:port`) or a
    /// full URL; only the origin is used. Empty = the URL's own host.
    pub hosts: Vec<String>,
    /// Name of this session's cache file (without the `.part` suffix). The
    /// player passes `<bvid>-<cid>-<quality>`; `None` names the file after the
    /// media URL's own path, which is what [`cache_key_from_url`] returns. The
    /// file lives in `%LOCALAPPDATA%\o11a1\cache` (temporary directory when
    /// that variable is missing).
    pub cache_key: Option<String>,
    /// Duration of the media in milliseconds, when the caller knows it. With
    /// the total length this gives the exact bitrate, which sets the retained
    /// window ([`PLAYBACK_AHEAD_SECONDS`]) and the speculative rate floor
    /// ([`SPECULATIVE_RATE_FACTOR`]). `None` falls back to the rate the client
    /// actually pulls.
    pub media_duration_ms: Option<u64>,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            read_ahead_bytes: DEFAULT_READ_AHEAD_BYTES,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            connections: DEFAULT_CONNECTIONS,
            warm_points: crate::settings::default_seek_points() as usize,
            point_bytes: DEFAULT_POINT_BYTES,
            hosts: Vec::new(),
            cache_key: None,
            media_duration_ms: None,
        }
    }
}
impl PrefetchConfig {
    /// Clamp every field into its supported range. Blank host entries are
    /// dropped.
    pub fn normalized(mut self) -> Self {
        self.read_ahead_bytes = self
            .read_ahead_bytes
            .clamp(MIN_READ_AHEAD_BYTES, MAX_READ_AHEAD_BYTES);
        self.chunk_bytes = self
            .chunk_bytes
            .clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES)
            .min(self.read_ahead_bytes);
        self.connections = if self.connections == 0 {
            DEFAULT_CONNECTIONS
        } else {
            self.connections.clamp(1, MAX_CONNECTIONS)
        };
        self.warm_points = self.warm_points.min(MAX_WARM_POINTS);
        self.point_bytes = self
            .point_bytes
            .clamp(1, DEFAULT_POINT_BYTES)
            .min(self.read_ahead_bytes);
        self.hosts.retain(|host| !host.trim().is_empty());
        self
    }
}

/// What one read-ahead worker is doing right now.
///
/// `start`/`end` describe the upstream byte range it holds (or held, when it is
/// between chunks); `bytes` is what that worker itself pulled since the proxy
/// started. A worker that is between chunks keeps its last range and reports
/// `busy: false`, so the panel can show "4 connections, 2 fetching" instead of
/// guessing from the aggregate rate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerStat {
    /// 0-based index of the configured connection, stable for the proxy's life.
    pub id: u32,
    /// Label of the mirror host this worker is fetching from (`""` before its
    /// first claim).
    pub host: String,
    /// Absolute byte offset of the range it is fetching now.
    pub start: u64,
    /// Exclusive end of that range (`start + chunk`, clamped to the file
    /// length when the total is already known at claim time).
    pub end: u64,
    /// `false` while the worker waits for work (or after it stopped).
    pub busy: bool,
    /// Bytes this worker has fetched since the proxy started.
    pub bytes: u64,
    /// Throughput over the last five seconds, bytes/s.
    pub bps: u64,
    /// The health watchdog marked this worker slow (zero/low throughput).
    pub slow: bool,
}

/// Byte accounting for the profiler panel.
///
/// [`PrefetchStats::bytes_fetched`] counts bytes *taken from upstream*, not
/// bytes handed to ffmpeg; the two match within one chunk because the engine
/// fetches whole chunks and serves them in order.
///
/// [`PrefetchStats::workers`], [`PrefetchStats::downloaded`] and
/// [`PrefetchStats::point_cache`] are the per-connection / per-byte view the
/// network panel draws (live workers, progress heatmap, warm points). They are
/// rebuilt under the state lock on every publish and cloned as small vectors,
/// so a UI frame never waits on the fetch path.
#[derive(Clone, Debug, Default)]
pub struct PrefetchStats {
    pub bytes_fetched: u64,
    /// Sliding one-second average of upstream throughput.
    pub bytes_per_sec: u64,
    /// Complete media length, learned from the first real range response (or
    /// an EOF short read) instead of a dedicated probe.
    pub bytes_total: Option<u64>,
    /// Bytes buffered ahead of the read cursor (fetched but not served).
    pub read_ahead_bytes: u64,
    /// Bytes pulled from each configured host, in config order.
    pub hosts: Vec<(String, u64)>,
    /// Upstream range attempts that failed (a recovered retry still counts;
    /// this is a health counter, not a count of lost bytes).
    pub failed_ranges: u64,
    /// Seek points already warmed (or found covered) in the background task.
    pub warm_points_done: u32,
    /// Seek points the background task set out to warm, i.e. the distinct
    /// `i / count` offsets of the current file (0 until the total is known).
    pub warm_points_total: u32,
    /// Requests answered from the point cache instead of going upstream.
    pub point_hits: u64,
    /// Bytes written to the client out of the point cache.
    pub point_bytes: u64,
    /// Bytes currently held by the point cache (bounded by
    /// [`MAX_POINT_CACHE_BYTES`]).
    pub point_cache_bytes: u64,
    /// One entry per configured connection, in id order. The entry exists even
    /// before the worker's first claim (empty host, zero range).
    pub workers: Vec<WorkerStat>,
    /// Merged, sorted, non-overlapping `[start, end)` byte ranges already held:
    /// the union of the read-ahead window and the point cache. Capped at
    /// [`MAX_DOWNLOADED_RANGES`] entries, merging the smallest gaps first, so
    /// the UI always receives a compact list.
    pub downloaded: Vec<(u64, u64)>,
    /// Everything the session can serve **right now**: the session cache file
    /// plus the in-memory read-ahead window and the point cache.
    ///
    /// The 下载分布 panel draws this, not [`PrefetchStats::downloaded`]: bytes
    /// that were fetched and are being served from memory are already playable,
    /// so painting them as "not downloaded" (and, worse, as "downloading" while
    /// the worker happens to hold that range again) contradicted the picture the
    /// user was watching. `downloaded` stays the disk truth, which is what the
    /// seek path must use.
    pub available: Vec<(u64, u64)>,
    /// Same value as [`PrefetchStats::bytes_total`], under the name the panel
    /// uses for the heatmap denominator.
    pub total_bytes: Option<u64>,
    /// Workers with `busy == true` at snapshot time.
    pub active_workers: u32,
    /// Upstream range claims in flight right now (`state.inflight`). `active`
    /// counts worker slots that hold a claim; this counts the claims themselves,
    /// so `active=2 inflight=2` on a configured 16 is the honest "only two of
    /// the sixteen connections are fetching" the QA line has to expose.
    pub inflight: u64,
    /// `[start, end)` ranges currently held by the warm point cache.
    pub point_cache: Vec<(u64, u64)>,
    /// Requests answered (at least partly) from the session cache file, counted
    /// once per disk-served block.
    pub disk_hits: u64,
    /// Bytes written to the client out of the session cache file.
    pub disk_bytes: u64,
    /// Bytes the session cache file holds (present blocks, clamped to the known
    /// length). Equal to the sum of [`PrefetchStats::downloaded`] when the disk
    /// is the only source; the memory window can only add bytes the file could
    /// not persist (cache cap, no cache file).
    pub cache_bytes: u64,
    /// The session cache file is at [`CACHE_CAP_BYTES`]; nothing further is
    /// persisted (memory serving continues).
    pub cache_capped: bool,
    /// Whole-file chunks not fetched yet (the global scheduling queue).
    pub queue_chunks: u64,
    /// Bytes those chunks represent (whole-file scheduling, not the window).
    pub queue_bytes: u64,
    /// Upstream bytes by scheduling class: the client's hot window, the seek
    /// warm points, the speculative whole-file queue, and retries (a requeued
    /// or raced range). The split proves speculative work keeps its share while
    /// nothing waits, and loses it to the hot window while a body waits.
    pub bytes_hot: u64,
    pub bytes_warm: u64,
    pub bytes_rest: u64,
    pub bytes_retry: u64,
}

/// One chunk of a playlist preload (2 MiB): large enough that the CDN round
/// trip is amortised, small enough that a cancel is noticed quickly.
const PRELOAD_CHUNK_BYTES: u64 = 2 << 20;
/// Gap between two preload chunks: the preload is one sequential reader that
/// deliberately leaves the link to playback.
const PRELOAD_YIELD: Duration = Duration::from_millis(40);

/// Fetch one URL completely into its own session cache file, at background
/// priority. `stop` is checked between network chunks; `progress` is updated as
/// bytes land so a caller can show the preload.
async fn preload_one(
    engine: &Engine,
    key: &str,
    url: &str,
    stop: &AtomicBool,
    progress: &PreloadProgressShared,
) {
    let started = Instant::now();
    if let Ok(mut current) = progress.current.lock() {
        *current = key.to_string();
    }
    let mut disk = DiskCache::open(key);
    if !disk.present() {
        crate::debug_log(&format!("preload file key={key} skipped=no-file"));
        return;
    }
    let mut offset = disk.first_hole();
    let mut total = disk.len();
    // A file the previous pass already completed: the ring stays full without
    // touching the network.
    if total > 0 && offset >= total {
        set_preload_progress(key, total, total, true);
    }
    let mut fetched = 0u64;
    let mut failures = 0u32;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if total > 0 && offset >= total {
            break;
        }
        let end = offset.saturating_add(PRELOAD_CHUNK_BYTES);
        let last = if total > 0 { end.min(total - 1) } else { end - 1 };
        let range = format!("bytes={offset}-{last}");
        let request = engine
            .client
            .get(url)
            .header(RANGE, range.clone())
            .send();
        let response = match tokio::time::timeout(CHUNK_TIMEOUT, request).await {
            Ok(Ok(response)) => Some(response),
            Ok(Err(error)) => {
                crate::debug_log(&format!("preload retry key={key} err={error}"));
                None
            }
            Err(_) => {
                crate::debug_log(&format!("preload retry key={key} err=timeout"));
                None
            }
        };
        let Some(mut response) = response else {
            failures += 1;
            if failures >= 3 {
                crate::debug_log(&format!("preload file key={key} failed attempts={failures}"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            if total == 0 {
                disk.adopt_len(offset);
            }
            break;
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            crate::debug_log(&format!(
                "preload file key={key} failed status={}",
                response.status().as_u16()
            ));
            break;
        }
        if total == 0 {
            if let Some(learned) = content_range_total(response.headers()) {
                disk.adopt_len(learned);
                total = learned;
                progress
                    .bytes_total
                    .fetch_add(learned, Ordering::SeqCst);
                set_preload_progress(key, offset, total, false);
            }
        }
        let mut body = 0u64;
        while let Some(chunk) = match tokio::time::timeout(CHUNK_TIMEOUT, response.chunk()).await {
            Ok(Ok(chunk)) => chunk,
            _ => None,
        } {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            disk.write(offset, &chunk);
            offset += chunk.len() as u64;
            body += chunk.len() as u64;
            fetched += chunk.len() as u64;
            progress.bytes_done.fetch_add(chunk.len() as u64, Ordering::SeqCst);
            set_preload_progress(key, offset, total, total > 0 && offset >= total);
            if total > 0 && offset >= total {
                break;
            }
            if body >= PRELOAD_CHUNK_BYTES {
                break;
            }
        }
        if body == 0 {
            if total == 0 {
                // A short open-ended body is the only EOF hint available.
                disk.adopt_len(offset);
                total = offset;
            }
            break;
        }
        tokio::time::sleep(PRELOAD_YIELD).await;
    }
    let complete = total > 0 && offset >= total;
    if complete {
        // Flush, then the sidecar: the map may only name bytes that are already
        // in the file (this file is meant to outlive its writer, unlike a
        // session file).
        disk.flush_and_save_map();
        set_preload_progress(key, total, total, true);
    } else if offset == 0 && total == 0 {
        // Nothing ever landed: a failed resolution must not leave a ring that
        // looks pending forever.
        set_preload_progress(key, 0, 0, false);
    }
    if let Ok(mut current) = progress.current.lock() {
        current.clear();
    }
    if disk.capped {
        crate::debug_log(&format!("preload file key={key} capped bytes={offset}"));
    }
    crate::debug_log(&format!(
        "preload file key={key} bytes={fetched} have={offset} total={total} complete={} ms={}",
        u8::from(complete),
        started.elapsed().as_millis()
    ));
    drop(disk);
}

/// Handle to a running proxy. Dropping it shuts the server down.
pub struct PrefetchHandle {
    /// `http://127.0.0.1:<port><path>?<original query>`.
    pub local_url: String,
    shared: Arc<Engine>,
    shutdown: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
    /// Playlist whole-file preloads (see [`PrefetchHandle::preload`]).
    preload: Mutex<Option<PreloadJob>>,
    runtime: &'static tokio::runtime::Runtime,
}

/// One running playlist preload: a single background task plus the flag that
/// stops it. Kept together so `cancel_preload` can wait for the task to notice.
struct PreloadJob {
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
    progress: Arc<PreloadProgressShared>,
}

#[derive(Debug, Default)]
struct PreloadProgressShared {
    files_total: AtomicU64,
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    /// Cache key of the file being fetched right now, so a cancel can delete the
    /// partial file instead of leaving it behind.
    current: Mutex<String>,
}

/// One playlist-preload file as the playlist rows see it: bytes landed, the
/// media's own length once known, and whether the file is complete.
#[derive(Clone, Copy, Debug, Default)]
pub struct PreloadFileProgress {
    pub done: u64,
    pub total: u64,
    pub complete: bool,
}

/// Per-key progress of the **last** playlist preload pass.
///
/// [`PrefetchHandle::preload_progress`] is the aggregate counter; the playlist
/// panel draws one 16px ring per row, so it needs the per-file split. The map is
/// process-wide on purpose: the preload lives inside the active player's proxy,
/// while the rows are published by the main window's playlist host, which never
/// holds that handle. Entries of completed files stay until the file is removed
/// (`PreloadFileProgress::complete` keeps the ring full).
static PRELOAD_FILES: OnceLock<Mutex<HashMap<String, PreloadFileProgress>>> = OnceLock::new();

fn preload_files() -> &'static Mutex<HashMap<String, PreloadFileProgress>> {
    PRELOAD_FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_preload_progress(key: &str, done: u64, total: u64, complete: bool) {
    if let Ok(mut files) = preload_files().lock() {
        let entry = files.entry(key.to_string()).or_default();
        // The sequential reader only moves forward; a later pass that finds the
        // file already full keeps the completed state.
        if done > entry.done || complete {
            entry.done = done;
        }
        if total > 0 {
            entry.total = total;
        }
        entry.complete |= complete;
    }
}

/// Snapshot for the playlist rows: `(cache key, bytes done, bytes total,
/// complete)`, sorted by key so a diagnostic line is stable. An empty map (no
/// preload ever started) returns an empty vector.
pub fn preload_snapshot() -> Vec<(String, u64, u64, bool)> {
    let mut rows: Vec<(String, u64, u64, bool)> = preload_files()
        .lock()
        .map(|files| {
            files
                .iter()
                .map(|(key, entry)| (key.clone(), entry.done, entry.total, entry.complete))
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Drop one key from the progress map (cache file removed / preload cancelled).
fn forget_preload_progress(key: &str) {
    if let Ok(mut files) = preload_files().lock() {
        files.remove(key);
    }
}

/// One `cache removed` line in the same shape [`DiskCache::remove`] uses, so a
/// reader can grep either removal path.
fn log_cache_removed(path: &std::path::Path, bytes: u64, reason: &str) {
    crate::debug_log(&format!(
        "cache removed path={} bytes={} written=0 reason={reason}",
        path.display(),
        bytes
    ));
}

/// Delete every session cache file belonging to `bvid`/`cid` (video and audio,
/// every quality/codec variant).
///
/// This is the playlist's ownership path: a queued item that was preloaded into
/// its own `.part` has no player holding it, so removing the row (or finishing
/// its playback) must remove the bytes it reserved. The active player's own
/// session file is also matched; Windows only deletes it once the decoder has
/// closed its handle, so a held file is retried on the low-priority pool rather
/// than blocking the UI thread.
///
/// Returns the number of media files the call targeted (0 when nothing was
/// cached). The progress map is cleared at the same time, so the row's ring
/// cannot stay full after its file is gone.
pub fn remove_cached_for(bvid: &str, cid: u64, reason: &str) -> usize {
    if !disk_cache_enabled() {
        return 0;
    }
    let dir = cache_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let prefix = format!("{}-{}-", cache_key_name(bvid), cid);
    let mut stems: BTreeSet<String> = BTreeSet::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name
            .strip_suffix(".part")
            .or_else(|| name.strip_suffix(".map"))
        else {
            continue;
        };
        if stem.starts_with(&prefix) {
            stems.insert(stem.to_string());
        }
    }
    let mut removed = 0usize;
    for stem in stems {
        forget_preload_progress(&stem);
        let map = dir.join(format!("{stem}.map"));
        let part = dir.join(format!("{stem}.part"));
        let _ = std::fs::remove_file(&map);
        let bytes = std::fs::metadata(&part).map(|meta| meta.len()).unwrap_or(0);
        match std::fs::remove_file(&part) {
            Ok(()) => {
                removed += 1;
                log_cache_removed(&part, bytes, reason);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                removed += 1;
                log_cache_removed(&part, bytes, reason);
            }
            Err(_) => {
                // The decoder or the preload reader still holds the handle.
                // Retry off-thread; the same bounded 2 s the session removal
                // gives ffmpeg on a close.
                removed += 1;
                let reason = reason.to_string();
                crate::netpool::submit(
                    crate::netpool::Prio::Low,
                    Box::new(move || {
                        for _ in 0..40 {
                            match std::fs::remove_file(&part) {
                                Ok(()) => {
                                    log_cache_removed(&part, bytes, &reason);
                                    return;
                                }
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                    log_cache_removed(&part, bytes, &reason);
                                    return;
                                }
                                Err(_) => std::thread::sleep(Duration::from_millis(50)),
                            }
                        }
                        crate::debug_log(&format!(
                            "cache remove failed path={} bytes={bytes} reason=held",
                            part.display()
                        ));
                    }),
                );
            }
        }
    }
    if removed > 0 {
        crate::debug_log(&format!(
            "cache removed items={removed} bvid={bvid} cid={cid} reason={reason}"
        ));
    }
    removed
}

impl PrefetchHandle {
    /// Current counters, with `bytes_per_sec` recomputed against the live
    /// one-second window (the published copy is refreshed on chunk arrival).
    pub fn snapshot(&self) -> PrefetchStats {
        self.shared.snapshot()
    }

    /// Change the number of upstream connections while the proxy runs
    /// (clamped to `1..=32`; `0` selects the default). Only the tests drive it
    /// today; the player's A/B harness is what keeps it honest.
    #[cfg(test)]
    pub fn set_connections(&self, n: u32) {
        let count = if n == 0 {
            DEFAULT_CONNECTIONS
        } else {
            (n as usize).clamp(1, MAX_CONNECTIONS)
        };
        self.shared
            .desired_connections
            .store(count as u64, Ordering::SeqCst);
        crate::debug_log(&format!("prefetch connections={count} live"));
    }

    /// Merged, sorted `[start, end)` ranges currently held in memory: the
    /// read-ahead window plus the seek point cache.
    #[cfg(test)]
    pub fn cached_ranges(&self) -> Vec<(u64, u64)> {
        self.shared.cached_ranges()
    }

    /// Contiguous bytes available from `offset` in the merged cached ranges
    /// (0 when the offset is not cached). This is what tells the caller
    /// whether a seek can be answered without any upstream request: memory
    /// window, warm points and the session cache file all count.
    pub fn cached_bytes_at(&self, offset: u64) -> u64 {
        self.shared.cached_bytes_at(offset)
    }

    /// Path of this session's cache file, when the session really has one.
    ///
    /// The file is a sparse, exactly-sized copy of the media: every byte the
    /// proxy fetched is in it. The player hands this path to ffmpeg instead of
    /// the loopback URL for a seek that is already downloaded, which skips the
    /// HTTP round trip and the demuxer's byte-range search entirely.
    pub fn cache_path(&self) -> Option<PathBuf> {
        self.shared.cache_path()
    }

    /// Fetch ahead from `offset` **before** any client asks for it, into the
    /// bounded seek-point cache.
    ///
    /// The bytes land in the point cache rather than the read-ahead window, and
    /// that is the whole point: a point is served by `point_hit` *before* the
    /// response lock and without moving the read cursor, so a pre-warmed seek
    /// neither waits behind the body a client is still reading nor pushes that
    /// client's window away. The pre-warm is also not rate limited (unlike
    /// scheduled seek points): it is urgent work for a seek that is happening.
    ///
    /// `bytes` is clamped to the point-cache budget, and to the end of the
    /// media. `true` means the region was already covered (memory window, point
    /// cache or the session cache file) and no task was queued.
    pub fn prime_at(&self, offset: u64, bytes: u64) -> bool {
        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(total) = state.total else {
            // No length yet: the byte offset of a timestamp is meaningless
            // without it. The client's own request primes the window instead.
            drop(state);
            crate::debug_log(&format!("prefetch prime offset={offset} deferred reason=no-total"));
            return false;
        };
        if offset >= total {
            return true;
        }
        if state.points.lookup(offset).is_some()
            || chunks_cover(&state.chunks, offset)
            || self.shared.disk_covers(offset)
        {
            drop(state);
            crate::debug_log(&format!(
                "prefetch prime offset={offset} covered=1 after_ms={}",
                self.shared.started.elapsed().as_millis()
            ));
            return true;
        }
        // The client's read window owns the cursor; the automatic seek-point
        // plan (if this session has one) still comes first.
        if !state.warm_plan_started {
            build_warm_plan(
                &mut state,
                self.shared.config.chunk_bytes,
                self.shared.config.point_bytes,
                self.shared.config.warm_points,
                total,
            );
        }
        let chunk = self.shared.config.chunk_bytes.max(1);
        let start = chunk_start(offset, chunk);
        let per_point = point_chunk_bytes(total, 1, DEFAULT_POINT_BYTES).max(1);
        let span = bytes.clamp(1, per_point);
        let end = total.min(start.saturating_add(span));
        if state.warm_plan.iter().any(|task| task.start == start) {
            drop(state);
            return false;
        }
        state.warm_plan.push(WarmTask {
            point: start,
            start,
            end,
            done: false,
            claimed: false,
            urgent: true,
        });
        state.warm_plan_started = true;
        state.warm_points_total = state.warm_plan.len().min(MAX_WARM_POINTS) as u32;
        drop(state);
        crate::debug_log(&format!(
            "prefetch prime offset={offset} start={start} end={end} bytes={} after_ms={}",
            end - start,
            self.shared.started.elapsed().as_millis()
        ));
        false
    }

    /// Path of the cache file **when it holds at least `need` contiguous bytes
    /// at `offset`** — the honest test for "can ffmpeg play this locally
    /// without hitting a hole". `None` keeps the proxy path.
    pub fn cache_path_for(&self, offset: u64, need: u64) -> Option<PathBuf> {
        self.shared.cache_path_for(offset, need)
    }

    /// Merged, sorted `[start, end)` ranges the session cache file holds — the
    /// disk map, *without* the memory window. This is the map the 下载分布
    /// panel draws and the one a seek must not change.
    pub fn disk_ranges(&self) -> Vec<(u64, u64)> {
        self.shared.disk_ranges()
    }

    /// `(contiguous bytes at `offset` on disk, range count, total on-disk bytes)`.
    ///
    /// The seek cache decision reads exactly this: a target counts as cached
    /// only when the disk map really holds the bytes, never because a warm point
    /// is *near* the offset or because the read-ahead window happens to be big.
    pub fn disk_cover(&self, offset: u64) -> (u64, usize, u64) {
        self.shared.disk_cover(offset)
    }


    /// Preload the tracks of a playlist, in playlist order, at **background**
    /// priority.
    ///
    /// Each URL is fetched completely into its own session cache file, one
    /// connection at a time, in the given order, so the film after the current
    /// one is already on disk when the player opens it (the first frame then
    /// comes from the local file, and a seek anywhere is instant). Nothing here
    /// touches the proxy's read-ahead window or its worker pool: the preload is
    /// one sequential range reader that yields between chunks, and it stops the
    /// moment [`PrefetchHandle::cancel_preload`] is called, the handle is
    /// dropped, or the URL's cache file reaches [`CACHE_CAP_BYTES`].
    ///
    /// Same as [`PrefetchHandle::preload`], with an explicit cache key per URL
    /// (`(key, url)`), so a preload can fill the file a later player will open:
    /// the player passes `<bvid>-<cid>-<quality>`.
    pub fn preload_keys(&self, order: &[(String, String)]) -> usize {
        self.cancel_preload();
        let live = self.shared.cache_key.clone();
        let mut queue: Vec<(String, String)> = Vec::with_capacity(order.len());
        for (key, url) in order {
            if url.trim().is_empty() {
                continue;
            }
            let key = cache_key_name(key);
            if key == live {
                crate::debug_log(&format!("preload skip key={key} reason=live-session"));
                continue;
            }
            queue.push((key, url.clone()));
        }
        if queue.is_empty() {
            return 0;
        }
        let progress = Arc::new(PreloadProgressShared::default());
        progress
            .files_total
            .store(queue.len() as u64, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let shared = self.shared.clone();
        let task_stop = stop.clone();
        let task_progress = progress.clone();
        crate::debug_log(&format!(
            "preload start files={} keys={}",
            queue.len(),
            queue
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
        let task = self.runtime.spawn(async move {
            for (key, url) in queue {
                if task_stop.load(Ordering::SeqCst) {
                    break;
                }
                preload_one(&shared, &key, &url, &task_stop, &task_progress).await;
                if task_stop.load(Ordering::SeqCst) {
                    break;
                }
                task_progress.files_done.fetch_add(1, Ordering::SeqCst);
            }
            crate::debug_log(&format!(
                "preload done files={}/{} bytes={} cancelled={}",
                task_progress.files_done.load(Ordering::SeqCst),
                task_progress.files_total.load(Ordering::SeqCst),
                task_progress.bytes_done.load(Ordering::SeqCst),
                u8::from(task_stop.load(Ordering::SeqCst)),
            ));
        });
        let mut slot = match self.preload.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(PreloadJob {
            stop,
            task,
            progress,
        });
        order.len()
    }

    /// Stop the playlist preload (no-op when none is running). Returns whether
    /// one was running. The task is signalled, not awaited: it holds the file
    /// for at most one chunk (`PRELOAD_CHUNK_BYTES`) and then removes it.
    pub fn cancel_preload(&self) -> bool {
        let job = {
            let mut slot = match self.preload.lock() {
                Ok(slot) => slot,
                Err(poisoned) => poisoned.into_inner(),
            };
            slot.take()
        };
        let Some(job) = job else {
            return false;
        };
        job.stop.store(true, Ordering::SeqCst);
        job.task.abort();
        // A cancelled preload leaves a partial file: delete it, so the cache
        // directory never grows unfinished entries nobody will resume.
        let partial = match job.progress.current.lock() {
            Ok(key) => key.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if !partial.is_empty() {
            let dir = cache_dir();
            let name = cache_key_name(&partial);
            let _ = std::fs::remove_file(dir.join(format!("{name}.part")));
            let _ = std::fs::remove_file(dir.join(format!("{name}.map")));
            forget_preload_progress(&name);
            crate::debug_log(&format!("preload cancel removed key={name}"));
        }
        crate::debug_log(&format!(
            "preload cancel files_done={} bytes={}",
            job.progress.files_done.load(Ordering::SeqCst),
            job.progress.bytes_done.load(Ordering::SeqCst)
        ));
        true
    }

    /// `(id, host, slow, bytes_per_sec)` per live worker, in id order.
    #[cfg(test)]
    pub fn worker_health(&self) -> Vec<(u32, String, bool, u64)> {
        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        let live = self.shared.desired_connections();
        let mut health = Vec::with_capacity(live);
        for (id, slot) in state.workers.iter_mut().take(live).enumerate() {
            slot.bps = window_rate(&mut slot.samples, now, HEALTH_WINDOW);
            health.push((id as u32, slot.host.clone(), slot.slow, slot.bps));
        }
        health
    }

    /// Hold (`true`) or release the proxy's **background** work.
    ///
    /// Held, the workers fetch only the client's hot window: seek points and
    /// the speculative whole-file parts wait. Playback itself — the read-ahead
    /// window and any points that already landed — is untouched, so this can
    /// never stall the picture. The
    /// player holds it while the first frames are still arriving and during a
    /// stall/buffering episode, then releases it once playback has been healthy
    /// long enough. The proxy starts un-held, so standalone users of
    /// [`spawn`] (the live QA tests) keep the old schedule.
    pub fn set_background_paused(&self, paused: bool) {
        self.shared
            .background_paused
            .store(paused, Ordering::SeqCst);
    }

    /// Stop the server and every fetch task, then wait (bounded) until the
    /// listener is closed. Safe to call more than once.
    ///
    /// The session cache file is deleted here (player close, new video, quality
    /// switch): no `.part` file outlives its session.
    ///
    /// Must be called from a synchronous thread; inside a Tokio runtime this
    /// falls back to aborting the server task because nested `block_on`
    /// panics.
    pub fn shutdown(&mut self) {
        self.cancel_preload();
        self.shutdown.store(true, Ordering::SeqCst);
        // Give the worker ids back to the pool first: from here on no thread
        // picks a range for this session, even if the server task is aborted
        // rather than joined below.
        unbind_slots(&self.shared);
        let Some(mut task) = self.server.take() else {
            self.remove_cache();
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            task.abort();
            self.remove_cache();
            return;
        }
        let _ = self.runtime.block_on(async {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = (&mut task).await;
            }
        });
        self.remove_cache();
    }

    /// Close and delete the session cache file, logging one `cache removed`
    /// line. Idempotent: a second call finds no file.
    ///
    /// The writer is drained and joined **before** the file is touched: every
    /// queued block lands, the handle closes, and only then the file (and its
    /// map) is deleted, so no late write can resurrect or truncate it.
    fn remove_cache(&mut self) {
        let writer = match self.shared.writer.lock() {
            Ok(mut writer) => writer.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(writer);
        match self.shared.disk.lock() {
            Ok(mut disk) => disk.remove(),
            Err(poisoned) => poisoned.into_inner().remove(),
        }
    }
}

impl Drop for PrefetchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Bind a loopback proxy for `url` and start fetching it.
pub fn spawn(url: &str, config: PrefetchConfig) -> Result<PrefetchHandle> {
    let target = Target::parse(url)?;
    let config = config.normalized();

    let host_specs = if config.hosts.is_empty() {
        vec![HostSpec {
            label: target.host_label.clone(),
            origin: target.origin.clone(),
        }]
    } else {
        config
            .hosts
            .iter()
            .map(|entry| HostSpec::parse(entry, &target.scheme))
            .collect::<Result<Vec<_>>>()?
    };
    if host_specs.is_empty() {
        bail!("没有可用的上游主机");
    }

    let runtime = crate::bilibili::shared_runtime()
        .ok_or_else(|| anyhow!("网络运行时不可用，无法启动预取代理"))?;

    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_static("https://www.bilibili.com/"));
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    // Byte ranges must refer to the bytes on the wire, never to a compressed
    // representation.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let client = reqwest::Client::builder()
        .user_agent(crate::bilibili::UA)
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(CHUNK_TIMEOUT)
        .gzip(false)
        .brotli(false)
        .build()
        .context("无法创建预取客户端")?;

    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("无法绑定本地回环端口")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;

    let stats = Arc::new(Mutex::new(PrefetchStats {
        hosts: host_specs
            .iter()
            .map(|host| (host.label.clone(), 0))
            .collect(),
        ..PrefetchStats::default()
    }));
    // Startup gate: a session begins with a smaller window and fewer fetchers,
    // so the first frames are not queued behind 32 MiB of speculative ranges.
    let gate = startup_gate_enabled();
    let startup_read_ahead = if gate {
        config.read_ahead_bytes.min(STARTUP_READ_AHEAD_BYTES)
    } else {
        u64::MAX
    };
    let cache_key = config
        .cache_key
        .clone()
        .unwrap_or_else(|| cache_key_from_url(url));
    // The single writer, created before any fetch task exists: from here on no
    // worker touches the file.
    let disk = Arc::new(Mutex::new(DiskCache::open(&cache_key)));
    let writer = DiskWriter::spawn(disk.clone());
    let shared = Arc::new(Engine {
        config: config.clone(),
        target: target.clone(),
        client,
        cache_key: cache_key_name(&cache_key),
        disk,
        writer: Mutex::new(writer),
        state: Mutex::new(State {
            base: 0,
            cursor: 0,
            generation: 0,
            chunks: BTreeMap::new(),
            inflight: BTreeSet::new(),
            dead: BTreeSet::new(),
            done: BTreeSet::new(),
            parts: Vec::new(),
            parts_built: false,
            requeued: BTreeSet::new(),
            short_bodies: BTreeMap::new(),
            warm_plan: Vec::new(),
            warm_plan_started: false,
            serve_samples: VecDeque::new(),
            races: VecDeque::new(),
            urgent: VecDeque::new(),
            race_tokens: BTreeMap::new(),
            total: None,
            fatal: None,
            content_type: None,
            hosts: host_specs
                .into_iter()
                .map(|spec| Host {
                    label: spec.label,
                    origin: spec.origin,
                    failures: 0,
                    failed: false,
                    bytes: 0,
                    degraded_until: None,
                })
                .collect(),
            next_host: 0,
            samples: VecDeque::new(),
            bytes_fetched: 0,
            failed_ranges: 0,
            served: 0,
            serve_bps: 0,
            first_serve: None,
            // One chunk so the first speculative claim can start immediately;
            // the bucket refills it to one chunk per live connection.
            spec_tokens: DEFAULT_CHUNK_BYTES,
            spec_at: Instant::now(),
            spec_bytes: 0,
            read_ahead: 0,
            points: PointCache::default(),
            warm_points_total: 0,
            warm_points_done: 0,
            warm_chunk: 0,
            warm_bytes: 0,
            hot_bytes: 0,
            warm_fetch_bytes: 0,
            rest_bytes: 0,
            retry_bytes: 0,
            point_hits: 0,
            point_served: 0,
            disk_hits: 0,
            disk_served: 0,
            workers: (0..MAX_CONNECTIONS)
                .map(|_| WorkerSlot::default())
                .collect(),
        }),
        stats: stats.clone(),
        serving: AtomicBool::new(false),
        serve_epoch: AtomicU64::new(0),
        read_ahead_cap: AtomicU64::new(startup_read_ahead),
        background_paused: AtomicBool::new(false),
        // Gated sessions start at the measured knee (two streams); the
        // connection governor owns the count from the first tick and may raise
        // it all the way to the configured value. Background work is never
        // capped below the live count — the "two workers for twelve seconds"
        // rule was half of why a sixteen-connection setting showed one or two
        // busy bars.
        desired_connections: AtomicU64::new(if gate {
            config.connections.min(STARTUP_CONNECTIONS) as u64
        } else {
            config.connections as u64
        }),
        configured_connections: config.connections as u64,
        background_workers: AtomicU64::new(0),
        serve_waits: AtomicU64::new(0),
        serve_wait_slow: AtomicU64::new(0),
        worker_cancel: (0..MAX_CONNECTIONS).map(|_| AtomicU64::new(0)).collect(),
        media_bps: AtomicU64::new(0),
        serve_wait_pos: AtomicU64::new(u64::MAX),
        serve_wait_since_ms: AtomicU64::new(0),
        serve_wait_escalations: AtomicU64::new(0),
        started: Instant::now(),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = runtime.spawn(run_server(listener, shared.clone(), shutdown.clone()));

    let local_url = format!(
        "http://127.0.0.1:{}{}{}",
        addr.port(),
        target.path,
        if target.query.is_empty() {
            String::new()
        } else {
            format!("?{}", target.query)
        }
    );
    crate::debug_log(&format!(
        "prefetch spawn port={} read_ahead={} chunk={} connections={} warm_points={} point_bytes={} hosts={} startup=first-chunk gate={} read_ahead_start={} connections_start={}",
        addr.port(),
        config.read_ahead_bytes,
        config.chunk_bytes,
        config.connections,
        config.warm_points,
        config.point_bytes,
        shared
            .state
            .lock()
            .map(|state| state
                .hosts
                .iter()
                .map(|host| host.label.as_str())
                .collect::<Vec<_>>()
                .join(","))
            .unwrap_or_default(),
        if gate { "on" } else { "off" },
        shared.read_ahead_window(),
        shared.desired_connections.load(Ordering::SeqCst),
    ));

    Ok(PrefetchHandle {
        local_url,
        shared,
        shutdown,
        server: Some(server),
        preload: Mutex::new(None),
        runtime,
    })
}

// ---------------------------------------------------------------------------
// URL / range helpers
// ---------------------------------------------------------------------------

/// The configured media URL, split into the parts the proxy needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    /// Full original URL, used verbatim for `/v1/resource` paths.
    url: String,
    scheme: String,
    origin: String,
    host_label: String,
    path: String,
    query: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).with_context(|| format!("无法解析媒体地址：{url}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("仅支持 http/https 媒体地址");
        }
        let host = parsed
            .host_str()
            .context("媒体地址缺少主机名")?
            .to_string();
        let port = parsed
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        let origin = format!("{}://{}{}", parsed.scheme(), host, port);
        let host_label = format!("{host}{port}");
        let path = if parsed.path().is_empty() {
            "/".to_string()
        } else {
            parsed.path().to_string()
        };
        let query = parsed.query().unwrap_or("").to_string();
        Ok(Self {
            url: url.to_string(),
            scheme: parsed.scheme().to_string(),
            origin,
            host_label,
            path,
            query,
        })
    }
}

/// One configured upstream host: display label plus the origin used in URLs.
#[derive(Clone, Debug)]
struct HostSpec {
    label: String,
    origin: String,
}

impl HostSpec {
    /// Accepts `host:port` or a full URL (the latter only contributes its
    /// origin, so a playurl backup URL can be passed directly).
    fn parse(entry: &str, scheme: &str) -> Result<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            bail!("主机名为空");
        }
        if entry.contains("://") {
            let parsed = reqwest::Url::parse(entry).with_context(|| format!("无法解析主机：{entry}"))?;
            let host = parsed.host_str().context("主机缺少主机名")?;
            let port = parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            Ok(Self {
                label: format!("{host}{port}"),
                origin: format!("{}://{}{}", parsed.scheme(), host, port),
            })
        } else {
            let origin = format!("{scheme}://{entry}");
            let parsed = reqwest::Url::parse(&origin).with_context(|| format!("无法解析主机：{entry}"))?;
            let host = parsed.host_str().context("主机缺少主机名")?;
            let port = parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            Ok(Self {
                label: format!("{host}{port}"),
                origin,
            })
        }
    }
}

/// Upstream URL for one mirror: the configured path+query are forwarded
/// unchanged. `/v1/resource` URLs are never rewritten (the signature is bound
/// to their own host).
fn forward_url(target: &Target, host_origin: &str) -> String {
    if target.path.starts_with("/v1/resource") {
        return target.url.clone();
    }
    if target.query.is_empty() {
        format!("{host_origin}{}", target.path)
    } else {
        format!("{host_origin}{}?{}", target.path, target.query)
    }
}

/// A parsed `Range` request header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeSpec {
    /// `bytes=N-`
    From(u64),
    /// `bytes=N-M`
    Bounded(u64, u64),
    /// `bytes=-N` (last N bytes)
    Suffix(u64),
}

/// Parse a single-range `Range` header. Multi-range requests are rejected.
fn parse_range_header(value: &str) -> Option<RangeSpec> {
    let rest = value.trim().strip_prefix("bytes=")?;
    if rest.contains(',') {
        return None;
    }
    let (start, end) = rest.split_once('-')?;
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() {
        let count = end.parse::<u64>().ok()?;
        if count == 0 {
            return None;
        }
        return Some(RangeSpec::Suffix(count));
    }
    let start = start.parse::<u64>().ok()?;
    if end.is_empty() {
        return Some(RangeSpec::From(start));
    }
    let end = end.parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    Some(RangeSpec::Bounded(start, end))
}

/// `bytes 0-0/12345` (or `bytes 0-0/12345` with parameters) -> `12345`.
fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    content_range_total_str(value)
}

fn content_range_total_str(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// A range resolved against the (possibly unknown) total length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolvedRange {
    start: u64,
    /// Inclusive end; `None` means "until EOF".
    end: Option<u64>,
}

/// Resolve a request range. `None` = unsatisfiable (`416`).
fn resolve_range(spec: Option<RangeSpec>, total: Option<u64>) -> Option<ResolvedRange> {
    match spec {
        None => {
            if total == Some(0) {
                return None;
            }
            Some(ResolvedRange {
                start: 0,
                end: total.map(|total| total - 1),
            })
        }
        Some(RangeSpec::From(start)) => match total {
            Some(total) if start >= total => None,
            Some(total) => Some(ResolvedRange {
                start,
                end: Some(total - 1),
            }),
            None => Some(ResolvedRange { start, end: None }),
        },
        Some(RangeSpec::Bounded(start, end)) => match total {
            Some(total) if start >= total => None,
            Some(total) => {
                let end = end.min(total - 1);
                if end < start {
                    None
                } else {
                    Some(ResolvedRange {
                        start,
                        end: Some(end),
                    })
                }
            }
            None => {
                if end < start {
                    None
                } else {
                    Some(ResolvedRange {
                        start,
                        end: Some(end),
                    })
                }
            }
        },
        Some(RangeSpec::Suffix(count)) => {
            let total = total?;
            if count == 0 || total == 0 {
                return None;
            }
            Some(ResolvedRange {
                start: total.saturating_sub(count),
                end: Some(total - 1),
            })
        }
    }
}

/// Build the `Content-Range` value of a `206`. When the total is unknown the
/// complete-length is `*` and an open-ended range is rendered as `N-*`.
fn content_range_header(range: ResolvedRange, total: Option<u64>) -> String {
    match (range.end, total) {
        (Some(end), Some(total)) => format!("bytes {}-{end}/{total}", range.start),
        (Some(end), None) => format!("bytes {}-{end}/*", range.start),
        (None, _) => format!("bytes {}-*/*", range.start),
    }
}

/// `BILI_PREFETCH_LOG=1` enables the one-line-per-response trace; the sink is
/// still `BILI_DEBUG_LOG`, like every other diagnostic line in this crate.
fn serve_log_enabled() -> bool {
    std::env::var("BILI_PREFETCH_LOG")
        .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// The request range as it arrived, for the serve trace.
fn range_request_label(spec: Option<RangeSpec>) -> String {
    match spec {
        None => "0-".to_string(),
        Some(RangeSpec::From(start)) => format!("{start}-"),
        Some(RangeSpec::Bounded(start, end)) => format!("{start}-{end}"),
        Some(RangeSpec::Suffix(count)) => format!("-{count}"),
    }
}

/// One `serve req=… -> …` line per answered range, so a `Content-Range` can be
/// compared with the bytes actually sent without a packet capture.
fn serve_trace(
    spec: Option<RangeSpec>,
    range: ResolvedRange,
    total: Option<u64>,
    source: &str,
    request_started: Instant,
) {
    if !serve_log_enabled() {
        return;
    }
    let end = range
        .end
        .map_or_else(|| "*".to_string(), |end| end.to_string());
    let total = total.map_or_else(|| "*".to_string(), |total| total.to_string());
    crate::debug_log(&format!(
        "prefetch serve req={} -> {}-{end}/{total} src={source} ms={}",
        range_request_label(spec),
        range.start,
        request_started.elapsed().as_millis()
    ));
}

/// Metadata carried by one real range response. `total` is the complete media
/// length (`None` when the server did not state it); `content_type` is the
/// upstream MIME type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Learned {
    total: Option<u64>,
    content_type: Option<String>,
}

fn header_u64(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn header_text(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// Learn the media metadata from one real range response, so no dedicated
/// probe is needed. A `206` states the complete length in `Content-Range`; a
/// `200` (a server that ignored `Range`) can only answer offset 0 and states
/// it in `Content-Length`. Anything else teaches nothing.
fn learn_metadata(status: StatusCode, start: u64, headers: &HeaderMap) -> Option<Learned> {
    if !status.is_success() {
        return None;
    }
    let total = if status == StatusCode::PARTIAL_CONTENT {
        content_range_total(headers)
    } else if start == 0 {
        header_u64(headers, CONTENT_LENGTH)
    } else {
        return None;
    };
    Some(Learned {
        total,
        content_type: header_text(headers, CONTENT_TYPE),
    })
}

/// Whether resolving a client range needs the complete length up front. Only a
/// suffix range (`bytes=-N`) does; every other form is anchored to the start
/// the client named and can stream before the total is known.
fn needs_total(spec: Option<RangeSpec>) -> bool {
    matches!(spec, Some(RangeSpec::Suffix(_)))
}

/// Whether the first client request can answer yet: as soon as the total is
/// known (the normal case: the headers of a real range response) or the bytes
/// at the requested offset are buffered. A suffix range additionally needs the
/// total, because it names no start.
fn metadata_ready(spec: Option<RangeSpec>, total: Option<u64>, buffered_at_start: bool) -> bool {
    if needs_total(spec) {
        total.is_some()
    } else {
        total.is_some() || buffered_at_start
    }
}

// ---------------------------------------------------------------------------
// Window arithmetic (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Whether the chunk map already holds the byte at `pos`.
fn chunks_cover(chunks: &BTreeMap<u64, Vec<u8>>, pos: u64) -> bool {
    chunks
        .range(..=pos)
        .next_back()
        .is_some_and(|(key, data)| key.saturating_add(data.len() as u64) > pos)
}

/// The next chunk offset to fetch for the window `[cursor, cursor+read_ahead)`.
///
/// Chunk starts are aligned to `base + k*chunk`. A candidate is skipped when a
/// buffered chunk covers it, when a fetch for it is in flight, or when it is
/// known dead. Returns `None` when the window is full or the file ended.
#[cfg(test)] // legacy window arithmetic kept for the range-selection tests
fn next_chunk_offset(
    base: u64,
    cursor: u64,
    chunk: u64,
    read_ahead: u64,
    total: Option<u64>,
    chunks: &BTreeMap<u64, Vec<u8>>,
    inflight: &BTreeSet<(u64, u64)>,
    dead: &BTreeSet<(u64, u64)>,
    generation: u64,
) -> Option<u64> {
    if chunk == 0 {
        return None;
    }
    let window_end = total.map_or_else(
        || cursor.saturating_add(read_ahead),
        |total| total.min(cursor.saturating_add(read_ahead)),
    );
    let mut index = if cursor <= base {
        0
    } else {
        (cursor - base).div_ceil(chunk)
    };
    loop {
        let start = base.saturating_add(index.saturating_mul(chunk));
        if start >= window_end {
            return None;
        }
        let covered = chunks_cover(chunks, start);
        let busy = inflight.contains(&(generation, start));
        let dead_here = dead.contains(&(generation, start));
        if !covered && !busy && !dead_here {
            return Some(start);
        }
        index += 1;
    }
}

/// Bytes buffered beyond the cursor. Every stored chunk starts at or after the
/// cursor by construction.
#[cfg(test)] // unit-test helper; the runtime path never calls it
fn buffered_bytes(chunks: &BTreeMap<u64, Vec<u8>>) -> u64 {
    chunks.values().map(|data| data.len() as u64).sum()
}

/// Merge arbitrary `[start, end)` ranges into a sorted, coalesced list of at
/// most `max_ranges` entries.
///
/// Adjacent (`start == previous end`) and overlapping ranges are one range;
/// empty and inverted ranges are dropped. When the result still has more than
/// `max_ranges` entries, the pair across the **smallest gap** is merged first
/// and that repeats, so the compact list loses the least information: a list of
/// 512 aligned 256 KiB chunks collapses to one entry, and a fragmented window
/// keeps its largest holes visible instead of dropping ranges wholesale. Equal
/// gaps are bridged left to right, so the result is deterministic.
fn merge_ranges(mut ranges: Vec<(u64, u64)>, max_ranges: usize) -> Vec<(u64, u64)> {
    ranges.retain(|(start, end)| end > start);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => {
                if end > last.1 {
                    last.1 = end;
                }
            }
            _ => merged.push((start, end)),
        }
    }
    let cap = max_ranges.max(1);
    while merged.len() > cap {
        let mut best = 1usize;
        let mut best_gap = u64::MAX;
        for index in 1..merged.len() {
            let gap = merged[index].0.saturating_sub(merged[index - 1].1);
            if gap < best_gap {
                best_gap = gap;
                best = index;
            }
        }
        let end = merged[best].1;
        if end > merged[best - 1].1 {
            merged[best - 1].1 = end;
        }
        merged.remove(best);
    }
    merged
}

/// The `[start, end)` ranges of the point cache, in offset order.
fn point_ranges(points: &PointCache) -> Vec<(u64, u64)> {
    points
        .entries
        .iter()
        .filter(|(_, data)| !data.is_empty())
        .map(|(start, data)| (*start, start.saturating_add(data.len() as u64)))
        .collect()
}

// ---------------------------------------------------------------------------
// Disk-backed session cache
// ---------------------------------------------------------------------------

/// Directory of the session cache files: `%LOCALAPPDATA%\o11a1\cache`, or the
/// temporary directory when the variable is missing (a stripped environment).
fn cache_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("o11a1").join("cache"),
        _ => std::env::temp_dir().join("o11a1-cache"),
    }
}

/// `BILI_DISK_CACHE=0` (or `false`/`no`/`off`) turns the session cache file off
/// completely: the session runs memory-only, so the owner can A/B the disk path
/// in one launch without rebuilding. Anything else (including unset) keeps it
/// on; a blank value is "on".
fn disk_cache_enabled() -> bool {
    match std::env::var("BILI_DISK_CACHE") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Replace everything that is not safe in a file name, so a cache key can be a
/// URL path or `<bvid>-<cid>-<quality>`.
fn cache_key_name(key: &str) -> String {
    let mut name: String = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_string();
    while name.contains("--") {
        name = name.replace("--", "-");
    }
    if name.is_empty() {
        name.push_str("media");
    }
    name.truncate(96);
    name
}

/// Cache key derived from a media URL, for callers that have no `<bvid>-<cid>-
/// <quality>` triple (see [`PrefetchHandle::preload`]).
///
/// Bilibili DASH paths look like
/// `/upgcxcode/17/88/<cid>/<cid>-1-<stream>.m4s`, so the cid (the first long
/// numeric directory) plus the stream id make a stable, human-readable key that
/// does not depend on the expiring query string.
pub fn cache_key_from_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let cid = segments
        .iter()
        .rev()
        .skip(1)
        .find(|part| part.len() >= 6 && part.chars().all(|ch| ch.is_ascii_digit()))
        .copied()
        .unwrap_or("media");
    let stream = segments
        .last()
        .copied()
        .unwrap_or("media")
        .trim_end_matches(".m4s")
        .trim_end_matches(".mp4");
    cache_key_name(&format!("{cid}-{stream}"))
}

/// Delete session cache files left behind by a process that was killed instead
/// of closed (a crash, a debugger stop, `taskkill`): a `.part` **without** its
/// `.map` sidecar is an unfinished *session* file, never a preloaded one, and a
/// killed process cannot run its own `cache removed`.
///
/// Only files older than [`ORPHAN_AGE`] are touched, so a second player that is
/// starting up right now cannot have its file swept.
fn sweep_orphans(dir: &std::path::Path) {
    const ORPHAN_AGE: Duration = Duration::from_secs(600);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut removed = 0u32;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("part") {
            continue;
        }
        if path.with_extension("map").exists() {
            // A preloaded file: keep it, it is meant to outlive its writer.
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= ORPHAN_AGE);
        if old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
            bytes = bytes.saturating_add(meta.len());
        }
    }
    if removed > 0 {
        crate::debug_log(&format!(
            "cache swept orphans={removed} allocated_bytes={bytes}"
        ));
    }
}

/// One session's sparse cache file plus the exact list of ranges that are
/// really in it.
///
/// Presence is a merged `BTreeMap<start, end>` of written ranges: a write of
/// `[from, to)` is inserted and coalesced with its neighbours, so the map is
/// always the exact byte set the file holds. That is what the 下载分布 panel
/// draws, what a seek hit test consults, and what the local-file decode trusts —
/// a range in this map is a range that can be read without touching the network.
///
/// An earlier bitmap of 256 KiB blocks looked cheaper, but network reads are not
/// block-aligned: a unit that straddled two reads was never marked, so a fully
/// downloaded file could report an empty map (measured on an 8 MiB file). The
/// interval map has no such edge, and for a 2 GiB file it stays at the number of
/// holes — a handful — because consecutive ranges coalesce as they fill.
struct DiskCache {
    /// `<dir>/<key>.part`; kept for the removal log and for the reader.
    path: PathBuf,
    /// `None` when the file could not be created, when the environment turned
    /// disk caching off: the session is memory-only.
    ///
    /// Shared with the writer thread through an `Arc`: Windows positioned I/O
    /// (`seek_read`/`seek_write`) does not use the handle's file pointer, so one
    /// handle serves both the reader and the single writer.
    file: Option<Arc<File>>,
    /// Complete media length once known (`set_len`), else the highest byte
    /// written so far.
    len: u64,
    /// Present ranges, merged, keyed by start.
    present: BTreeMap<u64, u64>,
    /// The file reached [`CACHE_CAP_BYTES`]; nothing further is persisted.
    capped: bool,
    cap_logged: bool,
    /// Bytes the file itself has taken (diagnostics, `cache removed` line).
    written: u64,
    /// `cache key`, for the log lines.
    label: String,
}

impl DiskCache {
    /// Create (or truncate) the cache file for `label`.
    fn open(label: &str) -> Self {
        Self::open_at(cache_dir(), label)
    }

    /// [`DiskCache::open`] in an explicit directory (the tests).
    fn open_at(dir: PathBuf, label: &str) -> Self {
        let name = cache_key_name(label);
        if !disk_cache_enabled() {
            // `BILI_DISK_CACHE=0`: no directory, no file, no sweep. Nothing in
            // the session is ever persisted, so the owner can A/B the disk path
            // in one launch.
            crate::debug_log(&format!("cache open key={name} path=- mode=memory reason=env"));
            return Self::memory_only(name);
        }
        let _ = std::fs::create_dir_all(&dir);
        sweep_orphans(&dir);
        let path = dir.join(format!("{name}.part"));
        // A preloaded file (and its map) is reused instead of truncated: that
        // is the whole point of `PrefetchHandle::preload`.
        let reuse = path.exists() && dir.join(format!("{name}.map")).exists();
        let created = if reuse {
            File::options().read(true).write(true).open(&path)
        } else {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
        };
        let (file, error) = match created {
            Ok(file) => (Some(Arc::new(file)), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let mut cache = Self {
            path,
            file,
            len: 0,
            present: BTreeMap::new(),
            capped: false,
            cap_logged: false,
            written: 0,
            label: name.clone(),
        };
        let reused = reuse && cache.load_map();
        if !reused && reuse {
            // A stale map (or an unreadable one) cannot describe the file:
            // start over instead of trusting it.
            let _ = std::fs::remove_file(&cache.path);
            cache.file = None;
            if let Ok(file) = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&cache.path)
            {
                cache.file = Some(Arc::new(file));
            }
        }
        let log = match &error {
            None => format!(
                "cache open key={name} path={} mode=disk reused={}",
                cache.path.display(),
                u8::from(reused)
            ),
            Some(error) => format!(
                "cache open key={name} path={} mode=memory err={error}",
                cache.path.display()
            ),
        };
        crate::debug_log(&log);
        cache
    }

    /// A cache with no file: memory-only by configuration, or the fallback when
    /// the environment has no writable directory.
    fn memory_only(label: String) -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            len: 0,
            present: BTreeMap::new(),
            capped: false,
            cap_logged: false,
            written: 0,
            label,
        }
    }

    /// A cache that never touches the disk (unit tests, and the fallback when
    /// the environment has no writable directory).
    #[cfg(test)]
    fn disabled() -> Self {
        Self::memory_only("test".to_string())
    }

    fn present(&self) -> bool {
        self.file.is_some()
    }

    /// Byte length of the file (the complete media length once known).
    fn len(&self) -> u64 {
        self.len
    }

    /// Adopt the complete media length learned from an upstream response:
    /// `set_len(total)` makes the file a sparse file of exactly the right size,
    /// which is what lets ffmpeg read it as media.
    fn ensure_len(&mut self, file: Option<&File>, total: u64) {
        if total == 0 || self.len == total {
            return;
        }
        self.len = total;
        if let Some(file) = file {
            let _ = file.set_len(total.min(CACHE_CAP_BYTES));
        }
        // Nothing outside the media exists any more.
        self.present.retain(|_, end| {
            *end = (*end).min(total);
            *end > 0
        });
    }

    /// [`DiskCache::ensure_len`] through this cache's own handle: the map and
    /// the file are updated together (preload path and memory-only engines).
    fn adopt_len(&mut self, total: u64) {
        let file = self.file.clone();
        self.ensure_len(file.as_deref(), total);
    }

    /// Persist one fetched range through this cache's own handle (the preload
    /// task and the unit tests; the engine's fetch workers go through
    /// [`DiskWriter`] instead).
    fn write(&mut self, offset: u64, data: &[u8]) {
        let Some(file) = self.file.clone() else {
            return;
        };
        self.write_with(&file, offset, data);
    }

    /// Persist one fetched range through `file` and record its presence.
    ///
    /// The caller holds the unique right to write this range (the writer thread
    /// is the only writer in an engine), so `present` is only extended after the
    /// bytes really landed: a range in the map is a range that can be read.
    fn write_with(&mut self, file: &File, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let end = offset.saturating_add(data.len() as u64);
        if end > CACHE_CAP_BYTES {
            if !self.cap_logged {
                self.cap_logged = true;
                crate::debug_log(&format!(
                    "cache cap reached key={} cap={} at={end}",
                    self.label, CACHE_CAP_BYTES
                ));
            }
            self.capped = true;
            return;
        }
        if self.len == 0 {
            // No `Content-Range` ever stated the length: grow to fit so the
            // first bytes are still usable, and let `ensure_len` correct it.
            self.len = end;
        }
        if offset >= self.len {
            return;
        }
        let to = end.min(self.len);
        let bytes = &data[..(to - offset) as usize];
        let mut done = 0usize;
        while done < bytes.len() {
            match file.seek_write(&bytes[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(written) => done += written,
                Err(_) => break,
            }
        }
        self.written = self.written.saturating_add(done as u64);
        self.insert(offset, to);
    }

    /// Add `[from, to)` to the present set, coalescing with its neighbours so
    /// the map always holds the minimum number of exact ranges.
    fn insert(&mut self, from: u64, to: u64) {
        if to <= from {
            return;
        }
        let mut start = from;
        let mut end = to;
        // Every range that starts at or before `to` may touch this one
        // (`key <= to` covers the adjacency case `previous.end == from`).
        let touched: Vec<u64> = self
            .present
            .range(..=to)
            .filter(|(_, range_end)| **range_end >= from)
            .map(|(key, _)| *key)
            .collect();
        for key in touched {
            if let Some(range_end) = self.present.remove(&key) {
                start = start.min(key);
                end = end.max(range_end);
            }
        }
        self.present.insert(start, end);
    }

    /// Whether `[start, end)` is wholly present.
    fn covers_range(&self, start: u64, end: u64) -> bool {
        if end <= start {
            return false;
        }
        self.end_at(start) >= end
    }

    /// End of the contiguous present run that contains `offset` (`0` when the
    /// offset itself is not present).
    fn end_at(&self, offset: u64) -> u64 {
        self.present
            .range(..=offset)
            .next_back()
            .filter(|(_, end)| **end > offset)
            .map(|(_, end)| *end)
            .unwrap_or(0)
    }

    /// Read up to `max_len` present bytes at `offset`. `None` when the offset is
    /// not on disk; a short read at the end of the present run is still a hit.
    fn read_at(&self, offset: u64, max_len: u64) -> Option<Vec<u8>> {
        if max_len == 0 {
            return None;
        }
        let end = self.end_at(offset);
        if end <= offset {
            return None;
        }
        let want = (end - offset).min(max_len) as usize;
        let file = self.file.as_ref()?;
        let mut buffer = vec![0u8; want];
        let mut done = 0usize;
        while done < buffer.len() {
            match file.seek_read(&mut buffer[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(read) => done += read,
                Err(_) => break,
            }
        }
        if done == 0 {
            return None;
        }
        buffer.truncate(done);
        Some(buffer)
    }

    /// The present ranges, in offset order (already merged).
    fn ranges(&mut self) -> Vec<(u64, u64)> {
        self.present
            .iter()
            .map(|(start, end)| (*start, *end))
            .collect()
    }

    /// `(contiguous run starting at `offset`, range count, total present bytes)`
    /// from the disk map, in one pass under the caller's lock.
    fn cover(&mut self, offset: u64) -> (u64, usize, u64) {
        let run = self.end_at(offset).saturating_sub(offset);
        let mut ranges = 0usize;
        let mut bytes = 0u64;
        for (start, end) in self.present.iter() {
            ranges += 1;
            bytes = bytes.saturating_add(end.saturating_sub(*start));
        }
        (run, ranges, bytes)
    }

    /// Total present bytes.
    fn bytes(&mut self) -> u64 {
        self.present
            .iter()
            .map(|(start, end)| end.saturating_sub(*start))
            .sum()
    }

    /// First byte of the first hole in `[0, len)` (`len` when complete).
    fn first_hole(&self) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let mut at = 0u64;
        for (start, end) in &self.present {
            if *start > at {
                return at;
            }
            at = at.max(*end);
            if at >= self.len {
                return self.len;
            }
        }
        at.min(self.len)
    }

    /// Sidecar map path: the present ranges next to the `.part` file, so a
    /// preloaded file can be adopted by the next player instead of truncated.
    fn map_path(&self) -> PathBuf {
        self.path.with_extension("map")
    }

    /// Persist the present ranges next to the file (`O11A1MAP` + length + ranges).
    fn save_map(&self) -> bool {
        if self.path.as_os_str().is_empty() || self.len == 0 {
            return false;
        }
        let mut bytes = Vec::with_capacity(24 + self.present.len() * 16);
        bytes.extend_from_slice(b"O11A1MAP");
        bytes.extend_from_slice(&self.len.to_le_bytes());
        bytes.extend_from_slice(&(self.present.len() as u64).to_le_bytes());
        for (start, end) in &self.present {
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.extend_from_slice(&end.to_le_bytes());
        }
        std::fs::write(self.map_path(), bytes).is_ok()
    }

    /// Flush the file, then write the `.map` sidecar. A sidecar must never
    /// describe bytes that are still in a buffer somewhere, so the two steps
    /// have a fixed order: `flush`/`sync_all` first, map second. Used by the
    /// playlist preload, whose file is meant to outlive its writer.
    fn flush_and_save_map(&self) -> bool {
        if let Some(file) = self.file.as_ref() {
            // `&File` is enough: `Write` is implemented for `&File`.
            let _ = (&**file).flush();
            // The preload file is adopted by the *next* process, so its bytes
            // really have to reach the device before the sidecar names them.
            let _ = file.sync_all();
        }
        self.save_map()
    }

    /// Adopt a previously saved map. Returns `false` when the sidecar is
    /// missing, malformed, or does not fit this file length.
    fn load_map(&mut self) -> bool {        let Ok(bytes) = std::fs::read(self.map_path()) else {
            return false;
        };
        if bytes.len() < 24 || &bytes[..8] != b"O11A1MAP" {
            return false;
        }
        let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap_or_default());
        let count = u64::from_le_bytes(bytes[16..24].try_into().unwrap_or_default()) as usize;
        if len == 0 || count > 1 << 20 || bytes.len() < 24 + count * 16 {
            return false;
        }
        let mut present = BTreeMap::new();
        for index in 0..count {
            let at = 24 + index * 16;
            let start = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default());
            let end = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap_or_default());
            if end > start && end <= len {
                present.insert(start, end);
            }
        }
        self.len = len;
        self.present = present;
        true
    }

    /// Close the file and delete it, logging one `cache removed` line. Windows
    /// deletes a file that is still open by a *shared* reader (ffmpeg holding
    /// the decoded file, for instance) only when the last handle closes, so the
    /// removal is retried for a short, bounded while.
    fn remove(&mut self) {
        let bytes = self.bytes();
        let path = self.path.clone();
        let map = self.map_path();
        let written = self.written;
        self.file = None;
        if path.as_os_str().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&map);
        // The retry loop can sleep up to two seconds while ffmpeg still holds
        // the file. That belongs on the pool as speculative (Low) work, never
        // on a thread created just for one close.
        crate::netpool::submit(
            crate::netpool::Prio::Low,
            Box::new(move || {
                for _ in 0..40 {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            crate::debug_log(&format!(
                                "cache removed path={} bytes={bytes} written={written}",
                                path.display()
                            ));
                            return;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            crate::debug_log(&format!(
                                "cache removed path={} bytes={bytes} written={written}",
                                path.display()
                            ));
                            return;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
                crate::debug_log(&format!(
                    "cache remove failed path={} bytes={bytes} reason=held",
                    path.display()
                ));
            }),
        );
    }
}

/// One block a fetch worker wants persisted.
struct WriteJob {
    offset: u64,
    data: Arc<[u8]>,
}

/// What the writer is asked to do. Ordered: `EnsureLen` queued before a block is
/// processed before that block, so `set_len` can never lose bytes to the clamp.
enum WriterMsg {
    /// Persist these bytes at this offset.
    Write(WriteJob),
    /// `set_len(total)`: the media length was learned from an upstream response.
    EnsureLen(u64),
    /// Drain the queue, flush, and stop.
    Stop,
}

/// Blocks waiting for *some* writer, process-wide (the stall line's `disk_q=`).
static DISK_QUEUE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Blocks a worker dropped because the queue was full (they are simply not
/// cached; the map only ever records written bytes).
static DISK_DROPPED: AtomicU64 = AtomicU64::new(0);

/// How many blocks are queued for the disk right now (all sessions).
pub fn disk_queue_depth() -> u64 {
    DISK_QUEUE_TOTAL.load(Ordering::Relaxed)
}

/// The single thread that owns every mutation of one session's cache file.
///
/// Workers call [`DiskWriter::write`], which is a `try_send` on a bounded
/// channel: a full queue drops the block, and since presence is only recorded
/// after a successful write, a dropped block simply reads as "not cached" - no
/// worker ever waits for the disk.
struct DiskWriter {
    tx: Option<SyncSender<WriterMsg>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DiskWriter {
    /// Start the writer for `disk`. `None` when the session has no file
    /// (memory-only): there is nothing to write and no thread to keep.
    fn spawn(disk: Arc<Mutex<DiskCache>>) -> Option<Self> {
        let (file, label) = match disk.lock() {
            Ok(disk) => (disk.file.clone(), disk.label.clone()),
            Err(poisoned) => {
                let disk = poisoned.into_inner();
                (disk.file.clone(), disk.label.clone())
            }
        };
        let file = file?;
        let (tx, rx) = std::sync::mpsc::sync_channel(DISK_QUEUE_DEPTH);
        let thread_label = label.clone();
        let handle = std::thread::Builder::new()
            .name("cache-writer".to_string())
            .spawn(move || {
                crate::media_hooks::note_thread_role("disk-writer");
                writer_loop(rx, &disk, &file, &thread_label);
            })
            .ok()?;
        crate::debug_log(&format!("cache writer start key={label}"));
        Some(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    /// Queue one block. Never blocks: a full queue drops the block.
    fn write(&self, offset: u64, data: Arc<[u8]>) {
        if data.is_empty() {
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(WriterMsg::Write(WriteJob { offset, data })) {
            Ok(()) => {
                DISK_QUEUE_TOTAL.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                DISK_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Queue a `set_len` (rare: once per session, when the length is learned).
    /// The length itself is already in the map (the caller applied it), so the
    /// only thing left here is the file's sparse sizing. `false` means the
    /// queue was full or the writer is gone: the caller sizes the file itself.
    fn ensure_len(&self, total: u64) -> bool {
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        match tx.try_send(WriterMsg::EnsureLen(total)) {
            Ok(()) => {
                DISK_QUEUE_TOTAL.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => false,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    /// Drain, flush and join. Idempotent; the file handle closes with the thread.
    fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            // `Stop` goes through the same queue as the blocks, so everything
            // queued before it is written and flushed before the join returns.
            let _ = tx.send(WriterMsg::Stop);
            DISK_QUEUE_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DiskWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One contiguous span being accumulated into a single write.
struct OpenRun {
    start: u64,
    bytes: Vec<u8>,
    last: Instant,
}

impl OpenRun {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.bytes.len() as u64)
    }

    /// Extend this run with `data` at `offset`. The caller guarantees the block
    /// is adjacent to or overlapping the run, so the result stays contiguous;
    /// bytes that arrive twice (a retried block) take the newest value.
    fn absorb(&mut self, mut offset: u64, mut data: &[u8]) {
        loop {
            if offset < self.start {
                let head = ((self.start - offset) as usize).min(data.len());
                let mut merged = Vec::with_capacity(head + self.bytes.len());
                merged.extend_from_slice(&data[..head]);
                merged.append(&mut self.bytes);
                self.bytes = merged;
                self.start = offset;
                if head == data.len() {
                    return;
                }
                data = &data[head..];
                offset = self.start + head as u64;
                continue;
            }
            let at = (offset - self.start) as usize;
            if at >= self.bytes.len() {
                self.bytes.extend_from_slice(data);
                return;
            }
            let tail = self.bytes.len() - at;
            let overlap = tail.min(data.len());
            self.bytes[at..at + overlap].copy_from_slice(&data[..overlap]);
            if data.len() > overlap {
                self.bytes.extend_from_slice(&data[overlap..]);
            }
            return;
        }
    }
}

/// The writer thread: merge what arrives into large sequential writes.
///
/// - A block adjacent to an open run is appended to it (one `memcpy`, no syscall)
///   until the run reaches [`DISK_MERGE_MAX`], then the run is written.
/// - A run that stops growing for [`DISK_RUN_IDLE`] is written; that is what
///   keeps latency bounded without waiting for a range's last byte.
/// - At most [`DISK_RUNS_MAX`] runs and [`DISK_BUFFER_BUDGET`] bytes are held;
///   past either limit the smallest / oldest run is written, and runs are
///   written in offset order so the file fills mostly front to back.
fn writer_loop(
    rx: std::sync::mpsc::Receiver<WriterMsg>,
    disk: &Mutex<DiskCache>,
    file: &File,
    label: &str,
) {
    let started = Instant::now();
    let mut runs: Vec<OpenRun> = Vec::new();
    let mut stats = WriterStats::new(started);
    loop {
        // Wait for the next block, or for the oldest open run to go idle.
        let now = Instant::now();
        let next_flush = runs
            .iter()
            .map(|run| run.last + DISK_RUN_IDLE)
            .min()
            .unwrap_or(now + DISK_WRITER_TRACE);
        let wait = next_flush.saturating_duration_since(now);
        let msg = match rx.recv_timeout(wait) {
            Ok(msg) => Some(msg),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                flush_all(runs, disk, file, &mut stats);
                stats.log(label, started, true);
                return;
            }
        };
        let stop = match msg {
            None => false,
            Some(WriterMsg::Stop) => {
                DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                true
            }
            Some(WriterMsg::EnsureLen(total)) => {
                DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                apply_len(disk, file, total);
                false
            }
            Some(WriterMsg::Write(job)) => {
                DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                stats.pieces = stats.pieces.saturating_add(1);
                absorb(job, &mut runs, disk, file, &mut stats);
                // Drain whatever else is already queued before paying for a
                // write: a batch of small reads becomes one large call. Bounded,
                // so a queue that refills as fast as we drain it can never delay
                // flushing a run past its idle deadline.
                for _ in 0..DISK_QUEUE_DEPTH * 2 {
                    let Ok(next) = rx.try_recv() else { break };
                    match next {
                        WriterMsg::Write(job) => {
                            DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                            stats.pieces = stats.pieces.saturating_add(1);
                            absorb(job, &mut runs, disk, file, &mut stats);
                        }
                        WriterMsg::EnsureLen(total) => {
                            DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                            apply_len(disk, file, total);
                        }
                        WriterMsg::Stop => {
                            DISK_QUEUE_TOTAL.fetch_sub(1, Ordering::Relaxed);
                            flush_all(runs, disk, file, &mut stats);
                            stats.log(label, started, true);
                            return;
                        }
                    }
                }
                false
            }
        };
        // Write the runs that are full, idle, or over the retainer's budget.
        flush_ready(&mut runs, disk, file, &mut stats);
        if stop {
            flush_all(runs, disk, file, &mut stats);
            stats.log(label, started, true);
            return;
        }
        stats.log(label, started, false);
    }
}

/// `set_len` on the writer thread, under the map lock so the length the readers
/// see and the length of the file never disagree.
fn apply_len(disk: &Mutex<DiskCache>, file: &File, total: u64) {
    match disk.lock() {
        Ok(mut disk) => disk.ensure_len(Some(file), total),
        Err(poisoned) => poisoned.into_inner().ensure_len(Some(file), total),
    }
}

/// Append `job` to the run it continues, or open a new one.
fn absorb(
    job: WriteJob,
    runs: &mut Vec<OpenRun>,
    disk: &Mutex<DiskCache>,
    file: &File,
    stats: &mut WriterStats,
) {
    let WriteJob { offset, data } = job;
    let now = Instant::now();
    let end = offset.saturating_add(data.len() as u64);
    for (index, run) in runs.iter_mut().enumerate() {
        // Overlap or adjacency both extend the run: the writer never issues two
        // calls for bytes it could have written together.
        if offset <= run.end() && end >= run.start {
            run.absorb(offset, &data);
            run.last = now;
            if run.bytes.len() >= DISK_MERGE_MAX {
                let mut run = runs.remove(index);
                // A block that would push the run past the cap is split, so one
                // call is never larger than [`DISK_MERGE_MAX`].
                if run.bytes.len() > DISK_MERGE_MAX {
                    let tail = run.bytes.split_off(DISK_MERGE_MAX);
                    let tail_start = run.end();
                    runs.push(OpenRun {
                        start: tail_start,
                        bytes: tail,
                        last: now,
                    });
                }
                write_run(run, disk, file, stats);
                if runs.len() > DISK_RUNS_MAX {
                    flush_oldest(runs, disk, file, stats);
                }
            }
            return;
        }
    }
    let mut run = OpenRun {
        start: offset,
        bytes: Vec::with_capacity(data.len().min(DISK_MERGE_MAX)),
        last: now,
    };
    run.bytes.extend_from_slice(&data);
    runs.push(run);
    if runs.len() > DISK_RUNS_MAX {
        flush_oldest(runs, disk, file, stats);
    }
}

/// Write every run that is full, idle, or no longer fits the budget.
fn flush_ready(
    runs: &mut Vec<OpenRun>,
    disk: &Mutex<DiskCache>,
    file: &File,
    stats: &mut WriterStats,
) {
    let now = Instant::now();
    loop {
        let over_budget = runs.iter().map(|run| run.bytes.len()).sum::<usize>() > DISK_BUFFER_BUDGET;
        let victim = runs
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.bytes.len() >= DISK_MERGE_MAX || now.duration_since(run.last) >= DISK_RUN_IDLE
            })
            .map(|(index, _)| index)
            .next();
        if let Some(index) = victim {
            let run = runs.remove(index);
            write_run(run, disk, file, stats);
            continue;
        }
        if over_budget {
            flush_oldest(runs, disk, file, stats);
            continue;
        }
        return;
    }
}

/// Write the run that has been waiting longest (largest first on a tie).
fn flush_oldest(
    runs: &mut Vec<OpenRun>,
    disk: &Mutex<DiskCache>,
    file: &File,
    stats: &mut WriterStats,
) {
    let Some(index) = runs
        .iter()
        .enumerate()
        .min_by_key(|(_, run)| (run.last, std::cmp::Reverse(run.bytes.len())))
        .map(|(index, _)| index)
    else {
        return;
    };
    let run = runs.remove(index);
    write_run(run, disk, file, stats);
}

fn flush_all(
    runs: Vec<OpenRun>,
    disk: &Mutex<DiskCache>,
    file: &File,
    stats: &mut WriterStats,
) {
    let mut ordered = runs;
    // Ascending offset: the file fills front to back even after a burst.
    ordered.sort_by_key(|run| run.start);
    for run in ordered {
        write_run(run, disk, file, stats);
    }
}

/// Persist one merged run in a single call and record it in the map.
fn write_run(run: OpenRun, disk: &Mutex<DiskCache>, file: &File, stats: &mut WriterStats) {
    if run.bytes.is_empty() {
        return;
    }
    let len = run.bytes.len() as u64;
    crate::media_hooks::tag_here("prefetch:write");
    match disk.lock() {
        Ok(mut disk) => disk.write_with(file, run.start, &run.bytes),
        Err(poisoned) => poisoned.into_inner().write_with(file, run.start, &run.bytes),
    }
    stats.calls = stats.calls.saturating_add(1);
    stats.bytes = stats.bytes.saturating_add(len);
    stats.max_call = stats.max_call.max(len);
}

/// Call pattern of the writer, logged every [`DISK_WRITER_TRACE`].
struct WriterStats {
    /// `seek_write` calls (each one is one merged run).
    calls: u64,
    /// Bytes those calls wrote.
    bytes: u64,
    /// Blocks received from workers (what the old per-worker path wrote one
    /// call each).
    pieces: u64,
    /// Largest single call.
    max_call: u64,
    /// Snapshot at the previous log line, for the per-interval rates.
    logged_calls: u64,
    logged_bytes: u64,
    logged_pieces: u64,
    logged_at: Instant,
}

impl WriterStats {
    fn new(started: Instant) -> Self {
        Self {
            calls: 0,
            bytes: 0,
            pieces: 0,
            max_call: 0,
            logged_calls: 0,
            logged_bytes: 0,
            logged_pieces: 0,
            logged_at: started,
        }
    }

    fn log(&mut self, label: &str, started: Instant, final_line: bool) {
        if !final_line && self.logged_at.elapsed() < DISK_WRITER_TRACE {
            return;
        }
        let span = self.logged_at.elapsed().as_secs_f64().max(1.0e-6);
        let calls = self.calls.saturating_sub(self.logged_calls);
        let bytes = self.bytes.saturating_sub(self.logged_bytes);
        let pieces = self.pieces.saturating_sub(self.logged_pieces);
        crate::debug_log(&format!(
            "cache writer key={label} calls={} avg={} max={} pieces={} merged={:.1}x calls_s={:.1} pieces_s={:.1} bytes_s={} total_calls={} total_bytes={} dropped={} ms={} done={}",
            calls,
            format_size_div(bytes, calls),
            format_bytes(self.max_call),
            pieces,
            pieces as f64 / calls.max(1) as f64,
            calls as f64 / span,
            pieces as f64 / span,
            format_rate((bytes as f64 / span) as u64),
            self.calls,
            self.bytes,
            DISK_DROPPED.load(Ordering::Relaxed),
            started.elapsed().as_millis(),
            u8::from(final_line)
        ));
        self.logged_calls = self.calls;
        self.logged_bytes = self.bytes;
        self.logged_pieces = self.pieces;
        self.logged_at = Instant::now();
    }
}

/// `691.2KiB` for an average call size; `-` when nothing was written.
fn format_size_div(bytes: u64, count: u64) -> String {
    if count == 0 {
        return "-".to_string();
    }
    format_bytes(bytes / count)
}

/// `3.2 MB/s` for the QA stats line; counted bytes, so no `≈`.
fn format_rate(bytes_per_sec: u64) -> String {    if bytes_per_sec >= 1_000_000 {
        format!("{:.1}MB/s", bytes_per_sec as f64 / 1_000_000.0)
    } else if bytes_per_sec >= 1_000 {
        format!("{:.0}KB/s", bytes_per_sec as f64 / 1_000.0)
    } else {
        format!("{bytes_per_sec}B/s")
    }
}

/// The `count` offsets warmed for seek-ahead, `i / count` of the total size
/// for `i = 0 .. count`.
///
/// The offsets are monotonic and de-duplicated, so a file smaller than the
/// requested point count yields fewer points instead of repeated fetches at
/// the same offset. A zero-length total yields no points.
fn warm_point_offsets(total: u64, count: usize) -> Vec<u64> {
    if total == 0 || count == 0 {
        return Vec::new();
    }
    let count = count.min(MAX_WARM_POINTS);
    let mut offsets: Vec<u64> = Vec::with_capacity(count);
    for index in 0..count as u64 {
        let offset = total.saturating_mul(index) / count as u64;
        let offset = offset.min(total - 1);
        if offsets.last().copied() != Some(offset) {
            offsets.push(offset);
        }
    }
    offsets
}

/// Bytes to fetch for one seek point.
///
/// `min(configured, total / count)` keeps a small file's points proportional
/// to its size, and the 80 MiB budget clamp is applied a second time so the
/// cache can never exceed [`MAX_POINT_CACHE_BYTES`] even if the caller passes
/// a huge `count` or `configured` value.
fn point_chunk_bytes(total: u64, count: usize, configured: u64) -> u64 {
    if total == 0 || count == 0 {
        return 0;
    }
    let count = count.min(MAX_WARM_POINTS);
    let spacing = (total / count as u64).max(1);
    let budget = (MAX_POINT_CACHE_BYTES / count as u64).max(1);
    configured
        .clamp(1, DEFAULT_POINT_BYTES)
        .min(spacing)
        .min(budget)
}

/// Warm seek points held in memory, keyed by absolute offset and bounded to
/// `max_points` entries / `max_bytes`. Entries are only evicted when the bound
/// is exceeded, oldest first; a hit never removes the entry, so the same seek
/// position stays instant.
#[derive(Default)]
struct PointCache {
    entries: BTreeMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<u64>,
    bytes: u64,
}

impl PointCache {
    /// Store one point, evicting the oldest entries until both bounds hold.
    /// Returns how many entries were evicted. An empty `data` is ignored.
    fn insert(&mut self, offset: u64, data: Vec<u8>, max_points: usize, max_bytes: u64) -> usize {
        if data.is_empty() {
            return 0;
        }
        if let Some(old) = self.entries.remove(&offset) {
            self.bytes = self.bytes.saturating_sub(old.len() as u64);
            self.order.retain(|key| *key != offset);
        }
        self.bytes += data.len() as u64;
        self.entries.insert(offset, Arc::new(data));
        self.order.push_back(offset);
        let mut evicted = 0;
        while self.order.len() > max_points.max(1) || self.bytes > max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(old.len() as u64);
                evicted += 1;
            }
        }
        evicted
    }

    /// `(point offset, bytes, skip)` for the entry containing `pos`, or `None`
    /// when no cached point covers it. The `Arc` clone is cheap, so callers do
    /// not hold the state lock while writing to a socket.
    fn lookup(&self, pos: u64) -> Option<(u64, Arc<Vec<u8>>, usize)> {
        let (&start, data) = self.entries.range(..=pos).next_back()?;
        let skip = (pos - start) as usize;
        if skip >= data.len() {
            return None;
        }
        Some((start, data.clone(), skip))
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Remove up to `max_len` bytes starting at `pos` from the chunk map, keeping
/// the unserved remainder addressable at the new cursor. A missing chunk
/// returns `None` (the caller waits).
fn take_bounded(
    chunks: &mut BTreeMap<u64, Vec<u8>>,
    pos: u64,
    max_len: u64,
) -> Option<Vec<u8>> {
    if max_len == 0 {
        return None;
    }
    let mut data = if let Some(data) = chunks.remove(&pos) {
        data
    } else {
        // Defensive: a chunk starting before `pos` that still covers it.
        let key = *chunks.range(..pos).next_back().map(|(key, _)| key)?;
        let data = chunks.remove(&key)?;
        let offset = (pos - key) as usize;
        if offset >= data.len() {
            return None;
        }
        data[offset..].to_vec()
    };
    let take = data.len().min(max_len as usize);
    if take < data.len() {
        let rest = data.split_off(take);
        chunks.insert(pos + take as u64, rest);
    }
    Some(data)
}

/// Keep the buffered window inside `limit` bytes: drop bytes the cursor left
/// behind, then evict the farthest chunk until the bound holds. The remaining
/// data stays contiguous around the cursor, so a small seek forward inside the
/// window is still instant.
fn trim_window(state: &mut State, limit: u64) {
    let limit = limit.max(1);
    let cursor = state.cursor;
    state.chunks.retain(|start, data| {
        let end = start.saturating_add(data.len() as u64);
        end > cursor
    });
    state.read_ahead = state.chunks.values().map(|data| data.len() as u64).sum();
    while state.read_ahead > limit {
        let Some((&key, _)) = state.chunks.iter().next_back() else {
            state.read_ahead = 0;
            break;
        };
        if let Some(data) = state.chunks.remove(&key) {
            state.read_ahead = state.read_ahead.saturating_sub(data.len() as u64);
        }
    }
}

/// Sliding throughput over `window`. Traffic older than the window is dropped;
/// the rate is extrapolated while the window is still filling and never
/// divides by less than 250 ms (so the first chunk does not read as a spike).
fn window_rate(
    samples: &mut VecDeque<(Instant, u64)>,
    now: Instant,
    window: Duration,
) -> u64 {
    while samples.len() > 1 && now.duration_since(samples[0].0) > window {
        samples.pop_front();
    }
    let Some(&(first, _)) = samples.front() else {
        return 0;
    };
    // A window that has seen no traffic for the whole period reads as zero
    // instead of holding its last value forever.
    if now.duration_since(samples.back().map_or(first, |(stamp, _)| *stamp)) > window {
        return 0;
    }
    let bytes: u64 = samples.iter().map(|(_, bytes)| *bytes).sum();
    let span = now
        .duration_since(first)
        .as_secs_f64()
        .clamp(0.25, window.as_secs_f64().max(0.25));
    (bytes as f64 / span).round() as u64
}

/// One-second sliding throughput (the profiler panel's aggregate rate).
fn sliding_rate(samples: &mut VecDeque<(Instant, u64)>, now: Instant) -> u64 {
    window_rate(samples, now, RATE_WINDOW)
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Host {
    label: String,
    origin: String,
    /// Consecutive failed range attempts. A single transient body error must
    /// not poison the mirror: the host is cooled down, then tried again, and
    /// the first stored byte clears the count. Only [`HOST_FAILURE_LIMIT`]
    /// consecutive failures retire it for the session.
    failures: u32,
    failed: bool,
    bytes: u64,
    /// A route the health watchdog found slow is skipped by the round robin
    /// until this instant, then tried again.
    degraded_until: Option<Instant>,
}

/// Live state of one upstream worker, owned by the state lock so a snapshot
/// sees the exact range each connection holds.
#[derive(Clone, Debug, Default)]
struct WorkerSlot {
    host: String,
    start: u64,
    end: u64,
    busy: bool,
    /// What the held range is for: the client's hot window, a warm point, the
    /// speculative queue, or a retry. `None` before the first claim.
    kind: Option<ChunkKind>,
    bytes: u64,
    /// Throughput over the last [`HEALTH_WINDOW`], bytes/s.
    bps: u64,
    /// Set by the health watchdog; cleared by the first byte after a retry.
    slow: bool,
    /// When the current range was claimed (drives the health timers).
    range_started: Option<Instant>,
    /// Byte samples of the current range, for the health watchdog.
    samples: VecDeque<(Instant, u64)>,
}

/// One contiguous byte part of the whole-file rest queue. A part is owned by a
/// worker while the dynamic split is in force; `next` is the first sub-chunk
/// of it that has not been handed out.
#[derive(Clone, Debug)]
struct Part {
    end: u64,
    next: u64,
    owner: Option<usize>,
}

/// One seek point scheduled through the same worker pool. `point` is the
/// configured `i/count` offset, `start`/`end` the chunk-aligned range that
/// actually gets cached.
#[derive(Clone, Debug)]
struct WarmTask {
    point: u64,
    start: u64,
    end: u64,
    done: bool,
    claimed: bool,
    /// Queued by [`PrefetchHandle::prime_at`]: a seek is happening *now*, so the
    /// task skips the speculative rate limit (a scheduled point can wait).
    urgent: bool,
}

/// A duplicate request for a range a slow worker is still fetching.
#[derive(Clone, Debug)]
struct RaceRequest {
    generation: u64,
    start: u64,
    end: u64,
    slow_host: String,
}

/// What a worker is fetching: the client's read window, a seek warm point, a
/// speculative whole-file chunk, a retry after a slow worker, or a duplicate
/// race for a slow range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkKind {
    Hot,
    Warm,
    Rest,
    Requeue,
    Race,
}

struct State {
    /// First byte of the current chunk alignment (reset on seek).
    base: u64,
    /// Next byte to serve.
    cursor: u64,
    /// Bumped on every reset so stale in-flight results are discarded.
    generation: u64,
    /// Fetched bytes not yet served, keyed by absolute offset.
    chunks: BTreeMap<u64, Vec<u8>>,
    /// `(generation, chunk start)` currently being fetched.
    inflight: BTreeSet<(u64, u64)>,
    /// `(generation, chunk start)` that failed on every attempted host.
    dead: BTreeSet<(u64, u64)>,
    /// Chunk starts already fetched for this generation. Unlike `chunks`, this
    /// survives memory eviction, so speculative bytes are never fetched twice.
    done: BTreeSet<u64>,
    /// Whole-file rest queue, split dynamically for long files.
    parts: Vec<Part>,
    /// Whether [`State::parts`] was built for the current file.
    parts_built: bool,
    /// Ranges that failed on their retry route and went back to the queue.
    requeued: BTreeSet<u64>,
    /// Per chunk start, how many times an upstream body stopped before the
    /// requested end while the file length was already known. A chunk is only
    /// retired after [`SHORT_BODY_LIMIT`] of them (see `complete_range`).
    short_bodies: BTreeMap<u64, u32>,
    /// Seek points scheduled through the worker pool.
    warm_plan: Vec<WarmTask>,
    /// Whether [`State::warm_plan`] was built (the total length was known).
    warm_plan_started: bool,
    /// Byte samples of what was served, for the "next two seconds" priority.
    serve_samples: VecDeque<(Instant, u64)>,
    /// Duplicate requests waiting for an idle worker.
    races: VecDeque<RaceRequest>,
    /// Offsets a response body is blocked on *right now* (a serve wait). The
    /// next worker claims these exact bytes before the chunk-aligned hot scan:
    /// a wait must not sit behind the chunk prefix on a slow-to-start route,
    /// which is seconds of media on a 4 MiB chunk.
    urgent: VecDeque<u64>,
    /// One winner token per raced chunk start (first finisher wins).
    race_tokens: BTreeMap<u64, Arc<AtomicBool>>,
    total: Option<u64>,
    fatal: Option<String>,
    content_type: Option<String>,
    hosts: Vec<Host>,
    next_host: usize,
    samples: VecDeque<(Instant, u64)>,
    bytes_fetched: u64,
    failed_ranges: u64,
    served: u64,
    /// One-second sliding rate of the bytes the client pulled (bytes/s). With
    /// no duration to divide by, this is the media bitrate estimate the
    /// speculative rate cap is built on.
    serve_bps: u64,
    /// First byte handed to the client, the epoch of the `serve_bps` average.
    first_serve: Option<Instant>,
    /// Token bucket for whole-file (speculative) fetching: bytes currently
    /// available, when they were last refilled, and how many were spent.
    spec_tokens: u64,
    spec_at: Instant,
    spec_bytes: u64,
    /// Buffered bytes beyond the cursor (mirrors `chunks`, kept incrementally).
    read_ahead: u64,
    /// Warm seek points fetched into memory, independent of the window.
    points: PointCache,
    /// Distinct seek points the warm task intends to fill.
    warm_points_total: u32,
    /// Seek points already available (warmed here or covered elsewhere).
    warm_points_done: u32,
    /// Bytes per warm point chosen for this file.
    warm_chunk: u64,
    /// Bytes put into the point cache so far (diagnostics only).
    warm_bytes: u64,
    /// Upstream bytes by scheduling class (hot window / seek points / whole-file
    /// queue / retries). Diagnostics: the split the priority rule is judged on.
    hot_bytes: u64,
    warm_fetch_bytes: u64,
    rest_bytes: u64,
    retry_bytes: u64,
    /// Requests served from the point cache, and bytes those served.
    point_hits: u64,
    point_served: u64,
    /// Requests served (at least partly) from the session cache file, and the
    /// bytes those served.
    disk_hits: u64,
    disk_served: u64,
    /// One slot per configured connection (`workers.len() == connections`).
    workers: Vec<WorkerSlot>,
}

struct Engine {
    config: PrefetchConfig,
    target: Target,
    client: reqwest::Client,
    /// Name of this session's cache file, so a preload can never touch the file
    /// a decoder is reading.
    cache_key: String,
    state: Mutex<State>,
    /// The session cache file's read handle and its present ranges. Nothing on
    /// this path writes the file: the single [`DiskWriter`] owns every mutation,
    /// and a range appears here only after the writer's call returned.
    disk: Arc<Mutex<DiskCache>>,
    /// The session's one coalescing writer (`None` for a memory-only session or
    /// after shutdown).
    writer: Mutex<Option<DiskWriter>>,
    stats: Arc<Mutex<PrefetchStats>>,
    /// Serialises response handlers (the read cursor is shared).
    serving: AtomicBool,
    /// Token of the response that may write now. Every accepted request bumps
    /// it; a handler that sees a newer token stops, so a seek is never queued
    /// behind the open-ended response ffmpeg abandoned.
    serve_epoch: AtomicU64,
    /// Startup gate: cap on the read-ahead window, raised to
    /// [`PrefetchConfig::read_ahead_bytes`] once frames are flowing (or
    /// `u64::MAX` when the gate is off, i.e. no cap beyond the config).
    read_ahead_cap: AtomicU64,
    /// Player-driven hold on background work (see
    /// [`PrefetchHandle::set_background_paused`]). Held, workers fetch only the
    /// client's hot window; seek points and speculative parts wait.
    background_paused: AtomicBool,
    /// Live connection count. The governor (or `set_connections`) raises and
    /// lowers it; workers with an id at or above the current value stay out of
    /// the queue.
    desired_connections: AtomicU64,
    /// The configured count the governor may widen to (immutable).
    configured_connections: u64,
    /// Speculative (Warm/Rest) workers allowed right now; `0` = no cap. Kept at
    /// zero: the connection count is the cap, and it follows the measured knee.
    background_workers: AtomicU64,
    /// Serve waits opened since the session started. The connection governor
    /// reads the delta as telemetry (every wait, however short).
    serve_waits: AtomicU64,
    /// Serve waits that actually hurt: closed after at least
    /// [`SERVE_WAIT_SLOW_MS`] of waiting. Only this counter steps the pool
    /// down. A normal read that arrives a few milliseconds before the startup
    /// window lands used to halve sixteen connections to two on every seek and
    /// every buffer refill (measured: 22 waits, all 5–14 ms), which is the
    /// "16 线程只有一两个在干活" the governor was supposed to prevent.
    serve_wait_slow: AtomicU64,
    /// Per-worker cancellation tokens. The health watchdog bumps one to make a
    /// slow worker drop its current request at the next network read.
    worker_cancel: Vec<AtomicU64>,
    /// Media bitrate in bytes per second (`total / duration`), `0` until both
    /// are known. Sets the read-ahead horizon and the speculative rate cap;
    /// lock-free so [`Engine::read_ahead_window`] can stay a pure load.
    media_bps: AtomicU64,
    /// Offset a response body is blocked on right now, `u64::MAX` when none.
    /// While this is set the scheduler gives the client's hot window absolute
    /// priority: speculative claims (Warm/Rest) are refused and the ones already
    /// in flight are cancelled, so a worker frees up for the missing chunk.
    serve_wait_pos: AtomicU64,
    /// Millis since `started` when that wait began (its first miss).
    serve_wait_since_ms: AtomicU64,
    /// Budget overruns (`>=` [`SERVE_WAIT_BUDGET_MS`]) escalated in the open
    /// wait span, logged once per span and reset when it closes.
    serve_wait_escalations: AtomicU64,
    started: Instant,
}

impl Engine {
    /// Bitrate of the stream in bytes per second, when the caller supplied the
    /// duration and an upstream response supplied the length.
    fn media_bps(&self) -> u64 {
        self.media_bps.load(Ordering::Relaxed)
    }

    /// The bitrate estimate the scheduler runs on: the exact one when the
    /// duration is known, otherwise the session-average rate the client pulls,
    /// clamped to a plausible stream so a first-second burst cannot inflate
    /// every horizon (and a stalled client cannot collapse them).
    fn bitrate_for(&self, state: &State) -> u64 {
        let exact = self.media_bps();
        if exact > 0 {
            return exact;
        }
        state
            .serve_bps
            .clamp(FALLBACK_BITRATE_MIN, FALLBACK_BITRATE_MAX)
    }

    /// Recompute the bitrate hint from `state.total` and the configured
    /// duration. Called whenever the length is learned.
    fn refresh_media_bps(&self, total: u64) {
        let Some(ms) = self.config.media_duration_ms.filter(|ms| *ms > 0) else {
            return;
        };
        if total == 0 {
            return;
        }
        let bps = ((total as u128) * 1000 / (ms as u128)).min(u64::MAX as u128) as u64;
        if bps > 0 {
            self.media_bps.store(bps, Ordering::Relaxed);
        }
    }

    /// Read-ahead window in force right now: the startup cap until the gate
    /// opens, the configured window afterwards, and never more than
    /// [`PLAYBACK_AHEAD_SECONDS`] of media. Always at least one chunk.
    fn read_ahead_window(&self) -> u64 {
        let configured = self
            .config
            .read_ahead_bytes
            .min(self.read_ahead_cap.load(Ordering::Relaxed))
            .max(self.config.chunk_bytes);
        let bps = self.media_bps();
        if bps == 0 {
            return configured;
        }
        let horizon = bps
            .saturating_mul(PLAYBACK_AHEAD_SECONDS)
            .max(self.config.chunk_bytes.saturating_mul(2));
        configured.min(horizon)
    }

    /// Live connection count (clamped 1..=[`MAX_CONNECTIONS`]).
    fn desired_connections(&self) -> usize {
        (self.desired_connections.load(Ordering::Relaxed) as usize).clamp(1, MAX_CONNECTIONS)
    }

    /// Whether background work may run now (seek points and the speculative
    /// whole-file parts wait while it is held). Test-only: the runtime path
    /// reads [`Engine::background_held`] directly.
    #[cfg(test)]
    fn background_allowed(&self) -> bool {
        !self.background_held()
    }

    /// Whether the player holds background work right now.
    fn background_held(&self) -> bool {
        self.background_paused.load(Ordering::SeqCst)
    }

    /// Whether a response body is waiting for bytes right now. While it is,
    /// only the client's hot window may be claimed (see [`pick_chunk`]).
    fn serve_waiting(&self) -> bool {
        self.serve_wait_pos.load(Ordering::Relaxed) != u64::MAX
    }

    /// Register that a response body is blocked at `pos`, and return how long
    /// the (possibly already open) wait has lasted. The first miss opens the
    /// span; it closes when bytes arrive or the body ends.
    fn open_serve_wait(&self, pos: u64) -> u64 {
        let now_ms = self.started.elapsed().as_millis() as u64;
        if self.serve_wait_pos.swap(pos, Ordering::SeqCst) == u64::MAX {
            self.serve_wait_since_ms.store(now_ms, Ordering::SeqCst);
            self.serve_wait_escalations.store(0, Ordering::SeqCst);
            self.serve_waits.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        now_ms.saturating_sub(self.serve_wait_since_ms.load(Ordering::SeqCst))
    }

    /// Close the open serve wait. `reason=available` when bytes landed,
    /// `reason=timeout` when the body ended without them (superseded, error,
    /// EOF, shutdown). No-op when nothing is waiting.
    fn close_serve_wait(&self, reason: &str) {
        let pos = self.serve_wait_pos.swap(u64::MAX, Ordering::SeqCst);
        if pos == u64::MAX {
            return;
        }
        let since = self.serve_wait_since_ms.load(Ordering::SeqCst);
        let waited = (self.started.elapsed().as_millis() as u64).saturating_sub(since);
        if waited >= SERVE_WAIT_SLOW_MS {
            self.serve_wait_slow.fetch_add(1, Ordering::Relaxed);
        }
        let escalated = self.serve_wait_escalations.swap(0, Ordering::SeqCst);
        crate::debug_log(&format!(
            "serve wait end pos={pos} waited_ms={waited} reason={reason} escalated={escalated}"
        ));
    }

    /// `(hot, warm, rest)` claims in flight right now (the serve-wait line).
    fn inflight_kinds(&self) -> (u32, u32, u32) {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut hot = 0u32;
        let mut warm = 0u32;
        let mut rest = 0u32;
        for slot in state.workers.iter().filter(|slot| slot.busy) {
            match slot.kind {
                Some(ChunkKind::Hot) => hot = hot.saturating_add(1),
                Some(ChunkKind::Warm) => warm = warm.saturating_add(1),
                Some(ChunkKind::Rest) => rest = rest.saturating_add(1),
                _ => {}
            }
        }
        (hot, warm, rest)
    }

    /// Give the body waiting at `pos` absolute priority over speculative work.
    ///
    /// Called by the serve path while a wait is open (every
    /// [`SERVE_WAIT_ENFORCE_POLL`]), and once when it opens:
    ///
    /// 1. The exact offset the body is blocked on is registered as an urgent
    ///    claim ([`State::urgent`]) when nothing is fetching it. The next worker
    ///    starts its request at `pos`, not at the chunk boundary: on a 4 MiB
    ///    chunk the prefix is three seconds of media the client cannot use yet.
    ///    If every connection is busy with look-ahead, the farthest one is
    ///    cancelled once per wait to free a slot.
    /// 2. Every in-flight speculative claim (`Warm`/`Rest`) is cancelled. Its
    ///    worker id's cancellation token is bumped, so the transfer lets go at
    ///    its next network read and the freed slot claims the hot bytes; the
    ///    range itself goes back to the queue and is fetched later.
    /// 3. A `Hot` claim covering `pos` that produced no byte for
    ///    [`HOT_STUCK_MS`] is stuck: the worker is marked slow — which makes
    ///    `run_claim` retry the same range on the next host — its route is
    ///    degraded, and a duplicate race is queued on another mirror. That is
    ///    the existing per-host retry/race path, running 4.5 s earlier than the
    ///    general health timer.
    /// 4. Past [`SERVE_WAIT_BUDGET_MS`] the breach is logged once per wait span
    ///    — the escalation itself is the host switch above, which cannot help
    ///    when no single claim is to blame.
    fn enforce_serve_priority(&self, pos: u64, waited_ms: u64) {
        let now = Instant::now();
        let force = waited_ms >= SERVE_WAIT_BUDGET_MS;
        let live = self.desired_connections();
        let mut cancelled = 0usize;
        let mut stuck: Option<(usize, u64, u64, String, u64)> = None;
        let mut breach = false;
        let mut ahead: Option<(usize, u64)> = None;
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            let generation = state.generation;
            let covered = chunks_cover(&state.chunks, pos);
            let mut inflight_cover = false;
            let mut free_slots = 0usize;
            let mut farthest_ahead: Option<(usize, u64)> = None;
            for id in 0..state.workers.len() {
                let Some(slot) = state.workers.get(id) else {
                    continue;
                };
                if !slot.busy {
                    if id < live {
                        free_slots = free_slots.saturating_add(1);
                    }
                    continue;
                }
                if slot.start <= pos && pos < slot.end {
                    inflight_cover = true;
                }
                if slot.kind == Some(ChunkKind::Hot) && slot.start > pos {
                    // The farthest look-ahead claim is the least valuable right
                    // now: the body is blocked, that chunk is not due yet.
                    if farthest_ahead.map_or(true, |(_, start)| slot.start > start) {
                        farthest_ahead = Some((id, slot.start));
                    }
                }
                // Silence is measured from the last byte this request produced
                // (or from its claim when it has not produced one yet).
                let last = slot
                    .samples
                    .back()
                    .map_or_else(|| slot.range_started.unwrap_or(now), |(at, _)| *at);
                let silent_ms = now.saturating_duration_since(last).as_millis() as u64;
                match slot.kind {
                    Some(ChunkKind::Warm) | Some(ChunkKind::Rest) => {
                        if let Some(token) = self.worker_cancel.get(id) {
                            token.fetch_add(1, Ordering::SeqCst);
                        }
                        cancelled = cancelled.saturating_add(1);
                    }
                    Some(ChunkKind::Hot) => {
                        // Only the claim that covers the missing offset can end
                        // this wait, and only after it has had a fair chance
                        // ([`HOT_STUCK_MS`]): a just-re-routed range must not be
                        // cancelled again before its first read.
                        let age_ms = slot.range_started.map_or(0, |claimed| {
                            now.saturating_duration_since(claimed).as_millis() as u64
                        });
                        if !(slot.start <= pos && pos < slot.end)
                            || slot.slow
                            || age_ms < HOT_STUCK_MS
                        {
                            continue;
                        }
                        // Dead or stalled: not one byte for 500 ms. A range
                        // that is merely slow-but-flowing is not cancelled here;
                        // the budget line reports the breach instead, because
                        // restarting a transfer that is making progress only
                        // makes the wait longer.
                        if silent_ms >= HOT_STUCK_MS && stuck.is_none() {
                            stuck = Some((id, slot.start, slot.end, slot.host.clone(), silent_ms));
                        }
                    }
                    _ => {}
                }
            }
            if !covered && !inflight_cover {
                // Nothing is fetching the byte the client is blocked on (or the
                // range that contained it was just cancelled): register the
                // exact offset. The next claim starts there, not at the chunk
                // boundary, so the wait never sits behind a chunk prefix.
                let urgent_new = !state.urgent.contains(&pos);
                if urgent_new {
                    state.urgent.retain(|start| *start == pos);
                    state.urgent.push_back(pos);
                }
                // Every connection busy with look-ahead: take the farthest one
                // for this byte. Once per wait (not per enforcement tick), so a
                // claim is never churned while it is being established.
                if urgent_new && free_slots == 0 {
                    if let Some((id, start)) = farthest_ahead {
                        if let Some(token) = self.worker_cancel.get(id) {
                            token.fetch_add(1, Ordering::SeqCst);
                        }
                        ahead = Some((id, start));
                    }
                }
            }
            if let Some((id, start, end, ref host, _)) = stuck {
                if let Some(slot) = state.workers.get_mut(id) {
                    slot.slow = true;
                }
                if let Some(index) = state.hosts.iter().position(|known| &known.label == host) {
                    degrade_host(&mut state, index);
                }
                if let Some(token) = self.worker_cancel.get(id) {
                    token.fetch_add(1, Ordering::SeqCst);
                }
                if start < end && !state.race_tokens.contains_key(&start) {
                    state.races.push_back(RaceRequest {
                        generation,
                        start,
                        end,
                        slow_host: host.clone(),
                    });
                    state
                        .race_tokens
                        .insert(start, Arc::new(AtomicBool::new(false)));
                }
            }
            if force && self.serve_wait_escalations.load(Ordering::SeqCst) == 0 {
                self.serve_wait_escalations.store(1, Ordering::SeqCst);
                breach = true;
            }
        }
        if cancelled > 0 {
            crate::debug_log(&format!(
                "serve wait preempt pos={pos} waited_ms={waited_ms} speculative_cancelled={cancelled}"
            ));
        }
        if let Some((id, start)) = ahead {
            crate::debug_log(&format!(
                "serve wait preempt pos={pos} waited_ms={waited_ms} hot_ahead_cancelled={id} range_start={start}"
            ));
        }
        if let Some((id, start, end, host, silent_ms)) = stuck {
            crate::debug_log(&format!(
                "hot stuck pos={pos} waited_ms={waited_ms} range={start}-{end} host={host} silent_ms={silent_ms} worker={id} switching host action=retry"
            ));
        }
        if breach {
            crate::debug_log(&format!(
                "serve wait budget pos={pos} waited_ms={waited_ms} budget_ms={SERVE_WAIT_BUDGET_MS} action=escalate"
            ));
        }
    }

    /// Publish counters from `state`. Callers hold the state lock; the stats
    /// lock is only ever taken *after* it, everywhere in this module.
    fn publish(&self, state: &mut State, now: Instant) {
        let rate = sliding_rate(&mut state.samples, now);
        if let Ok(mut stats) = self.stats.lock() {
            stats.bytes_fetched = state.bytes_fetched;
            stats.bytes_per_sec = rate;
            stats.bytes_total = state.total;
            stats.total_bytes = state.total;
            stats.read_ahead_bytes = state.read_ahead;
            stats.hosts.clear();
            stats.hosts.extend(
                state
                    .hosts
                    .iter()
                    .map(|host| (host.label.clone(), host.bytes)),
            );
            stats.failed_ranges = state.failed_ranges;
            stats.warm_points_done = state.warm_points_done;
            stats.warm_points_total = state.warm_points_total;
            stats.point_hits = state.point_hits;
            stats.point_bytes = state.point_served;
            stats.point_cache_bytes = state.points.bytes;
            // Per-connection view: the exact range and host each worker holds
            // right now (a waiting worker keeps its last range, `busy: false`).
            // Only the live connection count is published; the extra worker
            // tasks wait outside the queue when the count is lowered.
            let live = self.desired_connections();
            stats.workers.clear();
            stats.workers.extend(
                state
                    .workers
                    .iter()
                    .take(live)
                    .enumerate()
                    .map(|(id, slot)| WorkerStat {
                        id: id as u32,
                        host: slot.host.clone(),
                        start: slot.start,
                        end: slot.end,
                        busy: slot.busy,
                        bytes: slot.bytes,
                        bps: slot.bps,
                        slow: slot.slow,
                    }),
            );
            stats.active_workers = state
                .workers
                .iter()
                .take(live)
                .filter(|slot| slot.busy)
                .count() as u32;
            stats.inflight = state.inflight.len() as u64;
            // Whole-file queue: chunks the scheduler still has to hand out
            // (fetched and in-flight chunks are no longer queued).
            let chunk = self.config.chunk_bytes.max(1);
            let total_chunks = state.total.map(|total| total.div_ceil(chunk)).unwrap_or(0);
            let claimed = state.done.len() as u64 + state.inflight.len() as u64;
            let queued = total_chunks.saturating_sub(claimed);
            stats.queue_chunks = queued;
            stats.queue_bytes = queued.saturating_mul(chunk);
            // Heatmap view: the session cache file is the truth, so the map is
            // stable across seeks and window trims. The memory window and the
            // point cache can only add bytes the file could not persist (cache
            // cap, or a session with no writable directory).
            //
            // The disk lock is taken with `try_lock`: `publish` runs with the
            // state lock held on every network read, and a worker writing a
            // 256 KiB block must never make the serve path wait. A busy disk
            // leaves the previous (still correct) map in place instead of
            // publishing a memory-only one.
            if let Ok(mut disk) = self.disk.try_lock() {
                let cache_capped = disk.capped;
                let present = disk.ranges();
                let cache_bytes: u64 = present
                    .iter()
                    .map(|(start, end)| end.saturating_sub(*start))
                    .sum();
                let mut ranges: Vec<(u64, u64)> =
                    Vec::with_capacity(present.len() + state.chunks.len());
                let on_disk = disk.present();
                // Availability (what the panel paints): every byte the session
                // can serve, file or memory.
                let mut available: Vec<(u64, u64)> =
                    Vec::with_capacity(present.len() + state.chunks.len());
                available.extend(present.iter().copied());
                available.extend(state.chunks.iter().filter_map(|(start, data)| {
                    let len = data.len() as u64;
                    (len > 0).then_some((*start, start.saturating_add(len)))
                }));
                available.extend(point_ranges(&state.points).iter().copied());
                stats.available = merge_ranges(available, MAX_DOWNLOADED_RANGES);
                if on_disk {
                    ranges.extend(present.iter().copied());
                }
                if !on_disk || cache_capped {
                    // Without a file (or past its cap) the memory window and the
                    // warm points are the only record of those bytes.
                    ranges.extend(state.chunks.iter().filter_map(|(start, data)| {
                        let len = data.len() as u64;
                        (len > 0).then_some((*start, start.saturating_add(len)))
                    }));
                    ranges.extend(point_ranges(&state.points).iter().copied());
                }
                stats.downloaded = merge_ranges(ranges, MAX_DOWNLOADED_RANGES);
                stats.cache_bytes = cache_bytes;
                stats.cache_capped = cache_capped;
            }
            stats.point_cache = point_ranges(&state.points);
            stats.disk_hits = state.disk_hits;
            stats.disk_bytes = state.disk_served;
            stats.bytes_hot = state.hot_bytes;
            stats.bytes_warm = state.warm_fetch_bytes;
            stats.bytes_rest = state.rest_bytes;
            stats.bytes_retry = state.retry_bytes;
        }
    }

    fn snapshot(&self) -> PrefetchStats {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.publish(&mut state, Instant::now());
        self.stats
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }

    /// Per-worker `(id, host, slow, bytes_per_sec)` for the live connections,
    /// refreshing the five-second rate before returning it.
    fn worker_health_snapshot(&self) -> Vec<(u32, String, bool, u64)> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        let live = self.desired_connections();
        let mut health = Vec::with_capacity(live);
        for (id, slot) in state.workers.iter_mut().take(live).enumerate() {
            if slot.busy {
                slot.bps = window_rate(&mut slot.samples, now, HEALTH_WINDOW);
            }
            health.push((id as u32, slot.host.clone(), slot.slow, slot.bps));
        }
        health
    }

    /// Merged, sorted `[start, end)` ranges present in the session cache file —
    /// the disk map, which is what "downloaded" means — unioned with the
    /// memory window and the point cache for the bytes the file could not
    /// persist (cache cap or no writable directory).
    ///
    /// This does not change when the read cursor moves: a seek trims the memory
    /// window, never the file.
    fn cached_ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges = {
            let mut disk = match self.disk.lock() {
                Ok(disk) => disk,
                Err(poisoned) => poisoned.into_inner(),
            };
            disk.ranges()
        };
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        ranges.extend(state.chunks.iter().filter_map(|(start, data)| {
            let len = data.len() as u64;
            (len > 0).then_some((*start, start.saturating_add(len)))
        }));
        ranges.extend(point_ranges(&state.points).iter().copied());
        let cap = ranges.len().max(1);
        merge_ranges(ranges, cap)
    }

    /// Ranges the session cache file holds (the disk map the 下载分布 panel
    /// draws). Unlike [`Engine::cached_ranges`] this excludes the memory window,
    /// which by design changes when the read cursor moves.
    fn disk_ranges(&self) -> Vec<(u64, u64)> {
        let mut disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        disk.ranges()
    }

    /// What the **disk map** holds, in one lock: the contiguous run of bytes
    /// starting at `offset` (0 when the offset itself is missing), the number of
    /// present ranges and their total bytes.
    ///
    /// This is the only honest answer to "is this seek already downloaded": the
    /// map is what the 下载分布 panel draws and what the player's local-file
    /// decode tests, while the memory window and the warm-point *size estimate*
    /// say nothing about a hole in the middle of a range.
    fn disk_cover(&self, offset: u64) -> (u64, usize, u64) {
        let mut disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        disk.cover(offset)
    }

    /// Whether the session cache file holds the byte at `pos`.
    fn disk_covers(&self, pos: u64) -> bool {
        match self.disk.lock() {
            Ok(disk) => disk.end_at(pos) > pos,
            Err(poisoned) => poisoned.into_inner().end_at(pos) > pos,
        }
    }

    /// Complete length the cache file already knows (`set_len` or a loaded
    /// preload map), without waiting for any upstream response.
    fn disk_total(&self) -> Option<u64> {
        let disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        (disk.len() > 0).then(|| disk.len())
    }

    /// Path of the session cache file, when the session really has one.
    fn cache_path(&self) -> Option<PathBuf> {
        let disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        disk.present().then(|| disk.path.clone())
    }

    /// Path of the session cache file when it holds at least `need` contiguous
    /// bytes at `offset` (the caller decodes the local file instead of the
    /// proxy). `None` when the run is shorter, so the proxy keeps serving.
    fn cache_path_for(&self, offset: u64, need: u64) -> Option<PathBuf> {
        let disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !disk.present() || disk.len() == 0 {
            return None;
        }
        let end = disk.end_at(offset);
        if end.saturating_sub(offset) < need.max(1) {
            return None;
        }
        Some(disk.path.clone())
    }

    /// Hand one fetched range to the session's writer. Never blocks on the disk:
    /// a full queue drops the block (it then simply is not cached), and the
    /// range only reaches the presence map once the writer's call returned.
    fn disk_write(&self, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let writer = match self.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        match writer.as_ref() {
            Some(writer) => writer.write(offset, Arc::from(data)),
            // No writer: a memory-only session (nothing to do) or a test engine.
            None => match self.disk.lock() {
                Ok(mut disk) => disk.write(offset, data),
                Err(poisoned) => poisoned.into_inner().write(offset, data),
            },
        }
    }

    /// Give the cache file the complete media length (`set_len`) the moment an
    /// upstream response states it, so the file is a valid sparse media file.
    ///
    /// The map adopts the length here (readers see it immediately); the
    /// `set_len` itself is queued to the writer. Only a full queue falls back to
    /// sizing the file inline - once per session, never on a data path.
    fn disk_ensure_len(&self, total: u64) {
        let writer = match self.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        let file = match self.disk.lock() {
            Ok(mut disk) => {
                disk.ensure_len(None, total);
                disk.file.clone()
            }
            Err(poisoned) => {
                let mut disk = poisoned.into_inner();
                disk.ensure_len(None, total);
                disk.file.clone()
            }
        };
        let queued = writer
            .as_ref()
            .map(|writer| writer.ensure_len(total))
            .unwrap_or(false);
        if !queued {
            if let Some(file) = file {
                if total > 0 && total <= CACHE_CAP_BYTES {
                    let _ = file.set_len(total);
                }
            }
        }
    }

    /// Read from the session cache file. The caller then credits the bytes with
    /// [`Engine::note_disk_served`].
    fn disk_read(&self, pos: u64, max_len: u64) -> Option<Vec<u8>> {
        let disk = match self.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        disk.read_at(pos, max_len)
    }

    /// Account a disk-served block: the read cursor moves, the map does not.
    fn note_disk_served(&self, pos: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.cursor = pos.saturating_add(bytes);
        state.served = state.served.saturating_add(bytes);
        state.disk_served = state.disk_served.saturating_add(bytes);
        state.disk_hits = state.disk_hits.saturating_add(1);
        note_serve_rate(&mut state, now, bytes);
        self.publish(&mut state, now);
    }

    fn cached_bytes_at(&self, offset: u64) -> u64 {
        let ranges = self.cached_ranges();
        match ranges.binary_search_by(|(start, _)| start.cmp(&offset)) {
            Ok(index) => ranges[index].1.saturating_sub(offset),
            Err(0) => 0,
            Err(index) => {
                let (_start, end) = ranges[index - 1];
                if offset < end {
                    end.saturating_sub(offset)
                } else {
                    0
                }
            }
        }
    }

    /// Point the read cursor at `offset` and tell the caller whether the bytes
    /// there were already buffered (a window hit).
    ///
    /// A seek never throws buffered bytes away. The buffered window and the
    /// point cache belong to the whole stream, and the whole-file scheduler
    /// keeps fetching independently of the cursor, so a seek to an offset that
    /// is already in memory is served with no upstream request at all. Only
    /// bytes *behind* the new cursor are dropped: they can never be served
    /// again. The generation is left alone for the same reason: in-flight
    /// requests are still useful to the whole-file queue.
    fn seek(&self, offset: u64) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let covered = chunks_cover(&state.chunks, offset);
        if state.cursor == offset && (state.total == Some(offset) || covered) {
            // Repeated read at the position the cursor already sits on.
            return covered;
        }
        state.cursor = offset;
        state.base = offset;
        // A seek ends the previous wait: an offset the old body was blocked on
        // is not what this client reads any more.
        state.urgent.clear();
        // Drop only what the new cursor left behind.
        state.chunks.retain(|start, data| {
            let end = start.saturating_add(data.len() as u64);
            end > offset
        });
        state.read_ahead = state.chunks.values().map(|data| data.len() as u64).sum();
        drop(state);
        crate::debug_log(&format!(
            "prefetch seek cursor={offset} hit={covered} after_ms={}",
            self.started.elapsed().as_millis()
        ));
        covered
    }

    /// Take ownership of the proxy for one response. The returned token is what
    /// [`Engine::is_current`] compares; the newest request always wins.
    fn begin_response(&self) -> u64 {
        self.serve_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether `epoch` still owns the proxy (no newer request arrived).
    fn is_current(&self, epoch: u64) -> bool {
        self.serve_epoch.load(Ordering::SeqCst) == epoch
    }

    /// Take the next chunk worth writing at `pos`, limited to `max_len`.
    fn consume(&self, pos: u64, max_len: u64) -> Result<Option<Vec<u8>>, String> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Buffered bytes are served even after every upstream host failed: a
        // cache/window seek must never depend on the network being healthy.
        let Some(data) = take_bounded(&mut state.chunks, pos, max_len) else {
            if let Some(error) = &state.fatal {
                return Err(error.clone());
            }
            if state.dead.contains(&(state.generation, pos)) {
                return Err(format!("区间 {pos} 无法从任何主机获取"));
            }
            return Ok(None);
        };
        let take = data.len() as u64;
        let now = Instant::now();
        state.cursor = pos + take;
        state.served += take;
        state.read_ahead = state.read_ahead.saturating_sub(take);
        // What the client pulled over the whole session, not over the last
        // second: with no duration to divide by this is the media bitrate, and
        // a one-second window read a startup burst (2 MiB in 50 ms = 40 MB/s)
        // as the bitrate, which then multiplied every scheduling horizon.
        note_serve_rate(&mut state, now, take);
        self.publish(&mut state, now);
        Ok(Some(data))
    }

    /// Why a byte at `pos` can never arrive, if that is already known: the
    /// upstream pool failed fatally or the chunk was retired on every host.
    /// Used by the serve path only *after* the memory window and the session
    /// cache file both missed, so a cached range never inherits a network error.
    fn consume_error(&self, pos: u64) -> Result<(), String> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(error) = &state.fatal {
            return Err(error.clone());
        }
        if state.dead.contains(&(state.generation, pos)) {
            return Err(format!("区间 {pos} 无法从任何主机获取"));
        }
        Ok(())
    }

    /// Whether the file ends exactly at `pos` (the response should stop).
    fn at_eof(&self, pos: u64) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.total == Some(pos)
    }

    /// Whether the complete length is known yet.
    fn total_known(&self) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.total.is_some()
    }

    /// The complete media length, once an upstream response has stated it.
    fn total(&self) -> Option<u64> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.total
    }

    /// Bytes buffered ahead of the read cursor right now (diagnostics).
    fn read_ahead(&self) -> u64 {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.read_ahead
    }

    /// Retire a range that a `416` proved to be past EOF while the total was
    /// still unknown. The response only says "at or before this offset", so it
    /// must not become the total (the first real `206` states that exactly);
    /// the range is still marked dead so it is not requested again.
    fn mark_range_beyond_end(&self, generation: u64, chunk_start: u64, shutdown: &AtomicBool) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.inflight.remove(&(generation, chunk_start));
        if generation != state.generation || shutdown.load(Ordering::SeqCst) {
            return;
        }
        state.dead.insert((generation, chunk_start));
    }

    /// Store one network read at its absolute offset. Bytes become servable
    /// immediately, so the first response does not wait for a whole chunk.
    ///
    /// The read is only *retained* inside the read-ahead window around the
    /// current cursor. Whole-file scheduling deliberately fetches far past that
    /// window; those bytes are counted (throughput, health, host split) and
    /// then dropped, which is what keeps the window bounded to
    /// [`PrefetchConfig::read_ahead_bytes`] no matter how far ahead the queue
    /// is.
    ///
    /// Returns `false` when the read belongs to a superseded generation or the
    /// proxy is shutting down: the caller stops reading this response instead
    /// of draining a body nobody can use.
    fn push_bytes(
        &self,
        generation: u64,
        offset: u64,
        data: &[u8],
        host: usize,
        worker: usize,
        kind: ChunkKind,
        shutdown: &AtomicBool,
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if generation != state.generation || shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let len = data.len() as u64;
        let limit = self.read_ahead_window();
        let cursor = state.cursor;
        let window_end = cursor.saturating_add(limit);
        let retain = offset < window_end && offset.saturating_add(len) > cursor;
        if retain {
            if let Some(old) = state.chunks.insert(offset, data.to_vec()) {
                state.read_ahead = state.read_ahead.saturating_sub(old.len() as u64);
            }
            state.read_ahead = state.read_ahead.saturating_add(len);
            trim_window(&mut state, limit);
        }
        state.bytes_fetched += len;
        match kind {
            ChunkKind::Hot => state.hot_bytes = state.hot_bytes.saturating_add(len),
            ChunkKind::Warm => {
                state.warm_fetch_bytes = state.warm_fetch_bytes.saturating_add(len)
            }
            ChunkKind::Rest => state.rest_bytes = state.rest_bytes.saturating_add(len),
            ChunkKind::Requeue | ChunkKind::Race => {
                state.retry_bytes = state.retry_bytes.saturating_add(len)
            }
        }
        if let Some(host) = state.hosts.get_mut(host) {
            host.bytes += len;
            // Bytes arrived from this route: it works. Clears the failure
            // streak `mark_host_failure` recorded (see its doc comment).
            if host.failures > 0 || host.failed {
                host.failures = 0;
                host.failed = false;
                host.degraded_until = None;
            }
        }
        let mut recovered = None;
        if let Some(slot) = state.workers.get_mut(worker) {
            slot.bytes = slot.bytes.saturating_add(len);
            slot.samples.push_back((now, len));
            if slot.slow {
                slot.slow = false;
                recovered = Some((worker, slot.host.clone()));
            }
        }
        state.samples.push_back((now, len));
        self.publish(&mut state, now);
        drop(state);
        // The file is the truth for "downloaded": every fetched byte is
        // persisted, whether or not the memory window keeps a copy of it. The
        // write happens outside the state lock so the serve path never waits on
        // a disk write.
        self.disk_write(offset, data);
        if let Some((worker, host)) = recovered {
            crate::debug_log(&format!("worker recover id={worker} host={host}"));
        }
        true
    }

    /// Store the total length / content type learned from a real range
    /// response. The first response wins; later ones only fill a gap, so a
    /// mirror with an odd `Content-Range` cannot rewrite the length mid-stream.
    fn learn_metadata(&self, learned: &Learned, shutdown: &AtomicBool) {
        if learned.total.is_none() && learned.content_type.is_none() {
            return;
        }
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut learned_total = None;
        let mut changed = false;
        if state.total.is_none() {
            if let Some(total) = learned.total {
                state.total = Some(total);
                learned_total = Some(total);
                changed = true;
            }
        }
        if state.content_type.is_none() {
            if let Some(content_type) = &learned.content_type {
                state.content_type = Some(content_type.clone());
                changed = true;
            }
        }
        // The warm plan needs the complete length; build it the moment it is
        // known so the workers can put seek points ahead of the rest.
        let mut warm_plan = None;
        if !state.warm_plan_started {
            if let Some(total) = state.total {
                build_warm_plan(
                    &mut state,
                    self.config.chunk_bytes,
                    self.config.point_bytes,
                    self.config.warm_points,
                    total,
                );
                warm_plan = Some((total, state.warm_points_total, state.warm_chunk));
            }
        }
        if changed || warm_plan.is_some() {
            self.publish(&mut state, now);
        }
        drop(state);
        // A known length makes the cache file a sparse file of exactly the right
        // size (`set_len`), which is what a local-file decode needs.
        if let Some(total) = learned_total {
            self.disk_ensure_len(total);
            self.refresh_media_bps(total);
            crate::debug_log(&format!(
                "prefetch learned total={total} media_bps={} duration_ms={} window={} after_ms={}",
                self.media_bps(),
                self.config.media_duration_ms.map_or(0, |ms| ms),
                self.read_ahead_window(),
                self.started.elapsed().as_millis()
            ));
        }
        if let Some((total, points, chunk)) = warm_plan {
            crate::debug_log(&format!(
                "seek warm plan total={total} points={points} chunk={chunk}"
            ));
        }
    }

    /// Release the range claim after the upstream body ended (possibly short).
    ///
    /// A body that stops before the requested end is **not** evidence about the
    /// file's length once a real `Content-Range` has stated it: rewriting the
    /// total here is what closed the serving body at that offset ("Stream ends
    /// prematurely at N, should be M", then EOF, then the "playback ended"
    /// machinery). The length from the response headers is authoritative; a
    /// short body sends the range back to the queue so another attempt can
    /// fetch the rest, and only a range that keeps stopping early is retired.
    ///
    /// Returns `true` when the body stopped short against a known length.
    fn complete_range(
        &self,
        generation: u64,
        chunk_start: u64,
        next: u64,
        end: u64,
        shutdown: &AtomicBool,
    ) -> bool {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if generation != state.generation || shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let mut learned = None;
        let mut short_against_total = None;
        if next < end {
            match state.total {
                None => {
                    // Short body with no known length: this is the only way to
                    // learn the length here, so it is still EOF evidence.
                    state.total = Some(next);
                    learned = Some(next);
                    crate::debug_log(&format!(
                        "prefetch eof total={} after_ms={}",
                        next,
                        self.started.elapsed().as_millis()
                    ));
                }
                Some(total) if next >= total => {}
                Some(total) => {
                    // Known length, body stopped early: a truncated range, not
                    // a file end. Requeue it a bounded number of times.
                    let attempts = state
                        .short_bodies
                        .entry(chunk_start)
                        .and_modify(|count| *count = count.saturating_add(1))
                        .or_insert(1);
                    let attempts = *attempts;
                    let retired = attempts >= SHORT_BODY_LIMIT;
                    if retired {
                        state.dead.insert((generation, chunk_start));
                    } else {
                        state.requeued.insert(chunk_start);
                    }
                    short_against_total = Some((next, total, attempts, retired));
                }
            }
        }
        self.publish(&mut state, now);
        drop(state);
        if let Some(total) = learned {
            self.disk_ensure_len(total);
            self.refresh_media_bps(total);
        }
        if let Some((next, total, attempts, retired)) = short_against_total {
            crate::debug_log(&format!(
                "prefetch range short start={chunk_start} next={next} total={total} attempt={attempts} action={}",
                if retired { "retire" } else { "requeue" }
            ));
            return true;
        }
        false
    }

    /// Record one failed range attempt on a host.
    ///
    /// This used to set `failed = true` on the first error — with a single
    /// mirror (the normal Bilibili case) one transient `error decoding response
    /// body` mid-range retired the only route, the range was marked dead, and
    /// the serving body closed a few hundred milliseconds later: the client saw
    /// a truncated stream and the player's "ended" machinery took over. Now a
    /// failure only cools the route down; the first byte it stores later
    /// ([`Engine::note_host_ok`]) clears the count.
    fn mark_host_failure(&self, host: usize, error: &str) {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut retired = false;
        if let Some(entry) = state.hosts.get_mut(host) {
            entry.failures = entry.failures.saturating_add(1);
            if entry.failures >= HOST_FAILURE_LIMIT {
                entry.failed = true;
                retired = true;
            } else {
                // Linear cooldown: 1 s, 2 s, then capped. Short enough that a
                // single-host config is usable again almost immediately.
                let cooldown = Duration::from_secs(u64::from(entry.failures).min(3));
                entry.degraded_until = Some(now + cooldown);
            }
        }
        state.failed_ranges += 1;
        self.publish(&mut state, now);
        let failures = state.hosts.get(host).map_or(0, |entry| entry.failures);
        drop(state);
        crate::debug_log(&format!(
            "prefetch host failure host={host} failures={failures} retired={retired} err={error} after_ms={}",
            self.started.elapsed().as_millis()
        ));
    }

    /// Give up on a range: the bytes at `next` will never arrive, so a reader
    /// waiting there gets an error. Stops every task only when every host has
    /// been retired after repeated failures.
    fn fail_range(
        &self,
        generation: u64,
        chunk_start: u64,
        next: u64,
        attempted: &[usize],
        error: &str,
        shutdown: &AtomicBool,
    ) {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.inflight.remove(&(generation, chunk_start));
        if generation != state.generation || shutdown.load(Ordering::SeqCst) {
            return;
        }
        for index in attempted {
            if let Some(host) = state.hosts.get_mut(*index) {
                host.failures = host.failures.saturating_add(1);
                if host.failures >= HOST_FAILURE_LIMIT {
                    host.failed = true;
                }
            }
        }
        state.dead.insert((generation, next));
        if state.hosts.iter().all(|host| host.failed) {
            state.fatal = Some(format!("所有上游主机均失败：{error}"));
        }
        self.publish(&mut state, now);
        drop(state);
        crate::debug_log(&format!(
            "prefetch range failed start={chunk_start} next={next} attempted={attempted:?} err={error}"
        ));
    }

    /// Next host that is neither failed nor already attempted, excluding the
    /// current one. Holds the state lock only for the lookup.
    fn next_host(&self, exclude: &[usize]) -> Option<(usize, String, String)> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let index = pick_host(&mut state, exclude)?;
        let host = state.hosts[index].clone();
        Some((index, host.origin, host.label))
    }

    /// Complete length plus the upstream MIME type, once both/one are known.
    fn metadata(&self) -> Option<(u64, Option<String>)> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .total
            .map(|total| (total, state.content_type.clone()))
    }

    /// Bytes the current read-ahead window still holds at `pos`.
    fn window_covers(&self, pos: u64) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        chunks_cover(&state.chunks, pos)
    }

    /// Point-cache entry containing `pos`, if any. The returned bytes are
    /// shared, so the caller reads them without holding the state lock.
    fn point_lookup(&self, pos: u64) -> Option<(u64, Arc<Vec<u8>>, usize)> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.points.lookup(pos)
    }

    /// Record a request answered from the point cache and log the QA line.
    fn note_point_hit(&self, offset: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let now = Instant::now();
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.point_hits = state.point_hits.saturating_add(1);
            state.point_served = state.point_served.saturating_add(bytes);
            self.publish(&mut state, now);
        }
        crate::debug_log(&format!("seek warm hit offset={offset} bytes={bytes}"));
    }

    /// The configured `i/count` offset of the warm task that owns `start`.
    fn warm_point_at(&self, start: u64) -> Option<u64> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .warm_plan
            .iter()
            .find(|task| task.start == start)
            .map(|task| task.point)
    }

    /// Store one warm point (chunk-aligned data) in the bounded point cache and
    /// publish the QA line as it lands.
    fn store_point(&self, offset: u64, data: Vec<u8>, point: u64) {
        let len = data.len() as u64;
        if len == 0 {
            return;
        }
        // A warm point is fetched outside `push_bytes`, so it is persisted here,
        // before the memory copy: the seek it serves must also work from disk.
        self.disk_write(offset, &data);
        let now = Instant::now();
        let (done, points, bytes, chunk, cached, cached_points, total) = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            for task in &mut state.warm_plan {
                if task.start == offset {
                    task.done = true;
                    task.claimed = true;
                }
            }
            let count = state.warm_plan.len().min(MAX_WARM_POINTS).max(1);
            state
                .points
                .insert(offset, data, count, MAX_POINT_CACHE_BYTES);
            state.warm_points_done = state.warm_points_done.saturating_add(1);
            state.warm_bytes = state.warm_bytes.saturating_add(len);
            self.publish(&mut state, now);
            (
                state.warm_points_done,
                state.warm_points_total,
                state.warm_bytes,
                state.warm_chunk,
                state.points.bytes,
                state.points.len(),
                state.total.unwrap_or(0),
            )
        };
        crate::debug_log(&format!(
            "seek warm total={total} points={points} chunk={chunk} done={done} bytes={bytes}"
        ));
        crate::debug_log(&format!(
            "seek warm point offset={point} cached_at={offset} bytes={len} host=worker"
        ));
        if done >= points {
            crate::debug_log(&format!(
                "seek warm done total={total} points={points} chunk={chunk} done={done} bytes={bytes} cached={cached} points_cached={cached_points} ms={}",
                self.started.elapsed().as_millis()
            ));
        }
    }

    /// Account a warm-point network read: it is upstream traffic and counts for
    /// the host split and the worker's health, but it is not part of the
    /// read-ahead window.
    fn note_warm_bytes(&self, host: usize, worker: usize, len: u64, shutdown: &AtomicBool) {
        if len == 0 || shutdown.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.bytes_fetched = state.bytes_fetched.saturating_add(len);
        state.warm_fetch_bytes = state.warm_fetch_bytes.saturating_add(len);
        if let Some(host) = state.hosts.get_mut(host) {
            host.bytes = host.bytes.saturating_add(len);
        }
        let mut recovered = None;
        if let Some(slot) = state.workers.get_mut(worker) {
            slot.bytes = slot.bytes.saturating_add(len);
            slot.samples.push_back((now, len));
            if slot.slow {
                slot.slow = false;
                recovered = Some((worker, slot.host.clone()));
            }
        }
        state.samples.push_back((now, len));
        self.publish(&mut state, now);
        drop(state);
        if let Some((worker, host)) = recovered {
            crate::debug_log(&format!("worker recover id={worker} host={host}"));
        }
    }

    /// Retire a claim: the chunk is fetched, its range leaves the inflight set,
    /// and a raced chunk is resolved in favour of the first finisher. Returns
    /// `true` when this worker won a race (the caller logs the winner line).
    fn finish_chunk(
        &self,
        generation: u64,
        start: u64,
        kind: ChunkKind,
        worker: usize,
        race: Option<&Arc<AtomicBool>>,
    ) -> bool {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if generation == state.generation {
            state.done.insert(start);
            state.inflight.remove(&(generation, start));
        }
        let mut won = false;
        if let Some(token) = race {
            won = !token.swap(true, Ordering::SeqCst);
            if won {
                state.race_tokens.remove(&start);
            }
        }
        if kind == ChunkKind::Race && !won {
            // The loser stops here; the winner owns the chunk.
            state.inflight.remove(&(generation, start));
        }
        if let Some(slot) = state.workers.get_mut(worker) {
            slot.busy = false;
            slot.range_started = None;
        }
        self.publish(&mut state, now);
        won
    }

    /// Return a claim that produced no useful bytes to the queue. A slow
    /// worker's range stays owned while a duplicate race is running; the race
    /// finishes it instead.
    fn release_range(
        &self,
        generation: u64,
        start: u64,
        kind: ChunkKind,
        race: Option<&Arc<AtomicBool>>,
    ) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let race_won = race.is_some_and(|token| token.load(Ordering::SeqCst));
        let racer_running = race.is_none() && state.race_tokens.contains_key(&start);
        if kind == ChunkKind::Race && !race_won {
            // The duplicate failed; drop its token so the original route keeps
            // the range (a later health tick may race it again).
            state.race_tokens.remove(&start);
            state.inflight.remove(&(generation, start));
            state.requeued.insert(start);
            if let Some(task) = state.warm_plan.iter_mut().find(|task| task.start == start) {
                task.claimed = false;
            }
            return;
        }
        if race_won {
            return;
        }
        if !racer_running {
            state.inflight.remove(&(generation, start));
            if kind == ChunkKind::Warm {
                if let Some(task) = state.warm_plan.iter_mut().find(|task| task.start == start) {
                    task.claimed = false;
                }
            } else {
                state.requeued.insert(start);
            }
        }
    }

    /// Whether the health watchdog marked a worker slow.
    fn worker_is_slow(&self, worker: usize) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.workers.get(worker).is_some_and(|slot| slot.slow)
    }

    /// Last measured throughput of one worker, bytes/s.
    fn worker_bps(&self, worker: usize) -> u64 {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.workers.get(worker).map_or(0, |slot| slot.bps)
    }
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

struct FetchClaim {
    generation: u64,
    start: u64,
    end: u64,
    host: usize,
    kind: ChunkKind,
    /// Cancellation token of the worker at claim time (bumped by the health
    /// watchdog when the worker is slow).
    token: u64,
    /// Set for a duplicate race: the first finisher wins, the other cancels.
    race: Option<Arc<AtomicBool>>,
    /// Host labels of a race, for the winner log.
    race_hosts: String,
}

enum Claim {
    Fetch(FetchClaim),
    Wait,
    Stop,
}

/// Pick the next healthy host round-robin, excluding already-tried indices.
///
/// A route the watchdog found slow is skipped while it is degraded; if that
/// leaves nothing (a single-host config, or every mirror degraded), the round
/// robin falls back to the reachable hosts instead of failing playback.
fn pick_host(state: &mut State, exclude: &[usize]) -> Option<usize> {
    let count = state.hosts.len();
    let now = Instant::now();
    for pass in 0..2 {
        for step in 0..count {
            let index = (state.next_host + step) % count;
            let host = &state.hosts[index];
            if host.failed || exclude.contains(&index) {
                continue;
            }
            if pass == 0 && host.degraded_until.is_some_and(|until| until > now) {
                continue;
            }
            state.next_host = (index + 1) % count;
            return Some(index);
        }
    }
    None
}

/// Skip a route the health watchdog found slow for a cooldown period. It stays
/// in the mirror list, so a single-host config keeps using it.
fn degrade_host(state: &mut State, host: usize) {
    if state.hosts.len() <= 1 {
        return;
    }
    if let Some(host) = state.hosts.get_mut(host) {
        host.degraded_until = Some(Instant::now() + Duration::from_secs(30));
    }
}

/// Chunk start for `offset`: all chunk offsets are absolute multiples of the
/// configured chunk size, independent of the cursor.
fn chunk_start(offset: u64, chunk: u64) -> u64 {
    if chunk == 0 {
        return 0;
    }
    offset - offset % chunk
}

/// Whether a chunk start is already spoken for: **on disk**, buffered, in
/// flight, dead, or fetched earlier and evicted.
///
/// The session cache file is part of "already have": a chunk it holds must never
/// go upstream again, whatever the memory window did with it.
fn chunk_taken(state: &State, generation: u64, start: u64, chunk: u64, disk: &DiskCache) -> bool {
    let end = state
        .total
        .map_or(start.saturating_add(chunk), |total| total.min(start.saturating_add(chunk)));
    disk.covers_range(start, end)
        || state.done.contains(&start)
        || state.inflight.contains(&(generation, start))
        || state.dead.contains(&(generation, start))
        || chunks_cover(&state.chunks, start)
        || chunks_cover(&state.chunks, start.saturating_add(chunk.saturating_sub(1)))
}

/// Whether a chunk the client is reading is already available. Unlike
/// [`chunk_taken`], a chunk fetched earlier by the speculative whole-file queue
/// and then discarded (it was outside the window at the time) is *not*
/// available: playback must fetch it again.
///
/// The chunk that *contains* the cursor is special: bytes the client has
/// already pulled are removed from the map as they are served, so coverage of
/// that chunk has to be probed **at the cursor**. Probing at the chunk start
/// said "missing" on every tick and re-downloaded the whole chunk every few
/// seconds (measured: 577 MB fetched for a 27.6 MB file).
fn hot_taken(state: &State, generation: u64, start: u64, chunk: u64, disk: &DiskCache) -> bool {
    let probe = start.max(state.cursor);
    let end = state
        .total
        .map_or(start.saturating_add(chunk), |total| total.min(start.saturating_add(chunk)));
    // A chunk the cache file already holds is as good as buffered: re-fetching
    // it was the 12 MiB of upstream traffic a cached seek used to cost.
    disk.covers_range(start, end)
        || state.inflight.contains(&(generation, start))
        || state.dead.contains(&(generation, start))
        || chunks_cover(&state.chunks, probe)
}

/// Byte frontier the client is expected to consume within
/// [`HOT_AHEAD_SECONDS`], from the bitrate estimate alone.
///
/// It deliberately does **not** use the one-second serve rate: after a burst
/// that rate reads tens of MB/s and the frontier becomes the whole file, which
/// is how the speculative queue used to pull 66 MB/s. A stalled client cannot
/// shrink the frontier either — the bitrate is a property of the stream.
fn hot_frontier(state: &mut State, chunk: u64, bitrate: u64, _now: Instant) -> u64 {
    let ahead = bitrate
        .saturating_mul(HOT_AHEAD_SECONDS)
        .max(chunk.saturating_mul(2));
    state.cursor.saturating_add(ahead)
}

/// Record served bytes and keep the session-average serve rate up to date.
///
/// `serve_samples` still feeds the panel's one-second rate; `serve_bps` is the
/// *average since the first byte*, which is the honest fallback bitrate when
/// the caller did not supply the media duration.
fn note_serve_rate(state: &mut State, now: Instant, bytes: u64) {
    state.serve_samples.push_back((now, bytes));
    let _ = window_rate(&mut state.serve_samples, now, RATE_WINDOW);
    let first = *state.first_serve.get_or_insert(now);
    let elapsed = now.saturating_duration_since(first).as_secs_f64();
    state.serve_bps = if elapsed < 0.5 {
        0
    } else {
        (state.served as f64 / elapsed).round() as u64
    };
}

/// QA escape hatch: `BILI_PREFETCH_SPEC_RATE=<bytes/s>` pins the speculative
/// budget instead of deriving it, so the same binary can reproduce a previous
/// policy on the same CDN moment (the before/after table in the report used
/// `462000`, the old 2× bitrate of the test stream). Read once.
fn spec_rate_override() -> Option<u64> {
    static RATE: OnceLock<Option<u64>> = OnceLock::new();
    *RATE.get_or_init(|| {
        std::env::var("BILI_PREFETCH_SPEC_RATE")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|rate| *rate > 0)
    })
}

/// QA escape hatch: `BILI_PREFETCH_BURST_CHUNKS=<n>` pins the speculative burst
/// to `n × chunk` (the old policy used two), same purpose as
/// [`spec_rate_override`].
fn spec_burst_override() -> Option<u64> {
    static CHUNKS: OnceLock<Option<u64>> = OnceLock::new();
    *CHUNKS.get_or_init(|| {
        std::env::var("BILI_PREFETCH_BURST_CHUNKS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|chunks| *chunks > 0)
    })
}

/// How far ahead of the playhead the whole-file queue may run, in media
/// seconds. `BILI_PREFETCH_HORIZON_S` reproduces the previous
/// [`PLAYBACK_AHEAD_SECONDS`] queue for an A/B run; normal runs use
/// [`PREFETCH_AHEAD_SECONDS`].
fn prefetch_horizon_seconds() -> u64 {
    static HORIZON: OnceLock<u64> = OnceLock::new();
    *HORIZON.get_or_init(|| {
        std::env::var("BILI_PREFETCH_HORIZON_S")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(PREFETCH_AHEAD_SECONDS)
    })
}

/// QA escape hatch: `BILI_PREFETCH_CEILING_BPS=<bytes/s>` pins the sustained
/// speculative ceiling (and therefore how many streams the bucket can feed), so
/// the same binary can compare 16 / 48 / 64 MiB/s on the same CDN moment.
fn spec_ceiling() -> u64 {
    static CEILING: OnceLock<u64> = OnceLock::new();
    *CEILING.get_or_init(|| {
        std::env::var("BILI_PREFETCH_CEILING_BPS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|rate| *rate > 0)
            .unwrap_or(SPECULATIVE_CEILING_BPS)
    })
}

/// Refill rate of the speculative token bucket: [`SPECULATIVE_RATE_FACTOR`] ×
/// the bitrate for the whole-file queue, [`WARM_RATE_FACTOR`] × while seek
/// points are still being warmed, but never less than
/// [`MIN_SPECULATIVE_CONNECTION_BPS`] per live connection (so a configured
/// worker is never starved by the bucket) and never more than
/// [`SPECULATIVE_CEILING_BPS`].
fn speculative_rate(bitrate: u64, warm_pending: bool, warmup: bool) -> u64 {
    if let Some(rate) = spec_rate_override() {
        return rate;
    }
    let ceiling = spec_ceiling();
    if warmup {
        // The opening keeps the old budget: a couple of streams' worth, never
        // a burst (see [`SPECULATIVE_WARMUP`]).
        let factor = if warm_pending {
            WARM_RATE_FACTOR
        } else {
            SPECULATIVE_RATE_FACTOR
        };
        return bitrate
            .saturating_mul(factor)
            .max(MIN_SPECULATIVE_CONNECTION_BPS)
            .min(ceiling);
    }
    // Steady state: the *ceiling* is the sustained bound (CPU, not bandwidth),
    // and the live connection count is the parallelism bound. Tying the rate to
    // `per connection × live` made a settled two-stream session crawl at
    // 4 MiB/s: the floor that was meant to stop starvation became the cap.
    ceiling
}

/// One claim's worth of burst allowance per live connection (see
/// [`SPECULATIVE_BURST_CHUNKS_PER_CONNECTION`]).
fn speculative_burst(chunk: u64, live: usize, warmup: bool) -> u64 {
    if let Some(chunks) = spec_burst_override() {
        return chunk.saturating_mul(chunks);
    }
    // One claim per live connection, with a two-chunk floor so a single-worker
    // session (or a one-chunk file) can always start. During the opening only
    // that floor applies: two chunks, the old behaviour, until the player is
    // steady (see [`SPECULATIVE_WARMUP`]).
    let live = if warmup {
        live.min(GOVERNOR_MIN_CONNECTIONS)
    } else {
        live
    };
    chunk
        .saturating_mul((live.max(1) as u64).max(2))
        .saturating_mul(SPECULATIVE_BURST_CHUNKS_PER_CONNECTION)
}

/// Take `bytes` worth of speculative bandwidth. `false` means "do not start
/// this fetch now" — the worker goes idle and the bytes are handed to the
/// client path, which is never limited.
fn spec_take(state: &mut State, bytes: u64, rate: u64, burst: u64) -> bool {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(state.spec_at);
    if !elapsed.is_zero() {
        let refill = (rate as u128)
            .saturating_mul(elapsed.as_millis())
            .saturating_div(1000)
            .min(u64::MAX as u128) as u64;
        state.spec_tokens = state.spec_tokens.saturating_add(refill).min(burst);
        state.spec_at = now;
    }
    if state.spec_tokens >= bytes {
        state.spec_tokens -= bytes;
        state.spec_bytes = state.spec_bytes.saturating_add(bytes);
        true
    } else {
        false
    }
}

/// Build the warm plan once the total length is known. Points are aligned down
/// to the chunk grid so a point is a whole scheduling unit and can share the
/// same worker pool as playback.
fn build_warm_plan(state: &mut State, chunk: u64, point_bytes: u64, points: usize, total: u64) {
    if state.warm_plan_started || points == 0 || chunk == 0 || total == 0 {
        return;
    }
    state.warm_plan_started = true;
    let points = points.min(MAX_WARM_POINTS);
    let per_point = point_chunk_bytes(total, points, point_bytes).max(1);
    let mut seen = BTreeSet::new();
    let mut plan = Vec::new();
    for point in warm_point_offsets(total, points) {
        let start = chunk_start(point, chunk);
        if !seen.insert(start) {
            continue;
        }
        let end = total.min(start.saturating_add(per_point.max(1)));
        plan.push(WarmTask {
            point,
            start,
            end,
            done: false,
            claimed: false,
            urgent: false,
        });
    }
    state.warm_points_total = plan.len() as u32;
    state.warm_points_done = 0;
    state.warm_chunk = per_point;
    state.warm_bytes = 0;
    state.points = PointCache::default();
    state.warm_plan = plan;
}

/// Split `[start, total)` into `connections` equal, chunk-aligned parts. The
/// rest of the file is then fetched part-per-worker instead of one global FIFO,
/// so every connection owns a slice and the tail is reached quickly.
fn build_parts(state: &mut State, connections: usize, chunk: u64) {
    if state.parts_built {
        return;
    }
    let Some(total) = state.total else {
        return;
    };
    state.parts_built = true;
    let start = chunk_start(state.cursor, chunk);
    let span = total.saturating_sub(start);
    if span < DYNAMIC_SPLIT_THRESHOLD || chunk == 0 {
        return;
    }
    let count = connections.clamp(1, MAX_CONNECTIONS) as u64;
    let per = span.div_ceil(count).div_ceil(chunk).saturating_mul(chunk).max(chunk);
    let mut cursor = start;
    let mut index = 0usize;
    while cursor < total && index < MAX_CONNECTIONS {
        let end = cursor.saturating_add(per).min(total);
        state.parts.push(Part {
            end,
            next: cursor,
            owner: Some(index),
        });
        cursor = end;
        index += 1;
    }
}

/// Take the next sub-chunk of one part, skipping chunks that are already
/// buffered, in flight or dead. Nothing at or past `limit` is claimable: the
/// far end of a part stays unclaimed until the playhead moves the download
/// horizon forward.
fn take_part_chunk(
    state: &mut State,
    index: usize,
    generation: u64,
    chunk: u64,
    disk: &DiskCache,
    limit: u64,
) -> Option<u64> {
    if chunk == 0 {
        return None;
    }
    loop {
        let next = state.parts.get(index)?.next;
        let end = state.parts.get(index)?.end.min(limit);
        if next >= end {
            return None;
        }
        let start = chunk_start(next, chunk);
        if start >= end {
            // The part continues past the horizon: leave `next` alone so it
            // resumes when the horizon moves, and report "nothing here now".
            return None;
        }
        if let Some(part) = state.parts.get_mut(index) {
            part.next = start.saturating_add(chunk);
        }
        if !chunk_taken(state, generation, start, chunk, disk) {
            return Some(start);
        }
    }
}

/// Take one sub-chunk from the part with the most unclaimed bytes. A worker
/// whose own part is exhausted keeps working by shrinking the largest one, so
/// no connection ever sits idle while bytes remain.
fn steal_chunk(
    state: &mut State,
    generation: u64,
    chunk: u64,
    disk: &DiskCache,
    limit: u64,
) -> Option<u64> {
    loop {
        let mut best = None;
        let mut best_remaining = 0u64;
        for index in 0..state.parts.len() {
            let part = &state.parts[index];
            // Only the bytes inside the download horizon count as available
            // work; a part that is entirely past it must not look like the
            // largest one and starve the parts the playhead can reach.
            let remaining = part.end.min(limit).saturating_sub(part.next);
            if remaining > best_remaining {
                best_remaining = remaining;
                best = Some(index);
            }
        }
        let index = best?;
        if let Some(start) = take_part_chunk(state, index, generation, chunk, disk, limit) {
            return Some(start);
        }
        // The largest claimable part had no free chunk (all covered); retire it
        // for this pass, but never past the horizon.
        if let Some(part) = state.parts.get_mut(index) {
            part.next = part.end.min(limit).max(part.next);
        }
    }
}

/// Keep the live connection count at the measured throughput knee.
///
/// A Bilibili mirror throttles per stream, so "use all sixteen" is not the same
/// as "fastest": measured on one mirror, 1 connection pulled 22 MB/s, 4 pulled
/// 19 MB/s and 16 pulled 8–10 MB/s. The session therefore starts at
/// [`STARTUP_CONNECTIONS`] and this loop — once per [`GOVERNOR_TICK`] — first
/// **explores up to the configured count** (doubling every decision window, so
/// the setting is measured instead of assumed), then doubles/halves while the
/// measured aggregate keeps improving and settles on the count that measured
/// best. Bounds: at least [`GOVERNOR_MIN_CONNECTIONS`], at most configured.
///
/// It never raises the count while the client is waiting for bytes (a *slow*
/// serve wait in the window halves it at once): playback is what the pool must
/// not slow down, and a wait that cost the client 250 ms or more is the only
/// honest signal that it did. Short waits are counted and logged, not acted on.
///
/// The same tick logs the once-per-second-class line the QA reads:
/// `prefetch fetch active=N inflight=M live=K configured=C agg=…/s
/// per=[id:…] waits=W slow=S`.
async fn connection_governor(engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    // Without the startup gate the session starts at the configured count and
    // the governor still trims it to the measured knee (a configured 16 on a
    // throttling mirror is sixteen slow streams, not a fast one).
    let configured = engine.configured_connections.clamp(1, MAX_CONNECTIONS as u64) as usize;
    if configured <= GOVERNOR_MIN_CONNECTIONS {
        return;
    }
    let mut best_agg = 0u64;
    let mut best_live = engine.desired_connections();
    let mut settled = false;
    // Whether the configured count has been reached once (see "explore" below).
    let mut explored = engine.desired_connections() >= configured;
    // Consecutive decision windows in which throughput looked collapsed. One
    // quiet window is a chunk landing on either side of the sample, not a
    // reason to re-open sixteen streams; three in a row is.
    let mut slow_windows = 0u32;
    let mut last_waits = engine.serve_waits.load(Ordering::Relaxed);
    let mut last_slow = engine.serve_wait_slow.load(Ordering::Relaxed);
    // Throughput *of this tick*, from the fetched-byte counter: the per-worker
    // five-second windows overlap and keep a finished worker's last rate, which
    // made the first version oscillate 2→16→2 on noise.
    let mut last_bytes = engine.snapshot().bytes_fetched;
    let mut last_at = Instant::now();
    let mut dec_bytes = last_bytes;
    let mut dec_at = last_at;
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(GOVERNOR_TICK).await;
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tick = tick.saturating_add(1);
        let decide = tick % 2 == 0;
        let health = engine.worker_health_snapshot();
        let live = engine.desired_connections();
        let stats = engine.snapshot();
        let active = stats.active_workers as usize;
        let inflight = stats.inflight;
        let now = Instant::now();
        let elapsed_ms = now.saturating_duration_since(last_at).as_millis().max(1) as u64;
        let agg = stats
            .bytes_fetched
            .saturating_sub(last_bytes)
            .saturating_mul(1000)
            .saturating_div(elapsed_ms);
        last_bytes = stats.bytes_fetched;
        last_at = now;
        let waits = engine.serve_waits.load(Ordering::Relaxed);
        let new_waits = waits.saturating_sub(last_waits);
        last_waits = waits;
        // Only a wait that actually cost the client time steps the pool down;
        // a few-millisecond miss is the normal read, not a starved pool.
        let slow = engine.serve_wait_slow.load(Ordering::Relaxed);
        let new_slow = slow.saturating_sub(last_slow);
        last_slow = slow;
        let per = health
            .iter()
            .map(|(id, _, _, bps)| format!("{id}:{}", format_rate(*bps)))
            .collect::<Vec<_>>()
            .join(",");
        let mut next = live;
        let mut reason = "sampled";
        // Nothing claimable (the whole file is local, the window is full, or the
        // session is paused). A finished file must not read as a throughput
        // collapse: the old rule halved 16→8 right after the queue drained.
        let queue_idle = stats.queue_chunks == 0 && stats.inflight == 0;
        // A *slow* serve wait is acted on immediately: the client is waiting
        // right now, so the pool gives the machine back on this tick, not the
        // next. A normal 8 ms miss is not a reason to throw away the ramp.
        if queue_idle {
            dec_bytes = stats.bytes_fetched;
            dec_at = now;
            reason = "queue-idle";
        } else if new_slow > 0 && live > GOVERNOR_MIN_CONNECTIONS {
            next = (live / 2).max(GOVERNOR_MIN_CONNECTIONS);
            best_agg = 0;
            best_live = next;
            settled = false;
            reason = "serve-wait-slow";
        } else if !decide {
            // The odd tick is telemetry only.
        } else {
            // Decision window: two ticks, so a 4 MiB chunk that lands between
            // samples is counted once and the ramp is not decided by a fragment.
            let decision_ms = now
                .saturating_duration_since(dec_at)
                .as_millis()
                .max(1) as u64;
            let decision_agg = stats
                .bytes_fetched
                .saturating_sub(dec_bytes)
                .saturating_mul(1000)
                .saturating_div(decision_ms);
            dec_bytes = stats.bytes_fetched;
            dec_at = now;
            reason = "settled";
            if decision_agg > 0 && decision_agg as f64 > best_agg as f64 * GOVERNOR_COLLAPSE {
                slow_windows = 0;
            }
            if decision_agg == 0 {
                // Nothing was fetched this window (window full, queue done, or
                // the session is paused): keep the count, learn nothing.
                reason = "idle";
            } else if !explored {
                // The first pass always measures the configured count: the user
                // asked for sixteen, so sixteen is tried once even when two
                // already saturate the bucket (a 19 % gain does not clear
                // [`GOVERNOR_IMPROVE`], and the session used to settle on two
                // without ever testing the setting).
                if decision_agg as f64 >= best_agg as f64 * GOVERNOR_IMPROVE
                    || best_agg == 0
                {
                    best_agg = decision_agg;
                    best_live = live;
                }
                next = (live.saturating_mul(2)).min(configured);
                if next >= configured {
                    explored = true;
                }
                reason = "explore";
            } else if best_agg == 0
                || decision_agg as f64 >= best_agg as f64 * GOVERNOR_IMPROVE
            {
                best_agg = decision_agg;
                best_live = live;
                settled = false;
                if live < configured {
                    next = (live.saturating_mul(2)).min(configured);
                    reason = "improved";
                }
            } else if live > GOVERNOR_MIN_CONNECTIONS && !settled {
                // Worse than the best count: step back toward it (and below it
                // once, so a mirror that is best at two is not left at four).
                next = if best_live >= live {
                    (live / 2).max(GOVERNOR_MIN_CONNECTIONS)
                } else {
                    best_live
                };
                if next >= live {
                    next = live;
                    settled = true;
                }
                reason = "regressed";
            } else if decision_agg as f64 <= best_agg as f64 * GOVERNOR_COLLAPSE {
                // Throughput collapsed (shared link, throttled mirror). One
                // window is noise; three in a row means the best is stale, so
                // re-explore from the current count.
                slow_windows = slow_windows.saturating_add(1);
                if slow_windows >= 3 {
                    best_agg = decision_agg;
                    best_live = live;
                    settled = false;
                    slow_windows = 0;
                    if live < configured {
                        next = (live.saturating_mul(2)).min(configured);
                    }
                    reason = "collapse";
                }
            } else {
                settled = true;
            }
        }
        if next != live {
            engine.desired_connections.store(next as u64, Ordering::SeqCst);
            crate::netpool::wake();
        }
        // One line per tick: the concurrency the scheduler really had (not the
        // configured number), and the per-connection share behind it.
        crate::debug_log(&format!(
            "prefetch fetch active={active} inflight={inflight} live={live}->{next} configured={configured} agg={}/s bytes={} per=[{per}] waits={new_waits} slow={new_slow} reason={reason}",
            format_rate(agg),
            format_bytes(stats.bytes_fetched)
        ));
    }
}

/// Priority-ordered chunk selection over the whole file.
///
/// 1. the chunk(s) the client reads now and the next [`HOT_AHEAD_SECONDS`] of
///    playback (never rate limited: this is what plays),
/// 2. a range that failed its retry route (back in the queue),
/// 3. the seek warm points,
/// 4. the worker's own part of the horizon, then the largest remaining part
///    (work-stealing), or — for a short/unsplit file — the horizon in order.
///
/// 2–4 are speculative and share one token bucket refilled at
/// [`speculative_rate`], which is sized so every live connection can be in
/// flight (and bounded by [`SPECULATIVE_CEILING_BPS`]).
///
/// **A serve wait suspends 2–4.** When `serve_waiting` (a response body is
/// blocked on a missing hot chunk) only the hot window is claimable: every
/// worker that finishes — including one preempted by
/// [`Engine::enforce_serve_priority`] — takes the client's bytes, and the
/// speculative queue resumes the moment the wait closes.
#[allow(clippy::too_many_arguments)]
fn pick_chunk(
    state: &mut State,
    worker_id: usize,
    config: &PrefetchConfig,
    held: bool,
    serve_waiting: bool,
    background_cap: usize,
    live: usize,
    bitrate: u64,
    disk: &DiskCache,
) -> Option<(u64, ChunkKind)> {
    // A young session runs a small speculative pool: only the first
    // `background_cap` workers may take Warm/Rest chunks. The hot window (what
    // playback actually needs) is never limited.
    let background_ok = background_cap == 0 || worker_id < background_cap;
    let chunk = config.chunk_bytes.max(1);
    let generation = state.generation;
    let now = Instant::now();

    // 0. The exact byte a response body is blocked on (a serve wait). This is
    //    the client's own request and outranks every look-ahead chunk: claiming
    //    the chunk that *contains* the offset would still deliver
    //    `offset - chunk_start` bytes first — seconds of media on a 4 MiB
    //    chunk behind a slow-to-start route. The entry is dropped as soon as
    //    the bytes are here, in flight, dead, or already on disk.
    while let Some(start) = state.urgent.pop_front() {
        let end = state.total.map_or(start.saturating_add(chunk), |total| {
            total.min(start.saturating_add(chunk))
        });
        if chunks_cover(&state.chunks, start)
            || state.inflight.contains(&(generation, start))
            || state.dead.contains(&(generation, start))
            || disk.covers_range(start, end)
        {
            continue;
        }
        return Some((start, ChunkKind::Hot));
    }

    // 1. Hot window: current chunk plus the next [`HOT_AHEAD_SECONDS`] of
    //    playback. The frontier is clamped to the known length: past EOF there
    //    is nothing to fetch. An
    //    unclamped frontier used to claim `start >= total` ranges forever
    //    (start=16777216 end=15402399), and because nothing can ever mark such
    //    a range as covered, every worker spun on it at 1-4k claims/s.
    let frontier = hot_frontier(state, chunk, bitrate, now).min(state.total.unwrap_or(u64::MAX));
    let mut start = chunk_start(state.cursor, chunk);
    while start < frontier {
        if !hot_taken(state, generation, start, chunk, disk) {
            return Some((start, ChunkKind::Hot));
        }
        start = start.saturating_add(chunk);
    }

    // 2. A range whose retry route failed goes back ahead of the rest. It is a
    //    range the client was waiting for, so it is not rate limited.
    //
    //    A serve wait suspends everything below: while the body is blocked on a
    //    missing hot chunk, the freed workers must claim hot bytes, not
    //    speculative ones. A requeued hot chunk is still reachable — the hot
    //    scan above picks it, because `hot_taken` does not know about
    //    `requeued`.
    if serve_waiting {
        return None;
    }
    if !background_ok {
        return None;
    }
    if let Some(&start) = state.requeued.iter().next() {
        state.requeued.remove(&start);
        if !chunk_taken(state, generation, start, chunk, disk) {
            return Some((start, ChunkKind::Requeue));
        }
    }

    // Everything below is speculative. A player-held session and an exhausted
    // token bucket both leave the workers idle, which is the point: those
    // bytes are a convenience for a later seek, not playback.
    if held {
        return None;
    }
    let warm_pending = state
        .warm_plan
        .iter()
        .any(|task| !task.done && !task.claimed);
    // The opening of a session keeps the old, small budget: the decoder and the
    // interpolator are starting and must not compete with a sixteen-stream
    // burst (see [`SPECULATIVE_WARMUP`]). The hot window is never affected.
    let warmup = state
        .first_serve
        .map_or(true, |first| first.elapsed() < SPECULATIVE_WARMUP);
    let rate = speculative_rate(bitrate, warm_pending, warmup);
    let burst = speculative_burst(chunk, live, warmup);

    // 3. Seek warm points (one-shot: the plan is a few dozen MiB in total).
    //    A point primed for a seek that is happening right now (`urgent`) does
    //    not take from the speculative bucket: it is the client's own work, and
    //    the bucket would hold it back for seconds on a small stream.
    if state.warm_plan_started {
        for index in 0..state.warm_plan.len() {
            if state.warm_plan[index].done || state.warm_plan[index].claimed {
                continue;
            }
            let start = state.warm_plan[index].start;
            if chunk_taken(state, generation, start, chunk, disk) {
                state.warm_plan[index].done = true;
                state.warm_points_done = state.warm_points_done.saturating_add(1);
                continue;
            }
            if state.dead.contains(&(generation, start)) {
                state.warm_plan[index].done = true;
                state.warm_points_done = state.warm_points_done.saturating_add(1);
                continue;
            }
            let bytes = state.warm_plan[index]
                .end
                .saturating_sub(start)
                .max(chunk);
            if !state.warm_plan[index].urgent && !spec_take(state, bytes, rate, burst) {
                return None;
            }
            state.warm_plan[index].claimed = true;
            return Some((start, ChunkKind::Warm));
        }
    }

    // 4. The whole-file queue, inside the download horizon: the file may be at
    //    most [`PREFETCH_AHEAD_SECONDS`] of media ahead of the playhead. That is
    //    an hour, i.e. the whole file for anything this player opens: the queue
    //    is meant to cache the file, not to pace itself to playback (the old
    //    90 s horizon made "整片缓存完成" take as long as watching). What is
    //    *retained* in memory is still [`PLAYBACK_AHEAD_SECONDS`], so the wider
    //    queue costs cache file, not memory.
    //
    //    Without a bitrate estimate (`0`: tests, and the first few hundred
    //    milliseconds of a session) there is no horizon the caller could
    //    express, so only the token bucket limits this queue.
    let horizon = if bitrate == 0 {
        u64::MAX
    } else {
        bitrate.saturating_mul(prefetch_horizon_seconds())
    };
    let limit = state
        .total
        .map_or(state.cursor.saturating_add(horizon), |total| {
            total.min(state.cursor.saturating_add(horizon))
        });
    if disk.end_at(state.cursor) >= limit && limit > state.cursor {
        return None;
    }
    // One chunk is reserved from the bucket *before* the search (the bucket is
    // the aggregate rate limit) and given back when there was nothing left to
    // fetch.
    if !state.parts_built {
        let total = state.total?;
        if total.saturating_sub(chunk_start(state.cursor, chunk)) >= DYNAMIC_SPLIT_THRESHOLD {
            build_parts(state, live.max(1), chunk);
        } else {
            state.parts_built = true;
        }
    }
    if !spec_take(state, chunk, rate, burst) {
        return None;
    }
    let found = pick_rest_chunk(state, worker_id, generation, chunk, disk, limit);
    if found.is_none() {
        state.spec_tokens = state.spec_tokens.saturating_add(chunk).min(burst);
        return None;
    }
    found.map(|start| (start, ChunkKind::Rest))
}

/// The next whole-file chunk: the worker's own part, then the largest remaining
/// part, then (short or unsplit files) read order wrapping to the head.
fn pick_rest_chunk(
    state: &mut State,
    worker_id: usize,
    generation: u64,
    chunk: u64,
    disk: &DiskCache,
    limit: u64,
) -> Option<u64> {
    if !state.parts.is_empty() {
        for index in 0..state.parts.len() {
            if state.parts[index].owner != Some(worker_id) {
                continue;
            }
            if let Some(start) = take_part_chunk(state, index, generation, chunk, disk, limit) {
                return Some(start);
            }
        }
        // Own part has nothing inside the horizon: steal from the largest
        // remaining claimable part.
        if let Some(start) = steal_chunk(state, generation, chunk, disk, limit) {
            return Some(start);
        }
    }

    // Short file (or no split yet): the rest in read order, wrapping to the
    // head of the file once the cursor's tail is done. `limit` caps the
    // forward scan so nothing beyond the horizon is fetched.
    let total = state.total?;
    // The cursor can sit at (or past) EOF once the file is fully read. Clamp
    // the wrap to the length so the head scan never runs past it.
    let from = chunk_start(state.cursor, chunk).min(total);
    let mut start = from;
    while start < total.min(limit) {
        if !chunk_taken(state, generation, start, chunk, disk) {
            return Some(start);
        }
        start = start.saturating_add(chunk);
    }
    let mut start = 0u64;
    while start < from {
        if !chunk_taken(state, generation, start, chunk, disk) {
            return Some(start);
        }
        start = start.saturating_add(chunk);
    }
    None
}

/// Reserve the next piece of work for one worker, honouring the priority
/// order. The slot is updated under the same lock that hands out the range, so
/// a snapshot can never see a claimed range with no worker behind it.
fn claim(engine: &Engine, shutdown: &AtomicBool, worker_id: usize) -> Claim {
    let mut state = match engine.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    if shutdown.load(Ordering::SeqCst) || state.fatal.is_some() {
        mark_worker_idle(&mut state, worker_id);
        return Claim::Stop;
    }
    if worker_id >= engine.desired_connections() {
        mark_worker_idle(&mut state, worker_id);
        return Claim::Wait;
    }
    let generation = state.generation;
    let chunk = engine.config.chunk_bytes.max(1);
    if !state.warm_plan_started {
        if let Some(total) = state.total {
            build_warm_plan(
                &mut state,
                chunk,
                engine.config.point_bytes,
                engine.config.warm_points,
                total,
            );
        }
    }

    // A duplicate race for a slow range comes first: an idle worker takes it
    // on a different host while the slow worker keeps trying.
    let mut race_claim: Option<(u64, u64, Arc<AtomicBool>, String, usize)> = None;
    while let Some(request) = state.races.pop_front() {
        let start = request.start;
        if state.done.contains(&start) {
            state.race_tokens.remove(&start);
            continue;
        }
        if !state.inflight.contains(&(request.generation, start)) {
            // The slow worker already gave the range back; requeue normally.
            state.race_tokens.remove(&start);
            state.requeued.insert(start);
            continue;
        }
        let exclude = state
            .hosts
            .iter()
            .position(|host| host.label == request.slow_host)
            .into_iter()
            .collect::<Vec<_>>();
        let host = pick_host(&mut state, &exclude).or_else(|| pick_host(&mut state, &[]));
        let Some(host) = host else {
            state.races.push_front(request);
            break;
        };
        let token = state
            .race_tokens
            .entry(start)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        race_claim = Some((
            start,
            request.end,
            token,
            format!("{},{}", request.slow_host, state.hosts[host].label),
            host,
        ));
        break;
    }
    let bitrate = engine.bitrate_for(&state);
    let (start, end, kind, race, race_hosts, race_host) = if let Some((
        start,
        end,
        token,
        hosts,
        host,
    )) = race_claim
    {
        (start, end, ChunkKind::Race, Some(token), hosts, Some(host))
    } else {
        // `pick_chunk` already skips everything the session cache file holds
        // (`chunk_taken` / `hot_taken`), so this is one pass, not a search loop:
        // the disk lock is taken once, under the state lock, exactly like
        // `publish`.
        let disk = match engine.disk.lock() {
            Ok(disk) => disk,
            Err(poisoned) => poisoned.into_inner(),
        };
        match pick_chunk(
            &mut state,
            worker_id,
            &engine.config,
            engine.background_held(),
            engine.serve_waiting(),
            engine.background_workers.load(Ordering::Relaxed) as usize,
            engine.desired_connections(),
            bitrate,
            &disk,
        ) {
            Some((start, kind)) => {
                let mut end = state.total.map_or(start.saturating_add(chunk), |total| {
                    total.min(start.saturating_add(chunk))
                });
                if kind == ChunkKind::Warm {
                    // A warm point may be larger than one scheduling chunk; use
                    // the range the plan fixed for it.
                    if let Some(task) = state.warm_plan.iter().find(|task| task.start == start) {
                        end = task.end;
                    }
                }
                (start, end, kind, None, String::new(), None)
            }
            None => {
                drop(disk);
                mark_worker_idle(&mut state, worker_id);
                return Claim::Wait;
            }
        }
    };

    // Defensive: a past-EOF start can only produce an empty body or a `416`,
    // and since no chunk can ever cover it, the pool would claim it again on
    // the next tick. Retire it and let the worker look for real work.
    if let Some(total) = state.total {
        if start >= total {
            state.done.insert(start);
            mark_worker_idle(&mut state, worker_id);
            return Claim::Wait;
        }
    }

    let host = match race_host {
        Some(host) => host,
        None => match pick_host(&mut state, &[]) {
            Some(host) => host,
            None => {
                state.fatal = Some("没有可用的上游主机".to_string());
                mark_worker_idle(&mut state, worker_id);
                engine.publish(&mut state, Instant::now());
                return Claim::Stop;
            }
        },
    };
    let label = state.hosts.get(host).map_or_else(String::new, |host| host.label.clone());
    state.inflight.insert((generation, start));
    let token = engine
        .worker_cancel
        .get(worker_id)
        .map(|token| token.load(Ordering::SeqCst))
        .unwrap_or(0);
    if let Some(slot) = state.workers.get_mut(worker_id) {
        slot.host = label.clone();
        slot.start = start;
        slot.end = end;
        slot.busy = true;
        slot.kind = Some(kind);
        slot.samples.clear();
        slot.range_started = Some(Instant::now());
    }
    drop(state);
    crate::debug_log(&format!(
        "prefetch chunk claim after_ms={} gen={generation} worker={worker_id} start={start} end={end} host={label} kind={kind:?} bitrate={bitrate}",
        engine.started.elapsed().as_millis()
    ));
    Claim::Fetch(FetchClaim {
        generation,
        start,
        end,
        host,
        kind,
        token,
        race,
        race_hosts,
    })
}

/// Mark a worker as waiting for work, keeping its last range visible.
fn mark_worker_idle(state: &mut State, worker_id: usize) {
    if let Some(slot) = state.workers.get_mut(worker_id) {
        slot.busy = false;
        slot.range_started = None;
    }
}

/// Everything one fetch attempt needs to know about cancellation.
struct FetchContext {
    worker: usize,
    token: u64,
    race: Option<Arc<AtomicBool>>,
    /// The session's shutdown flag. A claim is no longer a task the server can
    /// abort (the pool owns it), so the transfer polls this too: closing a
    /// player lets go of every connection within [`WAIT_POLL`] instead of
    /// leaving it to the request timeout, and never competes with the next
    /// video for the link.
    closed: Arc<AtomicBool>,
}

impl FetchContext {
    fn cancelled(&self, engine: &Engine) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return true;
        }
        if self
            .race
            .as_ref()
            .is_some_and(|token| token.load(Ordering::SeqCst))
        {
            return true;
        }
        engine
            .worker_cancel
            .get(self.worker)
            .is_some_and(|token| token.load(Ordering::SeqCst) != self.token)
    }
}

/// Await `future`, checking for cancellation every [`WAIT_POLL`]. Used for
/// every connect and body wait, so a hung route lets go within milliseconds of
/// being marked slow instead of at the 30 s request timeout. The future is
/// pinned and re-polled across the short waits rather than recreated.
async fn cancellable<F>(engine: &Engine, context: &FetchContext, future: F) -> Option<F::Output>
where
    F: std::future::Future,
{
    let mut future = std::pin::pin!(future);
    loop {
        if context.cancelled(engine) {
            return None;
        }
        match tokio::time::timeout(WAIT_POLL, &mut future).await {
            Ok(output) => return Some(output),
            Err(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Pool-owned connection slots
//
// A session no longer starts a thread per connection (and never starts one per
// request): it **binds** its `connections` worker ids into the global slot
// registry below, and the fixed [`crate::netpool`] threads that already exist
// pick the work up. Slot *i* always means worker id *i*, so the per-worker
// health, route, slow-route and duplicate-race bookkeeping in [`State::workers`]
// is exactly what it was.
//
// The transfer itself stays on the shared async runtime: a step is the
// admission decision plus one dispatch, which keeps sixteen connections in
// flight from twelve threads - the pool must not become the ceiling on
// throughput. What changes is ownership: no thread and no long-lived fetch task
// is created for a video session any more.
// ---------------------------------------------------------------------------

/// One bound session: the engine, and the worker id this slot drives.
struct EngineBinding {
    engine: Arc<Engine>,
    shutdown: Arc<AtomicBool>,
    slot: usize,
    /// One dispatch at a time per slot (the worker-id bookkeeping is per slot).
    busy: AtomicBool,
}

/// Releases a bound slot when the transfer task ends — including a panicking or
/// cancelled task — so a connection can never be lost to a slot that still
/// looks busy.
struct SlotGuard(Arc<EngineBinding>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

/// Sessions the registry can host at once, each with [`MAX_CONNECTIONS`] slots.
/// Two player windows are the supported case, and each window may hold a video
/// proxy, an audio proxy and a seek shadow's audio proxy at the same time; the
/// extra blocks are headroom so a briefly overlapping close/open never falls
/// back to per-session tasks.
const MAX_BOUND_ENGINES: usize = 8;

static SLOTS: Mutex<Vec<Option<Arc<EngineBinding>>>> = Mutex::new(Vec::new());

impl Engine {
    /// One scheduling step of one worker id: claim a range and dispatch it.
    ///
    /// Returns `true` when a transfer was dispatched. `false` means the slot has
    /// nothing to do right now (queue empty, hot window full, or this id is
    /// above the live connection count) - the caller should sleep, not spin.
    ///
    /// The slot stays reserved (`binding.busy`) until the transfer *ends*, not
    /// until this function returns. Releasing it earlier let `pump_slot` hand a
    /// second claim to the same worker id while the first was still streaming:
    /// the per-worker slot then described only the newest claim, so a slow hot
    /// range became invisible to the health watchdog and to the serve-path
    /// priority, and the session could exceed its configured connection count.
    fn worker_step(self: &Arc<Self>, binding: &Arc<EngineBinding>) -> bool {
        let slot = binding.slot;
        let shutdown = &binding.shutdown;
        if shutdown.load(Ordering::SeqCst) {
            binding.busy.store(false, Ordering::SeqCst);
            return false;
        }
        let claim = match claim(self, shutdown, slot) {
            Claim::Fetch(claim) => claim,
            Claim::Wait | Claim::Stop => {
                binding.busy.store(false, Ordering::SeqCst);
                return false;
            }
        };
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            self.release_range(
                claim.generation,
                claim.start,
                claim.kind,
                claim.race.as_ref(),
            );
            binding.busy.store(false, Ordering::SeqCst);
            return false;
        };
        let engine = self.clone();
        let shutdown = shutdown.clone();
        let binding = binding.clone();
        runtime.spawn(async move {
            // The slot is released when this task ends, however it ends.
            let _slot = SlotGuard(binding);
            run_claim(slot, &engine, &shutdown, claim).await;
            // The worker id must never be left looking busy: the health
            // watchdog reads `busy` + `range_started` to decide on a slow
            // route, and a stale range would race a transfer that is over.
            if let Ok(mut state) = engine.state.lock() {
                mark_worker_idle(&mut state, slot);
            }
        });
        true
    }
}

/// Bind this session's worker ids into the global slot registry.
///
/// `false` when the pool is not running (a unit test, or `main` wiring failed)
/// or every block is taken; the caller then keeps the old task-per-worker
/// schedule, so a session can never end up bound to slots nothing pumps.
fn bind_slots(engine: &Arc<Engine>, shutdown: &Arc<AtomicBool>) -> bool {
    if !crate::netpool::initialized() {
        crate::debug_log("prefetch slots unbound reason=no-netpool");
        return false;
    }
    let mut slots = match SLOTS.lock() {
        Ok(slots) => slots,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut chosen = None;
    for block in 0..MAX_BOUND_ENGINES {
        let base = block * MAX_CONNECTIONS;
        if base + MAX_CONNECTIONS > slots.len() {
            slots.resize(base + MAX_CONNECTIONS, None);
        }
        if slots[base..base + MAX_CONNECTIONS]
            .iter()
            .all(Option::is_none)
        {
            chosen = Some(base);
            break;
        }
    }
    let Some(base) = chosen else {
        crate::debug_log(&format!(
            "prefetch slots unbound reason=registry-full blocks={MAX_BOUND_ENGINES}"
        ));
        return false;
    };
    for slot in 0..MAX_CONNECTIONS {
        slots[base + slot] = Some(Arc::new(EngineBinding {
            engine: engine.clone(),
            shutdown: shutdown.clone(),
            slot,
            busy: AtomicBool::new(false),
        }));
    }
    let total = slots.iter().filter(|entry| entry.is_some()).count();
    let threads = crate::netpool::threads();
    let connections = engine.desired_connections();
    drop(slots);
    crate::netpool::wake();
    crate::debug_log(&format!(
        "prefetch slots bound at={base} slots={MAX_CONNECTIONS} connections={connections} pool_threads={threads} bound_total={total}"
    ));
    true
}

/// Drop every slot this engine owns (session close / new video / quality
/// switch). In-flight transfers notice the shutdown flag and exit on their own;
/// nothing is aborted mid-write.
fn unbind_slots(engine: &Arc<Engine>) {
    let mut slots = match SLOTS.lock() {
        Ok(slots) => slots,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut dropped = 0usize;
    for entry in slots.iter_mut() {
        if entry
            .as_ref()
            .is_some_and(|binding| Arc::ptr_eq(&binding.engine, engine))
        {
            *entry = None;
            dropped += 1;
        }
    }
    let left = slots.iter().filter(|entry| entry.is_some()).count();
    drop(slots);
    if dropped > 0 {
        crate::netpool::wake();
        crate::debug_log(&format!(
            "prefetch slots unbound dropped={dropped} bound_left={left}"
        ));
    }
}

/// Whether any session owns slots right now (the pool uses this to choose
/// between a 5 ms prefetch poll and parking until a job arrives).
pub(crate) fn has_bindings() -> bool {
    match SLOTS.lock() {
        Ok(slots) => slots.iter().any(Option::is_some),
        Err(poisoned) => poisoned.into_inner().iter().any(Option::is_some),
    }
}

/// Give one bound slot a step. Called by an idle [`crate::netpool`] thread.
///
/// `cursor` is the calling thread's own round-robin position, so idle slots are
/// skipped without re-claiming the same one on every tick.
pub(crate) fn pump_slot(cursor: &mut usize) -> bool {
    let slots = match SLOTS.lock() {
        Ok(slots) => slots,
        Err(poisoned) => poisoned.into_inner(),
    };
    let total = slots.len();
    if total == 0 {
        return false;
    }
    let start = *cursor % total;
    for step in 0..total {
        let index = (start + step) % total;
        let Some(binding) = slots[index].as_ref() else {
            continue;
        };
        if binding.slot >= binding.engine.desired_connections() {
            continue;
        }
        if binding
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            continue;
        }
        let binding = binding.clone();
        // Never hold the registry while claiming: `claim` takes the engine
        // lock, and a session close wants the registry.
        drop(slots);
        *cursor = index + 1;
        let dispatched = binding.engine.worker_step(&binding);
        return dispatched;
    }
    *cursor = start + 1;
    false
}

/// Fallback schedule (only when no pool exists, e.g. a unit test): one async
/// task per worker id, exactly the pre-pool behaviour.
async fn worker(worker_id: usize, engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if worker_id >= engine.desired_connections() {
            // Outside the live pool: wait without touching the shared state.
            tokio::time::sleep(IDLE_CONNECTION_POLL).await;
            continue;
        }
        match claim(&engine, &shutdown, worker_id) {
            Claim::Stop => break,
            Claim::Wait => {
                // Nothing is due for this worker: the hot window is filled,
                // the speculative bucket is empty or no seek point is pending.
                tokio::time::sleep(IDLE_WAIT).await;
            }
            Claim::Fetch(claim) => {
                run_claim(worker_id, &engine, &shutdown, claim).await;
            }
        }
    }
    if let Ok(mut state) = engine.state.lock() {
        mark_worker_idle(&mut state, worker_id);
    }
}

/// Fetch one claim to completion, rotating hosts on failure, retrying once on
/// a new route after the health watchdog marks the worker slow, and handing a
/// range that still fails back to the queue.
async fn run_claim(
    worker_id: usize,
    engine: &Arc<Engine>,
    shutdown: &Arc<AtomicBool>,
    claim: FetchClaim,
) {
    let started = Instant::now();
    let Some(mut current) = engine.host_info(claim.host) else {
        engine.release_range(claim.generation, claim.start, claim.kind, claim.race.as_ref());
        return;
    };
    let context = FetchContext {
        worker: worker_id,
        token: claim.token,
        race: claim.race.clone(),
        closed: shutdown.clone(),
    };
    let mut attempted: Vec<usize> = Vec::new();
    let mut attempts = 0u32;
    let mut cursor = claim.start;
    let mut received_total = 0u64;
    let mut retried_slow = false;
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let url = forward_url(&engine.target, &current.1);
        let outcome = if claim.kind == ChunkKind::Warm {
            fetch_warm_stream(
                engine,
                &url,
                claim.generation,
                cursor,
                claim.end,
                current.0,
                worker_id,
                shutdown,
                &context,
                &mut collected,
            )
            .await
        } else {
            fetch_stream(
                engine,
                &url,
                claim.generation,
                cursor,
                claim.end,
                current.0,
                worker_id,
                claim.kind,
                shutdown,
                &context,
            )
            .await
        };
        match outcome {
            Ok(received) => {
                cursor += received;
                received_total += received;
                let short = engine.complete_range(
                    claim.generation,
                    claim.start,
                    cursor,
                    claim.end,
                    shutdown,
                );
                if short {
                    // The body stopped before the promised end while the file
                    // length is known: a truncated range. It is back in the
                    // queue (or retired); it must not be marked fetched, and
                    // it must not be reported as a finished chunk.
                    engine.release_range(
                        claim.generation,
                        claim.start,
                        claim.kind,
                        claim.race.as_ref(),
                    );
                    crate::debug_log(&format!(
                        "prefetch chunk short start={} next={cursor} end={} kind={:?} host={}",
                        claim.start, claim.end, claim.kind, current.2
                    ));
                    return;
                }
                if claim.kind == ChunkKind::Warm {
                    let point = engine.warm_point_at(claim.start).unwrap_or(claim.start);
                    engine.store_point(claim.start, collected, point);
                }
                let won = engine.finish_chunk(
                    claim.generation,
                    claim.start,
                    claim.kind,
                    worker_id,
                    claim.race.as_ref(),
                );
                if won {
                    crate::debug_log(&format!(
                        "worker race range={}-{} hosts={} winner={worker_id}",
                        claim.start, claim.end, claim.race_hosts
                    ));
                }
                crate::debug_log(&format!(
                    "prefetch chunk done after_ms={} start={} bytes={received_total} kind={:?} host={} ms={}",
                    engine.started.elapsed().as_millis(),
                    claim.start,
                    claim.kind,
                    current.2,
                    started.elapsed().as_millis()
                ));
                return;
            }
            Err(fail) => {
                cursor += fail.received;
                received_total += fail.received;
                if fail.cancelled {
                    // A race loser does nothing: the winner owns the chunk.
                    if context
                        .race
                        .as_ref()
                        .is_some_and(|token| token.load(Ordering::SeqCst))
                        && claim.kind == ChunkKind::Race
                    {
                        engine.finish_chunk(
                            claim.generation,
                            claim.start,
                            claim.kind,
                            worker_id,
                            claim.race.as_ref(),
                        );
                        return;
                    }
                    // Slow (zero bytes for 5 s): drop the request, re-pick a
                    // host round-robin, retry the same range exactly once.
                    if engine.worker_is_slow(worker_id) && !retried_slow {
                        retried_slow = true;
                        attempted.push(current.0);
                        // A slow route is transient: exclude it for this one
                        // retry, but keep it in the mirror list (a single-host
                        // config must keep working).
                        let next = engine.next_host(&attempted);
                        if let Some(next) = next {
                            crate::debug_log(&format!(
                                "worker slow id={worker_id} bps={} action=retry host={}",
                                engine.worker_bps(worker_id),
                                next.2
                            ));
                            current = next;
                            continue;
                        }
                    }
                    if shutdown.load(Ordering::SeqCst) {
                        engine.release_range(
                            claim.generation,
                            claim.start,
                            claim.kind,
                            claim.race.as_ref(),
                        );
                        return;
                    }
                    engine.release_range(claim.generation, claim.start, claim.kind, claim.race.as_ref());
                    crate::debug_log(&format!(
                        "worker requeue id={worker_id} range={}-{} kind={:?} received={received_total}",
                        claim.start, claim.end, claim.kind
                    ));
                    return;
                }
                if fail.eof {
                    // 416 or an empty body: the file ends here.
                    if fail.received == 0 && cursor > 0 && !engine.total_known() {
                        engine.mark_range_beyond_end(claim.generation, claim.start, shutdown);
                    } else {
                        let short = engine.complete_range(
                            claim.generation,
                            claim.start,
                            cursor,
                            claim.end,
                            shutdown,
                        );
                        if short {
                            // Same rule as a short `206`: the advertised length
                            // stands and the range is requeued, never marked
                            // fetched.
                            engine.release_range(
                                claim.generation,
                                claim.start,
                                claim.kind,
                                claim.race.as_ref(),
                            );
                            return;
                        }
                        engine.finish_chunk(
                            claim.generation,
                            claim.start,
                            claim.kind,
                            worker_id,
                            claim.race.as_ref(),
                        );
                    }
                    return;
                }
                if shutdown.load(Ordering::SeqCst) {
                    engine.release_range(claim.generation, claim.start, claim.kind, claim.race.as_ref());
                    return;
                }
                attempted.push(current.0);
                engine.mark_host_failure(current.0, &fail.error);
                attempts += 1;
                // The first host can reject the very first chunk (a `403`, a
                // dead mirror). Fall through to the next healthy host for this
                // same range, still inside the request that waits for it.
                match engine.next_host(&attempted) {
                    Some(next) if attempts < RANGE_ATTEMPTS => {
                        crate::debug_log(&format!(
                            "prefetch range retry start={} at={cursor} host={} attempt={attempts} err={}",
                            claim.start, next.2, fail.error
                        ));
                        current = next;
                    }
                    other => {
                        if attempts < RANGE_ATTEMPTS {
                            // No untried route left. A body that died mid-range
                            // is transient far more often than it is a dead
                            // mirror, so resume the remainder on the same host
                            // after a short pause instead of ending the stream.
                            attempted.clear();
                            crate::debug_log(&format!(
                                "prefetch range retry start={} at={cursor} host={} attempt={attempts} same_host=true err={}",
                                claim.start, other.map_or_else(|| current.2.clone(), |next| next.2),
                                fail.error
                            ));
                            tokio::time::sleep(RETRY_BACKOFF * attempts).await;
                            if shutdown.load(Ordering::SeqCst) {
                                engine.release_range(
                                    claim.generation,
                                    claim.start,
                                    claim.kind,
                                    claim.race.as_ref(),
                                );
                                return;
                            }
                            continue;
                        }
                        engine.fail_range(
                            claim.generation,
                            claim.start,
                            cursor,
                            &attempted,
                            &fail.error,
                            shutdown,
                        );
                        // The range is dead, not done: never mark it fetched.
                        engine.release_range(
                            claim.generation,
                            claim.start,
                            claim.kind,
                            claim.race.as_ref(),
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Per-worker health watchdog: measures each busy worker over a five-second
/// window, marks zero-throughput (5 s) and below-20 %-of-median (15 s) workers
/// slow, cancels their request, and queues a duplicate race when another
/// connection is idle.
async fn health_loop(engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(HEALTH_POLL).await;
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let mut slow: Vec<(usize, u64, String)> = Vec::new();
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            let live = engine.desired_connections();
            let mut rates: Vec<(usize, u64)> = Vec::new();
            for (id, slot) in state.workers.iter_mut().take(live).enumerate() {
                if !slot.busy {
                    continue;
                }
                slot.bps = window_rate(&mut slot.samples, now, HEALTH_WINDOW);
                rates.push((id, slot.bps));
            }
            let mut sorted: Vec<u64> = rates.iter().map(|(_, bps)| *bps).filter(|bps| *bps > 0).collect();
            sorted.sort_unstable();
            let median = if sorted.is_empty() {
                0
            } else {
                sorted[sorted.len() / 2]
            };
            for (id, bps) in rates {
                let Some(slot) = state.workers.get(id) else {
                    continue;
                };
                if slot.slow {
                    // Already cancelled once; the retry is in flight.
                    continue;
                }
                let Some(started) = slot.range_started else {
                    continue;
                };
                let age = now.duration_since(started);
                let zero = bps == 0 && age >= HEALTH_SLOW_AFTER;
                let low = median > 0 && bps.saturating_mul(5) < median && age >= HEALTH_LOW_AFTER;
                if zero || low {
                    let host = slot.host.clone();
                    slow.push((id, bps, host));
                }
            }
            for (id, bps, host) in &slow {
                if let Some(slot) = state.workers.get_mut(*id) {
                    slot.slow = true;
                }
                if let Some(index) = state.hosts.iter().position(|known| known.label == *host) {
                    degrade_host(&mut state, index);
                }
                if let Some(token) = engine.worker_cancel.get(*id) {
                    token.fetch_add(1, Ordering::SeqCst);
                }
                crate::debug_log(&format!(
                    "worker slow id={id} bps={bps} action=cancel host={host}"
                ));
                // One duplicate per slow range, and only when a connection is
                // idle (or its part is done). Take whichever finishes first.
                let target = state.workers.get(*id).map(|slot| (slot.start, slot.end));
                if let Some((start, end)) = target {
                    // The next worker that finishes (its part is done) takes
                    // the duplicate, so this is also queued when every
                    // connection is currently busy.
                    if start < end && !state.race_tokens.contains_key(&start) {
                        let generation = state.generation;
                        state.races.push_back(RaceRequest {
                            generation,
                            start,
                            end,
                            slow_host: host.clone(),
                        });
                        state
                            .race_tokens
                            .insert(start, Arc::new(AtomicBool::new(false)));
                        crate::debug_log(&format!(
                            "worker race queued range={start}-{end} slow={id} hosts={host}"
                        ));
                    }
                }
            }
        }
        let _ = engine.snapshot();
    }
}

// ---------------------------------------------------------------------------
// Startup gate (playback-first scheduling)
// ---------------------------------------------------------------------------

/// Widen the read-ahead window from [`STARTUP_READ_AHEAD_BYTES`] (and start the
/// held-back connections) once the first frames are actually flowing.
///
/// "Frames flowing" is observed as the first bytes the client pulled: the
/// player's decoder asked for media, which happens before it can present a
/// frame. A proxy whose client never reads stays on the small window for at
/// most [`STARTUP_WINDOW_MAX`], so a dead reader cannot pin the gate forever.
/// Only spawned when the gate is on.
async fn startup_gate_loop(engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    let started = Instant::now();
    while started.elapsed() < STARTUP_WINDOW {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let served = {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.served
        };
        if served > 0 || started.elapsed() >= STARTUP_WINDOW_MAX {
            break;
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
    let before = engine.read_ahead_window();
    engine.read_ahead_cap.store(u64::MAX, Ordering::Relaxed);
    // The connection count is not touched here: the governor owns it, and it
    // only raises the count while the measured aggregate keeps improving (a
    // throttling mirror is fastest at two streams, not sixteen).
    crate::debug_log(&format!(
        "startup gate readahead={}→{} connections={} (governor) at_ms={}",
        before >> 20,
        engine.read_ahead_window() >> 20,
        engine.desired_connections.load(Ordering::SeqCst),
        engine.started.elapsed().as_millis()
    ));
}

impl Engine {
    /// `(index, origin, label)` for one host, or `None` when it disappeared.
    fn host_info(&self, index: usize) -> Option<(usize, String, String)> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let host = state.hosts.get(index)?;
        Some((index, host.origin.clone(), host.label.clone()))
    }
}

/// Why one ranged GET ended without its whole range.
struct FetchFail {
    received: u64,
    eof: bool,
    /// The health watchdog cancelled this request (slow worker), or a race
    /// winner owns the chunk.
    cancelled: bool,
    error: String,
}

impl FetchFail {
    fn cancelled(received: u64, reason: &str) -> Self {
        Self {
            received,
            eof: false,
            cancelled: true,
            error: reason.to_string(),
        }
    }
}

/// Stream one range request straight into the shared buffer. Every network
/// read is stored at its absolute offset, so a reader can start before the
/// range completes. Returns the bytes stored, or the bytes received plus why
/// the range could not finish (EOF, a route failure, or cancellation).
async fn fetch_stream(
    engine: &Engine,
    url: &str,
    generation: u64,
    start: u64,
    end: u64,
    host: usize,
    worker: usize,
    kind: ChunkKind,
    shutdown: &AtomicBool,
    context: &FetchContext,
) -> std::result::Result<u64, FetchFail> {
    let expected = end.saturating_sub(start);
    if expected == 0 {
        return Ok(0);
    }
    if context.cancelled(engine) {
        return Err(FetchFail::cancelled(0, "请求已取消"));
    }
    let send = tokio::time::timeout(
        CHUNK_TIMEOUT,
        engine
            .client
            .get(url)
            .header(RANGE, format!("bytes={start}-{}", end - 1))
            .send(),
    );
    let response = match cancellable(engine, context, send).await {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(error))) => {
            return Err(FetchFail {
                received: 0,
                eof: false,
                cancelled: false,
                error: error.to_string(),
            });
        }
        Some(Err(_)) => {
            return Err(FetchFail {
                received: 0,
                eof: false,
                cancelled: false,
                error: "请求超时".to_string(),
            });
        }
        None => return Err(FetchFail::cancelled(0, "请求已取消")),
    };

    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(FetchFail {
            received: 0,
            eof: true,
            cancelled: false,
            error: "HTTP 416".to_string(),
        });
    }
    // A server that ignores Range is only usable for offset 0; in that case
    // the first `expected` bytes of the body are the requested range.
    if status != StatusCode::PARTIAL_CONTENT && !(status.is_success() && start == 0) {
        return Err(FetchFail {
            received: 0,
            eof: false,
            cancelled: false,
            error: format!("HTTP {}", status.as_u16()),
        });
    }

    // This response *is* the metadata source: no probe ever ran for it. Learn
    // the total / MIME type before the body is read so a client waiting on its
    // first request can write its `Content-Range` without a second round trip.
    if let Some(learned) = learn_metadata(status, start, response.headers()) {
        engine.learn_metadata(&learned, shutdown);
    }

    let mut response = response;
    let mut received = 0u64;
    let mut offset = start;
    // See the flush rule below: one push per block, except the first read.
    let mut batch: Vec<u8> = Vec::with_capacity(READ_BATCH_BYTES as usize);
    let mut batch_at = start;
    let mut pushed_once = false;
    while received < expected {
        if shutdown.load(Ordering::SeqCst) {
            return Err(FetchFail {
                received,
                eof: false,
                cancelled: false,
                error: "代理已关闭".to_string(),
            });
        }
        // Health cancellation and a race winner are checked per network read,
        // so a slow or duplicate request lets go within one chunk.
        if context.cancelled(engine) {
            return Err(FetchFail::cancelled(received, "请求已取消"));
        }
        let read = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk());
        let chunk = match cancellable(engine, context, read).await {
            Some(Ok(Ok(Some(chunk)))) => chunk,
            Some(Ok(Ok(None))) => break,
            Some(Ok(Err(error))) => {
                return Err(FetchFail {
                    received,
                    eof: false,
                    cancelled: false,
                    error: error.to_string(),
                });
            }
            Some(Err(_)) => {
                return Err(FetchFail {
                    received,
                    eof: false,
                    cancelled: false,
                    error: "读取超时".to_string(),
                });
            }
            None => return Err(FetchFail::cancelled(received, "请求已取消")),
        };
        let take = chunk.len().min((expected - received) as usize);
        if take == 0 {
            break;
        }
        // Batch network reads into one `push_bytes` block: reqwest hands out
        // whatever an HTTP/2 frame carried (16 KiB), and every push takes the
        // state lock, copies the bytes, publishes the whole download map and
        // queues a disk write. At 16 streams × 8 MB/s that was thousands of
        // lock/publish/queue round trips per second for 16 KiB each. The first
        // read is pushed on its own so a serve wait still sees bytes at once;
        // the rest go out in READ_BATCH_BYTES blocks (the disk block size).
        if batch.is_empty() {
            batch_at = offset;
        }
        batch.extend_from_slice(&chunk[..take]);
        offset += take as u64;
        received += take as u64;
        let flush = !pushed_once
            || batch.len() as u64 >= READ_BATCH_BYTES
            || received >= expected;
        if flush {
            if !engine.push_bytes(
                generation,
                batch_at,
                &batch,
                host,
                worker,
                kind,
                shutdown,
            ) {
                return Ok(received);
            }
            pushed_once = true;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        engine.push_bytes(
            generation,
            batch_at,
            &batch,
            host,
            worker,
            kind,
            shutdown,
        );
    }
    Ok(received)
}

/// Fetch one seek point into `data` instead of the window. The bytes still
/// count as upstream traffic and for the worker's health; they are committed
/// to the point cache by the caller only once the whole chunk arrived.
#[allow(clippy::too_many_arguments)]
async fn fetch_warm_stream(
    engine: &Engine,
    url: &str,
    _generation: u64,
    start: u64,
    end: u64,
    host: usize,
    worker: usize,
    shutdown: &AtomicBool,
    context: &FetchContext,
    data: &mut Vec<u8>,
) -> std::result::Result<u64, FetchFail> {
    let expected = end.saturating_sub(start);
    if expected == 0 {
        return Ok(0);
    }
    if context.cancelled(engine) {
        return Err(FetchFail::cancelled(0, "请求已取消"));
    }
    let send = tokio::time::timeout(
        CHUNK_TIMEOUT,
        engine
            .client
            .get(url)
            .header(RANGE, format!("bytes={start}-{}", end - 1))
            .send(),
    );
    let mut response = match cancellable(engine, context, send).await {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(error))) => {
            return Err(FetchFail {
                received: 0,
                eof: false,
                cancelled: false,
                error: error.to_string(),
            });
        }
        Some(Err(_)) => {
            return Err(FetchFail {
                received: 0,
                eof: false,
                cancelled: false,
                error: "请求超时".to_string(),
            });
        }
        None => return Err(FetchFail::cancelled(0, "请求已取消")),
    };
    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(FetchFail {
            received: 0,
            eof: true,
            cancelled: false,
            error: "HTTP 416".to_string(),
        });
    }
    if status != StatusCode::PARTIAL_CONTENT && !(status.is_success() && start == 0) {
        return Err(FetchFail {
            received: 0,
            eof: false,
            cancelled: false,
            error: format!("HTTP {}", status.as_u16()),
        });
    }
    if let Some(learned) = learn_metadata(status, start, response.headers()) {
        engine.learn_metadata(&learned, shutdown);
    }
    data.reserve(expected.min(MAX_CHUNK_BYTES) as usize);
    let mut received = 0u64;
    while received < expected {
        if shutdown.load(Ordering::SeqCst) {
            return Err(FetchFail {
                received,
                eof: false,
                cancelled: false,
                error: "代理已关闭".to_string(),
            });
        }
        if context.cancelled(engine) {
            data.truncate(received as usize);
            return Err(FetchFail::cancelled(received, "请求已取消"));
        }
        let read = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk());
        let chunk = match cancellable(engine, context, read).await {
            Some(Ok(Ok(Some(chunk)))) => chunk,
            Some(Ok(Ok(None))) => break,
            Some(Ok(Err(error))) => {
                return Err(FetchFail {
                    received,
                    eof: false,
                    cancelled: false,
                    error: error.to_string(),
                });
            }
            Some(Err(_)) => {
                return Err(FetchFail {
                    received,
                    eof: false,
                    cancelled: false,
                    error: "读取超时".to_string(),
                });
            }
            None => {
                data.truncate(received as usize);
                return Err(FetchFail::cancelled(received, "请求已取消"));
            }
        };
        let take = chunk.len().min((expected - received) as usize);
        if take == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..take]);
        received += take as u64;
        engine.note_warm_bytes(host, worker, take as u64, shutdown);
    }
    Ok(received)
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

async fn run_server(
    listener: StdTcpListener,
    engine: Arc<Engine>,
    shutdown: Arc<AtomicBool>,
) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            crate::debug_log(&format!("prefetch listener failed err={error}"));
            return;
        }
    };
    let mut workers = JoinSet::new();
    let gate = startup_gate_enabled();
    // Every connection is live from the first byte: the window is the only
    // thing the startup gate still holds back, never the worker count. Slots
    // above the configured count wait cheaply so `set_connections` can raise
    // the count at any moment.
    //
    // The fetch workers themselves are the process-wide pool's threads, bound
    // to this session's worker ids: opening a video creates no thread at all.
    // Only a session that cannot bind (no pool: unit tests, or every registry
    // block taken) falls back to one async task per worker id.
    if !bind_slots(&engine, &shutdown) {
        for id in 0..MAX_CONNECTIONS {
            workers.spawn(worker(id, engine.clone(), shutdown.clone()));
        }
    }
    // Widens the window after the first frames are flowing (a no-op when the
    // gate is off: `read_ahead_cap` is already `u64::MAX`).
    if gate {
        workers.spawn(startup_gate_loop(
            engine.clone(),
            shutdown.clone(),
        ));
    }
    // The live connection count follows the measured knee, from the first
    // second: no "two background workers for twelve seconds" phase, and no
    // sixteen throttled streams when the mirror is fastest at two.
    workers.spawn(connection_governor(engine.clone(), shutdown.clone()));
    // Per-worker throughput watchdog: slow routes, route fallback and
    // duplicate-request races.
    workers.spawn(health_loop(engine.clone(), shutdown.clone()));
    // `BILI_PREFETCH_STATS_TEST=1`: one live per-second line describing the
    // workers and the downloaded ranges (QA only; same JoinSet, same shutdown).
    if stats_probe_enabled() {
        workers.spawn(stats_probe(engine.clone(), shutdown.clone()));
    }
    let mut handlers: JoinSet<()> = JoinSet::new();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        while handlers.try_join_next().is_some() {}
        match tokio::time::timeout(ACCEPT_POLL, listener.accept()).await {
            Ok(Ok((stream, _peer))) => {
                let engine = engine.clone();
                let shutdown = shutdown.clone();
                handlers.spawn(async move {
                    if let Err(error) = handle_connection(stream, &engine, &shutdown).await {
                        crate::debug_log(&format!("prefetch connection err={error}"));
                    }
                });
            }
            Ok(Err(error)) => {
                crate::debug_log(&format!("prefetch accept err={error}"));
                break;
            }
            Err(_) => {}
        }
    }

    shutdown.store(true, Ordering::SeqCst);
    unbind_slots(&engine);
    handlers.abort_all();
    workers.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while workers.join_next().await.is_some() {}
    })
    .await;
    drop(listener);
    // One line per session: the upstream byte split by scheduling class. A run
    // whose hot share is a small fraction still did the right thing (playback
    // is cheap); one whose warm+rest share is zero spent every byte on the
    // client, which is what a permanent priority would look like and must not
    // happen without a serve wait.
    let stats = engine.snapshot();
    crate::debug_log(&format!(
        "prefetch split end hot={} warm={} rest={} retry={} fetched={} total={:?}",
        format_bytes(stats.bytes_hot),
        format_bytes(stats.bytes_warm),
        format_bytes(stats.bytes_rest),
        format_bytes(stats.bytes_retry),
        format_bytes(stats.bytes_fetched),
        stats.bytes_total
    ));
    crate::debug_log("prefetch server stopped");
}

// ---------------------------------------------------------------------------
// Live stats probe (`BILI_PREFETCH_STATS_TEST=1`)
// ---------------------------------------------------------------------------

/// `BILI_PREFETCH_STATS_TEST=1` turns on the once-per-second worker/range line.
fn stats_probe_enabled() -> bool {
    std::env::var("BILI_PREFETCH_STATS_TEST")
        .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// `12.6MiB` / `640.0KiB` / `0B` for the QA lines.
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1}MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1}KiB", bytes / KIB)
    } else {
        format!("{bytes:.0}B")
    }
}

/// Bytes covered by a merged range list.
fn ranges_bytes(ranges: &[(u64, u64)]) -> u64 {
    ranges
        .iter()
        .map(|(start, end)| end.saturating_sub(*start))
        .sum()
}

/// Number of adjacent pairs that overlap (always 0 for `merge_ranges` output;
/// the probe asserts the published list really is coalesced).
fn range_overlaps(ranges: &[(u64, u64)]) -> usize {
    ranges
        .windows(2)
        .filter(|pair| pair[0].1 > pair[1].0)
        .count()
}

/// Read the same snapshot the panel reads, once a second, and log:
///
/// ```text
/// prefetch workers n=4 active=4 ranges=[h1 0-4194304 …] downloaded=3/12.6MiB/25.9% total=48577821 speed=3.2MB/s point_cache=0/0B
/// prefetch stats qa tick=1 busy_ranges=4/4 overlaps=0 ranges=3 covered=12.6MiB delta=+12.6MiB point_ranges=0 point_bytes=0
/// ```
///
/// The first line is the panel's data; the second proves the invariants: the
/// busy workers hold distinct ranges, the merged list never overlaps and grows
/// as chunks land, and the point cache shows up once warm points are stored.
async fn stats_probe(engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    let mut tick = 0u64;
    let mut previous_covered = 0u64;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tick = tick.saturating_add(1);
        let stats = engine.snapshot();
        let ranges = stats
            .workers
            .iter()
            .map(|worker| {
                let host = if worker.host.is_empty() {
                    "-"
                } else {
                    worker.host.as_str()
                };
                format!("{host} {}-{}", worker.start, worker.end)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let covered = ranges_bytes(&stats.downloaded);
        let percent = match stats.total_bytes {
            Some(total) if total > 0 => covered as f64 * 100.0 / total as f64,
            _ => 0.0,
        };
        let total = stats
            .total_bytes
            .map_or_else(|| "-".to_string(), |total| total.to_string());
        crate::debug_log(&format!(
            "prefetch workers n={} active={} ranges=[{ranges}] downloaded={}/{}/{:.1}% total={total} speed={} point_cache={}/{}",
            stats.workers.len(),
            stats.active_workers,
            stats.downloaded.len(),
            format_bytes(covered),
            percent,
            format_rate(stats.bytes_per_sec),
            stats.point_cache.len(),
            format_bytes(stats.point_cache_bytes)
        ));
        let busy: Vec<(u64, u64)> = stats
            .workers
            .iter()
            .filter(|worker| worker.busy)
            .map(|worker| (worker.start, worker.end))
            .collect();
        let mut distinct = busy.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let delta = covered as i64 - previous_covered as i64;
        // Upstream bytes by scheduling class: hot (what plays), warm (seek
        // points), rest (speculative whole file) and retry. While a serve wait
        // is open the hot share must own the link; with nothing waiting,
        // warm/rest keep filling — this line proves both.
        let split = format!(
            "hot={} warm={} rest={} retry={}",
            format_bytes(stats.bytes_hot),
            format_bytes(stats.bytes_warm),
            format_bytes(stats.bytes_rest),
            format_bytes(stats.bytes_retry)
        );
        crate::debug_log(&format!(
            "prefetch split tick={tick} {split} fetched={}",
            format_bytes(stats.bytes_fetched)
        ));
        crate::debug_log(&format!(
            "prefetch stats qa tick={tick} busy_ranges={}/{} overlaps={} ranges={} covered={} delta={delta:+} point_ranges={} point_bytes={} active={} {split}",
            distinct.len(),
            busy.len(),
            range_overlaps(&stats.downloaded),
            stats.downloaded.len(),
            format_bytes(covered),
            stats.point_cache.len(),
            format_bytes(stats.point_cache_bytes),
            stats.active_workers
        ));
        // Scheduler view: busy/idle split, per-worker throughput, whole-file
        // queue and in-memory bytes. This is what the dynamic-connection QA
        // reads to prove every connection is working and none idles while
        // unclaimed chunks exist.
        let health = engine.worker_health_snapshot();
        let bps = health
            .iter()
            .map(|(id, _, _, bps)| format!("{id}:{}", format_rate(*bps)))
            .collect::<Vec<_>>()
            .join(",");
        let slow = health.iter().filter(|(_, _, slow, _)| *slow).count();
        let cached = engine.cached_ranges();
        crate::debug_log(&format!(
            "prefetch sched tick={tick} busy={} idle={} connections={} slow={slow} bps=[{bps}] queue={}/{} window={} cached={} ranges={}",
            stats.active_workers,
            stats.workers.len().saturating_sub(stats.active_workers as usize),
            stats.workers.len(),
            stats.queue_chunks,
            format_bytes(stats.queue_bytes),
            format_bytes(stats.read_ahead_bytes),
            format_bytes(ranges_bytes(&cached)),
            cached.len()
        ));
        previous_covered = covered;
    }
}

struct Request {
    method: String,
    target: String,
    range: Option<String>,
}

fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let version = request_line.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            range = Some(value.trim().to_string());
        }
    }
    Some(Request {
        method,
        target,
        range,
    })
}

/// Split an origin-form (or absolute-form) request target into path+query.
fn split_target(target: &str) -> Option<(String, String)> {
    let rest = if let Some(index) = target.find("://") {
        let after_scheme = &target[index + 3..];
        match after_scheme.find('/') {
            Some(slash) => &after_scheme[slash..],
            None => "/",
        }
    } else {
        target
    };
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, query.to_string()),
        None => (rest, String::new()),
    };
    if !path.starts_with('/') {
        return None;
    }
    Some((path.to_string(), query))
}

async fn read_head(stream: &mut TcpStream, shutdown: &AtomicBool) -> Result<String, String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut scratch = [0u8; 1024];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Err("代理已关闭".to_string());
        }
        stream
            .readable()
            .await
            .map_err(|error| error.to_string())?;
        match stream.try_read(&mut scratch) {
            Ok(0) => return Err("连接已关闭".to_string()),
            Ok(read) => {
                buffer.extend_from_slice(&scratch[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if buffer.len() > 16 * 1024 {
                    return Err("请求头过大".to_string());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    String::from_utf8(buffer).map_err(|error| error.to_string())
}

async fn write_all(stream: &mut TcpStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        stream.writable().await?;
        match stream.try_write(data) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "本地连接已关闭",
                ))
            }
            Ok(written) => data = &data[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// `write_all` that lets go as soon as a newer request owns the proxy.
///
/// A client that seeked away never reads the rest of this body, so waiting on
/// `writable()` until the kernel notices keeps the response lock — and with it
/// the seek — for tens of seconds. The timeout is a polling interval, not a
/// write deadline: a slow or paused reader keeps its connection indefinitely.
async fn write_all_live(
    stream: &mut TcpStream,
    engine: &Engine,
    epoch: u64,
    mut data: &[u8],
) -> io::Result<()> {
    while !data.is_empty() {
        if !engine.is_current(epoch) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "已被新的请求接管",
            ));
        }
        match tokio::time::timeout(WRITE_POLL, stream.writable()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
        match stream.try_write(data) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "本地连接已关闭",
                ))
            }
            Ok(written) => data = &data[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Why a body write failed, in the serve log's vocabulary. A client that seeks
/// or closes abandons the socket, which the kernel reports as a reset or a
/// broken pipe: that is the normal end of an abandoned response, not a proxy
/// error, and it must not read like a truncation.
fn write_end_reason(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::WriteZero
        | io::ErrorKind::UnexpectedEof => "client-closed",
        io::ErrorKind::TimedOut => "write-timeout",
        _ => "write-error",
    }
}

async fn send_error(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    detail: &str,
) -> io::Result<()> {
    let body = detail.as_bytes();
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write_all(stream, head.as_bytes()).await?;
    write_all(stream, body).await
}

/// Only one response is served at a time: two clients (or ffmpeg reconnecting
/// mid-response) must not move the shared read cursor under each other.
struct ServeGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for ServeGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Take the response lock for this request, preempting whatever held it.
async fn acquire_serve<'a>(
    engine: &'a Engine,
    shutdown: &AtomicBool,
) -> Option<(ServeGuard<'a>, u64)> {
    let started = Instant::now();
    // This request is the one the client wants now. Bumping the token makes the
    // response that still holds the flag — usually the open-ended `bytes=N-`
    // body ffmpeg abandoned when it seeked — stop at its next write poll
    // instead of streaming the rest of the file into a socket nobody reads.
    let epoch = engine.begin_response();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        if !engine.serving.swap(true, Ordering::SeqCst) {
            return Some((
                ServeGuard {
                    flag: &engine.serving,
                },
                epoch,
            ));
        }
        if started.elapsed() > SERVE_TIMEOUT {
            return None;
        }
        tokio::time::sleep(SERVE_POLL).await;
    }
}

/// Wait until the first client response can be written. No extra round trip is
/// made: the workers are already fetching real ranges (the first claims happen
/// at spawn), so this returns as soon as one of their responses revealed the
/// total length or the requested offset's bytes landed in the window.
async fn await_first_metadata(
    engine: &Engine,
    start: Option<u64>,
    spec: Option<RangeSpec>,
    shutdown: &AtomicBool,
) -> Result<(Option<u64>, Option<String>), String> {
    let started = Instant::now();
    loop {
        {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            let buffered = start.is_some_and(|start| {
                chunks_cover(&state.chunks, start)
                    || engine.disk_covers(start)
            });
            // A preloaded/cached file states its own length, so a seek into it
            // never has to wait for an upstream response to shape the 206.
            let disk_total = if buffered { engine.disk_total() } else { None };
            // A buffered offset is served from memory even when the upstream
            // pool is in a bad state: a failed speculative range must never
            // break a seek the cache can already answer.
            if metadata_ready(spec, state.total.or(disk_total), buffered) {
                return Ok((state.total.or(disk_total), state.content_type.clone()));
            }
            if let Some(error) = &state.fatal {
                return Err(error.clone());
            }
            // The chunk that would deliver this offset is dead on every host:
            // no metadata can arrive from it, so fail instead of hanging.
            if let Some(start) = start {
                let chunk = engine.config.chunk_bytes;
                let dead = state.dead.iter().any(|&(generation, key)| {
                    generation == state.generation
                        && key <= start
                        && start < key.saturating_add(chunk)
                });
                if dead {
                    return Err(format!("区间 {start} 无法从任何主机获取"));
                }
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            return Err("代理已关闭".to_string());
        }
        if started.elapsed() > FIRST_METADATA_TIMEOUT {
            return Err("等待首个上游响应超时".to_string());
        }
        tokio::time::sleep(WAIT_POLL).await;
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    engine: &Arc<Engine>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), String> {
    let head = tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut stream, shutdown))
        .await
        .map_err(|_| "读取请求超时".to_string())??;
    let request = parse_request(&head).ok_or_else(|| "无法解析请求".to_string())?;
    let request_started = Instant::now();
    crate::debug_log(&format!(
        "prefetch request method={} range={} after_ms={}",
        request.method,
        request.range.as_deref().unwrap_or("-"),
        engine.started.elapsed().as_millis()
    ));

    if request.method != "GET" && request.method != "HEAD" {
        let _ = send_error(&mut stream, 405, "Method Not Allowed", "仅支持 GET/HEAD").await;
        return Ok(());
    }
    match split_target(&request.target) {
        Some((path, query))
            if path == engine.target.path && query == engine.target.query => {}
        _ => {
            let _ = send_error(&mut stream, 403, "Forbidden", "此代理只服务一个媒体地址").await;
            return Ok(());
        }
    }

    let spec = match request.range.as_deref() {
        None => None,
        Some(value) => match parse_range_header(value) {
            Some(spec) => Some(spec),
            None => {
                let _ =
                    send_error(&mut stream, 416, "Range Not Satisfiable", "不支持的 Range 头").await;
                return Ok(());
            }
        },
    };

    let requested_start = match spec {
        None => Some(0),
        Some(RangeSpec::From(start)) => Some(start),
        Some(RangeSpec::Bounded(start, _)) => Some(start),
        Some(RangeSpec::Suffix(_)) => None,
    };
    // Sample this before any seek below: a seek keeps the buffer, but the
    // question "did the client wait for upstream?" is still answered now.
    let buffered_memory = requested_start.is_some_and(|start| engine.window_covers(start));
    let buffered_disk = requested_start.is_some_and(|start| engine.disk_covers(start));
    let buffered_at_request = buffered_memory || buffered_disk;
    if buffered_at_request {
        if let Some(start) = requested_start {
            if buffered_disk {
                crate::debug_log(&format!(
                    "seek hit offset={start} src=disk ms={} upstream=0",
                    request_started.elapsed().as_millis()
                ));
            } else {
                crate::debug_log(&format!(
                    "seek hit offset={start} src=window ms={} upstream=0",
                    request_started.elapsed().as_millis()
                ));
            }
        }
    }

    // Fast path, before the response lock: a warmed seek point is answered
    // from memory. It must not wait behind an older open-ended response (a
    // seek makes ffmpeg abandon one, and that response can hold the lock until
    // its socket errors), and it must not move the read cursor or clear the
    // window — playback keeps its buffer.
    if request.method == "GET" {
        if let Some(hit) = point_hit(engine, spec, requested_start, request_started) {
            // The point bytes go out before the lock is taken (a warm seek must
            // not wait behind an older body), so this request claims the proxy
            // here: the response it supersedes stops at its next write poll.
            let epoch = engine.begin_response();
            match point_serve(&mut stream, engine, shutdown, epoch, hit).await {
                PointServe::Done => return Ok(()),
                PointServe::Continue {
                    pos,
                    remaining,
                    chunked,
                } => {
                    let (_guard, epoch) = match acquire_serve(engine, shutdown).await {
                        Some(acquired) => acquired,
                        // Headers and the point bytes are already out; closing
                        // now is the honest signal that the body is short.
                        None => return Err("代理已关闭".to_string()),
                    };
                    engine.seek(pos);
                    return serve_body(
                        &mut stream, engine, shutdown, epoch, pos, remaining, chunked,
                    )
                    .await;
                }
            }
        }
    }

    let (_guard, epoch) = match acquire_serve(engine, shutdown).await {
        Some(acquired) => acquired,
        None => return Err("代理已关闭".to_string()),
    };
    // The epoch is what an older body checks itself against. Logging it with
    // the range makes "this body was superseded by which request" answerable
    // from the log alone.
    crate::debug_log(&format!(
        "serve claim epoch={epoch} range={} waited_ms={}",
        request.range.as_deref().unwrap_or("-"),
        request_started.elapsed().as_millis()
    ));

    // Move the read-ahead window to the client's offset before waiting: the
    // workers claim their next chunks from the new cursor, so this request's
    // bytes are exactly what they are fetching. Only a suffix range names no
    // start; it needs the total first and seeks after that resolves.
    if let Some(start) = requested_start {
        engine.seek(start);
    }

    let (total, content_type) = match await_first_metadata(engine, requested_start, spec, shutdown)
        .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = send_error(&mut stream, 504, "Gateway Timeout", &error).await;
            return Err(error);
        }
    };
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    let Some(range) = resolve_range(spec, total) else {
        let suffix = total.map_or("*".to_string(), |total| total.to_string());
        let head = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{suffix}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = write_all(&mut stream, head.as_bytes()).await;
        return Ok(());
    };

    // A no-op for every anchored range (already seeked above); this is what
    // moves a suffix range, whose start was only known after the metadata.
    engine.seek(range.start);

    let length = range.end.map(|end| end - range.start + 1);
    let chunked = length.is_none();
    let status = if request.range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let head = response_head(
        &status,
        &content_type,
        range,
        total,
        length,
        chunked,
        request.range.is_some(),
    );
    // `window` = the requested offset was already buffered when the request
    // arrived, `disk` = it came out of the session cache file, `upstream` =
    // this response waited for a network read.
    let source = if buffered_disk {
        "disk"
    } else if buffered_memory {
        "window"
    } else {
        "upstream"
    };
    serve_trace(spec, range, total, source, request_started);
    write_all(&mut stream, head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;

    if request.method == "HEAD" {
        return Ok(());
    }

    let pos = range.start;
    let remaining = length;
    serve_body(&mut stream, engine, shutdown, epoch, pos, remaining, chunked).await
}

/// Result of the point-cache fast path.
enum PointServe {
    /// The whole request was answered from memory.
    Done,
    /// The point bytes are written; the rest of the range needs the window.
    Continue {
        pos: u64,
        remaining: Option<u64>,
        chunked: bool,
    },
}

/// A request resolved from a warmed seek point, before anything is written.
/// Head and bytes are decided together, so a `206` can never promise a range
/// the point does not actually start with.
struct PointHit {
    head: String,
    range: ResolvedRange,
    /// Bytes promised from `range.start` to `range.end` (the point has the
    /// total, so the length is always known).
    length: u64,
    data: Arc<Vec<u8>>,
    /// Offset of the response start inside `data`.
    skip: usize,
}

/// Answer a request that starts inside a warmed seek point, without touching
/// the read cursor or the read-ahead window. `None` = no point covers the
/// offset (or the response cannot be shaped yet), so the normal path serves it.
fn point_hit(
    engine: &Engine,
    spec: Option<RangeSpec>,
    requested_start: Option<u64>,
    request_started: Instant,
) -> Option<PointHit> {
    let start = requested_start?;
    let (_, data, skip) = engine.point_lookup(start)?;
    let (total, content_type) = engine.metadata()?;
    let range = resolve_range(spec, Some(total))?;
    let length = range.end.map(|end| end - range.start + 1)?;
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let status = if spec.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let head = response_head(
        &status,
        &content_type,
        range,
        Some(total),
        Some(length),
        false,
        spec.is_some(),
    );
    serve_trace(spec, range, Some(total), "point", request_started);
    crate::debug_log(&format!(
        "seek hit offset={start} src=cache ms={}",
        request_started.elapsed().as_millis()
    ));
    Some(PointHit {
        head,
        range,
        length,
        data,
        skip,
    })
}

/// Write one resolved point answer: head, the cached bytes, and (when the
/// requested range reaches past the point) a hand-off to the window.
async fn point_serve(
    stream: &mut TcpStream,
    engine: &Engine,
    shutdown: &AtomicBool,
    epoch: u64,
    hit: PointHit,
) -> PointServe {
    if let Err(error) = write_all_live(stream, engine, epoch, hit.head.as_bytes()).await {
        crate::debug_log(&format!("prefetch point head err={error}"));
        return PointServe::Done;
    }
    let available = (hit.data.len().saturating_sub(hit.skip)) as u64;
    let want = hit.length.min(available);
    if want == 0 {
        return PointServe::Done;
    }
    let bytes = &hit.data[hit.skip..hit.skip + want as usize];
    if write_all_live(stream, engine, epoch, bytes).await.is_err() {
        return PointServe::Done;
    }
    engine.note_point_hit(hit.range.start, want);
    let pos = hit.range.start + want;
    let remaining = hit.length - want;
    if remaining == 0 || shutdown.load(Ordering::SeqCst) {
        return PointServe::Done;
    }
    PointServe::Continue {
        pos,
        remaining: Some(remaining),
        chunked: false,
    }
}

/// Response head for one resolved range (shared by the window and point paths).
fn response_head(
    status: &str,
    content_type: &str,
    range: ResolvedRange,
    total: Option<u64>,
    length: Option<u64>,
    chunked: bool,
    ranged: bool,
) -> String {
    let mut head =
        format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\n");
    if let Some(length) = length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    if ranged {
        head.push_str(&format!(
            "Content-Range: {}\r\n",
            content_range_header(range, total)
        ));
    }
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    head
}

/// Write one body chunk, framing it when the response is chunked. Superseded
/// responses stop here instead of writing into a socket nobody reads.
async fn write_chunk_live(
    stream: &mut TcpStream,
    engine: &Engine,
    epoch: u64,
    chunked: bool,
    data: &[u8],
) -> io::Result<()> {
    if chunked {
        write_all_live(
            stream,
            engine,
            epoch,
            format!("{:x}\r\n", data.len()).as_bytes(),
        )
        .await?;
        write_all_live(stream, engine, epoch, data).await?;
        write_all_live(stream, engine, epoch, b"\r\n").await
    } else {
        write_all_live(stream, engine, epoch, data).await
    }
}

/// Write the body from `pos` out of the read-ahead window, then the session
/// cache file. The caller holds the serve guard and the window has already been
/// moved to `pos`.
///
/// Serving order is memory → **disk** → upstream: a block the cache file holds
/// is answered locally even when the network is dead, which is what makes a
/// cached seek cost zero upstream bytes.
///
/// When a newer request takes the proxy over (`epoch` is stale) the body simply
/// ends: the client abandoned it — a seek always opens a new connection — and
/// holding the lock while it is not read would starve that seek.
async fn serve_body(
    stream: &mut TcpStream,
    engine: &Engine,
    shutdown: &AtomicBool,
    epoch: u64,
    mut pos: u64,
    mut remaining: Option<u64>,
    chunked: bool,
) -> Result<(), String> {
    let started = Instant::now();
    let start = pos;
    let mut disk_logged = false;
    let mut disk_bytes = 0u64;
    let mut window_bytes = 0u64;
    // Times this response had to wait for a network read at all. 0 = the whole
    // body came out of memory/the session cache file, which is what a cached
    // seek must show.
    let mut upstream_waits = 0u64;
    // Why the body stopped. Every early end must name itself: a truncated
    // response is the "playback ends in the middle" bug, and a bare `serve
    // done` line could not tell a completed range from a superseded one.
    // Every exit from the loop names itself before breaking, so the compiler
    // can see the value is always set.
    let reason;
    // Throttle of the `serve wait` line (see the wait arm below), and of the
    // hot-priority enforcement it accompanies.
    let mut wait_logged: Option<Instant> = None;
    let mut last_enforce: Option<Instant> = None;
    let result: Result<(), String> = loop {
        if remaining == Some(0) {
            reason = "complete";
            break Ok(());
        }
        if !engine.is_current(epoch) {
            reason = "superseded";
            break Ok(());
        }
        let want = remaining.unwrap_or(u64::MAX);
        match engine.consume(pos, want) {
            Ok(Some(data)) => {
                let written = data.len() as u64;
                if let Err(error) =
                    write_chunk_live(stream, engine, epoch, chunked, &data).await
                {
                    // A write that failed because we were superseded is the
                    // normal end of an abandoned response, not an error.
                    if !engine.is_current(epoch) {
                        reason = "write-superseded";
                        break Ok(());
                    }
                    reason = write_end_reason(&error);
                    break Err(error.to_string());
                }
                window_bytes = window_bytes.saturating_add(written);
                pos += written;
                remaining = remaining.map(|left| left - written);
                // Bytes landed: the wait (if any) is over. `available` is the
                // only ending that cost the client the hot window's priority.
                engine.close_serve_wait("available");
                wait_logged = None;
                last_enforce = None;
            }
            Ok(None) | Err(_) => {
                // Not in the hot window: try the session cache file before
                // waiting on the network (or giving up on a dead range).
                if let Some(data) = engine.disk_read(pos, want) {
                    let written = data.len() as u64;
                    if !disk_logged {
                        disk_logged = true;
                        if serve_log_enabled() {
                            crate::debug_log(&format!(
                                "serve src=disk pos={pos} bytes={written}"
                            ));
                        }
                    }
                    if let Err(error) =
                        write_chunk_live(stream, engine, epoch, chunked, &data).await
                    {
                        if !engine.is_current(epoch) {
                            reason = "write-superseded";
                            break Ok(());
                        }
                        reason = write_end_reason(&error);
                        break Err(error.to_string());
                    }
                    engine.note_disk_served(pos, written);
                    disk_bytes = disk_bytes.saturating_add(written);
                    pos += written;
                    remaining = remaining.map(|left| left - written);
                    // Local bytes: a wait that was open is satisfied without
                    // the network, which is exactly what a cached seek shows.
                    engine.close_serve_wait("available");
                    wait_logged = None;
                    last_enforce = None;
                    continue;
                }
                if shutdown.load(Ordering::SeqCst) {
                    reason = "shutdown";
                    break Err("代理已关闭".to_string());
                }
                if !engine.is_current(epoch) {
                    reason = "superseded-wait";
                    break Ok(());
                }
                if engine.at_eof(pos) {
                    // The upstream ended before the promised length (only
                    // reachable when the total never came from a response).
                    // Closing here is what tells the client the body is short.
                    reason = "eof";
                    break Ok(());
                }
                if let Err(error) = engine.consume_error(pos) {
                    reason = "upstream-error";
                    break Err(error);
                }
                upstream_waits = upstream_waits.saturating_add(1);
                // A hole the network should be filling. From here on the serve
                // path has absolute priority: the wait is registered (which
                // suspends speculative claims), and every 20 ms the scheduler
                // cancels in-flight speculative work so a worker frees up for
                // this chunk, and re-routes a silent hot range after
                // HOT_STUCK_MS instead of after the 5 s health timer.
                let waited_ms = engine.open_serve_wait(pos);
                let enforce_due = last_enforce
                    .map_or(true, |last| last.elapsed() >= SERVE_WAIT_ENFORCE_POLL);
                if enforce_due {
                    last_enforce = Some(Instant::now());
                    engine.enforce_serve_priority(pos, waited_ms);
                }
                // Say so once per second (and at the first miss) rather than
                // waiting in silence: this is the line that tells a proxy stall
                // apart from a decoder stall, and names what the workers were
                // doing instead of serving this offset.
                let log_due = wait_logged
                    .map_or(true, |last| last.elapsed() >= Duration::from_secs(1));
                if log_due {
                    wait_logged = Some(Instant::now());
                    let (hot, warm, rest) = engine.inflight_kinds();
                    crate::debug_log(&format!(
                        "serve wait pos={pos} waited_ms={waited_ms} hot_inflight={hot} warm_inflight={warm} rest_inflight={rest} buffered={} total={:?}",
                        engine.read_ahead(),
                        engine.total()
                    ));
                }
                tokio::time::sleep(WAIT_POLL).await;
            }
        }
    };
    // A body that ends while still waiting closes the span as `timeout` — the
    // bytes never came, whatever ended the body (supercede, error, EOF,
    // shutdown). The successful path already closed it as `available`.
    engine.close_serve_wait("timeout");
    if chunked && result.is_ok() {
        write_all(stream, b"0\r\n\r\n")
            .await
            .map_err(|error| error.to_string())?;
    }
    // One line per response: how much of it was local, whether it ever needed
    // the network, and *why* it ended. A cached seek must read
    // `upstream_waits=0`; a playback body must read `reason=complete`.
    crate::debug_log(&format!(
        "serve end reason={reason} from={start} pos={pos} sent={} window={window_bytes} disk={disk_bytes} total={:?} expected={:?} epoch={epoch} current={} upstream_waits={upstream_waits} ms={}",
        pos.saturating_sub(start),
        engine.total(),
        remaining.map(|left| pos.saturating_add(left)),
        engine.is_current(epoch),
        started.elapsed().as_millis()
    ));
    result
}

// ---------------------------------------------------------------------------
// QA hook: driven by the `#[ignore]`d live tests below, never by the app.
// ---------------------------------------------------------------------------

#[cfg(test)]
const QA_PREFIX_BYTES: u64 = 8 << 20;
#[cfg(test)]
const QA_SEEK_BYTES: u64 = 2 << 20;

#[cfg(test)]
struct QaRead {
    content_range: String,
    bytes: u64,
    ms: u128,
    first_byte_ms: u128,
    hash_all: String,
    hash_prefix: String,
    max_rate: u64,
    max_ahead: u64,
}

/// Read exactly `want` bytes of one HTTP range from `url`, hashing the prefix
/// and the whole sample; when `handle` is given, sample the proxy stats while
/// reading.
#[cfg(test)]
async fn qa_read(
    client: &reqwest::Client,
    url: &str,
    range: &str,
    want: u64,
    handle: Option<&PrefetchHandle>,
) -> Result<QaRead> {
    use sha2::{Digest, Sha256};

    let started = Instant::now();
    let mut response = client
        .get(url)
        .header(RANGE, range)
        .send()
        .await
        .with_context(|| format!("请求失败：{url} ({range})"))?;
    let status = response.status();
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    if status != StatusCode::PARTIAL_CONTENT {
        bail!("{url}: HTTP {status} Content-Range={content_range}");
    }

    let mut first_byte = None;
    let mut total = 0u64;
    let mut all = Sha256::new();
    let mut prefix = Sha256::new();
    let mut max_rate = 0u64;
    let mut max_ahead = 0u64;
    let mut since_sample = 0u64;
    while total < want {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let take = ((want - total) as usize).min(chunk.len());
        all.update(&chunk[..take]);
        let prefix_room = (QA_PREFIX_BYTES.saturating_sub(total)).min(take as u64) as usize;
        if prefix_room > 0 {
            prefix.update(&chunk[..prefix_room]);
        }
        total += take as u64;
        if first_byte.is_none() && take > 0 {
            first_byte = Some(started.elapsed());
        }
        if let Some(handle) = handle {
            since_sample += take as u64;
            if since_sample >= 256 << 10 {
                since_sample = 0;
                let stats = handle.snapshot();
                max_rate = max_rate.max(stats.bytes_per_sec);
                max_ahead = max_ahead.max(stats.read_ahead_bytes);
            }
        }
    }
    Ok(QaRead {
        content_range,
        bytes: total,
        ms: started.elapsed().as_millis(),
        first_byte_ms: first_byte.unwrap_or_default().as_millis(),
        hash_all: hex::encode(all.finalize()),
        hash_prefix: hex::encode(prefix.finalize()),
        max_rate,
        max_ahead,
    })
}

#[cfg(test)]
fn qa_mbps(bytes: u64, ms: u128) -> f64 {
    if ms == 0 {
        return 0.0;
    }
    bytes as f64 * 8.0 / (ms as f64 / 1000.0) / 1_000_000.0
}

#[cfg(test)]
fn qa_log_contains(needle: &str) -> bool {
    std::env::var("BILI_DEBUG_LOG")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.lines().any(|line| line.contains(needle)))
        .unwrap_or(false)
}

/// Headless end-to-end check of one proxy instance against the real CDN:
/// parallel reads, hash equality with a direct fetch, a mid-file seek, the
/// stats split and shutdown cleanup. Returns a human-readable report.
///
/// This is the "plain HTTP client instead of ffmpeg" harness required for the
/// first pass; the player can call it from a diagnostic env hook later. Call
/// it from a synchronous thread.
#[cfg(test)]
pub fn qa_verify(
    url: &str,
    hosts: &[String],
    sample_bytes: u64,
    seek_offset: u64,
) -> Result<String> {
    let target = Target::parse(url)?;
    let direct_origin = match hosts.first() {
        Some(host) => HostSpec::parse(host, &target.scheme)?.origin,
        None => target.origin.clone(),
    };
    let direct_url = forward_url(&target, &direct_origin);
    let runtime = crate::bilibili::shared_runtime()
        .ok_or_else(|| anyhow!("网络运行时不可用"))?;

    let (handle, report) = runtime.block_on(async move {
        let config = PrefetchConfig {
            hosts: hosts.to_vec(),
            ..PrefetchConfig::default()
        };
        let handle = spawn(url, config)?;
        let local = handle.local_url.clone();
        // The CDN wants the same Referer/UA the player sends; without them a
        // direct media GET answers 403.
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, HeaderValue::from_static("https://www.bilibili.com/"));
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let client = reqwest::Client::builder()
            .user_agent(crate::bilibili::UA)
            .default_headers(headers)
            .timeout(Duration::from_secs(180))
            .build()?;

        // Three alternating A/B rounds at different offsets: the CDN's
        // per-connection throughput varies enough that one pair is noisy.
        let mut rounds: Vec<(QaRead, QaRead)> = Vec::new();
        for offset in [0u64, 8 << 20, 16 << 20] {
            let proxy = qa_read(
                &client,
                &local,
                &format!("bytes={offset}-"),
                sample_bytes,
                Some(&handle),
            )
            .await?;
            let direct = qa_read(
                &client,
                &direct_url,
                &format!("bytes={offset}-{}", offset + sample_bytes.saturating_sub(1)),
                sample_bytes,
                None,
            )
            .await?;
            rounds.push((proxy, direct));
        }
        let seek_proxy = qa_read(
            &client,
            &local,
            &format!("bytes={seek_offset}-"),
            QA_SEEK_BYTES,
            Some(&handle),
        )
        .await?;
        let seek_direct = qa_read(
            &client,
            &direct_url,
            &format!(
                "bytes={seek_offset}-{}",
                seek_offset + QA_SEEK_BYTES.saturating_sub(1)
            ),
            QA_SEEK_BYTES,
            None,
        )
        .await?;

        // Bounded range: exercises 206 + Content-Length and leaves a partial
        // chunk buffered behind the cursor.
        let bounded_start = 1 << 20;
        let bounded_len = 4096u64;
        let bounded_proxy = qa_read(
            &client,
            &local,
            &format!(
                "bytes={bounded_start}-{}",
                bounded_start + bounded_len - 1
            ),
            bounded_len,
            Some(&handle),
        )
        .await?;
        let bounded_direct = qa_read(
            &client,
            &direct_url,
            &format!(
                "bytes={bounded_start}-{}",
                bounded_start + bounded_len - 1
            ),
            bounded_len,
            None,
        )
        .await?;

        // Only the configured path+query is proxied.
        let port = reqwest::Url::parse(&local)
            .ok()
            .and_then(|url| url.port())
            .unwrap_or(0);
        let forbidden_status = client
            .get(format!("http://127.0.0.1:{port}/not-the-media"))
            .send()
            .await?
            .status()
            .as_u16();
        let unsatisfiable_status = client
            .get(&local)
            .header(RANGE, "bytes=999999999-")
            .send()
            .await?
            .status()
            .as_u16();

        // Playback resumes at the start and then pauses: the read-ahead window
        // fills to its limit (all workers idle, `active_workers == 0`) while the
        // warm task keeps landing seek points. This is the state the panel and
        // the `BILI_PREFETCH_STATS_TEST` probe must describe.
        let resume = qa_read(&client, &local, "bytes=0-", 2 << 20, Some(&handle)).await?;
        tokio::time::sleep(Duration::from_secs(4)).await;

        let stats = handle.snapshot();
        let proxy_ms: u128 = rounds.iter().map(|(proxy, _)| proxy.ms).sum();
        let direct_ms: u128 = rounds.iter().map(|(_, direct)| direct.ms).sum();
        let speedup = if proxy_ms == 0 {
            0.0
        } else {
            direct_ms as f64 / proxy_ms as f64
        };
        let mut report = String::new();
        report.push_str(&format!("local_url={local}\n"));
        report.push_str(&format!("direct_url={direct_url}\n"));
        let (first_proxy, first_direct) = &rounds[0];
        report.push_str(&format!(
            "proxy_prefix8m_sha256={} direct_prefix8m_sha256={} hash_equal_prefix8m={}\n",
            first_proxy.hash_prefix,
            first_direct.hash_prefix,
            first_proxy.hash_prefix == first_direct.hash_prefix
        ));
        for (round, (proxy, direct)) in rounds.iter().enumerate() {
            report.push_str(&format!(
                "round={round} proxy bytes={} ms={} first_byte_ms={} mbps={:.2} content_range={} | direct bytes={} ms={} first_byte_ms={} mbps={:.2} | hash_equal={} speedup={:.2}x\n",
                proxy.bytes,
                proxy.ms,
                proxy.first_byte_ms,
                qa_mbps(proxy.bytes, proxy.ms),
                proxy.content_range,
                direct.bytes,
                direct.ms,
                direct.first_byte_ms,
                qa_mbps(direct.bytes, direct.ms),
                proxy.hash_all == direct.hash_all,
                if proxy.ms == 0 {
                    0.0
                } else {
                    direct.ms as f64 / proxy.ms as f64
                }
            ));
        }
        report.push_str(&format!(
            "speedup_vs_single={:.2}x (proxy_ms={proxy_ms} direct_ms={direct_ms})\n",
            speedup
        ));
        report.push_str(&format!(
            "seek offset={seek_offset} proxy_bytes={} proxy_ms={} direct_bytes={} direct_ms={} hash_equal={} proxy_content_range={} direct_content_range={}\n",
            seek_proxy.bytes,
            seek_proxy.ms,
            seek_direct.bytes,
            seek_direct.ms,
            seek_proxy.hash_all == seek_direct.hash_all,
            seek_proxy.content_range,
            seek_direct.content_range
        ));
        report.push_str(&format!(
            "bounded range={bounded_start}+{bounded_len} proxy_bytes={} proxy_ms={} direct_bytes={} direct_ms={} hash_equal={} proxy_content_range={} direct_content_range={}\n",
            bounded_proxy.bytes,
            bounded_proxy.ms,
            bounded_direct.bytes,
            bounded_direct.ms,
            bounded_proxy.hash_all == bounded_direct.hash_all,
            bounded_proxy.content_range,
            bounded_direct.content_range
        ));
        report.push_str(&format!(
            "path_rejected_status={forbidden_status} unsatisfiable_status={unsatisfiable_status}\n"
        ));
        report.push_str(&format!(
            "stats bytes_fetched={} bytes_per_sec={} total={:?} read_ahead={} failed_ranges={} warm_done={}/{} point_hits={} point_bytes={} point_cache={} hosts={:?}\n",
            stats.bytes_fetched,
            stats.bytes_per_sec,
            stats.bytes_total,
            stats.read_ahead_bytes,
            stats.failed_ranges,
            stats.warm_points_done,
            stats.warm_points_total,
            stats.point_hits,
            stats.point_bytes,
            stats.point_cache_bytes,
            stats.hosts
        ));
        report.push_str(&format!(
            "workers n={} active={} ranges=[{}]\n",
            stats.workers.len(),
            stats.active_workers,
            stats
                .workers
                .iter()
                .map(|worker| format!(
                    "{} {} {}-{}{}",
                    worker.id,
                    worker.host,
                    worker.start,
                    worker.end,
                    if worker.busy { "" } else { " idle" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        report.push_str(&format!(
            "downloaded ranges={} bytes={} point_ranges={} point_bytes={}\n",
            stats.downloaded.len(),
            ranges_bytes(&stats.downloaded),
            stats.point_cache.len(),
            ranges_bytes(&stats.point_cache)
        ));
        report.push_str(&format!(
            "resume bytes={} content_range={} window={} active={}\n",
            resume.bytes, resume.content_range, stats.read_ahead_bytes, stats.active_workers
        ));
        report.push_str(&format!(
            "peak_bytes_per_sec={} peak_read_ahead={}\n",
            rounds
                .iter()
                .map(|(proxy, _)| proxy.max_rate)
                .chain(std::iter::once(seek_proxy.max_rate))
                .max()
                .unwrap_or(0),
            rounds
                .iter()
                .map(|(proxy, _)| proxy.max_ahead)
                .chain(std::iter::once(seek_proxy.max_ahead))
                .max()
                .unwrap_or(0)
        ));
        Ok::<_, anyhow::Error>((handle, report))
    })?;

    // Shutdown outside the runtime: `PrefetchHandle::shutdown` joins the
    // server task with `block_on`.
    let mut handle = handle;
    let port = reqwest::Url::parse(&handle.local_url)
        .ok()
        .and_then(|url| url.port())
        .unwrap_or(0);
    handle.shutdown();
    std::thread::sleep(Duration::from_millis(100));
    let address = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let closed = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err();
    let stopped = qa_log_contains("prefetch server stopped");
    let seek_reset = qa_log_contains(&format!("prefetch seek reset offset={seek_offset}"));
    let mut report = report;
    report.push_str(&format!(
        "cleanup port={port} port_closed={closed} server_stopped_logged={stopped} seek_reset_logged={seek_reset}\n"
    ));
    for line in report.lines() {
        crate::debug_log(&format!("prefetch qa {line}"));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Unit tests (no network, no ffmpeg)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(start: u64, len: usize) -> (u64, Vec<u8>) {
        (start, vec![0u8; len])
    }

    #[test]
    fn range_header_forms() {
        assert_eq!(parse_range_header("bytes=0-"), Some(RangeSpec::From(0)));
        assert_eq!(
            parse_range_header("bytes=123-"),
            Some(RangeSpec::From(123))
        );
        assert_eq!(
            parse_range_header("bytes=100-199"),
            Some(RangeSpec::Bounded(100, 199))
        );
        assert_eq!(
            parse_range_header("  bytes=100-199  "),
            Some(RangeSpec::Bounded(100, 199))
        );
        assert_eq!(
            parse_range_header("bytes=-4096"),
            Some(RangeSpec::Suffix(4096))
        );
        assert_eq!(parse_range_header("bytes=0-0"), Some(RangeSpec::Bounded(0, 0)));
        assert_eq!(parse_range_header("bytes=9-3"), None);
        assert_eq!(parse_range_header("bytes=abc-"), None);
        assert_eq!(parse_range_header("bytes=0-1,4-5"), None);
        assert_eq!(parse_range_header("items=0-"), None);
        assert_eq!(parse_range_header("bytes=-0"), None);
    }

    #[test]
    fn content_range_total_parses() {
        assert_eq!(content_range_total_str("bytes 0-0/12345"), Some(12345));
        assert_eq!(content_range_total_str("bytes 10-19/987654"), Some(987654));
        assert_eq!(content_range_total_str("bytes 0-0/*"), None);
        assert_eq!(content_range_total_str("garbage"), None);
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/42"));
        assert_eq!(content_range_total(&headers), Some(42));
    }

    #[test]
    fn range_resolution_clamps_and_rejects() {
        assert_eq!(
            resolve_range(None, Some(100)),
            Some(ResolvedRange {
                start: 0,
                end: Some(99)
            })
        );
        assert_eq!(resolve_range(None, None).map(|r| r.start), Some(0));
        assert_eq!(resolve_range(None, Some(0)), None);
        assert_eq!(
            resolve_range(Some(RangeSpec::From(50)), Some(100)),
            Some(ResolvedRange {
                start: 50,
                end: Some(99)
            })
        );
        assert_eq!(resolve_range(Some(RangeSpec::From(100)), Some(100)), None);
        assert_eq!(
            resolve_range(Some(RangeSpec::Bounded(10, 500)), Some(100)),
            Some(ResolvedRange {
                start: 10,
                end: Some(99)
            })
        );
        assert_eq!(
            resolve_range(Some(RangeSpec::Bounded(10, 20)), None),
            Some(ResolvedRange {
                start: 10,
                end: Some(20)
            })
        );
        assert_eq!(
            resolve_range(Some(RangeSpec::Suffix(10)), Some(100)),
            Some(ResolvedRange {
                start: 90,
                end: Some(99)
            })
        );
        assert_eq!(resolve_range(Some(RangeSpec::Suffix(10)), None), None);
        assert_eq!(
            content_range_header(
                ResolvedRange {
                    start: 10,
                    end: Some(99)
                },
                Some(500)
            ),
            "bytes 10-99/500"
        );
        assert_eq!(
            content_range_header(
                ResolvedRange {
                    start: 10,
                    end: None
                },
                None
            ),
            "bytes 10-*/*"
        );
    }

    #[test]
    fn host_rewrite_keeps_path_and_query() {
        let target =
            Target::parse("https://upos-sz.bilivideo.com/upgcxcode/1/2/x.m4s?e=1&u=2").unwrap();
        assert_eq!(target.path, "/upgcxcode/1/2/x.m4s");
        assert_eq!(target.query, "e=1&u=2");
        assert_eq!(target.host_label, "upos-sz.bilivideo.com");
        assert_eq!(
            forward_url(&target, "https://mirror.example.com"),
            "https://mirror.example.com/upgcxcode/1/2/x.m4s?e=1&u=2"
        );
        let no_query = Target::parse("https://cdn.example.com/a/b.m4s").unwrap();
        assert_eq!(
            forward_url(&no_query, "https://mirror.example.com"),
            "https://mirror.example.com/a/b.m4s"
        );
        let spec = HostSpec::parse("mirror.example.com:8443", "https").unwrap();
        assert_eq!(spec.label, "mirror.example.com:8443");
        assert_eq!(spec.origin, "https://mirror.example.com:8443");
        let from_url = HostSpec::parse("http://upos-hz.mirror.com/x/y?q=1", "https").unwrap();
        assert_eq!(from_url.origin, "http://upos-hz.mirror.com");
        assert_eq!(from_url.label, "upos-hz.mirror.com");
    }

    #[test]
    fn resource_paths_never_rewrite_host() {
        let target =
            Target::parse("https://api.example.com/v1/resource/abc?token=1").unwrap();
        assert_eq!(
            forward_url(&target, "https://mirror.example.com"),
            "https://api.example.com/v1/resource/abc?token=1"
        );
    }

    #[test]
    fn request_target_split_and_local_url_shape() {
        assert_eq!(
            split_target("/a/b.m4s?e=1&u=2"),
            Some(("/a/b.m4s".to_string(), "e=1&u=2".to_string()))
        );
        assert_eq!(
            split_target("http://127.0.0.1:9/a/b.m4s?x=1"),
            Some(("/a/b.m4s".to_string(), "x=1".to_string()))
        );
        assert_eq!(
            split_target("/a/b.m4s"),
            Some(("/a/b.m4s".to_string(), String::new()))
        );
        assert_eq!(split_target("nonsense"), None);
        let target = Target::parse("https://h/p?q=1").unwrap();
        let local = format!(
            "http://127.0.0.1:1234{}{}",
            target.path,
            if target.query.is_empty() {
                String::new()
            } else {
                format!("?{}", target.query)
            }
        );
        assert_eq!(local, "http://127.0.0.1:1234/p?q=1");
    }

    #[test]
    fn chunk_window_selection() {
        let empty = BTreeMap::new();
        let no_inflight = BTreeSet::new();
        let no_dead = BTreeSet::new();
        // Fresh window starts at the cursor.
        assert_eq!(
            next_chunk_offset(0, 0, 4, 12, Some(100), &empty, &no_inflight, &no_dead, 0),
            Some(0)
        );
        // Fetched chunk is skipped, the next gap is chosen.
        let mut chunks = BTreeMap::new();
        chunks.insert(chunk(0, 4).0, chunk(0, 4).1);
        assert_eq!(
            next_chunk_offset(0, 0, 4, 12, Some(100), &chunks, &no_inflight, &no_dead, 0),
            Some(4)
        );
        // In-flight chunks are skipped too.
        chunks.insert(chunk(4, 4).0, chunk(4, 4).1);
        let mut inflight = BTreeSet::new();
        inflight.insert((0, 8));
        assert_eq!(
            next_chunk_offset(0, 0, 4, 16, Some(100), &chunks, &inflight, &no_dead, 0),
            Some(12)
        );
        // A stale generation does not claim the current window's work.
        assert_eq!(
            next_chunk_offset(0, 0, 4, 16, Some(100), &chunks, &inflight, &no_dead, 1),
            Some(8)
        );
        // Window end limits the look-ahead.
        assert_eq!(
            next_chunk_offset(0, 0, 4, 8, Some(100), &chunks, &no_inflight, &no_dead, 0),
            None
        );
        // Total length limits the window and the file end stops it.
        let mut tail = BTreeMap::new();
        tail.insert(0, vec![0u8; 4]);
        assert_eq!(
            next_chunk_offset(0, 4, 4, 32, Some(6), &tail, &no_inflight, &no_dead, 0),
            Some(4)
        );
        tail.insert(4, vec![0u8; 2]);
        assert_eq!(
            next_chunk_offset(0, 4, 4, 32, Some(6), &tail, &no_inflight, &no_dead, 0),
            None
        );
        // Dead ranges are not retried.
        let mut dead = BTreeSet::new();
        dead.insert((0, 4));
        assert_eq!(
            next_chunk_offset(0, 4, 4, 32, Some(100), &tail, &no_inflight, &dead, 0),
            Some(8)
        );
        // A partially served chunk keeps the covered tail from being refetched.
        let mut partial = BTreeMap::new();
        partial.insert(4, vec![0u8; 4]);
        assert_eq!(
            next_chunk_offset(4, 6, 4, 32, Some(100), &partial, &no_inflight, &no_dead, 0),
            Some(8)
        );
    }

    #[test]
    fn merge_ranges_coalesces_adjacent_and_overlapping() {
        // Nothing to merge.
        assert!(merge_ranges(Vec::new(), MAX_DOWNLOADED_RANGES).is_empty());
        // Empty and inverted ranges carry no bytes and are dropped.
        assert!(merge_ranges(vec![(10, 10), (20, 19)], MAX_DOWNLOADED_RANGES).is_empty());
        // Adjacent chunks (the normal window shape) are one range.
        assert_eq!(
            merge_ranges(vec![(0, 4), (4, 8), (8, 12)], MAX_DOWNLOADED_RANGES),
            vec![(0, 12)]
        );
        // Overlap extends the end, a contained range does not.
        assert_eq!(
            merge_ranges(
                vec![(0, 10), (5, 12), (20, 30), (22, 25)],
                MAX_DOWNLOADED_RANGES
            ),
            vec![(0, 12), (20, 30)]
        );
        assert_eq!(
            merge_ranges(vec![(0, 100), (10, 20)], MAX_DOWNLOADED_RANGES),
            vec![(0, 100)]
        );
        // Input order never matters: the result is always sorted.
        assert_eq!(
            merge_ranges(vec![(20, 30), (0, 4), (4, 20)], MAX_DOWNLOADED_RANGES),
            vec![(0, 30)]
        );
        // A gap of exactly one byte stays a gap (ranges are half-open).
        assert_eq!(
            merge_ranges(vec![(0, 10), (11, 20)], MAX_DOWNLOADED_RANGES),
            vec![(0, 10), (11, 20)]
        );
    }

    #[test]
    fn merge_ranges_caps_by_merging_the_smallest_gap_first() {
        // The small gap (10..20) is bridged; the large hole (30..100) survives.
        assert_eq!(
            merge_ranges(vec![(0, 10), (20, 30), (100, 110)], 2),
            vec![(0, 30), (100, 110)]
        );
        // Ties are resolved left to right, so the tail keeps its detail.
        let merged = merge_ranges(
            (0..8u64).map(|index| (index * 20, index * 20 + 10)).collect(),
            3,
        );
        assert_eq!(merged.len(), 3);
        assert!(merged.windows(2).all(|pair| pair[0].1 <= pair[1].0));
        assert_eq!(merged.first().copied(), Some((0, 110)));
        assert_eq!(merged.last().copied(), Some((140, 150)));
        // The cap always holds, down to a single range. Every gap is equal
        // here, so the leftmost pair is bridged first and the tail survives.
        let many: Vec<(u64, u64)> = (0..500u64).map(|index| (index * 10, index * 10 + 1)).collect();
        let merged = merge_ranges(many.clone(), MAX_DOWNLOADED_RANGES);
        assert_eq!(merged.len(), MAX_DOWNLOADED_RANGES);
        assert_eq!(merged.first().map(|range| range.0), Some(0));
        assert_eq!(merged.last().copied(), Some((4990, 4991)));
        assert!(merged.windows(2).all(|pair| pair[0].1 < pair[1].0));
        let single = merge_ranges(many, 1);
        assert_eq!(single, vec![(0, 4991)]);
    }

    #[test]
    fn merge_ranges_keeps_a_full_window_as_one_entry() {
        // Worst-case fragmentation of a 128 MiB window at 256 KiB chunks: 512
        // entries in, one contiguous entry out (well under the 64 cap).
        let chunk = 256 << 10;
        let ranges: Vec<(u64, u64)> = (0..512)
            .map(|index| (index * chunk, (index + 1) * chunk))
            .collect();
        assert_eq!(merge_ranges(ranges, MAX_DOWNLOADED_RANGES), vec![(0, 512 * chunk)]);
    }

    #[test]
    fn published_downloaded_is_the_window_plus_point_cache() {
        let engine = test_engine();
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(1000);
            // Two adjacent window chunks: one merged range.
            state.chunks.insert(0, vec![0u8; 100]);
            state.chunks.insert(100, vec![0u8; 50]);
            // A warm point elsewhere: its own range.
            state
                .points
                .insert(400, vec![7u8; 10], 10, MAX_POINT_CACHE_BYTES);
            engine.publish(&mut state, Instant::now());
        }
        let stats = engine.snapshot();
        assert_eq!(stats.downloaded, vec![(0, 150), (400, 410)]);
        assert_eq!(stats.point_cache, vec![(400, 410)]);
        assert_eq!(stats.point_cache_bytes, 10);
        assert_eq!(stats.total_bytes, Some(1000));
        assert_eq!(stats.bytes_total, Some(1000));
        assert_eq!(ranges_bytes(&stats.downloaded), 160);
        assert_eq!(range_overlaps(&stats.downloaded), 0);
    }

    #[test]
    fn serve_wait_claims_the_exact_blocked_offset() {
        let mut engine = test_engine();
        engine.config.chunk_bytes = 4;
        engine.config.read_ahead_bytes = 8;
        // A 1 B/s stream: the hot frontier is 12 bytes, so chunks 0/4/8 are
        // look-ahead. The blocked byte sits inside chunk 0, two bytes in.
        engine.media_bps.store(1, Ordering::Relaxed);
        engine.desired_connections.store(1, Ordering::SeqCst);
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(4000);
            state.cursor = 2;
        }
        let shutdown = AtomicBool::new(false);
        assert_eq!(engine.open_serve_wait(2), 0);
        engine.enforce_serve_priority(2, 0);
        {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(
                state.urgent.front().copied(),
                Some(2),
                "the blocked offset must be registered as an urgent claim"
            );
        }
        // The claim starts at the blocked byte, not at the chunk boundary: the
        // two-byte prefix is worthless until the wait is over.
        assert!(
            matches!(
                claim(&engine, &shutdown, 0),
                Claim::Fetch(claim) if claim.kind == ChunkKind::Hot && claim.start == 2 && claim.end == 6
            ),
            "the serve wait must be served from its own offset first"
        );

        // A wait already covered by an in-flight claim adds nothing.
        let engine = test_engine();
        engine.desired_connections.store(1, Ordering::SeqCst);
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(4000);
        }
        assert!(matches!(claim(&engine, &shutdown, 0), Claim::Fetch(_)));
        assert_eq!(engine.open_serve_wait(1), 0);
        engine.enforce_serve_priority(1, 0);
        {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(
                state.urgent.is_empty(),
                "an offset already being fetched needs no urgent claim"
            );
        }
    }

    #[test]
    fn only_a_slow_serve_wait_counts_against_the_connection_count() {
        // The governor must not halve the pool for the ordinary 5–14 ms miss a
        // healthy reader makes; only a wait that cost frames does.
        let engine = Arc::new(test_engine());
        engine.open_serve_wait(0);
        engine.close_serve_wait("available");
        assert_eq!(engine.serve_waits.load(Ordering::Relaxed), 1);
        assert_eq!(
            engine.serve_wait_slow.load(Ordering::Relaxed),
            0,
            "a wait closed in the same millisecond is not a stall"
        );
        engine.open_serve_wait(0);
        std::thread::sleep(Duration::from_millis(SERVE_WAIT_SLOW_MS + 50));
        engine.close_serve_wait("available");
        assert_eq!(engine.serve_waits.load(Ordering::Relaxed), 2);
        assert_eq!(
            engine.serve_wait_slow.load(Ordering::Relaxed),
            1,
            "a wait past SERVE_WAIT_SLOW_MS is the signal the governor acts on"
        );
    }

    #[test]
    fn serve_wait_suspends_speculative_claims_until_bytes_land() {        let mut engine = test_engine();
        engine.config.chunk_bytes = 4;
        engine.config.read_ahead_bytes = 8;
        // A 1 B/s stream puts the hot frontier 12 chunky bytes ahead (the
        // [`HOT_AHEAD_SECONDS`] floor is `chunk * 2`), so exactly three hot
        // chunks (0, 4, 8) are claimable before speculation starts.
        engine.media_bps.store(1, Ordering::Relaxed);
        engine.desired_connections.store(8, Ordering::SeqCst);
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(4000);
            state.warm_plan_started = true;
            state.warm_plan = vec![
                WarmTask {
                    point: 2000,
                    start: 2000,
                    end: 2004,
                    done: false,
                    claimed: false,
                    urgent: false,
                },
                WarmTask {
                    point: 3000,
                    start: 3000,
                    end: 3004,
                    done: false,
                    claimed: false,
                    urgent: false,
                },
            ];
        }
        let shutdown = AtomicBool::new(false);
        assert!(
            matches!(claim(&engine, &shutdown, 0), Claim::Fetch(claim) if claim.kind == ChunkKind::Hot && claim.start == 0)
        );
        assert!(
            matches!(claim(&engine, &shutdown, 1), Claim::Fetch(claim) if claim.kind == ChunkKind::Hot && claim.start == 4)
        );
        assert!(
            matches!(claim(&engine, &shutdown, 2), Claim::Fetch(claim) if claim.kind == ChunkKind::Hot && claim.start == 8)
        );
        // With the hot window fully in flight, the next worker speculates.
        assert!(
            matches!(claim(&engine, &shutdown, 3), Claim::Fetch(claim) if claim.kind == ChunkKind::Warm && claim.start == 2000)
        );

        // A body blocked on offset 0 suspends everything speculative: even a
        // free worker may not claim the second seek point while it waits.
        assert_eq!(engine.open_serve_wait(0), 0);
        assert!(engine.serve_waiting());
        assert!(matches!(claim(&engine, &shutdown, 4), Claim::Wait));

        // Bytes landed: the queue resumes at the pending seek point.
        engine.close_serve_wait("available");
        assert!(!engine.serve_waiting());
        assert!(
            matches!(claim(&engine, &shutdown, 4), Claim::Fetch(claim) if claim.kind == ChunkKind::Warm && claim.start == 3000),
            "speculative work must resume after the serve wait closes"
        );
    }

    #[test]
    fn serve_wait_preempts_speculation_and_routes_around_a_stuck_hot_range() {
        let mut engine = test_engine();
        engine.config.chunk_bytes = 4;
        engine.config.read_ahead_bytes = 8;
        engine.desired_connections.store(2, Ordering::SeqCst);
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(4000);
            for (id, kind, start) in [(0usize, ChunkKind::Warm, 2000u64), (1, ChunkKind::Rest, 3000)] {
                state.inflight.insert((0, start));
                if let Some(slot) = state.workers.get_mut(id) {
                    slot.busy = true;
                    slot.kind = Some(kind);
                    slot.start = start;
                    slot.end = start + 4;
                    slot.host = "cdn.example.com".to_string();
                    slot.range_started = Some(Instant::now());
                }
            }
        }
        let waited = engine.open_serve_wait(100);
        engine.enforce_serve_priority(100, waited);
        // Both speculative claims were cancelled by bumping their worker token.
        assert_eq!(engine.worker_cancel[0].load(Ordering::SeqCst), 1);
        assert_eq!(engine.worker_cancel[1].load(Ordering::SeqCst), 1);

        // Now the worker holds the chunk that covers the waiting offset and has
        // been silent for longer than the fast stuck threshold.
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.inflight.insert((0, 100));
            if let Some(slot) = state.workers.get_mut(0) {
                slot.busy = true;
                slot.kind = Some(ChunkKind::Hot);
                slot.start = 100;
                slot.end = 104;
                slot.samples.clear();
                slot.range_started = Some(Instant::now() - Duration::from_millis(700));
            }
        }
        engine.enforce_serve_priority(100, 700);
        {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(state.workers[0].slow, "a silent hot claim must be marked slow");
            assert_eq!(state.races.len(), 1, "and raced on another route");
            assert_eq!(state.races.front().map(|race| race.start), Some(100));
        }
        // The escalation is also counted on the worker that held the range.
        assert_eq!(engine.worker_cancel[0].load(Ordering::SeqCst), 2);
        engine.close_serve_wait("timeout");
        assert!(!engine.serve_waiting());
    }

    #[test]
    fn serve_wait_span_reports_its_elapsed_time() {
        let engine = test_engine();
        assert_eq!(engine.open_serve_wait(512), 0);
        std::thread::sleep(Duration::from_millis(5));
        let waited = engine.open_serve_wait(512);
        assert!(waited >= 5, "a continuing wait reports its age, got {waited} ms");
        engine.close_serve_wait("available");
        assert!(!engine.serve_waiting());
        // Closing twice is a no-op (the second reason cannot overwrite the end).
        engine.close_serve_wait("timeout");
        assert!(!engine.serve_waiting());
    }

    #[test]
    fn fetched_bytes_are_split_by_scheduling_class() {
        let engine = test_engine();
        let shutdown = AtomicBool::new(false);
        engine.push_bytes(0, 0, &[1u8; 16], 0, 0, ChunkKind::Hot, &shutdown);
        engine.push_bytes(0, 16, &[1u8; 8], 0, 0, ChunkKind::Rest, &shutdown);
        engine.push_bytes(0, 24, &[1u8; 4], 0, 0, ChunkKind::Requeue, &shutdown);
        engine.push_bytes(0, 28, &[1u8; 2], 0, 0, ChunkKind::Race, &shutdown);
        engine.note_warm_bytes(0, 0, 6, &shutdown);
        let stats = engine.snapshot();
        assert_eq!(stats.bytes_hot, 16);
        assert_eq!(stats.bytes_warm, 6);
        assert_eq!(stats.bytes_rest, 8);
        assert_eq!(stats.bytes_retry, 6, "requeue and race are retries");
        assert_eq!(stats.bytes_fetched, 36);
    }

    #[test]
    fn startup_gate_caps_memory_and_holds_background_work() {
        let mut engine = test_engine();
        // The unfetched test engine has no cap: the configured window is used.
        assert_eq!(engine.read_ahead_window(), DEFAULT_READ_AHEAD_BYTES);
        // A session starts at the startup window ...
        engine.config.read_ahead_bytes = 8;
        engine.config.chunk_bytes = 4;
        engine.read_ahead_cap.store(4, Ordering::Relaxed);
        assert_eq!(engine.read_ahead_window(), 4);
        let shutdown = AtomicBool::new(false);
        // All workers may still fetch (whole-file scheduling): the cap only
        // bounds what is *retained* in memory.
        assert!(matches!(
            claim(&engine, &shutdown, 0),
            Claim::Fetch(claim) if claim.start == 0
        ));
        // A read outside the 4-byte window is counted but not retained.
        engine.push_bytes(0, 8, &[0u8; 4], 0, 0, ChunkKind::Hot, &shutdown);
        assert_eq!(engine.cached_bytes_at(8), 0);
        assert_eq!(engine.snapshot().bytes_fetched, 4);
        // A read inside it is.
        engine.push_bytes(0, 0, &[1u8; 4], 0, 0, ChunkKind::Hot, &shutdown);
        assert_eq!(engine.cached_bytes_at(0), 4);
        // Opening the gate restores the configured window.
        engine.read_ahead_cap.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(engine.read_ahead_window(), 8);
        // The player's hold gates background work (warm points, parts) only.
        assert!(engine.background_allowed());
        engine.background_paused.store(true, Ordering::SeqCst);
        assert!(!engine.background_allowed());
        // A held session still claims the hot window.
        assert!(matches!(
            claim(&engine, &shutdown, 0),
            Claim::Fetch(claim) if claim.kind == ChunkKind::Hot
        ));
    }

    #[test]
    fn claim_publishes_the_worker_range_and_idles_it() {
        let mut engine = test_engine();
        // A tiny window so "full" is reachable without allocating megabytes:
        // 8 bytes of read-ahead, 4-byte chunks, 2 chunks.
        engine.config.read_ahead_bytes = 8;
        engine.config.chunk_bytes = 4;
        let shutdown = AtomicBool::new(false);

        // First claim: a real range from the configured host.
        let first = claim(&engine, &shutdown, 0);
        assert!(matches!(
            first,
            Claim::Fetch(claim) if claim.start == 0 && claim.end == 4
        ));
        let stats = engine.snapshot();
        assert_eq!(stats.workers.len(), 1);
        assert_eq!(stats.active_workers, 1);
        assert_eq!(stats.workers[0].id, 0);
        assert_eq!(stats.workers[0].host, "cdn.example.com");
        assert!(stats.workers[0].busy);
        assert_eq!(stats.workers[0].start, 0);
        assert_eq!(stats.workers[0].end, 4);
        assert_eq!(stats.workers[0].bytes, 0);

        // One more chunk fills the window; the next claim waits and the worker
        // reports idle while keeping the last range it held.
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(8);
            state.chunks.insert(0, vec![0u8; 4]);
            state.chunks.insert(4, vec![0u8; 4]);
            state.read_ahead = 8;
        }
        let waiting = claim(&engine, &shutdown, 0);
        assert!(matches!(waiting, Claim::Wait));
        let stats = engine.snapshot();
        assert_eq!(stats.active_workers, 0);
        assert!(!stats.workers[0].busy);
        assert_eq!(stats.workers[0].start, 0);
        assert_eq!(stats.workers[0].end, 4);
    }

    #[test]
    fn worker_bytes_are_counted_per_connection() {
        let engine = test_engine();
        let shutdown = AtomicBool::new(false);
        engine.push_bytes(0, 0, &[1u8; 32], 0, 0, ChunkKind::Hot, &shutdown);
        engine.push_bytes(0, 32, &[2u8; 8], 0, 0, ChunkKind::Hot, &shutdown);
        let stats = engine.snapshot();
        assert_eq!(stats.workers[0].bytes, 40);
        assert_eq!(stats.bytes_fetched, 40);
        assert_eq!(stats.hosts, vec![("cdn.example.com".to_string(), 40)]);
        assert_eq!(stats.downloaded, vec![(0, 40)]);
    }

    #[test]
    fn take_bounded_advances_and_keeps_remainder() {
        let mut chunks = BTreeMap::new();
        chunks.insert(0, vec![7u8; 8]);
        let first = take_bounded(&mut chunks, 0, 3).unwrap();
        assert_eq!(first, vec![7u8; 3]);
        assert_eq!(buffered_bytes(&chunks), 5);
        assert_eq!(chunks.keys().copied().collect::<Vec<_>>(), vec![3]);
        // The remainder is addressable at the new cursor.
        let rest = take_bounded(&mut chunks, 3, 100).unwrap();
        assert_eq!(rest, vec![7u8; 5]);
        assert_eq!(buffered_bytes(&chunks), 0);
        assert!(take_bounded(&mut chunks, 3, 100).is_none());
        // A chunk starting before the cursor still serves from the cursor.
        let mut offset = BTreeMap::new();
        offset.insert(2, vec![9u8; 6]);
        assert_eq!(take_bounded(&mut offset, 4, 2).unwrap(), vec![9u8; 2]);
        assert_eq!(buffered_bytes(&offset), 2);
        assert!(take_bounded(&mut offset, 0, 2).is_none());
    }

    #[test]
    fn sliding_rate_is_a_one_second_window() {
        let now = Instant::now();
        let mut samples = VecDeque::new();
        assert_eq!(sliding_rate(&mut samples, now), 0);
        samples.push_back((now - Duration::from_secs(2), 1_000));
        samples.push_back((now - Duration::from_millis(500), 2_000));
        samples.push_back((now - Duration::from_millis(100), 500));
        // The two-second-old sample is dropped; the remaining 2.5 kB span
        // 0.5 s -> 5 kB/s.
        assert_eq!(sliding_rate(&mut samples, now), 5_000);
        assert_eq!(samples.len(), 2);
        // A stale window decays to zero.
        let later = now + Duration::from_secs(5);
        assert_eq!(sliding_rate(&mut samples, later), 0);
    }

    #[test]
    fn config_is_clamped() {
        let clamped = PrefetchConfig {
            read_ahead_bytes: 1,
            chunk_bytes: 1,
            connections: 0,
            warm_points: 999,
            point_bytes: 0,
            hosts: vec!["".to_string(), "  ".to_string(), "cdn.example.com".to_string()],
            cache_key: None,
            media_duration_ms: None,
        }
        .normalized();
        assert_eq!(clamped.read_ahead_bytes, MIN_READ_AHEAD_BYTES);
        assert_eq!(clamped.chunk_bytes, MIN_CHUNK_BYTES);
        assert_eq!(clamped.connections, DEFAULT_CONNECTIONS, "0 = auto");
        assert_eq!(clamped.warm_points, MAX_WARM_POINTS);
        assert_eq!(clamped.point_bytes, 1);
        assert_eq!(clamped.hosts, vec!["cdn.example.com".to_string()]);

        let clamped = PrefetchConfig {
            read_ahead_bytes: u64::MAX,
            chunk_bytes: u64::MAX,
            connections: 99,
            warm_points: 0,
            point_bytes: u64::MAX,
            hosts: Vec::new(),
            cache_key: None,
            media_duration_ms: None,
        }
        .normalized();
        assert_eq!(clamped.read_ahead_bytes, MAX_READ_AHEAD_BYTES);
        assert_eq!(clamped.chunk_bytes, MAX_CHUNK_BYTES.min(MAX_READ_AHEAD_BYTES));
        assert_eq!(clamped.connections, MAX_CONNECTIONS);
        assert_eq!(clamped.warm_points, 0, "0 disables warming");
        assert_eq!(clamped.point_bytes, DEFAULT_POINT_BYTES);
        assert_eq!(
            PrefetchConfig::default().warm_points,
            crate::settings::default_seek_points() as usize
        );
    }

    #[test]
    fn warm_points_are_evenly_spaced_for_every_offered_count() {
        let total = 100_000_000u64;
        for count in [4usize, 10, 20] {
            let offsets = warm_point_offsets(total, count);
            assert_eq!(offsets.len(), count, "count {count}");
            assert_eq!(offsets[0], 0);
            assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(offsets.iter().all(|offset| *offset < total));
            for (index, offset) in offsets.iter().enumerate() {
                let expected = total * index as u64 / count as u64;
                assert_eq!(*offset, expected, "point {index} of {count}");
            }
            // Spacing is exactly one `count`-th of the file (within rounding).
            let spacing = total / count as u64;
            for pair in offsets.windows(2) {
                assert!(pair[1] - pair[0] <= spacing + 1);
            }
        }
    }

    #[test]
    fn warm_points_handle_tiny_and_empty_files() {
        // A file smaller than the point count collapses to one point per byte.
        assert_eq!(warm_point_offsets(3, 10), vec![0, 1, 2]);
        assert_eq!(warm_point_offsets(1, 20), vec![0]);
        assert_eq!(warm_point_offsets(2, 4), vec![0, 1]);
        assert!(warm_point_offsets(0, 10).is_empty());
        assert!(warm_point_offsets(100, 0).is_empty());
        // More points than the cap are clamped, never repeated.
        let offsets = warm_point_offsets(1_000_000, 99);
        assert_eq!(offsets.len(), MAX_WARM_POINTS);
        assert_eq!(offsets[0], 0);
    }

    #[test]
    fn already_covered_offsets_are_recognised() {
        // Window bytes: the warm task must skip a point the window holds.
        let mut chunks = BTreeMap::new();
        chunks.insert(4 << 20, vec![0u8; 4 << 20]); // 4 MiB at 4 MiB
        assert!(!chunks_cover(&chunks, 0));
        assert!(chunks_cover(&chunks, 4 << 20));
        assert!(chunks_cover(&chunks, (8 << 20) - 1));
        assert!(!chunks_cover(&chunks, 8 << 20));

        // Point cache bytes: the same check before a second warm pass.
        let mut cache = PointCache::default();
        cache.insert(1000, vec![7u8; 64], 10, MAX_POINT_CACHE_BYTES);
        assert!(cache.lookup(999).is_none());
        assert!(cache.lookup(1000).is_some());
        assert!(cache.lookup(1063).is_some());
        assert!(cache.lookup(1064).is_none());
        let Some((_, _, skip)) = cache.lookup(1010) else {
            return;
        };
        assert_eq!(skip, 10);
    }

    #[test]
    fn warm_chunk_is_bounded_by_spacing_and_budget() {
        // A 46 MiB file: a tenth is larger than 4 MiB, so the default wins.
        assert_eq!(point_chunk_bytes(48_577_821, 10, DEFAULT_POINT_BYTES), 4 << 20);
        // A small file: total/count is the natural chunk size.
        assert_eq!(point_chunk_bytes(1000, 10, DEFAULT_POINT_BYTES), 100);
        assert_eq!(point_chunk_bytes(3, 10, DEFAULT_POINT_BYTES), 1);
        // Nothing to warm.
        assert_eq!(point_chunk_bytes(0, 10, DEFAULT_POINT_BYTES), 0);
        assert_eq!(point_chunk_bytes(1000, 0, DEFAULT_POINT_BYTES), 0);
        // The 80 MiB memory cap holds for every offered count, even when the
        // caller asks for an absurd per-point size.
        for count in 1..=MAX_WARM_POINTS {
            let chunk = point_chunk_bytes(u64::MAX, count, u64::MAX);
            assert!(
                chunk * count as u64 <= MAX_POINT_CACHE_BYTES,
                "count {count} chunk {chunk}"
            );
        }
    }

    #[test]
    fn point_cache_evicts_oldest_and_stays_bounded() {
        let mut cache = PointCache::default();
        assert_eq!(cache.insert(0, vec![1u8; 10], 2, MAX_POINT_CACHE_BYTES), 0);
        assert_eq!(cache.insert(10, vec![2u8; 10], 2, MAX_POINT_CACHE_BYTES), 0);
        // Third point evicts the oldest one (offset 0).
        assert_eq!(cache.insert(20, vec![3u8; 10], 2, MAX_POINT_CACHE_BYTES), 1);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.bytes, 20);
        assert!(cache.lookup(0).is_none());
        assert!(cache.lookup(19).is_some());
        assert!(cache.lookup(20).is_some());
        assert!(cache.lookup(30).is_none());

        // Replacing an offset updates the byte count exactly once.
        assert_eq!(cache.insert(20, vec![9u8; 5], 2, MAX_POINT_CACHE_BYTES), 0);
        assert_eq!(cache.bytes, 15);
        assert_eq!(cache.len(), 2);

        // The byte bound also evicts, even with room for more entries.
        assert_eq!(cache.insert(40, vec![4u8; 10], 10, 10), 2);
        assert_eq!(cache.bytes, 10);
        assert_eq!(cache.len(), 1);
        assert!(cache.lookup(40).is_some());

        // An empty fetch is not cached (and cannot evict anything).
        assert_eq!(cache.insert(50, Vec::new(), 1, 10), 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn request_head_parsing() {
        let head = "GET /p/x.m4s?e=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=5-\r\nConnection: close\r\n\r\n";
        let request = parse_request(head).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/p/x.m4s?e=1");
        assert_eq!(request.range.as_deref(), Some("bytes=5-"));
        let no_range = parse_request("HEAD /p HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(no_range.method, "HEAD");
        assert!(no_range.range.is_none());
        assert!(parse_request("not http").is_none());
    }

    #[test]
    fn first_response_learns_metadata_without_probe() {
        // Real shape of the first `206` for a 48,577,821-byte DASH video: a
        // 4 MiB chunk at offset 0. The total comes from `Content-Range`; the
        // `Content-Length` here is only this range, never the media length.
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-4194303/48577821"),
        );
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("4194304"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
        let learned = learn_metadata(StatusCode::PARTIAL_CONTENT, 0, &headers)
            .expect("a 206 teaches the media metadata");
        assert_eq!(learned.total, Some(48_577_821));
        assert_eq!(learned.content_type.as_deref(), Some("video/mp4"));

        // ffmpeg's `Range: bytes=0-` is answered from that one response: no
        // probe, no second round trip, and the 206 it gets is complete.
        let spec = parse_range_header("bytes=0-");
        assert!(!needs_total(spec));
        assert!(metadata_ready(spec, learned.total, false));
        let range = resolve_range(spec, learned.total).expect("range");
        assert_eq!(range.start, 0);
        assert_eq!(range.end, Some(48_577_820));
        assert_eq!(
            content_range_header(range, learned.total),
            "bytes 0-48577820/48577821"
        );

        // A later chunk (offset 4 MiB) teaches nothing new but keeps the type.
        let mut later = HeaderMap::new();
        later.insert(
            CONTENT_RANGE,
            HeaderValue::from_static("bytes 4194304-8388607/48577821"),
        );
        let learned = learn_metadata(StatusCode::PARTIAL_CONTENT, 4_194_304, &later)
            .expect("a 206");
        assert_eq!(learned.total, Some(48_577_821));
        assert_eq!(learned.content_type, None);

        // A `206` that omits the total still teaches the content type, and the
        // response falls back to a chunked body.
        let mut open = HeaderMap::new();
        open.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 100-199/*"));
        open.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
        let learned = learn_metadata(StatusCode::PARTIAL_CONTENT, 100, &open).expect("206");
        assert_eq!(learned.total, None);
        assert_eq!(learned.content_type.as_deref(), Some("video/mp4"));

        // Offset 0 served as a plain `200` (Range ignored) uses Content-Length;
        // a `200` for any other offset is not the requested bytes at all.
        let mut plain = HeaderMap::new();
        plain.insert(CONTENT_LENGTH, HeaderValue::from_static("1234567"));
        assert_eq!(
            learn_metadata(StatusCode::OK, 0, &plain).map(|learned| learned.total),
            Some(Some(1_234_567))
        );
        assert!(learn_metadata(StatusCode::OK, 99, &plain).is_none());
        assert!(learn_metadata(StatusCode::FORBIDDEN, 0, &plain).is_none());
    }

    #[test]
    fn first_request_streams_without_waiting_for_a_probe() {
        // Only `bytes=-N` needs the complete length before it can resolve.
        assert!(needs_total(Some(RangeSpec::Suffix(1024))));
        assert!(!needs_total(None));
        assert!(!needs_total(Some(RangeSpec::From(0))));
        assert!(!needs_total(Some(RangeSpec::Bounded(0, 99))));

        // An anchored range starts as soon as its bytes are buffered, even if
        // the upstream never stated a total.
        assert!(metadata_ready(Some(RangeSpec::From(0)), None, true));
        assert!(metadata_ready(Some(RangeSpec::From(0)), Some(10), false));
        assert!(!metadata_ready(Some(RangeSpec::From(0)), None, false));
        // A suffix range cannot use buffered bytes (it names no start).
        assert!(!metadata_ready(Some(RangeSpec::Suffix(1)), None, true));
        assert!(metadata_ready(Some(RangeSpec::Suffix(1)), Some(10), false));

        // The requested offset is the first chunk of the window after a seek;
        // `next_chunk_offset` aligns there, so no probe decides it.
        assert_eq!(
            resolve_range(Some(RangeSpec::From(40_000_000)), None).map(|range| range.start),
            Some(40_000_000)
        );
        let empty = BTreeMap::new();
        let no_inflight = BTreeSet::new();
        let no_dead = BTreeSet::new();
        assert_eq!(
            next_chunk_offset(
                40_000_000,
                40_000_000,
                4 << 20,
                32 << 20,
                None,
                &empty,
                &no_inflight,
                &no_dead,
                0
            ),
            Some(40_000_000)
        );
    }

    #[test]
    fn queue_priority_prefers_hot_then_warm_then_rest() {
        let mut engine = test_engine();
        let chunk = 1 << 20;
        engine.config.chunk_bytes = chunk;
        engine.config.connections = 4;
        engine.config.warm_points = 1;
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(256 << 20);
            state.cursor = 0;
            state.warm_plan_started = true;
            state.warm_plan = vec![WarmTask {
                point: 40 << 20,
                start: 40 << 20,
                end: 41 << 20,
                done: false,
                claimed: false,
                urgent: false,
            }];
        }
        let mut state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        // The client's own chunk, then the next two seconds of playback.
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((0, ChunkKind::Hot))
        );
        state.inflight.insert((0, 0));
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((chunk, ChunkKind::Hot))
        );
        state.inflight.insert((0, chunk));
        // Only then the seek warm point.
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((40 << 20, ChunkKind::Warm))
        );
        state.inflight.insert((0, 40 << 20));
        state.warm_plan[0].claimed = true;
        // And finally the rest of the whole-file split.
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((2 * chunk, ChunkKind::Rest))
        );
    }

    /// The hot window reaches two seconds past the cursor. At the end of the
    /// file that frontier runs past `total`, and before the clamp it produced
    /// `start >= end` claims that could never be covered, so every worker
    /// re-claimed the same past-EOF range thousands of times per second.
    #[test]
    fn past_eof_cursor_never_claims_a_range_beyond_the_total() {
        let mut engine = test_engine();
        let chunk = 1 << 20;
        engine.config.chunk_bytes = chunk;
        engine.config.warm_points = 0;
        let total = 15_402_399u64; // not chunk aligned, like the reported file
        let tail = chunk_start(total - 1024, chunk);
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(total);
            state.cursor = total - 1024;
            let mut start = 0;
            while start < total {
                state.done.insert(start);
                start = start.saturating_add(chunk);
            }
            state.done.remove(&tail);
        }
        let mut state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((tail, ChunkKind::Hot))
        );
        state.inflight.insert((0, tail));
        // The whole file is spoken for: the workers must go idle instead of
        // spinning on the past-EOF tail of the frontier.
        assert_eq!(pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()), None);
    }

    #[test]
    fn hot_window_does_not_refetch_the_served_prefix_of_the_cursor_chunk() {
        let mut engine = test_engine();
        let chunk = 1 << 20;
        engine.config.chunk_bytes = chunk;
        engine.config.warm_points = 0;
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(8 << 20);
            state.cursor = 300 << 10;
            // The prefix of the cursor's chunk was already served (and removed
            // from the map); the rest is buffered.
            state.chunks.insert(300 << 10, vec![0u8; 724 << 10]);
            state.done.insert(0);
        }
        let mut state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 0, &test_disk()),
            Some((chunk, ChunkKind::Hot))
        );
    }

    #[test]
    fn dynamic_split_covers_the_file_and_steals_from_the_largest_part() {
        let mut engine = test_engine();
        let chunk = 1 << 20;
        engine.config.chunk_bytes = chunk;
        let mut state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.total = Some(256 << 20);
        state.cursor = 0;
        build_parts(&mut state, 4, chunk);
        assert_eq!(state.parts.len(), 4);
        // The parts tile the whole remaining range without gaps or overlaps.
        assert_eq!(state.parts[0].next, 0);
        assert_eq!(state.parts.last().map(|part| part.end), Some(256 << 20));
        for pair in state.parts.windows(2) {
            assert_eq!(pair[0].end, pair[1].next);
        }
        assert!(state
            .parts
            .iter()
            .all(|part| (part.end - part.next) % chunk == 0 || part.end == 256 << 20));
        // A worker takes sub-chunks from its own part.
        let everywhere = u64::MAX;
        assert_eq!(
            take_part_chunk(&mut state, 0, 0, chunk, &test_disk(), everywhere),
            Some(0)
        );
        state.inflight.insert((0, 0));
        // Exhaust one part; the next take steals from the largest remaining
        // (part 1, untouched) instead of leaving that worker idle.
        state.parts[3].next = state.parts[3].end;
        let largest_next = state.parts[1].next;
        let stolen = steal_chunk(&mut state, 0, chunk, &test_disk(), everywhere);
        assert_eq!(stolen, Some(largest_next));
        assert_eq!(state.parts[1].next, largest_next + chunk);
        // Nothing at or past the horizon is claimable, and the blocked part
        // keeps its cursor so it resumes when the horizon moves.
        state.parts[1].next = largest_next + chunk;
        assert_eq!(
            take_part_chunk(&mut state, 1, 0, chunk, &test_disk(), largest_next + chunk),
            None
        );
        assert_eq!(state.parts[1].next, largest_next + chunk);
    }

    #[test]
    fn live_connection_count_clamps_and_republishes() {
        let engine = test_engine();
        assert_eq!(engine.desired_connections(), 1);
        engine.desired_connections.store(16, Ordering::SeqCst);
        assert_eq!(engine.desired_connections(), 16);
        assert_eq!(engine.snapshot().workers.len(), 16);
        engine.desired_connections.store(0, Ordering::SeqCst);
        assert_eq!(engine.desired_connections(), 1, "never below one");
        engine
            .desired_connections
            .store(999, Ordering::SeqCst);
        assert_eq!(engine.desired_connections(), MAX_CONNECTIONS);
        assert_eq!(engine.snapshot().workers.len(), MAX_CONNECTIONS);
    }

    #[test]
    fn duplicate_race_first_finisher_wins() {
        let engine = test_engine();
        if let Ok(mut state) = engine.state.lock() {
            state.inflight.insert((0, 0));
            state
                .race_tokens
                .insert(0, Arc::new(AtomicBool::new(false)));
        }
        let token = Arc::new(AtomicBool::new(false));
        assert!(
            engine.finish_chunk(0, 0, ChunkKind::Race, 1, Some(&token)),
            "the first finisher wins"
        );
        assert!(
            !engine.finish_chunk(0, 0, ChunkKind::Race, 2, Some(&token)),
            "the loser must not claim the range"
        );
        let state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert!(state.done.contains(&0));
        assert!(!state.race_tokens.contains_key(&0));
    }

    #[test]
    fn cached_ranges_merge_window_and_point_cache() {
        let engine = test_engine();
        if let Ok(mut state) = engine.state.lock() {
            state.chunks.insert(0, vec![0u8; 4]);
            state.chunks.insert(4, vec![0u8; 4]);
            state
                .points
                .insert(8, vec![1u8; 4], 10, MAX_POINT_CACHE_BYTES);
            state
                .points
                .insert(20, vec![2u8; 4], 10, MAX_POINT_CACHE_BYTES);
            state
                .points
                .insert(22, vec![3u8; 4], 10, MAX_POINT_CACHE_BYTES);
        }
        // Adjacent window and point ranges coalesce; overlapping points too.
        assert_eq!(engine.cached_ranges(), vec![(0, 12), (20, 26)]);
        assert_eq!(engine.cached_bytes_at(0), 12);
        assert_eq!(engine.cached_bytes_at(6), 6);
        assert_eq!(engine.cached_bytes_at(12), 0);
        assert_eq!(engine.cached_bytes_at(21), 5);
        assert_eq!(engine.cached_bytes_at(26), 0);
    }

    /// Bare engine for the window/token/point tests: no listener, no workers,
    /// and a client that is never used.
    /// A cache that never touches the disk, for the scheduler tests.
    fn test_disk() -> DiskCache {
        DiskCache::disabled()
    }

    /// The seek-cache decision may only trust the disk map: a hole in the
    /// middle of a fetched file must read as "not covered" even when the ranges
    /// around it are present (the old size heuristic claimed those as cached and
    /// the seek then fetched megabytes upstream).
    #[test]
    fn disk_cover_reports_a_hole_instead_of_an_estimate() {
        let mut disk = test_disk();
        disk.insert(0, 4096);
        disk.insert(8192, 12288);
        // Covered from the start: the run ends where the hole starts.
        let (run, ranges, bytes) = disk.cover(0);
        assert_eq!(run, 4096);
        assert_eq!(ranges, 2);
        assert_eq!(bytes, 8192);
        // The hole itself: no run at all, even though ranges exist on both
        // sides.
        assert_eq!(disk.cover(4096).0, 0);
        assert_eq!(disk.cover(5000).0, 0);
        // Inside the second range the run is what is left of it.
        assert_eq!(disk.cover(8192).0, 4096);
        assert_eq!(disk.cover(10000).0, 2288);
        // Past the end.
        assert_eq!(disk.cover(12288).0, 0);
        // The whole-file case: one range, and the run reaches the end.
        let mut whole = test_disk();
        whole.insert(0, 12288);
        assert_eq!(whole.cover(0), (12288, 1, 12288));
        assert!(whole.covers_range(0, 12288));
        assert!(!whole.covers_range(0, 12289));
        assert!(!disk.covers_range(0, 12288), "a hole is not coverage");
    }

    fn test_engine() -> Engine {
        Engine {
            config: PrefetchConfig::default().normalized(),
            target: Target {
                url: "https://cdn.example.com/a.m4s?x=1".to_string(),
                scheme: "https".to_string(),
                origin: "https://cdn.example.com".to_string(),
                host_label: "cdn.example.com".to_string(),
                path: "/a.m4s".to_string(),
                query: "x=1".to_string(),
            },
            client: reqwest::Client::new(),
            cache_key: "test".to_string(),
            state: Mutex::new(State {
                base: 0,
                cursor: 0,
                generation: 0,
                chunks: BTreeMap::new(),
                inflight: BTreeSet::new(),
                dead: BTreeSet::new(),
                done: BTreeSet::new(),
                parts: Vec::new(),
                parts_built: false,
                requeued: BTreeSet::new(),
                short_bodies: BTreeMap::new(),
                warm_plan: Vec::new(),
                warm_plan_started: false,
                serve_samples: VecDeque::new(),
                races: VecDeque::new(),
                urgent: VecDeque::new(),
                race_tokens: BTreeMap::new(),
                total: None,
                fatal: None,
                content_type: None,
                hosts: vec![Host {
                    label: "cdn.example.com".to_string(),
                    origin: "https://cdn.example.com".to_string(),
                    failures: 0,
                    failed: false,
                    bytes: 0,
                    degraded_until: None,
                }],
                next_host: 0,
                samples: VecDeque::new(),
                bytes_fetched: 0,
                failed_ranges: 0,
                served: 0,
                serve_bps: 0,
                first_serve: None,
                spec_tokens: DEFAULT_CHUNK_BYTES,
                spec_at: Instant::now(),
                spec_bytes: 0,
                read_ahead: 0,
                points: PointCache::default(),
                warm_points_total: 0,
                warm_points_done: 0,
                warm_chunk: 0,
                warm_bytes: 0,
                hot_bytes: 0,
                warm_fetch_bytes: 0,
                rest_bytes: 0,
                retry_bytes: 0,
                point_hits: 0,
                point_served: 0,
                disk_hits: 0,
                disk_served: 0,
                workers: (0..MAX_CONNECTIONS)
                    .map(|_| WorkerSlot::default())
                    .collect(),
            }),
            disk: Arc::new(Mutex::new(DiskCache::disabled())),
            // No writer in a scheduler test: `disk_write` falls back to writing
            // through the map, and a disabled cache has no file anyway.
            writer: Mutex::new(None),
            stats: Arc::new(Mutex::new(PrefetchStats::default())),
            serving: AtomicBool::new(false),
            serve_epoch: AtomicU64::new(0),
            // No startup cap: the unit tests exercise the scheduling arithmetic
            // directly against `config`.
            read_ahead_cap: AtomicU64::new(u64::MAX),
            background_paused: AtomicBool::new(false),
            desired_connections: AtomicU64::new(1),
            configured_connections: 1,
            background_workers: AtomicU64::new(0),
            serve_waits: AtomicU64::new(0),
            serve_wait_slow: AtomicU64::new(0),
            worker_cancel: (0..MAX_CONNECTIONS).map(|_| AtomicU64::new(0)).collect(),
            media_bps: AtomicU64::new(0),
            serve_wait_pos: AtomicU64::new(u64::MAX),
            serve_wait_since_ms: AtomicU64::new(0),
            serve_wait_escalations: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn generation_of(engine: &Engine) -> u64 {
        match engine.state.lock() {
            Ok(state) => state.generation,
            Err(poisoned) => poisoned.into_inner().generation,
        }
    }

    #[test]
    fn seek_keeps_buffered_ranges_and_reports_a_hit() {
        let engine = test_engine();
        // The buffered window covers 0..16 and the point cache covers 400..410.
        if let Ok(mut state) = engine.state.lock() {
            state.chunks.insert(0, vec![7u8; 16]);
            state.total = Some(1000);
            state
                .points
                .insert(400, vec![9u8; 10], 10, MAX_POINT_CACHE_BYTES);
        }

        // A seek inside the window is a hit and keeps every buffered byte.
        assert!(engine.seek(8));
        let stats = engine.snapshot();
        assert_eq!(stats.downloaded, vec![(0, 16), (400, 410)]);
        assert_eq!(
            generation_of(&engine),
            0,
            "a seek must not invalidate in-flight whole-file work"
        );
        let cached = engine.cached_ranges();
        assert_eq!(cached, vec![(0, 16), (400, 410)]);
        // Contiguous bytes ahead: inside the window chunk and inside the point.
        assert_eq!(engine.cached_bytes_at(4), 12);
        assert_eq!(engine.cached_bytes_at(400), 10);
        assert_eq!(engine.cached_bytes_at(500), 0);

        // A seek past the window moves the cursor but keeps the bytes ahead.
        assert!(!engine.seek(20));
        let stats = engine.snapshot();
        assert_eq!(stats.read_ahead_bytes, 0, "bytes behind the cursor are gone");
        assert_eq!(engine.cached_ranges(), vec![(400, 410)]);
        assert_eq!(generation_of(&engine), 0);

        // A seek backwards is answered from the point cache with no reset.
        assert!(engine.cached_bytes_at(400) > 0);
        assert!(engine.cached_bytes_at(409) == 1);
    }

    #[test]
    fn newest_request_owns_the_proxy() {
        let engine = test_engine();
        let first = engine.begin_response();
        assert!(engine.is_current(first));
        let second = engine.begin_response();
        assert_ne!(first, second);
        assert!(!engine.is_current(first), "the abandoned response must stop");
        assert!(engine.is_current(second));
    }

    #[test]
    fn point_hit_matches_the_requested_offset_and_head() {
        let engine = test_engine();
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(1000);
            state.content_type = Some("video/mp4".to_string());
            state
                .points
                .insert(100, vec![9u8; 200], 10, MAX_POINT_CACHE_BYTES);
        }

        // `bytes=150-`: the point at 100 is entered 50 bytes in and promises
        // the whole remaining range, starting at the requested offset.
        let hit = point_hit(
            &engine,
            Some(RangeSpec::From(150)),
            Some(150),
            Instant::now(),
        );
        assert!(hit.is_some(), "the warmed point covers 150");
        let Some(hit) = hit else {
            return;
        };
        assert_eq!(hit.range.start, 150);
        assert_eq!(hit.range.end, Some(999));
        assert_eq!(hit.length, 850);
        assert_eq!(hit.skip, 50);
        assert_eq!(hit.data.len(), 200);
        assert!(hit.head.contains("Content-Range: bytes 150-999/1000"));
        assert!(hit.head.contains("Content-Length: 850"));
        // The first byte the body writes is the one at absolute offset 150.
        assert_eq!(hit.data[hit.skip], 9);

        // A bounded range inside the point keeps its own end.
        let bounded = point_hit(
            &engine,
            Some(RangeSpec::Bounded(150, 160)),
            Some(150),
            Instant::now(),
        );
        assert!(bounded.is_some(), "the warmed point covers 150");
        let Some(bounded) = bounded else {
            return;
        };
        assert_eq!(bounded.length, 11);
        assert!(bounded.head.contains("Content-Range: bytes 150-160/1000"));

        // Outside the point, and for a suffix (which names no start), the fast
        // path declines and the window path answers.
        assert!(point_hit(
            &engine,
            Some(RangeSpec::From(400)),
            Some(400),
            Instant::now()
        )
        .is_none());
        assert!(point_hit(&engine, Some(RangeSpec::Suffix(64)), None, Instant::now()).is_none());
    }

    #[test]
    fn serve_trace_labels_every_range_form() {
        assert_eq!(range_request_label(None), "0-");
        assert_eq!(
            range_request_label(Some(RangeSpec::From(989_145))),
            "989145-"
        );
        assert_eq!(
            range_request_label(Some(RangeSpec::Bounded(10, 19))),
            "10-19"
        );
        assert_eq!(range_request_label(Some(RangeSpec::Suffix(4096))), "-4096");
        // The traced range is exactly the head's `Content-Range`, so the log
        // line can be compared with the bytes on the wire.
        let range = ResolvedRange {
            start: 989_145,
            end: Some(513_415_507),
        };
        assert_eq!(
            content_range_header(range, Some(513_415_508)),
            "bytes 989145-513415507/513415508"
        );
    }

    /// Read exactly `want` bytes from `url` starting at `start` through the
    /// local proxy. Used by the live scheduler proof below.
    async fn live_read(
        client: &reqwest::Client,
        url: &str,
        start: u64,
        want: u64,
    ) -> Result<u64> {
        let mut response = client
            .get(url)
            .header(
                RANGE,
                format!("bytes={start}-{}", start.saturating_add(want).saturating_sub(1)),
            )
            .send()
            .await
            .with_context(|| format!("live read {start}+{want}"))?;
        if response.status() != StatusCode::PARTIAL_CONTENT
            && response.status() != StatusCode::OK
        {
            bail!("live read {start}: HTTP {}", response.status());
        }
        let mut read = 0u64;
        while read < want {
            let Some(chunk) = response.chunk().await? else {
                break;
            };
            read += chunk.len() as u64;
        }
        Ok(read)
    }

    /// The dynamic multi-connection run required by the work order. Ignored by
    /// default; driven by `BILI_PREFETCH_TEST_STREAM=<url>` plus
    /// `BILI_DEBUG_LOG=<file>` (and `BILI_PREFETCH_LOG=1` for the serve lines):
    ///
    /// `cargo test --release -- --ignored --nocapture live_dynamic_scheduler`
    ///
    /// Phase 1 runs a real sequential reader for 10 s at 16 connections and
    /// samples busy/idle, per-worker bps, queue and cached bytes every second.
    /// It then seeks to a warmed point and to a buffered window offset and
    /// proves both are answered from memory (`serve src=cache|window`).
    /// Phase 2 injects a black-holed host and proves the slow worker is
    /// cancelled, re-routed (and raced) and recovers.
    #[test]
    #[ignore = "network: BILI_PREFETCH_TEST_STREAM=<url> live 16-connection scheduler run"]
    fn live_dynamic_scheduler() {
        let url = match std::env::var("BILI_PREFETCH_TEST_STREAM") {
            Ok(url) if !url.trim().is_empty() => url,
            _ => return,
        };
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            crate::debug_log("prefetch live skipped reason=no-runtime");
            return;
        };
        let mut handle = match spawn(
            &url,
            PrefetchConfig {
                connections: 16,
                warm_points: 10,
                ..PrefetchConfig::default()
            },
        ) {
            Ok(handle) => handle,
            Err(error) => {
                crate::debug_log(&format!("prefetch live skipped reason=spawn err={error}"));
                return;
            }
        };
        let local = handle.local_url.clone();
        let client = match reqwest::Client::builder()
            .user_agent(crate::bilibili::UA)
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                crate::debug_log(&format!("prefetch live skipped reason=client err={error}"));
                return;
            }
        };
        let reader_stop = Arc::new(AtomicBool::new(false));
        let mut peak_busy = 0u32;
        let mut min_busy_after_start = u32::MAX;
        let mut queue_peak = 0u64;
        let (cache_offset, window_offset) = runtime.block_on(async {
            let reader = {
                let client = client.clone();
                let local = local.clone();
                let stop = reader_stop.clone();
                tokio::spawn(async move {
                    let mut offset = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        if live_read(&client, &local, offset, 512 << 10).await.is_err() {
                            break;
                        }
                        offset = offset.saturating_add(512 << 10);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    offset
                })
            };
            let mut took_next_after_idle = false;
            for tick in 1..=20u64 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Live connection change: lower the pool, then raise it. The
                // workers that were waiting outside the queue must pick up the
                // next chunk immediately, not after a restart.
                if tick == 4 {
                    handle.set_connections(4);
                }
                if tick == 5 {
                    handle.set_connections(16);
                }
                let stats = handle.snapshot();
                let health = handle.worker_health();
                let busy = stats.active_workers;
                peak_busy = peak_busy.max(busy);
                if tick > 1 {
                    min_busy_after_start = min_busy_after_start.min(busy);
                }
                queue_peak = queue_peak.max(stats.queue_chunks);
                let bps = health
                    .iter()
                    .map(|(id, _, _, bps)| format!("{id}:{}", format_rate(*bps)))
                    .collect::<Vec<_>>()
                    .join(",");
                let idle = health.len().saturating_sub(busy as usize);
                crate::debug_log(&format!(
                    "prefetch live t={tick}s busy={busy}/{} idle={idle} bps=[{bps}] queue={} cached={} window={}",
                    health.len(),
                    stats.queue_chunks,
                    format_bytes(ranges_bytes(&handle.cached_ranges())),
                    format_bytes(stats.read_ahead_bytes)
                ));
                if tick == 1 {
                    assert_eq!(
                        busy as usize,
                        health.len(),
                        "every configured connection must be busy at start"
                    );
                }
                if tick == 6 {
                    assert!(
                        busy >= 12,
                        "raised pool must take the next chunk immediately: busy={busy}"
                    );
                    took_next_after_idle = true;
                }
            }
            reader_stop.store(true, Ordering::SeqCst);
            let cursor = reader.await.unwrap_or(0);
            crate::debug_log(&format!(
                "prefetch live reader done offset={cursor} peak_busy={peak_busy} min_busy={min_busy_after_start} queue_peak={queue_peak} took_next={took_next_after_idle}"
            ));
            tokio::time::sleep(Duration::from_millis(300)).await;
            let stats = handle.snapshot();
            let cached = handle.cached_ranges();
            // Prefer a point-cache range for the cache seek; for the window
            // seek pick a buffered range the point cache does not cover.
            let cache_offset = stats
                .point_cache
                .iter()
                .find(|(start, end)| end.saturating_sub(*start) >= 64 << 10)
                .map(|(start, _)| *start)
                .unwrap_or(0);
            // The window part of the merged cache: walk each merged range and
            // take the first gap the point cache does not cover.
            let mut window_offset = 0u64;
            'outer: for (start, end) in &cached {
                let mut cursor = *start;
                for (point_start, point_end) in &stats.point_cache {
                    if *point_end <= cursor || *point_start >= *end {
                        continue;
                    }
                    if *point_start > cursor && *point_start - cursor >= 64 << 10 {
                        window_offset = cursor.saturating_add(1024);
                        break 'outer;
                    }
                    cursor = (*point_end).max(cursor);
                }
                if *end > cursor && *end - cursor >= 64 << 10 {
                    window_offset = cursor.saturating_add(1024);
                    break;
                }
            }
            (cache_offset, window_offset)
        });
        // Seek proofs: each must be served from memory, and its `seek hit`
        // line must exist with the right source.
        let cache_read = runtime.block_on(live_read(&client, &local, cache_offset, 64 << 10));
        let window_read = runtime.block_on(live_read(&client, &local, window_offset, 64 << 10));
        let cache_ms = handle.snapshot().point_hits;
        crate::debug_log(&format!(
            "prefetch live seek cache_offset={cache_offset} read={cache_read:?} point_hits={cache_ms}"
        ));
        crate::debug_log(&format!(
            "prefetch live seek window_offset={window_offset} read={window_read:?}"
        ));
        assert!(
            qa_log_contains(&format!("seek hit offset={cache_offset} src=cache")),
            "cache seek must log src=cache"
        );
        assert!(
            qa_log_contains(&format!("seek hit offset={window_offset} src=window")),
            "window seek must log src=window"
        );
        handle.shutdown();

        // Phase 2: a black-holed mirror. The listener accepts and never
        // answers, so a worker on it reports zero bytes and the watchdog must
        // cancel, re-route and recover it.
        let blackhole = match std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) => {
                crate::debug_log(&format!("prefetch live slow skipped err={error}"));
                return;
            }
        };
        let blackhole_port = blackhole.local_addr().map(|addr| addr.port()).unwrap_or(0);
        let held_sockets: Arc<Mutex<Vec<std::net::TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let held = held_sockets.clone();
        std::thread::spawn(move || {
            for stream in blackhole.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Ok(mut sockets) = held.lock() {
                            sockets.push(stream);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let origin = match reqwest::Url::parse(&url) {
            Ok(parsed) => parsed
                .host_str()
                .map(|host| {
                    parsed
                        .port()
                        .map(|port| format!("{host}:{port}"))
                        .unwrap_or_else(|| host.to_string())
                })
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        let mut slow_handle = match spawn(
            &url,
            PrefetchConfig {
                connections: 3,
                warm_points: 0,
                hosts: vec![origin, format!("127.0.0.1:{blackhole_port}")],
                ..PrefetchConfig::default()
            },
        ) {
            Ok(handle) => handle,
            Err(error) => {
                crate::debug_log(&format!("prefetch live slow skipped err={error}"));
                return;
            }
        };
        slow_handle.set_background_paused(true);
        let slow_local = slow_handle.local_url.clone();
        runtime.block_on(async {
            let stop = Arc::new(AtomicBool::new(false));
            let reader = {
                let client = client.clone();
                let local = slow_local.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    let mut offset = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        if live_read(&client, &local, offset, 256 << 10).await.is_err() {
                            break;
                        }
                        offset = offset.saturating_add(256 << 10);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                })
            };
            for tick in 1..=9u64 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let stats = slow_handle.snapshot();
                let health = slow_handle.worker_health();
                let bps = health
                    .iter()
                    .map(|(id, _, slow, bps)| {
                        format!(
                            "{id}:{}{}",
                            if *slow { "S" } else { "" },
                            format_rate(*bps)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                crate::debug_log(&format!(
                    "prefetch live slow t={tick}s busy={} bps=[{bps}] fetched={} slow={}",
                    stats.active_workers,
                    stats.bytes_fetched,
                    health.iter().filter(|(_, _, slow, _)| *slow).count()
                ));
            }
            stop.store(true, Ordering::SeqCst);
            let _ = reader.await;
        });
        let slow_stats = slow_handle.snapshot();
        crate::debug_log(&format!(
            "prefetch live slow done fetched={} failed_ranges={} hosts={:?}",
            slow_stats.bytes_fetched, slow_stats.failed_ranges, slow_stats.hosts
        ));
        assert!(
            qa_log_contains("worker slow id="),
            "the watchdog must log the slow worker"
        );
        assert!(
            qa_log_contains("action=retry host="),
            "the slow worker must be re-routed"
        );
        assert!(
            qa_log_contains("worker recover id="),
            "the re-routed worker must log recovery"
        );
        slow_handle.shutdown();
    }

    /// Live end-to-end check against the real CDN. Ignored by default:
    /// `$env:BILI_DEBUG_LOG="prefetch-live.log"; cargo test --release -- --ignored --nocapture live_prefetch_proxy`
    /// Set `BILI_PREFETCH_BVID=<bvid>` to pin a video, or `BILI_PREFETCH_URL`
    /// to skip the playurl lookup entirely.
    /// Live fetch benchmark: cold session cache, real CDN, a reader that keeps
    /// the media pace (like ffmpeg), and the numbers the work order asks for.
    ///
    /// ```powershell
    /// $env:BILI_DEBUG_LOG="prefetch-bench.log"
    /// $env:BILI_PREFETCH_BVID="BV1Kznj6VEwm"
    /// $env:BILI_PREFETCH_BENCH_SECS="60"
    /// cargo test --release -- --ignored --nocapture live_fetch_benchmark
    /// ```
    ///
    /// Logs, once per second: `prefetch bench t=N active=… live=… agg=… cached=…
    /// window=… queue=…`; and once at the end:
    /// `prefetch bench result first_byte_ms=… buffer10_ms=… complete_ms=…
    /// cached=…/… mbps=… seeks=[…] p50_ms=…`.
    ///
    /// `buffer10_ms` is the first second at which the session cache covers ten
    /// seconds of media from offset 0 (`10 × total / duration`), measured from
    /// the cache map — not from "some byte arrived". `complete_ms` is the first
    /// second the map covers the whole file (absent when it does not finish
    /// inside the window). Seeks are 64 KiB ranged reads at nine spread
    /// offsets, timed to first byte through the proxy, after the read window.
    #[test]
    #[ignore = "network: benchmarks the real CDN through the proxy"]
    fn live_fetch_benchmark() {
        let Some((url, _hosts, duration)) = live_media() else {
            return;
        };
        let secs = std::env::var("BILI_PREFETCH_BENCH_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(60)
            .max(10);
        let runtime = match crate::bilibili::shared_runtime() {
            Some(runtime) => runtime,
            None => return,
        };
        let cache_key = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            format!(
                "bench-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            )
        };
        let mut handle = match spawn(
            &url,
            PrefetchConfig {
                connections: 16,
                warm_points: 10,
                media_duration_ms: (duration > 0).then(|| duration.saturating_mul(1000)),
                cache_key: Some(cache_key),
                ..PrefetchConfig::default()
            },
        ) {
            Ok(handle) => handle,
            Err(error) => {
                crate::debug_log(&format!("prefetch bench spawn failed err={error}"));
                return;
            }
        };
        let local = handle.local_url.clone();
        let client = match reqwest::Client::builder()
            .user_agent(crate::bilibili::UA)
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                crate::debug_log(&format!("prefetch bench client failed err={error}"));
                handle.shutdown();
                return;
            }
        };
        let spawn_at = Instant::now();
        let (first_byte_ms, buffer10_ms, complete_ms, cached_end, total_end, paced_bytes) =
            runtime.block_on(async {
                let mut first_byte_ms: Option<u128> = None;
                let mut buffer10_ms: Option<u128> = None;
                let mut complete_ms: Option<u128> = None;
                let mut paced = 0u64;
                let paced_bps: u64 = std::env::var("BILI_PREFETCH_BENCH_BPS")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|bps| *bps > 0)
                    .unwrap_or(0);
                let mut response = match client
                    .get(&local)
                    .header(RANGE, "bytes=0-")
                    .send()
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        crate::debug_log(&format!("prefetch bench open failed err={error}"));
                        return (None, None, None, 0u64, 0u64, 0u64);
                    }
                };
                let started = Instant::now();
                let mut tick = 0u64;
                let mut next_tick = Duration::from_secs(1);
                while started.elapsed() < Duration::from_secs(secs) {
                    // Pace exactly like playback: one media second of bytes per
                    // second, from t=0. `BILI_PREFETCH_BENCH_BPS` supplies the
                    // bitrate before the proxy has learned the length; without
                    // it the first second reads as fast as the proxy gives
                    // bytes, which is what a decoder's first read does.
                    let total = handle.snapshot().bytes_total;
                    let target = if paced_bps > 0 {
                        paced_bps
                    } else {
                        match (total, duration) {
                            (Some(total), d) if d > 0 => total / d,
                            _ => 0,
                        }
                    };
                    if let Some(total) = total {
                        if paced >= total {
                            break;
                        }
                    }
                    let wait = if target > 0 {
                        let due = Duration::from_millis(paced.saturating_mul(1000) / target.max(1));
                        due.saturating_sub(started.elapsed())
                    } else {
                        Duration::ZERO
                    };
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                    match response.chunk().await {
                        Ok(Some(chunk)) if !chunk.is_empty() => {
                            if first_byte_ms.is_none() {
                                first_byte_ms = Some(spawn_at.elapsed().as_millis());
                            }
                            paced = paced.saturating_add(chunk.len() as u64);
                        }
                        // ffmpeg reopens an ended body at the position it
                        // reached; do the same so a short response (a superseded
                        // or timed-out body) does not end the read.
                        _ => {
                            response = match client
                                .get(&local)
                                .header(RANGE, format!("bytes={paced}-"))
                                .send()
                                .await
                            {
                                Ok(response) => response,
                                Err(error) => {
                                    crate::debug_log(&format!(
                                        "prefetch bench reopen failed pos={paced} err={error}"
                                    ));
                                    break;
                                }
                            };
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    if started.elapsed() >= next_tick {
                        tick = tick.saturating_add(1);
                        next_tick = Duration::from_secs(tick + 1);
                        let stats = handle.snapshot();
                        let cached = ranges_bytes(&handle.cached_ranges());
                        if let (Some(total), d) = (stats.bytes_total, duration) {
                            if d > 0 {
                                let ten = (total / d).saturating_mul(10);
                                if buffer10_ms.is_none() && cached >= ten {
                                    buffer10_ms = Some(spawn_at.elapsed().as_millis());
                                }
                                if complete_ms.is_none() && cached >= total.saturating_sub(64 << 10)
                                {
                                    complete_ms = Some(spawn_at.elapsed().as_millis());
                                }
                            }
                        }
                        crate::debug_log(&format!(
                            "prefetch bench t={tick}s active={} inflight={} live={} agg={}/s cached={} window={} queue={} paced={}",
                            stats.active_workers,
                            stats.inflight,
                            handle.worker_health().len(),
                            format_rate(stats.bytes_per_sec),
                            format_bytes(cached),
                            format_bytes(stats.read_ahead_bytes),
                            stats.queue_chunks,
                            format_bytes(paced)
                        ));
                    }
                }
                let stats = handle.snapshot();
                let cached = ranges_bytes(&handle.cached_ranges());
                (
                    first_byte_ms,
                    buffer10_ms,
                    complete_ms,
                    cached,
                    stats.bytes_total.unwrap_or(0),
                    paced,
                )
            });
        // Nine seeks spread over the file, 64 KiB each, timed to first byte.
        let mut seeks: Vec<u128> = Vec::new();
        let span = total_end.max(1);
        for step in 1..=9u64 {
            let offset = span.saturating_mul(step) / 10;
            let at = Instant::now();
            let read = runtime.block_on(live_read(&client, &local, offset, 64 << 10));
            let elapsed = at.elapsed().as_millis();
            if read.is_ok() {
                seeks.push(elapsed);
            }
        }
        let mut sorted = seeks.clone();
        sorted.sort_unstable();
        let p50 = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        let elapsed_secs = spawn_at.elapsed().as_secs_f64().max(0.001);
        crate::debug_log(&format!(
            "prefetch bench result first_byte_ms={} buffer10_ms={} complete_ms={} cached={} total={} paced={} mbps={:.2} seeks={:?} p50_ms={p50}",
            first_byte_ms.map_or("-".to_string(), |ms| ms.to_string()),
            buffer10_ms.map_or("-".to_string(), |ms| ms.to_string()),
            complete_ms.map_or("-".to_string(), |ms| ms.to_string()),
            format_bytes(cached_end),
            format_bytes(total_end),
            format_bytes(paced_bytes),
            cached_end as f64 / elapsed_secs / 1e6,
            seeks
        ));
        println!(
            "first_byte={first_byte_ms:?} buffer10={buffer10_ms:?} complete={complete_ms:?} cached={} total={} seek_p50={p50}",
            format_bytes(cached_end),
            format_bytes(total_end)
        );
        handle.shutdown();
    }

    #[test]
    #[ignore = "network: streams a real Bilibili media file through the proxy"]
    fn live_prefetch_proxy() {
        let Some((url, hosts, _duration)) = live_media() else {
            return;
        };
        let summary = match qa_verify(&url, &hosts, 16 << 20, 40_000_000) {
            Ok(summary) => summary,
            Err(error) => {
                crate::debug_log(&format!("prefetch live qa failed err={error}"));
                return;
            }
        };
        println!("{summary}");
    }

    /// One real media URL (plus its mirror hosts and duration) for the live
    /// tests. `BILI_PREFETCH_URL` skips the playurl lookup; otherwise a popular
    /// video is picked, longest first unless `BILI_PREFETCH_BVID` pins one.
    fn live_media() -> Option<(String, Vec<String>, u64)> {
        let runtime = crate::bilibili::shared_runtime();
        let Some(runtime) = runtime else {
            crate::debug_log("prefetch live skipped reason=no-runtime");
            return None;
        };
        let explicit = std::env::var("BILI_PREFETCH_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if let Some(url) = explicit {
            // A benchmark on a pinned URL still needs the duration for the
            // bitrate (and therefore the horizon and the rate floor).
            let duration = std::env::var("BILI_PREFETCH_DURATION_S")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0);
            return Some((url, Vec::new(), duration));
        }
        // `BILI_PREFETCH_ACCOUNT=1` benchmarks the logged-in session (the real
        // case: 1080p instead of the anonymous 480p cap the CDN gives an empty
        // cookie jar). The saved account is the app's own file, read only.
        let account_session = std::env::var("BILI_PREFETCH_ACCOUNT")
            .map(|value| value.trim() == "1")
            .unwrap_or(false);
        let session = if account_session {
            let account = crate::accounts::load();
            let built = match account {
                Some(account) => {
                    let token = crate::accounts::to_token(&account);
                    runtime.block_on(crate::bilibili::Session::from_token(&token))
                }
                None => {
                    crate::debug_log("prefetch live skipped reason=no-saved-account");
                    return None;
                }
            };
            match built {
                Ok(session) => session,
                Err(error) => {
                    crate::debug_log(&format!(
                        "prefetch live skipped reason=account-session err={error}"
                    ));
                    return None;
                }
            }
        } else {
            match runtime.block_on(crate::bilibili::Session::new()) {
                Ok(session) => session,
                Err(error) => {
                    crate::debug_log(&format!("prefetch live skipped reason=session err={error}"));
                    return None;
                }
            }
        };
        let pinned = std::env::var("BILI_PREFETCH_BVID")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let bvid = match pinned {
            Some(bvid) => bvid,
            None => {
                let popular = match runtime
                    .block_on(session.search_video("原神", 1, "", 30))
                {
                    Ok(popular) => popular,
                    Err(error) => {
                        crate::debug_log(&format!(
                            "prefetch live skipped reason=feed err={error}"
                        ));
                        return None;
                    }
                };
                match popular
                    .items
                    .iter()
                    .max_by_key(|card| card.duration.clone())
                {
                    Some(card) => card.bvid.clone(),
                    None => {
                        crate::debug_log("prefetch live skipped reason=no-feed-item");
                        return None;
                    }
                }
            }
        };
        let detail = match runtime.block_on(session.video_view(&bvid)) {
            Ok(detail) => detail,
            Err(error) => {
                crate::debug_log(&format!("prefetch live skipped reason=video_view err={error}"));
                return None;
            }
        };
        let info = match runtime.block_on(session.playurl(&bvid, detail.cid.max(0) as u64)) {
            Ok(info) => info,
            Err(error) => {
                crate::debug_log(&format!("prefetch live skipped reason=playurl err={error}"));
                return None;
            }
        };
        let track = match info.video_tracks.iter().max_by_key(|track| track.bandwidth) {
            Some(track) => track,
            None => {
                crate::debug_log("prefetch live skipped reason=no-video-track");
                return None;
            }
        };
        let mut hosts: Vec<String> = Vec::new();
        for candidate in &track.urls {
            if let Ok(parsed) = reqwest::Url::parse(candidate) {
                if let Some(host) = parsed.host_str() {
                    if !hosts.iter().any(|known| known == host) {
                        hosts.push(host.to_string());
                    }
                }
            }
        }
        crate::debug_log(&format!(
            "prefetch live target bvid={bvid} cid={} duration={} hosts={hosts:?}",
            detail.cid, info.duration
        ));
        Some((track.url.clone(), hosts, info.duration))
    }

    fn ffmpeg_bin() -> std::path::PathBuf {
        std::env::var("BILI_FFMPEG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Programs\ffmpeg-unpack\bin"))
            .join("ffmpeg.exe")
    }

    fn first_pts_time(stderr_text: &str) -> Option<f64> {
        let index = stderr_text.find("pts_time:")?;
        let rest = &stderr_text[index + "pts_time:".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .unwrap_or(rest.len());
        rest[..end].parse::<f64>().ok()
    }

    /// Decode `frames` video frames from `local`, exactly as the player does
    /// (`-ss` before `-i`, the same reconnect policy), and return the frames
    /// that arrived plus ffmpeg's stderr.
    fn decode_video(local: &str, start_secs: u64, frames: u64) -> (u64, String) {
        use std::io::Read;
        use std::process::{Command, Stdio};
        const FRAME_W: u64 = 960;
        const FRAME_H: u64 = 540;
        const FRAME_BYTES: u64 = FRAME_W * FRAME_H * 4;

        let mut command = Command::new(ffmpeg_bin());
        command
            .arg("-hide_banner")
            // Keep the media timestamps, so `showinfo` reports the position the
            // seek actually landed on instead of a re-based 0.
            .arg("-copyts")
            .arg("-loglevel")
            .arg("info")
            .arg("-nostdin")
            .arg("-threads")
            .arg("0")
            .arg("-reconnect")
            .arg("1")
            .arg("-reconnect_streamed")
            .arg("1")
            .arg("-reconnect_delay_max")
            .arg("5")
            .arg("-rw_timeout")
            .arg("15000000");
        if start_secs > 0 {
            command.arg("-ss").arg(format!("{start_secs}.000"));
        }
        command
            .arg("-i")
            .arg(local)
            .arg("-an")
            .arg("-frames:v")
            .arg(frames.to_string())
            .arg("-vf")
            .arg(format!("scale={FRAME_W}:{FRAME_H},showinfo"))
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                crate::debug_log(&format!("prefetch seek ffmpeg spawn failed err={error}"));
                return (0, error.to_string());
            }
        };
        let mut child_stdout = child.stdout.take();
        let mut child_stderr = child.stderr.take();
        let out = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut stream) = child_stdout.take() {
                let _ = stream.read_to_end(&mut buffer);
            }
            buffer
        });
        let err = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut stream) = child_stderr.take() {
                let _ = stream.read_to_end(&mut buffer);
            }
            String::from_utf8_lossy(&buffer).into_owned()
        });
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let stderr_text = match err.join() {
            Ok(text) => text,
            Err(_) => String::new(),
        };
        let bytes = match out.join() {
            Ok(bytes) => bytes.len() as u64,
            Err(_) => 0,
        };
        (bytes / FRAME_BYTES, stderr_text)
    }

    /// Decode a real 1080p stream *through the proxy* while the whole file is
    /// being prefetched, and report what the decoder itself achieves.
    ///
    /// `$env:BILI_DEBUG_LOG="prefetch-decode.log"`
    /// `$env:BILI_PREFETCH_BVID="<bvid>"; $env:BILI_PREFETCH_BENCH_SECS="90"`
    /// `cargo test --release -- --ignored --nocapture live_decode_under_prefetch`
    ///
    /// This is the prefetch-scoped half of the "30 fps" gate: the player
    /// window's `fps_presented` also depends on the renderer and the frame
    /// interpolator, which this module does not own. What is asserted here is
    /// that the fetch pool never starved the decoder: the whole file ends up
    /// local while it decodes, no range failed, and ffmpeg never saw a partial
    /// file. Run it with `BILI_PREFETCH_HORIZON_S=90` to compare against the
    /// previous queue policy.
    #[test]
    #[ignore = "network: runs the real ffmpeg against a real media file"]
    fn live_decode_under_prefetch() {
        let Some((url, _hosts, duration)) = live_media() else {
            return;
        };
        let seconds = std::env::var("BILI_PREFETCH_BENCH_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(90)
            .max(30);
        let frames = seconds.saturating_mul(30);
        let mut handle = match spawn(
            &url,
            PrefetchConfig {
                connections: 16,
                warm_points: 10,
                media_duration_ms: (duration > 0).then(|| duration.saturating_mul(1000)),
                cache_key: Some(format!("decode-{}-{seconds}", std::process::id())),
                ..PrefetchConfig::default()
            },
        ) {
            Ok(handle) => handle,
            Err(error) => {
                crate::debug_log(&format!("prefetch decode spawn failed err={error}"));
                return;
            }
        };
        let local = handle.local_url.clone();
        // Sample the fetch side once a second while the decoder runs, on its own
        // thread: the point is the pair (decode rate, fetch concurrency).
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let engine = handle.shared.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut tick = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(1));
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    tick = tick.saturating_add(1);
                    let stats = engine.snapshot();
                    let cached = ranges_bytes(&engine.cached_ranges());
                    crate::debug_log(&format!(
                        "prefetch fetch active={} inflight={} live={} configured=16 agg={}/s bytes={} cached={} window={} queue={}",
                        stats.active_workers,
                        stats.inflight,
                        engine.desired_connections(),
                        format_rate(stats.bytes_per_sec),
                        format_bytes(stats.bytes_fetched),
                        format_bytes(cached),
                        format_bytes(stats.read_ahead_bytes),
                        stats.queue_chunks
                    ));
                }
            })
        };
        let at = Instant::now();
        let (decoded, stderr_text) = decode_video(&local, 0, frames);
        let wall = at.elapsed().as_secs_f64();
        stop.store(true, Ordering::SeqCst);
        let cached = ranges_bytes(&handle.cached_ranges());
        let stats = handle.snapshot();
        let decode_fps = decoded as f64 / wall.max(0.001);
        let media_s = decoded as f64 / 29.766;
        crate::debug_log(&format!(
            "prefetch decode result frames={decoded} wall_s={wall:.2} decode_fps={decode_fps:.2} media_s={media_s:.1} realtime={:.2}x cached={} total={} fetched={} failed_ranges={}",
            media_s / wall.max(0.001),
            format_bytes(cached),
            format_bytes(stats.bytes_total.unwrap_or(0)),
            format_bytes(stats.bytes_fetched),
            stats.failed_ranges
        ));
        println!(
            "decode frames={decoded} wall={wall:.2}s decode_fps={decode_fps:.2} realtime={:.2}x cached={}",
            media_s / wall.max(0.001),
            format_bytes(cached)
        );
        assert!(decoded > 0, "the proxy must feed the decoder");
        assert!(
            !stderr_text.contains("partial file"),
            "ffmpeg must not see a partial file"
        );
        // The gate is starvation, not the codec's own ceiling: this box decodes
        // 1080p→540p rgba with `-threads 0` at ~12 fps whatever the network
        // does, so asserting 30 fps here would measure the cpu, not the proxy.
        // What must hold is that the fetch pool never made the decoder wait for
        // bytes: the whole file is local by the end, no range failed, and the
        // decode progressed the whole time.
        assert!(
            stats.bytes_total.is_some_and(|total| cached + (16 << 20) >= total),
            "the whole file must be cached while the decoder ran: {cached} of {:?}",
            stats.bytes_total
        );
        assert_eq!(stats.failed_ranges, 0, "no range may fail under load");
        handle.shutdown();
        let _ = sampler.join();
    }

    /// The reported bug, end to end: a forward seek past the read-ahead window
    /// through the proxy left ffmpeg with `root atom offset …: partial file`
    /// and no frames, because the abandoned open-ended response kept the serve
    /// lock while it wrote into a socket ffmpeg no longer read.
    ///
    /// Runs the real `ffmpeg.exe` (never a synthetic client) at the video's
    /// start, half way, an eighth (backwards), and the last five seconds.
    ///
    /// `$env:BILI_DEBUG_LOG="prefetch-seek.log"; $env:BILI_PREFETCH_LOG="1"`
    /// `$env:BILI_PREFETCH_BVID="<bvid longer than the window>"`
    /// `cargo test --release -- --ignored --nocapture live_ffmpeg_seeks`
    #[test]
    #[ignore = "network: runs the real ffmpeg against a real media file"]
    fn live_ffmpeg_seeks() {
        let Some((url, hosts, duration)) = live_media() else {
            return;
        };
        if duration < 30 {
            crate::debug_log(&format!(
                "prefetch seek skipped reason=short duration={duration}"
            ));
            return;
        }
        let mut handle = match spawn(
            &url,
            PrefetchConfig {
                hosts: hosts.clone(),
                ..PrefetchConfig::default()
            },
        ) {
            Ok(handle) => handle,
            Err(error) => {
                crate::debug_log(&format!("prefetch seek skipped reason=spawn err={error}"));
                return;
            }
        };
        let local = handle.local_url.clone();
        let cases: [(u64, &str); 4] = [
            (0, "start"),
            (duration / 2, "forward"),
            (duration / 8, "backward"),
            (duration.saturating_sub(5), "end"),
        ];
        let mut failures: Vec<String> = Vec::new();
        for (target, label) in cases {
            let before = handle.snapshot();
            let (frames, stderr_text) = decode_video(&local, target, 30);
            let after = handle.snapshot();
            let pts = first_pts_time(&stderr_text);
            let partial = stderr_text.contains("partial file");
            let landed = pts.is_some_and(|pts| (pts - target as f64).abs() <= 3.0);
            crate::debug_log(&format!(
                "prefetch seek case={label} target_s={target} frames={frames} pts={pts:?} landed={landed} partial_file={partial} fetched={} point_hits={} warm={}/{}",
                after.bytes_fetched.saturating_sub(before.bytes_fetched),
                after.point_hits.saturating_sub(before.point_hits),
                after.warm_points_done,
                after.warm_points_total
            ));
            if frames == 0 {
                failures.push(format!("{label}: no frames decoded"));
            }
            if partial {
                failures.push(format!("{label}: ffmpeg reported a partial file"));
            }
            if !landed {
                failures.push(format!("{label}: decoded position {pts:?} is not {target}s"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let stats = handle.snapshot();
        crate::debug_log(&format!(
            "prefetch seek done total={:?} fetched={} failed_ranges={} point_hits={} point_bytes={} warm={}/{}",
            stats.bytes_total,
            stats.bytes_fetched,
            stats.failed_ranges,
            stats.point_hits,
            stats.point_bytes,
            stats.warm_points_done,
            stats.warm_points_total
        ));
        handle.shutdown();
        assert!(failures.is_empty(), "{failures:?}");
    }

    // ---- session cache file -------------------------------------------------

    /// A cache in its own temporary directory, removed when the test ends.
    struct TempCache {
        dir: PathBuf,
        cache: DiskCache,
    }

    impl TempCache {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "o11a1-cache-test-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let cache = DiskCache::open_at(dir.clone(), name);
            Self { dir, cache }
        }
    }

    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn disk_cache_tracks_exact_written_ranges() {
        let mut temp = TempCache::new("ranges");
        temp.cache.adopt_len(4 * CACHE_BLOCK_BYTES);
        // An unaligned write (the shape of a network read) is tracked exactly:
        // this was the bug in the earlier 256 KiB bitmap, where a unit that
        // straddled two reads never completed and a full file reported empty.
        temp.cache.write(0, &vec![1u8; 100_000]);
        assert!(temp.cache.end_at(0) > 0);
        assert_eq!(temp.cache.ranges(), vec![(0, 100_000)]);
        assert_eq!(temp.cache.end_at(0), 100_000);
        assert!(temp.cache.read_at(0, 16).is_some());
        // A gap is a real gap: the run ends there.
        temp.cache.write(200_000, &vec![2u8; 50_000]);
        assert_eq!(temp.cache.end_at(0), 100_000);
        assert_eq!(temp.cache.end_at(150_000), 0);
        assert_eq!(temp.cache.end_at(200_000), 250_000);
        assert_eq!(temp.cache.ranges(), vec![(0, 100_000), (200_000, 250_000)]);
        assert_eq!(temp.cache.bytes(), 150_000);
        // The next write bridges the gap and the two ranges become one.
        temp.cache.write(100_000, &vec![3u8; 100_000]);
        assert_eq!(temp.cache.ranges(), vec![(0, 250_000)]);
        assert_eq!(temp.cache.first_hole(), 250_000);
        // Adjacent writes coalesce too.
        temp.cache.write(250_000, &vec![4u8; 10_000]);
        assert_eq!(temp.cache.ranges(), vec![(0, 260_000)]);
        assert_eq!(temp.cache.len(), 4 * CACHE_BLOCK_BYTES);
    }

    #[test]
    fn disk_cache_keeps_ranges_across_a_seek_shaped_rewrite() {
        let mut temp = TempCache::new("stable");
        temp.cache.adopt_len(8 * CACHE_BLOCK_BYTES);
        temp.cache
            .write(0, &vec![3u8; (4 * CACHE_BLOCK_BYTES) as usize]);
        let before = temp.cache.ranges();
        assert_eq!(before, vec![(0, 4 * CACHE_BLOCK_BYTES)]);
        // Reading another region (what a seek does) changes the file, never the
        // ranges that were already there.
        temp.cache.write(
            6 * CACHE_BLOCK_BYTES,
            &vec![4u8; CACHE_BLOCK_BYTES as usize],
        );
        let after = temp.cache.ranges();
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], (6 * CACHE_BLOCK_BYTES, 7 * CACHE_BLOCK_BYTES));
    }

    #[test]
    fn disk_cache_tail_read_stops_at_the_media_end() {
        let mut temp = TempCache::new("tail");
        // 2.5 blocks; the last write ends exactly at the media end.
        let total = 2 * CACHE_BLOCK_BYTES + CACHE_BLOCK_BYTES / 2;
        temp.cache.adopt_len(total);
        temp.cache.write(0, &vec![5u8; total as usize]);
        assert_eq!(temp.cache.ranges(), vec![(0, total)]);
        assert_eq!(temp.cache.end_at(total - 1), total);
        assert_eq!(temp.cache.end_at(total), 0);
        // A write past the known length is clamped away, not recorded.
        temp.cache.write(total, &vec![6u8; 1024]);
        assert_eq!(temp.cache.ranges(), vec![(0, total)]);
    }

    #[test]
    fn disk_cache_refuses_bytes_past_the_cap() {
        let mut temp = TempCache::new("cap");
        temp.cache.adopt_len(CACHE_CAP_BYTES + CACHE_BLOCK_BYTES);
        // A write that crosses the cap is dropped whole, and the flag is set.
        temp.cache.write(CACHE_CAP_BYTES, &vec![1u8; 4096]);
        assert!(temp.cache.capped);
        assert!(temp.cache.ranges().is_empty());
        // Nothing past the cap is recorded, but the memory path is unaffected —
        // the cache only ever refuses its own writes.
        temp.cache.write(0, &vec![1u8; CACHE_BLOCK_BYTES as usize]);
        assert_eq!(temp.cache.ranges(), vec![(0, CACHE_BLOCK_BYTES)]);
    }

    #[test]
    fn disk_cache_removes_its_file() {
        let mut temp = TempCache::new("remove");
        temp.cache.adopt_len(2 * CACHE_BLOCK_BYTES);
        temp.cache.write(0, &vec![7u8; CACHE_BLOCK_BYTES as usize]);
        let path = temp.cache.path.clone();
        assert!(path.exists());
        temp.cache.remove();
        // The removal runs on a bounded retry thread; a shared handle is gone
        // here, so the first attempt succeeds.
        for _ in 0..40 {
            if !path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!path.exists());
    }

    #[test]
    fn disk_cache_map_round_trips_for_a_preload() {
        let dir = std::env::temp_dir().join(format!("o11a1-cache-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut first = DiskCache::open_at(dir.clone(), "roundtrip");
        first.adopt_len(10 * CACHE_BLOCK_BYTES);
        first.write(0, &vec![1u8; (3 * CACHE_BLOCK_BYTES) as usize]);
        first.write(7 * CACHE_BLOCK_BYTES, &vec![2u8; CACHE_BLOCK_BYTES as usize]);
        assert!(first.save_map());
        let expected = first.ranges();
        drop(first);
        // Reopening adopts the saved map instead of truncating.
        let mut second = DiskCache::open_at(dir.clone(), "roundtrip");
        assert_eq!(second.len(), 10 * CACHE_BLOCK_BYTES);
        assert_eq!(second.ranges(), expected);
        assert_eq!(second.first_hole(), 3 * CACHE_BLOCK_BYTES);
        assert!(second.read_at(7 * CACHE_BLOCK_BYTES, 16).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_key_from_url_keeps_the_stream_identity() {
        let url = "https://upos-sz-mirrorcos.bilivideo.com/upgcxcode/17/88/42872408817/42872408817-1-30080.m4s?e=abc&deadline=1";
        assert_eq!(cache_key_from_url(url), "42872408817-42872408817-1-30080");
        // A different stream of the same video is a different file.
        let other = url.replace("30080", "30077");
        assert_ne!(cache_key_from_url(url), cache_key_from_url(&other));
        // Query strings (expiring signatures) never leak into the name.
        assert_eq!(
            cache_key_from_url(url),
            cache_key_from_url(url.split('?').next().unwrap_or(url))
        );
        assert!(!cache_key_name("../a b/c:d").contains(['/', ':', ' ']));
    }

    #[test]
    fn published_downloaded_is_the_disk_map_after_a_seek() {
        let mut temp = TempCache::new("publish");
        temp.cache.adopt_len(8 * CACHE_BLOCK_BYTES);
        temp.cache.write(0, &vec![9u8; CACHE_BLOCK_BYTES as usize]);
        let engine = test_engine();
        if let Ok(mut disk) = engine.disk.lock() {
            *disk = DiskCache::open_at(temp.dir.clone(), "publish");
            disk.write(0, &vec![9u8; CACHE_BLOCK_BYTES as usize]);
        }
        let before = engine.snapshot().downloaded;
        assert_eq!(before, vec![(0, CACHE_BLOCK_BYTES)]);
        // A far seek drops the memory window; the map is the file and must not
        // move.
        engine.seek(4 * CACHE_BLOCK_BYTES);
        assert_eq!(engine.snapshot().downloaded, before);
        assert_eq!(engine.cached_ranges(), before);
        assert_eq!(engine.cached_bytes_at(0), CACHE_BLOCK_BYTES);
    }

    /// The single writer is the whole point of this module's disk path: one
    /// thread, one merged call per contiguous run, flushed before shutdown.
    #[test]
    fn single_writer_merges_adjacent_blocks_and_flushes_on_shutdown() {
        let dir = std::env::temp_dir().join(format!("o11a1-writer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = Arc::new(Mutex::new(DiskCache::open_at(dir.clone(), "writer")));
        if let Ok(mut cache) = disk.lock() {
            cache.adopt_len(64 * 1024);
        }
        let mut writer = DiskWriter::spawn(disk.clone()).expect("a cache file opens");
        // Twelve 4 KiB network reads of one contiguous range arrive one at a
        // time: the writer merges them into a single run instead of twelve
        // calls, and shutdown flushes what is still open.
        for index in 0..12u64 {
            writer.write(index * 4096, Arc::from(vec![index as u8 + 1; 4096]));
        }
        writer.shutdown();
        let mut cache = disk.lock().expect("map lock");
        assert_eq!(cache.ranges(), vec![(0, 49_152)]);
        let bytes = cache.read_at(0, 49_152).expect("the merged run is in the file");
        assert_eq!(bytes.len(), 49_152);
        // Every block kept exactly the bytes it was sent with.
        for index in 0..12usize {
            assert_eq!(
                &bytes[index * 4096..(index + 1) * 4096],
                &vec![index as u8 + 1; 4096][..],
                "block {index} landed at the wrong offset"
            );
        }
        drop(cache);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session with no file has no writer thread: nothing to write, nothing
    /// to keep alive (this is the `BILI_DISK_CACHE=0` shape).
    #[test]
    fn a_memory_only_cache_has_no_writer() {
        let disk = Arc::new(Mutex::new(DiskCache::memory_only("memory".to_string())));
        assert!(!disk.lock().expect("map lock").present());
        assert!(DiskWriter::spawn(disk).is_none());
    }

    /// The coalescer's merge itself: adjacent, overlapping and prepending
    /// blocks all stay one contiguous run, with the newest bytes winning.
    #[test]
    fn coalescer_keeps_runs_contiguous() {
        let mut run = OpenRun {
            start: 100,
            bytes: vec![0u8; 10],
            last: Instant::now(),
        };
        run.absorb(110, &[7u8; 10]);
        assert_eq!((run.start, run.bytes.len()), (100, 20));
        assert_eq!(&run.bytes[10..], &[7u8; 10][..]);
        // A block starting before the run prepends, keeps the tail, and its own
        // bytes win where they overlap.
        run.absorb(90, &[9u8; 15]);
        assert_eq!((run.start, run.bytes.len()), (90, 30));
        assert_eq!(&run.bytes[..15], &[9u8; 15][..]);
        assert_eq!(&run.bytes[15..20], &[0u8; 5][..]);
        assert_eq!(&run.bytes[20..], &[7u8; 10][..]);
    }

    /// A `206` that stops early must never rewrite the length the file's own
    /// `Content-Range` stated. Doing so made the serving body close at that
    /// offset — ffmpeg's "Stream ends prematurely at N, should be M" — and the
    /// player then ran its whole "playback ended" path mid-video.
    #[test]
    fn a_short_body_never_rewrites_the_known_length() {
        let engine = test_engine();
        let chunk = 4 << 20;
        let total = 60_412_891u64;
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(total);
        }
        let shutdown = AtomicBool::new(false);

        assert!(
            engine.complete_range(0, chunk, 11_959_445, 4 * chunk, &shutdown),
            "a short body against a known length is reported to the caller"
        );
        assert_eq!(engine.total(), Some(total), "the length must not shrink");
        if let Ok(state) = engine.state.lock() {
            assert!(!state.done.contains(&chunk), "a short range is not fetched");
            assert!(state.requeued.contains(&chunk), "it goes back to the queue");
            assert!(state.dead.is_empty(), "and it is not dead yet");
        }

        // A range that keeps stopping early is eventually retired — still
        // without touching the length.
        for _ in 0..SHORT_BODY_LIMIT {
            engine.complete_range(0, chunk, 11_959_445, 4 * chunk, &shutdown);
        }
        assert_eq!(engine.total(), Some(total));
        if let Ok(state) = engine.state.lock() {
            assert!(state.dead.contains(&(0, chunk)));
        }

        // With no length known at all a short body is still EOF evidence (that
        // is the only place the length can come from on such a response).
        let unknown = test_engine();
        assert!(!unknown.complete_range(0, 0, 4096, chunk, &shutdown));
        assert_eq!(unknown.total(), Some(4096));
    }

    /// The whole-file queue is a convenience for a later seek, not playback:
    /// it must not fetch while the bucket is empty or while the file is already
    /// further ahead of the playhead than [`PREFETCH_AHEAD_SECONDS`]. It must
    /// also never be starved by the bucket: one chunk per live connection is
    /// exactly the allowance, so a configured pool can actually run.
    #[test]
    fn whole_file_queue_is_rate_limited_and_horizon_bounded() {
        let mut engine = test_engine();
        let chunk = 1 << 20;
        engine.config.chunk_bytes = chunk;
        engine.config.warm_points = 1;
        let bitrate = 400_000u64; // ~3.2 Mbit/s
        let mut disk = test_disk();
        {
            let mut state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.total = Some(400 << 20);
            state.cursor = 0;
            state.warm_plan_started = false;
            // Fill the hot window: everything up to the frontier is spoken for.
            let frontier = (bitrate * HOT_AHEAD_SECONDS).max(chunk * 2);
            let mut start = 0;
            while start < frontier {
                state.inflight.insert((0, start));
                start += chunk;
            }
            // The bucket starts with one burst; spend it so the next claim has
            // nothing left.
            state.spec_tokens = 0;
            state.spec_at = Instant::now();
        }
        let mut state = match engine.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, bitrate, &disk),
            None,
            "an empty speculative bucket must idle the worker"
        );
        // A full bucket lets one chunk per live connection through, then the
        // refill rate takes over.
        state.spec_tokens = chunk;
        state.spec_at = Instant::now() + Duration::from_secs(600);
        assert!(matches!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, bitrate, &disk),
            Some((_, ChunkKind::Rest))
        ));
        // The steady budget is the ceiling, not a multiple of the (tiny) stream
        // bitrate: the old 2× cap is what starved the pool down to two workers.
        assert_eq!(
            speculative_rate(bitrate, false, false),
            SPECULATIVE_CEILING_BPS,
            "the steady budget is the ceiling; the live count bounds parallelism"
        );
        assert_eq!(
            speculative_burst(chunk, 16, false),
            chunk * 16,
            "every configured connection may hold one claim"
        );
        // The opening keeps the small budget, whatever the connection count.
        assert_eq!(
            speculative_rate(bitrate, false, true),
            MIN_SPECULATIVE_CONNECTION_BPS,
            "the opening must not open sixteen speculative claims"
        );
        assert_eq!(
            speculative_burst(chunk, 16, true),
            chunk * 2,
            "the opening burst is two chunks"
        );
        // Ahead of the horizon: the file is already downloaded far enough.
        // (A slow stream keeps the 1-hour horizon inside the file, which is the
        // case the bound exists for.)
        let horizon = 1000u64 * PREFETCH_AHEAD_SECONDS;
        state.spec_tokens = chunk;
        state.spec_at = Instant::now() + Duration::from_secs(600);
        disk.insert(0, horizon);
        assert_eq!(
            pick_chunk(&mut state, 0, &engine.config, false, false, 0, 1, 1000, &disk),
            None,
            "nothing may be fetched past the {PREFETCH_AHEAD_SECONDS}s horizon"
        );
    }

    /// The read-ahead window is the configured size, but never more than
    /// [`PLAYBACK_AHEAD_SECONDS`] of the stream (a 400 kbit/s video must not
    /// buffer ten minutes), which is what keeps memory and trim work bounded.
    #[test]
    fn read_ahead_window_is_capped_to_the_horizon() {
        let mut engine = test_engine();
        engine.config.read_ahead_bytes = 64 << 20;
        engine.config.media_duration_ms = Some(600_000);
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(60_000_000);
        }
        // 60 MB over 600 s = 100 kB/s -> 90 s is 9 MB.
        engine.refresh_media_bps(60_000_000);
        assert_eq!(engine.media_bps(), 100_000);
        assert_eq!(engine.read_ahead_window(), 9_000_000);
        // Without a duration the configured window stands (fallback path).
        let plain = test_engine();
        assert_eq!(plain.read_ahead_window(), DEFAULT_READ_AHEAD_BYTES);
    }
}

