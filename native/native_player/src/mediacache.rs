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
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, CONTENT_RANGE, RANGE,
};
use reqwest::StatusCode;

use crate::log;

/// Media chunks are small enough that the first frame only waits for a short
/// range on a slow link; workers still fill the rest of the file in parallel.
const CHUNK_BYTES: u64 = 512 * 1024;
/// Very small resources are not worth a cache file and a local server.
const MIN_CACHE_BYTES: u64 = 512 * 1024;
/// Eight HTTP/1.1 range workers: one connection each, enough parallel
/// streams to saturate a normal CDN link without the HTTP/2 resets that made
/// sixteen multiplexed ranges fail. The user's `prefetch_connections` setting
/// can lower this, never raise it.
const MAX_WORKERS: usize = 8;
const READ_CHUNK: usize = 256 * 1024;
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Pending,
    Active,
    Done,
}

struct MediaState {
    chunks: Vec<ChunkState>,
    failed: bool,
    done: bool,
    error: Option<String>,
    /// A reader asked for this chunk; workers pick it before the next
    /// sequential chunk so seeking feels immediate.
    priority: Option<usize>,
}

pub struct CachedMedia {
    pub id: String,
    pub total: u64,
    chunk: u64,
    file: File,
    state: Mutex<MediaState>,
    ready: Condvar,
    stop: AtomicBool,
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

fn write_state_file(id: &str, total: u64, chunks: &[ChunkState]) {
    let Ok(dir) = cache_dir() else {
        return;
    };
    let Ok(mut file) = File::create(state_for(&dir, id)) else {
        return;
    };
    let _ = writeln!(file, "{total}");
    let _ = writeln!(file, "{CHUNK_BYTES}");
    for (index, chunk) in chunks.iter().enumerate() {
        if *chunk == ChunkState::Done {
            let _ = writeln!(file, "{index}");
        }
    }
}

/// Return a loopback URL that ffmpeg can read, starting the parallel download
/// if needed. `None` means the URL did not expose a usable byte size/range
/// response; the caller should keep the original URL.
pub fn cached_url(url: &str, headers: &[(String, String)], key: &str) -> Option<String> {
    if url.trim().is_empty() || key.trim().is_empty() {
        return None;
    }
    let media = prepare(url, headers, key)?;
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

/// Wait until a cached key is complete (or failed). Used by the playlist
/// prefetch worker so it does not steal bandwidth from the active video.
pub fn wait_for_key(key: &str) {
    let id = media_id(key);
    let media = registry().lock().ok().and_then(|map| map.get(&id).cloned());
    if let Some(media) = media {
        let _ = media.wait_complete();
    }
}

/// Download both representations of a resolved source completely. The
/// playlist prefetcher calls this one entry at a time so cache disk usage and
/// bandwidth are predictable.
pub fn prefetch_source(source: &crate::source::MediaSource) -> bool {
    let mut ok = true;
    if let Some(key) = crate::source::media_cache_key(source, false) {
        if let Some(media) = prepare(&source.video_url, &source.headers, &key) {
            ok &= media.wait_complete().is_ok();
        }
    }
    if let Some(url) = source.audio_url.as_deref() {
        if let Some(key) = crate::source::media_cache_key(source, true) {
            if let Some(media) = prepare(url, &source.headers, &key) {
                ok &= media.wait_complete().is_ok();
            }
        }
    }
    ok
}

fn prepare(url: &str, headers: &[(String, String)], key: &str) -> Option<Arc<CachedMedia>> {
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
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
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
                    state: Mutex::new(MediaState {
                        chunks: vec![ChunkState::Done; chunks_for(total)],
                        failed: false,
                        done: true,
                        error: None,
                        priority: None,
                    }),
                    ready: Condvar::new(),
                    stop: AtomicBool::new(false),
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

    // Resume an interrupted parallel download. The `.state` file is a total
    // line followed by one completed chunk index per line. A chunk is only
    // listed after its whole range reached the sparse file, so a crash can
    // lose at most the chunks that were in flight.
    let mut resume: Option<(u64, Vec<usize>)> = None;
    if let Ok(text) = std::fs::read_to_string(&state_path) {
        let mut lines = text.lines();
        if let Some(total) = lines.next().and_then(|line| line.trim().parse::<u64>().ok()) {
            let state_chunk = lines.next().and_then(|line| line.trim().parse::<u64>().ok());
            if state_chunk == Some(CHUNK_BYTES)
                && total >= MIN_CACHE_BYTES
                && file_len(&path) == Some(total)
            {
                let count = chunks_for(total);
                let done: Vec<usize> = lines
                    .filter_map(|line| line.trim().parse::<usize>().ok())
                    .filter(|index| *index < count)
                    .collect();
                resume = Some((total, done));
            }
        }
    }

    let (total, done, file) = if let Some((total, done)) = resume {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .ok()?;
        (total, done, file)
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
        if let Ok(mut state) = File::create(&state_path) {
            let _ = writeln!(state, "{total}");
            let _ = writeln!(state, "{CHUNK_BYTES}");
        }
        (total, Vec::new(), file)
    };
    let chunks = chunks_for(total);
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
        state: Mutex::new(MediaState {
            chunks: chunk_states,
            failed: false,
            done: complete,
            error: None,
            priority: None,
        }),
        ready: Condvar::new(),
        stop: AtomicBool::new(false),
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

    let workers = o11a1::settings::load()
        .prefetch_connections
        .clamp(1, MAX_WORKERS as u32) as usize;
    let workers = workers.min(chunks).max(1);
    for _ in 0..workers {
        let media = media.clone();
        let url = url.to_string();
        let headers = header_map.clone();
        let client = client.clone();
        let _ = std::thread::Builder::new()
            .name("media-cache-worker".into())
            .spawn(move || download_worker(media, client, url, headers));
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
    loop {
        if media.stop.load(Ordering::SeqCst) {
            break;
        }
        let index = {
            let Ok(mut state) = media.state.lock() else {
                break;
            };
            if state.failed || state.done {
                None
            } else {
                let priority = state.priority.take();
                let candidate = priority
                    .filter(|index| {
                        *index < state.chunks.len()
                            && state.chunks[*index] == ChunkState::Pending
                    })
                    .or_else(|| {
                        state
                            .chunks
                            .iter()
                            .position(|chunk| *chunk == ChunkState::Pending)
                    });
                match candidate {
                    Some(index) => {
                        state.chunks[index] = ChunkState::Active;
                        Some(index)
                    }
                    None => None,
                }
            }
        };
        let Some(index) = index else {
            break;
        };
        let start = index as u64 * media.chunk;
        let end = ((index as u64 + 1) * media.chunk - 1).min(media.total - 1);
        let mut last_error = String::new();
        let mut ok = false;
        // A single flaky CDN connection used to abort the whole video after
        // three tries. Eight backed-off attempts ride out a reset or a short
        // CDN hiccup; only a genuinely unavailable range fails the media.
        for attempt in 0..8 {
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
                    let backoff = (300 * (attempt as u64 + 1)).min(2000);
                    std::thread::sleep(Duration::from_millis(backoff));
                }
            }
        }
        let done = {
            let Ok(mut state) = media.state.lock() else {
                break;
            };
            if ok {
                state.chunks[index] = ChunkState::Done;
                let complete = state.chunks.iter().all(|chunk| *chunk == ChunkState::Done);
                if complete {
                    state.done = true;
                } else {
                    write_state_file(&media.id, media.total, &state.chunks);
                }
                complete
            } else {
                state.failed = true;
                state.error = Some(last_error);
                false
            }
        };
        media.ready.notify_all();
        if done {
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
    if response.status() != StatusCode::PARTIAL_CONTENT
        && response.status() != StatusCode::OK
    {
        bail!("http status {}", response.status());
    }
    let mut offset = start;
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        if media.stop.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let read = response.read(&mut buffer)?;
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
        if offset > end + 1 {
            bail!("range overflow");
        }
    }
    if offset != end + 1 {
        bail!("short range body: got {} expected {}", offset - start, end - start + 1);
    }
    Ok(())
}

impl CachedMedia {
    fn failed(&self) -> bool {
        self.state.lock().map(|state| state.failed).unwrap_or(true)
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
            let (next, _) = self
                .ready
                .wait_timeout(state, Duration::from_millis(250))
                .map_err(|_| anyhow!("media cache wait poisoned"))?;
            state = next;
        }
    }

    fn wait_ready(&self, offset: u64) -> Result<()> {
        if offset >= self.total {
            return Ok(());
        }
        let index = (offset / self.chunk) as usize;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("media cache lock poisoned"))?;
        if index < state.chunks.len() {
            state.priority = Some(index);
        }
        loop {
            if state.failed {
                return Err(anyhow!(
                    "media cache failed: {}",
                    state.error.clone().unwrap_or_default()
                ));
            }
            if state
                .chunks
                .get(index)
                .map(|chunk| *chunk == ChunkState::Done)
                .unwrap_or(true)
            {
                return Ok(());
            }
            let (next, _) = self
                .ready
                .wait_timeout(state, Duration::from_millis(200))
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
        if let Err(error) = media.wait_ready(offset) {
            log::line(&format!(
                "media cache proxy fallback id={} offset={offset} end={end} err={error:#}",
                media.id
            ));
            media.proxy_body(&mut stream, offset, end)?;
            return Ok(());
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
            break;
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
