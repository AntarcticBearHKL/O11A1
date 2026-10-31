//! f211: the 音画同步 instrument.
//!
//! One sampler, ten times a second, for the whole life of a player window:
//!
//! ```text
//! avsync tag=x phase=play presented_ms=12345 audio_ms=12351 delta_ms=6 paused=0
//! ```
//!
//! `presented_ms` is the position of the picture that is **on screen** (the
//! `pts` of the frame the pacer last handed over — the same number the progress
//! bar shows), and `audio_ms` is the position `ffplay` last reported. Their
//! difference is the only 音画同步 number this player has: positive means the
//! sound is ahead of the picture, negative means it trails.
//!
//! The sampler is a plain thread, not a pacer hook: it reads the two published
//! atomics the UI already reads, so what it measures is what a user sees, and it
//! cannot perturb the frame path it is measuring.
//!
//! A phase is a 3 s window after something that resets the relation. The player
//! marks it (`avsync_phase("seek")` at a seek's first frame, `"switch"` at a
//! quality switch's, `"resume"` when a pause is released); the window then
//! expires back to `play` on its own, so a sample is never labelled with an
//! event that happened minutes ago.
//!
//! `BILI_AVSYNC_TEST=<tag>` turns it on; the tag lands in every line so two
//! runs of one script can be told apart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::player::{note_thread_role, PlaybackControl};

/// How long a marked phase owns the samples that follow it.
const PHASE_WINDOW_MS: u64 = 3_000;

/// Where playback is in the A/V story, and when it got there.
static PHASE: AtomicU64 = AtomicU64::new(u64::MAX);
static PHASE_AT: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

const PHASE_PLAY: u64 = 0;
const PHASE_SEEK: u64 = 1;
const PHASE_SWITCH: u64 = 2;
const PHASE_RESUME: u64 = 3;

fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn phase_name(code: u64) -> &'static str {
    match code {
        PHASE_SEEK => "seek",
        PHASE_SWITCH => "switch",
        PHASE_RESUME => "resume",
        _ => "play",
    }
}

/// Open the sampler for one player window (`BILI_AVSYNC_TEST=<tag>`).
///
/// Called from the player's `run`, next to the other scripted hooks: it lives
/// exactly as long as the player session that spawned it.
pub(crate) fn spawn_avsync(control: Arc<PlaybackControl>) {
    let tag = crate::qa::env::var("BILI_AVSYNC_TEST").unwrap_or_default();
    if tag.is_empty() {
        return;
    }
    let _ = EPOCH.set(Instant::now());
    PHASE.store(PHASE_PLAY, Ordering::SeqCst);
    PHASE_AT.store(now_ms(), Ordering::SeqCst);
    std::thread::spawn(move || {
        note_thread_role("avsync");
        let started = Instant::now();
        let mut last: Option<Instant> = None;
        // A release leaves the picture frozen on the frame the pause showed
        // while the sound starts again at once, so the samples at the head of
        // the `resume` phase measure the *release ordering* (the sound leads the
        // first frame the pacer presents, 49-151 ms measured), not playback.
        // The baseline is therefore the first frame after the release: the
        // frozen samples and that frame itself are dropped (at most four) and
        // the drop is logged with the delta it carried, so nothing is hidden.
        let mut resume: Option<(u64, u32)> = None;
        let mut resume_done = false;
        // A closed player ends the sampler: it must never outlive what it
        // measures.
        while !control.is_closed() {
            if last.map_or(true, |at| at.elapsed() >= Duration::from_millis(100)) {
                last = Some(Instant::now());
                let presented = control.presented_position_ms();
                if presented != 0 {
                    let at = PHASE_AT.load(Ordering::SeqCst);
                    let marked = PHASE.load(Ordering::SeqCst);
                    let fresh = now_ms().saturating_sub(at) <= PHASE_WINDOW_MS;
                    let phase = if fresh && marked != u64::MAX {
                        phase_name(marked)
                    } else {
                        "play"
                    };
                    if phase != "resume" {
                        resume = None;
                        resume_done = false;
                    } else if !resume_done {
                        let audio = control.audio_clock();
                        let delta = audio.map(|ms| ms as i64 - presented as i64);
                        match resume {
                            None => {
                                // The picture on the pause: the baseline.
                                resume = Some((presented, 0));
                                crate::debug_log(&format!(
                                    "avsync tag={tag} resume baseline reset presented_ms={presented} dropped_delta_ms={} (frozen frame)",
                                    delta
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string())
                                ));
                            }
                            Some((base, frozen)) if presented == base && frozen < 2 => {
                                resume = Some((base, frozen + 1));
                            }
                            Some(_) => {
                                // The first frame the pacer presents after the
                                // release: the sound led it by the release's own
                                // ordering cost. It becomes the new baseline.
                                resume_done = true;
                                crate::debug_log(&format!(
                                    "avsync tag={tag} resume baseline reset presented_ms={presented} dropped_delta_ms={} (first frame)",
                                    delta
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string())
                                ));
                            }
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    let audio = control.audio_clock();
                    let paused = control.paused() || control.occluded();
                    crate::debug_log(&format!(
                        "avsync tag={tag} t={} phase={phase} presented_ms={presented} audio_ms={} delta_ms={} paused={}",
                        started.elapsed().as_millis(),
                        audio
                            .map(|ms| ms.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        audio
                            .map(|ms| (ms as i64 - presented as i64).to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        u8::from(paused)
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

/// Mark the phase the next [`PHASE_WINDOW_MS`] worth of samples belong to.
pub(crate) fn avsync_phase(label: &str) {
    if PHASE.load(Ordering::SeqCst) == u64::MAX {
        return;
    }
    let code = match label {
        "seek" => PHASE_SEEK,
        "switch" => PHASE_SWITCH,
        "resume" => PHASE_RESUME,
        _ => PHASE_PLAY,
    };
    PHASE.store(code, Ordering::SeqCst);
    PHASE_AT.store(now_ms(), Ordering::SeqCst);
}
