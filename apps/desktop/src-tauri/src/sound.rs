//! Звук копирования: короткий щелчок, когда в историю попадает новое.
//!
//! Файл `sounds/copy.wav` вшит в программу. На Windows играет `PlaySoundW`
//! из памяти без ожидания, на macOS — `NSSound` из тех же байтов, на Linux —
//! системный проигрыватель звуковой службы (`pw-play` у PipeWire, `paplay` у
//! PulseAudio, `aplay` у голой ALSA): постоянный аудиопоток в программе
//! держал бы звуковую карту занятой ради щелчка раз в несколько минут.

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

/// Сыграть звук копирования (Linux). WAV один раз кладётся в
/// `$XDG_RUNTIME_DIR`; новый звук останавливает ещё играющий.
#[cfg(target_os = "linux")]
pub fn play_copy() {
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};

    static COPY_WAV: &[u8] = include_bytes!("../sounds/copy.wav");
    static FILE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    static PLAYING: Mutex<Option<Child>> = Mutex::new(None);
    /// Первый проигрыватель, который нашёлся; `None` — никакого нет.
    static PLAYER: OnceLock<Option<(&str, &[&str])>> = OnceLock::new();

    let Some(file) = FILE
        .get_or_init(|| {
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            // Имя постоянное: файлы не копятся от запуска к запуску, а
            // содержимое у всех копий одинаковое.
            let path = dir.join("clipwire-copy.wav");
            std::fs::write(&path, COPY_WAV)
                .map_err(|e| tracing::debug!("звук копирования не записался: {e}"))
                .ok()
                .map(|_| path)
        })
        .as_ref()
    else {
        return;
    };
    let Some((cmd, args)) = *PLAYER.get_or_init(|| {
        const PLAYERS: [(&str, &[&str]); 3] =
            [("pw-play", &[]), ("paplay", &[]), ("aplay", &["-q"])];
        let path = std::env::var("PATH").unwrap_or_default();
        let found = PLAYERS.into_iter().find(|(cmd, _)| {
            path.split(':')
                .any(|d| std::path::Path::new(d).join(cmd).is_file())
        });
        if found.is_none() {
            tracing::info!("звук копирования: нет pw-play, paplay или aplay");
        }
        found
    }) else {
        return;
    };
    let mut playing = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut old) = playing.take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    match Command::new(cmd)
        .args(args)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => *playing = Some(child),
        Err(e) => tracing::debug!("звук копирования не сыгрался: {e}"),
    }
    // Доигравший процесс забирается при следующем звуке (kill по завершённому
    // ничего не делает), зомби не копятся дольше одного.
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn play_copy() {}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    #[test]
    fn copy_sound_loads_and_repeats() {
        // Без звуковой карты (CI) play вернёт false, но не упадёт.
        super::play_copy();
        super::play_copy();
    }
}
