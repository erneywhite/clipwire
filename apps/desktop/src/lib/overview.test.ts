// Проверка подписей блока «Состояние». Запуск: node src/lib/overview.test.ts
// (Node 23.6+ сам убирает типы), как devices.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { GroupDevice, StorageState } from './api'

const { devicesLabel, historyLabel, serverFilesLabel, serverItemsLabel, storageLabel } = (await import('./overview' + '.ts')) as typeof import('./overview')
const fmt = (await import('./format' + '.ts')) as typeof import('./format')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

check(historyLabel(0, 0, fmt) === 'Пусто', 'пустая история')
check(historyLabel(1, 0, fmt) === '1 запись', 'одна запись')
check(historyLabel(128, 5, fmt) === '128 записей, 5 закреплено', 'с закреплёнными')
check(historyLabel(22, 1, fmt) === '22 записи, 1 закреплена', 'склонение закреплённых')

const zero = { items: 0, item_bytes: 0, files: 0, file_bytes: 0, quota_used: 0, quota: 0 }
check(serverItemsLabel(zero, fmt) === 'Пусто' && serverFilesLabel(zero, fmt) === 'Нет', 'пустой сервер')
const s = { ...zero, items: 42, item_bytes: 2048, files: 3, file_bytes: 3 * 1024 * 1024 }
check(serverItemsLabel(s, fmt) === '42 элемента · 2 КБ', serverItemsLabel(s, fmt))
check(serverFilesLabel(s, fmt) === '3 файла · 3,0 МБ', serverFilesLabel(s, fmt))

function d(id: string, over: Partial<GroupDevice>): GroupDevice {
  return { id, fingerprint: id, name: id, os: null, known: true, me: false, online: false, last_seen_ms: 0, ...over }
}
check(devicesLabel([d('a', { me: true }), d('b', { online: true }), d('c', {})]) === '3, в сети 2', 'устройства')

function st(over: Partial<StorageState>, kind: 'server' | 's3' | 'sftp'): StorageState {
  return {
    form: {
      kind,
      file_ttl_hours: 24,
      s3: { endpoint: '', region: '', bucket: 'clip', prefix: '', access_key: '', secret_key: '', path_style: true },
      sftp: { host: 'nas.local', port: 22, user: '', auth: 'password', password: '', private_key: '', passphrase: '', path: '', host_key_sha256: '' },
    },
    saved: { s3_secret: false, sftp_password: false, sftp_key: false, sftp_passphrase: false },
    version: 1,
    error: null,
    insecure_http: false,
    warning: null,
    access_exposed: false,
    ...over,
  }
}
check(storageLabel(st({}, 'server')).text === 'Сервер Clipwire', 'сервер')
check(storageLabel(st({ error: 'x' }, 'server')).tone === 'ok', 'ошибка внешнего не касается сервера')
check(storageLabel(st({}, 's3')).text === 'S3 · clip', 's3')
check(storageLabel(st({ error: 'нет' }, 'sftp')).tone === 'error', 'sftp с ошибкой')
check(storageLabel(st({ warning: { text: 'x', at_ms: 1 } }, 's3')).tone === 'warn', 'сбой')

const { lastSyncLabel, linkLabel, quotaLabel } = (await import('./overview' + '.ts')) as typeof import('./overview')
const GB = 1024 ** 3
check(quotaLabel({ ...zero, quota: 2 * GB, quota_used: 0 }, fmt).text === '0 Б из 2,0 ГБ (0 %)', quotaLabel({ ...zero, quota: 2 * GB }, fmt).text)
check(quotaLabel({ ...zero, quota: 2 * GB, quota_used: 1.7 * GB }, fmt).tone === 'warn', 'квота 85 %')
check(quotaLabel({ ...zero, quota: 2 * GB, quota_used: 2 * GB }, fmt).tone === 'error', 'квота кончилась')
check(quotaLabel(zero, fmt).text === '—', 'без квоты')
check(linkLabel({ direct: true, rtt_ms: 12 }) === 'Напрямую · 12 мс', 'прямая связь')
check(linkLabel({ direct: false, rtt_ms: 0 }) === 'Через relay', 'relay без rtt')
const now = Date.UTC(2026, 10, 14, 12, 0)
check(lastSyncLabel(0, 0, fmt, now) === 'С запуска не было', 'ничего')
check(lastSyncLabel(now - 5 * 60_000, now - 10_000, fmt, now) === 'Отправлено 5 мин назад, получено только что', lastSyncLabel(now - 5 * 60_000, now - 10_000, fmt, now))
check(lastSyncLabel(0, now - 3 * 3600_000, fmt, now) === 'Получено 3 ч назад', lastSyncLabel(0, now - 3 * 3600_000, fmt, now))

console.log('overview: всё прошло')
