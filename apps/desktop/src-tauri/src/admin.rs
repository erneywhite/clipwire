//! Администратор сервера и админ группы (`clipwire/9`): команды раздела
//! «Сервер» в настройках (с `clipwire/10` — и список групп сервера с
//! удалением, с `clipwire/11` — одноразовые коды создания группы, с
//! `clipwire/12` — имена групп), «Я администратор сервера» в «Синхронизации»
//! и роли в списке устройств. Запросы идут через движок (`clipwire_agent::admin`),
//! ключ администратора лежит в `admin.bin`.

use std::sync::atomic::Ordering;

use clipwire_agent::admin::{adopt_server_code, check_admin_code, claim_code, fail_text};
use clipwire_agent::i18n::{tr, trf};
use clipwire_core::admin::{
    GroupEntry, GroupInviteInfo, GroupState, MAX_INVITE_TTL_HOURS, SETTINGS, ServerSettings,
    ServerSummary, SettingUnit, check_setting, normalize_group_name,
};
use clipwire_core::{DeviceId, GroupId};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::AppState;
use crate::devices::{controller, parse_device};

type CmdResult<T> = Result<T, String>;

/// Команды этого файла меняют сервер и роли в группе: их вызывает только
/// окно настроек. Окно вставки и редактор показывают чужое содержимое
/// (HTML из буфера, превью ссылок), и, если в них когда-нибудь найдётся
/// способ выполнить скрипт, он не должен дотянуться до ключа
/// администратора.
fn only_settings(window: &WebviewWindow) -> CmdResult<()> {
    if window.label() == "settings" {
        Ok(())
    } else {
        tracing::warn!(
            window = window.label(),
            "команда администратора не из окна настроек"
        );
        Err(tr("Это доступно только в окне настроек").into())
    }
}

/// Сервис сообщил, что ключ администратора устарел и забыт: отметить и
/// сообщить окну настроек.
pub fn on_admin_key_stale(app: &AppHandle) {
    app.state::<AppState>()
        .admin_stale
        .store(true, Ordering::Relaxed);
    let _ = app.emit("clipwire://admin-changed", ());
}

/// Есть ли у устройства ключ администратора сервера, к которому оно
/// подключено. Без группы — нет. `stale` — ключ был, но сервер его не
/// принял (сменили): нужен новый код сервера администратора.
#[derive(Debug, Clone, Serialize)]
pub struct AdminStateDto {
    pub has_key: bool,
    pub stale: bool,
}

#[tauri::command]
pub fn admin_state(window: WebviewWindow, app: AppHandle) -> CmdResult<AdminStateDto> {
    only_settings(&window)?;
    let state = app.state::<AppState>();
    let has_key = state
        .paths
        .load()
        .ok()
        .flatten()
        .and_then(|s| state.paths.load_admin_key(s.invite.server.server_id))
        .is_some();
    Ok(AdminStateDto {
        has_key,
        stale: !has_key && state.admin_stale.load(Ordering::Relaxed),
    })
}

/// «Я администратор сервера»: проверить код `cwserver2-…` на сервере (тот
/// же сервер, ключ подходит) и сохранить ключ.
#[tauri::command]
pub async fn admin_claim(window: WebviewWindow, app: AppHandle, code: String) -> CmdResult<()> {
    only_settings(&window)?;
    let state = app.state::<AppState>();
    let agent = state
        .paths
        .load()
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| tr("Устройство не в группе").to_string())?;
    let info = claim_code(code.trim(), &agent)?;
    // Токен доступа в коде другой: его сменили, и со старым это устройство
    // к серверу уже не подключится, проверить ключ через движок не на чем.
    // Проверяем отдельным соединением по новому коду (relay и токен из него,
    // `AdminAuth` с ключом из него) и только при успехе сохраняем: иначе
    // опечатка или чужой код заменили бы рабочий сервер в состоянии.
    if info.server != agent.invite.server {
        check_admin_code(&info).await?;
        adopt_server_code(&state.paths, code.trim())
            .map_err(|e| trf("Ключ не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
        state.admin_stale.store(false, Ordering::Relaxed);
        crate::restart_service(&app)
            .await
            .map_err(|e| format!("{e:#}"))?;
        return Ok(());
    }
    // Сначала сервер: неверный или сменённый ключ сохранять незачем.
    controller(&app)?.admin_auth(info.admin_key.clone()).await?;
    state
        .paths
        .save_admin_key(info.server.server_id, &info.admin_key)
        .map_err(|e| trf("Ключ не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
    state.admin_stale.store(false, Ordering::Relaxed);
    Ok(())
}

/// Новый код приглашения в ту же группу, без выхода из неё: после смены
/// токена relay ключ группы прежний, меняется только сервер. Код в другую
/// группу не принимается. Синхронизация перезапускается с новым кодом.
#[tauri::command]
pub async fn sync_update_invite(
    window: WebviewWindow,
    app: AppHandle,
    code: String,
) -> CmdResult<()> {
    only_settings(&window)?;
    clipwire_agent::admin::adopt_invite(&app.state::<AppState>().paths, &code)?;
    crate::restart_service(&app)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Забыть ключ администратора на этом устройстве. Движок перезапускается,
/// чтобы соединение потеряло права администратора.
#[tauri::command]
pub async fn admin_forget(window: WebviewWindow, app: AppHandle) -> CmdResult<()> {
    only_settings(&window)?;
    app.state::<AppState>()
        .paths
        .forget_admin_key()
        .map_err(|e| format!("{e:#}"))?;
    crate::restart_service(&app)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Настройка сервера для формы: описание из `SETTINGS` и значения сервера.
#[derive(Debug, Clone, Serialize)]
pub struct SettingDto {
    pub key: &'static str,
    /// `count`, `bytes` или `hours`.
    pub unit: &'static str,
    pub min: u64,
    pub max: u64,
    /// Действующее значение.
    pub value: u64,
    /// Значение по умолчанию: из переменной окружения или встроенное.
    pub default: u64,
    /// Задано из клиента (лежит в базе сервера): его можно сбросить.
    pub stored: bool,
    /// Переменная окружения, из которой берётся значение по умолчанию.
    pub env: &'static str,
}

fn unit_name(u: SettingUnit) -> &'static str {
    match u {
        SettingUnit::Count => "count",
        SettingUnit::Bytes => "bytes",
        SettingUnit::Hours => "hours",
    }
}

/// Настройки в порядке `SETTINGS`. Настройки, которых сервер не прислал
/// (сервер старее клиента), не показываются.
fn settings_dto(s: &ServerSettings) -> Vec<SettingDto> {
    let get =
        |list: &[(String, u64)], key: &str| list.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
    SETTINGS
        .iter()
        .filter_map(|spec| {
            let value = get(&s.values, spec.key)?;
            Some(SettingDto {
                key: spec.key,
                unit: unit_name(spec.unit),
                min: spec.min,
                max: spec.max,
                value,
                default: get(&s.defaults, spec.key).unwrap_or(value),
                stored: s.stored.iter().any(|k| k == spec.key),
                env: spec.env,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn server_settings_get(
    window: WebviewWindow,
    app: AppHandle,
) -> CmdResult<Vec<SettingDto>> {
    only_settings(&window)?;
    let s = controller(&app)?.server_settings().await?;
    Ok(settings_dto(&s))
}

/// Изменение настройки: `value: null` — сбросить к значению по умолчанию.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingChange {
    pub key: String,
    pub value: Option<u64>,
}

/// Проверить изменения до отправки: те же пределы, что на сервере.
fn check_changes(changes: &[SettingChange]) -> CmdResult<()> {
    for c in changes {
        if let Some(v) = c.value {
            check_setting(&c.key, v).map_err(|f| fail_text(&f))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn server_settings_set(
    window: WebviewWindow,
    app: AppHandle,
    changes: Vec<SettingChange>,
) -> CmdResult<Vec<SettingDto>> {
    if changes.is_empty() {
        return server_settings_get(window, app).await;
    }
    only_settings(&window)?;
    check_changes(&changes)?;
    let values = changes.into_iter().map(|c| (c.key, c.value)).collect();
    let s = controller(&app)?.set_server_settings(values).await?;
    Ok(settings_dto(&s))
}

#[tauri::command]
pub async fn server_summary(window: WebviewWindow, app: AppHandle) -> CmdResult<ServerSummary> {
    only_settings(&window)?;
    controller(&app)?.server_summary().await
}

/// Итог смены токена relay.
#[derive(Debug, Clone, Serialize)]
pub struct RelayRotatedDto {
    /// Новый код сервера администратора.
    pub server_code: String,
    /// Новый код приглашения в эту группу.
    pub invite: String,
}

/// Сменить токен relay. Все устройства отключаются; это устройство
/// сохраняет новый код сервера сразу, до обрыва соединения, и
/// переподключается с ним.
#[tauri::command]
pub async fn server_rotate_relay_token(
    window: WebviewWindow,
    app: AppHandle,
) -> CmdResult<RelayRotatedDto> {
    only_settings(&window)?;
    let server_code = controller(&app)?.rotate_relay_token().await?;
    let paths = &app.state::<AppState>().paths;
    let state = adopt_server_code(paths, &server_code).map_err(|e| {
        // Код всё равно показать: без него к серверу больше не подключиться.
        tracing::error!("новый код сервера не сохранился: {e:#}");
        trf(
            "Токен сменён, но новый код сервера не сохранился: {e}. Запишите код: {code}",
            &[("e", &format!("{e:#}")), ("code", &server_code)],
        )
    })?;
    if let Err(e) = crate::restart_service(&app).await {
        tracing::error!("синхронизация не перезапустилась после смены токена: {e:#}");
    }
    Ok(RelayRotatedDto {
        server_code,
        invite: state.invite.encode(),
    })
}

/// Сменить ключ администратора. Возвращает новый код сервера; ключ здесь
/// сохраняется сразу, движок уже входит с ним.
#[tauri::command]
pub async fn server_rotate_admin_key(window: WebviewWindow, app: AppHandle) -> CmdResult<String> {
    only_settings(&window)?;
    let server_code = controller(&app)?.rotate_admin_key().await?;
    let paths = &app.state::<AppState>().paths;
    adopt_server_code(paths, &server_code).map_err(|e| {
        tracing::error!("новый ключ администратора не сохранился: {e:#}");
        trf(
            "Ключ сменён, но не сохранился на этом устройстве: {e}. Запишите код: {code}",
            &[("e", &format!("{e:#}")), ("code", &server_code)],
        )
    })?;
    Ok(server_code)
}

/// Админы группы для списка устройств.
#[derive(Debug, Clone, Serialize)]
pub struct GroupAdminDto {
    /// Сервер знает админов групп (`clipwire/9`).
    pub supported: bool,
    /// Сервер знает нескольких админов (`clipwire/13`): назначать и снимать
    /// админов. Нет — админ один, роль только передаётся.
    pub multi: bool,
    /// Id админов (64 hex), самый ранний первым; пусто — у группы админа нет.
    pub admins: Vec<String>,
}

fn admins_dto(supported: bool, multi: bool, admins: &[DeviceId]) -> GroupAdminDto {
    GroupAdminDto {
        supported,
        multi,
        admins: admins
            .iter()
            .map(|d| data_encoding::HEXLOWER.encode(&d.0))
            .collect(),
    }
}

#[tauri::command]
pub async fn group_admin(window: WebviewWindow, app: AppHandle) -> CmdResult<GroupAdminDto> {
    only_settings(&window)?;
    let ctl = controller(&app)?;
    let protocol = ctl.link().await?.server_protocol;
    // Админы групп появились в `clipwire/9`, несколько админов — в
    // `clipwire/13`. Старому серверу новый запрос не шлём: он оборвал бы
    // соединение.
    if protocol < 9 {
        return Ok(admins_dto(false, false, &[]));
    }
    if protocol >= 13 {
        return Ok(admins_dto(true, true, &ctl.group_admins().await?));
    }
    let admin = ctl.group_admin().await?;
    Ok(admins_dto(true, false, &Vec::from_iter(admin)))
}

/// Сделать устройство группы ещё одним админом (`clipwire/13`).
#[tauri::command]
pub async fn device_grant_admin(
    window: WebviewWindow,
    app: AppHandle,
    device_id: String,
) -> CmdResult<GroupAdminDto> {
    only_settings(&window)?;
    let id = parse_device(&device_id)?;
    let admins = controller(&app)?.grant_group_admin(id).await?;
    Ok(admins_dto(true, true, &admins))
}

/// Снять роль админа с устройства группы, в том числе с этого
/// (`clipwire/13`). Последнего админа сервер снять не даст.
#[tauri::command]
pub async fn device_revoke_admin(
    window: WebviewWindow,
    app: AppHandle,
    device_id: String,
) -> CmdResult<GroupAdminDto> {
    only_settings(&window)?;
    let id = parse_device(&device_id)?;
    let admins = controller(&app)?.revoke_group_admin(id).await?;
    Ok(admins_dto(true, true, &admins))
}

/// Передать роль админа группы другому устройству (сервер старее
/// `clipwire/13`, где админ один).
#[tauri::command]
pub async fn device_make_admin(
    window: WebviewWindow,
    app: AppHandle,
    device_id: String,
) -> CmdResult<()> {
    only_settings(&window)?;
    let id = parse_device(&device_id)?;
    controller(&app)?.transfer_group_admin(id).await
}

/// Стать админом группы самому: устройство администратора сервера чинит
/// группу, чей админ потерян. Сервер проверяет права администратора.
#[tauri::command]
pub async fn group_admin_claim(window: WebviewWindow, app: AppHandle) -> CmdResult<()> {
    only_settings(&window)?;
    controller(&app)?.claim_group_admin().await
}

/// Группа сервера для карточки «Группы».
#[derive(Debug, Clone, Serialize)]
pub struct ServerGroupDto {
    /// Id группы (64 hex): им карточка просит удалить.
    pub id: String,
    /// Короткий отпечаток id (10 hex), как у устройств.
    pub fingerprint: String,
    /// Имя группы — заметка администратора сервера (`clipwire/12`); пусто —
    /// имени нет, заголовком идёт отпечаток.
    pub name: String,
    /// Группа этого устройства.
    pub mine: bool,
    pub created_ms: i64,
    /// Последний вход любого устройства, 0 — устройств нет.
    pub last_seen_ms: i64,
    pub devices: u64,
    pub online: u64,
    pub items: u64,
    pub item_bytes: u64,
    pub files: u64,
    pub file_bytes: u64,
    pub pins: u64,
    pub pin_bytes: u64,
    pub has_admin: bool,
    /// `live`, `pending`, `rotated` или `idle`.
    pub state: &'static str,
    /// У `rotated`: сколько дней ещё ждут пакеты нового ключа.
    pub days_left: Option<u32>,
}

/// Короткий отпечаток id группы (10 hex): в списке групп и у самого клиента.
pub fn group_fingerprint(group: &GroupId) -> String {
    data_encoding::HEXLOWER.encode(&group.0[..5])
}

fn group_dto(e: &GroupEntry, mine: Option<GroupId>) -> ServerGroupDto {
    let g = &e.info;
    let (state, days_left) = match g.state {
        GroupState::Live => ("live", None),
        GroupState::Pending => ("pending", None),
        GroupState::Rotated { days_left } => ("rotated", Some(days_left)),
        GroupState::Idle => ("idle", None),
    };
    ServerGroupDto {
        id: data_encoding::HEXLOWER.encode(&g.group.0),
        fingerprint: group_fingerprint(&g.group),
        name: e.name.clone(),
        mine: mine == Some(g.group),
        created_ms: g.created_ms,
        last_seen_ms: g.last_seen_ms,
        devices: g.devices,
        online: g.online,
        items: g.items,
        item_bytes: g.item_bytes,
        files: g.files,
        file_bytes: g.file_bytes,
        pins: g.pins,
        pin_bytes: g.pin_bytes,
        has_admin: g.has_admin,
        state,
        days_left,
    }
}

fn groups_dto(list: &[GroupEntry], app: &AppHandle) -> Vec<ServerGroupDto> {
    let mine = app
        .state::<AppState>()
        .paths
        .load()
        .ok()
        .flatten()
        .map(|s| s.invite.group_key.group_id());
    list.iter().map(|g| group_dto(g, mine)).collect()
}

/// Ответ `server_groups`.
#[derive(Debug, Clone, Serialize)]
pub struct ServerGroupsDto {
    /// Сервер знает группы сервера (`clipwire/10`). Нет — список пустой, в
    /// карточке пояснение «обновите сервер».
    pub supported: bool,
    /// Сервер хранит имена групп (`clipwire/12`): можно переименовывать.
    pub names: bool,
    pub groups: Vec<ServerGroupDto>,
}

#[tauri::command]
pub async fn server_groups(window: WebviewWindow, app: AppHandle) -> CmdResult<ServerGroupsDto> {
    only_settings(&window)?;
    let ctl = controller(&app)?;
    // Группы сервера появились в `clipwire/10`; старому серверу запрос не
    // шлём совсем (он оборвал бы соединение).
    let protocol = ctl.link().await?.server_protocol;
    if protocol < 10 {
        return Ok(ServerGroupsDto {
            supported: false,
            names: false,
            groups: Vec::new(),
        });
    }
    let list = ctl.server_groups().await?;
    Ok(ServerGroupsDto {
        supported: true,
        names: protocol >= 12,
        groups: groups_dto(&list, &app),
    })
}

/// Id группы из 64 hex.
fn parse_group(group_id: &str) -> CmdResult<GroupId> {
    data_encoding::HEXLOWER
        .decode(group_id.as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .map(GroupId)
        .ok_or_else(|| tr("Неверный id группы").to_string())
}

/// Удалить группу сервера целиком. Возвращает список после удаления.
#[tauri::command]
pub async fn server_group_delete(
    window: WebviewWindow,
    app: AppHandle,
    group_id: String,
) -> CmdResult<Vec<ServerGroupDto>> {
    only_settings(&window)?;
    let group = parse_group(&group_id)?;
    let list = controller(&app)?.delete_server_group(group).await?;
    Ok(groups_dto(&list, &app))
}

/// Задать имя группы сервера (пустое — снять). Имя проверяется здесь теми
/// же правилами, что на сервере, чтобы отказ пришёл сразу. Возвращает список
/// после правки.
#[tauri::command]
pub async fn set_server_group_name(
    window: WebviewWindow,
    app: AppHandle,
    group_id: String,
    name: String,
) -> CmdResult<Vec<ServerGroupDto>> {
    only_settings(&window)?;
    let group = parse_group(&group_id)?;
    let name = normalize_group_name(&name).map_err(|f| fail_text(&f))?;
    let list = controller(&app)?.set_server_group_name(group, name).await?;
    Ok(groups_dto(&list, &app))
}

/// Код создания группы для карточки «Приглашения на создание группы». Секрета
/// здесь нет: сервер хранит только хэш, код показывается один раз при выпуске.
#[derive(Debug, Clone, Serialize)]
pub struct GroupInviteDto {
    pub id: u64,
    pub label: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub used_ms: Option<i64>,
    /// Отпечаток созданной группы (10 hex, как в «Группах»).
    pub used_group: Option<String>,
    pub revoked_ms: Option<i64>,
}

fn invite_dto(i: &GroupInviteInfo) -> GroupInviteDto {
    GroupInviteDto {
        id: i.id,
        label: i.label.clone(),
        created_ms: i.created_ms,
        expires_ms: i.expires_ms,
        used_ms: i.used_ms,
        used_group: i
            .used_group
            .map(|g| data_encoding::HEXLOWER.encode(&g.0[..5])),
        revoked_ms: i.revoked_ms,
    }
}

/// Ответ `server_invites`.
#[derive(Debug, Clone, Serialize)]
pub struct GroupInvitesDto {
    /// Сервер знает коды создания группы (`clipwire/11`). Нет — список
    /// пустой, в карточке пояснение «обновите сервер».
    pub supported: bool,
    pub invites: Vec<GroupInviteDto>,
}

#[tauri::command]
pub async fn server_invites(window: WebviewWindow, app: AppHandle) -> CmdResult<GroupInvitesDto> {
    only_settings(&window)?;
    let ctl = controller(&app)?;
    // Старому серверу запрос не шлём: он оборвал бы соединение.
    if ctl.link().await?.server_protocol < 11 {
        return Ok(GroupInvitesDto {
            supported: false,
            invites: Vec::new(),
        });
    }
    let list = ctl.group_invites().await?;
    Ok(GroupInvitesDto {
        supported: true,
        invites: list.iter().map(invite_dto).collect(),
    })
}

/// Выпущенный код: весь `cwgroup1-…` (больше его не узнать) и список после
/// выпуска. В отладочном выводе кода нет: в нём секрет.
#[derive(Clone, Serialize)]
pub struct InviteCreatedDto {
    pub code: String,
    pub id: u64,
    pub invites: Vec<GroupInviteDto>,
}

impl std::fmt::Debug for InviteCreatedDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteCreatedDto")
            .field("code", &"..")
            .field("id", &self.id)
            .field("invites", &self.invites)
            .finish()
    }
}

#[tauri::command]
pub async fn server_invite_create(
    window: WebviewWindow,
    app: AppHandle,
    label: String,
    ttl_hours: u32,
) -> CmdResult<InviteCreatedDto> {
    only_settings(&window)?;
    if ttl_hours == 0 || ttl_hours > MAX_INVITE_TTL_HOURS {
        return Err(trf(
            "Срок действия — от 1 до {max} часов",
            &[("max", &MAX_INVITE_TTL_HOURS)],
        ));
    }
    let ctl = controller(&app)?;
    let (code, id) = ctl.create_group_invite(label, ttl_hours).await?;
    // Список — отдельным запросом; если он не придёт, код всё равно показать.
    let invites = ctl
        .group_invites()
        .await
        .map(|l| l.iter().map(invite_dto).collect())
        .unwrap_or_default();
    Ok(InviteCreatedDto { code, id, invites })
}

/// Отозвать неиспользованный код. Возвращает список после отзыва.
#[tauri::command]
pub async fn server_invite_revoke(
    window: WebviewWindow,
    app: AppHandle,
    id: u64,
) -> CmdResult<Vec<GroupInviteDto>> {
    only_settings(&window)?;
    let list = controller(&app)?.revoke_group_invite(id).await?;
    Ok(list.iter().map(invite_dto).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_created_debug_hides_code() {
        let dto = InviteCreatedDto {
            code: "cwgroup1-секрет".into(),
            id: 7,
            invites: Vec::new(),
        };
        let s = format!("{dto:?}");
        assert!(!s.contains("секрет") && s.contains('7'), "{s}");
    }

    #[test]
    fn group_dto_marks_mine_and_state() {
        use clipwire_core::admin::GroupInfo;
        let g = GroupInfo {
            group: GroupId([0xab; 32]),
            created_ms: 1,
            last_seen_ms: 2,
            devices: 3,
            online: 1,
            items: 4,
            item_bytes: 5,
            files: 6,
            file_bytes: 7,
            pins: 8,
            pin_bytes: 9,
            has_admin: true,
            state: GroupState::Rotated { days_left: 12 },
        };
        let named = GroupEntry {
            info: g.clone(),
            name: "Семья".into(),
        };
        let d = group_dto(&named, Some(GroupId([0xab; 32])));
        assert_eq!(d.name, "Семья");
        assert!(d.mine);
        assert_eq!(d.fingerprint, "ababababab");
        assert_eq!(d.id.len(), 64);
        assert_eq!((d.state, d.days_left), ("rotated", Some(12)));
        let other = group_dto(
            &GroupEntry {
                info: GroupInfo {
                    state: GroupState::Idle,
                    ..g
                },
                name: String::new(),
            },
            None,
        );
        assert!(!other.mine);
        assert_eq!(other.name, "");
        assert_eq!((other.state, other.days_left), ("idle", None));
    }

    #[test]
    fn group_id_is_parsed_strictly() {
        let id = "ab".repeat(32);
        assert_eq!(parse_group(&id).unwrap(), GroupId([0xab; 32]));
        assert!(parse_group("ab").is_err());
        assert!(parse_group(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn invite_dto_has_fingerprint_and_no_secret() {
        let i = GroupInviteInfo {
            id: 3,
            label: "Маше".into(),
            created_ms: 1,
            expires_ms: 2,
            used_ms: Some(5),
            used_group: Some(GroupId([0xcd; 32])),
            revoked_ms: None,
        };
        let d = invite_dto(&i);
        assert_eq!(d.used_group.as_deref(), Some("cdcdcdcdcd"));
        assert_eq!((d.id, d.used_ms, d.revoked_ms), (3, Some(5), None));
        let none = invite_dto(&GroupInviteInfo {
            used_group: None,
            ..i
        });
        assert_eq!(none.used_group, None);
    }

    #[test]
    fn settings_follow_spec_and_mark_stored() {
        let s = ServerSettings {
            values: vec![("max_devices".into(), 7), ("ttl_hours".into(), 48)],
            defaults: vec![("max_devices".into(), 20), ("ttl_hours".into(), 24)],
            stored: vec!["max_devices".into()],
        };
        let dto = settings_dto(&s);
        // Порядок — как в SETTINGS; чего сервер не прислал, того нет.
        assert_eq!(
            dto.iter().map(|d| d.key).collect::<Vec<_>>(),
            ["ttl_hours", "max_devices"]
        );
        let dev = &dto[1];
        assert_eq!((dev.value, dev.default, dev.stored), (7, 20, true));
        assert_eq!((dev.unit, dev.min, dev.max), ("count", 1, 1000));
        assert_eq!(dto[0].unit, "hours");
        assert!(!dto[0].stored);
    }

    #[test]
    fn changes_are_checked_before_sending() {
        let ok = [
            SettingChange {
                key: "max_devices".into(),
                value: Some(5),
            },
            SettingChange {
                key: "ttl_hours".into(),
                value: None,
            },
        ];
        assert!(check_changes(&ok).is_ok());
        let bad = [SettingChange {
            key: "max_devices".into(),
            value: Some(0),
        }];
        assert!(check_changes(&bad).unwrap_err().contains("1000"));
    }
}
