//! Win+V открывает окно Clipwire вместо системного журнала буфера.
//!
//! Зарегистрировать Win+V обычной горячей клавишей нельзя: её держит Проводник.
//! Поэтому, как AutoHotkey и PowerToys, ставим низкоуровневый хук клавиатуры
//! (`WH_KEYBOARD_LL`): нажатие V при зажатой Win глушим и показываем своё окно.
//! Чтобы после отпускания Win не открылось меню «Пуск» (оно открывается, если
//! между нажатием и отпусканием Win не было других клавиш), сразу посылаем
//! неназначенную клавишу `0xE8`, как делает AutoHotkey.
//!
//! Где не работает: в окнах, запущенных от администратора, если Clipwire
//! запущен без прав. Windows не пускает туда ни хук, ни вставку Ctrl+V из
//! обычного процесса, поэтому там открывается системный журнал. Вставить в такое
//! окно Clipwire всё равно бы не смог.

use tauri::AppHandle;

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use tauri::AppHandle;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
        PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    static APP: OnceLock<AppHandle> = OnceLock::new();
    /// Поток с хуком, 0 — хук не стоит.
    static THREAD: AtomicU32 = AtomicU32::new(0);
    /// Нажатие V заглушено: заглушить и его отпускание.
    static SWALLOWED: AtomicBool = AtomicBool::new(false);
    static LOCK: Mutex<()> = Mutex::new(());

    /// Неназначенная клавиша: прячет одиночное нажатие Win от меню «Пуск».
    const MASK: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

    fn down(vk: VIRTUAL_KEY) -> bool {
        let state = unsafe { GetAsyncKeyState(vk.0 as i32) };
        state < 0
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Решение хука по одному событию: `true` — событие заглушить.
    /// Вынесено из `hook` ради теста.
    pub(super) fn decide(
        vk: u32,
        pressed: bool,
        win: bool,
        others: bool,
        swallowed: &AtomicBool,
    ) -> (bool, bool) {
        // (заглушить, открыть окно)
        if vk != VK_V.0 as u32 {
            return (false, false);
        }
        if pressed {
            if win && !others {
                // Автоповтор зажатой V окно не дёргает.
                let first = !swallowed.swap(true, Ordering::Relaxed);
                return (true, first);
            }
            return (false, false);
        }
        (swallowed.swap(false, Ordering::Relaxed), false)
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let msg = wparam.0 as u32;
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let released = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if pressed || released {
                let win = down(VK_LWIN) || down(VK_RWIN);
                let others = down(VK_CONTROL) || down(VK_MENU) || down(VK_SHIFT);
                let (swallow, open) = decide(kb.vkCode, pressed, win, others, &SWALLOWED);
                if open {
                    let inputs = [key(MASK, false), key(MASK, true)];
                    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
                    if let Some(app) = APP.get() {
                        let a = app.clone();
                        let _ = app.run_on_main_thread(move || crate::popup::toggle(&a));
                    }
                }
                if swallow {
                    return LRESULT(1);
                }
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    pub fn set(app: &AppHandle, on: bool) {
        let _ = APP.set(app.clone());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let running = THREAD.load(Ordering::SeqCst);
        if on == (running != 0) {
            return;
        }
        if !on {
            unsafe {
                let _ = PostThreadMessageW(running, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            THREAD.store(0, Ordering::SeqCst);
            SWALLOWED.store(false, Ordering::Relaxed);
            tracing::info!("Win+V: системный журнал буфера");
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("clipwire-winv".into())
            .spawn(move || {
                let module = unsafe { GetModuleHandleW(None) }.ok();
                let h = unsafe {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(Into::into), 0)
                };
                let h = match h {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!("Win+V: хук клавиатуры не встал: {e}");
                        let _ = tx.send(0);
                        return;
                    }
                };
                let _ = tx.send(unsafe { GetCurrentThreadId() });
                // Хуку нужен цикл сообщений в его потоке.
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
            })
            .ok();
        let id = rx.recv().unwrap_or(0);
        THREAD.store(id, Ordering::SeqCst);
        if id != 0 {
            tracing::info!("Win+V: окно Clipwire");
        }
    }
}

/// Включить или выключить перехват Win+V. Повторный вызов с тем же значением
/// ничего не делает.
pub fn set(app: &AppHandle, on: bool) {
    #[cfg(windows)]
    imp::set(app, on);
    #[cfg(not(windows))]
    let _ = (app, on);
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp::decide;
    use std::sync::atomic::AtomicBool;

    const V: u32 = 0x56;

    #[test]
    fn swallows_win_v_pair_and_opens_once() {
        let s = AtomicBool::new(false);
        assert_eq!(decide(V, true, true, false, &s), (true, true));
        // Автоповтор: глушим, окно не трогаем.
        assert_eq!(decide(V, true, true, false, &s), (true, false));
        // Отпускание глушится, даже если Win уже отпустили.
        assert_eq!(decide(V, false, false, false, &s), (true, false));
        // Следующее отпускание V без нажатия — обычное.
        assert_eq!(decide(V, false, false, false, &s), (false, false));
    }

    #[test]
    fn leaves_other_keys_and_combinations() {
        let s = AtomicBool::new(false);
        assert_eq!(
            decide(V, true, false, false, &s),
            (false, false),
            "просто V"
        );
        assert_eq!(
            decide(V, true, true, true, &s),
            (false, false),
            "Ctrl+Win+V и т. п."
        );
        assert_eq!(decide(0x45, true, true, false, &s), (false, false), "Win+E");
        assert_eq!(decide(V, false, false, false, &s), (false, false));
    }
}
