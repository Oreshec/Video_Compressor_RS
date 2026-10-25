//! Константы и пресеты — порт начала `video_compressor/core.py`.

/// Целевой размер по умолчанию, МБ.
pub const DEFAULT_TARGET_SIZE_MB: f64 = 10.0;
/// Запас на контейнер и неточность кодирования (доля целевого размера).
pub const SAFETY_FACTOR: f64 = 0.92;
/// Минимальный видеобитрейт, ниже которого результат бессмысленный, бит/с.
pub const MIN_VIDEO_BITRATE_BPS: i64 = 50_000;
/// Максимальное число попыток, если результат не влез в лимит.
pub const MAX_ATTEMPTS: usize = 3;
/// Кодеки аудио, совместимые с MP4: умещается в бюджет — копируем без потерь.
pub const MP4_AUDIO_COPY_CODECS: [&str; 5] = ["aac", "mp3", "alac", "ac3", "eac3"];
/// Целевые биты на пиксель кадра: ниже — качество «плывёт», выгоднее
/// уменьшить разрешение.
pub const TARGET_BITS_PER_PIXEL: f64 = 0.06;
/// Лестница высот для авторесолюшна (только вниз от источника).
pub const SCALE_LADDER: [i64; 6] = [2160, 1440, 1080, 720, 540, 480];
/// Below ~300 kbps of video rate quality visibly mushes on motion.
pub const LOW_BITRATE_WARN_BPS: i64 = 300_000;

/// Пресет аппаратного кодера для каждого пресета libx264.
pub fn hw_preset(encoder: &str, preset: &str) -> String {
    // hw_preset mirror of _HW_PRESETS
    match encoder {
        "h264_nvenc" => match preset {
            "ultrafast" => "p1".into(),
            "superfast" => "p2".into(),
            "veryfast" | "faster" => "p4".into(),
            "fast" | "medium" => "p5".into(),
            "slow" => "p6".into(),
            _ => "p7".into(),
        },
        "h264_amf" => match preset {
            "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" => "speed".into(),
            "medium" => "balanced".into(),
            _ => "quality".into(),
        },
        // qsv понимает те же имена, что libx264.
        _ => preset.to_owned(),
    }
}

/// Надписи пресетов скорости для комбобокса (label → пресет ядра).
pub const SPEED_CHOICES: [(&str, &str); 3] = [
    ("Быстро (рекомендуется)", "veryfast"),
    ("Сбалансировано", "medium"),
    ("Максимальное качество", "slow"),
];

/// Надписи кодировщиков для комбобокса (label → имя ядра).
pub const ENCODER_LABELS: [(&str, &str); 5] = [
    ("CPU x264 — лучшее качество", "libx264"),
    ("Авто (аппаратный, если есть)", "auto"),
    ("NVIDIA NVENC", "h264_nvenc"),
    ("Intel QSV", "h264_qsv"),
    ("AMD AMF", "h264_amf"),
];

/// Человеческое имя кодировщика для статуса/лога.
pub fn encoder_label(encoder: &str) -> &str {
    match encoder {
        "libx264" => "CPU x264",
        "h264_nvenc" => "NVIDIA NVENC",
        "h264_qsv" => "Intel QSV",
        "h264_amf" => "AMD AMF",
        _ => encoder,
    }
}

/// Расширения видео для фильтра drag-drop и диалога выбора файлов.
pub const VIDEO_EXTENSIONS: [&str; 24] = [
    "mp4", "mkv", "mov", "avi", "webm", "wmv", "flv", "m4v", "mpg", "mpeg",
    "ts", "m2ts", "mts", "3gp", "3g2", "f4v", "ogv", "vob", "rm", "rmvb",
    "asf", "mxf", "dv", "mpe",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_presets_mirror_python() {
        assert_eq!(hw_preset("h264_nvenc", "veryfast"), "p4");
        assert_eq!(hw_preset("h264_nvenc", "veryslow"), "p7");
        assert_eq!(hw_preset("h264_amf", "medium"), "balanced");
        assert_eq!(hw_preset("h264_amf", "slow"), "quality");
        assert_eq!(hw_preset("h264_qsv", "fast"), "fast");
    }
}
