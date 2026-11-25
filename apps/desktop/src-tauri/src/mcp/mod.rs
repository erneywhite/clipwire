//! Доступ ИИ-инструментов к истории через MCP: сторона приложения.
//!
//! Мост (`clipwire-desktop --mcp`, его запускает ИИ-клиент) ходит сюда по
//! локальному каналу ([`channel`]). Здесь вся политика: что видно
//! (`clipwire_agent::ai_access`), согласие человека ([`consent`], окно
//! `consent`), запись и журнал обращений ([`handler`]).

pub mod channel;
pub mod consent;
#[cfg(test)]
mod e2e;
pub mod handler;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clipwire_agent::history::{AiLogEntry, now_ms};
use clipwire_agent::settings::{AiClient, AiSettings};
use clipwire_core::ai::AiAccess;
use clipwire_core::{Content, Ext, ItemId};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use consent::{Busy, Choice, ConsentDto, ConsentState};
use handler::{AppStatus, CONSENT_TIMEOUT, Host, Store};

/// Метка окна согласия.
pub const CONSENT_WINDOW: &str = "consent";
/// Как часто сохраняется `last_ms` клиентов: обращения идут десятками.
const TOUCH_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Адрес занятого канала: файл адреса убирается при выходе.
static ENDPOINT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Занять канал и обслуживать мост в фоне. Канал не занялся (его держит
/// другой экземпляр с той же папкой данных, нет прав) — предупреждение в
/// журнал, приложение работает дальше без MCP.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let dir = app.state::<AppState>().paths.dir.clone();
        let bound = match channel::bind(&dir).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                tracing::warn!(
                    "канал MCP уже занят другим запущенным Clipwire с той же папкой данных"
                );
                return;
            }
            Err(e) => {
                tracing::warn!("канал MCP не создался: {e:#}");
                return;
            }
        };
        tracing::info!("канал MCP: {}", bound.endpoint());
        *ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()) = Some(bound.endpoint().to_string());
        let host = Arc::new(AppHost { app });
        bound.run(move || handler::Conn::new(host.clone())).await;
    });
}

/// Убрать файл адреса канала при выходе.
pub fn shutdown(app: &AppHandle) {
    if let Some(ep) = ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()).take() {
        channel::cleanup(&app.state::<AppState>().paths.dir, &ep);
    }
}

/// Приложение для обработчика запросов.
struct AppHost {
    app: AppHandle,
}

impl AppHost {
    fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    async fn ask(&self, dto: ConsentDto) -> Result<Option<Choice>, Busy> {
        let app = self.app.clone();
        self.app
            .state::<ConsentState>()
            .ask(dto, CONSENT_TIMEOUT, move |any| consent_changed(&app, any))
            .await
    }
}

impl Host for AppHost {
    fn ai_settings(&self) -> AiSettings {
        self.state()
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ai
            .clone()
    }

    fn ignored_apps(&self) -> Vec<String> {
        self.state()
            .ignored_apps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store(&self) -> Store {
        let st = self.state();
        Store {
            history: st.history.clone(),
            reader: st.reader.clone(),
            files_dir: clipwire_agent::blobs::files_dir(&st.paths.dir),
        }
    }

    fn status(&self) -> AppStatus {
        let st = self.state();
        let group = st
            .paths
            .load()
            .ok()
            .flatten()
            .map(|a| crate::admin::group_fingerprint(&a.invite.group_key.group_id()));
        let connected = matches!(crate::current_status(&self.app), crate::StatusDto::Online);
        AppStatus { group, connected }
    }

    async fn ask_client(
        &self,
        client: &str,
        version: &str,
        allow_write: bool,
    ) -> Result<Option<AiAccess>, Busy> {
        let Some(choice) = self
            .ask(ConsentDto::client(client, version, allow_write))
            .await?
        else {
            return Ok(None);
        };
        // `ConsentState::answer` уже привёл ответ к вопросу: «чтение и
        // запись» приходит, только если их предлагали.
        Ok(Some(match choice {
            Choice::Read => AiAccess::Read,
            Choice::ReadWrite if allow_write => AiAccess::ReadWrite,
            Choice::ReadWrite => AiAccess::Read,
            Choice::Deny | Choice::Accept | Choice::Decline => AiAccess::Denied,
            // До ждущих `Dismiss` не доходит (`ConsentState::answer`).
            Choice::Dismiss => return Ok(None),
        }))
    }

    async fn ask_clipboard(&self, client: &str, text: &str) -> Result<bool, Busy> {
        Ok(self.ask(ConsentDto::clipboard(client, text)).await? == Some(Choice::Accept))
    }

    fn remember_client(&self, name: &str, access: AiAccess) {
        let now = now_ms();
        update_settings(&self.app, |ai| {
            match ai.clients.iter_mut().find(|c| c.name == name) {
                Some(c) => {
                    c.access = access;
                    c.last_ms = now;
                }
                None => ai.clients.push(AiClient {
                    name: name.to_string(),
                    access,
                    added_ms: now,
                    last_ms: now,
                }),
            }
        });
    }

    fn touch_client(&self, name: &str) {
        let now = now_ms();
        {
            let st = self.state();
            let mut s = st.settings.lock().unwrap_or_else(|e| e.into_inner());
            let Some(c) = s.ai.clients.iter_mut().find(|c| c.name == name) else {
                return;
            };
            c.last_ms = now;
        }
        // Сохранение и событие — одно на серию обращений.
        static PENDING: AtomicBool = AtomicBool::new(false);
        if PENDING.swap(true, Ordering::AcqRel) {
            return;
        }
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(TOUCH_SAVE_DELAY).await;
            PENDING.store(false, Ordering::Release);
            // Запись на диск — не на потоке рантайма.
            let _ =
                tauri::async_runtime::spawn_blocking(move || update_settings(&app, |_| {})).await;
        });
    }

    fn save(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<ItemId> {
        let st = self.state();
        let svc = st.service.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = svc.as_ref() else {
            anyhow::bail!("Clipwire is starting, try again");
        };
        s.save(content, None, extra)
    }

    fn copy(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<()> {
        let st = self.state();
        let svc = st.service.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = svc.as_ref() else {
            anyhow::bail!("Clipwire is starting, try again");
        };
        s.copy_marked(content, None, extra)
    }

    fn pin(&self, id: ItemId, note: Option<&str>) -> anyhow::Result<()> {
        // Тот же порядок, что у `history_pin` / `history_note`: сначала
        // очередь закрепов группы, потом история.
        let st = self.state();
        let note = note.map(clipwire_agent::history::clean_note);
        let hash = {
            let h = st.history.lock().unwrap_or_else(|e| e.into_inner());
            h.row(id)
                .map(|r| r.card.hash)
                .ok_or_else(|| anyhow::anyhow!("the entry is gone"))?
        };
        if let Some(pins) = crate::commands::group_pins(&st) {
            pins.pin(hash, note.as_deref().unwrap_or_default());
            // У живого закрепа с подписью `pin` подпись группы не трогает:
            // новая подпись — отдельной правкой, как у `history_note`.
            if let Some(n) = &note {
                pins.set_note(hash, n);
            }
        }
        {
            let mut h = st.history.lock().unwrap_or_else(|e| e.into_inner());
            h.set_pinned(id, true)?;
            if let Some(n) = &note {
                h.set_note(id, n)?;
            }
        }
        let _ = self.app.emit("clipwire://history-changed", ());
        Ok(())
    }

    fn notify(&self, text: &str) {
        crate::updates::notify(&self.app, text);
    }

    fn logged(&self) {
        let _ = self.app.emit("clipwire://ai-log", ());
    }
}

/// Поменять `settings.ai`, сохранить и разослать `clipwire://settings-changed`.
///
/// Запись на диск и событие идут под замком настроек, как и в
/// `save_settings`: иначе снимок, снятый до чужой записи, ляжет на диск
/// после неё, а событие со старым снимком откатит окно настроек.
fn update_settings(app: &AppHandle, f: impl FnOnce(&mut AiSettings)) {
    let st = app.state::<AppState>();
    let mut s = st.settings.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut s.ai);
    if let Err(e) = s.save(&st.paths.dir) {
        tracing::warn!("настройки ИИ не сохранились: {e:#}");
    }
    let _ = app.emit("clipwire://settings-changed", &*s);
}

/// Очередь вопросов изменилась: окно показать или закрыть.
fn consent_changed(app: &AppHandle, any: bool) {
    let _ = app.emit("clipwire://consent", ());
    let app = app.clone();
    // Окно создаётся и закрывается не из главного потока (`ensure_window`).
    tauri::async_runtime::spawn_blocking(move || {
        if any {
            match crate::ensure_window(&app, CONSENT_WINDOW) {
                Ok((w, _)) => {
                    // В окне может быть пароль или токен, который ИИ кладёт в буфер.
                    let _ = w.set_content_protected(crate::hide_from_capture(&app));
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
                Err(e) => tracing::warn!("окно согласия не открылось: {e}"),
            }
        } else {
            let consent = app.state::<ConsentState>();
            crate::destroy_window_if(&app, CONSENT_WINDOW, |_| consent.pending().is_none());
        }
    });
}

/// Окно согласия закрыли крестиком или Alt+F4: показанный вопрос остаётся
/// без ответа, как по сроку.
pub fn on_consent_close(app: &AppHandle) {
    let any = app.state::<ConsentState>().dismiss_shown();
    consent_changed(app, any);
}

// ---------------------------------------------------------------------------
// Команды для интерфейса
// ---------------------------------------------------------------------------

/// Как запустить мост: путь к программе и аргументы.
#[derive(Debug, Clone, Serialize)]
pub struct BridgeCommand {
    pub exe: String,
    pub args: Vec<String>,
}

#[tauri::command]
pub fn ai_bridge_command() -> Result<BridgeCommand, String> {
    // AppImage распаковывается в новый каталог на каждый запуск: стабильный
    // путь — сам файл образа.
    #[cfg(target_os = "linux")]
    let exe = std::env::var_os("APPIMAGE")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    #[cfg(not(target_os = "linux"))]
    let exe: Option<std::path::PathBuf> = None;
    let exe = match exe {
        Some(p) => p,
        None => std::env::current_exe().map_err(|e| e.to_string())?,
    };
    Ok(BridgeCommand {
        exe: exe.to_string_lossy().into_owned(),
        args: bridge_args(std::env::var_os("CLIPWIRE_HOME").map(std::path::PathBuf::from)),
    })
}

/// Аргументы моста. Своя папка данных (`CLIPWIRE_HOME`: dev-сборка,
/// переносная копия) передаётся `--home`, чтобы мост нашёл тот же канал.
/// Путь — абсолютный: мост запускается ИИ-клиентом из другого рабочего
/// каталога и без нашего окружения.
fn bridge_args(home: Option<std::path::PathBuf>) -> Vec<String> {
    let mut args = vec!["--mcp".to_string()];
    if let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) {
        let home = std::path::absolute(&home).unwrap_or(home);
        args.push("--home".into());
        args.push(home.to_string_lossy().into_owned());
    }
    args
}

/// Поменять доступ запомненного клиента. Список клиентов меняют только эта
/// команда, [`ai_client_remove`] и канал MCP: `save_settings` его не трогает,
/// чтобы копия окна не затёрла клиента, которого только что добавил канал.
#[tauri::command]
pub async fn ai_client_access(app: AppHandle, name: String, access: AiAccess) {
    let _ = tauri::async_runtime::spawn_blocking(move || {
        update_settings(&app, |ai| set_client_access(ai, &name, access));
    })
    .await;
}

/// Забыть клиента: при следующем обращении его спросят заново.
#[tauri::command]
pub async fn ai_client_remove(app: AppHandle, name: String) {
    let _ = tauri::async_runtime::spawn_blocking(move || {
        update_settings(&app, |ai| ai.clients.retain(|c| c.name != name));
    })
    .await;
}

/// Доступ клиента `name`; клиента, которого нет (его уже забыли), не добавляет.
fn set_client_access(ai: &mut AiSettings, name: &str, access: AiAccess) {
    if let Some(c) = ai.clients.iter_mut().find(|c| c.name == name) {
        c.access = access;
    }
}

#[tauri::command]
pub fn consent_pending(consent: tauri::State<'_, ConsentState>) -> Option<ConsentDto> {
    consent.show()
}

#[tauri::command]
pub fn consent_answer(app: AppHandle, id: String, choice: Choice) {
    let any = app.state::<ConsentState>().answer(&id, choice);
    consent_changed(&app, any);
}

#[tauri::command]
pub async fn ai_log(app: AppHandle, limit: usize) -> Vec<AiLogEntry> {
    let st = app.state::<AppState>();
    let (history, reader) = (st.history.clone(), st.reader.clone());
    tauri::async_runtime::spawn_blocking(move || {
        clipwire_agent::history::read(&history, reader.as_ref(), |h| {
            h.ai_log_list(limit.min(clipwire_agent::history::AI_LOG_KEEP))
        })
    })
    .await
    .unwrap_or_default()
}

#[tauri::command]
pub fn ai_log_clear(app: AppHandle) -> Result<(), String> {
    app.state::<AppState>()
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ai_log_clear()
        .map_err(|e| e.to_string())?;
    let _ = app.emit("clipwire://ai-log", ());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_client_access_changes_only_known_client() {
        let mut ai = AiSettings::default();
        ai.clients.push(AiClient {
            name: "codex".into(),
            access: AiAccess::Read,
            added_ms: 1,
            last_ms: 2,
        });
        set_client_access(&mut ai, "codex", AiAccess::ReadWrite);
        set_client_access(&mut ai, "gone", AiAccess::ReadWrite);
        assert_eq!(ai.clients.len(), 1);
        assert_eq!(ai.clients[0].access, AiAccess::ReadWrite);
        assert_eq!(ai.clients[0].last_ms, 2);
    }

    #[test]
    fn bridge_args_point_bridge_to_same_home() {
        assert_eq!(bridge_args(None), ["--mcp"]);
        assert_eq!(bridge_args(Some("".into())), ["--mcp"]);
        // Относительная папка данных становится абсолютной, и мост
        // разбирает её обратно в тот же путь.
        let args = bridge_args(Some("dev-home".into()));
        let home = crate::mcp_bridge::home_from_args(args.iter().map(Into::into)).unwrap();
        assert!(home.is_absolute(), "{home:?}");
        assert_eq!(home, std::env::current_dir().unwrap().join("dev-home"));
    }
}
