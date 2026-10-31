//! 设置 → 「语音识别模型」：状态、下载、取消、删除、目录与源站。
//!
//! 这一节只管 whisper.cpp 的 GGML 文件（`models/whisper/whisper.cpp/ggml-*.bin`），
//! 也就是发行包不再携带的那 466 MB。它读的全是磁盘事实（[`models_dl::list`]），
//! **启动时不做任何网络探测**：没装模型就是「未安装」，不联网、不报错。
//!
//! 下载在自己的线程里跑（[`models_dl::download`]），进度按 1 Hz 通过
//! `invoke_from_event_loop` 回到 UI 线程整页重画 —— 四行的状态都由
//! [`models_dl::list`] 一次算出来，所以「下载中 42% · 3.2 MB/s」和进度条
//! 永远是同一份数据。

use crate::*;

use std::sync::atomic::AtomicBool;

/// 一行文案：名字、体积、状态、按钮，以及进度条的比例。
fn build_row(status: &models_dl::Status) -> ModelRow {
    let model = status.model;
    let installed = status.state == models_dl::State::Installed;
    if status.downloading() {
        let pct = status.pct().unwrap_or(0);
        let speed = status.speed().unwrap_or(0.0);
        return ModelRow {
            name: model.name.into(),
            size: model.size_text().into(),
            state: format!("下载中 {pct}% · {}", models_dl::speed_text(speed)).into(),
            installed: false,
            busy: true,
            pct: (pct as f32 / 100.0).clamp(0.0, 1.0),
            action: "取消".into(),
        };
    }
    let (state, pct, action, busy) = match status.state {
        models_dl::State::Installed => ("已安装", 1.0, "删除", false),
        models_dl::State::Partial => (
            "已暂停",
            (status.have as f32 / status.total().max(1) as f32).clamp(0.0, 1.0),
            "下载",
            false,
        ),
        models_dl::State::Missing => ("未安装", 0.0, "下载", false),
    };
    ModelRow {
        name: model.name.into(),
        size: model.size_text().into(),
        state: state.into(),
        installed,
        busy,
        pct,
        action: action.into(),
    }
}

/// 整页重画：四行状态 + 目录 + 当前使用 + 源站 + 「耳与眼」那两行旧属性。
pub(crate) fn publish_models(ui: &AppWindow, store: &Arc<Mutex<settings::Settings>>) {
    let prefs = store
        .lock()
        .map(|prefs| prefs.clone())
        .unwrap_or_default();
    let root = models_dl::root_dir(&prefs.fish_asr_model_dir);
    let configured = settings::normalise_fish_asr_model(&prefs.fish_asr_model);
    let statuses = models_dl::list(&root);

    let rows: Vec<ModelRow> = statuses.iter().map(build_row).collect();
    ui.set_fish_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_fish_models_dir(root.display().to_string().into());
    ui.set_fish_model_mirror(
        settings::normalise_fish_model_mirror(&prefs.fish_model_mirror).into(),
    );

    // 当前使用：设置里的档位装了就是它，否则已安装里最大的那一档。
    let configured_status = statuses
        .iter()
        .find(|status| status.model.name == configured);
    let configured_installed = configured_status
        .map(|status| status.state == models_dl::State::Installed)
        .unwrap_or(false);
    let current = if configured_installed {
        configured.clone()
    } else {
        models_dl::largest_installed(&root)
            .map(|model| model.name.to_string())
            .unwrap_or_else(|| configured.clone())
    };
    ui.set_fish_models_current(
        if configured_installed || models_dl::largest_installed(&root).is_some() {
            current.clone().into()
        } else {
            format!("{current}（未安装）").into()
        },
    );

    // 「耳与眼」里的旧行（下载按钮 + 进度条）读的是这三个属性：让它们跟着
    // 这一节走，页面上不会出现两个互相矛盾的进度。
    let downloading = models_dl::running();
    let progress_text = match downloading.and_then(models_dl::info) {
        Some(model) => {
            let status = models_dl::status(&root, model);
            format!(
                "{} 下载中 {}% · {}",
                model.name,
                status.pct().unwrap_or(0),
                models_dl::speed_text(status.speed().unwrap_or(0.0))
            )
        }
        None if configured_installed => match models_dl::info(&configured) {
            Some(model) => format!("{} 模型已校验（{}）", model.name, model.size_text()),
            None => "模型已安装".to_string(),
        },
        None => match models_dl::info(&configured) {
            Some(model) if model_path_exists(&root, model) => {
                format!("{} 模型体积不对，建议重新下载", model.name)
            }
            Some(model) => format!(
                "{} 模型未下载（{}）",
                model.name,
                models_dl::size_text(model.bytes)
            ),
            None => "没有可用的模型档位".to_string(),
        },
    };
    ui.set_fish_model_progress(progress_text.into());
    ui.set_fish_model_ready(configured_installed);
    ui.set_fish_model_frac(match downloading.and_then(models_dl::info) {
        Some(model) => models_dl::status(&root, model).pct().unwrap_or(0) as f32 / 100.0,
        None if configured_installed => 1.0,
        None => 0.0,
    });

    // 说明行：只有下载中或「装了但引擎没用它」时才出现。
    let note = if let Some(name) = downloading {
        let pct = models_dl::info(name)
            .map(|model| models_dl::status(&root, model).pct().unwrap_or(0))
            .unwrap_or(0);
        format!("正在下载 {name} · {pct}% · 可随时取消，已经下到的部分会保留")
    } else if !last_error().is_empty() {
        format!("上次下载失败：{}（已下载的部分会续传，可重试）", last_error())
    } else {
        let config = fish::watch_config(&prefs);
        let whisper_cpp = fishwatch::asr::spec_for(&config)
            .map(|spec| spec.engine == "whisper.cpp")
            .unwrap_or(false);
        if !whisper_cpp && configured_installed {
            "当前引擎不是 whisper.cpp：装好的模型要在「耳与眼」里把引擎改成 whisper.cpp（或自动）才会被使用"
                .to_string()
        } else {
            String::new()
        }
    };
    ui.set_fish_models_note(note.into());
}

fn model_path_exists(root: &std::path::Path, model: &models_dl::ModelInfo) -> bool {
    std::fs::metadata(models_dl::model_path(root, model)).is_ok()
}

/// 接线。`fish` 只用来在设置变化后让引擎重读配置；线程里只带走
/// `Arc<Mutex<Settings>>`。
pub(crate) fn models_install(ui: &AppWindow, fish: Rc<FishUi>) {
    let store = fish.settings_store.clone();
    publish_models(ui, &store);

    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_fish_model_dl(move |name| {
            start_download(&weak, &store, name.as_str());
        });
    }
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_fish_model_cancel(move || {
            if models_dl::cancel() {
                crate::debug_log("models dl cancel requested");
            }
            if let Some(ui) = weak.upgrade() {
                publish_models(&ui, &store);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_fish_model_delete(move |name| {
            let root = current_root(&store);
            match models_dl::delete(&root, name.as_str()) {
                Ok(bytes) => crate::debug_log(&format!(
                    "models dl removed name={} bytes={bytes}",
                    name.as_str()
                )),
                Err(err) => crate::debug_log(&format!(
                    "models dl remove failed name={} err={err:#}",
                    name.as_str()
                )),
            }
            if let Some(ui) = weak.upgrade() {
                publish_models(&ui, &store);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_fish_model_goto(move || {
            // 「去安装模型」：养鱼页本来就靠折叠组来展开内容，这里只做同一件事
            // —— 把这一段展开，并把占地方最大的「配置」收起来，让这一段进到
            // 视口里。不新造导航，也不自己算滚动位置。
            if let Some(ui) = weak.upgrade() {
                ui.set_fish_open_config(false);
                ui.set_fish_open_models(true);
                crate::debug_log("models goto models=1 config=0");
            }
        });
    }
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let fish = fish.clone();
        ui.on_fish_model_pick_dir(move || {
            let current = current_root(&store);
            let Some(dir) = pick_folder(&current) else {
                return;
            };
            let text = dir.display().to_string();
            update_settings(&store, |prefs| prefs.fish_asr_model_dir = text.clone());
            if let Some(engine) = fish.engine.as_ref() {
                engine.refresh_from_settings();
            }
            crate::debug_log(&format!("models dir set path={text}"));
            if let Some(ui) = weak.upgrade() {
                publish_models(&ui, &store);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let fish = fish.clone();
        ui.on_fish_model_reset_dir(move || {
            update_settings(&store, |prefs| prefs.fish_asr_model_dir = String::new());
            if let Some(engine) = fish.engine.as_ref() {
                engine.refresh_from_settings();
            }
            crate::debug_log(&format!(
                "models dir reset default={}",
                models_dl::root_dir("").display()
            ));
            if let Some(ui) = weak.upgrade() {
                publish_models(&ui, &store);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let fish = fish.clone();
        ui.on_fish_set_model_mirror(move |value| {
            let mirror = settings::normalise_fish_model_mirror(value.as_str());
            update_settings(&store, |prefs| prefs.fish_model_mirror = mirror.clone());
            if let Some(engine) = fish.engine.as_ref() {
                engine.refresh_from_settings();
            }
            crate::debug_log(&format!("models mirror={mirror}"));
            if let Some(ui) = weak.upgrade() {
                publish_models(&ui, &store);
            }
        });
    }

    spawn_qa_hook(ui, &store);
}

fn current_root(store: &Arc<Mutex<settings::Settings>>) -> std::path::PathBuf {
    let configured = store
        .lock()
        .map(|prefs| prefs.fish_asr_model_dir.clone())
        .unwrap_or_default();
    models_dl::root_dir(&configured)
}

/// 上一次下载失败的原因：备注行里说清楚为什么停住，下一次点下载时清空。
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn remember_error(text: &str) {
    if let Ok(mut slot) = LAST_ERROR.lock() {
        slot.clear();
        slot.push_str(text);
    }
}

fn last_error() -> String {
    LAST_ERROR
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_default()
}

fn start_download(weak: &Weak<AppWindow>, store: &Arc<Mutex<settings::Settings>>, name: &str) {
    if let Some(running) = models_dl::running() {
        crate::debug_log(&format!(
            "models dl ignored running={running} requested={name}"
        ));
        return;
    }
    let Some(model) = models_dl::info(name) else {
        crate::debug_log(&format!("models dl ignored unknown={name}"));
        return;
    };
    let prefs = store
        .lock()
        .map(|prefs| prefs.clone())
        .unwrap_or_default();
    let root = models_dl::root_dir(&prefs.fish_asr_model_dir);
    let mirror = models_dl::Mirror::from_setting(&prefs.fish_model_mirror);
    let flag = Arc::new(AtomicBool::new(false));
    let name = model.name;
    remember_error("");
    let weak_thread = weak.clone();
    let store_thread = store.clone();
    if let Some(ui) = weak.upgrade() {
        publish_models(&ui, &store);
    }
    std::thread::spawn(move || {
        crate::media_hooks::note_thread_role("models-dl");
        {
            let publish_weak = weak_thread.clone();
            let publish_store = store_thread.clone();
            let mut sink = move |event: models_dl::Event| {
                let weak = publish_weak.clone();
                let store = publish_store.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    match event {
                        models_dl::Event::Progress => {}
                        models_dl::Event::Done { bytes } => {
                            crate::debug_log(&format!(
                                "models ui done name={name} bytes={bytes}"
                            ));
                            adopt_downloaded(&ui, &store, name);
                        }
                        models_dl::Event::Paused { kept } => crate::debug_log(&format!(
                            "models ui paused name={name} kept={kept}"
                        )),
                        models_dl::Event::Failed { error } => {
                            crate::debug_log(&format!(
                                "models ui failed name={name} err={error}"
                            ));
                            remember_error(&error);
                        }
                    }
                    publish_models(&ui, &store);
                });
            };
            let result = models_dl::download(&root, name, mirror, flag, &mut sink);
            match result {
                Ok(outcome) if outcome.paused() => {
                    crate::debug_log(&format!("models dl paused name={name}"));
                }
                Ok(_) => crate::debug_log(&format!("models dl finished name={name}")),
                Err(err) => crate::debug_log(&format!(
                    "models dl ended name={name} err={err:#}"
                )),
            }
        }
        let weak = weak_thread.clone();
        let store = store_thread.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| publish_models(&ui, &store));
    });
}

/// 下载完成之后让「装好的这一档」真的会被用上。
///
/// 只在两种情况下动设置，别的时候一个字节都不改：
///
/// * 设置里选的那一档在磁盘上没有可用文件 → 现在下好的这一档成为「当前使用」；
/// * 解析出来的引擎会跑别的模型（比如 `auto` 落到 faster-whisper，而它自己的
///   模型没装）→ 改成 `whisper.cpp`，因为刚下好的就是 whisper.cpp 的 GGML 文件。
///
/// `refresh_from_settings()` 让正在跑的养鱼引擎立刻读到新的路径/档位。
fn adopt_downloaded(ui: &AppWindow, store: &Arc<Mutex<settings::Settings>>, name: &str) {
    let _ = ui;
    let prefs = store
        .lock()
        .map(|prefs| prefs.clone())
        .unwrap_or_default();
    let root = models_dl::root_dir(&prefs.fish_asr_model_dir);
    let configured = settings::normalise_fish_asr_model(&prefs.fish_asr_model);
    let configured_ready = fishwatch::asr::models::spec(&configured, "whisper.cpp")
        .map(|spec| fishwatch::asr::models::model_ready(&root, spec))
        .unwrap_or(false);
    if !configured_ready {
        update_settings(store, |prefs| prefs.fish_asr_model = name.to_string());
        crate::debug_log(&format!("models dl adopt model={name}"));
    }
    let config = fish::watch_config(&prefs);
    let resolved = fishwatch::asr::spec_for(&config);
    let resolved_ready = resolved
        .map(|spec| fishwatch::asr::models::model_ready(&root, spec))
        .unwrap_or(false);
    let on_whisper_cpp = resolved.map(|spec| spec.engine == "whisper.cpp").unwrap_or(false);
    if !on_whisper_cpp && !resolved_ready && prefs.fish_asr_engine != "whisper.cpp" {
        update_settings(store, |prefs| prefs.fish_asr_engine = "whisper.cpp".to_string());
        crate::debug_log(&format!("models dl adopt engine=whisper.cpp for={name}"));
    }
    if let Some(fish) = crate::FISH_UI.with(|slot| slot.borrow().clone()) {
        if let Some(engine) = fish.engine.as_ref() {
            engine.refresh_from_settings();
        }
    }
}

// ---------------------------------------------------------------------------
// 目录选择（原生 shell 对话框，UI 线程上模态）
// ---------------------------------------------------------------------------

/// `SHBrowseForFolderW`：选一个目录，返回 `None` 表示用户取消。
///
/// 用 shell32 的这个老对话框而不是 `IFileOpenDialog`：它不需要 `windows`
/// crate 的 COM 接口面，也不需要自己写 `IShellItem` 的封装；调用期间是标准
/// 的模态窗口，和系统的「选择文件夹」表现一致。
#[cfg(windows)]
fn pick_folder(initial: &std::path::Path) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE,
        BIF_RETURNONLYFSDIRS, BROWSEINFOW,
    };

    // The UI thread already runs an apartment-threaded COM apartment in the
    // player paths; balancing this one only when it succeeded keeps it safe.
    let initialized = unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0
    };

    let mut display = vec![0u16; 1024];
    // `lpszTitle` must outlive the call; a fixed buffer is enough for the label.
    let title: Vec<u16> = "选择语音识别模型目录\0".encode_utf16().collect();
    let _ = initial;
    let browse = BROWSEINFOW {
        hwndOwner: std::ptr::null_mut(),
        pidlRoot: std::ptr::null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
        lpfn: None,
        lParam: 0,
        iImage: 0,
    };
    let pidl = unsafe { SHBrowseForFolderW(&browse) };
    let picked = if pidl.is_null() {
        None
    } else {
        let mut buffer = vec![0u16; 4096];
        let ok = unsafe { SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) } != 0;
        unsafe { CoTaskMemFree(pidl as *const std::ffi::c_void) };
        if ok {
            let len = buffer.iter().position(|ch| *ch == 0).unwrap_or(0);
            Some(std::path::PathBuf::from(String::from_utf16_lossy(
                &buffer[..len],
            )))
        } else {
            None
        }
    };
    if initialized {
        unsafe { CoUninitialize() };
    }
    picked
}

#[cfg(not(windows))]
fn pick_folder(_initial: &std::path::Path) -> Option<std::path::PathBuf> {
    None
}

// ---------------------------------------------------------------------------
// QA：`BILI_F206_TEST=<tag>[,key=value...]`
// ---------------------------------------------------------------------------
//
// 开发工具，不是产品代码。打开养鱼页 → 展开「语音识别模型」→ 滚到这一段 →
// （可选）触发一次真实下载 / 取消 → 在 `shot_ms` 时把窗口快照写进
// `BILI_CAPTURE_DIR/<tag>.png`。三个验收截图就是这样来的：
//
//   BILI_F206_TEST=f206_models_empty        （模型目录指向一个空目录）
//   BILI_F206_TEST=f206_models_downloading,dl=small,cancel_ms=20000,shot_ms=15000
//   BILI_F206_TEST=f206_models_installed    （模型目录指向真实目录）
//
// `quit=1` 让进程在截图后自己退出，脚本不必去杀进程。

fn spawn_qa_hook(ui: &AppWindow, store: &Arc<Mutex<settings::Settings>>) {
    let Some(spec) = crate::qa::env::var("BILI_F206_TEST") else {
        return;
    };
    let mut tag = String::new();
    let mut dl: Option<String> = None;
    let mut cancel_ms: Option<u64> = None;
    let mut shot_ms: u64 = 4_000;
    let mut quit = false;
    // `eyes=1` keeps 耳与眼 expanded (the no-model hint line lives there);
    // `goto=1` presses the page's own 「去安装模型」 path (expand + scroll)
    // instead of replaying wheel input.
    let mut eyes = false;
    let mut goto = false;
    for (index, part) in spec.split(',').enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if index == 0 && !part.contains('=') {
            tag = part.to_string();
            continue;
        }
        let (key, value) = match part.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        match key.trim() {
            "dl" => dl = Some(value.trim().to_string()),
            "cancel_ms" => cancel_ms = value.trim().parse().ok(),
            "shot_ms" => shot_ms = value.trim().parse().unwrap_or(shot_ms),
            "quit" => quit = value.trim() == "1",
            "eyes" => eyes = value.trim() == "1",
            "goto" => goto = value.trim() == "1",
            _ => {}
        }
    }
    if tag.is_empty() {
        tag = "f206_models".to_string();
    }
    let weak = ui.as_weak();
    let store = store.clone();
    let started = Instant::now();
    let shot_taken = Rc::new(Cell::new(false));
    let download_started = Rc::new(Cell::new(false));
    let cancel_sent = Rc::new(Cell::new(false));
    let goto_sent = Rc::new(Cell::new(false));
    let timer = Rc::new(slint::Timer::default());
    keep_qa_timer(timer.clone());
    let timer_a = timer.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(ui) = weak.upgrade() else {
                timer_a.stop();
                return;
            };
            let elapsed = started.elapsed();

            // 1 · 养鱼页 + 展开这一段（顺带收起别的折叠组，省滚动）。
            if ui.get_view_mode() != 20 {
                ui.invoke_fish_open();
            }
            if !ui.get_fish_open_models() && !goto {
                ui.set_fish_open_models(true);
            }
            // `eyes=1` 把「耳与眼」也展开：没有模型时那句提示和「去安装模型」
            // 就在那一组的 ASR 行里。
            if eyes && !ui.get_fish_open_eyes() {
                ui.set_fish_open_eyes(true);
            } else if !eyes && ui.get_fish_open_eyes() {
                ui.set_fish_open_eyes(false);
            }
            if goto && !goto_sent.get() && elapsed >= Duration::from_millis(4_000) {
                // 和「去安装模型」按下时完全同一条路径。
                goto_sent.set(true);
                crate::debug_log("f206 qa goto models");
                ui.invoke_fish_model_goto();
            }

            // 2 · 滚到底：真输入（和 f199 同一个 PointerScrolled 回放），
            //     不碰内部元素，也不会因为内容高度变化而算错位置。分很多个
            //     小 notch（每 250ms 8 个），而不是一次 40×400px —— Flickable
            //     只按每个事件走一格，一次性的大 delta 会被丢掉。截图前 1.2s
            //     停止，让惯性滚动先停下。
            if !goto
                && elapsed >= Duration::from_millis(800)
                && elapsed + Duration::from_millis(1_200)
                    < Duration::from_millis(shot_ms)
            {
                let size = ui.window().size();
                let position = slint::LogicalPosition::new(
                    size.width as f32 / 2.0,
                    size.height as f32 / 2.0,
                );
                for _ in 0..8 {
                    ui.window().dispatch_event(
                        slint::platform::WindowEvent::PointerScrolled {
                            position,
                            delta_x: 0.0,
                            delta_y: -240.0,
                        },
                    );
                }
            }

            // 3 · 可选：点「下载」（走的是真实回调，不是内部函数）。
            if let Some(name) = dl.as_ref() {
                if !download_started.get() && elapsed >= Duration::from_millis(1_500) {
                    download_started.set(true);
                    crate::debug_log(&format!("f206 qa download start name={name}"));
                    ui.invoke_fish_model_dl(name.as_str().into());
                }
            }

            // 4 · 可选：到点取消（断点续传的实测就用这一条）。
            if let Some(ms) = cancel_ms {
                if !cancel_sent.get() && elapsed >= Duration::from_millis(ms) {
                    cancel_sent.set(true);
                    crate::debug_log(&format!("f206 qa cancel at_ms={ms}"));
                    ui.invoke_fish_model_cancel();
                }
            }

            // 5 · 截图（一次），顺带把这一行的真实状态写进日志。
            if !shot_taken.get() && elapsed >= Duration::from_millis(shot_ms) {
                shot_taken.set(true);
                let path = qa_capture_path(&tag);
                match capture_window_png(&ui.window(), &path) {
                    Some((width, height)) => crate::debug_log(&format!(
                        "f206 shot tag={tag} png={} size={width}x{height} rows={} current={} note={}",
                        path.display(),
                        ui.get_fish_models().row_count(),
                        ui.get_fish_models_current(),
                        ui.get_fish_models_note(),
                    )),
                    None => crate::debug_log("f206 shot png=FAILED"),
                }
                publish_models(&ui, &store);
                if quit {
                    crate::debug_log("f206 shot quit=1");
                    let _ = slint::quit_event_loop();
                }
            }
        },
    );
}
