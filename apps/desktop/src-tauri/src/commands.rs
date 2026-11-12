//! Команды, которые вызывает интерфейс через `invoke`.
//!
//! Ошибки возвращаются строкой: интерфейс показывает их как есть, поэтому
//! текст должен быть понятен человеку.

use clipwire_agent::state::{AgentState, random_key};
use clipwire_core::{GroupKey, Invite, ServerInfo};
use tauri::{AppHandle, Manager, State};

use crate::{AppState, StatusDto, current_status, publish_status, start_service};

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command]
pub fn get_status(app: AppHandle) -> StatusDto {
    current_status(&app)
}

#[tauri::command]
pub fn set_paused(app: AppHandle, state: State<'_, AppState>, paused: bool) {
    if let Some(s) = state
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        s.set_paused(paused);
    }
    publish_status(&app);
}

/// Код приглашения для нового устройства (в нём ключ группы).
#[tauri::command]
pub fn invite_code(state: State<'_, AppState>) -> CmdResult<String> {
    let agent = state
        .paths
        .load()
        .map_err(err)?
        .ok_or("Устройство ещё не в группе")?;
    Ok(agent.invite.encode())
}

fn ensure_unconfigured(app: &AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if state.paths.load().map_err(err)?.is_some() {
        return Err("Устройство уже в группе".into());
    }
    Ok(())
}

/// Первое устройство: создать группу по коду сервера. Возвращает код приглашения.
#[tauri::command]
pub fn create_group(app: AppHandle, server_code: String) -> CmdResult<String> {
    let server = ServerInfo::decode(&server_code).map_err(err)?;
    ensure_unconfigured(&app)?;
    let agent = AgentState {
        invite: Invite {
            server,
            group_key: GroupKey::generate().map_err(err)?,
        },
        device_key: random_key().map_err(err)?,
    };
    app.state::<AppState>().paths.save(&agent).map_err(err)?;
    start_service(&app).map_err(err)?;
    Ok(agent.invite.encode())
}

/// Остальные устройства: войти по коду приглашения.
#[tauri::command]
pub fn join_group(app: AppHandle, invite_code: String) -> CmdResult<()> {
    let invite = Invite::decode(&invite_code).map_err(err)?;
    ensure_unconfigured(&app)?;
    let agent = AgentState {
        invite,
        device_key: random_key().map_err(err)?,
    };
    app.state::<AppState>().paths.save(&agent).map_err(err)?;
    start_service(&app).map_err(err)?;
    Ok(())
}
