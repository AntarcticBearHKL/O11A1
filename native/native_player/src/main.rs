//! Minimal runner for the native player window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//!
//! ```text
//! native_player [--bvid BV1mhxo6BEG4] [--cid 123] [--url URL_OR_FILE]
//!               [--title TEXT] [--start 00:30] [--quality 80]
//! ```
//!
//! Environment:
//!   BILI_OPEN_BVID    Bilibili id when no URL is given (default BV1mhxo6BEG4)
//!   BILI_PLAYER_URL   direct media URL or local file (wins over the bvid)
//!   BILI_PLAYER_SECONDS  auto-exit after N seconds
//!   BILI_PLAYER_QA    comma list: seek,pause,resume,interp,enhance,anime,shot,
//!                     stats,mute,fullscreen,mini,aspect,ontop,bright,
//!                     audioonly,speedup,speeddown,sidebar,info,episodes,
//!                     comments,performance,settings,dmpopup,chrome,
//!                     comments_time,like,coin,fav,page2,quality,
//!                     quality_low,quality_next,quality_locked,audio_menu,
//!                     audio_next,comments_more,hover,hover2,sleep,
//!                     sleep_short,fault,retry,stall,dm_menu,dm_hold,dm_like,
//!                     dm_report,dm_report_one,dm_block,dm_copy,dm_undo,dm_history,
//!                     dm_history_date,dm_history_close
//!   BILI_PLAYER_SHOT  screenshot path for the `shot` QA action
//!   BILI_PLAYER_SIZE  client size, default 1000x600

use native_player::{parse_time_ms, Player, PlayerConfig};

fn main() {
    if let Err(error) = run() {
        eprintln!("native_player: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut config = PlayerConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bvid" => config.bvid = args.next(),
            "--cid" => {
                if let Some(value) = args.next() {
                    config.cid = value.trim().parse().ok();
                }
            }
            "--url" => config.direct_url = args.next(),
            "--title" => config.title = args.next().unwrap_or_default(),
            "--start" => {
                if let Some(value) = args.next() {
                    config.start_ms = parse_time_ms(&value)
                        .ok_or_else(|| anyhow::anyhow!("invalid --start: {value}"))?;
                }
            }
            "--quality" => {
                if let Some(value) = args.next() {
                    config.quality_qn = value.trim().parse().ok();
                }
            }
            "-h" | "--help" => {
                println!(
                    "native_player [--bvid BV…] [--cid N] [--url URL_OR_FILE]\n\
                     \x20             [--title TEXT] [--start seconds|mm:ss] [--quality qn]\n\
                     Environment: BILI_OPEN_BVID, BILI_PLAYER_URL, BILI_PLAYER_SECONDS,\n\
                     BILI_PLAYER_QA, BILI_PLAYER_SHOT, BILI_PLAYER_SIZE, BILI_PLAYER_VOLUME,\n\
                     BILI_PLAYER_MINI"
                );
                return Ok(());
            }
            other => eprintln!("native_player: ignoring unknown argument {other}"),
        }
    }
    let player = Player::open(config)?;
    println!(
        "native_player: window open, duration {} ms, fps {:.0}",
        player.duration_ms(),
        player.fps()
    );
    player.wait();
    let mut player = player;
    player.close();
    println!("native_player: closed");
    Ok(())
}
