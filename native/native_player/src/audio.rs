//! Audio: a second ffmpeg child (`s16le`, 48 kHz, stereo) played through the
//! WinMM waveOut API.
//!
//! Four 20 ms buffers are kept in flight. `waveOutWrite` only queues; when all
//! four are busy the pump stops reading, the bounded channel fills, and the
//! ffmpeg pipe blocks — so a paused or slow presentation thread can never make
//! the player buffer an unbounded amount of sound. Volume is
//! `waveOutSetVolume`; pause/resume are `waveOutPause`/`waveOutResume`; a seek
//! resets the device and restarts the child at the target.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use windows::Win32::Media::Audio::{
    waveOutClose, waveOutGetPosition, waveOutOpen, waveOutPause, waveOutPrepareHeader,
    waveOutReset, waveOutRestart, waveOutSetVolume, waveOutWrite, HWAVEOUT, WAVEHDR,
    WAVE_FORMAT_PCM, WAVE_MAPPER,
};
use windows::Win32::Media::{MMTIME, TIME_SAMPLES};

use crate::log;
use crate::proc;

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: u16 = 2;
const CHUNK_MS: u64 = 20;
const CHUNK_BYTES: usize = (RATE as usize * CHUNK_MS as usize / 1000) * CHANNELS as usize * BYTES_PER_SAMPLE as usize;
const BUFFERS: usize = 4;
/// The reader may run this many chunks ahead (160 ms). Everything beyond is
/// backpressure into ffmpeg, which is exactly what we want.
const CHANNEL_CHUNKS: usize = 8;

/// Position/state the presentation thread reads.
#[derive(Default)]
pub struct AudioClock {
    pub position_ms: AtomicU64,
    pub ready: AtomicBool,
    pub playing: AtomicBool,
    pub available: AtomicBool,
    /// Milliseconds the device has had no data while it should be playing.
    pub starved_ms: AtomicU32,
    /// A device/open/child failure the engine should surface with a retry.
    pub failed: AtomicBool,
}

pub enum AudioCommand {
    Pause,
    Resume,
    Seek { target_ms: u64, speed: f64 },
    SetVolume(u8),
    Stop,
}

/// Handle to the audio pump thread. Dropping it stops the pump and both
/// children.
pub struct AudioEngine {
    tx: Sender<AudioCommand>,
    pub clock: Arc<AudioClock>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn spawn(
        url: String,
        headers: Vec<(String, String)>,
        remote: bool,
        start_ms: u64,
        speed: f64,
        volume: u8,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let clock = Arc::new(AudioClock::default());
        clock.position_ms.store(start_ms, Ordering::SeqCst);
        let thread_clock = clock.clone();
        let thread = std::thread::Builder::new()
            .name("player-audio".into())
            .spawn(move || audio_thread(rx, thread_clock, url, headers, remote, start_ms, speed, volume))
            .ok();
        Self { tx, clock, thread }
    }

    pub fn send(&self, command: AudioCommand) {
        let _ = self.tx.send(command);
    }

    pub fn stop(&mut self) {
        let _ = self.tx.send(AudioCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------- the pump

struct Session {
    child: Child,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
}

impl Session {
    fn spawn(
        url: &str,
        headers: &[(String, String)],
        remote: bool,
        target_ms: u64,
        speed: f64,
    ) -> Result<Self> {
        let mut command = proc::command("ffmpeg.exe");
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-threads")
            .arg("0")
            .arg("-probesize")
            .arg("500000")
            .arg("-analyzeduration")
            .arg("500000");
        if remote {
            proc::network_args(&mut command);
            let headers = proc::header_string(headers);
            if !headers.is_empty() {
                command.arg("-headers").arg(headers);
            }
        }
        if target_ms > 0 {
            command.arg("-ss").arg(proc::seconds(target_ms));
        }
        command.arg("-i").arg(url).arg("-vn").arg("-sn").arg("-dn");
        if (speed - 1.0).abs() > 0.001 {
            let rate = speed.clamp(0.5, 2.0);
            command.arg("-af").arg(format!("atempo={rate:.4}"));
        }
        command
            .arg("-f")
            .arg("s16le")
            .arg("-ac")
            .arg(CHANNELS.to_string())
            .arg("-ar")
            .arg(RATE.to_string())
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn ffmpeg audio for {url}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg audio stdout missing"))?;
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_CHUNKS);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("player-audio-reader".into())
                .spawn(move || read_audio(stdout, tx, stop))
                .context("spawn audio reader")?
        };
        let stderr = stderr.map(|stderr| {
            std::thread::Builder::new()
                .name("player-audio-err".into())
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    let mut tail: VecDeque<String> = VecDeque::new();
                    for line in reader.lines().map_while(Result::ok) {
                        let text = line.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        tail.push_back(text);
                        if tail.len() > 6 {
                            tail.pop_front();
                        }
                    }
                    if !tail.is_empty() {
                        log::line(&format!(
                            "audio stderr [{}]",
                            tail.into_iter().collect::<Vec<_>>().join(" | ")
                        ));
                    }
                })
                .ok()
        });
        Ok(Self {
            child,
            reader: Some(reader),
            stderr: stderr.flatten(),
            rx,
            stop,
        })
    }

    fn stop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_audio(mut stdout: ChildStdout, tx: SyncSender<Vec<u8>>, stop: Arc<AtomicBool>) {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if stdout.read_exact(&mut buffer).is_err() {
            return;
        }
        let mut chunk = buffer.clone();
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match tx.try_send(chunk) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    chunk = returned;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn audio_thread(
    commands: Receiver<AudioCommand>,
    clock: Arc<AudioClock>,
    url: String,
    headers: Vec<(String, String)>,
    remote: bool,
    start_ms: u64,
    mut speed: f64,
    mut volume: u8,
) {
    let format = windows::Win32::Media::Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * BYTES_PER_SAMPLE as u32,
        nBlockAlign: CHANNELS * BYTES_PER_SAMPLE,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let mut handle = HWAVEOUT::default();
    let opened = unsafe {
        waveOutOpen(
            Some(&mut handle),
            WAVE_MAPPER,
            &format,
            None,
            None,
            windows::Win32::Media::Audio::CALLBACK_NULL,
        )
    };
    if opened != 0 {
        clock.available.store(false, Ordering::SeqCst);
        clock.failed.store(true, Ordering::SeqCst);
        log::line(&format!("audio unavailable waveOutOpen mmr={opened}"));
        // Keep consuming commands so senders never block; nothing can play.
        while let Ok(command) = commands.recv() {
            if matches!(command, AudioCommand::Stop) {
                break;
            }
        }
        return;
    }
    clock.available.store(true, Ordering::SeqCst);
    unsafe {
        waveOutSetVolume(Some(handle), packed_volume(volume));
    }

    // Four prepared 20 ms buffers. The headers live in a Box so their
    // addresses never move after `waveOutPrepareHeader`.
    let mut buffers: Vec<Box<[u8]>> = (0..BUFFERS)
        .map(|_| vec![0u8; CHUNK_BYTES].into_boxed_slice())
        .collect();
    let mut wave_headers: Box<[WAVEHDR]> = (0..BUFFERS)
        .map(|_| WAVEHDR::default())
        .collect::<Vec<_>>()
        .into_boxed_slice();
    unsafe {
        for index in 0..BUFFERS {
            let header = &mut wave_headers[index];
            header.lpData = windows::core::PSTR(buffers[index].as_mut_ptr());
            header.dwBufferLength = CHUNK_BYTES as u32;
            let result = waveOutPrepareHeader(handle, header, std::mem::size_of::<WAVEHDR>() as u32);
            if result != 0 {
                log::line(&format!("audio prepare failed index={index} mmr={result}"));
                clock.available.store(false, Ordering::SeqCst);
                clock.failed.store(true, Ordering::SeqCst);
                let _ = waveOutClose(handle);
                return;
            }
            // Mark free until the first waveOutWrite.
            header.dwFlags |= windows::Win32::Media::Audio::WHDR_DONE;
        }
    }

    let mut session = match Session::spawn(&url, &headers, remote, start_ms, speed) {
        Ok(session) => Some(session),
        Err(error) => {
            log::line(&format!("audio session failed: {error:#}"));
            clock.failed.store(true, Ordering::SeqCst);
            None
        }
    };
    let mut base_ms = start_ms;
    let mut paused = false;
    let mut ready = false;
    let mut logged_started = false;
    let mut starved_since: Option<std::time::Instant> = None;
    let mut last_position = start_ms;
    let mut last_poll = std::time::Instant::now();
    clock.position_ms.store(start_ms, Ordering::SeqCst);

    loop {
        match commands.recv_timeout(Duration::from_millis(2)) {
            Ok(AudioCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(AudioCommand::Pause) => {
                if !paused {
                    paused = true;
                    unsafe {
                        let _ = waveOutPause(handle);
                    }
                    clock.playing.store(false, Ordering::SeqCst);
                }
            }
            Ok(AudioCommand::Resume) => {
                if paused {
                    paused = false;
                    unsafe {
                        let _ = waveOutRestart(handle);
                    }
                    clock.playing.store(ready, Ordering::SeqCst);
                }
            }
            Ok(AudioCommand::SetVolume(value)) => {
                volume = value.min(100);
                unsafe {
                    let _ = waveOutSetVolume(Some(handle), packed_volume(volume));
                }
            }
            Ok(AudioCommand::Seek { target_ms, speed: new_speed }) => {
                speed = new_speed.clamp(0.5, 2.0);
                base_ms = target_ms;
                restart_session(
                    &mut session,
                    &url,
                    &headers,
                    remote,
                    &mut base_ms,
                    speed,
                    &mut ready,
                    &mut logged_started,
                    &mut starved_since,
                    &mut last_position,
                    paused,
                    handle,
                    &mut wave_headers,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Fill every free buffer from the bounded reader channel. The channel
        // is the backpressure: when it is empty we simply wait for the next
        // command/timeout tick.
        if let Some(current) = session.as_mut() {
            for index in 0..BUFFERS {
                let is_free = wave_headers[index].dwFlags & windows::Win32::Media::Audio::WHDR_DONE != 0;
                if !is_free {
                    continue;
                }
                let Some(chunk) = take_chunk(&current.rx) else {
                    break;
                };
                if chunk.len() != CHUNK_BYTES {
                    continue;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        chunk.as_ptr(),
                        buffers[index].as_mut_ptr(),
                        CHUNK_BYTES,
                    );
                    wave_headers[index].dwBufferLength = CHUNK_BYTES as u32;
                    wave_headers[index].dwFlags &= !windows::Win32::Media::Audio::WHDR_DONE;
                    let result = waveOutWrite(
                        handle,
                        &mut wave_headers[index],
                        std::mem::size_of::<WAVEHDR>() as u32,
                    );
                    if result != 0 {
                        log::line(&format!("audio write failed index={index} mmr={result}"));
                        wave_headers[index].dwFlags |= windows::Win32::Media::Audio::WHDR_DONE;
                        continue;
                    }
                }
                if !ready {
                    ready = true;
                    clock.ready.store(true, Ordering::SeqCst);
                    clock.playing.store(!paused, Ordering::SeqCst);
                    if !logged_started {
                        logged_started = true;
                        log::line(&format!(
                            "audio started target_ms={base_ms} pid={} speed={speed:.2}",
                            current.child.id()
                        ));
                    }
                }
            }
        }

        // Device position, in output samples since the reset. With atempo the
        // media timeline advances `speed` times faster than output seconds.
        if last_poll.elapsed() >= Duration::from_millis(20) {
            last_poll = std::time::Instant::now();
            let mut time = MMTIME {
                wType: TIME_SAMPLES,
                ..Default::default()
            };
            let result = unsafe {
                waveOutGetPosition(handle, &mut time, std::mem::size_of::<MMTIME>() as u32)
            };
            if result == 0 && time.wType & TIME_SAMPLES != 0 {
                let samples = unsafe { time.u.sample } as u64;
                let media_ms = (samples as f64 * 1000.0 * speed / RATE as f64).round() as u64;
                last_position = base_ms.saturating_add(media_ms);
                clock.position_ms.store(last_position, Ordering::SeqCst);
            }
            // Starvation: playing, ready, and no buffer in the device queue.
            let busy = (0..BUFFERS).any(|index| {
                wave_headers[index].dwFlags & windows::Win32::Media::Audio::WHDR_INQUEUE != 0
            });
            if !paused && ready && !busy {
                let since = *starved_since.get_or_insert_with(std::time::Instant::now);
                clock
                    .starved_ms
                    .store(since.elapsed().as_millis() as u32, Ordering::SeqCst);
            } else {
                starved_since = None;
                clock.starved_ms.store(0, Ordering::SeqCst);
            }
        }
    }

    // Shutdown: stop the child first, then the device.
    drop(session.take());
    unsafe {
        let _ = waveOutReset(handle);
        let _ = waveOutClose(handle);
    }
    clock.available.store(false, Ordering::SeqCst);
    clock.playing.store(false, Ordering::SeqCst);
    log::line("audio stopped");
}

/// Restart the decode child at `base_ms`, reset the device and its headers.
#[allow(clippy::too_many_arguments)]
fn restart_session(
    session: &mut Option<Session>,
    url: &str,
    headers: &[(String, String)],
    remote: bool,
    base_ms: &mut u64,
    speed: f64,
    ready: &mut bool,
    logged_started: &mut bool,
    starved_since: &mut Option<std::time::Instant>,
    last_position: &mut u64,
    paused: bool,
    handle: HWAVEOUT,
    headers_out: &mut [WAVEHDR],
) {
    drop(session.take());
    unsafe {
        let _ = waveOutReset(handle);
    }
    for header in headers_out.iter_mut() {
        header.dwFlags |= windows::Win32::Media::Audio::WHDR_DONE;
    }
    *ready = false;
    *logged_started = false;
    *starved_since = None;
    *last_position = *base_ms;
    if paused {
        unsafe {
            let _ = waveOutPause(handle);
        }
    }
    match Session::spawn(url, headers, remote, *base_ms, speed) {
        Ok(new_session) => *session = Some(new_session),
        Err(error) => {
            log::line(&format!("audio seek failed target_ms={} err={error:#}", *base_ms));
            *session = None;
        }
    }
}

fn take_chunk(rx: &Receiver<Vec<u8>>) -> Option<Vec<u8>> {
    rx.try_recv().ok()
}

fn packed_volume(volume: u8) -> u32 {
    let value = (volume.min(100) as u32 * 0xFFFF / 100) & 0xFFFF;
    value | (value << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_packed_for_both_channels() {
        assert_eq!(packed_volume(0), 0);
        assert_eq!(packed_volume(100), 0xFFFF_FFFF);
        let half = packed_volume(50);
        assert_eq!(half & 0xFFFF, half >> 16);
        assert!((half & 0xFFFF) >= 0x7FFF);
    }

    #[test]
    fn chunk_is_exactly_twenty_milliseconds() {
        assert_eq!(CHUNK_BYTES, 3_840);
    }
}
