//! Состояние агента на диске.
//!
//! - `state.bin` — код приглашения (сервер + ключ группы) и ключ устройства,
//!   защищён через [`crate::secret`];
//! - `cursor` — id последнего элемента, который агент уже видел. Секретом не
//!   является, меняется часто, поэтому лежит отдельно открытым текстом.

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
    pub fn reset(&self) -> Result<()> {
        let uploads = self.dir.join(crate::blobs::UPLOADS_FILE);
        for p in [self.state(), self.cursor(), uploads] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("не удалился {}", p.display())),
            }
        }
        for dir in [
            crate::blobs::outbox_dir(&self.dir),
            crate::blobs::files_dir(&self.dir),
        ] {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("не удалился {}", dir.display()));
                }
            }
        }
        Ok(())
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

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
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
        let _ = std::fs::remove_dir_all(dir);
    }
}
