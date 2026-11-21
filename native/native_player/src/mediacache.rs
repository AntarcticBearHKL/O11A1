//! Disk-backed media prefetch.
//!
//! Bilibili DASH media is served as ordinary ranged resources. This module
//! downloads a representation into a sparse local file with several parallel
//! range workers, then exposes it through a tiny loopback HTTP server so
//! ffmpeg can read the growing file like a normal remote stream. Reads block
//! only until the chunk they touch has arrived; playback can start at byte 0
//! while the rest of the file is still downloading.
//!
//! Completed files keep a small `.ok` marker and are reused by the next open
//! or by the playlist prefetch worker. Partial files are restarted because the
//! cache key already identifies the exact representation and a half-written
//! sparse file is not trusted.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_RANGE, RANGE,
};
use reqwest::StatusCode;

use crate::log;

/// Media chunks are small enough that the first frame only waits for a short
/// range on a slow link; workers still fill the rest of the file in parallel.
const CHUNK_BYTES: u64 = 512 * 1024;
/// Very small resources are not worth a cache file and a local server.
const MIN_CACHE_BYTES: u64 = 512 * 1024;
/// Sixteen HTTP/1.1 range workers: one connection each. Measured on the same
/// Bilibili 4K AV1 source, 16 concurrent 512 KB ranges reach ~11.8 MB/s versus
/// ~7.2 MB/s with eight; the CDN resets that plagued multiplexed HTTP/2 do not
/// apply because the client is HTTP/1.1 only. The user's
/// `prefetch_connections` setting can lower this, never raise it.
const MAX_WORKERS: usize = 16;
const READ_CHUNK: usize = 256 * 1024;
/// A reader waits at most this long for one cache chunk before it asks the
/// loopback server to fetch that range straight from the CDN. A worker stuck
/// on a throttled connection must never freeze the picture for its full
/// 45-second HTTP timeout.
const READY_TIMEOUT: Duration = Duration::from_secs(4);
/// Give a range several chances before declaring that one chunk unavailable.
/// Other workers keep filling the rest of the file; one bad range no longer
/// stops the whole download.
const CHUNK_ATTEMPTS: u32 = 6;
/// Workers prefer chunks within this many chunks ahead of the last read.
/// Far-away chunks would only grow the file behind the playhead while the
/// decoder waits for the next few hundred kilobytes.
const CURSOR_WINDOW: usize = 64;
/// The post-playhead window that is filled first. At 512 KB per chunk this is
/// 64 MB: about 85 seconds of 6 Mbit/s AV1 or 28 seconds of 18 Mbit/s AVC.
/// Sixteen workers keep the whole window saturated in playhead order instead
/// of following FFmpeg's much farther read-ahead cursor.
const PLAYHEAD_WINDOW: usize = 128;
/// Bump when the completed-chunk journal changes meaning. Old partial caches
/// have no version line and are restarted instead of being trusted.
const STATE_VERSION: &str = "v2";
/// Completed or interrupted media is kept this long after its last download
/// activity unless a player, worker, or reservation still holds it.
pub const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// The janitor checks once a minute; the TTL itself is far longer.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Give the first player enough time to pin its current source (and reserve
/// the next queue row) before the first sweep. Otherwise a warm cache opened
/// right after launch could be deleted before the player reaches it.
const INITIAL_SWEEP_DELAY: Duration = Duration::from_secs(30);
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Pending,
    Active,
    Done,
    /// Repeated range attempts failed. Readers fetch this range through the
    /// CDN proxy; the rest of the cache stays usable and keeps downloading.
    Failed,
}

struct MediaState {
    chunks: Vec<ChunkState>,
    failed: bool,
    done: bool,
    error: Option<String>,
    /// A reader asked for this chunk; workers pick it before the next
    /// sequential chunk so seeking feels immediate.
    priority: Option<usize>,
    /// Last chunk a reader needed. Workers fill forward from here rather than
    /// from byte zero, so a seek starts downloading where the picture is.
    cursor: usize,
    /// Chunk under the presentation clock. Workers fill the post-playhead
    /// window in order before looking at the reader's farther-ahead cursor.
    playhead: usize,
}

pub struct CachedMedia {
    pub id: String,
    pub total: u64,
    chunk: u64,
    file: File,
    /// Range workers still attached to this media. The janitor never removes a
    /// file while a worker can still write it.
    workers: std::sync::atomic::AtomicUsize,
    state: Mutex<MediaState>,
    ready: Condvar,
    stop: AtomicBool,
    /// Append-only completion journal. Rewriting the whole `11k`-line list
    /// after every 512 KB chunk was O(n²) disk work and serialized readers
    /// behind the state lock; appending one index is constant work.
    journal: Mutex<Option<File>>,
    /// The original CDN resource. If a cache chunk cannot be fetched (a CDN
    /// reset, a throttled range, a stale signed URL after a long pause), the
    /// loopback server proxies the missing range directly instead of killing
    /// the whole video.
    url: String,
    headers: HeaderMap,
    client: Client,
}

static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<CachedMedia>>>> = OnceLock::new();
static PORT: OnceLock<u16> = OnceLock::new();
static PORT_LOCK: Mutex<()> = Mutex::new(());
static PREPARE_LOCK: Mutex<()> = Mutex::new(());
/// Bytes pulled from the CDN by every cache worker in this process. The
/// player HUD samples this instead of the decoder process's loopback reads.
static NETWORK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Cache ids currently held by a player window or a prefetch reservation.
/// Keyed by the hashed media id that also names the files on disk.
static PINS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

fn pins() -> &'static Mutex<HashMap<String, usize>> {
    PINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keeps one cache representation alive while a player session or reservation
/// exists. Dropping it makes the cache eligible for the 15-minute sweep.
pub struct CachePin {
    id: String,
}

impl Drop for CachePin {
    fn drop(&mut self) {
        let Ok(mut pins) = pins().lock() else {
            return;
        };
        if let Some(count) = pins.get_mut(&self.id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                pins.remove(&self.id);
            }
        }
    }
}

/// Pin a cache key (the representation identifier, not the on-disk hash).
pub fn pin_key(key: &str) -> CachePin {
    let id = media_id(key);
    if let Ok(mut pins) = pins().lock() {
        *pins.entry(id.clone()).or_insert(0) += 1;
    }
    CachePin { id }
}

/// Pin the video or audio representation of one resolved source.
pub fn pin_source(source: &crate::source::MediaSource, audio: bool) -> Option<CachePin> {
    crate::source::media_cache_key(source, audio).map(|key| pin_key(&key))
}

fn is_pinned(id: &str) -> bool {
    pins()
        .lock()
        .map(|pins| pins.get(id).copied().unwrap_or(0) > 0)
        .unwrap_or(true)
}

/// Start the background TTL sweeper once for this process. It removes caches
/// whose last download activity is older than [`CACHE_TTL`] and that no player,
/// worker, or reservation is using.
pub fn start_janitor() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("media-cache-janitor".into())
        .spawn(|| {
            std::thread::sleep(INITIAL_SWEEP_DELAY);
            loop {
                let removed = sweep_expired(SystemTime::now(), CACHE_TTL);
                if removed > 0 {
                    log::line(&format!("media cache sweep removed={removed}"));
                }
                std::thread::sleep(SWEEP_INTERVAL);
            }
        });
}

/// Run one sweep immediately (QA and manual diagnostics).
pub fn sweep_now() -> usize {
    sweep_expired(SystemTime::now(), CACHE_TTL)
}

/// Remove expired, unused cache entries. Returns the number of media entries
/// deleted. Exposed for tests and for the janitor.
fn sweep_expired(now: SystemTime, ttl: Duration) -> usize {
    let Ok(dir) = cache_dir() else {
        return 0;
    };
    sweep_directory(&dir, now, ttl, &|id| {
        if is_pinned(id) {
            return false;
        }
        drop_registry_if_idle(id)
    })
}

/// Drop this process's registry handle when nobody else holds the media.
/// Returns true when the id is free and its file may be unlinked.
fn drop_registry_if_idle(id: &str) -> bool {
    let Ok(mut map) = registry().lock() else {
        return false;
    };
    let Some(media) = map.get(id) else {
        return true;
    };
    if media.workers.load(Ordering::SeqCst) > 0 || Arc::strong_count(media) > 1 {
        return false;
    }
    map.remove(id);
    true
}

fn newest_mtime(paths: &[&std::path::Path]) -> Option<SystemTime> {
    paths
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
        .max()
}

fn expired(now: SystemTime, last: SystemTime, ttl: Duration) -> bool {
    now.duration_since(last)
        .map(|age| age >= ttl)
        .unwrap_or(false)
}

/// True when another process still has the file open. Asking for zero sharing
/// fails against any live reader/writer, including an older O11A1 process that
/// does not know about this process's in-memory pins.
#[cfg(windows)]
fn file_is_open_elsewhere(path: &std::path::Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => {
            drop(file);
            false
        }
        // Sharing violation is the expected "in use" answer. Treat any error
        // as busy: leaking one cache is safer than unlinking live media.
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn file_is_open_elsewhere(_path: &std::path::Path) -> bool {
    false
}

/// Pure directory sweep used by [`sweep_expired`] and the tests. `release`
/// returns false when the id is in use and true once any registry handle has
/// been dropped.
fn sweep_directory(
    dir: &std::path::Path,
    now: SystemTime,
    ttl: Duration,
    release: &dyn Fn(&str) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut media: Vec<(String, PathBuf)> = Vec::new();
    let mut sidecars: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("media") => media.push((stem.to_string(), path)),
            Some("ok") | Some("state") => sidecars.push((stem.to_string(), path)),
            _ => {}
        }
    }

    let mut removed = 0usize;
    for (id, path) in media {
        let ok = dir.join(format!("{id}.ok"));
        let state = dir.join(format!("{id}.state"));
        let Some(last) = newest_mtime(&[&path, &ok, &state]) else {
            continue;
        };
        if !expired(now, last, ttl) || !release(&id) {
            continue;
        }
        if file_is_open_elsewhere(&path) {
            log::line(&format!("media cache sweep busy id={id}"));
            continue;
        }
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if std::fs::remove_file(&path).is_ok() {
            let _ = std::fs::remove_file(&ok);
            let _ = std::fs::remove_file(&state);
            removed += 1;
            log::line(&format!(
                "media cache sweep removed id={id} bytes={bytes} age_s={}",
                now.duration_since(last).map(|age| age.as_secs()).unwrap_or(0)
            ));
        }
    }

    // A crash between writing `.ok` and creating `.media` (or a manual delete)
    // can leave a sidecar; remove it after the same grace period.
    for (id, path) in sidecars {
        if dir.join(format!("{id}.media")).exists() {
            continue;
        }
        let Some(last) = newest_mtime(&[&path]) else {
            continue;
        };
        if !expired(now, last, ttl) || !release(&id) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            log::line(&format!("media cache sweep removed orphan id={id}"));
        }
    }
    removed
}

fn registry() -> &'static Mutex<HashMap<String, Arc<CachedMedia>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stable, filename-safe id for a cache key. The key itself can contain the
/// BVID, cid and quality; hashing keeps paths short and Windows-safe.
fn media_id(key: &str) -> String {
    // FNV-1a: deterministic across runs, unlike `DefaultHasher`.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn cache_dir() -> Result<PathBuf> {
    let dir = o11a1::paths::root().join("cache").join("media");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create media cache dir {}", dir.display()))?;
    Ok(dir)
}

fn path_for(dir: &std::path::Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.media"))
}

fn marker_for(dir: &std::path::Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.ok"))
}

fn state_for(dir: &std::path::Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.state"))
}

/// Open the append-only journal. `fresh` truncates an old file and writes the
/// version header; otherwise the existing header is kept and new indices are
/// appended. One short write per finished chunk replaces rewriting the whole
/// list, and a crash loses at most the chunks still in flight.
fn open_journal(dir: &std::path::Path, id: &str, total: u64, fresh: bool) -> Option<File> {
    let path = state_for(dir, id);
    let opened = if fresh {
        File::create(&path)
    } else {
        OpenOptions::new().append(true).open(&path)
    };
    let mut file = match opened {
        Ok(file) => file,
        Err(error) => {
            log::line(&format!(
                "media cache journal open failed id={id} path={} err={error}",
                path.display()
            ));
            return None;
        }
    };
    if file.metadata().map(|meta| meta.len()).unwrap_or(0) == 0 {
        let header = format!("{STATE_VERSION}\n{total}\n{CHUNK_BYTES}\n");
        if let Err(error) = file.write_all(header.as_bytes()) {
            log::line(&format!(
                "media cache journal header failed id={id} err={error}"
            ));
            return None;
        }
    }
    log::line(&format!(
        "media cache journal ready id={id} fresh={fresh} path={} bytes={}",
        path.display(),
        file.metadata().map(|meta| meta.len()).unwrap_or(0)
    ));
    Some(file)
}

fn read_resume_state(path: &std::path::Path, total_hint: Option<u64>) -> Option<(u64, Vec<usize>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != STATE_VERSION {
        return None;
    }
    let total = lines.next()?.trim().parse::<u64>().ok()?;
    if total_hint.is_some_and(|hint| hint != total) {
        return None;
    }
    if lines.next()?.trim().parse::<u64>().ok()? != CHUNK_BYTES {
        return None;
    }
    let count = chunks_for(total);
    let mut done: Vec<usize> = lines
        .filter_map(|line| line.trim().parse::<usize>().ok())
        .filter(|index| *index < count)
        .collect();
    // A journal can contain the same index more than once if a process died
    // after marking a chunk done but before its state was compacted. Dedupe
    // so `resumed_chunks` and the progress math stay truthful.
    done.sort_unstable();
    done.dedup();
    Some((total, done))
}

/// Return a loopback URL that ffmpeg can read, starting the parallel download
/// if needed. `None` means the URL did not expose a usable byte size/range
/// response; the caller should keep the original URL.
pub fn cached_url(url: &str, headers: &[(String, String)], key: &str) -> Option<String> {
    cached_url_from(url, headers, key, 0)
}

/// Like [`cached_url`], but starts the workers at the byte offset the player
/// will first read. A seek to minute 10 must not spend its first 16 requests
/// on byte zero while the picture waits for the new position.
pub fn cached_url_from(
    url: &str,
    headers: &[(String, String)],
    key: &str,
    start_offset: u64,
) -> Option<String> {
    if url.trim().is_empty() || key.trim().is_empty() {
        return None;
    }
    let media = prepare(url, headers, key, start_offset)?;
    // A complete file is read directly. The loopback server exists so ffmpeg
    // can block on chunks that are still downloading; a finished 4K file has
    // no such chunks, and serving it over TCP added a full extra copy of
    // every 12.4 MB picture plus the HTTP stack. Direct file reads keep warm
    // 4K playback off that path entirely.
    if media.is_complete() {
        let dir = cache_dir().ok()?;
        return Some(
            path_for(&dir, &media.id)
                .to_string_lossy()
                .to_string(),
        );
    }
    let port = server_port()?;
    Some(format!("http://127.0.0.1:{port}/media/{}", media.id))
}

/// Total bytes fetched from media CDNs by cache workers in this process.
pub fn network_bytes() -> u64 {
    NETWORK_BYTES.load(Ordering::Relaxed)
}

/// Completed/total bytes for a cache key in this process, if it exists.
pub fn progress(key: &str) -> Option<(u64, u64)> {
    let id = media_id(key);
    let media = registry().lock().ok().and_then(|map| map.get(&id).cloned())?;
    let state = media.state.lock().ok()?;
    let mut done = 0u64;
    for (index, chunk) in state.chunks.iter().enumerate() {
        if *chunk == ChunkState::Done {
            let start = index as u64 * media.chunk;
            done = done.saturating_add((media.total - start).min(media.chunk));
        }
    }
    Some((done, media.total))
}

/// Contiguous bytes ready from `offset` forward. A hole stops the count: data
/// beyond a missing chunk cannot be played without a seek, even if it is on
/// disk. The engine uses this to wait for a real cache lead instead of
/// resuming on the first eight decoded frames and stalling again immediately.
pub fn lead_bytes(key: &str, offset: u64) -> Option<u64> {
    let id = media_id(key);
    let media = registry().lock().ok().and_then(|map| map.get(&id).cloned())?;
    let state = media.state.lock().ok()?;
    let start = (offset / media.chunk) as usize;
    if state.chunks.get(start) == Some(&ChunkState::Failed) {
        // The reader will proxy this range; there is no local lead to wait
        // for, but playback can still continue at network speed.
        return None;
    }
    Some(contiguous_lead(
        &state.chunks,
        media.chunk,
        media.total,
        offset,
    ))
}

fn contiguous_lead(chunks: &[ChunkState], chunk: u64, total: u64, offset: u64) -> u64 {
    if offset >= total {
        return 0;
    }
    let start = (offset / chunk) as usize;
    let mut bytes = 0u64;
    for (index, state) in chunks.iter().enumerate().skip(start) {
        if *state != ChunkState::Done {
            break;
        }
        let chunk_start = index as u64 * chunk;
        let chunk_end = (chunk_start + chunk).min(total);
        let from = if index == start {
            offset.max(chunk_start)
        } else {
            chunk_start
        };
        bytes = bytes.saturating_add(chunk_end.saturating_sub(from));
    }
    bytes
}

/// Completed/total bytes for a cache key in this process, if it exists.
pub fn is_complete_key(key: &str) -> bool {
    let id = media_id(key);
    if let Some(media) = registry().lock().ok().and_then(|map| map.get(&id).cloned()) {
        return media.is_complete();
    }
    let Ok(dir) = cache_dir() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(marker_for(&dir, &id)) else {
        return false;
    };
    let Ok(total) = text.trim().parse::<u64>() else {
        return false;
    };
    total >= MIN_CACHE_BYTES && file_len(&path_for(&dir, &id)) == Some(total)
}

/// Update the presentation-clock anchor used for cache scheduling. The engine
/// calls this once per second; it is deliberately separate from the reader's
/// cursor, which FFmpeg can move tens of megabytes ahead of the picture.
pub fn set_playhead(key: &str, offset: u64) {
    let id = media_id(key);
    let Some(media) = registry().lock().ok().and_then(|map| map.get(&id).cloned()) else {
        return;
    };
    let Ok(mut state) = media.state.lock() else {
        return;
    };
    let index = ((offset / media.chunk) as usize).min(state.chunks.len());
    if state.playhead != index {
        state.playhead = index;
        media.ready.notify_all();
    }
}

/// Ask the cache workers to fetch the chunk at `offset` next. The reader's
/// cursor can be tens of megabytes ahead (FFmpeg's demuxer buffer), which
/// leaves a hole at the picture the user is actually watching. Prioritising
/// that hole is what turns a 0-second lead into a filled chunk instead of
/// waiting for the cursor window to come back around.
pub fn prioritize(key: &str, offset: u64) {
    let id = media_id(key);
    let Some(media) = registry().lock().ok().and_then(|map| map.get(&id).cloned()) else {
        return;
    };
    let Ok(mut state) = media.state.lock() else {
        return;
    };
    let index = (offset / media.chunk) as usize;
    if index < state.chunks.len()
        && matches!(state.chunks[index], ChunkState::Pending | ChunkState::Active)
    {
        state.priority = Some(index);
        state.cursor = index;
        media.ready.notify_all();
    }
}

/// Wait until a cached key is complete (or failed). Used by the playlist
/// prefetch worker so it does not steal bandwidth from the active video.
pub fn wait_for_key(key: &str) {
    let id = media_id(key);
    let media = registry().lock().ok().and_then(|map| map.get(&id).cloned());
    if let Some(media) = media {
        let _ = media.wait_complete();
    }
}

/// Start the cache workers for both representations and return immediately.
/// Used by the next-part prefetcher ten seconds before the current part ends:
/// the opening chunks are then already on disk when the page switch happens.
pub fn warm_source(source: &crate::source::MediaSource) -> bool {
    let mut ok = true;
    if let Some(key) = crate::source::media_cache_key(source, false) {
        ok &= prepare_workers(
            &source.video_url,
            &source.headers,
            &key,
            0,
            Some(8),
        )
        .is_some();
    }
    if let Some(url) = source.audio_url.as_deref() {
        if let Some(key) = crate::source::media_cache_key(source, true) {
            ok &= prepare_workers(url, &source.headers, &key, 0, Some(4)).is_some();
        }
    }
    ok
}

/// Download both representations of a resolved source completely. The
/// playlist prefetcher calls this one entry at a time so cache disk usage and
/// bandwidth are predictable.
pub fn prefetch_source(source: &crate::source::MediaSource) -> bool {
    let mut ok = true;
    if let Some(key) = crate::source::media_cache_key(source, false) {
        if let Some(media) = prepare(&source.video_url, &source.headers, &key, 0) {
            ok &= media.wait_complete().is_ok();
        }
    }
    if let Some(url) = source.audio_url.as_deref() {
        if let Some(key) = crate::source::media_cache_key(source, true) {
            if let Some(media) = prepare(url, &source.headers, &key, 0) {
                ok &= media.wait_complete().is_ok();
            }
        }
    }
    ok
}

fn prepare(
    url: &str,
    headers: &[(String, String)],
    key: &str,
    start_offset: u64,
) -> Option<Arc<CachedMedia>> {
    prepare_workers(url, headers, key, start_offset, None)
}

fn prepare_workers(
    url: &str,
    headers: &[(String, String)],
    key: &str,
    start_offset: u64,
    workers_override: Option<usize>,
) -> Option<Arc<CachedMedia>> {
    let id = media_id(key);
    if let Ok(map) = registry().lock() {
        if let Some(existing) = map.get(&id) {
            if !existing.failed() {
                return Some(existing.clone());
            }
        }
    }
    // Two players (or the playlist prefetcher and a player) can ask for the
    // same key at once. Only one must create/truncate the sparse file.
    let _prepare_guard = PREPARE_LOCK.lock().ok()?;
    if let Ok(map) = registry().lock() {
        if let Some(existing) = map.get(&id) {
            if !existing.failed() {
                return Some(existing.clone());
            }
        }
    }
    let dir = cache_dir().ok()?;
    let path = path_for(&dir, &id);
    let marker = marker_for(&dir, &id);
    let state_path = state_for(&dir, &id);
    // HTTP/1.1 gives every range worker its own connection. Bilibili's CDN
    // resets multiplexed HTTP/2 range bodies under eight-way concurrency
    // ("end of file before message length reached"), which used to abort a
    // whole 4K video. One connection per worker is what download managers do.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // One 512 KB range. A stalled CDN connection has to give up quickly:
        // 300 seconds let a single wedged worker hide the whole read-ahead.
        .timeout(Duration::from_secs(45))
        .http1_only()
        .build()
        .ok()?;
    let header_map = header_map(headers);

    // A completed file survives app restarts; no network request is needed.
    if let Ok(text) = std::fs::read_to_string(&marker) {
        if let Ok(total) = text.trim().parse::<u64>() {
            if file_len(&path) == Some(total) && total >= MIN_CACHE_BYTES {
                let file = File::open(&path).ok()?;
                let media = Arc::new(CachedMedia {
                    id: id.clone(),
                    total,
                    chunk: CHUNK_BYTES,
                    file,
                    workers: std::sync::atomic::AtomicUsize::new(0),
                    state: Mutex::new(MediaState {
                        chunks: vec![ChunkState::Done; chunks_for(total)],
                        failed: false,
                        done: true,
                        error: None,
                        priority: None,
                        cursor: 0,
                        playhead: 0,
                    }),
                    ready: Condvar::new(),
                    stop: AtomicBool::new(false),
                    journal: Mutex::new(None),
                    url: url.to_string(),
                    headers: header_map.clone(),
                    client: client.clone(),
                });
                if let Ok(mut map) = registry().lock() {
                    map.insert(id, media.clone());
                }
                return Some(media);
            }
        }
    }

    // Resume an interrupted parallel download. The `.state` journal is a
    // version line, the total, the chunk size, then one completed index per
    // line. A chunk is only appended after its whole range reached the
    // sparse file, so a crash can lose at most the chunks that were in flight.
    let mut resume: Option<(u64, Vec<usize>)> = None;
    if let Ok(text) = std::fs::read_to_string(&state_path) {
        if let Some(header_total) = text.lines().nth(1).and_then(|line| line.trim().parse::<u64>().ok()) {
            if file_len(&path) == Some(header_total) && header_total >= MIN_CACHE_BYTES {
                resume = read_resume_state(&state_path, Some(header_total));
            }
        }
    }

    let (total, done, file, journal) = if let Some((total, done)) = resume {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .ok()?;
        let journal = open_journal(&dir, &id, total, false)?;
        (total, done, file, journal)
    } else {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&state_path);
        let total = probe_total(&client, url, &header_map)?;
        if total < MIN_CACHE_BYTES {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        if file.set_len(total).is_err() {
            return None;
        }
        let journal = open_journal(&dir, &id, total, true)?;
        (total, Vec::new(), file, journal)
    };
    let chunks = chunks_for(total);
    let initial_chunk =
        ((start_offset / CHUNK_BYTES) as usize).min(chunks.saturating_sub(1));
    let mut chunk_states = vec![ChunkState::Pending; chunks];
    let resumed_chunks = done.len();
    for index in done {
        if let Some(chunk) = chunk_states.get_mut(index) {
            *chunk = ChunkState::Done;
        }
    }
    if resumed_chunks > 0 {
        log::line(&format!(
            "media cache resume id={} bytes={} done_chunks={} total_chunks={}",
            id, total, resumed_chunks, chunks
        ));
    }
    let complete = chunk_states.iter().all(|chunk| *chunk == ChunkState::Done);
    if complete {
        let _ = std::fs::write(&marker, total.to_string());
        let _ = std::fs::remove_file(&state_path);
    }
    let media = Arc::new(CachedMedia {
        id: id.clone(),
        total,
        chunk: CHUNK_BYTES,
        file,
        workers: std::sync::atomic::AtomicUsize::new(0),
        state: Mutex::new(MediaState {
            chunks: chunk_states,
            failed: false,
            done: complete,
            error: None,
            priority: None,
            cursor: initial_chunk,
            playhead: initial_chunk,
        }),
        ready: Condvar::new(),
        stop: AtomicBool::new(false),
        journal: Mutex::new(Some(journal)),
        url: url.to_string(),
        headers: header_map.clone(),
        client: client.clone(),
    });
    if let Ok(mut map) = registry().lock() {
        if let Some(existing) = map.get(&id) {
            if !existing.failed() {
                return Some(existing.clone());
            }
        }
        map.insert(id.clone(), media.clone());
    }

    let configured = o11a1::settings::load()
        .prefetch_connections
        .clamp(1, MAX_WORKERS as u32) as usize;
    let workers = workers_override
        .unwrap_or(configured)
        .clamp(1, MAX_WORKERS)
        .min(chunks)
        .max(1);
    log::line(&format!(
        "media cache start id={id} bytes={total} chunks={chunks} workers={workers} resumed={resumed_chunks}"
    ));
    for _ in 0..workers {
        let worker_media = media.clone();
        let url = url.to_string();
        let headers = header_map.clone();
        let client = client.clone();
        media.workers.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("media-cache-worker".into())
            .spawn(move || download_worker(worker_media, client, url, headers));
        if spawned.is_err() {
            media.workers.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Some(media)
}

fn probe_total(client: &Client, url: &str, headers: &HeaderMap) -> Option<u64> {
    let response = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .ok()?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        // No Range support (or a server that ignored the header). Let ffmpeg
        // read the original URL rather than pretending we can cache it.
        return None;
    }
    response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_total)
}

fn header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        map.insert(name, value);
    }
    if !map.contains_key("user-agent") {
        if let Ok(value) = HeaderValue::from_str(DEFAULT_UA) {
            map.insert("user-agent", value);
        }
    }
    // Range offsets are useless if the response is compressed: the bytes on
    // the wire would no longer match the media file's offsets.
    if !map.contains_key(ACCEPT_ENCODING) {
        map.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    map
}

fn chunks_for(total: u64) -> usize {
    total.div_ceil(CHUNK_BYTES).max(1) as usize
}

fn file_len(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn content_range_total(text: &str) -> Option<u64> {
    text.rsplit('/').next()?.trim().parse().ok()
}

fn download_worker(media: Arc<CachedMedia>, client: Client, url: String, headers: HeaderMap) {
    struct WorkerGuard(Arc<CachedMedia>);
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            self.0.workers.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = WorkerGuard(media.clone());
    // Cache prefetch is background work. Below-normal priority keeps the
    // decoder, the pipe reader and the engine responsive while a 4K file is
    // filling in; the download still uses every idle cycle.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let mut logged_first = false;
    loop {
        if media.stop.load(Ordering::SeqCst) {
            break;
        }
        let index = {
            let Ok(mut state) = media.state.lock() else {
                break;
            };
            loop {
                if state.failed || state.done {
                    break None;
                }
                let candidate = state
                    .priority
                    .take()
                    .filter(|index| {
                        *index < state.chunks.len()
                            && state.chunks[*index] == ChunkState::Pending
                    })
                    .or_else(|| {
                        // Fill the post-playhead window first: this is the
                        // data the user is about to watch, regardless of how
                        // far FFmpeg's demuxer has read ahead.
                        let start = state.playhead.min(state.chunks.len());
                        let end = (start + PLAYHEAD_WINDOW).min(state.chunks.len());
                        (start..end).find(|index| state.chunks[*index] == ChunkState::Pending)
                    })
                    .or_else(|| {
                        // Fill forward from the last byte a reader asked for.
                        let start = state.cursor.min(state.chunks.len());
                        let end = (start + CURSOR_WINDOW).min(state.chunks.len());
                        (start..end).find(|index| state.chunks[*index] == ChunkState::Pending)
                    })
                    .or_else(|| {
                        // Backfill before the playhead only after the useful
                        // post-playhead windows are fully claimed.
                        let start = state.playhead.min(state.chunks.len());
                        (0..start).find(|index| state.chunks[*index] == ChunkState::Pending)
                    })
                    .or_else(|| {
                        // The window around the cursor is already complete
                        // (a long resume, or the reader is far behind the
                        // download front). Keep the remaining workers busy
                        // instead of parking the whole pool.
                        state
                            .chunks
                            .iter()
                            .position(|chunk| *chunk == ChunkState::Pending)
                    });
                match candidate {
                    Some(index) => {
                        state.chunks[index] = ChunkState::Active;
                        break Some(index);
                    }
                    None => {
                        // Nothing pending right now. If another worker is still
                        // fetching, wait for it instead of retiring this worker
                        // and leaving the pool empty for the next chunk.
                        let all_settled = state
                            .chunks
                            .iter()
                            .all(|chunk| matches!(chunk, ChunkState::Done | ChunkState::Failed));
                        if all_settled {
                            break None;
                        }
                        state = match media
                            .ready
                            .wait_timeout(state, Duration::from_millis(250))
                        {
                            Ok((next, _)) => next,
                            Err(_) => break None,
                        };
                    }
                }
            }
        };
        let Some(index) = index else {
            break;
        };
        if !logged_first {
            logged_first = true;
            log::line(&format!(
                "media cache worker first id={} chunk={index} playhead={} cursor={}",
                media.id,
                media.state.lock().map(|state| state.playhead).unwrap_or(0),
                media.state.lock().map(|state| state.cursor).unwrap_or(0)
            ));
        }
        let start = index as u64 * media.chunk;
        let end = ((index as u64 + 1) * media.chunk - 1).min(media.total - 1);
        let mut last_error = String::new();
        let mut ok = false;
        for attempt in 0..CHUNK_ATTEMPTS {
            match download_range(&media, &client, &url, &headers, start, end) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(error) => {
                    last_error = format!("{error:#}");
                    log::line(&format!(
                        "media cache range failed id={} chunk={index} attempt={attempt} err={last_error}",
                        media.id
                    ));
                    let backoff = (250 * (attempt as u64 + 1)).min(1_500);
                    std::thread::sleep(Duration::from_millis(backoff));
                }
            }
        }
        let complete = {
            let Ok(mut state) = media.state.lock() else {
                break;
            };
            if ok {
                state.chunks[index] = ChunkState::Done;
                let complete = state.chunks.iter().all(|chunk| *chunk == ChunkState::Done);
                if complete {
                    state.done = true;
                }
                complete
            } else {
                // One un-fetchable range must not stop the other workers or
                // fail the whole media. Readers proxy this chunk from the CDN
                // while the rest stays on the fast local path.
                state.chunks[index] = ChunkState::Failed;
                log::line(&format!(
                    "media cache chunk unavailable id={} chunk={index} err={last_error}",
                    media.id
                ));
                false
            }
        };
        if ok {
            media.append_done(index);
        }
        media.ready.notify_all();
        if complete {
            // Drop the journal before removing it: Windows refuses to unlink
            // a file that still has an open write handle.
            if let Ok(mut journal) = media.journal.lock() {
                *journal = None;
            }
            if let Ok(dir) = cache_dir() {
                let _ = std::fs::write(marker_for(&dir, &media.id), media.total.to_string());
                let _ = std::fs::remove_file(state_for(&dir, &media.id));
            }
            log::line(&format!(
                "media cache complete id={} bytes={} chunks={}",
                media.id,
                media.total,
                chunks_for(media.total)
            ));
            break;
        }
        if media.failed() {
            break;
        }
    }
}

fn download_range(
    media: &CachedMedia,
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    start: u64,
    end: u64,
) -> Result<()> {
    let mut response: Response = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .with_context(|| format!("range {start}-{end} {url}"))?;
    let status = response.status();
    if status != StatusCode::PARTIAL_CONTENT {
        // A 200 here means the CDN ignored Range and is sending the file from
        // byte zero. Writing that body at `start` shifts every byte and is a
        // direct source of "Invalid data" packets in the hardware decoder.
        // Accept it only for a request that really asked for the whole file.
        if status == StatusCode::OK && start == 0 && end + 1 == media.total {
            // Full-file body, offsets are correct.
        } else {
            bail!("http status {status} for range {start}-{end}");
        }
    }
    if let Some(value) = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
    {
        let expected = format!("bytes {start}-");
        if !value.trim().starts_with(&expected) {
            bail!("content-range mismatch: got {value:?} wanted {expected}...");
        }
    }
    let mut offset = start;
    let mut remaining = end - start + 1;
    let mut buffer = vec![0u8; READ_CHUNK];
    while remaining > 0 {
        if media.stop.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let want = (remaining as usize).min(buffer.len());
        let read = response.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        NETWORK_BYTES.fetch_add(read as u64, Ordering::Relaxed);
        let mut written = 0;
        while written < read {
            let count = media
                .file
                .seek_write(&buffer[written..read], offset)
                .context("write media cache")?;
            if count == 0 {
                bail!("short write");
            }
            written += count;
            offset += count as u64;
        }
        remaining -= read as u64;
    }
    if remaining != 0 {
        bail!(
            "short range body: got {} expected {}",
            end - start + 1 - remaining,
            end - start + 1
        );
    }
    Ok(())
}

impl CachedMedia {
    fn failed(&self) -> bool {
        self.state.lock().map(|state| state.failed).unwrap_or(true)
    }

    fn is_complete(&self) -> bool {
        self.state.lock().map(|state| state.done).unwrap_or(false)
    }

    /// Record one finished chunk. The journal is append-only and is not part
    /// of the state lock, so a disk hiccup here cannot freeze readers.
    fn append_done(&self, index: usize) {
        if let Ok(mut journal) = self.journal.lock() {
            if let Some(file) = journal.as_mut() {
                let _ = writeln!(file, "{index}");
            }
        }
    }

    pub fn wait_complete(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("media cache lock poisoned"))?;
        loop {
            if state.failed {
                return Err(anyhow!(
                    "media cache failed: {}",
                    state.error.clone().unwrap_or_default()
                ));
            }
            if state.done {
                return Ok(());
            }
            // A range already marked unavailable is proxied during playback,
            // but a background prefetch must not wait on it forever.
            if !state
                .chunks
                .iter()
                .any(|chunk| matches!(chunk, ChunkState::Pending | ChunkState::Active))
            {
                return Err(anyhow!(
                    "media cache incomplete: {} chunks unavailable",
                    state
                        .chunks
                        .iter()
                        .filter(|chunk| **chunk == ChunkState::Failed)
                        .count()
                ));
            }
            let (next, _) = self
                .ready
                .wait_timeout(state, Duration::from_millis(250))
                .map_err(|_| anyhow!("media cache wait poisoned"))?;
            state = next;
        }
    }

    /// `Ok(true)` means the chunk is on disk. `Ok(false)` means the reader
    /// should proxy this range: the chunk failed or a worker is taking too
    /// long. Waiting forever behind one slow HTTP connection was what turned a
    /// transient CDN hiccup into a multi-second picture freeze.
    fn wait_ready(&self, offset: u64, timeout: Duration) -> Result<bool> {
        if offset >= self.total {
            return Ok(true);
        }
        let index = (offset / self.chunk) as usize;
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("media cache lock poisoned"))?;
        if index < state.chunks.len()
            && matches!(state.chunks[index], ChunkState::Pending | ChunkState::Active)
        {
            state.priority = Some(index);
            state.cursor = index;
            // Wake a parked worker now so a seek does not wait for its
            // 250 ms timeout before the new playhead is fetched.
            self.ready.notify_all();
        }
        loop {
            if state.failed {
                return Err(anyhow!(
                    "media cache failed: {}",
                    state.error.clone().unwrap_or_default()
                ));
            }
            match state.chunks.get(index) {
                Some(ChunkState::Done) | None => return Ok(true),
                Some(ChunkState::Failed) => return Ok(false),
                Some(ChunkState::Pending | ChunkState::Active) => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            let (next, _) = self
                .ready
                .wait_timeout(state, wait)
                .map_err(|_| anyhow!("media cache wait poisoned"))?;
            state = next;
        }
    }

    /// Serve a range straight from the CDN. Called by the loopback server when
    /// a cache chunk failed (a throttled range, a CDN reset, or a stale signed
    /// URL). Playback continues at network speed instead of dying with
    /// "stream ends prematurely"; the cache stays a fast path, never a single
    /// point of failure.
    fn proxy_body(&self, stream: &mut TcpStream, start: u64, end: u64) -> Result<()> {
        if start > end {
            return Ok(());
        }
        let expected = end - start + 1;
        let mut response = self
            .client
            .get(&self.url)
            .headers(self.headers.clone())
            .header(RANGE, format!("bytes={start}-{end}"))
            .send()
            .context("proxy media range")?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            bail!("proxy media range status {}", response.status());
        }
        let mut buffer = vec![0u8; READ_CHUNK];
        let mut sent = 0u64;
        while sent < expected {
            let want = ((expected - sent) as usize).min(buffer.len());
            let read = response.read(&mut buffer[..want])?;
            if read == 0 {
                break;
            }
            stream.write_all(&buffer[..read])?;
            sent += read as u64;
        }
        if sent < expected {
            bail!("proxy media range short: {sent}/{expected}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------- loopback server

fn server_port() -> Option<u16> {
    if let Some(port) = PORT.get() {
        return Some(*port);
    }
    let _guard = PORT_LOCK.lock().ok()?;
    if let Some(port) = PORT.get() {
        return Some(*port);
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    std::thread::Builder::new()
        .name("media-cache-http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = std::thread::Builder::new()
                    .name("media-cache-conn".into())
                    .spawn(move || {
                        let _ = serve(stream);
                    });
            }
        })
        .ok()?;
    let _ = PORT.set(port);
    Some(port)
}

fn serve(mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = read_request(&mut stream)?;
    let mut lines = request.lines();
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    if method != "GET" {
        return write_status(&mut stream, 405, "Method Not Allowed");
    }
    let id = path
        .trim_start_matches('/')
        .strip_prefix("media/")
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let media = registry()
        .lock()
        .ok()
        .and_then(|map| map.get(id).cloned());
    let Some(media) = media else {
        return write_status(&mut stream, 404, "Not Found");
    };
    let range = parse_range(&request, media.total);
    let (start, end, partial) = match range {
        Some((start, end)) => {
            if start >= media.total || start > end {
                return write_status(&mut stream, 416, "Range Not Satisfiable");
            }
            (start, end.min(media.total - 1), true)
        }
        None => (0, media.total.saturating_sub(1), false),
    };
    let length = end - start + 1;
    let content_type = "application/octet-stream";
    let header = if partial {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
            media.total
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
        )
    };
    stream.write_all(header.as_bytes())?;
    let mut offset = start;
    let mut buffer = vec![0u8; READ_CHUNK];
    while offset <= end {
        match media.wait_ready(offset, READY_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => {
                log::line(&format!(
                    "media cache proxy fallback id={} offset={offset} end={end} reason=slow",
                    media.id
                ));
                media.proxy_body(&mut stream, offset, end)?;
                return Ok(());
            }
            Err(error) => {
                log::line(&format!(
                    "media cache proxy fallback id={} offset={offset} end={end} err={error:#}",
                    media.id
                ));
                media.proxy_body(&mut stream, offset, end)?;
                return Ok(());
            }
        }
        // Never read past the chunk just waited for: the sparse file has a
        // hole for every chunk still downloading, and returning those zeros
        // corrupts the bitstream (the H.264 parser then reports "missing
        // picture in access unit" and a hardware decoder exits).
        let chunk_end = ((offset / media.chunk) + 1).saturating_mul(media.chunk);
        let stop = chunk_bounded_end(end, chunk_end, media.total);
        if stop <= offset {
            break;
        }
        let want = (stop - offset).min(buffer.len() as u64) as usize;
        let read = media.file.seek_read(&mut buffer[..want], offset)?;
        if read == 0 {
            // The file is sized to the full media, so a zero read means the
            // chunk is not really there. Proxy the rest instead of closing the
            // connection and making ffmpeg treat the stream as ended.
            log::line(&format!(
                "media cache short read id={} offset={offset} end={end}",
                media.id
            ));
            media.proxy_body(&mut stream, offset, end)?;
            return Ok(());
        }
        stream.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    Ok(())
}

fn chunk_bounded_end(end: u64, chunk_end: u64, total: u64) -> u64 {
    end.saturating_add(1).min(chunk_end).min(total)
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        if data.windows(4).any(|window| window == b"\r\n\r\n") || data.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&data).to_string())
}

fn parse_range(request: &str, total: u64) -> Option<(u64, u64)> {
    for line in request.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("range") {
            continue;
        }
        let value = value.trim();
        let value = value.strip_prefix("bytes=")?;
        let (start, end) = value.split_once('-')?;
        if start.trim().is_empty() {
            // bytes=-N: the final N bytes.
            let suffix: u64 = end.trim().parse().ok()?;
            if suffix == 0 || total == 0 {
                return Some((0, 0));
            }
            let start = total.saturating_sub(suffix);
            return Some((start, total.saturating_sub(1)));
        }
        let start: u64 = start.trim().parse().ok()?;
        let end = if end.trim().is_empty() {
            total.saturating_sub(1)
        } else {
            end.trim().parse().ok()?
        };
        return Some((start, end));
    }
    None
}

fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> Result<()> {
    let body = format!("{code} {reason}");
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parses_the_total() {
        assert_eq!(content_range_total("bytes 0-0/123456"), Some(123456));
        assert_eq!(content_range_total("bytes 10-20/99"), Some(99));
        assert_eq!(content_range_total("nonsense"), None);
    }

    #[test]
    fn chunks_cover_the_last_byte() {
        assert_eq!(chunks_for(1), 1);
        assert_eq!(chunks_for(CHUNK_BYTES), 1);
        assert_eq!(chunks_for(CHUNK_BYTES + 1), 2);
        assert_eq!(chunks_for(CHUNK_BYTES * 3), 3);
    }

    #[test]
    fn range_headers_are_inclusive_and_clamped() {
        let total = 1000;
        assert_eq!(
            parse_range("GET /x HTTP/1.1\r\nRange: bytes=0-99\r\n", total),
            Some((0, 99))
        );
        assert_eq!(
            parse_range("GET /x HTTP/1.1\r\nRange: bytes=100-\r\n", total),
            Some((100, 999))
        );
        assert_eq!(
            parse_range("GET /x HTTP/1.1\r\nRange: bytes=-10\r\n", total),
            Some((990, 999))
        );
        assert_eq!(parse_range("GET /x HTTP/1.1\r\n", total), None);
    }

    #[test]
    fn journal_has_a_header_and_resumes() {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-journal-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut file = open_journal(&dir, "media", 10 * CHUNK_BYTES, true).unwrap();
            writeln!(file, "7").unwrap();
            writeln!(file, "3").unwrap();
            writeln!(file, "7").unwrap();
        }
        let text = std::fs::read_to_string(dir.join("media.state")).unwrap();
        assert!(
            text.starts_with("v2\n5242880\n524288\n"),
            "unexpected journal: {text:?}"
        );
        let (total, done) =
            read_resume_state(&dir.join("media.state"), Some(10 * CHUNK_BYTES)).unwrap();
        assert_eq!(total, 10 * CHUNK_BYTES);
        assert_eq!(done, vec![3, 7]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lead_stops_at_the_first_missing_chunk() {
        let chunks = vec![
            ChunkState::Done,
            ChunkState::Done,
            ChunkState::Pending,
            ChunkState::Done,
        ];
        assert_eq!(contiguous_lead(&chunks, 10, 40, 0), 20);
        assert_eq!(contiguous_lead(&chunks, 10, 40, 5), 15);
        assert_eq!(contiguous_lead(&chunks, 10, 40, 20), 0);
        assert_eq!(contiguous_lead(&chunks, 10, 40, 40), 0);
    }

    #[test]
    fn ttl_sweep_removes_only_expired_free_entries() {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-sweep-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for id in ["a", "b"] {
            std::fs::write(dir.join(format!("{id}.media")), vec![0u8; 1024]).unwrap();
            std::fs::write(dir.join(format!("{id}.ok")), "1024").unwrap();
        }
        let now = SystemTime::now() + Duration::from_secs(16 * 60);
        let removed = sweep_directory(&dir, now, CACHE_TTL, &|id| id != "b");
        assert_eq!(removed, 1);
        assert!(!dir.join("a.media").exists());
        assert!(!dir.join("a.ok").exists());
        assert!(dir.join("b.media").exists());
        assert!(dir.join("b.ok").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ttl_sweep_keeps_fresh_entries() {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-sweep-fresh-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.media"), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.join("a.ok"), "1024").unwrap();
        let removed = sweep_directory(&dir, SystemTime::now(), CACHE_TTL, &|_| true);
        assert_eq!(removed, 0);
        assert!(dir.join("a.media").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_pin_counts_and_releases_by_id() {
        let key = format!("BV-sweep-pin-{}", std::process::id());
        let id = media_id(&key);
        let first = pin_key(&key);
        let second = pin_key(&key);
        assert!(is_pinned(&id));
        drop(first);
        assert!(is_pinned(&id));
        drop(second);
        assert!(!is_pinned(&id));
    }

    #[test]
    fn a_read_never_crosses_into_a_downloading_chunk() {
        // 512 KB chunks; a 64 KB read at 500 KB would reach 564 KB unless it
        // is clamped to the chunk boundary. Past that boundary the sparse file
        // still has a hole, so returning it corrupted the bitstream.
        let chunk = CHUNK_BYTES;
        let offset = chunk - 12 * 1024;
        let chunk_end = chunk;
        assert_eq!(
            chunk_bounded_end(offset + 64 * 1024 - 1, chunk_end, 10 * chunk),
            chunk
        );
        // A request that ends before the boundary keeps its own end.
        assert_eq!(
            chunk_bounded_end(999, chunk, 10 * chunk),
            1000
        );
        // The last chunk is clamped to the file size.
        assert_eq!(
            chunk_bounded_end(10 * chunk - 1, 10 * chunk, 9 * chunk + 1000),
            9 * chunk + 1000
        );
    }
}
