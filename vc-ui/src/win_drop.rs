//! Windows: приём файлов drag&drop из ОС.
//!
//! Slint 1.x (winit-бэкенд) пока не передаёт файлы из ОС, поэтому делаем
//! это нативно двумя каналами:
//! 1. Официальный OLE-приёмник (`RegisterDragDrop` + `IDropTarget`) — так
//!    Explorer доставляет файлы в первую очередь (SPEC §2.3, задел под
//!    будущий нативный API Slint).
//! 2. Легаси-фолбэк `DragAcceptFiles` + `SetWindowSubclass` — для программ,
//!    которые пользуются классическим механизмом.
//!
//! Native-окно создаётся при `run()`, поэтому ставим хуки ретрай-таймером.
//! Весь модуль — на крейте `windows` (COM-объект требует его макроса
//! `#[implement]`); windows-sys тут не используется во избежание конфликтов
//! типов.

#![cfg(windows)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, LRESULT, POINTL, WPARAM, HWND};
use windows::Win32::System::Com::{
    CoInitializeEx, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Ole::{self as ole, CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, SetWindowSubclass, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, WM_DROPFILES};

use super::{add_files, FileInfo, Hub, MainWindow};
use crate::logging;
use slint::ComponentHandle;

type Slots = (slint::Weak<MainWindow>, Rc<slint::VecModel<FileInfo>>, Arc<Hub>);

thread_local! {
    static APP: RefCell<Option<Slots>> = const { RefCell::new(None) };
    /// Приёмник OLE нужно держать живым всё время работы окна: при Release
    /// OLE уменьшает счётчик, а наша ссылка гарантирует валидность vtable.
    static DROP_TARGET: RefCell<Option<IDropTarget>> = const { RefCell::new(None) };
    /// Таймер ретрая: нужно держать живым, пока окно не найдено.
    static RETRY: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
}

const SUBCLASS_ID: usize = 0x5653_4444; // "VCDD"

/// Включить приём перетаскиваемых файлов на окне приложения.
/// Native-окно Slint создаёт при старте event loop, поэтому ищем его
/// ретраями через slint::Timer — всё на UI-потоке, Rc/Weak в thread_local
/// безопасны: wndproc и COM-вызовы приходят в тот же поток.
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

/// Один заход: найти окно и поставить оба канала приёма.
/// Ok — готово, Err — окно ещё не создано.
unsafe fn try_hook() -> Result<(), ()> {
    let title: Vec<u16> = "Video Compressor\0".encode_utf16().collect();
    let hwnd = FindWindowW(None, PCWSTR::from_raw(title.as_ptr())).map_err(|_| ())?;
    // FindWindowW может вернуть HWND::default() вместо ошибки — проверяем явно.
    if hwnd.0.is_null() {
        return Err(());
    }

    // 1. Официальный OLE-приёмник (основной канал Explorer'а).
    install_ole_target(hwnd);
    // 2. Легаси-фолбэк для классических источников.
    DragAcceptFiles(hwnd, true);
    let subclassed = SetWindowSubclass(hwnd, Some(wnd_proc), SUBCLASS_ID, 0).as_bool();
    logging::log(&format!("drop: DragAcceptFiles включён, подкласс: {subclassed}"));
    Ok(())
}

/// Зарегистрировать COM-объект IDropTarget на окне.
unsafe fn install_ole_target(hwnd: HWND) {
    let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    logging::log(&format!("drop: CoInitializeEx → hr=0x{:08x}", hr.0));

    let target: IDropTarget = DropSink.into();
    // Winit (бэкенд winit-software) уже регистрирует на окне свой приёмник,
    // а файлы дальше не передаёт — снимаем его и ставим наш OLE-приёмник.
    match unsafe { ole::RevokeDragDrop(hwnd) } {
        Ok(()) => logging::log("drop: прежний drop target снят (RevokeDragDrop ок)"),
        // NOINTERFACE/DRAGDROP_E_NOTREGISTERED — регистрировали не мы, тоже норм.
        Err(e) => logging::log(&format!("drop: RevokeDragDrop — нечего снимать: {e}")),
    }
    match ole::RegisterDragDrop(hwnd, &target) {
        Ok(()) => {
            DROP_TARGET.with(|s| *s.borrow_mut() = Some(target));
            logging::log("drop: OLE RegisterDragDrop ок — файлы Explorer'а принимаются");
        }
        Err(e) => logging::log(&format!("drop: RegisterDragDrop не удался: {e}")),
    }
}

/// Процедура подкласса: перехватываем только WM_DROPFILES.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    _l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_DROPFILES {
        let paths = collect_paths(HDROP(w.0 as *mut std::ffi::c_void), true);
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
        return LRESULT(0);
    }
    DefSubclassProc(hwnd, msg, w, _l)
}

/// Реализация IDropTarget: подряд приходят DragEnter/Over/Leave, затем Drop.
#[windows::core::implement(IDropTarget)]
struct DropSink;

impl ole::IDropTarget_Impl for DropSink_Impl {
    fn DragEnter(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let effect = match pdataobj {
            Some(obj) => match unsafe { try_get_hdrop(obj) } {
                Ok(_) => DROPEFFECT_COPY,
                Err(_) => DROPEFFECT_NONE,
            },
            None => DROPEFFECT_NONE,
        };
        unsafe {
            *pdweffect = effect;
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let obj_out = match pdataobj {
            Some(obj) => match unsafe { try_get_hdrop(obj) } {
                Ok(paths) => paths,
                Err(e) => {
                    logging::log(&format!("drop(OLE): CF_HDROP не получен: {e}"));
                    unsafe {
                        *pdweffect = DROPEFFECT_NONE;
                    }
                    return Ok(());
                }
            },
            None => {
                unsafe {
                    *pdweffect = DROPEFFECT_NONE;
                }
                return Ok(());
            }
        };
        logging::log(&format!("drop(OLE): перетащено {} файлов", obj_out.len()));
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
        }
        if !obj_out.is_empty() {
            APP.with(|s| {
                let (weak, model, hub) = match s.borrow().as_ref() {
                    Some(t) => (t.0.clone(), t.1.clone(), t.2.clone()),
                    None => return,
                };
                if let Some(ui) = weak.upgrade() {
                    add_files(&ui, &model, &hub, obj_out);
                }
            });
        }
        Ok(())
    }
}

/// Есть ли у объекта файлы (CF_HDROP) и каковы они.
unsafe fn try_get_hdrop(obj: &IDataObject) -> Result<Vec<PathBuf>, windows::core::Error> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        ptd: std::ptr::null_mut(),
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = obj.GetData(&fmt)?;
    let hdrop = HDROP(medium.u.hGlobal.0);
    let paths = collect_paths(hdrop, false);
    ole::ReleaseStgMedium(&mut medium);
    Ok(paths)
}

/// HDROP → список путей. При `finish` буфер ОС освобождается
/// (`DragFinish` — только легаси-каналу; в OLE-канале это делает
/// `ReleaseStgMedium`).
fn collect_paths(hdrop: HDROP, finish: bool) -> Vec<PathBuf> {
    unsafe {
        let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            if len == 0 {
                continue;
            }
            // Обёртка передаёт cch = buf.len(): API обязан писать и NUL,
            // поэтому выделение ровно под len режет последний символ пути.
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(PathBuf::from(String::from_utf16_lossy(
                &buf[..len],
            )));
        }
        if finish {
            DragFinish(hdrop);
        }
        out
    }
}
