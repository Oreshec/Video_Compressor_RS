//! Анализ медиафайлов через ffprobe — порт `core.probe`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::bitrate::MediaInfo;
use super::ffmpeg::FfmpegPaths;

/// Ошибка анализа (аналог `MediaInfoError`).
#[derive(Debug)]
pub struct ProbeError(pub String);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn to_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn to_float(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Частота кадров вида ``29.97`` или ``30000/1001``.
fn parse_fps(raw: Option<&Value>) -> Option<f64> {
    let raw = raw?.as_str()?;
    if raw.is_empty() {
        return None;
    }
    if let Some((num, den)) = raw.split_once('/') {
        let n: f64 = num.trim().parse().ok()?;
        let d: f64 = den.trim().parse().ok()?;
        if n != 0.0 && d != 0.0 {
            return Some((n / d * 1000.0).round() / 1000.0);
        }
        // Битая дробь (нулевой числитель или знаменатель) — данных нет.
        return None;
    }
    raw.trim().parse().ok()
}

/// Битрейт аудио: из потока; иначе общий минус видео; иначе 128 kbps.
fn estimate_audio_bitrate(media: &Value, video_bps: Option<i64>) -> i64 {
    if let Some(streams) = media.get("streams").and_then(Value::as_array) {
        for stream in streams {
            if stream.get("codec_type").and_then(Value::as_str) == Some("audio") {
                if let Some(br) = to_int(stream.get("bit_rate")).filter(|b| *b > 0) {
                    return br;
                }
            }
        }
    }
    let total = media
        .get("format")
        .and_then(|f| to_int(f.get("bit_rate")))
        .filter(|b| *b > 0);
    match total {
        Some(total) => match video_bps {
            Some(video) => (total - video).max(64_000),
            None => total / 5,
        },
        None => 128_000,
    }
}

/// Прочитать информацию о файле через ffprobe.
pub fn probe(paths: &FfmpegPaths, path: &Path) -> Result<MediaInfo, ProbeError> {
    if !path.is_file() {
        return Err(ProbeError(format!("Файл не найден: {}", path.display())));
    }
    let mut command = Command::new(&paths.ffprobe);
    command.args([
        "-v", "error", "-print_format", "json", "-show_format", "-show_streams",
    ]);
    command.arg(path);
    super::ffmpeg::apply_no_window(&mut command);
    let out = command
        .output()
        .map_err(|e| ProbeError(format!("Не удалось запустить ffprobe: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim().lines().last().unwrap_or(".");
        return Err(ProbeError(format!("Не удалось проанализировать файл: {detail}")));
    }
    let media: Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| ProbeError("ffprobe вернул некорректные данные.".into()))?;

    let fmt = media.get("format").cloned().unwrap_or(Value::Null);
    let duration = to_float(fmt.get("duration")).unwrap_or(0.0);
    let size = to_int(fmt.get("size")).unwrap_or(0).max(0) as u64;

    let mut video_codec = None;
    let mut audio_codec = None;
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut video_bitrate = None;

    if let Some(streams) = media.get("streams").and_then(Value::as_array) {
        for stream in streams {
            let ctype = stream.get("codec_type").and_then(Value::as_str);
            if ctype == Some("video") && video_codec.is_none() {
                video_codec = stream.get("codec_name").and_then(Value::as_str).map(String::from);
                width = to_int(stream.get("width"));
                height = to_int(stream.get("height"));
                fps = parse_fps(
                    stream.get("avg_frame_rate").or_else(|| stream.get("r_frame_rate")),
                );
                video_bitrate = to_int(stream.get("bit_rate"));
            } else if ctype == Some("audio") && audio_codec.is_none() {
                audio_codec = stream.get("codec_name").and_then(Value::as_str).map(String::from);
            }
        }
    }

    if video_codec.is_none() {
        return Err(ProbeError("В файле нет видеопотока.".into()));
    }
    if duration <= 0.0 {
        return Err(ProbeError("Не удалось определить длительность видео.".into()));
    }
    let audio_bitrate = if audio_codec.is_some() {
        estimate_audio_bitrate(&media, video_bitrate)
    } else {
        0
    };

    Ok(MediaInfo {
        path: PathBuf::from(path),
        duration_sec: duration,
        size_bytes: size,
        video_codec,
        width,
        height,
        fps,
        video_bitrate_bps: video_bitrate,
        audio_codec,
        audio_bitrate_bps: audio_bitrate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fps_fraction() {
        assert_eq!(parse_fps(Some(&json!("30000/1001"))), Some(29.97));
        assert_eq!(parse_fps(Some(&json!("29.97"))), Some(29.97));
        assert_eq!(parse_fps(Some(&json!(""))), None);
    }

    #[test]
    fn audio_bitrate_fallbacks() {
        let media = json!({"format": {"bit_rate": "4000000"},
                           "streams": [{"codec_type": "audio", "bit_rate": "96000"}]});
        assert_eq!(estimate_audio_bitrate(&media, Some(3_500_000)), 96_000);
        let no_br = json!({"format": {"bit_rate": "4000000"},
                           "streams": [{"codec_type": "audio"}]});
        assert_eq!(estimate_audio_bitrate(&no_br, Some(3_500_000)), 500_000);
        let empty = json!({});
        assert_eq!(estimate_audio_bitrate(&empty, None), 128_000);
    }

    #[test]
    fn int_from_string() {
        assert_eq!(to_int(Some(&json!("1920"))), Some(1920));
        assert_eq!(to_int(Some(&json!("x"))), None);
    }
}
