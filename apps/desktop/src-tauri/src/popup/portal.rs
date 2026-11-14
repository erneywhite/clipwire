//! Вставка на Wayland через портал удалённого управления
//! (`org.freedesktop.portal.RemoteDesktop`).
//!
//! Wayland не даёт программам нажимать клавиши в чужих окнах. Законный путь —
//! портал, которым пользуются программы удалённого доступа: система один раз
//! спрашивает разрешение («управлять клавиатурой»), а Clipwire после выбора
//! элемента нажимает только Ctrl+V. Разрешение запоминается токеном
//! восстановления (`PersistMode::ExplicitlyRevoked`), он одноразовый: после
//! каждого запуска сеанса сохраняется новый.
//!
//! Пока сеанс открыт, GNOME показывает значок удалённого управления в панели,
//! поэтому сеанс открывается при вставке и закрывается через минуту простоя.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use tokio::sync::Mutex;

const XK_CONTROL_L: i32 = 0xffe3;
const XK_V: i32 = 0x0076;
/// Сколько сеанс живёт после последней вставки.
const IDLE: Duration = Duration::from_secs(60);

struct Live {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    last: Instant,
}

/// Состояние портала на время работы программы.
#[derive(Clone)]
pub struct Portal {
    live: Arc<Mutex<Option<Live>>>,
    token_file: PathBuf,
}

impl Portal {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Portal {
            live: Arc::default(),
            token_file: data_dir.join("portal-token"),
        }
    }

    /// Открыть сеанс: при первом разе (или если разрешение отозвали) система
    /// покажет диалог. Отказ — ошибка.
    async fn open(&self) -> Result<Live, String> {
        let token = std::fs::read_to_string(&self.token_file)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let proxy = RemoteDesktop::new().await.map_err(|e| e.to_string())?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(|e| e.to_string())?;
        proxy
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::KEYBOARD)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await
            .map_err(|e| e.to_string())?;
        let started = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        if !started.devices().contains(DeviceType::KEYBOARD) {
            let _ = session.close().await;
            return Err("клавиатуру не разрешили".into());
        }
        if let Some(t) = started.restore_token() {
            write_private(&self.token_file, t);
        }
        Ok(Live {
            proxy,
            session,
            last: Instant::now(),
        })
    }

    /// Включение в настройках: получить разрешение (диалог системы) и сразу
    /// закрыть сеанс, чтобы значок в панели не висел.
    pub async fn enable(&self) -> Result<(), String> {
        let live = self.open().await?;
        let _ = live.session.close().await;
        Ok(())
    }

    /// Выключение: закрыть сеанс и забыть разрешение.
    pub async fn disable(&self) {
        if let Some(live) = self.live.lock().await.take() {
            let _ = live.session.close().await;
        }
        let _ = std::fs::remove_file(&self.token_file);
    }

    /// Нажать Ctrl+V в активном окне. Окно вставки уже спрятано.
    pub async fn paste(&self) -> Result<(), String> {
        let mut guard = self.live.lock().await;
        if guard.is_none() {
            *guard = Some(self.open().await?);
            self.close_when_idle();
        }
        // Фокусу нужно вернуться в прежнее окно после того, как наше спряталось.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let live = guard.as_mut().expect("сеанс открыт");
        if let Err(e) = press_ctrl_v(live).await {
            // Сеанс мог закрыть сам композитор: открываем заново один раз.
            tracing::debug!("сеанс портала не ответил, открываю заново: {e}");
            let _ = live.session.close().await;
            *guard = Some(self.open().await?);
            let live = guard.as_mut().expect("сеанс открыт");
            press_ctrl_v(live).await?;
        }
        if let Some(live) = guard.as_mut() {
            live.last = Instant::now();
        }
        Ok(())
    }

    fn close_when_idle(&self) {
        let live = self.live.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let mut guard = live.lock().await;
                match guard.as_ref() {
                    None => return,
                    Some(l) if l.last.elapsed() >= IDLE => {
                        if let Some(l) = guard.take() {
                            let _ = l.session.close().await;
                        }
                        return;
                    }
                    Some(_) => {}
                }
            }
        });
    }
}

async fn press_ctrl_v(live: &Live) -> Result<(), String> {
    let key = |sym, state| {
        live.proxy
            .notify_keyboard_keysym(&live.session, sym, state, Default::default())
    };
    key(XK_CONTROL_L, KeyState::Pressed)
        .await
        .map_err(|e| e.to_string())?;
    let res = async {
        key(XK_V, KeyState::Pressed).await?;
        key(XK_V, KeyState::Released).await
    }
    .await;
    // Ctrl отпускаем в любом случае, иначе он «залипнет».
    let _ = key(XK_CONTROL_L, KeyState::Released).await;
    res.map_err(|e| e.to_string())
}

/// Токен — доступ к клавиатуре: файл только для владельца.
fn write_private(path: &std::path::Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(e) = res {
        tracing::warn!("токен портала не сохранился: {e}");
    }
}
