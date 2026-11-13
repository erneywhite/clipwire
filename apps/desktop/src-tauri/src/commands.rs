//! Команды, которые вызывает интерфейс через `invoke`.
//!
//! Ошибки возвращаются строкой: интерфейс показывает их как есть, поэтому
//! текст должен быть понятен человеку.

use clipwire_agent::history::Entry;
use clipwire_agent::settings::Settings;
use clipwire_agent::state::{AgentState, random_key};
use clipwire_core::{Content, GroupKey, Invite, ItemId, ServerInfo};
use serde::Serialize;
use tauri::Emitter;
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
pub async fn create_group(app: AppHandle, server_code: String) -> CmdResult<String> {
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
    crate::restart_service_joined(&app).await.map_err(err)?;
    Ok(agent.invite.encode())
}

/// Остальные устройства: войти по коду приглашения.
#[tauri::command]
pub async fn join_group(app: AppHandle, invite_code: String) -> CmdResult<()> {
    let invite = Invite::decode(&invite_code).map_err(err)?;
    ensure_unconfigured(&app)?;
    let agent = AgentState {
        invite,
        device_key: random_key().map_err(err)?,
    };
    app.state::<AppState>().paths.save(&agent).map_err(err)?;
    crate::restart_service_joined(&app).await.map_err(err)?;
    Ok(())
}

/// Элемент истории для интерфейса. В списке текст обрезан, полный — через `history_get`.
#[derive(Debug, Clone, Serialize)]
pub struct EntryDto {
    pub id: String,
    pub kind: &'static str,
    pub text: String,
    /// Полная длина в символах (для «44 символа» и решения, нужен ли предпросмотр).
    pub chars: usize,
    pub lines: usize,
    pub device_name: String,
    pub local: bool,
    pub pinned: bool,
    pub seen_ms: i64,
    /// Картинка: размеры в пикселях и вес PNG.
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// Миниатюра картинки для списка — data URL.
    pub thumb: Option<String>,
    /// Полная картинка — только в `history_get`, для предпросмотра.
    pub image: Option<String>,
    /// Текст с форматированием: «HTML» или «RTF» — для пометки в списке.
    pub rich: Option<&'static str>,
    /// HTML-фрагмент — только в `history_get`, для предпросмотра.
    pub html: Option<String>,
    /// Скопированные файлы и папки.
    pub files: Vec<FileDto>,
    /// Приложение, из которого скопировано; `None` — неизвестно.
    pub source_app: Option<SourceAppDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceAppDto {
    pub name: String,
    /// Иконка 32×32 — data URL PNG; `None`, если достать не удалось.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileDto {
    pub name: String,
    /// Путь на устройстве-источнике.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    /// `local_path` существует.
    pub exists: bool,
    /// Id переданного файла (32 hex), `None` — не передавался, есть только на
    /// источнике. Есть и у папок (архив), и у файлов больше порога.
    pub blob: Option<String>,
    /// Передаётся напрямую с устройства-источника, а не через сервер.
    pub p2p: bool,
    /// Сколько байт передаётся: у файла — его размер, у папки — размер архива.
    pub blob_size: u64,
    /// Путь на этом компьютере: исходный у своей записи, путь в кэше у
    /// скачанного чужого файла.
    pub local_path: Option<String>,
    /// Запись с другого устройства.
    pub remote: bool,
    /// Файл этого устройства уже на сервере: загрузка закончилась за
    /// последние сутки, в том числе до перезапуска.
    pub uploaded: bool,
}

/// Загружен ли файл на сервер (по журналу загрузок). В локальном режиме — нет.
pub type UploadedFn<'a> = &'a dyn Fn(clipwire_core::BlobId) -> bool;

/// Журнал загрузок текущего сервиса. Берётся до замка истории, чтобы не
/// держать оба замка сразу.
fn uploaded_check(state: &AppState) -> impl Fn(clipwire_core::BlobId) -> bool + use<> {
    let blobs = state
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.blobs());
    move |b| blobs.as_ref().is_some_and(|x| x.is_uploaded(b))
}

/// Где файл записи лежит на этом компьютере, если лежит. У своей записи —
/// исходный путь, а если файл с тех пор переместили или удалили — копия,
/// скачанная с сервера в кэш. У чужой — только кэш: её исходный путь с
/// другого устройства, здесь по нему может лежать совсем другой файл.
pub fn local_file(
    local: bool,
    f: &clipwire_core::FileRef,
    files_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if local {
        let p = std::path::PathBuf::from(&f.path);
        if p.exists() {
            return Some(p);
        }
    }
    clipwire_agent::blobs::cached_file(files_dir, f)
}

fn file_dto(
    local: bool,
    f: &clipwire_core::FileRef,
    files_dir: &std::path::Path,
    uploaded: UploadedFn<'_>,
) -> FileDto {
    let local_path = local_file(local, f, files_dir);
    FileDto {
        name: f.name.clone(),
        path: f.path.clone(),
        size: f.size,
        is_dir: f.is_dir,
        exists: local_path.is_some(),
        blob: f.blob.map(|b| b.hex()),
        p2p: f.p2p,
        blob_size: f.blob_size,
        local_path: local_path.map(|p| p.to_string_lossy().into_owned()),
        remote: !local,
        uploaded: f.blob.is_some_and(uploaded),
    }
}

/// Полная картинка файла-картинки для предпросмотра и редактора (data URL).
fn file_image_url(f: &clipwire_core::FileRef, path: &std::path::Path) -> Option<String> {
    let mime = clipwire_agent::image::image_mime(&f.name)?;
    if f.size > clipwire_agent::image::MAX_IMAGE_FILE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(format!(
        "data:{mime};base64,{}",
        data_encoding::BASE64.encode(&bytes)
    ))
}

/// Сколько символов текста отдавать в список: хватает на две строки превью.
const LIST_PREVIEW_CHARS: usize = 400;

fn png_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        data_encoding::BASE64.encode(png)
    )
}

fn to_dto(
    e: &Entry,
    thumb: Option<&[u8]>,
    full: bool,
    files_dir: &std::path::Path,
    uploaded: UploadedFn<'_>,
) -> EntryDto {
    let (kind, text, width, height, image) = match &e.content {
        Content::Text(t) | Content::Rich { text: t, .. } => ("text", t.as_str(), 0, 0, None),
        Content::Image { width, height, png } => ("image", "", *width, *height, Some(png)),
        Content::Files(f) => {
            let (w, h) = if f.len() == 1 {
                (f[0].width, f[0].height)
            } else {
                (0, 0)
            };
            ("files", "", w, h, None)
        }
        #[allow(unreachable_patterns)]
        _ => ("other", "", 0, 0, None),
    };
    let files: Vec<FileDto> = match &e.content {
        Content::Files(f) => f
            .iter()
            .map(|f| file_dto(e.local, f, files_dir, uploaded))
            .collect(),
        _ => Vec::new(),
    };
    let (rich, html) = match &e.content {
        Content::Rich { html, .. } => (
            Some(if html.is_some() { "HTML" } else { "RTF" }),
            html.as_ref().filter(|_| full).cloned(),
        ),
        _ => (None, None),
    };
    let shown: String = if full {
        text.to_string()
    } else {
        text.chars().take(LIST_PREVIEW_CHARS).collect()
    };
    EntryDto {
        id: e.id.0.to_string(),
        kind,
        text: shown,
        chars: text.chars().count(),
        lines: text.lines().count().max(1),
        device_name: e.device_name.clone(),
        local: e.local,
        pinned: e.pinned,
        seen_ms: e.seen_ms,
        width,
        height,
        bytes: match &e.content {
            Content::Files(f) => f.iter().map(|f| f.size as usize).sum(),
            c => c.len(),
        },
        thumb: thumb.map(png_url),
        image: match (&e.content, full) {
            (_, false) => None,
            (Content::Files(f), true) if f.len() == 1 && f[0].width > 0 => {
                local_file(e.local, &f[0], files_dir).and_then(|p| file_image_url(&f[0], &p))
            }
            (_, true) => image.map(|p| png_url(p)),
        },
        rich,
        html,
        files,
        source_app: e.source_app().map(|a| SourceAppDto {
            icon: (!a.icon_png.is_empty()).then(|| png_url(&a.icon_png)),
            name: a.name,
        }),
    }
}

pub fn parse_id(id: &str) -> CmdResult<ItemId> {
    id.parse()
        .map(ItemId)
        .map_err(|_| "Неверный id элемента".to_string())
}

#[tauri::command]
pub fn history_list(
    state: State<'_, AppState>,
    query: String,
    pinned_only: bool,
    limit: usize,
) -> Vec<EntryDto> {
    let files_dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
    let uploaded = uploaded_check(&state);
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    h.list(&query, pinned_only, limit.min(500))
        .into_iter()
        .map(|e| to_dto(e, h.thumb(e.id), false, &files_dir, &uploaded))
        .collect()
}

#[tauri::command]
pub fn history_get(state: State<'_, AppState>, id: String) -> CmdResult<EntryDto> {
    let id = parse_id(&id)?;
    let files_dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
    let uploaded = uploaded_check(&state);
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    h.get(id)
        .map(|e| to_dto(e, h.thumb(e.id), true, &files_dir, &uploaded))
        .ok_or_else(|| "Элемента уже нет в истории".into())
}

#[tauri::command]
pub fn history_pin(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    pinned: bool,
) -> CmdResult<()> {
    let id = parse_id(&id)?;
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_pinned(id, pinned)
        .map_err(err)?;
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

#[tauri::command]
pub fn history_delete(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let id = parse_id(&id)?;
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .delete(id)
        .map_err(err)?;
    crate::cache::prune_later(&app);
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

#[tauri::command]
pub fn history_clear(
    app: AppHandle,
    state: State<'_, AppState>,
    keep_pinned: bool,
) -> CmdResult<()> {
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear(keep_pinned)
        .map_err(err)?;
    crate::cache::prune_later(&app);
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Settings {
    state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[tauri::command]
pub async fn save_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let old = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    settings.save(&state.paths.dir).map_err(err)?;
    *state.settings.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
    *state.ignored_apps.lock().unwrap_or_else(|e| e.into_inner()) = settings.ignored_apps.clone();
    state.upload_limit_mb.store(
        settings.file_upload_limit_mb,
        std::sync::atomic::Ordering::Relaxed,
    );

    if old.popup_hotkey != settings.popup_hotkey
        && let Err(e) = crate::popup::register_hotkey(&app)
    {
        // Неудачное сочетание (занято другой программой) — откатываем на старое.
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        s.popup_hotkey = old.popup_hotkey.clone();
        let _ = s.save(&state.paths.dir);
        drop(s);
        let _ = crate::popup::register_hotkey(&app);
        return Err(e);
    }
    if old.explorer_menu != settings.explorer_menu {
        crate::send_to::set_menu(settings.explorer_menu);
    }
    if old.win_v != settings.win_v {
        crate::winv::set(&app, settings.win_v);
    }
    if old.hide_from_capture != settings.hide_from_capture {
        crate::apply_capture_protection(&app, settings.hide_from_capture);
    }
    if old.device_name != settings.device_name {
        crate::restart_service(&app).await.map_err(err)?;
    }
    let _ = app.emit("clipwire://settings-changed", &settings);
    Ok(())
}

/// Отключить устройство от группы: ключи удаляются, локальная история остаётся.
#[tauri::command]
pub async fn leave_group(app: AppHandle) -> CmdResult<()> {
    crate::stop_service(&app).await;
    let reset = app.state::<AppState>().paths.reset();
    // Сервис запускаем в любом случае: даже если выход не удался, приложение
    // не должно остаться без синхронизации и окна вставки. Если ключ
    // удалился, дальше работаем локально, история остаётся.
    let started = start_service(&app);
    publish_status(&app);
    reset.map_err(err)?;
    started.map_err(err)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub version: String,
    pub relay_url: Option<String>,
    /// Короткий отпечаток устройства (первые байты endpoint ID).
    pub device_id: Option<String>,
}

#[tauri::command]
pub fn device_info(app: AppHandle) -> DeviceInfo {
    let state = app.state::<AppState>();
    let agent = state.paths.load().ok().flatten();
    let device_id = state
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| {
            s.device().0[..5]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        });
    DeviceInfo {
        version: app.package_info().version.to_string(),
        relay_url: agent.map(|a| a.invite.server.relay_url),
        device_id,
    }
}

/// Положить текст в буфер, не отправляя в синхронизацию (код приглашения и т. п.).
#[tauri::command]
pub fn copy_quietly(state: State<'_, AppState>, text: String) -> CmdResult<()> {
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    match svc.as_ref() {
        Some(s) => s.put_quietly(&text),
        None => clipwire_agent::clipboard::write_text(&text),
    }
    .map_err(err)
}

/// Сохранить правку текста элемента в истории.
#[tauri::command]
pub fn history_update(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
) -> CmdResult<()> {
    let id = parse_id(&id)?;
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update_text(id, text)
        .map_err(err)?;
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

#[tauri::command]
pub fn set_window_background(app: AppHandle, color: String) {
    crate::apply_window_background(&app, &color);
}

/// Поставить обновление сейчас (кнопка в «О программе»). Движок
/// останавливается так же, как при выходе; на Windows процесс завершается.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> CmdResult<()> {
    crate::updates::install(app).await
}

/// Версия, которая уже скачана и поставится сама, или `null`.
#[tauri::command]
pub fn update_ready(app: AppHandle) -> Option<String> {
    crate::updates::ready_version(&app)
}
