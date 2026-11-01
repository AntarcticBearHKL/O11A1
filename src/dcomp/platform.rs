//! The one `Platform` the process installs: winit unless the player asks.
//!
//! Everything here is a forward to [`i_slint_backend_winit::Backend`] - event
//! loop, the clock timers and animations run on, clipboard, URL handler, the
//! font-size default it binds on Windows - except
//! [`Platform::create_window_adapter`], which is where the player window leaves
//! winit.

use std::cell::Cell;
use std::rc::Rc;

use slint::platform::{Platform, PlatformError, WindowAdapter};

thread_local! {
    /// True only while the player component is being instantiated.
    ///
    /// `create_window_adapter` cannot know which window is being built - the
    /// call carries no identity - so the caller brackets the one constructor
    /// that must not reach winit with [`with_player_scope`].
    static PLAYER_SCOPE: Cell<bool> = const { Cell::new(false) };
    /// f196: the adapter a *buffer* component (the danmaku layer, drawn into its
    /// own composition swapchain) must be bound to. Same mechanism as
    /// `PLAYER_SCOPE`, because a component only asks the platform for a window
    /// adapter on its first `window()`/`show()` - see
    /// [`with_buffer_scope`], which holds the adapter across exactly that call.
    static BUFFER_SCOPE: std::cell::RefCell<Option<Rc<dyn WindowAdapter>>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether a player adapter must be built for the current
/// `create_window_adapter` call.
///
/// Read by [`HybridPlatform`]; set only by [`with_player_scope`].
pub(crate) fn player_scope_active() -> bool {
    PLAYER_SCOPE.with(|flag| flag.get())
}

/// Instantiate the player component inside the window-selection scope.
///
/// The guard is dropped on unwind as well, so a failing constructor cannot
/// leave the next window (the mini player, a toast) pointed at the DComp path.
pub(crate) fn with_player_scope<T>(body: impl FnOnce() -> T) -> T {
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.0 {
                PLAYER_SCOPE.with(|flag| flag.set(false));
            }
        }
    }
    let previous = PLAYER_SCOPE.with(|flag| flag.replace(true));
    let _guard = Guard(!previous);
    body()
}

/// Instantiate a component into a caller-owned (buffer) window adapter.
///
/// f196: the danmaku layer is a second Slint component in the same context, but
/// it must NOT have an OS window: it renders into a composition swapchain. The
/// component asks the platform for an adapter on its first `window()` call, so
/// the adapter is armed for the duration of `body` and taken by the first
/// `create_window_adapter`.
pub(crate) fn with_buffer_scope<T>(adapter: Rc<dyn WindowAdapter>, body: impl FnOnce() -> T) -> T {
    let previous = BUFFER_SCOPE.with(|slot| slot.replace(Some(adapter)));
    struct Guard(Option<Rc<dyn WindowAdapter>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            BUFFER_SCOPE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _guard = Guard(previous);
    body()
}

///
/// f214: the path is **not a preference any more** — it is always on, and the
/// only way it is turned off is the escape valve `BILI_PLAYER_DCOMP=0` (the QA
/// scripts' A/B, and the one command a user can run to get the old window back
/// if the compositor path ever misbehaves on their machine). In particular this
/// no longer reads `settings.json` → `player_dcomp`: that field could only be
/// applied by the *next* process (the window class is chosen at start-up), so
/// the switch meant "restart to find out", and a write that did not land looked
/// exactly like one that did. See [`crate::settings::Settings::player_dcomp`].
///
/// Evaluated once per process: the player window class is chosen when the
/// platform is installed. The answer is logged once ([`resolve`]) with the
/// reason, so a run can be read back without guessing.
pub(crate) fn enabled() -> bool {
    resolve().0
}

/// [`enabled`] plus the reason it answered that way, for the startup log.
fn resolve() -> (bool, &'static str) {
    static ANSWER: std::sync::OnceLock<(bool, &'static str)> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| match std::env::var("BILI_PLAYER_DCOMP") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => (false, "BILI_PLAYER_DCOMP"),
            "1" | "true" | "yes" | "on" => (true, "BILI_PLAYER_DCOMP"),
            // An empty/unrecognised value is not a request to change anything:
            // the path stays on, and the odd value is worth a log line.
            _ => (true, "default"),
        },
        Err(_) => (true, "default"),
    })
}

/// The installed platform: the winit backend, plus the player branch.
pub(crate) struct HybridPlatform {
    winit: i_slint_backend_winit::Backend,
}

impl Platform for HybridPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // f196: a buffer component (the danmaku layer) named its own adapter.
        if let Some(adapter) = BUFFER_SCOPE.with(|slot| slot.borrow_mut().take()) {
            return Ok(adapter);
        }
        if player_scope_active() && enabled() {
            // The player window: our HWND, our composition tree, our input.
            match super::player_window::DcompPlayerWindow::create() {
                Ok(window) => return Ok(window as Rc<dyn WindowAdapter>),
                Err(error) => {
                    // Never fatal: the runtime fallback is the pre-f194 winit
                    // window, built by the same call the next line makes.
                    crate::debug_log(&format!(
                        "dcomp platform player fallback reason={error}"
                    ));
                }
            }
        }
        self.winit.create_window_adapter()
    }

    fn bind_context(
        &self,
        context: i_slint_core::SlintContextWeak,
        token: i_slint_core::InternalToken,
    ) {
        // Load-bearing: the winit backend stores the context here and its event
        // loop ticks Slint's timers and animations through it; on Windows it
        // also seeds the platform default font size.
        Platform::bind_context(&self.winit, context, token)
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        self.winit.run_event_loop()
    }

    fn process_events(
        &self,
        timeout: Option<core::time::Duration>,
        token: i_slint_core::InternalToken,
    ) -> Result<core::ops::ControlFlow<()>, PlatformError> {
        self.winit.process_events(timeout, token)
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        self.winit.new_event_loop_proxy()
    }

    fn duration_since_start(&self) -> core::time::Duration {
        Platform::duration_since_start(&self.winit)
    }

    fn click_interval(&self) -> core::time::Duration {
        self.winit.click_interval()
    }

    fn cursor_flash_cycle(&self) -> core::time::Duration {
        self.winit.cursor_flash_cycle()
    }

    fn set_clipboard_text(&self, text: &str, clipboard: slint::platform::Clipboard) {
        self.winit.set_clipboard_text(text, clipboard)
    }

    fn clipboard_text(&self, clipboard: slint::platform::Clipboard) -> Option<String> {
        self.winit.clipboard_text(clipboard)
    }

    fn debug_log(&self, arguments: core::fmt::Arguments) {
        self.winit.debug_log(arguments)
    }

    fn open_url(&self, url: &str) -> Result<(), PlatformError> {
        self.winit.open_url(url)
    }
}

/// Build the winit backend the way Slint's own selector would have.
///
/// `SLINT_BACKEND` keeps its meaning (`winit-femtovg`, `winit-software`, ...):
/// the process now installs the platform itself, so the parsing the selector
/// used to do has to happen here or the escape hatches would be silently dead.
/// An empty or unknown value means "winit default", which is femtovg-on-wgpu in
/// this build (see `Cargo.toml`).
fn winit_backend() -> Result<i_slint_backend_winit::Backend, PlatformError> {
    let requested = std::env::var("SLINT_BACKEND").unwrap_or_default();
    let requested = requested.trim().to_ascii_lowercase();
    let renderer = if requested.is_empty() || requested == "winit" {
        None
    } else if let Some(rest) = requested.strip_prefix("winit-") {
        Some(rest)
    } else if requested == "femtovg" {
        // The selector's aliases (`femtovg`, `sw`) name a winit renderer.
        Some("femtovg")
    } else if matches!(requested.as_str(), "sw" | "software") {
        Some("software")
    } else {
        // Not a winit request (`qt`, `testing`, `linuxkms`, ...): this process
        // only ships the winit backend, and the player's composition path is
        // Win32-only. Leave the default alone.
        crate::debug_log(&format!(
            "dcomp platform SLINT_BACKEND={requested} is not a winit backend; using the default"
        ));
        None
    };
    match renderer {
        Some(name) => match i_slint_backend_winit::Backend::new_with_renderer_by_name(Some(name)) {
            Ok(backend) => Ok(backend),
            Err(error) => {
                crate::debug_log(&format!(
                    "dcomp platform renderer={name} unavailable ({error}); using the default"
                ));
                i_slint_backend_winit::Backend::new()
            }
        },
        None => i_slint_backend_winit::Backend::new(),
    }
}

/// Install the hybrid platform. Must run before the first Slint context exists.
///
/// `BILI_HYBRID_PLATFORM=0` skips the install entirely, leaving Slint's own
/// backend selector in charge. That is the A/B control for this module: with the
/// variable set, the process is the pre-f194 one (winit, same renderer), which
/// is how "the main window did not change" is proven rather than asserted.
pub(crate) fn install_platform() -> Result<(), String> {
    if std::env::var("BILI_HYBRID_PLATFORM").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    }) {
        crate::debug_log("dcomp platform skipped (BILI_HYBRID_PLATFORM=0): Slint's selector");
        return Ok(());
    }
    let winit = winit_backend().map_err(|error| error.to_string())?;
    let (path, reason) = resolve();
    crate::debug_log(&format!(
        "dcomp platform installed player_path={} reason={reason}",
        if path { "dcomp" } else { "winit" }
    ));
    slint::platform::set_platform(Box::new(HybridPlatform { winit }))
        .map_err(|error| error.to_string())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The flag must be scoped: set inside, clear outside, and clear after a
    /// panic in the constructor (a failed player must not capture the next
    /// window).
    #[test]
    fn player_scope_is_scoped_and_unwinds() {
        assert!(!player_scope_active());
        assert!(with_player_scope(player_scope_active));
        assert!(!player_scope_active());

        let unwound = std::panic::catch_unwind(|| with_player_scope(|| panic!("boom")));
        assert!(unwound.is_err());
        assert!(!player_scope_active());
    }

    /// Nesting must not clear the outer scope when the inner guard drops.
    #[test]
    fn player_scope_nests() {
        with_player_scope(|| {
            with_player_scope(|| {});
            assert!(player_scope_active());
        });
        assert!(!player_scope_active());
    }
}
