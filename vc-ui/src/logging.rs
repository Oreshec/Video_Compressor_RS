//! Служебный лог `vc-ui.log` (рядом с exe, иначе `%TEMP%`).
//! У GUI-сборки консоли нет (`windows_subsystem = "windows"`), поэтому
//! трассировка запуска, сжатия и ошибок идёт в файл.

use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn path() -> &'static Option<PathBuf> {
    LOG_PATH.get_or_init(|| {
        let exe_side = std::env::current_exe().ok().and_then(|exe| {
            let p = exe.parent()?.join("vc-ui.log");
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()?;
            Some(p)
        });
        exe_side.or_else(|| Some(std::env::temp_dir().join("vc-ui.log")))
    })
}

/// Дописать строку в лог; ошибки записи молча игнорируются.
pub fn log(msg: &str) {
    use std::io::Write;
    let line = format!("[{}] {}", stamp(), msg);
    let Some(p) = path() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.write_all(b"\n");
    }
}

/// `2026-10-25 12:34:56 UTC` без внешних крейтов.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let tod = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Дни с эпохи → (год, месяц, день). Алгоритм Говарда Хиннанта.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 1970-01-01 → 2024-01-01: 54 года = 19710 дней + 13 високосных.
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_751), (2026, 10, 25));
        // Високосный февраль: 2024-02-29 = 19723 + 31 + 28 = 19782.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn stamp_has_timestamp_shape() {
        let s = stamp();
        assert_eq!(s.len(), 23, "формат 'ГГГГ-ММ-ДД ЧЧ:ММ:СС UTC': {s}");
        for (idx, expected) in [(4, '-'), (7, '-'), (10, ' '), (13, ':'), (16, ':'), (19, ' ')] {
            assert_eq!(s.as_bytes()[idx] as char, expected, "позиция {idx}: {s}");
        }
        assert!(s.ends_with(" UTC"));
        let digits: Vec<char> = s
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        assert_eq!(digits.len(), 14, "{s}");
    }

    #[test]
    fn log_appends_stamped_line() {
        let marker = format!(
            "тест-маркер-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        log(&marker);
        let path = path().clone().expect("лог должен быть доступен в тесте");
        let content = std::fs::read_to_string(path).expect("чтение лога");
        assert!(content.contains(&marker), "строка не дописана");
        assert!(content.lines().any(|l| l.starts_with('[')), "нет метки времени");
    }
}

