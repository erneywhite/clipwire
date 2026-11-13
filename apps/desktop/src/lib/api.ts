// Типизированные обёртки над командами Rust.
import { invoke } from '@tauri-apps/api/core'

export type Theme = 'graphite' | 'paper' | 'signal'
export type Mode = 'system' | 'light' | 'dark'
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
}

export interface DeviceInfo {
  version: string
  relay_url: string | null
  device_id: string | null
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
  site: string
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
  /** Сервер сообщил, что устройство исключено: синхронизация остановлена, ключ на месте. */
  | { kind: 'excluded' }

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
}

/** Итог «Проверить подключение». */
export type ProbeResult =
  | { status: 'ok'; host_key: string | null; insecure_http: boolean }
  | { status: 'confirm_host_key'; host_key: string }
  | { status: 'host_key_mismatch'; expected: string; got: string }
  | { status: 'error'; message: string }

export const api = {
  settings: () => invoke<Settings>('get_settings'),
  saveSettings: (settings: Settings) => invoke<void>('save_settings', { settings }),
  status: () => invoke<Status>('get_status'),
  setPaused: (paused: boolean) => invoke<void>('set_paused', { paused }),
  inviteCode: () => invoke<string>('invite_code'),
  createGroup: (serverCode: string) => invoke<string>('create_group', { serverCode }),
  joinGroup: (inviteCode: string) => invoke<void>('join_group', { inviteCode }),
  list: (query: string, pinnedOnly: boolean, limit = 200) =>
    invoke<Entry[]>('history_list', { query, pinnedOnly, limit }),
  get: (id: string) => invoke<Entry>('history_get', { id }),
  pin: (id: string, pinned: boolean) => invoke<void>('history_pin', { id, pinned }),
  remove: (id: string) => invoke<void>('history_delete', { id }),
  clear: (keepPinned: boolean) => invoke<void>('history_clear', { keepPinned }),
  /** `files` — номера файлов записи, если вставить нужно только часть. */
  paste: (id: string, intoActive?: boolean, plain?: boolean, files?: number[]) =>
    invoke<void>('popup_paste', { id, intoActive, plain, files }),
  pasteText: (text: string, intoActive?: boolean) => invoke<void>('popup_paste_text', { text, intoActive }),
  update: (id: string, text: string) => invoke<void>('history_update', { id, text }),
  preview: (open: boolean) => invoke<'left' | 'right'>('popup_preview', { open }),
  hidePopup: () => invoke<void>('popup_hide'),
  sizePreview: (width: number, height: number) => invoke<void>('size_preview', { width, height }),
  sizePreviewHide: () => invoke<void>('size_preview_hide'),
  leaveGroup: () => invoke<void>('leave_group'),
  deviceInfo: () => invoke<DeviceInfo>('device_info'),
  devicesList: () => invoke<GroupDevice[]>('devices_list'),
  deviceRemove: (deviceId: string) => invoke<RekeyResult>('device_remove', { deviceId }),
  groupRekey: () => invoke<RekeyResult>('group_rekey'),
  /** После исключения: снять отметку и снова запустить синхронизацию. */
  groupRetry: () => invoke<void>('group_retry'),
  copyQuietly: (text: string) => invoke<void>('copy_quietly', { text }),
  /** Сколько места занимает кэш скачанных файлов, байт. */
  cacheSize: () => invoke<number>('cache_size'),
  /** Удалить скачанные копии, кроме тех, что качаются сейчас. */
  cacheClear: () => invoke<CacheCleared>('cache_clear'),
  editImage: (id: string) => invoke<void>('editor_open', { id }),
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
}
