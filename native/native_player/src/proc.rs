//! Child-process plumbing shared by the video decoder, the audio decoder and
//! the one-shot ffprobe.

use std::path::PathBuf;
use std::process::Command;

use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// The directory holding `ffmpeg.exe` / `ffprobe.exe` (the same lookup the
/// shipped app uses: `BILI_FFMPEG_DIR`, then packaged folders, then the
/// developer machine's unpacked build).
pub fn ffmpeg_dir() -> PathBuf {
    o11a1::paths::ffmpeg_dir()
}

pub fn command(exe: &str) -> Command {
    let dir = ffmpeg_dir();
    let mut command = Command::new(dir.join(exe));
    if let Some(existing) = std::env::var_os("PATH") {
        let mut path = dir.into_os_string();
        path.push(";");
        path.push(existing);
        command.env("PATH", path);
    }
    use std::os::windows::process::CommandExt;
    // A console window for every decoder would steal focus and flash on open;
    // stdout/stderr are already redirected.
    command.creation_flags(CREATE_NO_WINDOW.0);
    command
}

/// A URL is remote unless it names an existing local file or a plain path.
pub fn is_local(url: &str) -> bool {
    !url.contains("://") || url.starts_with("file://")
}

/// ffmpeg's `-headers` syntax: one `Name: value` per line, CRLF-terminated.
/// The CDN rejects a bare request, so the UA/Referer/Cookie matter.
pub fn header_string(headers: &[(String, String)]) -> String {
    let mut text = String::new();
    for (name, value) in headers {
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value);
        text.push_str("\r\n");
    }
    text
}

/// Reconnect policy for every remote input: a dropped CDN connection resumes
/// instead of ending playback. No `-reconnect_at_eof` (it would restart a VOD).
pub fn network_args(command: &mut Command) {
    command
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-rw_timeout")
        .arg("15000000");
}

/// ffmpeg's own input-seek time format (seconds with millisecond precision).
pub fn seconds(ms: u64) -> String {
    format!("{:.3}", ms as f64 / 1000.0)
}
