// Подписи для блока «Состояние» в «О программе».
import type { GroupDevice, GroupStats, StorageState } from './api'
import type * as format from './format'

/** Функции из `format.ts`, параметром — как в `devices.ts`, ради тестов в Node. */
export type Fmt = Pick<typeof format, 'bytesLabel' | 'plural'>

export type Tone = 'ok' | 'warn' | 'error'

/** «128 записей, 5 закреплено», «Пусто». */
export function historyLabel(total: number, pinned: number, { plural }: Fmt): string {
  if (total === 0) return 'Пусто'
  const head = `${total} ${plural(total, ['запись', 'записи', 'записей'])}`
  return pinned > 0 ? `${head}, ${pinned} ${plural(pinned, ['закреплена', 'закреплены', 'закреплено'])}` : head
}

/** История группы на сервере: «42 элемента · 1,2 МБ». */
export function serverItemsLabel(s: GroupStats, { bytesLabel, plural }: Fmt): string {
  if (s.items === 0) return 'Пусто'
  return `${s.items} ${plural(s.items, ['элемент', 'элемента', 'элементов'])} · ${bytesLabel(s.item_bytes)}`
}

/** Файлы в хранилище сервера: «3 файла · 40,0 МБ». */
export function serverFilesLabel(s: GroupStats, { bytesLabel, plural }: Fmt): string {
  if (s.files === 0) return 'Нет'
  return `${s.files} ${plural(s.files, ['файл', 'файла', 'файлов'])} · ${bytesLabel(s.file_bytes)}`
}

/** «3, в сети 2». Это устройство считается в сети: список пришёл через сервер. */
export function devicesLabel(list: GroupDevice[]): string {
  const online = list.filter((d) => d.online || d.me).length
  return `${list.length}, в сети ${online}`
}

/** Хранилище файлов группы: где лежат файлы и в порядке ли оно. */
export function storageLabel(st: StorageState): { text: string; tone: Tone } {
  const f = st.form
  if (f.kind === 'server') return { text: 'Сервер Clipwire', tone: 'ok' }
  const where = f.kind === 's3' ? `S3 · ${f.s3.bucket}` : `SFTP · ${f.sftp.host}`
  if (st.error) return { text: `${where}: не подключается`, tone: 'error' }
  if (st.warning) return { text: `${where}: были сбои, файлы шли через сервер`, tone: 'warn' }
  return { text: where, tone: 'ok' }
}
