//! Команды, которые вызывает интерфейс через `invoke`.
//!
//! Ошибки возвращаются строкой: интерфейс показывает их как есть, поэтому
//! текст должен быть понятен человеку.

use clipwire_agent::history::{CardKind, Entry, RichFormat, Row};
use clipwire_agent::i18n::tr;
use clipwire_agent::settings::Settings;
use clipwire_agent::state::{AgentState, random_key};
use clipwire_core::{Content, GroupKey, Invite, ItemId};
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
        .ok_or(tr("Устройство ещё не в группе"))?;
    Ok(agent.invite.encode())
}

fn ensure_unconfigured(app: &AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if state.paths.load().map_err(err)?.is_some() {
        return Err(tr("Устройство уже в группе").into());
    }
    Ok(())
}

/// Первое устройство: создать группу по коду сервера администратора
/// (`cwserver2-…`). Ключ администратора сохраняется: с ним движок создаёт
/// группу, а устройство видит раздел «Сервер». Возвращает код приглашения.
#[tauri::command]
pub async fn create_group(app: AppHandle, server_code: String) -> CmdResult<String> {
    let admin = clipwire_agent::admin::creation_code(server_code.trim())?;
    ensure_unconfigured(&app)?;
    let paths = &app.state::<AppState>().paths;
    // Ключ раньше состояния: сервис, который запустится по состоянию, должен
    // уже найти ключ, иначе группа не создастся.
    paths
        .save_admin_key(admin.server.server_id, &admin.admin_key)
        .map_err(err)?;
    let agent = AgentState {
        invite: Invite {
            server: admin.server,
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
    /// Подпись закреплённой записи (до 80 символов), `None` — без подписи.
    pub note: Option<String>,
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

fn png_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        data_encoding::BASE64.encode(png)
    )
}

/// Строка списка из карточки: текст обрезан до начала (`HEAD_CHARS`, хватает
/// на две строки превью), счётчики — по полному тексту, тела нет.
fn row_dto(r: &Row, files_dir: &std::path::Path, uploaded: UploadedFn<'_>) -> EntryDto {
    let c = &r.card;
    EntryDto {
        id: r.id.0.to_string(),
        kind: match c.kind {
            CardKind::Text | CardKind::Rich => "text",
            CardKind::Image => "image",
            CardKind::Files => "files",
            CardKind::Other => "other",
        },
        text: c.head.clone(),
        chars: c.chars as usize,
        lines: c.lines as usize,
        device_name: c.device_name.clone(),
        local: c.local,
        pinned: r.pinned,
        note: r.note(),
        seen_ms: r.seen_ms,
        width: c.width,
        height: c.height,
        bytes: c.bytes as usize,
        thumb: r.thumb.as_deref().map(png_url),
        image: None,
        rich: c.rich.map(|f| match f {
            RichFormat::Html => "HTML",
            RichFormat::Rtf => "RTF",
        }),
        html: None,
        files: c
            .files
            .iter()
            .map(|f| file_dto(c.local, f, files_dir, uploaded))
            .collect(),
        source_app: r.source_app().map(|a| SourceAppDto {
            icon: (!a.icon_png.is_empty()).then(|| png_url(&a.icon_png)),
            name: a.name,
        }),
    }
}

/// Запись целиком для предпросмотра: строка списка плюс полный текст, HTML и
/// картинка из тела.
fn full_dto(r: &Row, e: &Entry, files_dir: &std::path::Path, uploaded: UploadedFn<'_>) -> EntryDto {
    let mut d = row_dto(r, files_dir, uploaded);
    match &e.content {
        Content::Text(t) => d.text = t.clone(),
        Content::Rich { text, html, .. } => {
            d.text = text.clone();
            d.html = html.clone();
        }
        Content::Image { png, .. } => d.image = Some(png_url(png)),
        Content::Files(f) if f.len() == 1 && f[0].width > 0 => {
            d.image = local_file(e.local, &f[0], files_dir).and_then(|p| file_image_url(&f[0], &p));
        }
        _ => {}
    }
    d
}

pub fn parse_id(id: &str) -> CmdResult<ItemId> {
    id.parse()
        .map(ItemId)
        .map_err(|_| tr("Неверный id элемента").to_string())
}

/// Обходы ленты окна вставки: новый запрос останавливает устаревшие.
static LIST_SCANS: clipwire_agent::history::Supersede = clipwire_agent::history::Supersede::new();

#[tauri::command]
pub async fn history_list(
    app: AppHandle,
    query: String,
    pinned_only: bool,
    kind: Option<clipwire_agent::history::Kind>,
    offset: Option<usize>,
    limit: usize,
) -> Vec<EntryDto> {
    // Поиск редкого слова или вкладка с редким типом читают все карточки:
    // отдельным потоком и вторым соединением, чтобы не ждали ни окно, ни
    // запись новых копирований. Запрос с другим текстом или вкладкой
    // останавливает старые обходы: окно их ответы всё равно отбрасывает.
    let kind = kind.unwrap_or_default();
    let ticket = LIST_SCANS.start(&format!("{query}\0{pinned_only}\0{kind:?}"));
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let files_dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
        let uploaded = uploaded_check(&state);
        // Окно вставки берёт ленту страницами; одна страница не больше 1000.
        // Проверки файлов на диске — после чтения карточек.
        let rows = clipwire_agent::history::read(&state.history, state.reader.as_ref(), |h| {
            h.list_page_until(
                &query,
                pinned_only,
                kind,
                offset.unwrap_or(0),
                limit.min(1000),
                &|| LIST_SCANS.stale(ticket),
            )
        });
        // shortcut: брошенный обход отдаёт пустую страницу, а не ошибку:
        // окно отбрасывает ответы на старый запрос по своему номеру списка.
        // Если история понадобится второму окну со своим запросом, ключ
        // надо дополнить окном, иначе окна будут останавливать друг друга.
        let Some(rows) = rows else {
            return Vec::new();
        };
        rows.iter()
            .map(|r| row_dto(r, &files_dir, &uploaded))
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("список истории не прочитался: {e}");
        Vec::new()
    })
}

#[tauri::command]
pub fn history_get(state: State<'_, AppState>, id: String) -> CmdResult<EntryDto> {
    let id = parse_id(&id)?;
    let files_dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
    let uploaded = uploaded_check(&state);
    let found = {
        let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        h.row(id).zip(h.get(id))
    };
    found
        .map(|(r, e)| full_dto(&r, &e, &files_dir, &uploaded))
        .ok_or_else(|| tr("Элемента уже нет в истории").into())
}

/// Закрепы группы текущего сервиса; `None` в локальном режиме.
fn group_pins(state: &AppState) -> Option<std::sync::Arc<clipwire_agent::pins::Pins>> {
    state
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.pins())
}

/// Хэш, закреплена ли и подпись записи (`None` — записи нет).
fn pin_info(state: &AppState, id: ItemId) -> Option<([u8; 32], bool, String)> {
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    let row = h.row(id)?;
    Some((row.card.hash, row.pinned, row.note().unwrap_or_default()))
}

/// Закрепить или открепить. В группе правка уходит на все устройства (без
/// связи — после входа); открепление стирает подпись. Повторное
/// закрепление подпись группы не меняет.
///
/// Правка встаёт в очередь закрепов до записи в историю: закреп с сервера,
/// который пришёл в это время, сервис применяет под замком истории и только
/// без своей правки в очереди, поэтому свежее закрепление он не снимет.
#[tauri::command]
pub fn history_pin(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    pinned: bool,
) -> CmdResult<()> {
    let id = parse_id(&id)?;
    let (hash, _, note) =
        pin_info(&state, id).ok_or_else(|| tr("Элемента уже нет в истории").to_string())?;
    if let Some(pins) = group_pins(&state) {
        if pinned {
            pins.pin(hash, &note);
        } else {
            pins.unpin(hash);
        }
    }
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_pinned(id, pinned)
        .map_err(err)?;
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

/// Подпись закреплённой записи (пустая — убрать). Возвращает сохранённую
/// подпись: без переводов строк и пробелов по краям, до 80 символов.
#[tauri::command]
pub fn history_note(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    note: String,
) -> CmdResult<String> {
    let id = parse_id(&id)?;
    let (hash, pinned, _) =
        pin_info(&state, id).ok_or_else(|| tr("Элемента уже нет в истории").to_string())?;
    if !pinned {
        return Err(tr("Подписать можно только закреплённую запись").into());
    }
    // В очередь — до истории, как в `history_pin`.
    if let Some(pins) = group_pins(&state) {
        pins.set_note(hash, &clipwire_agent::history::clean_note(&note));
    }
    let saved = state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_note(id, &note)
        .map_err(err)?;
    let _ = app.emit("clipwire://history-changed", ());
    Ok(saved)
}

/// Как убрать закреплённую запись (диалог Del на закрепе).
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinnedDelete {
    /// Снять закреп только на этом устройстве: на остальных он остаётся, и
    /// следующая синхронизация не закрепит запись здесь снова.
    Here,
    /// Снять закреп на всех устройствах: запись остаётся обычной.
    Everywhere,
    /// Снять закреп везде и удалить запись на всех устройствах.
    Delete,
}

#[tauri::command]
pub fn history_delete_pinned(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    mode: PinnedDelete,
) -> CmdResult<()> {
    let id = parse_id(&id)?;
    let hash = pin_info(&state, id).map(|(h, _, _)| h);
    // Книга — до истории, как в `history_pin`.
    if let (Some(pins), Some(hash)) = (group_pins(&state), hash) {
        match mode {
            PinnedDelete::Here => pins.unpin_here(hash),
            PinnedDelete::Everywhere => pins.unpin(hash),
            // Записка об удалении уходит, когда сервер примет снятие:
            // закреплённое записка не удаляет.
            PinnedDelete::Delete => pins.delete(hash),
        }
    }
    {
        let mut h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        match mode {
            PinnedDelete::Here | PinnedDelete::Everywhere => h.set_pinned(id, false),
            PinnedDelete::Delete => h.delete(id),
        }
        .map_err(err)?;
    }
    if matches!(mode, PinnedDelete::Delete) {
        crate::cache::prune_later(&app);
    }
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

/// Удалить запись. Закреплённая в группе сначала снимается только здесь
/// (`PinnedDelete::Here`): иначе закреп группы вернул бы её при следующей
/// правке с другого устройства. Выбор «снять везде / удалить везде» —
/// `history_delete_pinned`.
#[tauri::command]
pub fn history_delete(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let id = parse_id(&id)?;
    let info = pin_info(&state, id);
    if let (Some(pins), Some((hash, true, _))) = (group_pins(&state), &info) {
        pins.unpin_here(*hash);
    }
    state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .delete(id)
        .map_err(err)?;
    let hash = info.map(|(h, _, _)| h);
    // Удаление уходит на остальные устройства группы, если это включено в
    // настройках (там закреплённое не трогается). Без группы — только здесь.
    let everywhere = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .delete_everywhere;
    if let Some(hash) = hash
        && everywhere
        && let Some(ctl) = state
            .service
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.controller())
    {
        ctl.delete_everywhere(vec![hash]);
    }
    crate::cache::prune_later(&app);
    let _ = app.emit("clipwire://history-changed", ());
    Ok(())
}

/// Очистить историю. Без `keep_pinned` закрепы группы снимаются только
/// здесь, как при удалении закреплённой записи: иначе они вернулись бы.
#[tauri::command]
pub fn history_clear(
    app: AppHandle,
    state: State<'_, AppState>,
    keep_pinned: bool,
) -> CmdResult<()> {
    if !keep_pinned && let Some(pins) = group_pins(&state) {
        let pinned = state
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pinned_notes();
        for (hash, _, _) in pinned {
            pins.unpin_here(hash);
        }
    }
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
    clipwire_agent::clipboard::set_image_as_file(settings.image_as_file);

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
    #[cfg(target_os = "linux")]
    if old.portal_paste != settings.portal_paste {
        let portal = app.state::<crate::popup::Portal>();
        if settings.portal_paste {
            if let Err(e) = portal.enable().await {
                tracing::warn!("портал удалённого управления: {e}");
                let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
                s.portal_paste = false;
                let _ = s.save(&state.paths.dir);
                let s = s.clone();
                let _ = app.emit("clipwire://settings-changed", &s);
                return Err(tr(
                    "Система не дала разрешение. Окно вставки будет только класть элемент в буфер.",
                )
                .into());
            }
        } else {
            portal.disable().await;
        }
    }
    if old.language != settings.language {
        clipwire_agent::i18n::set_language(&settings.language);
        crate::tray::retranslate(&app);
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
    app.state::<AppState>()
        .admin_stale
        .store(false, std::sync::atomic::Ordering::Relaxed);
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
    /// Программа-источник копирования известна (на Wayland — нет).
    pub knows_source_app: bool,
    /// Окно вставки само вставляет в прежнее окно (на Wayland — только в буфер).
    pub can_paste_into_active: bool,
    /// Сочетание окна вставки прописывается в системные (GNOME на Wayland).
    pub system_shortcut: bool,
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
        knows_source_app: clipwire_agent::clipboard::knows_source_app(),
        can_paste_into_active: crate::popup::can_paste_into_active(),
        #[cfg(target_os = "linux")]
        system_shortcut: crate::popup::gnome_shortcut::applies()
            || crate::popup::kde_shortcut::bound()
            || crate::popup::global_shortcuts::bound(),
        #[cfg(not(target_os = "linux"))]
        system_shortcut: false,
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
    let pins = group_pins(&state);
    // У правленой записи другой хэш: закреп переезжает на новый (старый
    // снимается везде, новый ставится с той же подписью).
    let moved = {
        let mut h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        let before = h
            .row(id)
            .filter(|r| r.pinned)
            .map(|r| (r.card.hash, r.note()));
        h.update_text(id, text).map_err(err)?;
        before.zip(h.hash_of(id))
    };
    if let (Some(pins), Some(((old, note), new))) = (pins, moved)
        && old != new
    {
        pins.unpin(old);
        pins.pin(new, &note.unwrap_or_default());
    }
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

/// Версия, которую нашла фоновая проверка (или скачанная), или `null`. Окно
/// настроек на старте по ней решает, проверять ли обновление заново.
#[tauri::command]
pub fn update_found(app: AppHandle) -> Option<String> {
    crate::updates::found_version(&app)
}
