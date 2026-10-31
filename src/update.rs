//! 检查更新：启动 / 每小时查一次 GitHub release，有新版本就弹窗。
//!
//! 我们的 release 全是 pre-release（`v0.2.0-beta.4`），GitHub 的
//! `/releases/latest` 只认非 pre-release，会返回 null 或一个旧的稳定版，所以
//! 这里用 `/releases` 列表并自己按 `created_at` 取最新的一条。
//!
//! 时机（全部不阻塞 UI 线程）：
//!   * 启动：首屏真正就绪（`HOME_SHOWN`）之后 3 秒；
//!   * 每小时：`install` 里那个重复的 `slint::Timer`；
//!   * 设置 → 系统 → 检查：用户点一次查一次，结果写回设置行。
//!
//! 播放中不打断：自动查到了新版本但播放器正在播放时，先记在 `pending`，
//! 由 5 秒一次的等待计时器在播放停下后弹出。
//!
//! 提醒节流是「每个进程、每个版本一次」：`notified` 只在内存里，所以同一个
//! 版本这次启动只弹一次（每小时那次只写 `update skip notified`），重启后再
//! 弹一次 —— 这是用户明确要的行为。设置里的「检查」是用户主动点的，不受
//! 这个节流限制。
//!
//! 「去下载」用 `ShellExecuteW("open", url)` 打开浏览器到资产直链，不自己
//! 起进程、不假装下载。

use crate::*;
use regex::Regex;

const REPO: &str = "AntarcticBearHKL/O11A1";
const RELEASE_API: &str = "https://api.github.com/repos/AntarcticBearHKL/O11A1/releases";
/// The one asset every release carries; the URL is built from it when the
/// release JSON has no matching asset entry.
const ASSET_NAME: &str = "O11A1.exe";
const LOCAL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Startup: after the home page is on screen, wait this many 1 s ticks.
const STARTUP_READY_TICKS: u32 = 3;
const HOURLY: Duration = Duration::from_secs(60 * 60);
/// How often the deferred (playing) path re-checks whether playback stopped.
const DEFERRED_TICK: Duration = Duration::from_secs(5);
const NOTES_MAX_LINES: usize = 12;
const NOTES_MAX_CHARS: usize = 800;

/// Canned notes for `BILI_UPDATE_FAKE`, so the dialog can be captured without a
/// network round trip. Markdown on purpose: the stripper runs on it too.
const FAKE_NOTES: &str = "## 本次更新\n\n- 启动与每小时自动检查新版本\n- 设置 → 系统新增「检查更新」\n- 新版本弹窗：版本对比 + 更新说明\n- 「去下载」直接打开浏览器下载最新版\n\n完整更新说明见 [GitHub Releases](https://github.com/AntarcticBearHKL/O11A1/releases)。";

// ---------------------------------------------------------------------------
// Version parsing / comparison.
//
// SemVer without a dependency: 1-3 numeric core fields, then an optional
// `-pre.release` tail. Rules that matter for our tags:
//   * `beta.4 > beta.3` and `beta.10 > beta.9` (numeric identifiers compare as
//     numbers, not strings);
//   * `0.2.0-beta.4 < 0.2.0` (a pre-release sorts BEFORE its release);
//   * `alpha < beta` (alphanumeric identifiers compare lexically);
//   * a numeric identifier is LOWER than an alphanumeric one (SemVer 11.4.3).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    /// Empty = a final release, which sorts after every pre-release of the
    /// same core.
    pre: Vec<Ident>,
}

impl Version {
    fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        // Build metadata never participates in precedence.
        let text = text.split('+').next().unwrap_or(text);
        let (core, tail) = match text.split_once('-') {
            Some((core, tail)) => (core, Some(tail)),
            None => (text, None),
        };
        let mut parts = [0u64; 3];
        let mut seen = 0usize;
        for (index, field) in core.split('.').enumerate() {
            if index >= 3 {
                return None;
            }
            let field = field.trim();
            if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            parts[index] = field.parse().ok()?;
            seen = index + 1;
        }
        if seen == 0 {
            return None;
        }
        let pre = match tail {
            None => Vec::new(),
            Some("") => return None,
            Some(tail) => tail
                .split('.')
                .map(|ident| {
                    if !ident.is_empty() && ident.bytes().all(|byte| byte.is_ascii_digit()) {
                        Ident::Num(ident.parse().unwrap_or(u64::MAX))
                    } else {
                        Ident::Alpha(ident.to_string())
                    }
                })
                .collect(),
        };
        Some(Version { core: parts, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A final release is greater than any of its pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (left, right) in self.pre.iter().zip(other.pre.iter()) {
                        let order = match (left, right) {
                            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
                            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
                            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
                            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `true` when `remote` is strictly newer than `local`. `None` when either tag
/// cannot be parsed (no dialog, just the log line).
fn is_newer(remote: &str, local: &str) -> Option<bool> {
    let remote = Version::parse(remote)?;
    let local = Version::parse(local)?;
    Some(remote > local)
}

/// Every tag we build and every tag GitHub hands back starts with `v`; the URL
/// needs exactly one.
fn normalise_tag(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('v') || raw.starts_with('V') {
        raw.to_string()
    } else {
        format!("v{raw}")
    }
}

// ---------------------------------------------------------------------------
// Release model + fetch.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Release {
    /// `v0.2.0-beta.4`, as used in the log lines and the asset URL.
    tag: String,
    /// Markdown stripped, at most 12 lines / 800 chars.
    notes: String,
    /// The asset's direct download link.
    url: String,
}

fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{ASSET_NAME}")
}

fn asset_url(item: &serde_json::Value, tag: &str) -> String {
    if let Some(assets) = item.get("assets").and_then(|value| value.as_array()) {
        for asset in assets {
            let name = asset.get("name").and_then(|value| value.as_str());
            if name.map(|name| name.eq_ignore_ascii_case(ASSET_NAME)) != Some(true) {
                continue;
            }
            if let Some(url) = asset
                .get("browser_download_url")
                .and_then(|value| value.as_str())
            {
                if !url.trim().is_empty() {
                    return url.trim().to_string();
                }
            }
        }
    }
    release_url(tag)
}

fn release_from_json(item: &serde_json::Value) -> Option<Release> {
    let tag = normalise_tag(item.get("tag_name")?.as_str()?);
    // Only accept a tag we can actually compare; a release with a junk tag
    // must not pop a dialog.
    Version::parse(&tag)?;
    let body = item.get("body").and_then(|value| value.as_str()).unwrap_or("");
    Some(Release {
        notes: plain_notes(body),
        url: asset_url(item, &tag),
        tag,
    })
}

/// Blocking. Runs on a `netpool` worker, never on the UI thread.
fn fetch_latest() -> Result<Release, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!(
            "O11A1/",
            env!("CARGO_PKG_VERSION"),
            " (update check)"
        ))
        .build()
        .map_err(|err| format!("client: {err}"))?;
    let response = client
        .get(RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|err| format!("network: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("http {}", response.status().as_u16()));
    }
    let body = response.text().map_err(|err| format!("body: {err}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|err| format!("json: {err}"))?;
    let items = value
        .as_array()
        .ok_or_else(|| "json: not an array".to_string())?;
    // `/releases` is newest-first by `created_at` in practice, but the list is
    // not contractual: take the maximum explicitly. Drafts are skipped (they
    // are also invisible to unauthenticated clients, but the API shape allows
    // them); pre-releases are exactly what we want, so `prerelease` is ignored.
    let mut newest: Option<(&str, &serde_json::Value)> = None;
    for item in items {
        if item
            .get("draft")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(tag) = item.get("tag_name").and_then(|value| value.as_str()) else {
            continue;
        };
        if tag.trim().is_empty() {
            continue;
        }
        let created = item
            .get("created_at")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        newest = match newest {
            Some((best, best_item)) if best >= created => Some((best, best_item)),
            _ => Some((created, item)),
        };
    }
    let (_, item) = newest.ok_or_else(|| "no releases".to_string())?;
    release_from_json(item).ok_or_else(|| "release: unparsable tag".to_string())
}

fn fake_release(raw: &str) -> Result<Release, String> {
    let tag = normalise_tag(raw);
    Version::parse(&tag).ok_or_else(|| format!("fake: bad version {raw}"))?;
    Ok(Release {
        url: release_url(&tag),
        notes: plain_notes(FAKE_NOTES),
        tag,
    })
}

// ---------------------------------------------------------------------------
// Markdown → the plain-text preview shown in the dialog.
// ---------------------------------------------------------------------------

fn regexes() -> &'static (Regex, Regex, Regex) {
    static SLOT: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    SLOT.get_or_init(|| {
        (
            // Images disappear entirely (`![alt](url)`).
            Regex::new(r"!\[[^\]]*\]\([^)]*\)").expect("image regex"),
            // Links keep their text (`[text](url)` -> `text`).
            Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("link regex"),
            // HTML comments and tags; emphasis / code markers.
            Regex::new(r"<!--.*?-->|<[^>]*>|[*_`~]+").expect("markup regex"),
        )
    })
}

/// The release body as the dialog shows it: markdown markers gone, at most
/// [`NOTES_MAX_LINES`] lines and [`NOTES_MAX_CHARS`] characters.
pub(crate) fn plain_notes(body: &str) -> String {
    let (images, links, markup) = regexes();
    let mut lines: Vec<String> = Vec::new();
    for raw in body.lines() {
        let line = images.replace_all(raw, "");
        let line = links.replace_all(&line, "$1");
        let line = markup.replace_all(&line, "");
        let mut line = line.trim_end().to_string();
        // Nesting markers: headings, quotes, list bullets.
        let trimmed = line.trim_start();
        if trimmed.chars().all(|ch| ch == '-' || ch == '*' || ch == '=' || ch == '_') {
            // A horizontal rule (or an empty line of markers): no content.
            if !trimmed.is_empty() {
                continue;
            }
        }
        line = trimmed
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start()
            .to_string();
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            line = format!("· {}", rest.trim_start());
        } else if let Some(rest) = line.strip_prefix("+ ") {
            line = format!("· {}", rest.trim_start());
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            // At most one blank line in a row, and never a leading one.
            if matches!(lines.last(), Some(last) if !last.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        lines.push(line);
        if lines.len() > NOTES_MAX_LINES + 4 {
            break;
        }
    }
    while matches!(lines.last(), Some(last) if last.is_empty()) {
        lines.pop();
    }
    let truncated_lines = lines.len() > NOTES_MAX_LINES;
    lines.truncate(NOTES_MAX_LINES);
    let mut text = lines.join("\n");
    if text.chars().count() > NOTES_MAX_CHARS {
        text = text.chars().take(NOTES_MAX_CHARS - 1).collect::<String>();
        text.push('…');
    } else if truncated_lines {
        text.push('…');
    }
    text
}

// ---------------------------------------------------------------------------
// State (UI thread only) + timers.
// ---------------------------------------------------------------------------

struct UpdateState {
    ui: Weak<AppWindow>,
    /// Tags already shown in THIS process. Never persisted on purpose: every
    /// launch may show the same version once.
    notified: RefCell<HashSet<String>>,
    /// Found while a player was playing; shown once playback stops.
    pending: RefCell<Option<Release>>,
    /// What the dialog's 去下载 button will open.
    shown: RefCell<Option<Release>>,
    checking: Cell<bool>,
    startup_ticks: Cell<u32>,
    ready_ticks: Cell<u32>,
    deferred_started: Cell<bool>,
    startup_timer: Rc<slint::Timer>,
    hourly_timer: Rc<slint::Timer>,
    deferred_timer: Rc<slint::Timer>,
}

thread_local! {
    /// The one instance for this process. `install` puts it here before the
    /// event loop starts; the callbacks and the timers reach back through it.
    static UPDATE: RefCell<Option<Rc<UpdateState>>> = const { RefCell::new(None) };
}

fn current_state() -> Option<Rc<UpdateState>> {
    UPDATE.with(|slot| slot.borrow().clone())
}

/// True while the most recently opened player is actually playing. A paused or
/// closed player does not hold the dialog back.
fn playing() -> bool {
    player::active_control()
        .map(|control| !control.is_closed() && !control.paused())
        .unwrap_or(false)
}

/// Install the callbacks, publish the current version and start the clocks.
/// UI thread, before `ui.run()`.
pub(crate) fn install(ui: &AppWindow) {
    let state = Rc::new(UpdateState {
        ui: ui.as_weak(),
        notified: RefCell::new(HashSet::new()),
        pending: RefCell::new(None),
        shown: RefCell::new(None),
        checking: Cell::new(false),
        startup_ticks: Cell::new(0),
        ready_ticks: Cell::new(0),
        deferred_started: Cell::new(false),
        startup_timer: Rc::new(slint::Timer::default()),
        hourly_timer: Rc::new(slint::Timer::default()),
        deferred_timer: Rc::new(slint::Timer::default()),
    });
    UPDATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));

    ui.set_update_current(format!("当前版本 v{LOCAL_VERSION}").into());
    ui.set_update_status("".into());
    crate::debug_log(&format!(
        "update install local={LOCAL_VERSION} hourly_s={}",
        HOURLY.as_secs()
    ));

    // 「去下载」: open the asset link in the default browser, then close.
    {
        let weak = state.ui.clone();
        ui.on_update_download(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let url = current_state()
                .and_then(|state| state.shown.borrow().as_ref().map(|rel| rel.url.clone()))
                .unwrap_or_default();
            if url.is_empty() {
                crate::debug_log("update open url=- err=no-pending-release");
                return;
            }
            if qa_env_flag("BILI_UPDATE_CLICK_TEST") {
                // QA: prove the URL without launching a real download.
                crate::debug_log(&format!("update open url={url} dry_run=1"));
            } else {
                crate::debug_log(&format!("update open url={url}"));
                open_in_browser(&url);
            }
            ui.set_update_open(false);
        });
    }
    // 「稍后再说」/ Esc / scrim click.
    {
        let weak = state.ui.clone();
        ui.on_update_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_update_open() {
                    crate::debug_log("update dialog dismissed");
                }
                ui.set_update_open(false);
            }
        });
    }
    // 设置 → 系统 → 检查.
    {
        let weak = state.ui.clone();
        ui.on_update_check_now(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_update_status("正在检查…".into());
            request_check(&ui, true);
        });
    }

    // Startup clock: 1 s ticks, fires 3 s after `HOME_SHOWN`. Gives up after
    // 10 minutes on the login view instead of popping a dialog over it.
    {
        let timer = state.startup_timer.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || startup_tick(),
        );
    }
    // Hourly clock. `BILI_UPDATE_HOURLY_TEST=1` shortens it to 6 s so the
    // throttle can be shown in one process (see the QA runs in the report).
    {
        let interval = if qa_env_flag("BILI_UPDATE_HOURLY_TEST") {
            Duration::from_secs(6)
        } else {
            HOURLY
        };
        let weak = state.ui.clone();
        let timer = state.hourly_timer.clone();
        timer.start(slint::TimerMode::Repeated, interval, move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !HOME_SHOWN.load(Ordering::SeqCst) {
                crate::debug_log("update skip not-ready reason=login-view hourly=1");
                return;
            }
            request_check(&ui, false);
        });
    }
    // Deferred clock: started only when a newer release was found while a
    // player was playing, then stopped as soon as it fires. An idle process
    // therefore owns no polling timer beyond the hourly one.

    // ---- QA hooks ---------------------------------------------------------
    // `BILI_UPDATE_TEST=1`: check immediately instead of waiting for the home
    // page + 3 s, and print the `update check …` line.
    if qa_env_flag("BILI_UPDATE_TEST") {
        let weak = state.ui.clone();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            crate::debug_log("update qa immediate check");
            request_check(&ui, false);
        });
    }
    // `BILI_UPDATE_SETTINGS_SHOT=1`: open 设置 → 系统 through the real
    // callback, capture the row, then press 检查 and capture the result.
    if qa_env_flag("BILI_UPDATE_SETTINGS_SHOT") {
        let weak = state.ui.clone();
        slint::Timer::single_shot(Duration::from_millis(9_000), move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.invoke_open_settings();
            ui.set_settings_tab(5);
            crate::debug_log("update qa settings shot open tab=5");
            let shot_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(1_400), move || {
                if let Some(ui) = shot_weak.upgrade() {
                    shot(&ui, "f182_update_settings");
                }
            });
            let check_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(2_000), move || {
                if let Some(ui) = check_weak.upgrade() {
                    crate::debug_log("update qa settings shot press check");
                    ui.invoke_update_check_now();
                }
            });
            let latest_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(3_600), move || {
                if let Some(ui) = latest_weak.upgrade() {
                    shot(&ui, "f182_update_latest");
                    crate::debug_log(&format!(
                        "update qa latest shot status={:?}",
                        ui.get_update_status()
                    ));
                }
            });
        });
    }
}

/// One 1 s startup tick: wait for the home page, then 3 s of settle time.
fn startup_tick() {
    let Some(state) = current_state() else {
        return;
    };
    let Some(ui) = state.ui.upgrade() else {
        state.startup_timer.stop();
        return;
    };
    if qa_env_flag("BILI_UPDATE_TEST") {
        // The immediate QA check already ran; this clock has nothing to do.
        state.startup_timer.stop();
        return;
    }
    if !HOME_SHOWN.load(Ordering::SeqCst) {
        let ticks = state.startup_ticks.get() + 1;
        state.startup_ticks.set(ticks);
        if ticks >= 600 {
            state.startup_timer.stop();
            crate::debug_log("update startup skip reason=not-ready view=login");
        }
        return;
    }
    let ready = state.ready_ticks.get() + 1;
    state.ready_ticks.set(ready);
    if ready < STARTUP_READY_TICKS {
        return;
    }
    state.startup_timer.stop();
    crate::debug_log("update startup fire after_home_s=3");
    request_check(&ui, false);
}

/// Fetch (or fake) once. `manual` is 设置 → 检查: its result goes to the
/// settings row and it is allowed to re-open the dialog the user asked for.
fn request_check(ui: &AppWindow, manual: bool) {
    let Some(state) = current_state() else {
        return;
    };
    if state.checking.get() {
        if manual {
            ui.set_update_status("上一次检查还未结束".into());
        }
        return;
    }
    state.checking.set(true);
    crate::debug_log(&format!("update request manual={}", u8::from(manual)));

    // `BILI_UPDATE_FAKE`: a fixed remote version, no network at all.
    if let Some(fake) = qa_env_var("BILI_UPDATE_FAKE") {
        match fake_release(&fake) {
            Ok(rel) => finish_check(ui, Ok(rel), manual),
            Err(err) => finish_check(ui, Err(err), manual),
        }
        return;
    }

    let weak = ui.as_weak();
    netpool::submit(
        netpool::Prio::Low,
        Box::new(move || {
            let result = fetch_latest();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                finish_check(&ui, result, manual);
            });
        }),
    );
}

fn finish_check(ui: &AppWindow, result: Result<Release, String>, manual: bool) {
    let Some(state) = current_state() else {
        return;
    };
    state.checking.set(false);
    let rel = match result {
        Ok(rel) => rel,
        Err(err) => {
            crate::debug_log(&format!("update check failed err={err}"));
            if manual {
                ui.set_update_status(format!("检查失败：{err}").into());
            }
            return;
        }
    };
    let newer = match is_newer(&rel.tag, LOCAL_VERSION) {
        Some(newer) => newer,
        None => {
            crate::debug_log(&format!(
                "update check failed err=unparsable local={LOCAL_VERSION} remote={}",
                rel.tag
            ));
            if manual {
                ui.set_update_status("检查失败：版本号无法解析".into());
            }
            return;
        }
    };
    crate::debug_log(&format!(
        "update check local={LOCAL_VERSION} remote={} newer={} url={}",
        rel.tag,
        u8::from(newer),
        rel.url
    ));

    if !newer {
        if manual {
            ui.set_update_status("已是最新版本".into());
        }
        return;
    }

    if manual {
        // User-initiated: always shows, even if the hourly path already did.
        ui.set_update_status(format!("发现新版本 {}", rel.tag).into());
        show_dialog(ui, &state, rel);
        return;
    }
    if state.notified.borrow().contains(&rel.tag) {
        crate::debug_log(&format!("update skip notified {}", rel.tag));
        return;
    }
    if playing() {
        crate::debug_log(&format!(
            "update deferred playing=1 version={} retry_s={}",
            rel.tag,
            DEFERRED_TICK.as_secs()
        ));
        *state.pending.borrow_mut() = Some(rel);
        start_deferred_wait(&state);
        return;
    }
    show_dialog(ui, &state, rel);
}

/// Start the 5 s "has playback stopped?" clock, once per deferral. It stops
/// itself after showing the dialog (or when the window is gone).
fn start_deferred_wait(state: &Rc<UpdateState>) {
    if state.deferred_started.get() {
        return;
    }
    state.deferred_started.set(true);
    let weak = state.ui.clone();
    let timer = state.deferred_timer.clone();
    let stopper = timer.clone();
    let state_for_stop = state.clone();
    timer.start(slint::TimerMode::Repeated, DEFERRED_TICK, move || {
        let Some(ui) = weak.upgrade() else {
            stopper.stop();
            state_for_stop.deferred_started.set(false);
            return;
        };
        if playing() {
            return;
        }
        let Some(rel) = state_for_stop.pending.borrow_mut().take() else {
            stopper.stop();
            state_for_stop.deferred_started.set(false);
            return;
        };
        stopper.stop();
        state_for_stop.deferred_started.set(false);
        crate::debug_log(&format!("update deferred fire version={}", rel.tag));
        show_dialog(&ui, &state_for_stop, rel);
    });
}

fn show_dialog(ui: &AppWindow, state: &Rc<UpdateState>, rel: Release) {
    state.notified.borrow_mut().insert(rel.tag.clone());
    *state.shown.borrow_mut() = Some(rel.clone());
    let notes = if rel.notes.trim().is_empty() {
        "本次发布没有附带更新说明，可以直接下载。".to_string()
    } else {
        rel.notes.clone()
    };
    ui.set_update_title(format!("发现新版本 {}", rel.tag).into());
    ui.set_update_versions(format!("当前 v{LOCAL_VERSION}  →  {}", rel.tag).into());
    ui.set_update_notes(notes.into());
    ui.set_update_open(true);
    crate::debug_log(&format!(
        "update dialog shown version={} url={}",
        rel.tag, rel.url
    ));

    // `BILI_UPDATE_DIALOG_SHOT=1`: capture the dialog itself.
    if qa_env_flag("BILI_UPDATE_DIALOG_SHOT") {
        let weak = state.ui.clone();
        slint::Timer::single_shot(Duration::from_millis(700), move || {
            if let Some(ui) = weak.upgrade() {
                shot(&ui, "f182_update_dialog");
            }
        });
    }
    // `BILI_UPDATE_CLICK_TEST=1`: press 去下载 after the capture. The handler
    // logs `update open url=… dry_run=1` instead of starting a download.
    if qa_env_flag("BILI_UPDATE_CLICK_TEST") {
        let weak = state.ui.clone();
        slint::Timer::single_shot(Duration::from_millis(1_600), move || {
            if let Some(ui) = weak.upgrade() {
                crate::debug_log("update qa press download dry_run=1");
                ui.invoke_update_download();
            }
        });
    }
}

/// `ShellExecuteW("open", url)` — the shell resolves the default browser and
/// the browser downloads the asset. No `std::process::Command`, no temp file.
fn open_in_browser(url: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let operation = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // <= 32 is the documented failure range for ShellExecute.
    let code = result as isize;
    if code <= 32 {
        crate::debug_log(&format!("update open failed url={url} code={code}"));
    }
}

fn shot(ui: &AppWindow, tag: &str) {
    let path = qa_capture_path(tag);
    match capture_window_png(&ui.window(), &path) {
        Some((width, height)) => crate::debug_log(&format!(
            "update shot tag={tag} path={} {width}x{height}",
            path.display()
        )),
        None => crate::debug_log(&format!("update shot tag={tag} err=capture")),
    }
}

/// The QA switches go through `qa::env`, so a `--no-default-features` build
/// (which compiles the whole QA surface out) has no update hooks either.
fn qa_env_var(name: &str) -> Option<String> {
    qa::env::var(name)
}

fn qa_env_flag(name: &str) -> bool {
    qa::env::flag(name)
}

// ---------------------------------------------------------------------------
// Tests: the version rules the tags rely on, plus the notes stripper.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn cmp(left: &str, right: &str) -> Ordering {
        Version::parse(left)
            .unwrap_or_else(|| panic!("parse {left}"))
            .cmp(&Version::parse(right).unwrap_or_else(|| panic!("parse {right}")))
    }

    #[test]
    fn prerelease_number_compares_numerically() {
        assert_eq!(cmp("v0.2.0-beta.4", "0.2.0-beta.3"), Ordering::Greater);
        assert_eq!(cmp("0.2.0-beta.10", "0.2.0-beta.9"), Ordering::Greater);
        assert_eq!(cmp("0.2.0-beta.4", "0.2.0-beta.4"), Ordering::Equal);
    }

    #[test]
    fn prerelease_is_lower_than_release() {
        assert_eq!(cmp("0.2.0-beta.4", "0.2.0"), Ordering::Less);
        assert_eq!(cmp("v0.2.0", "0.2.0-beta.4"), Ordering::Greater);
        assert_eq!(cmp("0.2.0", "0.2.0"), Ordering::Equal);
    }

    #[test]
    fn prerelease_identifiers_follow_semver() {
        assert_eq!(cmp("0.2.0-alpha", "0.2.0-beta"), Ordering::Less);
        assert_eq!(cmp("0.2.0-alpha.1", "0.2.0-alpha.beta"), Ordering::Less);
        assert_eq!(cmp("0.2.0-beta.2", "0.2.0-beta"), Ordering::Greater);
        // Three fields beat one; leading zeros are just numbers.
        assert_eq!(cmp("0.2.1", "0.2"), Ordering::Greater);
        assert_eq!(cmp("0.02.0", "0.2.0"), Ordering::Equal);
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(cmp("0.2.0-beta.4+abc.1", "0.2.0-beta.4"), Ordering::Equal);
        assert_eq!(cmp("v0.10.0", "0.9.9"), Ordering::Greater);
    }

    #[test]
    fn is_newer_matches_the_release_we_ship() {
        assert_eq!(is_newer("v0.2.0-beta.5", "0.2.0-beta.4"), Some(true));
        assert_eq!(is_newer("v0.2.0-beta.4", "0.2.0-beta.4"), Some(false));
        assert_eq!(is_newer("v0.2.0-beta.3", "0.2.0-beta.4"), Some(false));
        assert_eq!(is_newer("v9.9.9", "0.2.0-beta.4"), Some(true));
        assert_eq!(is_newer("v0.2.0", "0.2.0-beta.4"), Some(true));
        assert_eq!(is_newer("not-a-version", "0.2.0-beta.4"), None);
    }

    #[test]
    fn asset_url_prefers_the_release_json() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"tag_name":"v0.2.0-beta.5","assets":[
                 {"name":"notes.txt","browser_download_url":"https://example.com/notes.txt"},
                 {"name":"o11a1.EXE","browser_download_url":"https://github.com/AntarcticBearHKL/O11A1/releases/download/v0.2.0-beta.5/O11A1.exe"}
               ]}"#,
        )
        .expect("json");
        assert_eq!(
            asset_url(&value, "v0.2.0-beta.5"),
            "https://github.com/AntarcticBearHKL/O11A1/releases/download/v0.2.0-beta.5/O11A1.exe"
        );
        // No matching asset: the canonical path is built from the tag.
        let bare: serde_json::Value =
            serde_json::from_str(r#"{"tag_name":"v0.2.0-beta.5"}"#).expect("json");
        assert_eq!(release_url("v0.2.0-beta.5"), asset_url(&bare, "v0.2.0-beta.5"));
    }

    #[test]
    fn notes_are_plain_and_truncated() {
        let body = "## 修复\n\n- **播放器**：[下载](https://example.com) 更快\n- `prefetch` 修复\n\n<!-- hidden -->\n---\n![cover](https://example.com/a.png)\n> 感谢反馈";
        let notes = plain_notes(body);
        assert!(notes.contains("修复"), "{notes}");
        assert!(notes.contains("· 播放器：下载 更快"), "{notes}");
        assert!(notes.contains("· prefetch 修复"), "{notes}");
        assert!(!notes.contains('#'), "{notes}");
        assert!(!notes.contains("https://"), "{notes}");
        assert!(!notes.contains('*'), "{notes}");
        assert!(!notes.contains('`'), "{notes}");
        assert!(!notes.contains("hidden"), "{notes}");
        assert!(notes.lines().count() <= NOTES_MAX_LINES, "{notes}");

        let long = (0..40)
            .map(|index| format!("- 第 {index} 行"))
            .collect::<Vec<_>>()
            .join("\n");
        let notes = plain_notes(&long);
        assert_eq!(notes.lines().count(), NOTES_MAX_LINES);
        assert!(notes.chars().count() <= NOTES_MAX_CHARS);
    }
}
