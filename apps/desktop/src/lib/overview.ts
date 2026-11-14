// Подписи для блока «Состояние» в «О программе».
import type { GroupDevice, GroupStats, StorageState } from './api'
import type * as format from './format'
import { t, tn } from './i18n.ts'

/** Функции из `format.ts`, параметром — как в `devices.ts`, ради тестов в Node. */
export type Fmt = Pick<typeof format, 'ago' | 'bytesLabel' | 'plural'>

export type Tone = 'ok' | 'warn' | 'error'

/** «128 записей, 5 закреплено», «Пусто». */
export function historyLabel(total: number, pinned: number, _fmt: Fmt): string {
  if (total === 0) return t('Пусто')
  const head = tn(total, ['запись', 'записи', 'записей'], ['entry', 'entries'])
  return pinned > 0 ? `${head}, ${tn(pinned, ['закреплена', 'закреплены', 'закреплено'], ['pinned', 'pinned'])}` : head
}

/** История группы на сервере: «42 элемента · 1,2 МБ». */
export function serverItemsLabel(s: GroupStats, { bytesLabel }: Fmt): string {
  if (s.items === 0) return t('Пусто')
  return `${tn(s.items, ['элемент', 'элемента', 'элементов'], ['item', 'items'])} · ${bytesLabel(s.item_bytes)}`
}

/** Файлы в хранилище сервера: «3 файла · 40,0 МБ». */
export function serverFilesLabel(s: GroupStats, { bytesLabel }: Fmt): string {
  if (s.files === 0) return t('Нет')
  return `${tn(s.files, ['файл', 'файла', 'файлов'], ['file', 'files'])} · ${bytesLabel(s.file_bytes)}`
}

/** «1,2 ГБ из 2,0 ГБ (60 %)». Тон предупреждает, когда место кончается. */
export function quotaLabel(s: GroupStats, { bytesLabel }: Fmt): { text: string; tone: Tone } {
  if (s.quota <= 0) return { text: '—', tone: 'ok' }
  const pct = Math.min(100, Math.round((s.quota_used / s.quota) * 100))
  const tone: Tone = pct >= 95 ? 'error' : pct >= 80 ? 'warn' : 'ok'
  return { text: t('{used} из {quota} ({pct} %)', { used: bytesLabel(s.quota_used), quota: bytesLabel(s.quota), pct }), tone }
}

/** «Напрямую · 12 мс», «Через relay · 48 мс». */
export function linkLabel(link: { direct: boolean; rtt_ms: number }): string {
  const how = link.direct ? t('Напрямую') : t('Через relay')
  return link.rtt_ms > 0 ? `${how} · ${t('{ms} мс', { ms: link.rtt_ms })}` : how
}

/** «отправлено 5 мин назад, получено сейчас»; с запуска ничего — «С запуска не было». */
export function lastSyncLabel(sent: number, received: number, { ago }: Fmt, now = Date.now()): string {
  const when = (ms: number) => {
    const a = ago(ms, now)
    return a === t('сейчас') ? t('только что') : /^\d/.test(a) ? t('{a} назад', { a }) : a
  }
  const parts: string[] = []
  if (sent) parts.push(t('отправлено {when}', { when: when(sent) }))
  if (received) parts.push(t('получено {when}', { when: when(received) }))
  if (!parts.length) return t('С запуска не было')
  const text = parts.join(', ')
  return text[0].toUpperCase() + text.slice(1)
}

/** «3, в сети 2». Это устройство считается в сети: список пришёл через сервер. */
export function devicesLabel(list: GroupDevice[]): string {
  const online = list.filter((d) => d.online || d.me).length
  return t('{total}, в сети {online}', { total: list.length, online })
}

/** Хранилище файлов группы: где лежат файлы и в порядке ли оно. */
export function storageLabel(st: StorageState): { text: string; tone: Tone } {
  const f = st.form
  if (f.kind === 'server') return { text: t('Сервер Clipwire'), tone: 'ok' }
  const where = f.kind === 's3' ? `S3 · ${f.s3.bucket}` : `SFTP · ${f.sftp.host}`
  if (st.error) return { text: t('{where}: не подключается', { where }), tone: 'error' }
  if (st.warning) return { text: t('{where}: были сбои, файлы шли через сервер', { where }), tone: 'warn' }
  return { text: where, tone: 'ok' }
}
