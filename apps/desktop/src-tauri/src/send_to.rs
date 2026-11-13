//! Уборка пункта «Отправить в Clipwire» из меню Проводника.
//!
//! В 0.1.2 Clipwire добавлял этот пункт в реестр пользователя. Пункт убрали:
//! он делал то же, что Ctrl+C, только длиннее. Чтобы у обновившихся он не
//! остался ссылкой в никуда, при запуске ключи удаляются.
//! shortcut: уборка для тех, кто ставил 0.1.2; убрать через несколько версий.

/// Удалить ключи пункта, если они есть. В отладочной сборке не трогает реестр.
pub fn remove_leftovers() {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
        use windows::core::HSTRING;
        for t in ["*", "Directory"] {
            let path = HSTRING::from(format!(r"Software\Classes\{t}\shell\Clipwire.Send"));
            // Ключа нет — это и есть цель.
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &path) };
        }
    }
}
