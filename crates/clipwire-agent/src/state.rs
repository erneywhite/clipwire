//! Состояние агента на диске.
//!
//! - `state.bin` — код приглашения (сервер + ключ группы) и ключ устройства,
//!   защищён через [`crate::secret`];
//! - `cursor` — id последнего элемента, который агент уже видел. Секретом не
//!   является, меняется часто, поэтому лежит отдельно открытым текстом;
//! - `devices.bin` — проверенные карточки устройств группы, `excluded` —
//!   отметка «устройство исключено» (`crate::devices`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clipwire_core::{Invite, ItemId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub invite: Invite,
    /// Секретный ключ Ed25519 устройства. Из него получается endpoint ID,
    /// под которым устройство известно серверу.
    pub device_key: [u8; 32],
}

impl std::fmt::Debug for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentState")
            .field("relay", &self.invite.server.relay_url)
            .finish_non_exhaustive()
    }
}

pub struct Paths {
    pub dir: PathBuf,
}

/// Начало имени папки, куда при выходе из группы уходит кэш скачанных файлов.
const TRASH_PREFIX: &str = "files.trash-";

impl Paths {
    /// `%APPDATA%\Clipwire` на Windows, `~/Library/Application Support/Clipwire`
    /// на macOS, `~/.config/clipwire` на Linux. `CLIPWIRE_HOME` переопределяет
    /// (удобно, чтобы запустить два агента на одной машине для проверки).
    pub fn resolve() -> Result<Self> {
        if let Some(dir) = std::env::var_os("CLIPWIRE_HOME") {
            return Ok(Self { dir: dir.into() });
        }
        let dirs = directories::ProjectDirs::from("", "", "Clipwire")
            .context("не удалось определить каталог настроек пользователя")?;
        Ok(Self {
            dir: dirs.config_dir().to_path_buf(),
        })
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state.bin")
    }

    fn cursor(&self) -> PathBuf {
        self.dir.join("cursor")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn load(&self) -> Result<Option<AgentState>> {
        let raw = match std::fs::read(self.state()) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("не читается state.bin"),
        };
        let plain = crate::secret::unprotect(&raw)?;
        Ok(Some(
            postcard::from_bytes(&plain).context("state.bin повреждён")?,
        ))
    }

    pub fn save(&self, state: &AgentState) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("не создался каталог {}", self.dir.display()))?;
        let plain = postcard::to_stdvec(state)?;
        write_atomic(&self.state(), &crate::secret::protect(&plain)?)
    }

    /// Выход из группы: ключ, курсор, очередь загрузок, архивы папок в
    /// outbox (всё это уходило в старую группу, в новой им не место) и кэш
    /// скачанных файлов группы: на диске не должно оставаться её следов.
    ///
    /// Сначала ключ, курсор и очередь: без них устройство уже не в группе, и
    /// ошибка здесь — ошибка выхода. Кэш скачанного чистится по возможности:
    /// скачанный файл может быть открыт в другой программе (на Windows такой
    /// файл не удалить), и выход из-за этого не должен ломаться. Кэш
    /// переименовывается в `files.trash-<uuid>` и удаляется; что не удалилось,
    /// остаётся там и убирается при следующем запуске ([`Paths::clean_trash`]).
    pub fn reset(&self) -> Result<()> {
        let uploads = self.dir.join(crate::blobs::UPLOADS_FILE);
        let known = self.dir.join(crate::devices::KNOWN_FILE);
        let excluded = self.dir.join(crate::devices::EXCLUDED_FILE);
        for p in [self.state(), self.cursor(), uploads, known, excluded] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("не удалился {}", p.display())),
            }
        }
        self.trash_files();
        let outbox = crate::blobs::outbox_dir(&self.dir);
        match std::fs::remove_dir_all(&outbox) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("не удалился {}", outbox.display()));
            }
        }
        Ok(())
    }

    /// Ключ группы сменился: очередь загрузок и архивы папок в outbox
    /// относились к старой группе, загружать их больше некуда.
    pub fn forget_uploads(&self) -> Result<()> {
        let uploads = self.dir.join(crate::blobs::UPLOADS_FILE);
        match std::fs::remove_file(&uploads) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("не удалился {}", uploads.display())),
        }
        let outbox = crate::blobs::outbox_dir(&self.dir);
        match std::fs::remove_dir_all(&outbox) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("не удалился {}", outbox.display())),
        }
    }

    /// Убрать кэш скачанных файлов, не падая на занятых файлах.
    fn trash_files(&self) {
        let files = crate::blobs::files_dir(&self.dir);
        let trash = self
            .dir
            .join(format!("{TRASH_PREFIX}{}", uuid::Uuid::now_v7().simple()));
        let target = match std::fs::rename(&files, &trash) {
            Ok(()) => trash,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            // Папку с открытым файлом Windows не переименует: удаляем на
            // месте всё, что получится.
            Err(e) => {
                tracing::warn!("кэш файлов не переименовался: {e}");
                files
            }
        };
        if let Err(e) = std::fs::remove_dir_all(&target) {
            tracing::warn!(
                "кэш файлов удалился не целиком ({}): {e}. Остаток уберётся при следующем запуске",
                target.display()
            );
        }
    }

    /// Удалить остатки кэша, которые не удалились при выходе из группы
    /// (`files.trash-*`). По возможности: занятое остаётся до следующего раза.
    /// Возвращает, сколько папок удалено целиком.
    pub fn clean_trash(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with(TRASH_PREFIX) {
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::info!("остаток кэша пока не удалился: {e}"),
            }
        }
        removed
    }

    pub fn load_cursor(&self) -> Option<ItemId> {
        let s = std::fs::read_to_string(self.cursor()).ok()?;
        s.trim().parse().ok().map(ItemId)
    }

    pub fn save_cursor(&self, id: ItemId) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.cursor(), id.0.to_string().as_bytes())
    }
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("не записался {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("не записался {}", path.display()))?;
    Ok(())
}

pub fn random_key() -> Result<[u8; 32]> {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{GroupKey, ServerInfo};

    #[test]
    fn save_load_reset() {
        let dir = std::env::temp_dir().join(format!("clipwire-state-{}", uuid::Uuid::now_v7()));
        let paths = Paths { dir: dir.clone() };
        assert!(paths.load().unwrap().is_none());

        let state = AgentState {
            invite: Invite {
                server: ServerInfo {
                    server_id: [1; 32],
                    relay_url: "https://r".into(),
                    relay_token: "t".into(),
                },
                group_key: GroupKey([2; 32]),
            },
            device_key: [3; 32],
        };
        paths.save(&state).unwrap();
        let back = paths.load().unwrap().unwrap();
        assert_eq!(back.invite, state.invite);
        assert_eq!(back.device_key, state.device_key);

        // На диске ключа группы в открытом виде нет (на Windows — DPAPI).
        #[cfg(windows)]
        {
            let raw = std::fs::read(dir.join("state.bin")).unwrap();
            assert!(!raw.windows(32).any(|w| w == [2u8; 32]));
        }

        let id = ItemId::new();
        paths.save_cursor(id).unwrap();
        assert_eq!(paths.load_cursor(), Some(id));

        std::fs::write(dir.join("uploads.json"), b"[]").unwrap();
        std::fs::write(dir.join("devices.bin"), b"x").unwrap();
        std::fs::write(dir.join("excluded"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("outbox")).unwrap();
        std::fs::write(dir.join("outbox").join("a.tar"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("files").join("ab12")).unwrap();
        std::fs::write(dir.join("files").join("ab12").join("отчёт.pdf"), b"x").unwrap();
        paths.reset().unwrap();
        assert!(!dir.join("outbox").exists());
        assert!(!dir.join("files").exists());
        assert!(paths.load().unwrap().is_none());
        assert!(paths.load_cursor().is_none());
        assert!(!dir.join("uploads.json").exists());
        assert!(!dir.join("devices.bin").exists());
        assert!(!dir.join("excluded").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Скачанный файл открыт другой программой без `FILE_SHARE_DELETE`
    /// (так открывают многие редакторы): выход из группы всё равно проходит,
    /// а остаток кэша убирается позже.
    #[cfg(windows)]
    #[test]
    fn reset_survives_open_file_in_cache() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;

        let dir = std::env::temp_dir().join(format!("clipwire-reset-{}", uuid::Uuid::now_v7()));
        let paths = Paths { dir: dir.clone() };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.bin"), b"x").unwrap();
        std::fs::write(dir.join("cursor"), b"x").unwrap();
        std::fs::write(dir.join("uploads.json"), b"[]").unwrap();
        let cached = dir.join("files").join("ab12").join("отчёт.pdf");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"x").unwrap();
        std::fs::write(dir.join("files").join("ab12").join("другой.txt"), b"y").unwrap();
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&cached)
            .unwrap();

        paths.reset().unwrap();
        assert!(!dir.join("state.bin").exists());
        assert!(!dir.join("cursor").exists());
        assert!(!dir.join("uploads.json").exists());
        assert!(!dir.join("files").join("ab12").join("другой.txt").exists());
        // Открытый файл удалить не дали: он остался в кэше или в корзине.
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("files"))
            .count();
        assert_eq!(leftovers, 1);

        drop(open);
        // Если кэш успел переименоваться, остаток в files.trash-*: его
        // убирает запуск. Иначе остался на месте, и его добирает повторный выход.
        paths.clean_trash();
        paths.reset().unwrap();
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clean_trash_removes_only_leftovers() {
        let dir = std::env::temp_dir().join(format!("clipwire-trash-{}", uuid::Uuid::now_v7()));
        let paths = Paths { dir: dir.clone() };
        std::fs::create_dir_all(dir.join("files.trash-1").join("ab")).unwrap();
        std::fs::write(dir.join("files.trash-1").join("ab").join("x"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("files")).unwrap();
        assert_eq!(paths.clean_trash(), 1);
        assert!(!dir.join("files.trash-1").exists());
        assert!(dir.join("files").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn core_device_id_matches_iroh_endpoint_id() {
        // Карточки устройств подписываются в core через ed25519-dalek, а id
        // устройства — это id эндпоинта iroh. Они обязаны совпадать.
        let key = random_key().unwrap();
        let iroh_id = iroh::SecretKey::from_bytes(&key).public();
        assert_eq!(
            clipwire_core::device::device_id(&key).0,
            *iroh_id.as_bytes()
        );
    }
}
