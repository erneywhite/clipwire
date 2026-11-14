// Проверка подписей на английском. Запуск: node src/lib/format.test.ts
// (Node 23.6+ сам убирает типы), как overview.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { GroupDevice } from './api'

const fmt = (await import('./format' + '.ts')) as typeof import('./format')
const { setLang } = (await import('./i18n' + '.ts')) as typeof import('./i18n')
const { seenLabel } = (await import('./devices' + '.ts')) as typeof import('./devices')
const { historyLabel, lastSyncLabel, quotaLabel } = (await import('./overview' + '.ts')) as typeof import('./overview')
const { validate } = (await import('./storage' + '.ts')) as typeof import('./storage')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const now = Date.parse('2026-11-13T12:00:00Z')
const dev = (over: Partial<GroupDevice>): GroupDevice => ({ id: 'a', fingerprint: 'a', name: 'a', os: null, known: true, me: false, online: false, last_seen_ms: 0, ...over })

setLang('en')
try {
  check(fmt.ago(now - 5_000, now) === 'now', 'ago: now')
  check(fmt.ago(now - 5 * 60_000, now) === '5 min', 'ago: minutes')
  check(fmt.ago(now - 86_400_000, now) === 'yesterday', 'ago: yesterday')
  check(fmt.bytesLabel(512) === '512 B' && fmt.bytesLabel(340 * 1024) === '340 KB', 'bytes')
  check(fmt.bytesLabel(1.5 * 1024 * 1024) === '1.5 MB', fmt.bytesLabel(1.5 * 1024 * 1024))
  check(fmt.sizeLabel(1, 1) === '1 character' && fmt.sizeLabel(10, 3) === '3 lines', 'sizeLabel')
  check(fmt.filesLabel([{ name: 'a.pdf' }, { name: 'b.png' }]) === '2 files: a.pdf, b.png', 'filesLabel')
  check(fmt.filesMeta([{ size: 0, is_dir: true }], 0, 0, 0) === 'folder', 'filesMeta')
  check(seenLabel(dev({ last_seen_ms: now - 5 * 60_000 }), fmt, now) === 'last seen 5 min ago', 'seenLabel ago')
  check(seenLabel(dev({ last_seen_ms: now - 86_400_000 }), fmt, now) === 'last seen yesterday', 'seenLabel yesterday')
  check(seenLabel(dev({ online: true }), fmt, now) === 'online', 'online')
  check(historyLabel(128, 5, fmt) === '128 entries, 5 pinned', historyLabel(128, 5, fmt))
  const sync = lastSyncLabel(now - 5 * 60_000, now, fmt, now)
  check(sync === 'Sent 5 min ago, received just now', sync)
  const q = { items: 0, item_bytes: 0, files: 0, file_bytes: 0, quota_used: 1024, quota: 2048 }
  check(quotaLabel(q, fmt).text === '1 KB of 2 KB (50%)', 'quota')
  const f = { kind: 'sftp', s3: {}, sftp: { host: '', port: 22, user: '', auth: 'password', password: '', private_key: '', host_key_sha256: '' } } as never
  check(validate(f, f, { s3_secret: false, sftp_password: false, sftp_key: false, sftp_passphrase: false })['host'] === 'Enter the server', 'validate')
} finally {
  setLang('ru')
}
check(fmt.ago(now - 5 * 60_000, now) === '5 мин' && fmt.bytesLabel(1.5 * 1024 * 1024) === '1,5 МБ', 'русский после возврата')
