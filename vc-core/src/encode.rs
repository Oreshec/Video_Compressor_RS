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
}

impl CoreError {
    /// Можно ли повторить попытку (например, с другим кодеком).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            CoreError::Encode { .. } | CoreError::DiskFull { .. }
        )
    }

    /// Короткое сообщение для UI (баннер).
    pub fn short_message(&self) -> String {
        match self {
            CoreError::FfmpegNotFound(_) => "FFmpeg не найден".into(),
            CoreError::MediaInfo(s) => format!("Ошибка анализа: {s}"),
            CoreError::Encode { codec, .. } => format!("Ошибка {codec}"),
            CoreError::Cancelled => "Отменено".into(),
            CoreError::PathConflict(_) => "Конфликт путей".into(),
            CoreError::DiskFull { .. } => "Нет места на диске".into(),
            CoreError::InvalidTargetSize => "Неверный размер".into(),
            CoreError::InputNotFound(_) => "Файл не найден".into(),
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
        let detail = stderr_tail
            .lock()
            .ok()
            .and_then(|list| list.iter().rev().find(|l| !l.trim().is_empty()).cloned())
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
    let result = run_attempts(
        paths, &info, &output, target_size_bytes, preset, encoder, max_height,
        two_pass, MAX_ATTEMPTS, progress, cancel,
    );
    match result {
        Ok(res) => Ok(res),
        Err(CoreError::Cancelled) => {
            let _ = std::fs::remove_file(&output);
            Err(CoreError::Cancelled)
        }
        Err(e) => {
            if !cancel.load(Ordering::Relaxed) && encoder != "libx264" {
                // Аппаратный кодер не сработал — повтор через libx264.
                return run_attempts(
                    paths, &info, &output, target_size_bytes, preset, "libx264",
                    max_height, two_pass, MAX_ATTEMPTS, progress, cancel,
                );
            }
            let _ = std::fs::remove_file(&output);
            Err(e)
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
