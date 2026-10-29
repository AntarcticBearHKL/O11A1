//! The media pipeline's diagnostic hooks, owned by nobody in particular.
//!
//! `prefetch` (the byte pipeline) needs to report *which* worker thread is
//! doing what, and `player` (the decode pipeline) is what knows the answer:
//! it keeps the thread-role table and the stall trace. Before this module the
//! two crates-in-one referred to each other (`prefetch` called
//! [`crate::player::note_thread_role`] and [`crate::player::tag_here`] while
//! `player` called [`crate::prefetch::startup_gate_enabled`] and
//! [`crate::prefetch::disk_queue_depth`]), which made either module impossible
//! to read on its own.
//!
//! Now the only thing both sides know is this trait. `main` installs the real
//! implementation once, at startup, before any worker thread exists
//! ([`install`]); everything else goes through the free functions below.
//!
//! The defaults are deliberately observable-but-inert: with nothing installed
//! a role/tag is dropped on the floor, the queue reports empty and the startup
//! gate reports *enabled* — which is exactly what `prefetch` itself defaults
//! to, so a test that never installs hooks still sees the shipping behaviour.

use std::sync::OnceLock;

/// The four things the byte pipeline asks of the rest of the app.
///
/// Every method is cheap and must stay non-blocking: `note_thread_role` runs
/// once per worker thread, `tag_here` runs on hot paths (a paused trace can
/// call it thousands of times a second), and the two getters are read from
/// packet loops.
pub trait MediaHooks: Send + Sync {
    /// Record what a worker thread is for (`"disk-writer"`, `"pacer"`, …).
    fn note_thread_role(&self, role: &'static str);

    /// Record the step the calling thread is in, for the stall trace.
    fn tag_here(&self, tag: &'static str);

    /// How many blocks are queued for the cache-file writer (`0` = idle).
    fn disk_queue_depth(&self) -> u64;

    /// Whether the startup gate (hold background fetches until the first
    /// frame) is on. `BILI_STARTUP_GATE=0` turns it off.
    fn startup_gate_enabled(&self) -> bool;
}

/// Nothing installed: drop the diagnostics, keep the shipping defaults.
struct InertHooks;

impl MediaHooks for InertHooks {
    fn note_thread_role(&self, _role: &'static str) {}

    fn tag_here(&self, _tag: &'static str) {}

    fn disk_queue_depth(&self) -> u64 {
        0
    }

    fn startup_gate_enabled(&self) -> bool {
        true
    }
}

static HOOKS: OnceLock<&'static dyn MediaHooks> = OnceLock::new();
static INERT: InertHooks = InertHooks;

/// Install the real implementation. Called once from `main`, before any
/// pipeline thread starts; later calls are ignored (the first one wins) so a
/// second window or a reload cannot swap the diagnostics out from under a
/// running worker.
pub fn install(hooks: &'static dyn MediaHooks) {
    let _ = HOOKS.set(hooks);
}

fn hooks() -> &'static dyn MediaHooks {
    HOOKS.get().copied().unwrap_or(&INERT)
}

/// Record what the calling thread is for.
pub(crate) fn note_thread_role(role: &'static str) {
    hooks().note_thread_role(role);
}

/// Record the step the calling thread is in (stall/deadlock traces).
pub(crate) fn tag_here(tag: &'static str) {
    hooks().tag_here(tag);
}

/// Blocks queued for the cache-file writer.
pub(crate) fn disk_queue_depth() -> u64 {
    hooks().disk_queue_depth()
}

/// Whether the startup gate is enabled.
pub(crate) fn startup_gate_enabled() -> bool {
    hooks().startup_gate_enabled()
}
