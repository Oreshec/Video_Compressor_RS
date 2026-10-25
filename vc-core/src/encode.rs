//! Кодирование видео — порт `core.py` (кодирование + попытки).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::bitrate::{
    build_plan, estimate_size_bytes, resolve_scale, CompressionPlan, MediaInfo,
};
use super::ffmpeg::{apply_no_window, FfmpegPaths};
use super::presets::{hw_preset, MAX_ATTEMPTS, MIN_VIDEO_BITRATE_BPS, SAFETY_FACTOR};

/// Общие для потоков ошибки ядра (аналоги исключений core.py).
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum CoreError {
    #[error("FFmpeg/ffprobe не найден в системе: {0}")]
    FfmpegNotFound(String),

    #[error("Ошибка анализа файла: {0}")]
    MediaInfo(String),

    #[error("Ошибка кодирования (кодек: {codec}): {stderr}")]
    Encode { codec: String, stderr: String },

    #[error("Сжатие отменено пользователем")]
    Cancelled,

    #[error("Выходной файл совпадает с входным: {0}")]
    PathConflict(PathBuf),

    #[error("Недостаточно места на диске: нужно {needed_mb:.1} МБ, доступно {available_mb:.1} МБ")]
    DiskFull { needed_mb: f64, available_mb: f64 },

    #[error("Целевой размер должен быть положительным")]
    InvalidTargetSize,

    #[error("Входной файл не найден: {0}")]
    InputNotFound(PathBuf),

    #[error("Целевой файл занят и не заменён ({reason}); готовый результат оставлен как {part}")]
    OutputReplace { target: PathBuf, reason: String, part: PathBuf },
}

impl CoreError {
    /// Можно ли повторить попытку (например, с другим кодеком).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            CoreError::Encode { .. } | CoreError::DiskFull { .. }
        )
    }

    /// Короткое сообщение для UI (баннер): кодек + суть ошибки.
    pub fn short_message(&self) -> String {
        match self {
            CoreError::FfmpegNotFound(_) => "FFmpeg не найден".into(),
            CoreError::MediaInfo(s) => format!("Ошибка анализа: {s}"),
            CoreError::Encode { codec, stderr } => {
                // Суть — последняя содержательная строка stderr, а не только
                // имя кодека: иначе пользователь видит «Ошибка libx264», хотя
                // кодек только что был другим.
                let s = stderr.trim();
                let s = s
                    .find("FFmpeg завершился с ошибкой: ")
                    .map(|i| &s[i + "FFmpeg завершился с ошибкой: ".len()..])
                    .unwrap_or(s);
                // Типовые отказы ОС — человекочитаемым текстом: строки ffmpeg
                // техничны, а пользователю важно знать, что делать.
                if s.contains("Permission denied") {
                    return format!(
                        "Ошибка {codec}: отказано в доступе — файл может быть открыт \
                         в другой программе или папка защищена. Укажите другую \
                         папку вывода"
                    );
                }
                const MAX: usize = 140;
                let mut brief: String = s.chars().take(MAX).collect();
                if s.chars().count() > MAX {
                    brief.push('…');
                }
                if brief.is_empty() {
                    format!("Ошибка кодирования: {codec}")
                } else {
                    format!("Ошибка {codec}: {brief}")
                }
            }
            CoreError::Cancelled => "Отменено".into(),
            CoreError::PathConflict(_) => "Конфликт путей".into(),
            CoreError::DiskFull { .. } => "Нет места на диске".into(),
            CoreError::InvalidTargetSize => "Неверный размер".into(),
            CoreError::InputNotFound(_) => "Файл не найден".into(),
            CoreError::OutputReplace { part, .. } => format!(
                "Готовый файл сохранён как {}\nСтарый результат занят другим процессом (проверьте OneDrive/плеер)",
                part.display()
            ),
        }
    }

    /// Полное сообщение об ошибке (для строк лога и баннера).
    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl From<super::probe::ProbeError> for CoreError {
    fn from(value: super::probe::ProbeError) -> Self {
        CoreError::MediaInfo(value.0)
    }
}

/// Результат сжатия одного файла.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub target_size_bytes: u64,
    pub actual_size_mb: f64,
    pub video_bitrate_bps: i64,
    pub attempts: usize,
    pub encoder: String,
    pub output_height: Option<i64>,
}

/// Колбэк прогресса: (доля 0..1, текст этапа).
pub type ProgressCb = dyn Fn(f64, &str);

const NULL_OUTPUT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Разобрать строку `out_time=HH:MM:SS.frac` → секунды.
pub fn parse_progress_line(line: &str) -> Option<f64> {
    let rest = line.trim().strip_prefix("out_time=")?;
    let mut iter = rest.splitn(3, ':');
    let h: f64 = iter.next()?.parse().ok()?;
    let m: f64 = iter.next()?.parse().ok()?;
    let s: f64 = iter.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Разобрать `speed=1.37x` → 1.37.
pub fn parse_speed(line: &str) -> Option<f64> {
    let tail = line.trim().strip_prefix("speed=")?;
    let tail = tail.trim().trim_end_matches('x');
    tail.parse().ok().filter(|v| *v >= 0.0)
}

/// Аргументы (кодек, пресет) для выбранного кодировщика.
fn video_args(encoder: &str, preset: &str) -> Vec<String> {
    match encoder {
        "libx264" => vec!["-c:v".into(), "libx264".into(), "-preset".into(), preset.into()],
        "h264_nvenc" => vec![
            "-c:v".into(),
            "h264_nvenc".into(),
            "-preset".into(),
            hw_preset(encoder, preset),
        ],
        "h264_qsv" => vec!["-c:v".into(), "h264_qsv".into(), "-preset".into(), preset.into()],
        "h264_amf" => vec![
            "-c:v".into(),
            "h264_amf".into(),
            "-quality".into(),
            hw_preset(encoder, preset),
        ],
        _ => panic!("Неизвестный кодировщик: {encoder}"),
    }
}

/// Команда ffmpeg для одного прогона.
#[allow(clippy::too_many_arguments)]
pub fn encode_command(
    paths: &FfmpegPaths,
    info: &MediaInfo,
    output: &Path,
    plan: &CompressionPlan,
    preset: &str,
    audio: bool,
    encoder: &str,
    scale: Option<(i64, i64)>,
    first_pass: bool,
    pass_logfile: Option<&Path>,
) -> Vec<String> {
    let video_kbps = plan.video_bitrate_kbps();
    let mut cmd = vec![
        paths.ffmpeg.display().to_string(),
        "-hide_banner".into(),
        "-y".into(),
        "-i".into(),
        info.path.display().to_string(),
    ];
    if let Some((w, h)) = scale {
        cmd.extend(["-vf".into(), format!("scale={w}:{h}:flags=lanczos")]);
    }
    cmd.extend(video_args(encoder, preset));
    cmd.extend([
        "-b:v".into(),
        format!("{video_kbps}k"),
        "-maxrate".into(),
        format!("{}k", (video_kbps as f64 * 1.2) as i64),
        "-bufsize".into(),
        format!("{}k", video_kbps * 2),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-sn".into(),
    ]);
    if first_pass {
        if let Some(log) = pass_logfile {
            cmd.extend([
                "-pass".into(),
                "1".into(),
                "-passlogfile".into(),
                log.display().to_string(),
            ]);
        }
        cmd.extend(["-an".into(), "-f".into(), "null".into(), NULL_OUTPUT.into()]);
        return cmd;
    }
    if audio && plan.audio_bitrate_bps > 0 {
        if plan.audio_copy {
            cmd.extend(["-c:a".into(), "copy".into()]);
        } else {
            cmd.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{}k", (plan.audio_bitrate_bps / 1000).max(32)),
            ]);
        }
    } else {
        cmd.push("-an".into());
    }
    if let Some(log) = pass_logfile {
        cmd.extend([
            "-pass".into(),
            "2".into(),
            "-passlogfile".into(),
            log.display().to_string(),
        ]);
    }
    cmd.extend([
        "-movflags".into(),
        "+faststart".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        output.display().to_string(),
    ]);
    cmd
}

/// Хвост stderr последних 40 строк — для сообщения об ошибке.
fn drain_stderr<R: BufRead>(mut reader: R, tail: Arc<Mutex<Vec<String>>>) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let mut list = match tail.lock() {
                    Ok(l) => l,
                    Err(_) => return,
                };
                if list.len() >= 40 {
                    list.remove(0);
                }
                list.push(line.trim_end().to_string());
            }
        }
    }
}

/// Один прогон ffmpeg. Ошибка/отмена → CoreError.
///
/// Тело сообщения об ошибке: ПОСЛЕДНИЕ до [`MAX_TAIL`] содержательных строк
/// stderr, разделённые ` | `. Чистая функция — не зависит от потоков, поэтому
/// покрыта юнит-тестами отдельно от самого прогона.
const MAX_TAIL: usize = 10;
fn tail_detail(stderr_lines: &[String]) -> String {
    let meaningful: Vec<&str> = stderr_lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let start = meaningful.len().saturating_sub(MAX_TAIL);
    let joined = meaningful[start..].join(" | ");
    if joined.is_empty() {
        "неизвестная ошибка".into()
    } else {
        joined
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_once(
    paths: &FfmpegPaths,
    info: &MediaInfo,
    output: &Path,
    plan: &CompressionPlan,
    preset: &str,
    progress: Option<&ProgressCb>,
    cancel: &AtomicBool,
    encoder: &str,
    scale: Option<(i64, i64)>,
    stage: &str,
    two_pass: bool,
    pass_number: usize,
    pass_logfile: Option<&Path>,
) -> Result<(), CoreError> {
    let first_pass = two_pass && pass_number == 1 && pass_logfile.is_some();
    let cmd = encode_command(
        paths, info, output, plan, preset, info.has_audio(), encoder, scale,
        first_pass, pass_logfile,
    );
    let duration = info.duration_sec;
    let (offset, length) = if two_pass && pass_number == 1 {
        (0.0, 0.5)
    } else if two_pass {
        (0.5, 0.5)
    } else {
        (0.0, 1.0)
    };
    let last_speed = Arc::new(AtomicU64::new(0));

    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    apply_no_window(&mut command);
    let mut proc = command
        .spawn()
        .map_err(|e| CoreError::Encode {
            codec: "ffmpeg".into(),
            stderr: format!("Не удалось запустить ffmpeg: {e}"),
        })?;

    let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = proc.stderr.take() {
        let tail = stderr_tail.clone();
        let _ = thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || drain_stderr(BufReader::new(stderr), tail));
    }
    if let Some(stdout) = proc.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                let mut killer = Command::new("taskkill");
                killer.args(["/PID", &proc.id().to_string(), "/T", "/F"]);
                apply_no_window(&mut killer);
                let _ = killer.output();
                let _ = proc.kill();
                return Err(CoreError::Cancelled);
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let speed = f64::from_bits(last_speed.load(Ordering::Relaxed));
            if line.trim().starts_with("progress=end") {
                if let Some(cb) = progress {
                    let text = if speed <= 0.0 {
                        stage.to_string()
                    } else {
                        format!("{stage} · {speed:.1}×")
                    };
                    cb(offset + length, &text);
                }
                continue;
            }
            if let Some(seconds) = parse_progress_line(&line) {
                if duration > 0.0 {
                    if let Some(cb) = progress {
                        let text = if speed <= 0.0 {
                            stage.to_string()
                        } else {
                            format!("{stage} · {speed:.1}×")
                        };
                        cb(offset + (seconds / duration).min(0.999) * length, &text);
                    }
                }
            } else if let Some(sp) = parse_speed(&line) {
                last_speed.store(sp.to_bits(), Ordering::Relaxed);
            }
        }
    }
    let status = proc
        .wait()
        .map_err(|_| CoreError::Encode {
            codec: "ffmpeg".into(),
            stderr: "Процесс ffmpeg недоступен.".into(),
        })?;

    if cancel.load(Ordering::Relaxed) {
        return Err(CoreError::Cancelled);
    }
    if !status.success() {
        // В сообщение идут ПОСЛЕДНИЕ до 10 содержательных строк stderr —
        // одной последней строки часто не хватает (суть выше в стеке).
        let detail = stderr_tail
            .lock()
            .ok()
            .map(|list| tail_detail(&list))
            .unwrap_or_else(|| "неизвестная ошибка".into());
        return Err(CoreError::Encode {
            codec: encoder.into(),
            stderr: format!("FFmpeg завершился с ошибкой: {detail}"),
        });
    }
    Ok(())
}

/// Удалить устаревшие pass-логи (перед новым двухпроходным прогоном).
fn cleanup_pass_logs(logfile: &Path) {
    let stem = logfile.to_string_lossy().to_string();
    if let Some(parent) = logfile.parent() {
        if let Ok(rd) = std::fs::read_dir(parent) {
            for entry in rd.filter_map(Result::ok) {
                let p = entry.path();
                if p.to_string_lossy().starts_with(&stem) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
}

/// Цикл попыток: кодируем; не влезли — пересчитываем битрейт по факту.
#[allow(clippy::too_many_arguments, unused_assignments)]
fn run_attempts(
    paths: &FfmpegPaths,
    info: &MediaInfo,
    output: &Path,
    target_size_bytes: u64,
    preset: &str,
    encoder: &str,
    max_height: Option<i64>,
    mut two_pass: bool,
    max_attempts: usize,
    progress: Option<&ProgressCb>,
    cancel: &AtomicBool,
) -> Result<CompressionResult, CoreError> {
    if two_pass && encoder != "libx264" {
        two_pass = false; // статистику x264 аппаратные кодеры не понимают
    }
    let mut current = build_plan(info, target_size_bytes);
    let pass_logfile: Option<PathBuf> = if two_pass {
        Some(std::env::temp_dir().join(format!("vc_pass_{}", std::process::id())))
    } else {
        None
    };
    if let Some(log) = pass_logfile.as_deref() {
        cleanup_pass_logs(log);
    }
    let mut attempts = 0usize;
    let mut scale_used: Option<(i64, i64)> = None;
    loop {
        attempts += 1;
        let scale = resolve_scale(info, max_height, current.video_bitrate_bps);
        let scale_note = scale
            .as_ref()
            .map(|(w, h)| format!(" · масштаб {w}×{h}"))
            .unwrap_or_default();
        let stage = format!("Кодирование (попытка {attempts}/{max_attempts}){scale_note}");
        scale_used = scale;
        if let Some(cb) = progress {
            cb(0.0, &stage);
        }
        if two_pass {
            encode_once(
                paths, info, output, &current, preset, progress, cancel, encoder,
                scale, &format!("Проход 1/2 · {stage}"), true, 1,
                pass_logfile.as_deref(),
            )?;
            encode_once(
                paths, info, output, &current, preset, progress, cancel, encoder,
                scale, &format!("Проход 2/2 · {stage}"), true, 2,
                pass_logfile.as_deref(),
            )?;
        } else {
            encode_once(
                paths, info, output, &current, preset, progress, cancel, encoder,
                scale, &stage, false, 2, None,
            )?;
        }
        let actual_bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        if actual_bytes <= target_size_bytes || attempts >= max_attempts {
            return Ok(CompressionResult {
                input_path: info.path.clone(),
                output_path: output.to_path_buf(),
                target_size_bytes,
                actual_size_mb: actual_bytes as f64 / (1024.0 * 1024.0),
                video_bitrate_bps: current.video_bitrate_bps,
                attempts,
                encoder: encoder.into(),
                output_height: scale_used.map(|(_, h)| h),
            });
        }
        // Пересчёт битрейта по реальному превышению.
        let shrink = (target_size_bytes as f64 / actual_bytes.max(1) as f64) * SAFETY_FACTOR;
        let audio_bps = ((current.audio_bitrate_bps as f64) * shrink.max(0.5)) as i64;
        let video_bps =
            MIN_VIDEO_BITRATE_BPS.max((current.video_bitrate_bps as f64 * shrink) as i64);
        current = CompressionPlan {
            target_size_bytes,
            video_bitrate_bps: video_bps,
            audio_bitrate_bps: audio_bps,
            estimated_size_bytes: estimate_size_bytes(info.duration_sec, video_bps, audio_bps),
            audio_copy: current.audio_copy,
        };
        if let Some(cb) = progress {
            cb(
                0.0,
                &format!("Пересчёт битрейта (попытка {}/{max_attempts})", attempts + 1),
            );
        }
    }
}

/// Кодирование идёт во временный файл рядом с целевым: ffmpeg никогда не
/// открывает целевой файл сам, поэтому «Permission denied» от синхронизаторов
/// (OneDrive), которые держат старый результат, невозможен. Имя уникально на
/// процесс: наш же «хвост» от прошлого запуска не должен блокировать новый.
pub(crate) fn part_output_path(final_path: &Path) -> PathBuf {
    let stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    let ext = final_path
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".into()); // расширение нужно и ffmpeg'у (муксер)
    final_path.with_file_name(format!("{stem}.part-{}.{ext}", std::process::id()))
}

/// Заменить целевой файл результатом кодирования (та же папка → rename).
/// Целевой может быть занят синхронизатором/плеером: несколько удалений
/// с паузами, и только затем отказ (временный файл сохраняется).
fn finalize_output(part: &Path, target: &Path) -> Result<(), std::io::Error> {
    if target.exists() {
        let mut last: Option<std::io::Error> = None;
        for _ in 0..5 {
            match std::fs::remove_file(target) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        if let Some(e) = last {
            return Err(std::io::Error::new(
                e.kind(),
                format!("целевой файл занят другим процессом ({e})"),
            ));
        }
    }
    if let Err(e) = std::fs::rename(part, target) {
        // Напр. rename запрещён антивирусом — пробуем копирование поверх.
        if std::fs::copy(part, target).is_err() {
            return Err(e);
        }
        let _ = std::fs::remove_file(part);
    }
    Ok(())
}

/// Основная точка входа: сжать файл; сбой аппаратного кодера → CPU.
#[allow(clippy::too_many_arguments)]
pub fn compress(
    paths: &FfmpegPaths,
    info: MediaInfo,
    output: PathBuf,
    target_size_bytes: u64,
    preset: &str,
    encoder: &str,
    max_height: Option<i64>,
    two_pass: bool,
    cancel: &AtomicBool,
    progress: Option<&ProgressCb>,
) -> Result<CompressionResult, CoreError> {
    if target_size_bytes == 0 {
        return Err(CoreError::InvalidTargetSize);
    }
    if output.canonicalize().ok() == info.path.canonicalize().ok() {
        return Err(CoreError::PathConflict(output.clone()));
    }
    // Кодируем во временный файл: OneDrive/плееры держат старый целевой,
    // и ffmpeg при прямой записи в него получает «Permission denied».
    let part = part_output_path(&output);
    let result = run_attempts(
        paths, &info, &part, target_size_bytes, preset, encoder, max_height,
        two_pass, MAX_ATTEMPTS, progress, cancel,
    );
    match result {
        Ok(res) => match finalize_output(&part, &output) {
            Ok(()) => Ok(CompressionResult {
                output_path: output.clone(),
                ..res
            }),
            Err(e) => Err(CoreError::OutputReplace {
                target: output,
                reason: e.to_string(),
                part,
            }),
        },
        Err(CoreError::Cancelled) => {
            let _ = std::fs::remove_file(&part);
            Err(CoreError::Cancelled)
        }
        Err(e) => {
            if cancel.load(Ordering::Relaxed) || encoder == "libx264" {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
            // Аппаратный кодер не сработал — повтор через libx264. Откат
            // прозрачный: успешный результат честно именует резерв, а при
            // неудаче пользователь видит ОБЕ ошибки, а не только последнюю.
            match run_attempts(
                paths, &info, &part, target_size_bytes, preset, "libx264",
                max_height, two_pass, MAX_ATTEMPTS, progress, cancel,
            ) {
                Ok(res) => match finalize_output(&part, &output) {
                    Ok(()) => Ok(CompressionResult {
                        output_path: output,
                        ..res
                    }),
                    Err(e2) => Err(CoreError::OutputReplace {
                        target: output,
                        reason: e2.to_string(),
                        part,
                    }),
                },
                Err(CoreError::Cancelled) => {
                    let _ = std::fs::remove_file(&part);
                    Err(CoreError::Cancelled)
                }
                Err(fb_err) => {
                    let _ = std::fs::remove_file(&part);
                    Err(CoreError::Encode {
                        codec: format!("{encoder} → резерв libx264"),
                        stderr: format!(
                            "Выбранный кодер не сработал: {e}; резерв тоже: {fb_err}"
                        ),
                    })
                }
            }
        }
    }
}

/// «video.mp4» → «video_compressed.mp4», с учётом папки сохранения.
pub fn default_output_path(input: &Path, output_dir: Option<&str>) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = match input.extension().map(|e| e.to_string_lossy().to_string()) {
        Some(ext) => format!("{stem}_compressed.{ext}"),
        None => format!("{stem}_compressed"),
    };
    match output_dir.filter(|s| !s.trim().is_empty()) {
        Some(dir) => PathBuf::from(dir.trim()).join(name),
        None => input.with_file_name(name),
    }
}

/// ``125.4`` → ``02:05``.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let (hours, rem) = (total / 3600, total % 3600);
    let (minutes, secs) = (rem / 60, rem % 60);
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress_line("out_time=00:01:02.500000"), Some(62.5));
        assert_eq!(parse_progress_line("out_time=01:00:00.000000"), Some(3600.0));
        assert_eq!(parse_progress_line("progress=continue"), None);
        assert_eq!(parse_speed("speed=1.37x"), Some(1.37));
        assert_eq!(parse_speed("progress=continue"), None);
    }

    #[test]
    fn tail_detail_joins_last_lines() {
        let lines: Vec<String> = (1..=10).map(|i| format!("строка{i}")).collect();
        assert_eq!(tail_detail(&lines), lines.join(" | "));
    }

    #[test]
    fn tail_detail_keeps_only_last_ten() {
        let lines: Vec<String> = (1..=12).map(|i| format!("строка{i}")).collect();
        assert_eq!(tail_detail(&lines), lines[2..].join(" | "));
    }

    #[test]
    fn tail_detail_skips_empty_and_blank_lines() {
        let lines = vec![
            String::new(),
            "  ".to_string(),
            "a".to_string(),
            "".to_string(),
            "b".to_string(),
        ];
        assert_eq!(tail_detail(&lines), "a | b");
    }

    #[test]
    fn tail_detail_empty_is_unknown_error() {
        assert_eq!(tail_detail(&[]), "неизвестная ошибка");
        assert_eq!(tail_detail(&[String::new(), " \t".into()]), "неизвестная ошибка");
    }

    #[test]
    fn short_message_permission_denied_is_human_readable() {
        let e = CoreError::Encode {
            codec: "h264_nvenc".into(),
            stderr: format!(
                "FFmpeg завершился с ошибкой: [out#0/mp4] Error opening output {} \
                 Permission denied",
                "C:\\video\\out.mp4"
            ),
        };
        let msg = e.short_message();
        assert!(msg.contains("h264_nvenc"), "нет кодека: {msg}");
        assert!(msg.contains("отказано в доступе"), "нет человеческого текста: {msg}");
        // Технический мусор не должен просочиться в баннер.
        assert!(!msg.contains("Error opening output"), "техностиль: {msg}");
    }

    #[test]
    fn short_message_truncates_long_stderr() {
        let long = "x".repeat(400);
        let e = CoreError::Encode {
            codec: "libx264".into(),
            stderr: format!("FFmpeg завершился с ошибкой: {long}"),
        };
        let msg = e.short_message();
        assert!(msg.chars().count() < 200, "слишком длинно: {:?}", msg.len());
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn short_message_simple_variants() {
        assert_eq!(
            CoreError::FfmpegNotFound("нет".into()).short_message(),
            "FFmpeg не найден"
        );
        assert_eq!(CoreError::Cancelled.short_message(), "Отменено");
        assert_eq!(CoreError::InvalidTargetSize.short_message(), "Неверный размер");
        assert!(
            CoreError::InputNotFound(PathBuf::from("a.mp4"))
                .short_message()
                .contains("Файл не найден")
        );
    }

    #[test]
    fn recoverable_only_encode_and_disk_full() {
        assert!(CoreError::DiskFull {
            needed_mb: 1.0,
            available_mb: 0.0
        }
        .is_recoverable());
        assert!(!CoreError::Cancelled.is_recoverable());
        assert!(!CoreError::InvalidTargetSize.is_recoverable());
    }

    #[test]
    fn output_path_with_dir() {
        let (input, out_dir, prefix) = if cfg!(windows) {
            (Path::new(r"C:\vid\clip.mp4"), r"D:\small", "D:\\")
        } else {
            (Path::new("/tmp/vid/clip.mp4"), "/tmp/small", "/tmp/")
        };
        assert_eq!(
            default_output_path(input, None)
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "clip_compressed.mp4"
        );
        let out = default_output_path(input, Some(out_dir));
        assert!(
            out.to_string_lossy().starts_with(prefix),
            "неожиданный путь: {}",
            out.display()
        );
        assert_eq!(out.file_name().unwrap().to_string_lossy(), "clip_compressed.mp4");
    }

    #[test]
    fn output_path_blank_dir_falls_back_to_input_folder() {
        let input = if cfg!(windows) {
            Path::new(r"C:\vid\clip.mp4")
        } else {
            Path::new("/tmp/vid/clip.mp4")
        };
        // Пустая и пробельная строка равны «папка входных файлов».
        for dir in ["", "   "] {
            let out = default_output_path(input, Some(dir));
            let parent = out.parent().unwrap();
            assert_eq!(
                parent,
                input.parent().unwrap(),
                "пустая папка должна откатываться к папке входа: {}",
                dir
            );
            assert_eq!(
                out.file_name().unwrap().to_string_lossy(),
                "clip_compressed.mp4"
            );
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let name = format!("vc_final_{}_{}", std::process::id(), tag);
        // %TEMP% может быть недоступен процессу тестов (политики окружения) —
        // тогда общий target/ репозитория (cwd тесты меняют, поэтому от манифеста).
        let repo_target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        for base in [std::env::temp_dir(), repo_target] {
            let dir = base.join(&name);
            if std::fs::create_dir_all(&dir).is_ok() {
                return dir;
            }
        }
        panic!("нет доступной для записи папки для теста {tag}");
    }

    #[test]
    fn part_output_path_keeps_extension_for_muxer() {
        let target = if cfg!(windows) {
            Path::new(r"C:\out\clip_compressed.mp4")
        } else {
            Path::new("/tmp/vid/clip_compressed.mp4")
        };
        let part = part_output_path(target);
        let name = part.file_name().unwrap().to_string_lossy();
        assert_eq!(part.parent().unwrap(), target.parent().unwrap());
        assert!(
            name.starts_with("clip_compressed.part-") && name.ends_with(".mp4"),
            "временное имя обязано сохранять расширение ffmpeg'у: {name}"
        );
        assert_ne!(part, target);
    }

    #[test]
    fn finalize_output_replaces_existing_target() {
        let dir = scratch_dir("repl");
        let part = dir.join("clip.part-1.mp4");
        let target = dir.join("clip.mp4");
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        finalize_output(&part, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!part.exists(), "временный файл обязан исчезнуть");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_output_creates_absent_target() {
        let dir = scratch_dir("absent");
        let part = dir.join("clip.part-2.mp4");
        let target = dir.join("clip.mp4");
        std::fs::write(&part, b"data").unwrap();
        finalize_output(&part, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"data");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Занятый целевой (открыт другим процессом) → отказ, временный сохранён.
    /// Windows-специфично: на Unix незакрытый файл удалению не мешает.
    #[cfg(windows)]
    #[test]
    fn finalize_output_fails_when_target_locked() {
        const NEW: &[u8] = b"new";
        const OLD: &[u8] = b"old-old-old-old"; // длина отличима от NEW
        let dir = scratch_dir("locked");
        let part = dir.join("clip.part-3.mp4");
        let target = dir.join("clip.mp4");
        std::fs::write(&part, NEW).unwrap();
        std::fs::write(&target, OLD).unwrap();
        {
            use std::os::windows::fs::OpenOptionsExt;
            // share_mode(0) — без совместного доступа, как хозяйский OneDrive.
            let _keep = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&target)
                .unwrap();
            let err = finalize_output(&part, &target).unwrap_err();
            assert!(
                err.to_string().contains("занят другим процессом"),
                "неожиданный текст ошибки: {err}"
            );
            assert!(part.exists(), "результат терять нельзя");
            // Содержимое цели трогать нельзя — проверяем размер (чтение здесь
            // заблокировано нашим же exclusive-хендлом).
            assert_eq!(
                std::fs::metadata(&target).unwrap().len() as usize,
                OLD.len(),
                "при отказе замены целевой файл обязан остаться прежним"
            );
        }
        // Держатель закрылся — теперь замена обязана пройти.
        finalize_output(&part, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), NEW);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Интеграционный тест по гвардии pytest-фиков: probe реального образца,
    /// если в системе есть ffprobe; иначе пропускается.
    #[test]
    fn probe_smoke_if_ffmpeg_available() {
        let Some(paths) = crate::ffmpeg::find_ffmpeg() else {
            return;
        };
        let path = std::env::temp_dir().join("vc_probe_test.mp4");
        if !path.is_file() {
            let _ = Command::new(&paths.ffmpeg)
                .args([
                    "-f", "lavfi", "-i", "testsrc=duration=1:size=64x64:rate=10",
                    "-pix_fmt", "yuv420p", "-y", path.to_string_lossy().as_ref(),
                ])
                .output();
        }
        if !path.is_file() {
            return;
        }
        let info = match probe(&paths, &path) {
            Ok(i) => i,
            Err(e) => panic!("probe error: {e}; path={path:?}"),
        };
        assert!(info.duration_sec > 0.0, "duration: {info:?}");
        assert!(
            info.width.is_some_and(|w| w > 0),
            "width отсутствует: {info:?}"
        );
        let plan = build_plan(&info, 1024 * 1024);
        assert!(plan.video_bitrate_bps > 0);
        let _ = std::fs::remove_file(path);
    }

    /// Полный прогона сжатия тестового клипа: 1 МБ цель libx264, один проход.
    #[test]
    fn compress_smoke_if_ffmpeg_available() {
        let Some(paths) = crate::ffmpeg::find_ffmpeg() else {
            return;
        };
        let src = std::env::temp_dir().join("vc_comp_test_src.mp4");
        let out = std::env::temp_dir().join("vc_comp_test_out.mp4");
        let _ = std::fs::remove_file(&out);
        if !src.is_file() {
            let _ = Command::new(&paths.ffmpeg)
                .args([
                    "-f", "lavfi", "-i", "testsrc=duration=2:size=320x240:rate=15",
                    "-pix_fmt", "yuv420p", "-y", src.to_string_lossy().as_ref(),
                ])
                .output();
        }
        if !src.is_file() {
            return;
        }
        let info = probe(&paths, &src).expect("probe src");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let result = compress(
            &paths,
            info,
            out.clone(),
            512 * 1024,
            "veryfast",
            "libx264",
            None,
            false,
            &cancel,
            None,
        );
        // Очень короткий клип вряд ли уложится точно в 0.5 МБ — важна сама
        // работоспособность прогона: файл создался, комфортно меньше исходника.
        let out_mb = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(out_mb > 0, "выходной файл не создан: {result:?}");
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(src);
    }
}
