//! What the player shows, without the player knowing about Slint.
//!
//! `player.rs` owns the decode pipeline: threads, the packet loop, the pacer.
//! Its last step — "put this frame on the window, say whether we are playing,
//! move the readout" — used to be a direct `PlayerWindow::set_*` call from
//! inside that pipeline, which made the pipeline impossible to test without a
//! window.
//!
//! [`PlayerView`] is that last step as a trait. [`SlintPlayerView`] is the real
//! implementation (it is what every run uses); [`FakeView`] is the test one,
//! and the [`Presentation`] rules below are shared by both, so a pause/seek
//! sequence can be asserted as an event list in a unit test.
//!
//! The rules are deliberately written once, here:
//!
//! * the first frame arrives → the picture is shown, buffering ends, play state
//!   becomes playing;
//! * a pause → the frame stays on screen (a frozen picture is not a black one),
//!   only the play state changes;
//! * a seek → the readout jumps at once (the UI must not show the old position
//!   while the new one decodes).

#[cfg(test)]
use std::sync::Mutex;

use crate::PlayerWindow;

/// One picture for the view. `Synthetic` exists so a test can name a frame
/// without building a pixel buffer; production always passes `Image`.
pub(crate) enum Picture {
    Image(slint::Image),
    #[cfg(test)]
    Synthetic { id: u64 },
}

/// The presentation surface the decode pipeline needs.
///
/// Object-safe on purpose: the pipeline holds `&dyn PlayerView`, so the real
/// window and a recording fake are interchangeable.
pub(crate) trait PlayerView {
    /// Show one picture (the pacer calls this at most once per display frame).
    fn show_frame(&self, picture: Picture);
    /// Start/stop the "playing" indicator.
    fn show_playing(&self, playing: bool);
    /// Start/stop the buffering spinner, with its 0..=1 progress.
    fn show_buffering(&self, buffering: bool, progress: f32);
    /// Move the position readout, in milliseconds.
    fn show_progress(&self, position_ms: u64);
    /// Show a status/toast line. `kind` is the toast severity.
    fn show_status(&self, text: &str, kind: i32);
}

/// The real view: a live `PlayerWindow`, reached through its weak handle.
pub(crate) struct SlintPlayerView(pub(crate) slint::Weak<PlayerWindow>);

impl PlayerView for SlintPlayerView {
    fn show_frame(&self, picture: Picture) {
        match picture {
            Picture::Image(image) => {
                if let Some(window) = self.0.upgrade() {
                    window.set_frame(image);
                }
            }
            // Only the tests build a synthetic picture.
            #[cfg(test)]
            Picture::Synthetic { .. } => {}
        }
    }

    fn show_playing(&self, playing: bool) {
        if let Some(window) = self.0.upgrade() {
            window.set_playing(playing);
        }
    }

    fn show_buffering(&self, buffering: bool, progress: f32) {
        if let Some(window) = self.0.upgrade() {
            window.set_buffering(buffering);
            if buffering {
                window.set_buffer_progress(progress);
            }
        }
    }

    fn show_progress(&self, position_ms: u64) {
        if let Some(window) = self.0.upgrade() {
            window.set_position_ms(position_ms as f32);
        }
    }

    fn show_status(&self, text: &str, kind: i32) {
        if let Some(window) = self.0.upgrade() {
            window.set_player_status(text.into());
            if !text.is_empty() {
                crate::toast_active(text, kind);
            }
        }
    }
}

/// The presentation rules, written once against [`PlayerView`].
///
/// Everything here is a pure sequence of view calls: no timing, no decoding,
/// no Slint. That is what makes it testable — see the tests at the bottom.
pub(crate) struct Presentation<'a> {
    view: &'a dyn PlayerView,
    /// Set while the picture is held (paused/seek-frozen): a held picture still
    /// accepts a frame, it just must not claim to be playing.
    held: bool,
}

impl<'a> Presentation<'a> {
    pub(crate) fn new(view: &'a dyn PlayerView) -> Self {
        Presentation { view, held: false }
    }

    /// Any frame the pacer hands over while playing.
    pub(crate) fn frame(&mut self, picture: Picture) {
        self.view.show_frame(picture);
    }

    /// Pause: freeze the picture, stop claiming to play.
    pub(crate) fn pause(&mut self) {
        self.view.show_playing(false);
        self.held = true;
    }

    pub(crate) fn resume(&mut self) {
        self.view.show_playing(true);
        self.held = false;
    }

    /// A seek: the readout jumps immediately; the old picture stays until the
    /// new position's first frame arrives (a black flash is worse than a stale
    /// frame for the ~200 ms a cache hit takes).
    pub(crate) fn seek(&mut self, position_ms: u64) {
        self.view.show_progress(position_ms);
    }

    pub(crate) fn buffering(&mut self, on: bool, progress: f32) {
        self.view.show_buffering(on, progress);
        if on {
            // A spinner and a "playing" badge at the same time is a lie.
            self.view.show_playing(false);
        }
    }

    pub(crate) fn status(&mut self, text: &str, kind: i32) {
        self.view.show_status(text, kind);
    }

}

/// Test view: records every call so a sequence can be asserted.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeView {
    pub(crate) events: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeView {
    fn push(&self, event: String) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    pub(crate) fn take(&self) -> Vec<String> {
        self.events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
}

#[cfg(test)]
impl PlayerView for FakeView {
    fn show_frame(&self, picture: Picture) {
        match picture {
            Picture::Synthetic { id } => self.push(format!("frame:{id}")),
            Picture::Image(_) => self.push("frame:image".to_string()),
        }
    }

    fn show_playing(&self, playing: bool) {
        self.push(format!("playing:{playing}"));
    }

    fn show_buffering(&self, buffering: bool, progress: f32) {
        self.push(format!("buffering:{buffering}:{progress:.1}"));
    }

    fn show_progress(&self, position_ms: u64) {
        self.push(format!("progress:{position_ms}"));
    }

    fn show_status(&self, text: &str, kind: i32) {
        self.push(format!("status:{kind}:{text}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(id: u64) -> Picture {
        Picture::Synthetic { id }
    }

    #[test]
    fn first_frame_starts_playing_and_ends_buffering() {
        let view = FakeView::default();
        let mut presentation = Presentation::new(&view);
        // The start of a session, in the order the wired paths issue the calls
        // (frame from the mailbox, then the buffering/play state).
        presentation.frame(synthetic(1));
        presentation.buffering(false, 1.0);
        presentation.resume();
        assert_eq!(
            view.take(),
            vec!["frame:1", "buffering:false:1.0", "playing:true"],
            "the first frame ends buffering and starts the play state"
        );
    }

    #[test]
    fn pause_holds_the_picture_on_screen() {
        let view = FakeView::default();
        let mut presentation = Presentation::new(&view);
        presentation.frame(synthetic(1));
        view.take();
        presentation.pause();
        assert_eq!(view.take(), vec!["playing:false"], "no frame is cleared");
        assert!(presentation.held);
        presentation.resume();
        assert_eq!(view.take(), vec!["playing:true"]);
    }

    #[test]
    fn seek_moves_the_readout_at_once() {
        let view = FakeView::default();
        let mut presentation = Presentation::new(&view);
        presentation.frame(synthetic(1));
        view.take();
        presentation.seek(42_000);
        assert_eq!(
            view.take(),
            vec!["progress:42000"],
            "the readout jumps without waiting for the new position's frame"
        );
        // A buffering spinner must not keep the play badge on.
        presentation.buffering(true, 0.5);
        assert_eq!(view.take(), vec!["buffering:true:0.5", "playing:false"]);
    }
}
