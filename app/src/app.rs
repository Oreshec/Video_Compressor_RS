// ---------------------------------------------------------------------------- //
//  GUI на egui: одно окно, адаптивная вёрстка, нативный drag-drop и DPI.        //
//  Палитра и последовательность элементов повторяют Dear PyGui-версию (gui.py). //
// ---------------------------------------------------------------------------- //

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;

use vc_core::{
    bitrate::build_plan,
    compress, default_output_path,
    encode::ProgressCb,
    find_ffmpeg, format_duration, probe, usable_encoders, CoreError, FfmpegPaths,
    presets::{encoder_label, ENCODER_LABELS, SPEED_CHOICES, VIDEO_EXTENSIONS},
    CompressionPlan, CompressionResult, MediaInfo,
};
use crate::settings::{self, Settings};

// -------- палитра из gui.py ------------------------------------------------- //
const BG: egui::Color32 = egui::Color32::from_rgb(10, 11, 15);
const CARD: egui::Color32 = egui::Color32::from_rgb(16, 17, 23);
const DROP_ZONE: egui::Color32 = egui::Color32::from_rgb(12, 13, 18);
const BORDER: egui::Color32 = egui::Color32::from_rgb(42, 44, 56);
const TEXT: egui::Color32 = egui::Color32::from_rgb(236, 239, 244);
const MUTED: egui::Color32 = egui::Color32::from_rgb(124, 128, 138);
const BTN: egui::Color32 = egui::Color32::from_rgb(20, 21, 28);
const BTN_HOVER: egui::Color32 = egui::Color32::from_rgb(32, 34, 44);
const BTN_ACTIVE: egui::Color32 = egui::Color32::from_rgb(43, 46, 60);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(125, 211, 252); // #7dd3fc
const OK: egui::Color32 = egui::Color32::from_rgb(110, 231, 183);
const ERR: egui::Color32 = egui::Color32::from_rgb(252, 165, 165);
const WARN: egui::Color32 = egui::Color32::from_rgb(253, 230, 138);

/// Высота пустой зоны перетаскивания — «ограничить высоту», не на всю панель.
const DROP_ZONE_HEIGHT: f32 = 170.0;

// ---------------------------------------------------------------------------- //
//  Состояние строк списка файлов
// ---------------------------------------------------------------------------- //

type ProbeSlot = Arc<Mutex<Option<Result<MediaInfo, String>>>>;
type ResultSlot = Arc<Mutex<Option<CompressionResult>>>;

struct FileRow {
    path: PathBuf,
    probe: ProbeSlot,
    result: ResultSlot,
}

impl FileRow {
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

// ---------------------------------------------------------------------------- //
//  Состояние фоновой задачи сжатия
// ---------------------------------------------------------------------------- //

#[derive(Default)]
struct JobShared {
    frac: f32,
    stage: String,
    done: bool,
    message: String,
}

impl JobShared {
    fn set(&mut self, frac: f64, stage: &str) {
        self.frac = frac as f32;
        self.stage = stage.to_string();
    }
}

// ---------------------------------------------------------------------------- //
//  Приложение
// ---------------------------------------------------------------------------- //

pub struct VcApp {
    settings: Settings,
    files: Vec<FileRow>,
    ffmpeg: Option<FfmpegPaths>,
    encoders: Vec<String>,
    encoders_rx: Option<Receiver<Vec<String>>>,
    job: Option<Arc<Mutex<JobShared>>>,
    log_rx: Option<Receiver<String>>,
    log_lines: Vec<String>,
    /// Итоговое сообщение после завершения задачи (успех/отмена/ошибка).
    banner: Option<(String, egui::Color32)>,
    cancel: Arc<AtomicBool>,
    /// Размер под контроллер DragValue (зеркало settings.size).
    size_mb: f64,
}

impl VcApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        load_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);

        let settings = settings::load();
        let size_mb = settings.target_size();

        // Поиск ffmpeg — быстрый, делаем синхронно; детект кодеров — в фоне.
        let ffmpeg = find_ffmpeg();
        let encoders_rx = ffmpeg.as_ref().map(|paths| {
            let (tx, rx) = channel();
            let paths = paths.clone();
            thread::Builder::new()
                .name("encoder-detect".to_string())
                .spawn(move || {
                    let _ = tx.send(usable_encoders(&paths));
                })
                .ok();
            rx
        });

        Self {
            settings,
            files: Vec::new(),
            ffmpeg,
            encoders: Vec::new(),
            encoders_rx,
            job: None,
            log_rx: None,
            log_lines: Vec::new(),
            banner: None,
            cancel: Arc::new(AtomicBool::new(false)),
            size_mb,
        }
    }

    // ------------------------------------------------------------ вспом. --- //

    fn add_log(&mut self, line: String) {
        if self.log_lines.len() >= 500 {
            self.log_lines.remove(0);
        }
        self.log_lines.push(line);
    }

    fn push_files(&mut self, paths: Vec<PathBuf>) {
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            self.banner = Some((
                "FFmpeg не найден — добавьте его в PATH или папку ffmpeg/ рядом с программой."
                    .into(),
                ERR,
            ));
            return;
        };
        for path in paths {
            // Без дубликатов.
            if self
                .files
                .iter()
                .any(|r| r.path == path || r.path.canonicalize().ok() == path.canonicalize().ok())
            {
                continue;
            }
            let filter = path
                .extension()
                .map(|e| {
                    let lower = e.to_string_lossy().to_lowercase();
                    VIDEO_EXTENSIONS.contains(&lower.as_str())
                })
                .unwrap_or(false);
            if !filter {
                continue;
            }
            let probe_slot: ProbeSlot = Arc::new(Mutex::new(None));
            let result: ResultSlot = Arc::new(Mutex::new(None));
            let slot = probe_slot.clone();
            let label = row_name(&path);
            let ffmpeg = ffmpeg.clone();
            let probe_path = path.clone();
            thread::Builder::new()
                .name("ffprobe".to_string())
                .spawn(move || {
                    let value = match probe(&ffmpeg, &probe_path) {
                        Ok(info) => Ok(info),
                        Err(e) => Err(e.to_string()),
                    };
                    *slot.lock().unwrap() = Some(value);
                })
                .ok();
            self.add_log(format!("Добавлен: {label}"));
            self.files.push(FileRow {
                path,
                probe: probe_slot,
                result,
            });
        }
        self.save_settings();
    }

    /// Синхронизировать DragValue с настройками и записать на диск.
    fn save_settings(&mut self) {
        self.settings.size = self.size_mb;
        settings::save(&self.settings);
    }

    /// Разрешить «авто» кодировщика в конкретный, доступный в этом ffmpeg.
    fn resolve_encoder(&self) -> String {
        let wanted = self.settings.encoder_setting();
        if wanted == "auto" {
            const PRIORITY: [&str; 3] = ["h264_nvenc", "h264_qsv", "h264_amf"];
            return PRIORITY
                .into_iter()
                .find(|enc| self.encoders.contains(&enc.to_string()))
                .unwrap_or("libx264")
                .into();
        }
        wanted
    }

    fn ready_files(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                matches!(row.probe.lock().unwrap().as_ref(), Some(Ok(_)))
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn start_job(&mut self) {
        if self.job.is_some() || self.ffmpeg.is_none() {
            return;
        }
        let ready = self.ready_files();
        if ready.is_empty() {
            return;
        }
        self.banner = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(JobShared {
            frac: 0.0,
            stage: "Подготовка…".into(),
            done: false,
            message: String::new(),
        }));

        let (log_tx, log_rx) = channel();
        self.log_rx = Some(log_rx);

        let settings = self.settings.clone();
        let ffmpeg = self.ffmpeg.clone().unwrap();
        let resolved_encoder = self.resolve_encoder();
        let cancel = self.cancel.clone();
        let shared_ref = shared.clone();

        let rows: Vec<(PathBuf, ProbeSlot, ResultSlot)> = ready
            .into_iter()
            .map(|i| {
                let row = &self.files[i];
                (row.path.clone(), row.probe.clone(), row.result.clone())
            })
            .collect();

        thread::Builder::new()
            .name("compress-job".to_string())
            .spawn(move || {
                let total = rows.len();
                for (idx, (path, probe_slot, result_slot)) in rows.into_iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let media = match probe_slot.lock().unwrap().take() {
                        Some(Ok(info)) => info,
                        _ => continue,
                    };
                    let header = if total > 1 {
                        format!("Файл {}/{} · {}", idx + 1, total, row_name(&path))
                    } else {
                        row_name(&path)
                    };
                    shared_ref.lock().unwrap().set(0.0, &header);

                    let output =
                        default_output_path(&path, output_dir_or_none(&settings.output_dir));
                    let target = settings.target_size_bytes();
                    let preset = settings.speed_preset();
                    let max_height = settings.max_height();
                    let two_pass = settings.two_pass;
                    let cancel_flag = cancel.clone();
                    let shared_for_cb = shared_ref.clone();
                    let log_for_cb = log_tx.clone();
                    let header_for_cb = header.clone();
                    let progress: Box<ProgressCb> = Box::new(move |frac: f64, stage: &str| {
                        shared_for_cb
                            .lock()
                            .unwrap()
                            .set(frac, &format!("{header_for_cb} · {stage}"));
                        let _ = log_for_cb.send(format!("{header_for_cb} · {stage}"));
                    });

                    let result = compress(
                        &ffmpeg,
                        media,
                        output,
                        target,
                        &preset,
                        &resolved_encoder,
                        max_height,
                        two_pass,
                        &cancel_flag,
                        Some(progress.as_ref()),
                    );

                    match result {
                        Ok(res) => {
                            *result_slot.lock().unwrap() = Some(res.clone());
                            let _ = log_tx.send(format!(
                                "Готово: {} → {} ({:.1} МБ, {})",
                                row_name(&path),
                                res.output_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                                res.actual_size_mb,
                                encoder_label(&res.encoder),
                            ));
                        }
                        Err(e) => {
                            let _ = log_tx
                                .send(format!("Ошибка: {}: {}", row_name(&path), e.message()));
                            if e == CoreError::Cancelled {
                                cancel_flag.store(true, Ordering::Relaxed);
                                break;
                            }
                            *shared_ref.lock().unwrap() = JobShared {
                                message: format!("{header}: {}", e.message()),
                                ..Default::default()
                            };
                        }
                    }
                }
                shared_ref.lock().unwrap().done = true;
            })
            .ok();
        self.job = Some(shared);
    }

    /// Опрос фоновой задачи (лог, детект кодеров, прогресс).
    fn poll_job(&mut self, ctx: &egui::Context) {
        // Приём лога.
        let mut new_lines = Vec::new();
        if let Some(rx) = self.log_rx.as_ref() {
        while let Ok(line) = rx.try_recv() {
            new_lines.push(line);
        }
        }
        for line in new_lines {
            self.add_log(line);
        }
        // Детект кодеров.
        if let Some(rx) = self.encoders_rx.as_ref() {
            match rx.try_recv() {
                Ok(list) => {
                    self.encoders = list;
                    self.encoders_rx = None;
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Err(TryRecvError::Disconnected) => self.encoders_rx = None,
            }
        }
        // Прогресс задачи.
        if let Some(job) = self.job.clone() {
            let guard = job.lock().unwrap();
            if guard.done {
                self.banner = Some(if guard.message.is_empty() {
                    ("Сжатие завершено.".into(), OK)
                } else {
                    (guard.message.clone(), ERR)
                });
                drop(guard);
                self.job = None;
                self.log_rx = None;
            } else {
                drop(guard);
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
        }
    }

    fn open_file(&self, path: &Path) {
        #[cfg(windows)]
        let _ = Command::new("explorer").arg(path).spawn();
        #[cfg(target_os = "macos")]
        let _ = Command::new("open").arg(path).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = Command::new("xdg-open").arg(path).spawn();
    }

    fn reveal_in_explorer(&self, path: &Path) {
        #[cfg(windows)]
        let _ = Command::new("explorer").arg("/select,").arg(path).spawn();
        #[cfg(not(windows))]
        if let Some(parent) = path.parent() {
            self.open_file(parent);
        }
    }
}

/// Имя файла пути (короткая утилита).
fn row_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Пустая строка → None (папка сохранения не задана).
fn output_dir_or_none(s: &str) -> Option<&str> {
    match s.trim().is_empty() {
        true => None,
        false => Some(s),
    }
}

// ---------------------------------------------------------------------------- //
//  Стиль
// ---------------------------------------------------------------------------- //

fn load_fonts(ctx: &egui::Context) {
    // Windows: системный Segoe UI (кириллица есть) и Consolas для лога.
    // На Linux/macOS egui использует встроенные Noto Sans / Noto Sans Mono.
    let mut fonts = egui::FontDefinitions::default();
    let regular = "C:\\Windows\\Fonts\\segoeui.ttf";
    let mono = "C:\\Windows\\Fonts\\consola.ttf";
    if let Ok(bytes) = std::fs::read(regular) {
        fonts
            .font_data
            .insert("segoe".into(), egui::FontData::from_owned(bytes).into());
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            list.insert(0, "segoe".into());
        }
    }
    if let Ok(bytes) = std::fs::read(mono) {
        fonts
            .font_data
            .insert("consolas".into(), egui::FontData::from_owned(bytes).into());
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            list.insert(0, "consolas".into());
        }
    }
    // На не-Windows egui сам подхватит системные шрифты через fontconfig / Core Text.
    ctx.set_fonts(fonts);
}

fn apply_theme(ctx: &egui::Context) {
    ctx.all_styles_mut(|st| {
        let v = &mut st.visuals;
        v.panel_fill = BG;
        v.window_fill = BG;
        v.extreme_bg_color = DROP_ZONE;
        v.override_text_color = Some(TEXT);
        v.widgets.noninteractive.bg_fill = CARD;
        v.widgets.noninteractive.fg_stroke.color = MUTED;
        v.widgets.inactive.bg_fill = BTN;
        v.widgets.inactive.fg_stroke.color = TEXT;
        v.widgets.hovered.bg_fill = BTN_HOVER;
        v.widgets.hovered.fg_stroke.color = TEXT;
        v.widgets.active.bg_fill = BTN_ACTIVE;
        v.widgets.active.fg_stroke.color = TEXT;
        v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(125, 211, 252, 40);
        v.selection.stroke.color = ACCENT;
    });
}

// ---------------------------------------------------------------------------- //
//  Отрисовка
// ---------------------------------------------------------------------------- //

impl eframe::App for VcApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);

        // Drag & drop — нативный приём.
        let dropped: Vec<PathBuf> = ctx
            .input(|i| i.raw.dropped_files.clone())
            .into_iter()
            .map(|f| f.path().to_path_buf())
            .collect();
        if !dropped.is_empty() {
            self.push_files(dropped);
        }

        // Док прижат к низу: прогресс + главные кнопки всегда на виду.
        egui::Panel::bottom("dock")
            .resizable(false)
            .frame(
                egui::Frame::default()
                    .fill(CARD)
                    .inner_margin(10.0),
            )
            .show(ui, |ui| self.draw_dock(ui));

        egui::CentralPanel::default().show(ui, |ui| self.draw_main(ui));
    }
}

impl VcApp {
    // --------------------------------------------------------------- док --- //

    fn draw_dock(&mut self, ui: &mut egui::Ui) {
        let job = self.job.clone();
        let (frac, stage) = job
            .as_ref()
            .map(|j| {
                let g = j.lock().unwrap();
                (g.frac, g.stage.clone())
            })
            .unwrap_or((0.0, String::new()));

        if job.is_some() {
            ui.add(
                egui::ProgressBar::new(frac)
                    .show_percentage()
                    .desired_height(16.0),
            );
            ui.label(egui::RichText::new(stage).small().color(MUTED));
        } else if let Some((text, color)) = &self.banner {
            ui.label(egui::RichText::new(text).color(*color).small());
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let busy = job.is_some();
            let ready = self.ready_files().len();
            if ui
                .add_enabled(
                    !busy && ready > 0,
                    egui::Button::new(
                        egui::RichText::new(format!("Сжать видео ({ready})"))
                            .strong()
                            .color(BG),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(150.0, 28.0)),
                )
                .clicked()
            {
                self.start_job();
            }

            if ui
                .add_enabled(
                    busy,
                    egui::Button::new(egui::RichText::new("Отмена").color(ERR))
                        .min_size(egui::vec2(80.0, 28.0)),
                )
                .clicked()
            {
                // Флаг под задачей: её поток увидит на ближайшей проверке.
                self.cancel.store(true, Ordering::Relaxed);
            }

            // Открыть папку последнего результата.
            let last_output = self
                .files
                .iter()
                .rev()
                .find_map(|row| row.result.lock().unwrap().clone())
                .map(|r| r.output_path);
            if ui
                .add_enabled(last_output.is_some(), egui::Button::new("Открыть папку"))
                .clicked()
            {
                if let Some(path) = last_output {
                    self.reveal_in_explorer(&path);
                }
            }

            // Статус справа: FFmpeg + доступные кодеры.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let note = match &self.ffmpeg {
                    Some(_) => {
                        let enc = self.resolve_encoder();
                        if self.encoders.is_empty() {
                            format!("FFmpeg найден · {}", encoder_label(&enc))
                        } else {
                            format!(
                                "FFmpeg найден · аппаратных: {}",
                                self.encoders.len().saturating_sub(1)
                            )
                        }
                    }
                    None => "FFmpeg не найден".into(),
                };
                ui.label(
                    egui::RichText::new(note)
                        .small()
                        .color(if self.ffmpeg.is_some() { OK } else { MUTED }),
                );
            });
        });
    }

    // ------------------------------------------------------ центральная --- //

    fn draw_main(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.draw_header(ui);
                ui.add_space(6.0);
                self.draw_files_card(ui);
                ui.add_space(6.0);
                self.draw_params_card(ui);
                ui.add_space(6.0);
                self.draw_details_card(ui);
                ui.add_space(4.0);
            });
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Video Compressor RS")
                    .heading()
                    .strong()
                    .color(TEXT),
            );
            ui.label(
                egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                    .small()
                    .color(MUTED),
            );
        });
    }

    fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
        egui::Frame::default()
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(10.0)
            .show(ui, |ui| add(ui));
    }

    fn draw_files_card(&mut self, ui: &mut egui::Ui) {
        Self::card(ui, |ui| {
            // Счётчик и сумма — до действия, чтобы не пересчитывать по ходу.
            let files = self.files.len();
            let total_in: f64 = self
                .files
                .iter()
                .filter_map(|r| r.probe.lock().unwrap().clone())
                .filter_map(|p| p.ok())
                .map(|info| info.size_mb())
                .sum();

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Файлы").strong());
                if ui.button("Добавить…").clicked() {
                    self.pick_files_dialog();
                }
                if ui
                    .add_enabled(files > 0, egui::Button::new("Очистить"))
                    .clicked()
                {
                    self.files.clear();
                    self.save_settings();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let summary = if files == 0 {
                        "файлов нет".to_string()
                    } else {
                        format!("Всего: {total_in:.1} МБ · файлов: {files}")
                    };
                    ui.label(egui::RichText::new(summary).small().color(MUTED));
                });
            });
            ui.add_space(4.0);

            if self.files.is_empty() {
                self.draw_drop_zone(ui);
            } else {
                self.draw_file_rows(ui);
            }
        });
    }

    /// Пустая зона перетаскивания фиксированной высоты, идеально центрированная
    /// и по горизонтали, и по вертикали.
    fn draw_drop_zone(&mut self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), DROP_ZONE_HEIGHT),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect.expand(1.5));
        painter.rect_filled(rect, egui::CornerRadius::same(8), DROP_ZONE);
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(8),
            egui::Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );
        let center = rect.center();
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            "Перетащите видеофайлы в окно",
            egui::TextStyle::Heading.resolve(ui.style()),
            TEXT,
        );
        painter.text(
            center + egui::vec2(0.0, 26.0),
            egui::Align2::CENTER_CENTER,
            "MP4 · MKV · MOV · AVI · WEBM · WMV · FLV и другие",
            egui::TextStyle::Small.resolve(ui.style()),
            MUTED,
        );
    }

    fn draw_file_rows(&mut self, ui: &mut egui::Ui) {
        // Удаление отложено до конца кадра: remove прямо во время цикла
        // отрисовки Grid смещал индексы — окно падало без сообщения.
        let mut remove_idx: Option<usize> = None;
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .id_salt("files_scroll")
            .show(ui, |ui| {
                egui::Grid::new("files_grid")
                    .num_columns(6)
                    .spacing([10.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for i in 0..self.files.len() {
                            self.draw_file_row(ui, i, &mut remove_idx);
                            ui.end_row();
                        }
                    });
            });
        if let Some(i) = remove_idx {
            if i < self.files.len() {
                self.add_log(format!("Убран: {}", self.files[i].name()));
                self.files.remove(i);
                self.save_settings();
            }
        }
    }

    fn draw_file_row(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        remove_idx: &mut Option<usize>,
    ) {
        let row = &self.files[i];
        let name = row.name();
        // 1: имя файла.
        ui.label(name);

        let probe_state = row.probe.lock().unwrap().clone();
        match probe_state {
            None => {
                // 2
                ui.label(egui::RichText::new("анализ…").color(MUTED).small());
                // 3–5
                ui.label("");
                ui.label("");
                ui.label("");
            }
            Some(Err(msg)) => {
                // 2
                ui.label(egui::RichText::new("—").color(MUTED).small());
                // 3
                ui.colored_label(ERR, egui::RichText::new(msg).small());
                // 4–5
                ui.label("");
                ui.label("");
            }
            Some(Ok(info)) => {
                // 2: было.
                ui.label(format!(
                    "{} · {:.1} МБ",
                    format_duration(info.duration_sec),
                    info.size_mb()
                ));
                // 3: план под текущие настройки.
                let plan = build_plan(&info, self.settings.target_size_bytes());
                ui.label(
                    egui::RichText::new(plan_note(&plan))
                        .small()
                        .color(if plan.low_bitrate() { WARN } else { MUTED }),
                );
                // 4: оценка.
                ui.label(
                    egui::RichText::new(format!(
                        "≈ {:.1} МБ",
                        plan.estimated_size_bytes as f64 / (1024.0 * 1024.0)
                    ))
                    .small()
                    .color(if plan.low_bitrate() { WARN } else { OK }),
                );
            }
        }
        // 5: статус.
        let result = row.result.lock().unwrap().clone();
        if let Some(res) = &result {
            ui.label(
                egui::RichText::new(format!(
                    "✓ {:.1} МБ ({} пр.)",
                    res.actual_size_mb, res.attempts
                ))
                .small()
                .color(OK),
            );
        } else {
            ui.label("");
        }
        // 6: действия.
        ui.horizontal(|ui| {
            if ui.button("Открыть").clicked() {
                match &result {
                    Some(res) => self.open_file(&res.output_path),
                    None => self.open_file(&self.files[i].path),
                }
            }
            if ui.button("×").clicked() {
                *remove_idx = Some(i);
                ui.ctx().request_repaint();
            }
        });
    }

    // --------------------------------------------------------- параметры --- //

    fn draw_params_card(&mut self, ui: &mut egui::Ui) {
        Self::card(ui, |ui| {
            ui.label(egui::RichText::new("Настройки").strong());
            ui.add_space(2.0);

            // Строка 1: размер + пресеты скорости и кодировщика.
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Целевой размер").color(MUTED).small());
                if ui
                    .add(
                        egui::DragValue::new(&mut self.size_mb)
                            .speed(0.5)
                            .range(0.1..=8192.0)
                            .suffix(" МБ")
                            .max_decimals(1),
                    )
                    .changed()
                {
                    self.save_settings();
                }
                for size in [5.0, 10.0, 25.0, 50.0, 100.0, 250.0] {
                    if ui
                        .button(format!("{size:.0}"))
                        .on_hover_text(format!("Установить {size:.0} МБ"))
                        .clicked()
                    {
                        self.size_mb = size;
                        self.save_settings();
                    }
                }
                ui.separator();
                ui.label(egui::RichText::new("Скорость").color(MUTED).small());
                let current = self.settings.speed_preset();
                let label_of = |p: &str| {
                    SPEED_CHOICES
                        .iter()
                        .find(|(_, v)| *v == p)
                        .map(|(l, _)| *l)
                        .unwrap_or(p)
                        .to_string()
                };
                egui::ComboBox::from_id_salt("speed_combo")
                    .selected_text(label_of(&current))
                    .show_ui(ui, |ui| {
                        for (label, value) in SPEED_CHOICES {
                            ui.selectable_value(&mut self.settings.speed, value.to_string(), label);
                        }
                    });
                ui.separator();
                ui.label(egui::RichText::new("Кодировщик").color(MUTED).small());
                egui::ComboBox::from_id_salt("enc_combo")
                    .selected_text(
                        ENCODER_LABELS
                            .iter()
                            .find(|(_, v)| *v == self.settings.encoder)
                            .map(|(l, _)| *l)
                            .unwrap_or("Авто (аппаратный, если есть)"),
                    )
                    .show_ui(ui, |ui| {
                        for (label, value) in ENCODER_LABELS {
                            ui.selectable_value(
                                &mut self.settings.encoder,
                                value.to_string(),
                                label,
                            );
                        }
                    });
            });

            // Строка 2: разрешение + два прохода.
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Разрешение").color(MUTED).small());
                egui::ComboBox::from_id_salt("res_combo")
                    .selected_text(match self.settings.resolution.as_str() {
                        "" | "source" => "Исходное разрешение",
                        "auto" => "Авто (по битрейту)",
                        "2160" => "≤2160p",
                        "1440" => "≤1440p",
                        "1080" => "≤1080p",
                        "720" => "≤720p",
                        "480" => "≤480p",
                        _ => "Исходное разрешение",
                    })
                    .show_ui(ui, |ui| {
                        for (label, value) in [
                            ("Исходное разрешение", "source"),
                            ("Авто (по битрейту)", "auto"),
                            ("≤2160p", "2160"),
                            ("≤1440p", "1440"),
                            ("≤1080p", "1080"),
                            ("≤720p", "720"),
                            ("≤480p", "480"),
                        ] {
                            ui.selectable_value(
                                &mut self.settings.resolution,
                                value.to_string(),
                                label,
                            );
                        }
                    });
                ui.separator();
                if ui
                    .checkbox(&mut self.settings.two_pass, "Два прохода (точнее размер)")
                    .changed()
                {
                    self.save_settings();
                }
            });

            // Строка 3: папка сохранения.
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Папка сохранения").color(MUTED).small());
                let dir = self.settings.output_dir.clone();
                let display = if dir.trim().is_empty() {
                    "рядом с исходными файлами".to_string()
                } else {
                    dir.clone()
                };
                ui.label(
                    egui::RichText::new(display)
                        .small()
                        .color(MUTED)
                        .monospace(),
                );
                if ui.button("Обзор…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.settings.output_dir = dir.display().to_string();
                        self.save_settings();
                    }
                }
                if ui
                    .add_enabled(!dir.is_empty(), egui::Button::new("Сброс"))
                    .clicked()
                {
                    self.settings.output_dir.clear();
                    self.save_settings();
                }
            });
        });
    }

    // ------------------------------------------------------ подробности --- //

    fn draw_details_card(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.settings.details, "Подробности (лог)")
            .changed()
        {
            self.save_settings();
        }
        if self.settings.details {
            egui::Frame::default()
                .fill(DROP_ZONE)
                .stroke(egui::Stroke::new(1.0, BORDER))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(8.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .stick_to_bottom(true)
                        .id_salt("log_scroll")
                        .show(ui, |ui| {
                            if self.log_lines.is_empty() {
                                ui.label(
                                    egui::RichText::new("Журнал пуст.").small().color(MUTED),
                                );
                            } else {
                                ui.push_id("log", |ui| {
                                    for line in &self.log_lines {
                                        ui.label(
                                            egui::RichText::new(line)
                                                .monospace()
                                                .small()
                                                .color(TEXT),
                                        );
                                    }
                                });
                            }
                        });
                });
        }
    }

    fn pick_files_dialog(&mut self) {
        let extensions: Vec<&str> = VIDEO_EXTENSIONS.to_vec();
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Выберите видеофайлы")
            .add_filter("Видеофайлы", &extensions)
            .pick_files()
        {
            self.push_files(paths);
        }
    }
}

/// Заметка плана: видео битрейт + аудио.
fn plan_note(plan: &CompressionPlan) -> String {
    let audio = if plan.audio_bitrate_bps > 0 {
        if plan.audio_copy {
            format!("аудио копия {}k", plan.audio_bitrate_bps / 1000)
        } else {
            format!("aac {}k", plan.audio_bitrate_bps / 1000)
        }
    } else {
        "без звука".into()
    };
    format!("{}k видео · {audio}", plan.video_bitrate_kbps())
}
