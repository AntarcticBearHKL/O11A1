//! Client for the persistent RIFE helper (`assets/rife/rife-helper.exe`).
//!
//! RIFE is an optical-flow interpolator: given the two decoded frames that
//! surround a display tick it synthesises a real in-between picture (blend mode
//! can only cross-fade them). The helper is a Vulkan + ncnn service that keeps
//! the model and the GPU device resident, so a request pays only the per-pair
//! compute — measured 15.1 ms median per pair at 1280x720 on an RTX 4060 Ti
//! (66 pairs/s), and ~40 ms at 1080p, which is why this client is capped at
//! 720p (see [`MAX_W`]/[`MAX_H`]).
//!
//! # Transport (shared memory + two events)
//!
//! The **client** owns the Windows objects; the helper only opens them:
//!
//! * a named section `Local\bili_rife_<pid>_<seq>` of
//!   `4096 + 3 * frame` bytes,
//! * two auto-reset events `<name>_req` and `<name>_resp`,
//! * the helper is started as `rife-helper.exe --shm <name> --model <dir>
//!   --timestep 0.5`.
//!
//! Layout inside the section (the helper's own contract):
//!
//! ```text
//! offset 0     [u32 status][u32 w][u32 h][u32 bytes]   status 1 = request, 2 = response
//! offset 4096  frame A (w*h*4 RGBA)
//! offset 4096+bytes   frame B
//! offset 4096+2*bytes interpolated frame (written by the helper)
//! ```
//!
//! A pair is: write A and B, set the header, `SetEvent(_req)`, wait on
//! `_resp`. The helper validates `bytes == w*h*4` and exits on a bad header, so
//! the client checks its own sizes first.
//!
//! # Timing model
//!
//! The timestep is a *process* argument of the helper, not part of the request
//! header, so one helper instance interpolates at one fixed phase (`0.5`: the
//! midpoint, which is exactly what a 30→60 Hz display grid needs). Requests for
//! a different `t` are refused ([`RifeClient::interpolate_into`] returns
//! `false`) instead of silently returning a frame at the wrong time — the pacer
//! then blends, which is correct at any phase.
//!
//! # Failure policy
//!
//! Playback must never be held hostage by the interpolator:
//!
//! * the helper and the model are resolved once; if either is missing,
//!   [`RifeClient::available`] is `false` and **nothing is spawned**;
//! * while the helper is still loading (0.5–4 s: the model + Vulkan pipeline
//!   are built before it opens the events) requests return `false` without
//!   blocking, so the picture simply keeps blending;
//! * a pair that does not answer within [`PAIR_TIMEOUT`] (120 ms) disables RIFE
//!   for the rest of the session: the helper is killed, the handles are closed
//!   and every later request returns `false` immediately. A late writer must
//!   never touch a buffer the pacer has already reused, so the process dies
//!   instead of being left behind;
//! * the same happens when the helper exits on its own (killed mid-playback,
//!   GPU reset).
//!
//! The child is assigned to the process-wide job object ([`crate::job`]) as
//! well, so a crash or a force-kill of the app reaps it at the OS level even
//! though no Rust destructor runs.

use std::ffi::{c_void, OsStr};
use std::io::{BufRead, BufReader};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest output frame RIFE is allowed to touch. 1280x720 measured 15.1 ms per
/// pair; 1080p is ~40 ms, i.e. slower than the 16.7 ms display slot it would
/// feed, so the player stays on frame blending above this size.
pub const MAX_W: u32 = 1280;
pub const MAX_H: u32 = 720;

/// Frames live at this offset, after the four-u32 header (helper contract).
const PAYLOAD_OFFSET: usize = 4096;
/// The helper runs with this timestep (see the module docs).
pub const DEFAULT_TIMESTEP: f32 = 0.5;
/// How far a requested `t` may sit from [`DEFAULT_TIMESTEP`] before the request
/// is refused. The display grid lands within a few percent of the midpoint.
const TIMESTEP_TOLERANCE: f32 = 0.06;
/// One pair must answer inside this budget, or RIFE is disabled for the session.
/// 120 ms is ~7x the measured 720p cost: only a hung/contended GPU trips it.
pub const PAIR_TIMEOUT: Duration = Duration::from_millis(120);
/// Helper startup (model + Vulkan pipeline) before the only spawned pair is
/// allowed to wait. The pacer never waits longer than one pair; requests made
/// while the helper is still loading are refused immediately.
const NAME_PREFIX: &str = "Local\\bili_rife";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const INVALID_HANDLE_VALUE: Handle = -1;
const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;

type Handle = isize;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileMappingW(
        file: Handle,
        attributes: *const c_void,
        protect: u32,
        high: u32,
        low: u32,
        name: *const u16,
    ) -> Handle;
    fn MapViewOfFile(mapping: Handle, access: u32, high: u32, low: u32, bytes: usize) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn CreateEventW(
        attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn SetEvent(event: Handle) -> i32;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
}

/// Why RIFE is off for this session (kept for the log line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// `rife-helper.exe` or the model was not found next to the app.
    Missing,
    /// The helper could not be started (or its shared memory could not be made).
    SpawnFailed,
    /// A pair did not answer inside [`PAIR_TIMEOUT`].
    Timeout,
    /// The helper process went away on its own.
    HelperExit,
    /// The helper answered with something other than `status = 2`.
    BadResponse,
    /// The frame is larger than [`MAX_W`]x[`MAX_H`].
    TooLarge,
}

impl Off {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "helper_missing",
            Self::SpawnFailed => "spawn_failed",
            Self::Timeout => "timeout",
            Self::HelperExit => "helper_exit",
            Self::BadResponse => "bad_response",
            Self::TooLarge => "frame_too_large",
        }
    }
}

/// One live helper process plus the objects the client owns for it.
struct Session {
    child: Child,
    mapping: Handle,
    /// `MapViewOfFile` result, kept as an integer so the type stays `Send`.
    view: usize,
    req: Handle,
    resp: Handle,
    /// Set by the stderr watcher once the helper printed `transport=shm`; that
    /// line is emitted only after the model is loaded and the events are open.
    ready: Arc<AtomicBool>,
    /// Last diagnostics the helper printed (its startup/error lines).
    log: Arc<Mutex<Vec<String>>>,
    /// Cached geometry of the last request, so the copies use the same lengths
    /// the header announced.
    name: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Clean shutdown first (the helper treats w=0,h=0 as "exit"), then a
        // bounded wait, then the hard kill. Handles are closed either way, so a
        // helper that ignores the request cannot outlive this struct.
        if self.view != 0 {
            unsafe {
                let header = self.view as *mut u32;
                *header.add(1) = 0;
                *header.add(2) = 0;
                *header.add(3) = 0;
                fence(Ordering::Release);
                *header = 1;
                fence(Ordering::Release);
                SetEvent(self.req);
            }
        }
        let mut waited = 0;
        while waited < 200 {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
            waited += 10;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            if self.view != 0 {
                UnmapViewOfFile(self.view as *const c_void);
            }
            for handle in [self.req, self.resp, self.mapping] {
                if handle != 0 && handle != INVALID_HANDLE_VALUE {
                    CloseHandle(handle);
                }
            }
        }
    }
}

/// A lazily spawned RIFE helper with the shared-memory transport.
pub struct RifeClient {
    /// `<dir>` holding `rife-helper.exe` and `model/flownet.{param,bin}`.
    dir: Option<PathBuf>,
    session: Option<Session>,
    /// Sticky reason RIFE is off for the rest of this session.
    off: Option<Off>,
    pairs: u64,
    timeouts: u64,
    last_pair_us: u32,
    seq: u64,
}

impl Default for RifeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RifeClient {
    /// Resolve the helper + model now (cheap: a handful of `exists` calls) and
    /// report whether RIFE could be used. Nothing is spawned yet.
    pub fn new() -> Self {
        Self {
            dir: probe().map(Path::to_path_buf),
            session: None,
            off: None,
            pairs: 0,
            timeouts: 0,
            last_pair_us: 0,
            seq: 0,
        }
    }

    /// Whether a request could succeed right now: the helper and the model must
    /// exist, and no failure may have latched. A helper that is still loading
    /// counts as available (the caller simply gets `false` from a request until
    /// the first pair succeeds).
    pub fn available(&self) -> bool {
        self.dir.is_some() && self.off.is_none()
    }

    /// Why RIFE is off for this session, if it is.
    pub fn reason(&self) -> Option<Off> {
        self.off
    }

    /// Wall time of the last successful pair, in milliseconds (`0.0` until the
    /// first pair lands).
    pub fn last_pair_ms(&self) -> f32 {
        self.last_pair_us as f32 / 1000.0
    }

    /// Successful pairs since the client was created.
    pub fn pairs(&self) -> u64 {
        self.pairs
    }

    /// Whether the helper has finished loading and is serving pairs.
    #[allow(dead_code)] // Diagnostic accessor: the pacer logs its own "serving" line.
    pub fn ready(&self) -> bool {
        match self.session.as_ref() {
            Some(session) => session.ready.load(Ordering::Acquire),
            None => false,
        }
    }

    /// Kill the helper and release its objects. Idempotent; the next request
    /// would spawn a fresh one.
    pub fn shutdown(&mut self) {
        if let Some(session) = self.session.take() {
            let lines = session
                .log
                .lock()
                .map(|lines| lines.join(" | "))
                .unwrap_or_default();
            crate::debug_log(&format!(
                "rife stop pairs={} timeouts={} pair_ms={:.2} shm={} log={}",
                self.pairs,
                self.timeouts,
                self.last_pair_ms(),
                session.name,
                if lines.is_empty() { "-" } else { lines.as_str() }
            ));
            // Drop runs the clean-shutdown handshake and closes every handle.
        }
    }

    /// Interpolate `t` between `a` and `b` into `out` (RGBA8, `w*h*4`).
    ///
    /// Returns `false` — never a wrong picture — when RIFE cannot serve this
    /// pair: helper missing, still loading, already disabled, frame above the
    /// cap, or `t` not at the helper's fixed timestep. The caller is expected to
    /// fall back to its own blend in that case.
    pub fn interpolate_into(
        &mut self,
        a: &[u8],
        b: &[u8],
        w: u32,
        h: u32,
        t: f32,
        out: &mut [u8],
    ) -> bool {
        if !self.available() {
            return false;
        }
        if w == 0 || h == 0 || w > MAX_W || h > MAX_H {
            self.disable(Off::TooLarge);
            return false;
        }
        let frame = (w as usize) * (h as usize) * 4;
        if a.len() < frame || b.len() < frame || out.len() < frame {
            return false;
        }
        if !t.is_finite() || (t - DEFAULT_TIMESTEP).abs() > TIMESTEP_TOLERANCE {
            // One helper instance interpolates at one phase; the caller blends,
            // which is exact at any t.
            return false;
        }
        if self.session.is_none() && !self.spawn(w, h) {
            return false;
        }
        // Take the session out while it is used, so a failure can drop it (which
        // kills the helper) without borrowing `self` twice.
        let Some(mut session) = self.session.take() else {
            return false;
        };
        if !session.ready.load(Ordering::Acquire) {
            // Still loading the model: never block the pacer for it.
            self.session = Some(session);
            return false;
        }
        if let Ok(Some(status)) = session.child.try_wait() {
            drop(session);
            self.disable_with(Off::HelperExit, Some(&format!("code={status}")));
            return false;
        }

        let start = Instant::now();
        // SAFETY: the view covers PAYLOAD_OFFSET + 3 * MAX_W * MAX_H * 4 bytes
        // (allocated in `spawn`), and `frame` is at most one of those frames, so
        // every write below stays inside the mapping. `out` was length-checked.
        unsafe {
            let view = session.view as *mut u8;
            let header = view as *mut u32;
            std::ptr::copy_nonoverlapping(a.as_ptr(), view.add(PAYLOAD_OFFSET), frame);
            std::ptr::copy_nonoverlapping(b.as_ptr(), view.add(PAYLOAD_OFFSET + frame), frame);
            *header.add(1) = w;
            *header.add(2) = h;
            *header.add(3) = frame as u32;
            fence(Ordering::Release);
            *header = 1;
            fence(Ordering::Release);
            SetEvent(session.req);
        }
        let wait_ms = PAIR_TIMEOUT.as_millis().min(u32::MAX as u128) as u32;
        let status = unsafe { WaitForSingleObject(session.resp, wait_ms) };
        let elapsed = start.elapsed();
        if status != WAIT_OBJECT_0 {
            self.timeouts += 1;
            // A late writer would race the caller's buffer, so this is fatal for
            // the session: kill the helper, close its objects, never wait again.
            drop(session);
            let reason = if status == WAIT_TIMEOUT {
                Off::Timeout
            } else {
                Off::BadResponse
            };
            self.disable_with(
                reason,
                Some(&format!(
                    "pair_ms={:.2} waited={} status={status}",
                    elapsed.as_secs_f64() * 1000.0,
                    wait_ms
                )),
            );
            return false;
        }
        // SAFETY: as above; the helper wrote the output frame at 2*bytes.
        let answer = unsafe { *(session.view as *const u32) };
        if answer != 2 {
            drop(session);
            self.disable_with(Off::BadResponse, Some(&format!("status={answer}")));
            return false;
        }
        // SAFETY: as above; the copy destination is `out`, length-checked above.
        unsafe {
            fence(Ordering::Acquire);
            let view = session.view as *mut u8;
            std::ptr::copy_nonoverlapping(
                view.add(PAYLOAD_OFFSET + 2 * frame),
                out.as_mut_ptr(),
                frame,
            );
            // The next request starts from a clean header.
            *(view as *mut u32) = 0;
        }
        self.session = Some(session);
        self.pairs += 1;
        self.last_pair_us = elapsed.as_micros().min(u32::MAX as u128) as u32;
        true
    }

    /// Convenience wrapper that allocates the output frame. The pacer uses
    /// [`Self::interpolate_into`] with a pooled buffer instead.
    #[allow(dead_code)] // Kept as the documented one-shot API for tests/tools.
    pub fn interpolate(
        &mut self,
        a: &[u8],
        b: &[u8],
        w: u32,
        h: u32,
        t: f32,
    ) -> Option<Vec<u8>> {
        let frame = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
        let mut out = vec![0u8; frame];
        self.interpolate_into(a, b, w, h, t, &mut out)
            .then_some(out)
    }

    /// Latch a fatal reason for the session.
    fn disable(&mut self, off: Off) {
        self.disable_with(off, None);
    }

    fn disable_with(&mut self, off: Off, detail: Option<&str>) {
        if self.off.is_some() {
            return;
        }
        self.off = Some(off);
        self.session = None;
        crate::debug_log(&format!(
            "rife disabled reason={} pairs={} timeouts={} detail={} -> blend",
            off.as_str(),
            self.pairs,
            self.timeouts,
            detail.unwrap_or("-")
        ));
    }

    /// Create the section + events and start the helper. The section is sized
    /// for the largest allowed frame once, so a later resize cannot desync it.
    fn spawn(&mut self, w: u32, h: u32) -> bool {
        let Some(dir) = self.dir.clone() else {
            return false;
        };
        self.seq += 1;
        let name = format!("{NAME_PREFIX}_{}_{}", std::process::id(), self.seq);
        let deep = PAYLOAD_OFFSET + 3 * (MAX_W as usize) * (MAX_H as usize) * 4;
        let wname = wide(&name);
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                deep as u32,
                wname.as_ptr(),
            )
        };
        if mapping == 0 || mapping == INVALID_HANDLE_VALUE {
            crate::debug_log(&format!(
                "rife spawn failed stage=mapping err={} bytes={deep}",
                std::io::Error::last_os_error()
            ));
            self.disable(Off::SpawnFailed);
            return false;
        }
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, deep) };
        if view.is_null() {
            unsafe {
                CloseHandle(mapping);
            }
            crate::debug_log(&format!(
                "rife spawn failed stage=view err={}",
                std::io::Error::last_os_error()
            ));
            self.disable(Off::SpawnFailed);
            return false;
        }
        let req = unsafe {
            CreateEventW(
                std::ptr::null(),
                0,
                0,
                wide(&format!("{name}_req")).as_ptr(),
            )
        };
        let resp = unsafe {
            CreateEventW(
                std::ptr::null(),
                0,
                0,
                wide(&format!("{name}_resp")).as_ptr(),
            )
        };
        if req == 0 || resp == 0 {
            unsafe {
                for handle in [req, resp, mapping] {
                    if handle != 0 {
                        CloseHandle(handle);
                    }
                }
                UnmapViewOfFile(view as *const c_void);
            }
            crate::debug_log(&format!(
                "rife spawn failed stage=events err={}",
                std::io::Error::last_os_error()
            ));
            self.disable(Off::SpawnFailed);
            return false;
        }

        let helper = dir.join("rife-helper.exe");
        let model = dir.join("model");
        let started = Instant::now();
        let mut command = Command::new(&helper);
        command
            .arg("--shm")
            .arg(&name)
            .arg("--model")
            .arg(&model)
            .arg("--timestep")
            .arg(format!("{DEFAULT_TIMESTEP:.2}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                unsafe {
                    for handle in [req, resp, mapping] {
                        CloseHandle(handle);
                    }
                    UnmapViewOfFile(view as *const c_void);
                }
                crate::debug_log(&format!("rife spawn failed stage=process err={err}"));
                self.disable(Off::SpawnFailed);
                return false;
            }
        };
        crate::job::assign(&child);
        crate::debug_log(&format!(
            "rife spawn shm={name} pid={} frame={w}x{h} model={}",
            child.id(),
            model.display()
        ));

        // Watch the helper's stderr: `transport=shm name=...` is printed exactly
        // when the model is loaded and the events are open, i.e. when it is
        // ready to serve. Errors are kept for the teardown line.
        let ready = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let ready = ready.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(pipe);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if line.starts_with("transport=shm") {
                        ready.store(true, Ordering::Release);
                    }
                    // Acknowledge the trim above so the compiler sees the copy.
                    crate::debug_log(&format!("rife helper: {line}"));
                    if let Ok(mut lines) = log.lock() {
                        if lines.len() < 8 {
                            lines.push(line);
                        }
                    }
                }
                // The helper closed its stderr: it is gone.
                ready.store(false, Ordering::Release);
            });
        }

        self.session = Some(Session {
            child,
            mapping,
            view: view as usize,
            req,
            resp,
            ready,
            log,
            name,
        });
        let load_ms = started.elapsed().as_millis();
        crate::debug_log(&format!(
            "rife spawn armed started_ms={load_ms} (loading in background)"
        ));
        true
    }
}

/// Killing the client (last `Arc` gone, mode change, player close) always
/// reaps the helper: the `Session` drop does the shutdown handshake, the kill
/// and the handle closes.
impl Drop for RifeClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Root directory of the helper, cached for the process: the install cannot
/// appear while the app is running, and a missing one is reported once.
static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// The resolved helper install, or `None` when either the helper or the model
/// is missing. Cheap to call (a cached `Option`), never spawns anything.
pub fn probe() -> Option<&'static Path> {
    ROOT.get_or_init(|| {
        let dir = resolve_dir();
        match dir.as_ref() {
            Some(dir) => crate::debug_log(&format!("rife found dir={}", dir.display())),
            None => crate::debug_log(&format!(
                "rife unavailable reason={} checked={}",
                Off::Missing.as_str(),
                search_paths()
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        }
        dir
    })
    .as_deref()
}

/// Root directory of the helper, or `None` when the install is incomplete.
fn resolve_dir() -> Option<PathBuf> {
    let mut candidates = search_paths();
    candidates.dedup();
    candidates.into_iter().find(|dir| complete(dir))
}

/// Candidate roots, most specific first. `BILI_RIFE_DIR` overrides everything
/// and is *authoritative*: when it is set the lookup never falls through to the
/// shipped copy, so a QA run can point at an empty/renamed install and see the
/// honest "missing" path.
fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(dir) = std::env::var("BILI_RIFE_DIR") {
        paths.push(PathBuf::from(dir));
        return paths;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Release bundle: `<exe dir>/assets/rife` or the exe's own folder.
            paths.push(dir.join("assets").join("rife"));
            paths.push(dir.join("rife"));
            paths.push(dir.to_path_buf());
        }
    }
    // Development build: the repository's assets folder.
    paths.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join("rife"));
    paths
}

/// A usable install has the helper and both model files.
fn complete(dir: &Path) -> bool {
    dir.join("rife-helper.exe").is_file()
        && dir.join("model").join("flownet.param").is_file()
        && dir.join("model").join("flownet.bin").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_install_is_recognised() {
        // The repository ships the measured helper + model.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("rife");
        assert!(complete(&dir), "assets/rife is part of the tree");
        assert!(resolve_dir().is_some());
    }

    #[test]
    fn an_incomplete_install_is_not_available() {
        let missing = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("rife-does-not-exist");
        assert!(!complete(&missing));
    }

    #[test]
    fn oversized_frames_are_refused_before_any_request() {
        let mut client = RifeClient::new();
        let mut out = vec![0u8; 4 * 8 * 8];
        let a = vec![0u8; 4 * 8 * 8];
        // 1281x1 is above the 1280 width cap: refused (no helper is started).
        assert!(!client.interpolate_into(&a, &a, 1281, 1, 0.5, &mut out));
        assert_eq!(client.reason(), Some(Off::TooLarge));
        assert!(!client.available());
    }

    #[test]
    fn off_phase_requests_are_refused_without_a_helper() {
        let mut client = RifeClient::new();
        let frame = 4 * 16 * 16;
        let a = vec![0u8; frame];
        let mut out = vec![0u8; frame];
        // The helper runs at t = 0.5 only; anything far from it must fall back.
        assert!(!client.interpolate_into(&a, &a, 16, 16, 0.1, &mut out));
        // A refused request is not a failure: the client stays available.
        assert!(client.available());
        assert_eq!(client.reason(), None);
        assert_eq!(client.last_pair_ms(), 0.0);
    }
}
