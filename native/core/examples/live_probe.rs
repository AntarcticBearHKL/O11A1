//! QA helper: print one live room's FLV/HLS URL. Not part of the app.
//!
//! `cargo run -p o11a1-core --example live_probe`

fn main() -> anyhow::Result<()> {
    let runtime = o11a1_core::bilibili::shared_runtime()
        .ok_or_else(|| anyhow::anyhow!("runtime unavailable"))?;
    let result = runtime.block_on(async {
        use o11a1_core::bilibili::Session;
        let session = match o11a1_core::accounts::load() {
            Some(account) => Session::from_token(&o11a1_core::accounts::to_token(&account)).await?,
            None => Session::new().await?,
        };
        let index = session.live_index().await?;
        for module in &index.modules {
            for room in &module.rooms {
                if room.live_status != 1 || room.room_id <= 0 {
                    continue;
                }
                let play = session.live_play_url(room.room_id).await?;
                if let Some(url) = play.flv.or(play.hls) {
                    println!("room={} title={:?} url={url}", room.room_id, room.title);
                    return Ok::<(), anyhow::Error>(());
                }
            }
        }
        anyhow::bail!("no live room with a stream URL");
    });
    result
}

