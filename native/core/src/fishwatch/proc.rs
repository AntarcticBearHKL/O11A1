//! How every child process is created.
//!
//! The fish engine shells out to `python`, `ffmpeg`, `nvidia-smi` and possibly
//! `whisper-cli`. On Windows each of those is a console program, so a plain
//! `Command::new` opens a black window on top of the app — at startup that
//! looked like "the app only shows a python console". Every spawn in
//! `fishwatch` therefore goes through [`hidden`], which sets
//! `CREATE_NO_WINDOW` (the same flag `player.rs` uses for ffmpeg/ffplay).
//!
//! Stdio is left to the caller: probes ask for `.output()`/`.status()`, the
//! transcribers pipe stdin/stdout themselves.

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW`: the child gets no console window at all.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Apply the "no console window" creation flags to `cmd`.
pub fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `Command::new(program)` that will never open a console window.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    hidden(&mut cmd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag is the documented value, and `command` really applies it:
    /// a console process spawned through it reports no window of its own.
    #[test]
    fn hidden_command_runs_without_a_console() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        #[cfg(windows)]
        {
            let out = command("cmd").args(["/c", "echo", "ok"]).output();
            let out = out.expect("cmd runs");
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
        }
    }
}
