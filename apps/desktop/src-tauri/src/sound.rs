//! Звук копирования: короткий щелчок, когда в историю попадает новое.
//!
//! Файл `sounds/copy.wav` вшит в программу. На Windows играет `PlaySoundW`
//! из памяти без ожидания, на macOS — `NSSound` из тех же байтов, на Linux
//! пока тишина.

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

/// Сыграть звук копирования (macOS). Один `NSSound` на всю программу: он
/// должен жить, пока играет, а новый звук перезапускает его с начала.
#[cfg(target_os = "macos")]
pub fn play_copy() {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;
    use std::sync::Mutex;

    struct Sound(Retained<NSSound>);
    // SAFETY: NSSound играет асинхронно сам; вызовы stop/play идут по одному
    // под мьютексом.
    unsafe impl Send for Sound {}

    static COPY_WAV: &[u8] = include_bytes!("../sounds/copy.wav");
    static SOUND: Mutex<Option<Sound>> = Mutex::new(None);

    let mut slot = SOUND.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let data = NSData::with_bytes(COPY_WAV);
        *slot = NSSound::initWithData(NSSound::alloc(), &data).map(Sound);
    }
    match slot.as_ref() {
        Some(Sound(s)) => {
            s.stop();
            if !s.play() {
                tracing::debug!("звук копирования не сыгрался");
            }
        }
        None => tracing::debug!("звук копирования не загрузился"),
    }
}

// shortcut: на Linux звука нет; сделать вместе с клиентом.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn play_copy() {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn copy_sound_loads_and_repeats() {
        // Без звуковой карты (CI) play вернёт false, но не упадёт.
        super::play_copy();
        super::play_copy();
    }
}
