//! The process-wide network thread pool.
//!
//! Every blocking network job the app runs - a cover, an avatar, a comment
//! page, a like/coin/fav mutation, a follow-up API round trip that is not worth
//! holding the UI thread for - used to start a fresh OS thread per request.
//! Dozens of them appear while a feed loads, each one is created, runs one
//! `block_on`, and dies, and the placement work that implies (thread creation,
//! stack commit, scheduler churn) is exactly what competes with the window's
//! pointer and paint handlers.
//!
//! This module replaces that with **one fixed set of threads**, created once in
//! [`init`] before the first job and never again. A thread is named `net-N`.
//! Pool threads stay at the process class default: the process itself runs in
//! `BELOW_NORMAL_PRIORITY_CLASS` (set once from the UI thread in `main`), so
//! every `net-N` thread is below normal in absolute terms without touching its
//! own priority - the UI thread is the only one raised, inside that class.
//! Jobs are FIFO within three priorities; a job that panics is caught and
//! logged and its thread goes straight back to the pool.
//!
//! The pool also drives the prefetch engine's connection slots (see
//! [`crate::prefetch`]): when there is no job to run, a thread asks the prefetch
//! registry for one slot that has work and lets it make a step. This is how a
//! video session gets its sixteen fetch workers without creating a single
//! thread: the threads already exist, the session only fills in slots.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Smallest pool [`init`] honours. Below two a single stalled job would leave
/// no thread for the next request.
pub const MIN_THREADS: usize = 2;
/// Largest pool [`init`] honours: the same ceiling the prefetch proxy may ask
/// for connections, so the pool can never be the smaller of the two.
pub const MAX_THREADS: usize = 32;
/// Default pool size when no setting and no environment override is present.
pub const DEFAULT_THREADS: usize = 12;
/// How long an idle pool thread sleeps before looking for prefetch work again.
///
/// Only reached while a player session has bound slots (otherwise the thread
/// parks on the condvar until a job arrives). Five milliseconds is well inside
/// a frame and costs twelve wake-ups per frame in the worst case, not twelve
/// busy threads.
const IDLE_POLL: Duration = Duration::from_millis(5);
/// How long a thread with nothing at all to do parks before it re-checks for
/// prefetch bindings. A binding notifies the condvar, so this is only a safety
/// net, not the normal wake path.
const PARK_POLL: Duration = Duration::from_millis(250);

/// Job priority. High first, then Normal, then Low; each class is FIFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prio {
    /// A request whose answer the user is waiting for (opening a video).
    High,
    /// Ordinary API mutations and page loads.
    Normal,
    /// Covers, thumbnails, avatars, speculative warming: always droppable.
    Low,
}

impl Prio {
    fn index(self) -> usize {
        match self {
            Prio::High => 0,
            Prio::Normal => 1,
            Prio::Low => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Prio::High => "high",
            Prio::Normal => "normal",
            Prio::Low => "low",
        }
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Queued {
    prio: Prio,
    job: Job,
}

#[derive(Default)]
struct Inner {
    queues: [VecDeque<Queued>; 3],
    running: usize,
    low_running: usize,
    completed: u64,
    panicked: u64,
}

static INNER: Mutex<Inner> = Mutex::new(Inner {
    queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
    running: 0,
    low_running: 0,
    completed: 0,
    panicked: 0,
});
static WAKE: Condvar = Condvar::new();
static THREADS: AtomicUsize = AtomicUsize::new(0);
/// `0` = not started, `1` = [`init`] is running, `2` = running.
static STATE: AtomicUsize = AtomicUsize::new(0);

fn lock_inner() -> std::sync::MutexGuard<'static, Inner> {
    INNER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A snapshot of the pool, for the profiler and the QA line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Threads the pool owns (fixed for the process lifetime).
    pub threads: usize,
    /// Jobs executing right now.
    pub running: usize,
    /// Jobs waiting in the three queues.
    pub queued: usize,
    /// Jobs that finished since [`init`] (includes panicked ones).
    pub completed: u64,
    /// Jobs whose closure panicked (caught, logged, thread survived).
    pub panicked: u64,
}

/// Start the pool. Call **once**, before any [`submit`].
///
/// Idempotent: the first call wins, later calls only log what they were asked
/// for. Size is clamped into `MIN_THREADS..=MAX_THREADS`.
pub fn init(threads: usize) {
    let wanted = threads.clamp(MIN_THREADS, MAX_THREADS);
    if wanted != threads {
        crate::debug_log(&format!(
            "netpool size clamped asked={threads} using={wanted}"
        ));
    }
    match STATE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => {}
        Err(_) => {
            crate::debug_log(&format!(
                "netpool init ignored threads={wanted} already={}",
                THREADS.load(Ordering::SeqCst)
            ));
            return;
        }
    }
    let mut started = 0usize;
    for index in 0..wanted {
        let spawned = thread::Builder::new()
            .name(format!("net-{index}"))
            .spawn(pool_thread);
        match spawned {
            Ok(_) => started += 1,
            Err(error) => {
                crate::debug_log(&format!(
                    "netpool thread failed index={index} err={error}"
                ));
                break;
            }
        }
    }
    THREADS.store(started, Ordering::SeqCst);
    STATE.store(2, Ordering::SeqCst);
    WAKE.notify_all();
    crate::debug_log(&format!(
        "netpool init threads={started} prio=below-normal naming=net-N"
    ));
}

/// Whether [`init`] has completed. Prefetch binds its slots only when it has:
/// a session started before the pool (in a test, say) keeps the old
/// task-per-worker schedule instead of binding slots nothing would pump.
pub fn initialized() -> bool {
    STATE.load(Ordering::SeqCst) == 2 && THREADS.load(Ordering::SeqCst) > 0
}

/// Number of threads in the pool (`0` before [`init`]).
pub fn threads() -> usize {
    THREADS.load(Ordering::SeqCst)
}

/// Queue `job` at `prio`. Never blocks and never spawns a thread.
///
/// If the pool has not been started yet - only reachable from a test or a
/// helper that runs before `main` wires the pool - it is started at the
/// default size first, so a submitted job can never be silently dropped.
pub fn submit(prio: Prio, job: Box<dyn FnOnce() + Send + 'static>) {
    if !initialized() {
        init(DEFAULT_THREADS);
    }
    let mut inner = lock_inner();
    inner.queues[prio.index()].push_back(Queued { prio, job });
    drop(inner);
    // One notification per job: the queue itself is the record, so a lost
    // notification only costs one timeout of the parking thread.
    WAKE.notify_one();
}

/// Counts for the profiler line: threads, running, queued, completed.
pub fn stats() -> PoolStats {
    let inner = lock_inner();
    PoolStats {
        threads: THREADS.load(Ordering::SeqCst),
        running: inner.running,
        queued: inner.queues.iter().map(VecDeque::len).sum(),
        completed: inner.completed,
        panicked: inner.panicked,
    }
}

/// Wake every pool thread: a prefetch session just bound or unbound slots, or
/// is shutting down and its claims should stop.
pub(crate) fn wake() {
    WAKE.notify_all();
}

fn pool_thread() {
    let mut cursor = 0usize;
    loop {
        // A request the user is waiting for always wins.
        if let Some(job) = take_high() {
            run_job(job);
            continue;
        }
        // Then give the prefetch sessions a step. Their transfers are
        // dispatched to the shared async runtime, so this call is short even
        // when sixteen connections are busy - it must not wait behind a feed's
        // worth of queued cover jobs, or opening a video would stall.
        if crate::prefetch::pump_slot(&mut cursor) {
            continue;
        }
        if let Some(job) = take_normal_or_low() {
            run_job(job);
            continue;
        }
        let wait = if crate::prefetch::has_bindings() {
            IDLE_POLL
        } else {
            PARK_POLL
        };
        let inner = lock_inner();
        let _ = WAKE.wait_timeout(inner, wait);
    }
}

/// Pop a High-priority job.
fn take_high() -> Option<Queued> {
    let mut inner = lock_inner();
    if let Some(job) = inner.queues[Prio::High.index()].pop_front() {
        inner.running += 1;
        return Some(job);
    }
    None
}

/// Pop a Normal job, then a Low one if the Low lane has room.
///
/// Low-priority work is capped at `threads - 2` at a time: covers and
/// speculative warming must never be able to occupy the whole pool, or a click
/// would wait behind a thumbnail.
fn take_normal_or_low() -> Option<Queued> {
    let mut inner = lock_inner();
    if let Some(job) = inner.queues[Prio::Normal.index()].pop_front() {
        inner.running += 1;
        return Some(job);
    }
    let low_limit = THREADS.load(Ordering::SeqCst).saturating_sub(2).max(1);
    if inner.low_running < low_limit {
        if let Some(job) = inner.queues[Prio::Low.index()].pop_front() {
            inner.running += 1;
            inner.low_running += 1;
            return Some(job);
        }
    }
    None
}

fn run_job(job: Queued) {
    let prio = job.prio;
    let result = catch_unwind(AssertUnwindSafe(job.job));
    let mut inner = lock_inner();
    if prio == Prio::Low {
        inner.low_running = inner.low_running.saturating_sub(1);
    }
    inner.running = inner.running.saturating_sub(1);
    match result {
        Ok(()) => inner.completed += 1,
        Err(payload) => {
            inner.completed += 1;
            inner.panicked += 1;
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            crate::debug_log(&format!(
                "netpool job panicked prio={} msg={message}",
                prio.name()
            ));
        }
    }
    drop(inner);
    // A Low slot may have just freed up, and a waiter may have skipped a job
    // for exactly that reason.
    WAKE.notify_one();
    let completed = stats();
    if completed.completed > 0 && completed.completed % 10 == 0 {
        crate::debug_log(&format!(
            "netpool jobs completed={} panicked={} queued={} running={}",
            completed.completed, completed.panicked, completed.queued, completed.running
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Instant;

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(10), "timeout: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn jobs_run_on_the_fixed_threads_and_priorities_hold() {
        init(4);
        assert_eq!(threads(), 4);
        let names = Arc::new(Mutex::new(Vec::<String>::new()));
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let names = names.clone();
            let done = done.clone();
            submit(
                Prio::Normal,
                Box::new(move || {
                    names
                        .lock()
                        .map(|mut names| {
                            names.push(
                                thread::current()
                                    .name()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        })
                        .ok();
                    done.fetch_add(1, Ordering::SeqCst);
                }),
            );
        }
        wait_until("jobs", || done.load(Ordering::SeqCst) == 8);
        let names = names.lock().map(|names| names.clone()).unwrap_or_default();
        assert_eq!(names.len(), 8);
        assert!(
            names.iter().all(|name| name.starts_with("net-")),
            "every job runs on a pool thread: {names:?}"
        );
        assert!(stats().completed >= 8);
        assert_eq!(stats().threads, 4);
    }

    #[test]
    fn a_panicking_job_leaves_the_thread_alive() {
        init(4);
        let before = stats().completed;
        submit(Prio::Normal, Box::new(|| panic!("netpool test panic")));
        wait_until("panic counted", || stats().completed > before);
        let proof = Arc::new(AtomicBool::new(false));
        let flag = proof.clone();
        submit(
            Prio::High,
            Box::new(move || {
                flag.store(true, Ordering::SeqCst);
            }),
        );
        wait_until("next job", || proof.load(Ordering::SeqCst));
        assert!(stats().panicked >= 1);
        assert_eq!(threads(), 4, "a panic never replaces a thread");
    }

    #[test]
    fn high_priority_runs_before_low_when_all_threads_are_busy() {
        init(4);
        let n = threads();
        // Occupy every thread so the order below is decided by the queues.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..n {
            let gate = gate.clone();
            submit(
                Prio::High,
                Box::new(move || {
                    let (lock, cvar) = &*gate;
                    let mut open = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !*open {
                        open = cvar.wait(open).unwrap_or_else(|p| p.into_inner());
                    }
                }),
            );
        }
        wait_until("pool busy", || stats().running == n);
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (tx, rx) = mpsc::channel();
        {
            let order = order.clone();
            let tx = tx.clone();
            submit(
                Prio::Low,
                Box::new(move || {
                    order
                        .lock()
                        .map(|mut order| order.push("low"))
                        .ok();
                    let _ = tx.send(());
                }),
            );
        }
        {
            let order = order.clone();
            submit(
                Prio::High,
                Box::new(move || {
                    order
                        .lock()
                        .map(|mut order| order.push("high"))
                        .ok();
                }),
            );
        }
        {
            let (lock, cvar) = &*gate;
            *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
            cvar.notify_all();
        }
        rx.recv_timeout(Duration::from_secs(10))
            .expect("low job ran");
        wait_until("high job", || {
            order
                .lock()
                .map(|order| order.contains(&"high"))
                .unwrap_or(false)
        });
        let order = order.lock().map(|order| order.clone()).unwrap_or_default();
        assert_eq!(order, vec!["high", "low"], "High is served first");
    }
}
