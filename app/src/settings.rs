//! Настройки приложения: собственный профиль Rust-версии в
//! `%APPDATA%\Roaming\VideoCompressorRS` (Python-версия использует свой
//! `VideoCompressor`; версии полностью раздельные).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use vc_core::presets::DEFAULT_TARGET_SIZE_MB;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub speed: String,
    #[serde(default)]
    pub encoder: String,
    /// «source» | «auto» | «2160»…
    #[serde(default)]
    pub resolution: String,
    #[serde(default)]
    pub two_pass: bool,
    #[serde(default)]
    pub details: bool,
    #[serde(default)]
    pub output_dir: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

fn settings_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join("AppData").join("Roaming"))
            .join("VideoCompressorRS")
    } else if cfg!(target_os = "macos") {
        dirs_home()
            .join("Library")
            .join("Application Support")
            .join("VideoCompressor")
    } else {
        dirs_home().join(".config").join("VideoCompressor")
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn settings_path() -> PathBuf {
    settings_dir().join("settings.json")
}

/// Загрузить; несуществующий/битый файл → значения по умолчанию.
pub fn load() -> Settings {
    match fs::read_to_string(settings_path()) {
        Ok(text) => serde_json::from_str::<Settings>(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

/// Записать на диск; ошибки не критичны.
pub fn save(settings: &Settings) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path, text);
    }
}

impl Settings {
    pub fn target_size(&self) -> f64 {
        if self.size.is_finite() && self.size > 0.0 {
            self.size
        } else {
            DEFAULT_TARGET_SIZE_MB
        }
    }

    /// Целевой размер в байтах (внутренняя арифметика ядра — байтовая).
    pub fn target_size_bytes(&self) -> u64 {
        (self.target_size() * 1024.0 * 1024.0).round() as u64
    }

    /// Имя пресета скорости.
    pub fn speed_preset(&self) -> String {
        const ALLOWED: [&str; 3] = ["veryfast", "medium", "slow"];
        match ALLOWED.contains(&self.speed.as_str()) {
            true => self.speed.clone(),
            false => "veryfast".into(),
        }
    }

    /// Имя кодировщика; «auto» и не именованные → «auto».
    pub fn encoder_setting(&self) -> String {
        const ALLOWED: [&str; 4] = ["libx264", "h264_nvenc", "h264_qsv", "h264_amf"];
        if ALLOWED.contains(&self.encoder.as_str()) {
            self.encoder.clone()
        } else {
            "auto".into()
        }
    }

    /// Ограничение высоты: None — исходное; -1 — авто; N — лимит.
    pub fn max_height(&self) -> Option<i64> {
        match self.resolution.as_str() {
            "" | "source" => None,
            "auto" => Some(-1),
            other => other.trim().parse::<i64>().ok().filter(|n| *n > 0),
        }
    }
}
