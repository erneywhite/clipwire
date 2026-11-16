// Типизированные обёртки над командами Rust.
import { invoke } from '@tauri-apps/api/core'

export type Theme = 'graphite' | 'paper' | 'signal'
export type Mode = 'system' | 'light' | 'dark'
/** Фильтр окна вставки по типу записи. */
export type KindFilter = 'all' | 'text' | 'link' | 'image' | 'files'

export type Retention = 'day' | 'week' | 'month' | 'year' | 'forever'

export interface Settings {
  device_name: string
  theme: Theme
  mode: Mode
  retention: Retention
  paste_into_active: boolean
  always_plain_text: boolean
  popup_width: number
  popup_height: number
  popup_hotkey: string
  sound_on_receive: boolean
  link_previews: boolean
  hide_from_capture: boolean
  ignored_apps: string[]
  /** Файлы до этого размера уходят на сервер; 0 — не загружать. */
  file_upload_limit_mb: number
  /** Скачивать обновления в фоне и ставить, когда программа не нужна. */
  auto_update: boolean
  /** Удаление записи уходит на остальные устройства группы. */
  delete_everywhere: boolean
  /** Язык интерфейса: как в системе, русский или английский. */
  language: 'system' | 'ru' | 'en'
  /** Wayland: вставлять через портал удалённого управления. */
  portal_paste: boolean
  /** Windows: картинка из буфера вставляется в Проводник PNG-файлом. */
  image_as_file: boolean
}

export interface DeviceInfo {
  version: string
  relay_url: string | null
  device_id: string | null
  /** Программа-источник копирования известна (на Wayland — нет). */
  knows_source_app: boolean
  /** Окно вставки само вставляет в прежнее окно (на Wayland — только в буфер). */
  can_paste_into_active: boolean
  /** Сочетание окна вставки прописывается в системные (GNOME на Wayland). */
  system_shortcut: boolean
}

/** Устройство группы (`devices_list`). */
export interface GroupDevice {
  /** Полный id, 64 hex. */
  id: string
  /** Короткий отпечаток. */
  fingerprint: string
  /** Имя и ОС из карточки; null — неизвестное устройство. */
  name: string | null
  os: string | null
  /** Карточка открылась: при смене ключа устройство получит новый. */
  known: boolean
  me: boolean
  online: boolean
  /** Последний вход в группу, мс; 0 — неизвестно. */
  last_seen_ms: number
  /** Когда это устройство впервые увидело карточку (по своим записям), мс; 0 — давно. */
  first_seen_ms: number
}

/** Смена ключа другим устройством, которая ждёт ответа (`group_rekey_offer`). */
export interface RekeyOffer {
  /** Имя, ОС и отпечаток устройства, сменившего ключ. */
  name: string
  os: string
  fingerprint: string
  /** Когда новый ключ пришёл на это устройство, мс. */
  received_ms: number
}

/** Что лежит у группы на сервере. */
export interface GroupStats {
  items: number
  item_bytes: number
  /** Файлы, загруженные целиком. */
  files: number
  file_bytes: number
  /** Занято в счёт квоты группы (зашифрованный размер всех файлов). */
  quota_used: number
  quota: number
}

/** Закрепы группы на сервере (`stats_overview`). */
export interface PinStats {
  /** Закреплённых записей (снятые не считаются). */
  pins: number
  /** Сколько места занимают все записи закрепов, включая снятые, байт. */
  bytes: number
  /** Пределы сервера: CLIPWIRE_PIN_MAX и CLIPWIRE_PIN_BYTES. */
  max_pins: number
  max_bytes: number
}

/** Как убрать закреплённую запись (`history_delete_pinned`). */
export type PinnedDelete =
  /** Снять закреп только здесь: на других устройствах он остаётся. */
  | 'here'
  /** Снять закреп везде: запись остаётся в истории как обычная. */
  | 'everywhere'
  /** Снять закреп везде и удалить запись на всех устройствах. */
  | 'delete'

/** Связь с сервером и его версия (`stats_overview`). */
export interface Link {
  direct: boolean
  rtt_ms: number
  /** null — сервер не сообщает версию (старше 0.4.0). */
  server_version: string | null
  /** Сервер старше приложения: образ пора обновить. */
  server_outdated: boolean
  client_version: string
}

/** Счётчики раздела «О программе» (`stats_overview`). */
export interface StatsOverview {
  history_total: number
  history_pinned: number
  /** null — устройство не в группе или сервер не ответил (см. server_error). */
  server: GroupStats | null
  server_error: string | null
  /** Закрепы группы на сервере; null — не в группе или нет ответа (см. pins_error). */
  pins: PinStats | null
  pins_error: string | null
  /** Путь до сервера, null — нет связи. */
  link: Link | null
  /** Мс Unix, 0 — с запуска не было. */
  last_sent_ms: number
  last_received_ms: number
}

/** Итог смены ключа группы. */
export interface RekeyResult {
  /** Новый код приглашения. */
  invite: string
  /** Отпечатки устройств, которым новый ключ не достался. */
  skipped: string[]
}

export interface Entry {
  id: string
  kind: 'text' | 'image' | 'files' | 'other'
  text: string
  chars: number
  lines: number
  device_name: string
  local: boolean
  pinned: boolean
  /** Подпись закреплённой записи (до 80 символов); null — без подписи. */
  note: string | null
  seen_ms: number
  /** Картинка: размеры и вес PNG. */
  width: number
  height: number
  bytes: number
  /** Миниатюра картинки (data URL) — в списке. */
  thumb: string | null
  /** Полная картинка (data URL) — только из history_get. */
  image: string | null
  /** Текст с форматированием: «HTML» или «RTF». */
  rich: 'HTML' | 'RTF' | null
  /** HTML-фрагмент — только из history_get. */
  html: string | null
  /** Скопированные файлы и папки. */
  files: FileInfo[]
  /** Приложение, из которого скопировано; null — неизвестно. */
  source_app: SourceApp | null
}

export interface SourceApp {
  name: string
  /** Иконка 32×32 — data URL PNG. */
  icon: string | null
}

export interface FileInfo {
  name: string
  path: string
  size: number
  is_dir: boolean
  /** Файл по local_path есть на диске. */
  exists: boolean
  /**
   * Id переданного файла (32 hex); null — не передавался, есть только на
   * источнике. Есть и у папок (архив), и у файлов больше порога (p2p).
   */
  blob: string | null
  /** Передаётся напрямую с устройства-источника, а не через сервер. */
  p2p: boolean
  /** Сколько байт передаётся: у файла — его размер, у папки — размер архива. */
  blob_size: number
  /** Путь на этом компьютере: исходный у локальной записи, путь в кэше у скачанной. */
  local_path: string | null
  /** Запись пришла с другого устройства. */
  remote: boolean
  /** Файл этого устройства уже на сервере (загружен за последние сутки, в том числе до перезапуска). */
  uploaded: boolean
}

export interface CacheCleared {
  removed: number
  /** Не удалились: файл открыт в другой программе. */
  failed: number
  /** Сколько кэш занимает после чистки, байт. */
  size: number
}

/** Передача одного файла: событие clipwire://transfer и ответ transfers(). */
export interface Transfer {
  blob: string
  direction: 'up' | 'down'
  done: number
  total: number
  state: 'active' | 'done' | 'failed'
  error: string | null
}

export interface FilePreview {
  kind: 'text' | 'pdf' | 'video' | 'audio' | 'none'
  text: string | null
  truncated: boolean
}

export interface LinkCard {
  status: 'ok' | 'off' | 'one_time' | 'failed'
  url: string
  /** Домен ссылки из записи, не og:site_name. */
  site: string
  /** Конечный домен после редиректов, если он другой. */
  final_site: string | null
  title: string | null
  description: string | null
  /** data URL — окно само в сеть не ходит. */
  image: string | null
  error: string | null
}

export type Status =
  | { kind: 'unconfigured' }
  | { kind: 'local' }
  | { kind: 'connecting' }
  | { kind: 'online' }
  | { kind: 'offline'; detail: string }
  | { kind: 'paused' }
  /**
   * Сервер сообщил, что устройство исключено: синхронизация остановлена, ключ на месте.
   * `deleted` — группу удалил администратор сервера.
   */
  | { kind: 'excluded'; detail: { deleted: boolean } }
  /** Ключ группы сменило другое устройство: синхронизация стоит, пока человек не ответит. */
  | { kind: 'awaiting' }

/** События группы, которые окно настроек забирает у Rust (`group_notices_take`). */
export interface GroupNotices {
  rekeyed: GroupRekeyed | null
  /** Новый ключ не дошёл: текст ошибки. */
  rekey_failed: string | null
  /** Сервер исключил устройство. */
  excluded: boolean
  /** Другое устройство сменило ключ, нужен ответ (`group_rekey_offer`). */
  offered: boolean
}

/** Для списка устройств: итог смены ключа, пришедший событием. */
export interface DevicesIncoming {
  result: RekeyResult | null
  error: string | null
}

/** Событие `clipwire://group-rekeyed`. */
export interface GroupRekeyed {
  /** Ключ сменили на этом устройстве. */
  by_me: boolean
  /** Имя устройства, которое сменило ключ (если не это). */
  by: string
  /** Новый код и кому ключ не достался — если сменили здесь. */
  result: RekeyResult | null
}

/** Вид хранилища файлов группы. */
export type StorageKind = 'server' | 's3' | 'sftp'

export interface S3Form {
  endpoint: string
  region: string
  bucket: string
  prefix: string
  access_key: string
  /** Пусто — оставить сохранённый. */
  secret_key: string
  path_style: boolean
}

export interface SftpForm {
  host: string
  port: number
  user: string
  auth: 'password' | 'key'
  /** Секреты: пусто — оставить сохранённые. */
  password: string
  private_key: string
  passphrase: string
  path: string
  /** Подтверждённый отпечаток ключа хоста, пусто — не подтверждён. */
  host_key_sha256: string
}

/** Форма «Хранилище файлов». Обе вложенные формы есть всегда. */
export interface StorageForm {
  kind: StorageKind
  /** Сколько хранить файлы во внешнем хранилище, часов; 0 — без чистки. */
  file_ttl_hours: number
  s3: S3Form
  sftp: SftpForm
}

/** Какие секреты сохранены (сами значения в окно не приходят). */
export interface SavedSecrets {
  s3_secret: boolean
  sftp_password: boolean
  sftp_key: boolean
  sftp_passphrase: boolean
}

/** Предупреждение «внешнее хранилище недоступно, файл ушёл через сервер». */
export interface StorageWarning {
  text: string
  at_ms: number
}

export interface StorageState {
  form: StorageForm
  saved: SavedSecrets
  /** 0 — настройки ещё никто не сохранял. */
  version: number
  /** Хранилище по сохранённым настройкам не открылось. */
  error: string | null
  insecure_http: boolean
  warning: StorageWarning | null
  /** Доступы к хранилищу остались у устройства вне группы (исключённого или без карточки): их надо сменить и сохранить заново. */
  access_exposed: boolean
}

/** Итог «Проверить подключение». */
export type ProbeResult =
  | { status: 'ok'; host_key: string | null; insecure_http: boolean }
  | { status: 'confirm_host_key'; host_key: string }
  | { status: 'host_key_mismatch'; expected: string; got: string }
  | { status: 'error'; message: string }

/** Настройка сервера для раздела «Сервер» (`server_settings_get`). */
export interface ServerSetting {
  key: string
  unit: 'count' | 'bytes' | 'hours'
  min: number
  max: number
  /** Действующее значение. */
  value: number
  /** Значение по умолчанию: из переменной окружения или встроенное. */
  default: number
  /** Задано из приложения (лежит в базе сервера): можно сбросить. */
  stored: boolean
  /** Переменная окружения со значением по умолчанию. */
  env: string
}

/** Изменение настройки сервера: `value: null` — сбросить к значению по умолчанию. */
export interface SettingChange {
  key: string
  value: number | null
}

/** Сводка сервера (`server_summary`). */
export interface ServerSummary {
  groups: number
  devices: number
  bytes_items: number
  bytes_files: number
  bytes_pins: number
  version: string
}

/** Итог смены токена relay. */
export interface RelayRotated {
  /** Новый код сервера администратора. */
  server_code: string
  /** Новый код приглашения в эту группу. */
  invite: string
}

/** Группа сервера (`server_groups`, `clipwire/10`). Имён у групп нет: только отпечаток id. */
export interface ServerGroup {
  /** Id группы, 64 hex. */
  id: string
  /** Короткий отпечаток id (10 hex). */
  fingerprint: string
  /** Группа этого устройства: её удалить нельзя. */
  mine: boolean
  created_ms: number
  /** Последний вход любого устройства; 0 — устройств нет. */
  last_seen_ms: number
  devices: number
  online: number
  items: number
  item_bytes: number
  files: number
  file_bytes: number
  pins: number
  pin_bytes: number
  has_admin: boolean
  state: 'live' | 'pending' | 'rotated' | 'idle'
  /** У `rotated`: сколько дней ещё ждут пакеты нового ключа. */
  days_left: number | null
}

/** Ответ `server_groups`. */
export interface ServerGroups {
  /** Сервер знает группы сервера; нет — список пустой, нужно обновить сервер. */
  supported: boolean
  groups: ServerGroup[]
}

/** Админ группы (`group_admin`). */
export interface GroupAdmin {
  /** Сервер знает админов групп (0.5.0 и новее). */
  supported: boolean
  /** Id админа (64 hex); null — у группы админа нет. */
  admin: string | null
}

export const api = {  settings: () => invoke<Settings>('get_settings'),
  saveSettings: (settings: Settings) => invoke<void>('save_settings', { settings }),
  status: () => invoke<Status>('get_status'),
  setPaused: (paused: boolean) => invoke<void>('set_paused', { paused }),
  inviteCode: () => invoke<string>('invite_code'),
  createGroup: (serverCode: string) => invoke<string>('create_group', { serverCode }),
  joinGroup: (inviteCode: string) => invoke<void>('join_group', { inviteCode }),
  list: (query: string, pinnedOnly: boolean, kind: KindFilter = 'all', limit = 200, offset = 0) =>
    invoke<Entry[]>('history_list', { query, pinnedOnly, kind, limit, offset }),
  get: (id: string) => invoke<Entry>('history_get', { id }),
  /** В группе закреп уходит на все устройства; открепление стирает подпись. */
  pin: (id: string, pinned: boolean) => invoke<void>('history_pin', { id, pinned }),
  /** Подпись закреплённой записи (пустая — убрать). Возвращает сохранённую: до 80 символов, без переводов строк. */
  note: (id: string, note: string) => invoke<string>('history_note', { id, note }),
  /** Del на закрепе. Отказ сервера по квоте приходит событием clipwire://pin-failed (строка). */
  deletePinned: (id: string, mode: PinnedDelete) => invoke<void>('history_delete_pinned', { id, mode }),
  remove: (id: string) => invoke<void>('history_delete', { id }),
  clear: (keepPinned: boolean) => invoke<void>('history_clear', { keepPinned }),
  /** `files` — номера файлов записи, если вставить нужно только часть. */
  paste: (id: string, intoActive?: boolean, plain?: boolean, files?: number[]) =>
    invoke<void>('popup_paste', { id, intoActive, plain, files }),
  pasteText: (text: string, intoActive?: boolean) => invoke<void>('popup_paste_text', { text, intoActive }),
  /** macOS: раздел «Универсальный доступ» в Системных настройках. */
  openAccessibilitySettings: () => invoke<void>('open_accessibility_settings'),
  update: (id: string, text: string) => invoke<void>('history_update', { id, text }),
  preview: (open: boolean) => invoke<'left' | 'right'>('popup_preview', { open }),
  hidePopup: () => invoke<void>('popup_hide'),
  sizePreview: (width: number, height: number) => invoke<void>('size_preview', { width, height }),
  sizePreviewHide: () => invoke<void>('size_preview_hide'),
  /** Раздел настроек, открытый в прошлый раз: окно закрывается насовсем. */
  settingsSectionGet: () => invoke<string | null>('settings_section_get'),
  settingsSectionSet: (section: string) => invoke<void>('settings_section_set', { section }),
  /** Спрашивать ли перед закрытием окна настроек (несохранённая форма хранилища). */
  settingsCloseGuard: (on: boolean) => invoke<void>('settings_close_guard', { on }),
  leaveGroup: () => invoke<void>('leave_group'),
  deviceInfo: () => invoke<DeviceInfo>('device_info'),
  /** Поставить обновление сейчас; на Windows программа закроется и запустится заново. */
  updateInstall: () => invoke<void>('update_install'),
  /** Версия, которая уже скачана и поставится сама, или null. */
  updateReady: () => invoke<string | null>('update_ready'),
  /** Версия, которую нашла фоновая проверка (или скачанная), или null. */
  updateFound: () => invoke<string | null>('update_found'),
  /** Забрать события группы, пришедшие с прошлого раза (в том числе пока настроек не было). */
  groupNoticesTake: () => invoke<GroupNotices>('group_notices_take'),
  devicesList: () => invoke<GroupDevice[]>('devices_list'),
  statsOverview: () => invoke<StatsOverview>('stats_overview'),
  /** `confirmed` — получатели нового ключа, которых человек видел в диалоге;
   *  `alsoRemove` — те, с кого он снял галочку. */
  deviceRemove: (deviceId: string, confirmed: string[], alsoRemove: string[]) =>
    invoke<RekeyResult>('device_remove', { deviceId, confirmed, alsoRemove }),
  groupRekey: (confirmed: string[], remove: string[]) => invoke<RekeyResult>('group_rekey', { confirmed, remove }),
  /** Смена ключа другим устройством, которая ждёт ответа; null — такой нет. */
  groupRekeyOffer: () => invoke<RekeyOffer | null>('group_rekey_offer'),
  /** «Перейти»: принять новый ключ. */
  groupRekeyAccept: () => invoke<void>('group_rekey_accept'),
  /** «Это не я»: не переходить, устройство помечается исключённым. */
  groupRekeyDecline: () => invoke<void>('group_rekey_decline'),
  /** После исключения: снять отметку и снова запустить синхронизацию. */
  groupRetry: () => invoke<void>('group_retry'),
  copyQuietly: (text: string) => invoke<void>('copy_quietly', { text }),
  /** Сколько места занимает кэш скачанных файлов, байт. */
  cacheSize: () => invoke<number>('cache_size'),
  /** Удалить скачанные копии, кроме тех, что качаются сейчас. */
  cacheClear: () => invoke<CacheCleared>('cache_clear'),
  editImage: (id: string) => invoke<void>('editor_open', { id }),
  /** Какую картинку показывать редактору: последний запрошенный id. */
  editorPending: () => invoke<string | null>('editor_pending'),
  linkPreview: (url: string) => invoke<LinkCard>('link_preview', { url }),
  filePreview: (path: string) => invoke<FilePreview>('file_preview', { path }),
  /** Скачать файлы записи с другого устройства без вставки. Прогресс идёт событиями. */
  filesDownload: (id: string) => invoke<void>('files_download', { id }),
  /** Открыть папку из истории (свою или скачанную) в файловом менеджере. */
  folderOpen: (path: string) => invoke<void>('folder_open', { path }),
  transfers: () => invoke<Transfer[]>('transfers'),
  /** Хранилище файлов группы: настройки без секретов. */
  storageGet: () => invoke<StorageState>('storage_get'),
  /** Сохранить настройки хранилища: уйдут всем устройствам группы. */
  storageSave: (form: StorageForm) => invoke<StorageState>('storage_save', { form }),
  storageProbe: (form: StorageForm) => invoke<ProbeResult>('storage_probe', { form }),
  /** Есть ли у устройства ключ администратора сервера; `stale` — ключ был, но сервер его не принял (сменили). */
  adminState: () => invoke<{ has_key: boolean; stale: boolean }>('admin_state'),
  /** Новый код приглашения в ту же группу (после смены токена relay): группа и ключи остаются. */
  syncUpdateInvite: (code: string) => invoke<void>('sync_update_invite', { code }),
  /** Стать админом группы самому (нужен ключ администратора сервера). */
  groupAdminClaim: () => invoke<void>('group_admin_claim'),
  /** «Я администратор сервера»: проверить код cwserver2 на сервере и сохранить ключ. */
  adminClaim: (code: string) => invoke<void>('admin_claim', { code }),
  /** Забыть ключ администратора на этом устройстве. */
  adminForget: () => invoke<void>('admin_forget'),
  serverSettings: () => invoke<ServerSetting[]>('server_settings_get'),
  serverSettingsSet: (changes: SettingChange[]) => invoke<ServerSetting[]>('server_settings_set', { changes }),
  serverSummary: () => invoke<ServerSummary>('server_summary'),
  serverGroups: () => invoke<ServerGroups>('server_groups'),
  /** Удалить группу сервера целиком: возвращает список после удаления. */
  serverGroupDelete: (groupId: string) => invoke<ServerGroup[]>('server_group_delete', { groupId }),
  /** Сменить токен relay: все устройства отключатся и войдут по новому коду приглашения. */
  serverRotateRelayToken: () => invoke<RelayRotated>('server_rotate_relay_token'),
  /** Сменить ключ администратора: возвращает новый код сервера. */
  serverRotateAdminKey: () => invoke<string>('server_rotate_admin_key'),
  groupAdmin: () => invoke<GroupAdmin>('group_admin'),
  /** Передать роль админа группы другому устройству. */
  deviceMakeAdmin: (deviceId: string) => invoke<void>('device_make_admin', { deviceId }),}
