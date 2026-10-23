//! Поиск ffmpeg/ffprobe — порт `ffmpeg_tools.py` (без автозагрузки).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Пути к экзешникам FFmpeg.
#[derive(Debug, Clone)]
pub struct FfmpegPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Папка с программой: рядом с exe.
pub fn program_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}

fn portable_dir() -> PathBuf {
    program_dir().join("ffmpeg")
}

/// `CREATE_NO_WINDOW` — дочерние процессы не должны мигать консолями
/// (пользователь видел CMD-окна при каждом анализе файла).
#[cfg(windows)]
pub fn apply_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
pub fn apply_no_window(_cmd: &mut Command) {}

/// Поиск по PATH вручную (без внешних крейтов).
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

fn find_one(names: &[&str]) -> Option<PathBuf> {
    let mut dirs = vec![portable_dir().join("bin"), portable_dir()];
    if let Some(exe) = which("ffmpeg") {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    for dir in &dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    names.iter().find_map(|name| which(name))
}

static CACHE: Mutex<Option<FfmpegPaths>> = Mutex::new(None);

/// Найти ffmpeg и ffprobe; результат кэшируется.
pub fn find_ffmpeg() -> Option<FfmpegPaths> {
    let mut cache = CACHE.lock().ok()?;
    if cache.is_none() {
        let ffmpeg = find_one(&["ffmpeg.exe", "ffmpeg"]);
        let ffprobe = find_one(&["ffprobe.exe", "ffprobe"]);
        *cache = ffmpeg.zip(ffprobe).map(|(ffmpeg, ffprobe)| FfmpegPaths {
            ffmpeg,
            ffprobe,
        });
    }
    cache.clone()
}

/// Проверить, какие аппаратные кодеры реально поддерживает этот ffmpeg.
/// Парсит вывод `ffmpeg -encoders`.
pub fn usable_encoders(paths: &FfmpegPaths) -> Vec<String> {
    let supported = ["h264_nvenc", "h264_qsv", "h264_amf"];
    let mut command = Command::new(&paths.ffmpeg);
    command.args(["-hide_banner", "-encoders"]);
    apply_no_window(&mut command);
    let Ok(out) = command.output() else {
        return vec!["libx264".into()];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    supported
        .into_iter()
        .filter(|enc| text.contains(enc))
        .map(String::from)
        .chain(std::iter::once("libx264".to_string()))
        .collect()
}
