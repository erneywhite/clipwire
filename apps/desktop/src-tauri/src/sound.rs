//! Звук копирования: короткий щелчок, когда в историю попадает новое.
//!
//! Файл `sounds/copy.wav` вшит в программу. На Windows играет `PlaySoundW`
//! из памяти без ожидания, на других ОС пока тишина.

/// Сыграть звук копирования. Не ждёт конца звука; новый звук прерывает
/// предыдущий, поэтому частое копирование не копит очередь.
#[cfg(windows)]
pub fn play_copy() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    use windows::core::PCWSTR;

    // С SND_MEMORY первый аргумент — указатель на сам WAV в памяти. Данные
    // статические, поэтому живут, пока звук играет асинхронно.
    static COPY_WAV: &[u8] = include_bytes!("../sounds/copy.wav");
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(COPY_WAV.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        tracing::debug!("звук копирования не сыгрался");
    }
}

// shortcut: на macOS и Linux звука нет; сделать вместе с клиентами (этап 4).
#[cfg(not(windows))]
pub fn play_copy() {}
