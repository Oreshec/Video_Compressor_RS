//! Windows: приём файлов drag&drop из ОС.
//!
//! Slint 1.x (winit-бэкенд) пока не передаёт `WM_DROPFILES` пользовательскому
//! коду, поэтому подсклассируем окно: `DragAcceptFiles` + `SetWindowSubclass`.
//! SPEC §2.3: когда у Slint появится нативный API, этот модуль можно удалить.

#![cfg(windows)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, HDROP, SetWindowSubclass,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, WM_DROPFILES};

use super::{add_files, FileInfo, Hub, MainWindow};
use crate::logging;
use slint::ComponentHandle;

type Slots = (slint::Weak<MainWindow>, Rc<slint::VecModel<FileInfo>>, Arc<Hub>);

thread_local! {
    static APP: RefCell<Option<Slots>> = const { RefCell::new(None) };
}

const SUBCLASS_ID: usize = 0x5653_4444; // "VCDD"

thread_local! {
    /// Таймер ретрая: нужно держать живым, пока окно не найдено.
    static RETRY: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
}

/// Включить приём перетаскиваемых файлов на окне приложения.
/// Native-окно Slint создаёт при старте event loop, поэтому ищем его
/// ретраями через slint::Timer — всё на UI-потоке, Rc/Weak в thread_local
/// безопасны: wndproc вызывается из того же потока.
pub(crate) fn install(app: &MainWindow, model: &Rc<slint::VecModel<FileInfo>>, hub: &Arc<Hub>) {
    APP.with(|s| {
        *s.borrow_mut() = Some((app.as_weak(), model.clone(), Arc::clone(hub)));
    });
    let timer = Rc::new(slint::Timer::default());
    let weak_timer = Rc::downgrade(&timer);
    RETRY.with(|s| *s.borrow_mut() = Some(Rc::clone(&timer)));
    let attempts = Rc::new(std::cell::Cell::new(0u32));
    {
        let attempts = Rc::clone(&attempts);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(200),
            move || {
                let n = attempts.get() + 1;
                attempts.set(n);
                match unsafe { try_hook() } {
                    Ok(()) => {
                        if let Some(t) = weak_timer.upgrade() {
                            t.stop();
                        }
                    }
                    Err(()) if n >= 50 => {
                        logging::log("drop: за 10 с окно не найдено — drag&drop отключён");
                        if let Some(t) = weak_timer.upgrade() {
                            t.stop();
                        }
                    }
                    Err(()) => {}
                }
            },
        );
    }
}

/// Один заход: найти окно и поставить хук. Ok — готово, Err — окно ещё не создано.
unsafe fn try_hook() -> Result<(), ()> {
    let title: Vec<u16> = "Video Compressor\0".encode_utf16().collect();
    let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
    if hwnd.is_null() {
        return Err(());
    }
    DragAcceptFiles(hwnd, 1);
    SetWindowSubclass(hwnd, Some(wnd_proc), SUBCLASS_ID, 0);
    logging::log("drop: DragAcceptFiles включён, окно подсклассировано");
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_DROPFILES {
        let paths = collect_paths(w as _);
        logging::log(&format!("drop: перетащено {} файлов", paths.len()));
        if !paths.is_empty() {
            APP.with(|s| {
                if let Some((weak, model, hub)) = s.borrow().as_ref() {
                    if let Some(ui) = weak.upgrade() {
                        add_files(&ui, model, hub, paths);
                    }
                }
            });
        }
        return 0;
    }
    DefSubclassProc(hwnd, msg, w, l)
}

/// HDROP → список путей. `DragFinish` освобождает буфер ОС.
fn collect_paths(hdrop: HDROP) -> Vec<PathBuf> {
    unsafe {
        let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) as usize;
        let mut out = Vec::with_capacity(count);
        for i in 0..count as u32 {
            let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32);
            out.push(PathBuf::from(String::from_utf16_lossy(
                &buf[..len as usize],
            )));
        }
        DragFinish(hdrop);
        out
    }
}
