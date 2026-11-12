//! Progress-bar storyboard (videoshot) worker.
//!
//! `Session::videoshot` names one or more sprite sheets (`cols × rows` cells,
//! each `cell_w × cell_h`) plus the time of every cell. The sheets are downloaded
//! and decoded once on this worker; each hover then asks for one cropped cell,
//! so the player thread never blocks on the network or on JPEG decoding.
//!
//! A failure is not an error for the user: the engine falls back to a time-only
//! bubble (`storyboard unavailable` in the log).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use crate::log;
use crate::overlay::ImageData;

static NEXT_ID_BASE: AtomicU64 = AtomicU64::new(1_000_000);

/// One decoded sprite sheet and the cell grid it belongs to.
struct Sheet {
    rgba: Vec<u8>,
}

pub struct Meta {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub sheets: usize,
    /// Seconds per cell, monotonically non-decreasing (from `videoshot.index`).
    pub times: Vec<f32>,
}

pub enum Request {
    Init,
    Cell { time_ms: u64 },
    Stop,
}

pub enum Event {
    Ready(Arc<Meta>),
    Cell { index: usize, image: ImageData },
    Failed,
}

/// The cell a hover time maps to: the last timestamp at or before `time_ms`,
/// else the first cell. Pure so the engine and the worker share one rule.
pub fn cell_index_for(times: &[f32], time_ms: u64) -> usize {
    if times.is_empty() {
        return 0;
    }
    let seconds = time_ms as f32 / 1000.0;
    let index = times.partition_point(|value| *value <= seconds);
    index.saturating_sub(1)
}

pub fn spawn(
    session: Arc<o11a1::bilibili::Session>,
    bvid: String,
    cid: u64,
) -> (Sender<Request>, Receiver<Event>) {
    let (request_tx, request_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("player-storyboard".into())
        .spawn(move || run(session, bvid, cid, request_rx, event_tx));
    (request_tx, event_rx)
}

fn run(
    session: Arc<o11a1::bilibili::Session>,
    bvid: String,
    cid: u64,
    requests: Receiver<Request>,
    events: Sender<Event>,
) {
    let Some(runtime) = o11a1::bilibili::shared_runtime() else {
        let _ = events.send(Event::Failed);
        return;
    };
    let mut meta: Option<Meta> = None;
    let mut sheets: Vec<Sheet> = Vec::new();
    let id_base = NEXT_ID_BASE.fetch_add(1_000_000, Ordering::Relaxed);
    while let Ok(request) = requests.recv() {
        match request {
            Request::Stop => break,
            Request::Init => {
                if meta.is_some() {
                    if let Some(meta) = &meta {
                        let _ = events.send(Event::Ready(Arc::new(Meta {
                            cols: meta.cols,
                            rows: meta.rows,
                            cell_w: meta.cell_w,
                            cell_h: meta.cell_h,
                            sheets: meta.sheets,
                            times: meta.times.clone(),
                        })));
                    }
                    continue;
                }
                let shot = match runtime.block_on(session.videoshot(&bvid, cid)) {
                    Ok(shot) => shot,
                    Err(error) => {
                        log::line(&format!(
                            "storyboard unavailable bvid={bvid} cid={cid} err={error:#}"
                        ));
                        let _ = events.send(Event::Failed);
                        continue;
                    }
                };
                let mut loaded = Vec::with_capacity(shot.sheets.len());
                let mut failed: Option<String> = None;
                for url in &shot.sheets {
                    match runtime.block_on(session.fetch_image(url)) {
                        Ok((rgba, width, height)) => {
                            log::line(&format!(
                                "storyboard sheet loaded {}x{} bytes={}",
                                width,
                                height,
                                rgba.len()
                            ));
                            loaded.push(Sheet { rgba });
                        }
                        Err(error) => {
                            failed = Some(format!("sheet: {error:#}"));
                            break;
                        }
                    }
                }
                if let Some(reason) = failed {
                    log::line(&format!(
                        "storyboard unavailable bvid={bvid} cid={cid} reason={reason}"
                    ));
                    let _ = events.send(Event::Failed);
                    continue;
                }
                let meta_value = Meta {
                    cols: shot.cols.max(1),
                    rows: shot.rows.max(1),
                    cell_w: shot.cell_w,
                    cell_h: shot.cell_h,
                    sheets: loaded.len(),
                    times: shot.times.clone(),
                };
                log::line(&format!(
                    "storyboard ready bvid={bvid} cid={cid} sheets={} cols={} rows={} cell={}x{} tiles={}",
                    meta_value.sheets,
                    meta_value.cols,
                    meta_value.rows,
                    meta_value.cell_w,
                    meta_value.cell_h,
                    meta_value.times.len()
                ));
                meta = Some(meta_value);
                sheets = loaded;
                if let Some(meta) = &meta {
                    let _ = events.send(Event::Ready(Arc::new(Meta {
                        cols: meta.cols,
                        rows: meta.rows,
                        cell_w: meta.cell_w,
                        cell_h: meta.cell_h,
                        sheets: meta.sheets,
                        times: meta.times.clone(),
                    })));
                }
            }
            Request::Cell { time_ms } => {
                let Some(meta) = &meta else {
                    continue;
                };
                if meta.cell_w == 0 || meta.cell_h == 0 || meta.sheets == 0 {
                    continue;
                }
                let per_sheet = (meta.cols as usize).saturating_mul(meta.rows as usize).max(1);
                let index = cell_index_for(&meta.times, time_ms).min(
                    meta.sheets
                        .saturating_mul(per_sheet)
                        .saturating_sub(1),
                );
                let sheet_index = index / per_sheet;
                let Some(sheet) = sheets.get(sheet_index) else {
                    continue;
                };
                let within = index % per_sheet;
                let col = (within % meta.cols as usize) as u32;
                let row = (within / meta.cols as usize) as u32;
                let Some(rgba) = crop_cell(
                    &sheet.rgba,
                    meta.cols.saturating_mul(meta.cell_w),
                    meta.cell_w,
                    meta.cell_h,
                    col,
                    row,
                ) else {
                    let _ = events.send(Event::Failed);
                    continue;
                };
                let image = ImageData {
                    id: id_base.saturating_add(index as u64),
                    width: meta.cell_w,
                    height: meta.cell_h,
                    rgba: Arc::new(rgba),
                };
                log::line(&format!(
                    "storyboard cell index={index} time_ms={time_ms} sheet={sheet_index} col={col} row={row}"
                ));
                let _ = events.send(Event::Cell { index, image });
            }
        }
    }
    log::line("storyboard worker stopped");
}

/// Crop one RGBA cell out of a sheet whose rows are `sheet_width * 4` bytes.
fn crop_cell(
    sheet: &[u8],
    sheet_width: u32,
    cell_w: u32,
    cell_h: u32,
    col: u32,
    row: u32,
) -> Option<Vec<u8>> {
    let sheet_width = sheet_width.max(1) as usize;
    let cell_w = cell_w.max(1) as usize;
    let cell_h = cell_h.max(1) as usize;
    let left = col as usize * cell_w;
    let top = row as usize * cell_h;
    if left + cell_w > sheet_width || top + cell_h > sheet.len() / (sheet_width * 4).max(1) {
        return None;
    }
    let mut out = Vec::with_capacity(cell_w * cell_h * 4);
    for y in 0..cell_h {
        let start = ((top + y) * sheet_width + left) * 4;
        let end = start + cell_w * 4;
        out.extend_from_slice(sheet.get(start..end)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_index_uses_the_last_timestamp_at_or_before_the_hover() {
        let times = [0.0f32, 5.0, 10.0, 15.0];
        assert_eq!(cell_index_for(&times, 0), 0);
        assert_eq!(cell_index_for(&times, 4_999), 0);
        assert_eq!(cell_index_for(&times, 5_000), 1);
        assert_eq!(cell_index_for(&times, 12_000), 2);
        assert_eq!(cell_index_for(&times, 99_000), 3);
        assert_eq!(cell_index_for(&[], 10_000), 0);
    }

    #[test]
    fn crop_cell_extracts_the_right_block() {
        // 4x2 cells of 2x2 px => sheet 8x4. Every cell has a unique gray value.
        let mut sheet = vec![0u8; 8 * 4 * 4];
        for row in 0..4 {
            for col in 0..8 {
                let value = (row * 8 + col) as u8;
                let at = (row * 8 + col) * 4;
                sheet[at..at + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let cell = crop_cell(&sheet, 8, 2, 2, 1, 1).expect("cell inside");
        assert_eq!(cell.len(), 2 * 2 * 4);
        assert_eq!(&cell[0..4], &[18u8, 18, 18, 255]);
        assert_eq!(&cell[4..8], &[19u8, 19, 19, 255]);
        assert_eq!(&cell[8..12], &[26u8, 26, 26, 255]);
        assert_eq!(&cell[12..16], &[27u8, 27, 27, 255]);
    }
}
