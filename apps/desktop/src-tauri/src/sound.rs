//! Звуки: щелчок копирования (в историю попало новое или скопировали ещё
//! раз) и щелчок вставки из окна Clipwire (вторая нота ниже).
//!
//! Файлы `sounds/copy.wav` и `sounds/paste.wav` вшиты в программу. На Windows играет `PlaySoundW`
//! из памяти без ожидания, на macOS — `NSSound` из тех же байтов, на Linux —
//! системный проигрыватель звуковой службы (`pw-play` у PipeWire, `paplay` у
//! PulseAudio, `aplay` у голой ALSA): постоянный аудиопоток в программе
//! держал бы звуковую карту занятой ради щелчка раз в несколько минут.

/// Какой звук.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Copy,
    Paste,
}

impl Sound {
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    fn wav(self) -> &'static [u8] {
        match self {
            Sound::Copy => include_bytes!("../sounds/copy.wav"),
            Sound::Paste => include_bytes!("../sounds/paste.wav"),
        }
    }
}

/// Сыграть звук копирования.
pub fn play_copy() {
    play(Sound::Copy);
}

/// Сыграть звук. Не ждёт конца звука; новый звук прерывает предыдущий,
/// поэтому частое копирование не копит очередь.
#[cfg(windows)]
pub fn play(sound: Sound) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    use windows::core::PCWSTR;

    // С SND_MEMORY первый аргумент — указатель на сам WAV в памяти. Данные
    // статические, поэтому живут, пока звук играет асинхронно.
    let wav = sound.wav();
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(wav.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        tracing::debug!(?sound, "звук не сыгрался");
    }
}

/// Сыграть звук (macOS). По одному `NSSound` на звук: он должен жить, пока
/// играет; новый звук останавливает оба и играет свой с начала.
#[cfg(target_os = "macos")]
pub fn play(sound: Sound) {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;
    use std::sync::Mutex;

    struct Player(Retained<NSSound>);
    // SAFETY: NSSound играет асинхронно сам; вызовы stop/play идут по одному
    // под мьютексом.
    unsafe impl Send for Player {}

    static SOUNDS: Mutex<[Option<Player>; 2]> = Mutex::new([None, None]);

    let mut slots = SOUNDS.lock().unwrap_or_else(|e| e.into_inner());
    for Player(s) in slots.iter().flatten() {
        s.stop();
    }
    let slot = &mut slots[sound as usize];
    if slot.is_none() {
        let data = NSData::with_bytes(sound.wav());
        *slot = NSSound::initWithData(NSSound::alloc(), &data).map(Player);
    }
    match slot.as_ref() {
        Some(Player(s)) => {
            if !s.play() {
                tracing::debug!(?sound, "звук не сыгрался");
            }
        }
        None => tracing::debug!(?sound, "звук не загрузился"),
    }
}

/// Сыграть звук (Linux). WAV один раз кладётся в `$XDG_RUNTIME_DIR` (или в
/// каталог данных Clipwire); новый звук останавливает ещё играющий.
#[cfg(target_os = "linux")]
pub fn play(sound: Sound) {
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};

    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    static FILES: [OnceLock<Option<std::path::PathBuf>>; 2] = [OnceLock::new(), OnceLock::new()];
    static PLAYING: Mutex<Option<Child>> = Mutex::new(None);
    /// Первый проигрыватель, который нашёлся; `None` — никакого нет.
    static PLAYER: OnceLock<Option<(&str, &[&str])>> = OnceLock::new();

    let dir = DIR.get_or_init(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            // Без него — каталог данных Clipwire, он личный; в общий /tmp
            // с предсказуемым именем писать нельзя (подмена файла чужим
            // пользователем или ссылкой).
            .or_else(|| {
                clipwire_agent::state::Paths::resolve()
                    .ok()
                    .map(|p| p.dir)
                    .filter(|d| std::fs::create_dir_all(d).is_ok())
            })
    });
    let Some(dir) = dir else {
        tracing::debug!("звук: нет личного каталога");
        return;
    };
    let Some(file) = FILES[sound as usize]
        .get_or_init(|| {
            // Имя постоянное: файлы не копятся от запуска к запуску, а
            // содержимое у всех копий одинаковое.
            let name = match sound {
                Sound::Copy => "clipwire-copy.wav",
                Sound::Paste => "clipwire-paste.wav",
            };
            let path = dir.join(name);
            std::fs::write(&path, sound.wav())
                .map_err(|e| tracing::debug!(?sound, "звук не записался: {e}"))
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
            tracing::info!("звуки: нет pw-play, paplay или aplay");
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
        Err(e) => tracing::debug!(?sound, "звук не сыгрался: {e}"),
    }
    // Доигравший процесс забирается при следующем звуке (kill по завершённому
    // ничего не делает), зомби не копятся дольше одного.
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn play(_sound: Sound) {}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    #[test]
    fn sounds_load_and_repeat() {
        // Без звуковой карты (CI) play вернёт false, но не упадёт.
        super::play_copy();
        super::play(super::Sound::Paste);
        super::play_copy();
    }
}
