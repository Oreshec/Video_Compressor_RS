//! Авто-обновление: GitHub Releases (self_update).
//!
//! Проверка — тихо при старте и по кнопке; установка — по кнопке.
//! Модуль владеет и версией приложения (`CURRENT_VERSION`): единый источник.

use crate::logging;
use crate::MainWindow;

const REPO_OWNER: &str = "Oreshec";
const REPO_NAME: &str = "Video_Compressor_RS";
const BIN_NAME: &str = "vc-ui";

pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn github_update(unattended: bool) -> Result<self_update::backends::github::Update, String> {
    let mut builder = self_update::backends::github::Update::configure();
    if unattended {
        builder.unattended();
    }
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .current_version(CURRENT_VERSION)
        .bin_name(BIN_NAME)
        .build()
        .map_err(|e| e.to_string())
}

/// Проверка наличия новой версии (без установки). `quiet` — не ругаться
/// на сетевые ошибки при тихой проверке при старте.
pub(crate) fn spawn_update_check(weak: slint::Weak<MainWindow>, quiet: bool) {
    logging::log("updates: проверка обновлений начата");
    std::thread::spawn(move || {
        let result = github_update(true)
            .and_then(|u| u.is_update_available().map_err(|e| e.to_string()));
        match &result {
            Ok(Some(r)) => logging::log(&format!("updates: доступно v{}", r.version())),
            Ok(None) => logging::log("updates: версия актуальна"),
            Err(e) => logging::log(&format!("updates: ошибка проверки: {e}")),
        }
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(Some(release)) => {
                ui.set_update_available(true);
                ui.set_update_note(
                    format!("Доступно обновление: v{} — нажмите «Установить»", release.version())
                        .into(),
                );
            }
            Ok(None) => {
                ui.set_update_available(false);
                ui.set_update_note("У вас самая свежая версия".into());
            }
            Err(e) => {
                ui.set_update_available(false);
                if !quiet {
                    ui.set_update_note(
                        format!("Не удалось проверить обновления: {e}").into(),
                    );
                }
            }
        });
    });
}

/// Установка обновления: скачать релиз, заменить exe, перезапуститься.
pub(crate) fn spawn_update_install(weak: slint::Weak<MainWindow>) {
    ui_note(&weak, "Устанавливаем обновление…".into());
    std::thread::spawn(move || {
        let result = github_update(true)
            .and_then(|u| u.update().map_err(|e| e.to_string()));
        match &result {
            Ok(s) => logging::log(&format!(
                "updates: установка завершена: версия {}",
                s.version()
            )),
            Err(e) => logging::log(&format!("updates: ошибка установки: {e}")),
        }
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(status) if status.is_updated() => {
                ui.set_update_note(
                    format!("Установлена версия {} — перезапускаем…", status.version()).into(),
                );
                // Перезапуск: уже установленная новая сборка подхватится сама.
                restart_app();
            }
            Ok(_status) => {
                ui.set_update_available(false);
                ui.set_update_note("Обновление не требуется".into());
            }
            Err(e) => {
                ui.set_update_note(format!("Ошибка обновления: {e}").into());
            }
        });
    });
}

fn ui_note(weak: &slint::Weak<MainWindow>, text: slint::SharedString) {
    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_update_note(text));
}

/// Запустить новую копию и аккуратно закрыть текущую.
fn restart_app() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    let _ = slint::quit_event_loop();
}
