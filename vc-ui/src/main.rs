//! Прототип интерфейса на Slint (SPEC v0.2, Этап 1).
//!
//! UI не вызывает FFmpeg напрямую и не считает параметры — всё через `vc-core`.

// GUI-приложение: без консоли в любом сборке. Вывод паник идёт в panic.log
// (install_panic_log), поэтому диагностика не теряется.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rfd::FileDialog;
use slint::{ComponentHandle, Model, VecModel};

slint::include_modules!();

use vc_core::{find_ffmpeg, probe, usable_encoders, CompressionResult, CoreError, FfmpegPaths};

mod logging;
mod updates;
#[cfg(windows)]
mod win_drop;

use updates::{spawn_update_check, spawn_update_install, CURRENT_VERSION};

/// Прототип: программный рендерер без зависимости от GL/D3D на машине.
/// (femtovg на этой машине даёт пустое белое окно; revisear при оптимизации.)
fn force_software_renderer() {
    std::env::set_var("SLINT_BACKEND", "winit-software");
}

/// Паники — в общий лог (консоли нет).
fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        logging::log(&format!("PANIC: {info}"));
        logging::log(&format!("{info:#?}"));
    }));
}

/// Константы простого режима (SPEC §5.1, §7.3): перевод МБ ↔ байты — на границе UI.
const PROFILE_PRESETS: [&str; 3] = ["slow", "medium", "veryfast"];
const BASE_PRESETS: [&str; 4] = ["veryfast", "faster", "medium", "slow"];
const RESOLUTION_LADDER: [i64; 7] = [0, -1, 2160, 1440, 1080, 720, 480];

/// Запись очереди внутри Rust: путь + результат probe (когда готов).
struct HubItem {
    path: PathBuf,
    info: Option<vc_core::MediaInfo>,
}

/// Общее состояние между потоками и UI.
struct Hub {
    items: Arc<Mutex<Vec<HubItem>>>,
    cancel: Arc<Mutex<Arc<AtomicBool>>>,
    busy: Arc<AtomicBool>,
    ffmpeg: Option<FfmpegPaths>,
}

impl Hub {
    /// Заменить флаг отмены перед новым запуском.
    fn reset_cancel(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.cancel.lock() {
            *guard = flag.clone();
        }
        flag
    }

    /// Текущий флаг отмены (кнопка «Отмена»).
    fn cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        self.cancel.lock().ok().map(|g| g.clone())
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} ГБ", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Строка списка до/после probe.
fn row(path: &Path, status: &str) -> FileInfo {
    FileInfo {
        name: file_name(path).into(),
        meta: "".into(),
        state: 0,
        status: status.into(),
        frac: 0.0,
    }
}

/// Обновлённая строка после probe: мета-строка собирается здесь (в UI — без DSL-интерполяции).
fn probed_row(mut old: FileInfo, info: &vc_core::MediaInfo) -> FileInfo {
    let fps = info
        .fps
        .map(|f| format!("{f:.3} fps"))
        .unwrap_or_else(|| "fps —".to_string());
    old.meta = format!(
        "{} · {} · {} · {fps}",
        vc_core::format_duration(info.duration_sec),
        fmt_size(info.size_bytes),
        info.resolution()
    )
    .into();
    old.state = 0;
    old.status = "В очереди".into();
    old.frac = 0.0;
    old
}

/// Плоский обход папки: только файлы с видеорасширением.
fn folder_videos(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && has_video_ext(&path) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn has_video_ext(path: &Path) -> bool {
    file_name(path)
        .split('.')
        .next_back()
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| {
            vc_core::presets::VIDEO_EXTENSIONS.contains(&ext.as_str())
        })
}

/// Добавить пути в очередь: строки списка + потоки probe.
fn add_files(
    ui: &MainWindow,
    model: &Rc<VecModel<FileInfo>>,
    hub: &Arc<Hub>,
    paths: Vec<PathBuf>,
) {
    for path in paths {
        logging::log(&format!("файл добавлен: {}", path.display()));
        let mut items = match hub.items.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let idx = items.len();
        items.push(HubItem { path: path.clone(), info: None });
        drop(items);

        model.push(row(&path, "анализ…"));
        ui.set_file_count(model.row_count() as i32);
        ui.set_files_label(format!("Файлы ({})", model.row_count()).into());

        let Some(ref ffmpeg) = hub.ffmpeg else {
            model.set_row_data(
                idx,
                FileInfo {
                    state: 4,
                    status: "FFmpeg не найден".into(),
                    ..row(&path, "ошибка")
                },
            );
            continue;
        };

        let weak = ui.as_weak();
        let ffmpeg = ffmpeg.clone();
        let path2 = path.clone();
        let hub2 = Arc::clone(hub);
        std::thread::spawn(move || {
            let info = probe(&ffmpeg, &path2).ok();
            match &info {
                Some(i) => logging::log(&format!(
                    "probe {}: {} с · {} байт · {}{}",
                    path2.display(),
                    i.duration_sec,
                    i.size_bytes,
                    i.resolution(),
                    i.fps
                        .map(|f| format!(" · {f:.3} fps"))
                        .unwrap_or_default()
                )),
                None => logging::log(&format!("probe {} не удался", path2.display())),
            }
            let _ = hub2.items.lock().map(|mut list| {
                if let Some(item) = list.get_mut(idx) {
                    if item.path == path2 {
                        item.info = info.clone();
                    }
                }
            });
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let model = ui.get_files();
                if let Some(old) = model.row_data(idx) {
                    if let Some(info) = &info {
                        model.set_row_data(idx, probed_row(old, info));
                    } else {
                        model.set_row_data(
                            idx,
                            FileInfo {
                                state: 4,
                                status: "Не удалось открыть файл".into(),
                                ..old
                            },
                        );
                    }
                }
            });
        });
    }
}

/// Список файлов через диалог (множественный выбор).
fn pick_files_dialog(ui: &MainWindow, model: &Rc<VecModel<FileInfo>>, hub: &Arc<Hub>) {
    if hub.busy.load(Ordering::Relaxed) {
        return;
    }
    let exts: Vec<&str> = vc_core::presets::VIDEO_EXTENSIONS.to_vec();
    let Some(chosen) = FileDialog::new()
        .add_filter("Видеофайлы", &exts)
        .pick_files()
    else {
        return;
    };
    add_files(ui, model, hub, chosen);
}

/// Список файлов из папки.
fn pick_folder_dialog(ui: &MainWindow, model: &Rc<VecModel<FileInfo>>, hub: &Arc<Hub>) {
    if hub.busy.load(Ordering::Relaxed) {
        return;
    }
    let Some(dir) = FileDialog::new().pick_folder() else {
        return;
    };
    add_files(ui, model, hub, folder_videos(&dir));
}

/// Запуск пакетного сжатия — построчно через `vc_core::compress`.
/// Диагностика отказов записи (логируем, не мешаем пользователю):
/// 1) создаётся ли файл в папке вывода вообще;
/// 2) открывается ли сам целевой файл *_compressed (занят? read-only?).
///
/// Ответы делят «заперта папка» и «занят файл».
fn log_write_probes(input: &Path, out_dir: &str) {
    let target = vc_core::default_output_path(input, Some(out_dir));    let folder = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let probe = folder.join("vc_write_probe.tmp");
    let folder_report = match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            "запись в папку разрешена".to_string()
        }
        Err(e) => format!("запись в папку ОТКЛОНЕНА: {e}"),
    };
    let existed = target.is_file();
    let file_report = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
    {
        Ok(_) => format!(
            "целевой файл {}открывается",
            if existed { "уже существует и " } else { "" }
        ),
        Err(e) => format!(
            "целевой файл {}НЕ открывается: {e}",
            if existed { "существует и " } else { "" }
        ),
    };
    logging::log(&format!(
        "job: диагностика {}: {folder_report}; {file_report}",
        folder.display()
    ));
}

fn start_job(ui: &MainWindow, _model: &Rc<VecModel<FileInfo>>, hub: &Arc<Hub>) {
    let Some(ref ffmpeg) = hub.ffmpeg else {
        ui.set_status_text("FFmpeg не найден — сжатие недоступно".into());
        return;
    };
    if hub.busy.swap(true, Ordering::Relaxed) {
        return; // уже идёт
    }

    let jobs: Vec<(usize, PathBuf, Option<vc_core::MediaInfo>)> = hub
        .items
        .lock()
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, item)| (i, item.path.clone(), item.info.clone()))
                .collect()
        })
        .unwrap_or_default();
    logging::log(&format!(
        "job: файлов в очереди {}",
        jobs.len()
    ));

    ui.set_busy(true);
    ui.set_dock_frac(0.0);

    let advanced = ui.get_advanced();
    let preset = if advanced {
        let idx = (ui.get_preset_index().max(0) as usize).min(BASE_PRESETS.len() - 1);
        BASE_PRESETS[idx].to_string()
    } else {
        let idx = (ui.get_profile_index().max(0) as usize).min(PROFILE_PRESETS.len() - 1);
        PROFILE_PRESETS[idx].to_string()
    };
    // Доступные кодеры: аппаратные + libx264 в конце (vc-core).
    let encoder_list = usable_encoders(ffmpeg);
    let enc_idx = (ui.get_enc_index().max(0) as usize).min(encoder_list.len().saturating_sub(1));
    let encoder = encoder_list[enc_idx].clone();

    let max_height = ui
        .get_resolution_index()
        .max(0)
        .min(RESOLUTION_LADDER.len() as i32 - 1);
    let max_height = {
        let h = RESOLUTION_LADDER[max_height as usize];
        if h > 0 {
            Some(h)
        } else if h == 0 {
            None
        } else {
            Some(-1)
        }
    };
    let two_pass = ui.get_two_pass();
    let target_bytes = (ui.get_size_mb().max(0) as u64) * 1024 * 1024;
    let out_dir = ui.get_output_dir().to_string();
    logging::log(&format!(
        "job: профиль='{}', кодер={}, целевой размер={} МБ, макс-высота={:?}, two_pass={}, папка='{}'",
        preset, encoder, target_bytes / 1024 / 1024, max_height, two_pass, out_dir
    ));

    let cancel = hub.reset_cancel();
    let weak = ui.as_weak();
    let hub2 = Arc::clone(hub);
    let ffmpeg = ffmpeg.clone();
    let total = jobs.len();

    std::thread::spawn(move || {
        let mut ok = 0usize;
        let mut cancelled = false;

        for (pos, (idx, path, info)) in jobs.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            // Мета-данные: из probe или быстрый перепрогон.
            let Some(info) = info.or_else(|| probe(&ffmpeg, &path).ok()) else {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let model = ui.get_files();
                    if let Some(mut old_row) = model.row_data(idx) {
                        old_row.state = 4;
                        old_row.status = "Не удалось открыть файл".into();
                        model.set_row_data(idx, old_row);
                    }
                });
                continue;
            };

            let output = vc_core::default_output_path(&path, Some(&out_dir));
            logging::log(&format!(
                "job: файл {} → выход {}",
                path.display(),
                output.display()
            ));

            let weak2 = weak.clone();
            let progress: Option<&vc_core::encode::ProgressCb> =
                Some(&move |frac: f64, msg: &str| {
                    let frac = frac as f32;
                    let msg = msg.to_string();
                    let _ = weak2.upgrade_in_event_loop(move |ui| {
                        let model = ui.get_files();
                        if let Some(mut old_row) = model.row_data(idx) {
                            old_row.state = 2;
                            old_row.frac = frac;
                            old_row.status = msg.clone().into();
                            model.set_row_data(idx, old_row);
                        }
                        ui.set_dock_frac((pos as f32 + frac) / total.max(1) as f32);
                        ui.set_status_text(
                            format!("Файл {} из {}: {msg}", pos + 1, total.max(1)).into(),
                        );
                    });
                });

            let result: Result<CompressionResult, CoreError> = vc_core::compress(
                &ffmpeg,
                info.clone(),
                output,
                target_bytes,
                &preset,
                &encoder,
                max_height,
                two_pass,
                &cancel,
                progress,
            );

            let weak3 = weak.clone();
            let result_ok = result.is_ok();
            // Формируем строки статуса на потоке задачи: замыкание должно быть 'static.
            let (state, status, err_full) = match &result {
                // В статусе — фактический кодер: при откате на резерв видно, чем сжали.
                Ok(res) => (
                    3i32,
                    format!("Готово · {:.1} МБ · {}", res.actual_size_mb, res.encoder),
                    None,
                ),
                Err(CoreError::Cancelled) => (5, "Отменено".to_string(), None),
                Err(e) => (4, e.short_message(), Some(e.message())),
            };
            match &result {
                Ok(res) => logging::log(&format!(
                    "job: готово {} → {} · {:.1} МБ · попыток {} · кодер {}",
                    path.display(),
                    res.output_path.display(),
                    res.actual_size_mb,
                    res.attempts,
                    res.encoder
                )),
                Err(e) => {
                    logging::log(&format!("job: ошибка {}: {e}", path.display()));
                    log_write_probes(&path, &out_dir);
                }
            }
            let _ = weak3.upgrade_in_event_loop(move |ui| {
                let model = ui.get_files();
                if let Some(mut old_row) = model.row_data(idx) {
                    old_row.state = state;
                    old_row.status = status.into();
                    model.set_row_data(idx, old_row);
                }
                if let Some(err) = err_full {
                    ui.set_status_text(err.into());
                }
                let done = if result_ok { 1.0 } else { 0.0 };
                ui.set_dock_frac((pos as f32 + done) / total.max(1) as f32);
            });
            if result_ok {
                ok += 1;
            }
            if matches!(result, Err(CoreError::Cancelled)) {
                cancelled = true;
                break;
            }
        }

        let weak = weak.clone();
        let hub2 = hub2.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            hub2.busy.store(false, Ordering::Relaxed);
            ui.set_dock_frac(0.0);
            let final_status: slint::SharedString = if cancelled {
                "Отменено".into()
            } else if ok == total {
                "Готово: все файлы сжаты".into()
            } else {
                format!("Готово: {ok}/{total}, с ошибками").into()
            };
            ui.set_status_text(final_status);
        });
    });
}

fn main() {
    install_panic_log();
    force_software_renderer();
    logging::log(&format!(
        "=== старт vc-ui {CURRENT_VERSION} (SLINT_BACKEND={:?}, advanced={:?}, basic-open={:?}) ===",
        std::env::var("SLINT_BACKEND").ok(),
        std::env::var("VC_UI_ADVANCED").ok(),
        std::env::var("VC_UI_BASIC_OPEN").ok(),
    ));
    let app = MainWindow::new().expect("Не удалось создать окно");

    // Версия + авто-обновление.
    app.set_version_label(format!("Версия {CURRENT_VERSION}").into());
    spawn_update_check(app.as_weak(), true);

    // Отладка вёрстки: VC_UI_BASIC_OPEN=1 / VC_UI_ADVANCED=1 → старт в другом состоянии.
    if std::env::var("VC_UI_BASIC_OPEN").as_deref() == Ok("1") {
        app.set_basic_open(true);
    }
    if std::env::var("VC_UI_ADVANCED").as_deref() == Ok("1") {
        app.set_advanced(true);
    }

    // FFmpeg: найден в системе / рядом с программой.
    let ffmpeg = find_ffmpeg();
    let encoders = ffmpeg
        .as_ref()
        .map(usable_encoders)
        .unwrap_or_default();
    app.set_has_ffmpeg(ffmpeg.is_some());
    app.set_encoder_note(
        if encoders.is_empty() {
            "FFmpeg не найден в системе или рядом с программой".to_string()
        } else {
            format!("Доступны: {}", encoders.join(", "))
        }
        .into(),
    );
    logging::log(&format!(
        "ffmpeg: {}",
        ffmpeg
            .as_ref()
            .map(|f| f.ffmpeg.display().to_string())
            .unwrap_or_else(|| "не найден".into())
    ));
    if !encoders.is_empty() {
        logging::log(&format!("доступные кодеры: {}", encoders.join(", ")));
    }

    // Модель строк списка.
    let model: Rc<VecModel<FileInfo>> = Rc::new(VecModel::default());
    app.set_files(model.clone().into());
    // Модели списков настроек.
    app.set_profile_models(
        Rc::new(VecModel::from(
            ["Качество", "Баланс", "Скорость"]
                .iter()
                .map(|s| (*s).into())
                .collect::<Vec<slint::SharedString>>(),
        ))
        .into(),
    );
    app.set_preset_models(
        Rc::new(VecModel::from(
            BASE_PRESETS
                .iter()
                .map(|s| (*s).into())
                .collect::<Vec<slint::SharedString>>(),
        ))
        .into(),
    );
    app.set_resolution_models(
        Rc::new(VecModel::from(
            ["Как есть", "Авто", "2160p", "1440p", "1080p", "720p", "480p"]
                .iter()
                .map(|s| (*s).into())
                .collect::<Vec<slint::SharedString>>(),
        ))
        .into(),
    );
    app.set_encoder_models(
        Rc::new(VecModel::from(
            encoders
                .iter()
                .map(|s| s.as_str().into())
                .collect::<Vec<slint::SharedString>>(),
        ))
        .into(),
    );

    let hub = Arc::new(Hub {
        items: Arc::new(Mutex::new(Vec::new())),
        cancel: Arc::new(Mutex::new(Arc::new(AtomicBool::new(false)))),
        busy: Arc::new(AtomicBool::new(false)),
        ffmpeg,
    });

    let ui = app.as_weak();
    let model2 = model.clone();
    let hub2 = Arc::clone(&hub);
    app.on_pick_files(move || {
        pick_files_dialog(&ui.unwrap(), &model2, &hub2);
    });

    let ui = app.as_weak();
    let model2 = model.clone();
    let hub2 = Arc::clone(&hub);
    app.on_pick_folder(move || {
        pick_folder_dialog(&ui.unwrap(), &model2, &hub2);
    });

    let ui = app.as_weak();
    app.on_pick_output_folder(move || {
        if let Some(dir) = FileDialog::new().pick_folder() {
            ui.unwrap().set_output_dir(dir.display().to_string().into());
        }
    });

    let ui = app.as_weak();
    let model2 = model.clone();
    let hub2 = Arc::clone(&hub);
    app.on_clear_files(move || {
        model2.set_vec(Vec::new());
        let _ = hub2.items.lock().map(|mut list| list.clear());
        ui.unwrap().set_file_count(0);
        ui.unwrap().set_files_label("Файлы".into());
        ui.unwrap().set_status_text("Готов к работе".into());
    });

    let ui = app.as_weak();
    let model2 = model.clone();
    let hub2 = Arc::clone(&hub);
    app.on_start_job(move || {
        start_job(&ui.unwrap(), &model2, &hub2);
    });

    let ui = app.as_weak();
    let hub2 = Arc::clone(&hub);
    app.on_cancel_job(move || {
        if hub2.busy.load(Ordering::Relaxed) {
            if let Some(flag) = hub2.cancel_flag() {
                flag.store(true, Ordering::Relaxed);
            }
            ui.unwrap().set_status_text("Отмена…".into());
        }
    });

    let ui = app.as_weak();
    app.on_check_updates(move || {
        let ui = ui.unwrap().as_weak();
        spawn_update_check(ui, false);
    });
    let ui = app.as_weak();
    app.on_install_update(move || {
        logging::log("updates: установка запущена пользователем");
        spawn_update_install(ui.unwrap().as_weak());
    });

    // Drag&drop из ОС (Slint пока не передаёт WM_DROPFILES — см. win_drop).
    #[cfg(windows)]
    win_drop::install(&app, &model, &hub);

    app.run().unwrap();
    logging::log("=== приложение закрыто ===");
}
