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
}

export interface DeviceInfo {
  version: string
  relay_url: string | null
  device_id: string | null
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
}

export interface FileInfo {
  name: string
  path: string
  size: number
  is_dir: boolean
  /** Файл ещё на месте. */
  exists: boolean
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
  paste: (id: string, intoActive?: boolean, plain?: boolean) =>
    invoke<void>('popup_paste', { id, intoActive, plain }),
  pasteText: (text: string, intoActive?: boolean) => invoke<void>('popup_paste_text', { text, intoActive }),
  update: (id: string, text: string) => invoke<void>('history_update', { id, text }),
  preview: (open: boolean) => invoke<'left' | 'right'>('popup_preview', { open }),
  hidePopup: () => invoke<void>('popup_hide'),
  sizePreview: (width: number, height: number) => invoke<void>('size_preview', { width, height }),
  sizePreviewHide: () => invoke<void>('size_preview_hide'),
  leaveGroup: () => invoke<void>('leave_group'),
  deviceInfo: () => invoke<DeviceInfo>('device_info'),
  copyQuietly: (text: string) => invoke<void>('copy_quietly', { text }),
  editImage: (id: string) => invoke<void>('editor_open', { id }),
  linkPreview: (url: string) => invoke<LinkCard>('link_preview', { url }),
  filePreview: (path: string) => invoke<FilePreview>('file_preview', { path }),
}
