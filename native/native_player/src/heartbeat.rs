//! Bilibili 历史 / 继续观看 heartbeat.
//!
//! The player thread only formats the current position/watched time and pushes
//! a [`Message`] into this worker's channel. The worker owns the session, lazily
//! resolves the real `aid` through `video_view` when the metadata worker has not
//! delivered it yet, and calls `Session::report_heartbeat` off the playback
//! thread. A rejected heartbeat is logged, never surfaced as a playback error.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::log;

/// One heartbeat payload. `aid` is filled by the worker (from [`Message::Aid`] or
/// a background `video_view`).
#[derive(Clone, Copy, Debug)]
pub struct Beat {
    pub played_s: u64,
    pub watched_s: u64,
    pub duration_s: u64,
    pub play_type: u64,
}

pub enum Message {
    Aid(u64),
    Beat(Beat),
    Stop,
}

/// Handle to the background heartbeat worker. Dropping it sends `Stop` and
/// joins the worker so closing the player never leaves a network thread behind.
pub struct Heartbeat {
    tx: Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl Heartbeat {
    pub fn spawn(session: Arc<o11a1::bilibili::Session>, bvid: String, cid: u64) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("player-heartbeat".into())
            .spawn(move || run(session, bvid, cid, rx))
            .ok();
        Self { tx, thread }
    }

    pub fn set_aid(&self, aid: u64) {
        let _ = self.tx.send(Message::Aid(aid));
    }

    pub fn beat(&self, beat: Beat) {
        let _ = self.tx.send(Message::Beat(beat));
    }

    pub fn stop(&mut self) {
        let _ = self.tx.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn run(
    session: Arc<o11a1::bilibili::Session>,
    bvid: String,
    cid: u64,
    messages: Receiver<Message>,
) {
    let Some(runtime) = o11a1::bilibili::shared_runtime() else {
        log::line("heartbeat worker stopped reason=no-runtime");
        return;
    };
    let start_ts = unix_now();
    let mut aid: u64 = 0;
    while let Ok(message) = messages.recv() {
        match message {
            Message::Stop => break,
            Message::Aid(value) => {
                if value > 0 {
                    aid = value;
                    log::line(&format!(
                        "heartbeat aid={aid} bvid={bvid} cid={cid} source=detail"
                    ));
                }
            }
            Message::Beat(beat) => {
                if aid == 0 {
                    // The 信息 worker usually wins this race, but a player whose
                    // right strip never opened still needs a real aid.
                    match runtime.block_on(session.video_view(&bvid)) {
                        Ok(detail) => {
                            aid = detail.aid.max(0) as u64;
                            log::line(&format!(
                                "heartbeat aid={aid} bvid={bvid} cid={cid} source=video_view"
                            ));
                        }
                        Err(error) => {
                            log::line(&format!(
                                "heartbeat aid unavailable bvid={bvid} err={error:#}"
                            ));
                        }
                    }
                }
                if aid == 0 {
                    log::line(&format!(
                        "heartbeat skipped play_type={} reason=no-aid",
                        beat.play_type
                    ));
                    continue;
                }
                let result = runtime.block_on(session.report_heartbeat(
                    aid,
                    &bvid,
                    cid,
                    beat.played_s,
                    beat.watched_s,
                    beat.duration_s,
                    start_ts,
                    beat.play_type,
                ));
                match result {
                    Ok(code) => log::line(&format!(
                        "heartbeat sent play_type={} aid={aid} bvid={bvid} cid={cid} played_s={} watched_s={} duration_s={} start_ts={start_ts} code={code}",
                        beat.play_type,
                        beat.played_s,
                        beat.watched_s,
                        beat.duration_s
                    )),
                    Err(error) => log::line(&format!(
                        "heartbeat failed play_type={} aid={aid} bvid={bvid} cid={cid} err={error:#}",
                        beat.play_type
                    )),
                }
            }
        }
    }
    log::line("heartbeat worker stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_now_is_a_plausible_epoch_second() {
        assert!(unix_now() > 1_700_000_000);
    }
}
