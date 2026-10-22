//! Local loopback prefetch proxy for the player.
//!
//! `player.rs` runs ffmpeg against a CDN URL and reads raw frames from its
//! stdout. ffmpeg's HTTP client is single-connection and reports no byte
//! counters. This module puts a tiny HTTP/1.1 server on `127.0.0.1:<random>`
//! between ffmpeg and the CDN:
//!
//! * ffmpeg reads the local URL; every `Range: bytes=N-` it sends is served
//!   from a read-ahead window that [`PrefetchConfig::connections`] tasks fill
//!   with `Range` requests spread round-robin over [`PrefetchConfig::hosts`].
//! * the engine counts exactly how many bytes it pulled from upstream, per
//!   host and per second, which is what the profiler panel shows.
//!
//! Once the first bytes have reached the client, a separate background task
//! warms [`PrefetchConfig::warm_points`] evenly spaced positions of the file
//! (`i / count` of the total length) into a bounded **point cache**. A request
//! whose offset falls inside a cached point is answered from memory at once;
//! anything else still resets the read-ahead window exactly as before. The
//! warm task fetches one small chunk per point (4 MiB, or `total / count` when
//! the file is small), never more than two requests in flight, so it cannot
//! starve playback; the whole point cache is capped at 80 MiB.
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
//! Shutdown is drop-based: dropping [`PrefetchHandle`] signals every task,
//! aborts any connection that is still streaming, and closes the listener.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
/// Default number of parallel upstream fetchers.
pub const DEFAULT_CONNECTIONS: usize = 4;
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

const MIN_READ_AHEAD_BYTES: u64 = 8 << 20;
const MAX_READ_AHEAD_BYTES: u64 = 128 << 20;
const MIN_CHUNK_BYTES: u64 = 256 << 10;
const MAX_CHUNK_BYTES: u64 = 16 << 20;
const MAX_CONNECTIONS: usize = 6;

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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the first client request waits for the first upstream response
/// headers (which carry the total length) before failing with a `504`.
const FIRST_METADATA_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// One second of traffic history is what `bytes_per_sec` averages over.
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Quiet time after the first served byte before warming starts, so the first
/// response (and the reader's initial burst) is never slowed by it.
const WARM_DELAY: Duration = Duration::from_millis(1500);
/// Pause between two warmed points; one sequential small fetch at a time.
const WARM_POINT_PAUSE: Duration = Duration::from_millis(150);
/// How long to keep waiting for a client to read its first bytes before giving
/// up on warming (a proxy nobody plays wastes no traffic).
const WARM_START_TIMEOUT: Duration = Duration::from_secs(60);
/// Slow poll of the shared state while waiting for a safe moment to warm.
const WARM_POLL: Duration = Duration::from_millis(50);
/// How long the read-ahead window may stay starved before warming proceeds
/// anyway (a paused reader leaves the window full, so this only covers EOF and
/// a client that stopped reading).
const WARM_STARVED_MS: Duration = Duration::from_millis(750);

// ---------------------------------------------------------------------------
// Public configuration and stats
// ---------------------------------------------------------------------------

/// Tuning for one proxy instance. [`PrefetchConfig::normalized`] clamps every
/// field, so callers may pass anything.
#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Bytes to keep buffered beyond the read cursor (8..=128 MiB).
    pub read_ahead_bytes: u64,
    /// Size of one upstream range request (256 KiB..=16 MiB, at most the
    /// read-ahead window).
    pub chunk_bytes: u64,
    /// Parallel upstream fetchers (1..=6).
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
        self.connections = self.connections.clamp(1, MAX_CONNECTIONS);
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
    /// `false` while the worker waits for window space (or after it stopped).
    pub busy: bool,
    /// Bytes this worker has fetched since the proxy started.
    pub bytes: u64,
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
    /// Same value as [`PrefetchStats::bytes_total`], under the name the panel
    /// uses for the heatmap denominator.
    pub total_bytes: Option<u64>,
    /// Workers with `busy == true` at snapshot time.
    pub active_workers: u32,
    /// `[start, end)` ranges currently held by the warm point cache.
    pub point_cache: Vec<(u64, u64)>,
}

/// Handle to a running proxy. Dropping it shuts the server down.
pub struct PrefetchHandle {
    /// `http://127.0.0.1:<port><path>?<original query>`.
    pub local_url: String,
    /// Published stats; refreshed on every upstream read. [`PrefetchHandle::snapshot`]
    /// additionally recomputes `bytes_per_sec` so it decays while idle.
    pub stats: Arc<Mutex<PrefetchStats>>,
    shared: Arc<Engine>,
    shutdown: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
    runtime: &'static tokio::runtime::Runtime,
}

impl PrefetchHandle {
    /// Current counters, with `bytes_per_sec` recomputed against the live
    /// one-second window (the published copy is refreshed on chunk arrival).
    pub fn snapshot(&self) -> PrefetchStats {
        self.shared.snapshot()
    }

    /// Stop the server and every fetch task, then wait (bounded) until the
    /// listener is closed. Safe to call more than once.
    ///
    /// Must be called from a synchronous thread; inside a Tokio runtime this
    /// falls back to aborting the server task because nested `block_on`
    /// panics.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let Some(mut task) = self.server.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            task.abort();
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
    let shared = Arc::new(Engine {
        config: config.clone(),
        target: target.clone(),
        client,
        state: Mutex::new(State {
            base: 0,
            cursor: 0,
            generation: 0,
            chunks: BTreeMap::new(),
            inflight: BTreeSet::new(),
            dead: BTreeSet::new(),
            total: None,
            fatal: None,
            content_type: None,
            hosts: host_specs
                .into_iter()
                .map(|spec| Host {
                    label: spec.label,
                    origin: spec.origin,
                    failed: false,
                    bytes: 0,
                })
                .collect(),
            next_host: 0,
            samples: VecDeque::new(),
            bytes_fetched: 0,
            failed_ranges: 0,
            served: 0,
            read_ahead: 0,
            points: PointCache::default(),
            warm_points_total: 0,
            warm_points_done: 0,
            warm_chunk: 0,
            warm_bytes: 0,
            point_hits: 0,
            point_served: 0,
            workers: (0..config.connections)
                .map(|_| WorkerSlot::default())
                .collect(),
        }),
        stats: stats.clone(),
        serving: AtomicBool::new(false),
        serve_epoch: AtomicU64::new(0),
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
        "prefetch spawn port={} read_ahead={} chunk={} connections={} warm_points={} point_bytes={} hosts={} startup=first-chunk",
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
            .unwrap_or_default()
    ));

    Ok(PrefetchHandle {
        local_url,
        stats,
        shared,
        shutdown,
        server: Some(server),
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

/// `3.2 MB/s` for the QA stats line; counted bytes, so no `≈`.
fn format_rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec >= 1_000_000 {
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

/// One-second sliding throughput. Traffic older than the window is dropped;
/// the rate is extrapolated while the window is still filling and never
/// divides by less than 250 ms (so the first chunk does not read as a spike).
fn sliding_rate(samples: &mut VecDeque<(Instant, u64)>, now: Instant) -> u64 {
    while samples.len() > 1 && now.duration_since(samples[0].0) > RATE_WINDOW {
        samples.pop_front();
    }
    let Some(&(first, _)) = samples.front() else {
        return 0;
    };
    // A window that has seen no traffic for a full second reads as zero
    // instead of holding its last value forever.
    if now.duration_since(samples.back().map_or(first, |(stamp, _)| *stamp)) > RATE_WINDOW {
        return 0;
    }
    let bytes: u64 = samples.iter().map(|(_, bytes)| *bytes).sum();
    let span = now
        .duration_since(first)
        .as_secs_f64()
        .clamp(0.25, 1.0);
    (bytes as f64 / span).round() as u64
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Host {
    label: String,
    origin: String,
    failed: bool,
    bytes: u64,
}

/// Live state of one read-ahead worker, owned by the state lock so a snapshot
/// sees the exact range each connection holds.
#[derive(Clone, Debug, Default)]
struct WorkerSlot {
    host: String,
    start: u64,
    end: u64,
    busy: bool,
    bytes: u64,
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
    total: Option<u64>,
    fatal: Option<String>,
    content_type: Option<String>,
    hosts: Vec<Host>,
    next_host: usize,
    samples: VecDeque<(Instant, u64)>,
    bytes_fetched: u64,
    failed_ranges: u64,
    served: u64,
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
    /// Requests served from the point cache, and bytes those served.
    point_hits: u64,
    point_served: u64,
    /// One slot per configured connection (`workers.len() == connections`).
    workers: Vec<WorkerSlot>,
}

struct Engine {
    config: PrefetchConfig,
    target: Target,
    client: reqwest::Client,
    state: Mutex<State>,
    stats: Arc<Mutex<PrefetchStats>>,
    /// Serialises response handlers (the read cursor is shared).
    serving: AtomicBool,
    /// Token of the response that may write now. Every accepted request bumps
    /// it; a handler that sees a newer token stops, so a seek is never queued
    /// behind the open-ended response ffmpeg abandoned.
    serve_epoch: AtomicU64,
    started: Instant,
}

impl Engine {
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
            stats.workers.clear();
            stats.workers.extend(
                state
                    .workers
                    .iter()
                    .enumerate()
                    .map(|(id, slot)| WorkerStat {
                        id: id as u32,
                        host: slot.host.clone(),
                        start: slot.start,
                        end: slot.end,
                        busy: slot.busy,
                        bytes: slot.bytes,
                    }),
            );
            stats.active_workers = state.workers.iter().filter(|slot| slot.busy).count() as u32;
            // Heatmap view: everything held now, as one compact range list.
            // The chunks are already offset-ordered; the point cache is added
            // and the whole list is coalesced/capped by `merge_ranges`.
            let mut ranges: Vec<(u64, u64)> =
                Vec::with_capacity(state.chunks.len() + state.points.entries.len());
            ranges.extend(state.chunks.iter().filter_map(|(start, data)| {
                let len = data.len() as u64;
                (len > 0).then_some((*start, start.saturating_add(len)))
            }));
            let points = point_ranges(&state.points);
            ranges.extend(points.iter().copied());
            stats.downloaded = merge_ranges(ranges, MAX_DOWNLOADED_RANGES);
            stats.point_cache = points;
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

    /// Point the read-ahead window at `offset` (the offset of the response
    /// being served now, not the old cursor).
    ///
    /// The window is kept only when it is already serving exactly that offset:
    /// the cursor sits there and either the chunk grid is still aligned there
    /// (the workers are fetching for it right now) or the byte is buffered.
    /// Every other request — a seek forward, a seek backward, or a cursor that
    /// an abandoned response moved past the last byte it actually delivered —
    /// recomputes the window from the requested offset, so the workers never
    /// fetch for a position the client has left.
    fn seek(&self, offset: u64) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        // `base == offset` means nothing was consumed since the last reset, so
        // the window is still exactly the one this offset needs — which is the
        // first-request case: keep the chunks the workers already started.
        let ready = state.cursor == offset
            && (state.base == offset
                || state.total == Some(offset)
                || chunks_cover(&state.chunks, offset));
        if ready {
            return;
        }
        state.generation += 1;
        state.base = offset;
        state.cursor = offset;
        state.chunks.clear();
        state.inflight.clear();
        state.dead.clear();
        state.read_ahead = 0;
        drop(state);
        crate::debug_log(&format!(
            "prefetch seek reset offset={offset} after_ms={}",
            self.started.elapsed().as_millis()
        ));
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
        if let Some(error) = &state.fatal {
            return Err(error.clone());
        }
        if state.dead.contains(&(state.generation, pos)) {
            return Err(format!("区间 {pos} 无法从任何主机获取"));
        }
        let Some(data) = take_bounded(&mut state.chunks, pos, max_len) else {
            return Ok(None);
        };
        let take = data.len() as u64;
        state.cursor = pos + take;
        state.served += take;
        state.read_ahead = state.read_ahead.saturating_sub(take);
        self.publish(&mut state, Instant::now());
        Ok(Some(data))
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
    /// Returns `false` when the read belongs to a superseded generation (a seek
    /// moved the window) or the proxy is shutting down: the bytes are dropped
    /// and the caller stops reading this response instead of draining a body
    /// nobody can use.
    fn push_bytes(
        &self,
        generation: u64,
        offset: u64,
        data: &[u8],
        host: usize,
        worker: usize,
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
        state.chunks.insert(offset, data.to_vec());
        state.read_ahead += len;
        state.bytes_fetched += len;
        if let Some(host) = state.hosts.get_mut(host) {
            host.bytes += len;
        }
        if let Some(slot) = state.workers.get_mut(worker) {
            slot.bytes = slot.bytes.saturating_add(len);
        }
        state.samples.push_back((now, len));
        self.publish(&mut state, now);
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
        if changed {
            self.publish(&mut state, now);
        }
        drop(state);
        if let Some(total) = learned_total {
            crate::debug_log(&format!(
                "prefetch learned total={total} after_ms={}",
                self.started.elapsed().as_millis()
            ));
        }
    }

    /// Release the range claim after the upstream body ended (possibly short).
    fn complete_range(
        &self,
        generation: u64,
        chunk_start: u64,
        next: u64,
        end: u64,
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
        if next < end {
            match state.total {
                None => {
                    // Short body with no known length: this is EOF.
                    state.total = Some(next);
                    crate::debug_log(&format!(
                        "prefetch eof total={} after_ms={}",
                        next,
                        self.started.elapsed().as_millis()
                    ));
                }
                Some(total) if next < total => {
                    // A `206` that stops early is the CDN's word on the real
                    // end. Trust it; a client that was promised more sees a
                    // closed connection instead of a hang.
                    state.total = Some(next);
                    crate::debug_log(&format!(
                        "prefetch total shortened start={chunk_start} next={next} was={total}"
                    ));
                }
                _ => {}
            }
        }
        self.publish(&mut state, now);
    }

    /// Mark one host unusable after a failed range attempt. Called even when a
    /// retry on another host succeeds, so a broken mirror is not picked again.
    fn mark_host_failure(&self, host: usize, error: &str) {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(host) = state.hosts.get_mut(host) {
            host.failed = true;
        }
        state.failed_ranges += 1;
        self.publish(&mut state, now);
        drop(state);
        crate::debug_log(&format!(
            "prefetch host failed host={host} err={error} after_ms={}",
            self.started.elapsed().as_millis()
        ));
    }

    /// Give up on a range: the bytes at `next` will never arrive, so a reader
    /// waiting there gets an error. Stops every task when no host is left.
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
                host.failed = true;
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

    /// Complete media length, once any real range response revealed it.
    fn total(&self) -> Option<u64> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.total
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

    /// Publish the warm plan (`points` distinct offsets, `chunk` bytes each).
    fn start_warm(&self, points: usize, chunk: u64, total: u64) {
        let now = Instant::now();
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.warm_points_total = points as u32;
            state.warm_points_done = 0;
            state.warm_chunk = chunk;
            state.warm_bytes = 0;
            state.points = PointCache::default();
            self.publish(&mut state, now);
        }
        crate::debug_log(&format!(
            "seek warm plan total={total} points={points} chunk={chunk}"
        ));
    }

    /// Mark one point as available without fetching it (the window or an
    /// earlier point already holds those bytes).
    fn note_warm_covered(&self, offset: u64, source: &str) {
        let now = Instant::now();
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.warm_points_done = state.warm_points_done.saturating_add(1);
            self.publish(&mut state, now);
        }
        crate::debug_log(&format!("seek warm skip offset={offset} covered={source}"));
    }

    /// Mark a point that could not be fetched. It still counts as processed, so
    /// `done == total` once the task has walked the whole plan.
    fn note_warm_failed(&self, offset: u64) {
        let now = Instant::now();
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.warm_points_done = state.warm_points_done.saturating_add(1);
            self.publish(&mut state, now);
        }
        crate::debug_log(&format!(
            "seek warm skip offset={offset} covered=none reason=fetch-failed"
        ));
    }

    /// Account one warm-point network read. It IS upstream traffic (so it
    /// appears in `bytes_fetched` and the per-host split) but it is not part of
    /// the read-ahead window; it is also left out of the one-second rate
    /// samples, whose job is to describe playback throughput.
    fn note_warm_bytes(&self, host: usize, len: u64, shutdown: &AtomicBool) {
        if len == 0 || shutdown.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.bytes_fetched = state.bytes_fetched.saturating_add(len);
        if let Some(host) = state.hosts.get_mut(host) {
            host.bytes = host.bytes.saturating_add(len);
        }
        self.publish(&mut state, now);
    }

    /// Store one warm point and log the QA line as it lands.
    fn store_point(&self, offset: u64, data: Vec<u8>, count: usize, total: u64) {
        let len = data.len() as u64;
        let now = Instant::now();
        let (done, points, bytes, chunk) = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state
                .points
                .insert(offset, data, count.min(MAX_WARM_POINTS).max(1), MAX_POINT_CACHE_BYTES);
            state.warm_points_done = state.warm_points_done.saturating_add(1);
            state.warm_bytes = state.warm_bytes.saturating_add(len);
            self.publish(&mut state, now);
            (
                state.warm_points_done,
                state.warm_points_total,
                state.warm_bytes,
                state.warm_chunk,
            )
        };
        crate::debug_log(&format!(
            "seek warm total={total} points={points} chunk={chunk} done={done} bytes={bytes}"
        ));
    }

    /// Final warm summary (`done == points` once every offset was processed).
    fn finish_warm(&self) {
        let (points, done, bytes, chunk, cached, cached_points, total) = {
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                state.warm_points_total,
                state.warm_points_done,
                state.warm_bytes,
                state.warm_chunk,
                state.points.bytes,
                state.points.len(),
                state.total.unwrap_or(0),
            )
        };
        crate::debug_log(&format!(
            "seek warm done total={total} points={points} chunk={chunk} done={done} bytes={bytes} cached={cached} points_cached={cached_points} ms={}",
            self.started.elapsed().as_millis()
        ));
    }
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

enum Claim {
    Fetch {
        generation: u64,
        start: u64,
        end: u64,
        host: usize,
    },
    Wait,
    Stop,
}

/// Pick the next healthy host round-robin, excluding already-tried indices.
fn pick_host(state: &mut State, exclude: &[usize]) -> Option<usize> {
    let count = state.hosts.len();
    for step in 0..count {
        let index = (state.next_host + step) % count;
        if state.hosts[index].failed || exclude.contains(&index) {
            continue;
        }
        state.next_host = (index + 1) % count;
        return Some(index);
    }
    None
}

/// Reserve the next chunk of the read-ahead window for one worker.
///
/// `worker_id` names the slot this connection owns; the slot is updated under
/// the same lock that hands out the range, so a snapshot can never see a
/// claimed range with no worker behind it (or the reverse).
fn claim(engine: &Engine, shutdown: &AtomicBool, worker_id: usize) -> Claim {
    let mut state = match engine.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    if shutdown.load(Ordering::SeqCst) || state.fatal.is_some() {
        mark_worker_idle(&mut state, worker_id);
        return Claim::Stop;
    }
    // No probe gate: the first claims happen as soon as the proxy is bound, so
    // the real first chunk is already in flight (or buffered) when ffmpeg's
    // first request lands. Its response headers are what teach `total`.
    let Some(start) = next_chunk_offset(
        state.base,
        state.cursor,
        engine.config.chunk_bytes,
        engine.config.read_ahead_bytes,
        state.total,
        &state.chunks,
        &state.inflight,
        &state.dead,
        state.generation,
    ) else {
        // Window full (or the file ended): the worker keeps its last range but
        // stops counting as active.
        mark_worker_idle(&mut state, worker_id);
        return Claim::Wait;
    };
    let Some(host) = pick_host(&mut state, &[]) else {
        state.fatal = Some("没有可用的上游主机".to_string());
        mark_worker_idle(&mut state, worker_id);
        engine.publish(&mut state, Instant::now());
        return Claim::Stop;
    };
    let generation = state.generation;
    let end = state
        .total
        .map_or(start.saturating_add(engine.config.chunk_bytes), |total| {
            total.min(start.saturating_add(engine.config.chunk_bytes))
        });
    let label = state.hosts[host].label.clone();
    state.inflight.insert((generation, start));
    if let Some(slot) = state.workers.get_mut(worker_id) {
        slot.host = label.clone();
        slot.start = start;
        slot.end = end;
        slot.busy = true;
    }
    drop(state);
    crate::debug_log(&format!(
        "prefetch chunk claim after_ms={} gen={generation} worker={worker_id} start={start} end={end} host={label}",
        engine.started.elapsed().as_millis()
    ));
    Claim::Fetch {
        generation,
        start,
        end,
        host,
    }
}

/// Mark a worker as waiting for window space, keeping its last range visible.
fn mark_worker_idle(state: &mut State, worker_id: usize) {
    if let Some(slot) = state.workers.get_mut(worker_id) {
        slot.busy = false;
    }
}

async fn worker(worker_id: usize, engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match claim(&engine, &shutdown, worker_id) {
            Claim::Stop => break,
            Claim::Wait => {
                tokio::time::sleep(WAIT_POLL).await;
            }
            Claim::Fetch {
                generation,
                start,
                end,
                host,
            } => {
                let started = Instant::now();
                let Some(mut current) = engine.host_info(host) else {
                    continue;
                };
                let mut attempted: Vec<usize> = Vec::new();
                let mut cursor = start;
                let mut received_total = 0u64;
                loop {
                    let url = forward_url(&engine.target, &current.1);
                    match fetch_stream(
                        &engine,
                        &url,
                        generation,
                        cursor,
                        end,
                        current.0,
                        worker_id,
                        &shutdown,
                    )
                    .await
                    {
                        Ok(received) => {
                            cursor += received;
                            received_total += received;
                            engine.complete_range(
                                generation,
                                start,
                                cursor,
                                end,
                                &shutdown,
                            );
                            crate::debug_log(&format!(
                                "prefetch chunk done after_ms={} start={start} bytes={received_total} host={} ms={}",
                                engine.started.elapsed().as_millis(),
                                current.2,
                                started.elapsed().as_millis()
                            ));
                            break;
                        }
                        Err((received, eof, error)) => {
                            cursor += received;
                            received_total += received;
                            if eof {
                                // 416 or an empty body: the file ends here.
                                if received == 0 && cursor > 0 && !engine.total_known() {
                                    // A 416 above 0 only proves the file ends
                                    // at or before this offset; the real total
                                    // comes from the first 206 response.
                                    engine.mark_range_beyond_end(
                                        generation,
                                        start,
                                        &shutdown,
                                    );
                                } else {
                                    engine.complete_range(
                                        generation,
                                        start,
                                        cursor,
                                        end,
                                        &shutdown,
                                    );
                                }
                                break;
                            }
                            if shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            attempted.push(current.0);
                            engine.mark_host_failure(current.0, &error);
                            if shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            // The first host can reject the very first chunk
                            // (a `403`, a dead mirror). Fall through to the next
                            // healthy host for this same range, still inside
                            // the request that is waiting for it.
                            match engine.next_host(&attempted) {
                                Some(next) => {
                                    crate::debug_log(&format!(
                                        "prefetch range retry start={start} at={cursor} host={} err={error}",
                                        next.2
                                    ));
                                    current = next;
                                }
                                None => {
                                    engine.fail_range(
                                        generation,
                                        start,
                                        cursor,
                                        &attempted,
                                        &error,
                                        &shutdown,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if let Ok(mut state) = engine.state.lock() {
        mark_worker_idle(&mut state, worker_id);
    }
}

// ---------------------------------------------------------------------------
// Seek-point warming
// ---------------------------------------------------------------------------

/// Background task: once playback has pulled its first bytes, fill the point
/// cache with one bounded chunk per evenly spaced seek position.
///
/// It is deliberately separate from the window workers: it fetches at most one
/// point at a time, waits for the read-ahead window to be healthy first, and
/// never claims, marks dead or retires a window chunk. Dropping the handle
/// aborts it (it lives in the server's `JoinSet`).
async fn warm_loop(engine: Arc<Engine>, shutdown: Arc<AtomicBool>) {
    let count = engine.config.warm_points;
    if count == 0 {
        return;
    }
    // Wait for the first client bytes AND the total length. Neither is on the
    // critical path of the first request: the warm task only observes them.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let (served, total, fatal) = {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            (state.served, state.total, state.fatal.is_some())
        };
        if fatal {
            return;
        }
        if served > 0 && total.is_some() {
            break;
        }
        if engine.started.elapsed() > WARM_START_TIMEOUT {
            return;
        }
        tokio::time::sleep(WARM_POLL).await;
    }
    let Some(total) = engine.total() else {
        return;
    };
    // Short quiet period after the first served byte, so the reader's initial
    // burst (and `buffer_s`) settles before any extra fetch starts.
    tokio::time::sleep(WARM_DELAY).await;
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    let offsets = warm_point_offsets(total, count);
    if offsets.is_empty() {
        return;
    }
    let chunk = point_chunk_bytes(total, count, engine.config.point_bytes);
    if chunk == 0 {
        return;
    }
    engine.start_warm(offsets.len(), chunk, total);
    for offset in offsets {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Never refetch bytes the window (or an overlapping point) already
        // holds: those seeks are instant already.
        if engine.window_covers(offset) {
            engine.note_warm_covered(offset, "window");
            continue;
        }
        if engine.point_lookup(offset).is_some() {
            engine.note_warm_covered(offset, "point");
            continue;
        }
        if !await_playback_room(&engine, chunk, &shutdown).await {
            return;
        }
        let end = total.min(offset.saturating_add(chunk));
        match fetch_warm_point(&engine, offset, end, &shutdown).await {
            Some(data) => engine.store_point(offset, data, count, total),
            None => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                engine.note_warm_failed(offset);
            }
        }
        tokio::time::sleep(WARM_POINT_PAUSE).await;
    }
    engine.finish_warm();
}

/// Wait until warming one point cannot starve playback: the window holds at
/// least one point-chunk of unread bytes, the file ends at the cursor, or the
/// reader has not asked for anything for [`WARM_STARVED_MS`] (paused/closed).
/// Returns `false` only when the proxy shuts down.
async fn await_playback_room(engine: &Engine, chunk: u64, shutdown: &AtomicBool) -> bool {
    let started = Instant::now();
    let mut last_served = u64::MAX;
    let mut idle_since = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let (read_ahead, served, at_eof) = {
            let state = match engine.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                state.read_ahead,
                state.served,
                state.total == Some(state.cursor),
            )
        };
        if read_ahead >= chunk || at_eof {
            return true;
        }
        if served != last_served {
            last_served = served;
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= WARM_STARVED_MS {
            return true;
        }
        // Never wait forever behind a reader that stays slow: after 30 s the
        // point is abandoned rather than fetched into a window under pressure.
        if started.elapsed() > Duration::from_secs(30) {
            return true;
        }
        tokio::time::sleep(WARM_POLL).await;
    }
}

/// Fetch one seek point, rotating through healthy hosts on failure. Returns
/// `None` when every host refused or the proxy shut down. A failure here never
/// marks a host dead for playback: the point is optional.
async fn fetch_warm_point(
    engine: &Engine,
    start: u64,
    end: u64,
    shutdown: &AtomicBool,
) -> Option<Vec<u8>> {
    let started = Instant::now();
    let mut attempted: Vec<usize> = Vec::new();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        let (host, origin, label) = engine.next_host(&attempted)?;
        let url = forward_url(&engine.target, &origin);
        match warm_point_request(engine, &url, start, end, host, shutdown).await {
            Ok(data) => {
                crate::debug_log(&format!(
                    "seek warm point offset={start} bytes={} host={label} ms={}",
                    data.len(),
                    started.elapsed().as_millis()
                ));
                return Some(data);
            }
            Err(error) => {
                crate::debug_log(&format!(
                    "seek warm point failed offset={start} host={label} err={error}"
                ));
                attempted.push(host);
            }
        }
    }
}

/// One ranged GET whose whole (bounded) body is kept in memory. It never
/// touches the window: no `push_bytes`, no `inflight`/`dead` bookkeeping.
async fn warm_point_request(
    engine: &Engine,
    url: &str,
    start: u64,
    end: u64,
    host: usize,
    shutdown: &AtomicBool,
) -> std::result::Result<Vec<u8>, String> {
    let expected = end.saturating_sub(start);
    if expected == 0 {
        return Err("空区间".to_string());
    }
    let mut response = tokio::time::timeout(
        CHUNK_TIMEOUT,
        engine
            .client
            .get(url)
            .header(RANGE, format!("bytes={start}-{}", end - 1))
            .send(),
    )
    .await
    .map_err(|_| "请求超时".to_string())?
    .map_err(|error| error.to_string())?;
    let status = response.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let capacity = expected.min(DEFAULT_POINT_BYTES) as usize;
    let mut data: Vec<u8> = Vec::with_capacity(capacity);
    while (data.len() as u64) < expected {
        if shutdown.load(Ordering::SeqCst) {
            return Err("代理已关闭".to_string());
        }
        let chunk = match tokio::time::timeout(CHUNK_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("读取超时".to_string()),
        };
        let take = chunk.len().min((expected - data.len() as u64) as usize);
        if take == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..take]);
        engine.note_warm_bytes(host, take as u64, shutdown);
    }
    if data.is_empty() {
        return Err("空响应".to_string());
    }
    Ok(data)
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

/// Stream one range request straight into the shared buffer. Every network
/// read is stored at its absolute offset, so a reader can start before the
/// range completes. Returns the bytes stored, or the bytes stored plus whether
/// the upstream reported EOF (416) and the error text.
#[allow(clippy::type_complexity)]
async fn fetch_stream(
    engine: &Engine,
    url: &str,
    generation: u64,
    start: u64,
    end: u64,
    host: usize,
    worker: usize,
    shutdown: &AtomicBool,
) -> std::result::Result<u64, (u64, bool, String)> {
    let expected = end.saturating_sub(start);
    if expected == 0 {
        return Ok(0);
    }
    let response = tokio::time::timeout(
        CHUNK_TIMEOUT,
        engine
            .client
            .get(url)
            .header(RANGE, format!("bytes={start}-{}", end - 1))
            .send(),
    )
    .await
    .map_err(|_| (0u64, false, "请求超时".to_string()))?
    .map_err(|error| (0u64, false, error.to_string()))?;

    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err((0, true, "HTTP 416".to_string()));
    }
    // A server that ignores Range is only usable for offset 0; in that case
    // the first `expected` bytes of the body are the requested range.
    if status != StatusCode::PARTIAL_CONTENT && !(status.is_success() && start == 0) {
        return Err((0, false, format!("HTTP {}", status.as_u16())));
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
    while received < expected {
        if shutdown.load(Ordering::SeqCst) {
            return Err((received, false, "代理已关闭".to_string()));
        }
        let chunk = match tokio::time::timeout(CHUNK_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                return Err((received, false, error.to_string()));
            }
            Err(_) => return Err((received, false, "读取超时".to_string())),
        };
        let take = chunk.len().min((expected - received) as usize);
        if take == 0 {
            break;
        }
        // A seek bumps the generation: stop reading this response at once
        // instead of draining bytes that can no longer be served. The worker
        // then claims a range for the new window, so `WorkerStat` never shows
        // it busy with work that has been discarded.
        if !engine.push_bytes(generation, offset, &chunk[..take], host, worker, shutdown) {
            return Ok(received);
        }
        offset += take as u64;
        received += take as u64;
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
    for id in 0..engine.config.connections {
        workers.spawn(worker(id, engine.clone(), shutdown.clone()));
    }
    // The warm task shares the JoinSet, so the usual shutdown path (abort_all)
    // and dropping the handle both cancel it.
    if engine.config.warm_points > 0 {
        workers.spawn(warm_loop(engine.clone(), shutdown.clone()));
    }
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
    handlers.abort_all();
    workers.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while workers.join_next().await.is_some() {}
    })
    .await;
    drop(listener);
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
        crate::debug_log(&format!(
            "prefetch stats qa tick={tick} busy_ranges={}/{} overlaps={} ranges={} covered={} delta={delta:+} point_ranges={} point_bytes={} active={}",
            distinct.len(),
            busy.len(),
            range_overlaps(&stats.downloaded),
            stats.downloaded.len(),
            format_bytes(covered),
            stats.point_cache.len(),
            format_bytes(stats.point_cache_bytes),
            stats.active_workers
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
            if let Some(error) = &state.fatal {
                return Err(error.clone());
            }
            let buffered = start.is_some_and(|start| chunks_cover(&state.chunks, start));
            if metadata_ready(spec, state.total, buffered) {
                return Ok((state.total, state.content_type.clone()));
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
    // Sample this before any seek below: a reset clears the chunks, so the
    // question "did the client wait for upstream?" must be answered now.
    let buffered_at_request = requested_start.is_some_and(|start| engine.window_covers(start));

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
    // arrived, `upstream` = this response waited for a network read.
    let source = if buffered_at_request {
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

/// Write the body from `pos` out of the read-ahead window. The caller holds
/// the serve guard and the window has already been moved to `pos`.
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
    loop {
        if remaining == Some(0) {
            break;
        }
        if !engine.is_current(epoch) {
            return Ok(());
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
                        return Ok(());
                    }
                    return Err(error.to_string());
                }
                pos += written;
                remaining = remaining.map(|left| left - written);
            }
            Ok(None) => {
                if shutdown.load(Ordering::SeqCst) {
                    return Err("代理已关闭".to_string());
                }
                if !engine.is_current(epoch) {
                    return Ok(());
                }
                if engine.at_eof(pos) {
                    // The upstream ended before the promised length (only
                    // reachable when the total never came from a response).
                    // Closing here is what tells the client the body is short.
                    break;
                }
                tokio::time::sleep(WAIT_POLL).await;
            }
            Err(error) => return Err(error),
        }
    }
    if chunked {
        write_all(stream, b"0\r\n\r\n")
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// QA hook (temporary live verification; also usable from the player later)
// ---------------------------------------------------------------------------

const QA_PREFIX_BYTES: u64 = 8 << 20;
const QA_SEEK_BYTES: u64 = 2 << 20;

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

fn qa_mbps(bytes: u64, ms: u128) -> f64 {
    if ms == 0 {
        return 0.0;
    }
    bytes as f64 * 8.0 / (ms as f64 / 1000.0) / 1_000_000.0
}

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
#[allow(dead_code)]
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
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
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
    fn claim_publishes_the_worker_range_and_idles_it() {
        let mut engine = test_engine();
        // A tiny window so "full" is reachable without allocating megabytes:
        // 8 bytes of read-ahead, 4-byte chunks, 2 chunks.
        engine.config.read_ahead_bytes = 8;
        engine.config.chunk_bytes = 4;
        let shutdown = AtomicBool::new(false);

        // First claim: a real range from the configured host.
        let first = claim(&engine, &shutdown, 0);
        assert!(matches!(first, Claim::Fetch { start: 0, end: 4, .. }));
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
        engine.push_bytes(0, 0, &[1u8; 32], 0, 0, &shutdown);
        engine.push_bytes(0, 32, &[2u8; 8], 0, 0, &shutdown);
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
        }
        .normalized();
        assert_eq!(clamped.read_ahead_bytes, MIN_READ_AHEAD_BYTES);
        assert_eq!(clamped.chunk_bytes, MIN_CHUNK_BYTES);
        assert_eq!(clamped.connections, 1);
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

    /// Bare engine for the window/token/point tests: no listener, no workers,
    /// and a client that is never used.
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
            state: Mutex::new(State {
                base: 0,
                cursor: 0,
                generation: 0,
                chunks: BTreeMap::new(),
                inflight: BTreeSet::new(),
                dead: BTreeSet::new(),
                total: None,
                fatal: None,
                content_type: None,
                hosts: vec![Host {
                    label: "cdn.example.com".to_string(),
                    origin: "https://cdn.example.com".to_string(),
                    failed: false,
                    bytes: 0,
                }],
                next_host: 0,
                samples: VecDeque::new(),
                bytes_fetched: 0,
                failed_ranges: 0,
                served: 0,
                read_ahead: 0,
                points: PointCache::default(),
                warm_points_total: 0,
                warm_points_done: 0,
                warm_chunk: 0,
                warm_bytes: 0,
                point_hits: 0,
                point_served: 0,
                workers: vec![WorkerSlot::default()],
            }),
            stats: Arc::new(Mutex::new(PrefetchStats::default())),
            serving: AtomicBool::new(false),
            serve_epoch: AtomicU64::new(0),
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
    fn seek_recomputes_the_window_from_the_requested_offset() {
        let engine = test_engine();
        // The window starts aligned at 0, so the first request keeps it: the
        // chunks the workers already started are the ones it is about to read.
        engine.seek(0);
        assert_eq!(generation_of(&engine), 0);

        // The buffer already serves the next byte: a repeat request keeps it,
        // so a resumed reader does not throw its read-ahead away.
        if let Ok(mut state) = engine.state.lock() {
            state.chunks.insert(0, vec![7u8; 4]);
        }
        engine.seek(0);
        assert_eq!(generation_of(&engine), 0);

        // A forward seek, a backward seek and a different offset all move the
        // window: the workers must never fetch for a position the client left.
        engine.seek(2);
        assert_eq!(generation_of(&engine), 1);
        engine.seek(0);
        assert_eq!(generation_of(&engine), 2);

        // A cursor that only equals the requested offset because an abandoned
        // response consumed past it (the grid sits behind it, the byte is not
        // buffered) is not a served offset: the window is rebuilt there.
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(1000);
            state.base = 0;
            state.cursor = 10;
            state.chunks.clear();
        }
        engine.seek(10);
        assert_eq!(
            generation_of(&engine),
            3,
            "a consumed-past cursor must be recomputed, not trusted"
        );

        // At EOF there is nothing to fetch and nothing to resend: no reset.
        if let Ok(mut state) = engine.state.lock() {
            state.total = Some(10);
            state.cursor = 10;
            state.base = 10;
        }
        engine.seek(10);
        assert_eq!(generation_of(&engine), 3);
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

    /// Live end-to-end check against the real CDN. Ignored by default:
    /// `$env:BILI_DEBUG_LOG="prefetch-live.log"; cargo test --release -- --ignored --nocapture live_prefetch_proxy`
    /// Set `BILI_PREFETCH_BVID=<bvid>` to pin a video, or `BILI_PREFETCH_URL`
    /// to skip the playurl lookup entirely.
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
            return Some((url, Vec::new(), 0));
        }
        let session = match runtime.block_on(crate::bilibili::Session::new()) {
            Ok(session) => session,
            Err(error) => {
                crate::debug_log(&format!("prefetch live skipped reason=session err={error}"));
                return None;
            }
        };
        let pinned = std::env::var("BILI_PREFETCH_BVID")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let bvid = match pinned {
            Some(bvid) => bvid,
            None => {
                let popular = match runtime.block_on(session.popular(1, 30)) {
                    Ok(popular) => popular,
                    Err(error) => {
                        crate::debug_log(&format!(
                            "prefetch live skipped reason=popular err={error}"
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
                        crate::debug_log("prefetch live skipped reason=no-popular-item");
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
}

