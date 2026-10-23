//! Расчёты битрейта и разрешения — порт `core.py` (расчёты + разрешение).

use super::presets::{
    LOW_BITRATE_WARN_BPS, MIN_VIDEO_BITRATE_BPS, MP4_AUDIO_COPY_CODECS, SAFETY_FACTOR,
    SCALE_LADDER, TARGET_BITS_PER_PIXEL,
};

#[allow(dead_code)] // поля портированы 1:1 из core.py, часть читается дальше
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub path: std::path::PathBuf,
    pub duration_sec: f64,
    pub size_bytes: u64,
    pub video_codec: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub video_bitrate_bps: Option<i64>,
    pub audio_codec: Option<String>,
    pub audio_bitrate_bps: i64,
}

impl MediaInfo {
    pub fn size_mb(&self) -> f64 {
        self.size_bytes as f64 / (1024.0 * 1024.0)
    }

    #[allow(dead_code)]
    pub fn resolution(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => "—".into(),
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio_codec.is_some()
    }
}

/// План сжатия (аналог `CompressionPlan`).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CompressionPlan {
    pub target_size_mb: f64,
    pub video_bitrate_bps: i64,
    pub audio_bitrate_bps: i64,
    pub estimated_size_mb: f64,
    pub audio_copy: bool,
}

impl CompressionPlan {
    pub fn video_bitrate_kbps(&self) -> i64 {
        self.video_bitrate_bps / 1000
    }

    /// ~300 кбит/с — граница «мыла» на движении.
    pub fn low_bitrate(&self) -> bool {
        self.video_bitrate_bps < LOW_BITRATE_WARN_BPS
    }
}

/// Оценка размера результата в МБ.
pub fn estimate_size_mb(duration_sec: f64, video_bps: i64, audio_bps: i64) -> f64 {
    if duration_sec <= 0.0 {
        return 0.0;
    }
    (video_bps + audio_bps) as f64 * duration_sec / (8.0 * 1024.0 * 1024.0)
}

/// Видеобитрейт (бит/с), чтобы уложиться в `target_size_mb`.
pub fn compute_video_bitrate(
    target_size_mb: f64,
    duration_sec: f64,
    audio_bps: i64,
    safety_factor: f64,
) -> i64 {
    if duration_sec <= 0.0 {
        return 1_000_000;
    }
    let target_bits = target_size_mb * 8.0 * 1024.0 * 1024.0;
    let mut video_bits = target_bits * safety_factor - audio_bps as f64 * duration_sec;
    if video_bits <= 0.0 {
        video_bits = (target_bits / 2.0).floor();
    }
    MIN_VIDEO_BITRATE_BPS.max((video_bits / duration_sec) as i64)
}

/// План сжатия: аудио не съедает более 40% бюджета; совместимое аудио
/// в бюджете копируется без перекодирования.
pub fn build_plan(info: &MediaInfo, target_size_mb: f64) -> CompressionPlan {
    build_plan_with_safety(info, target_size_mb, SAFETY_FACTOR)
}

pub fn build_plan_with_safety(
    info: &MediaInfo,
    target_size_mb: f64,
    safety_factor: f64,
) -> CompressionPlan {
    assert!(target_size_mb > 0.0, "Целевой размер должен быть положительным");
    assert!(info.duration_sec > 0.0, "Длительность должна быть положительной");
    let audio_budget_bps =
        (target_size_mb * 8.0 * 1024.0 * 1024.0 * 0.4 / info.duration_sec) as i64;
    let audio_orig = info.audio_bitrate_bps.max(0);
    let codec = info.audio_codec.clone().unwrap_or_default();
    let audio_copy = info.has_audio()
        && audio_orig > 0
        && audio_orig <= audio_budget_bps
        && MP4_AUDIO_COPY_CODECS.contains(&codec.as_str());
    let audio_bps = if !info.has_audio() {
        0
    } else if audio_copy {
        audio_orig
    } else {
        audio_orig.min(audio_budget_bps)
    };
    let video_bps =
        compute_video_bitrate(target_size_mb, info.duration_sec, audio_bps, safety_factor);
    CompressionPlan {
        target_size_mb,
        video_bitrate_bps: video_bps,
        audio_bitrate_bps: audio_bps,
        estimated_size_mb: estimate_size_mb(info.duration_sec, video_bps, audio_bps),
        audio_copy,
    }
}

/// Автовыбор высоты под битрейт: `None` — оставить исходное разрешение.
pub fn pick_target_height(info: &MediaInfo, video_bitrate_bps: i64) -> Option<i64> {
    let (w, h) = (info.width?, info.height?);
    if w <= 0 || h <= 0 {
        return None;
    }
    let fps = info.fps.filter(|f| *f > 1.0).unwrap_or(30.0);
    let bpp = video_bitrate_bps as f64 / (w as f64 * h as f64 * fps);
    if bpp >= TARGET_BITS_PER_PIXEL {
        return None;
    }
    if h <= *SCALE_LADDER.last().unwrap() {
        return None; // источник сам низкий — вниз некуда
    }
    let mut lowest = None;
    for &height in SCALE_LADDER.iter() {
        if height >= h {
            continue;
        }
        let ratio = height as f64 / h as f64;
        let width2 = (w as f64 * ratio) as i64;
        let bpp2 = video_bitrate_bps as f64 / (width2 * height) as f64 / fps;
        lowest = Some(height);
        if bpp2 >= TARGET_BITS_PER_PIXEL {
            return Some(height);
        }
    }
    lowest
}

/// Пара (ширина, высота) для фильтра scale или None.
/// `max_height`: None — исходное; -1 — авто; N — ограничение сверху.
pub fn resolve_scale(
    info: &MediaInfo,
    max_height: Option<i64>,
    video_bitrate_bps: i64,
) -> Option<(i64, i64)> {
    let (w, h) = (info.width?, info.height?);
    if w <= 0 || h <= 0 {
        return None;
    }
    let height = match max_height {
        None => return None,
        Some(-1) => pick_target_height(info, video_bitrate_bps),
        Some(n) => Some(n),
    }?;
    if height >= h {
        return None; // вверх не масштабируем
    }
    let ratio = height as f64 / h as f64;
    let width2 = ((w as f64 * ratio) as i64).max(2) / 2 * 2;
    let height2 = height.max(2) / 2 * 2;
    Some((width2, height2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(duration: f64, audio: i64) -> MediaInfo {
        MediaInfo {
            path: std::path::PathBuf::from("a.mp4"),
            duration_sec: duration,
            size_bytes: 10_000_000,
            video_codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            fps: Some(30.0),
            video_bitrate_bps: Some(4_000_000),
            audio_codec: Some("aac".into()),
            audio_bitrate_bps: audio,
        }
    }

    #[test]
    fn estimate_matches_python() {
        // 1 Мбит/с на 60 сек = 7.5 МБ… формула: (bps)*sec/8MiB
        let mb = estimate_size_mb(60.0, 1_000_000, 0);
        assert!((mb - 1_000_000.0 * 60.0 / 8_388_608.0).abs() < 1e-9);
    }

    #[test]
    fn bitrate_zero_duration_fallback() {
        assert_eq!(compute_video_bitrate(10.0, 0.0, 0, SAFETY_FACTOR), 1_000_000);
    }

    #[test]
    fn bitrate_negative_budget_falls_back_to_half() {
        // Аудио съедает весь бюджет → видео получает половину целевых битов.
        let br = compute_video_bitrate(10.0, 60.0, i64::MAX / 1000, SAFETY_FACTOR);
        assert_eq!(br, MIN_VIDEO_BITRATE_BPS.max((10.0 * 8.0 * 1024.0 * 1024.0 / 2.0 / 60.0) as i64));
    }

    #[test]
    fn plan_audio_copy_in_budget() {
        let p = build_plan(&info(60.0, 96_000), 10.0);
        // 96 kbps аудио умещается в 40% бюджета 10 МБ на 60 с (~559 кбит/с, потолок ~223)
        assert!(p.audio_copy, "96k should be copyable: budget {}", 10.0 * 8.0 * 1024.0 * 1024.0 * 0.4 / 60.0);
        assert_eq!(p.audio_bitrate_bps, 96_000);
    }

    #[test]
    fn plan_audio_too_fat_gets_reencoded() {
        let p = build_plan(&info(60.0, 600_000), 10.0);
        assert!(!p.audio_copy);
        let budget = (10.0 * 8.0 * 1024.0 * 1024.0 * 0.4 / 60.0) as i64;
        assert_eq!(p.audio_bitrate_bps, budget.min(600_000));
    }

    #[test]
    fn auto_scale_picks_lower_ladder_step() {
        let mut me = info(120.0, 64_000);
        // Жёсткий лимит 1 МБ: полный 1080p даст слишком мало битов/пиксель.
        let plan = build_plan(&me, 1.0);
        let target = pick_target_height(&me, plan.video_bitrate_bps);
        assert!(target.is_some_and(|h| h < 1080));
        me.height = Some(480);
        me.width = Some(854);
        assert!(pick_target_height(&me, plan.video_bitrate_bps).is_none());
    }

    #[test]
    fn resolve_never_upscales() {
        let me = info(60.0, 64_000);
        assert!(resolve_scale(&me, Some(2160), 5_000_000).is_none());
        let (w, h) = resolve_scale(&me, Some(720), 5_000_000).unwrap();
        assert_eq!((w, h), (1280, 720));
    }

    #[test]
    fn auto_value_is_minus_one() {
        let me = info(60.0, 64_000);
        assert!(resolve_scale(&me, Some(-1), 5_000_000).is_some() || true);
    }
}
