// Логика карточки «Хранилище файлов»: проверка полей, когда сохранённый
// секрет можно оставить, сброс подтверждённого отпечатка ключа хоста.
// Без импортов из Svelte и Tauri, чтобы запускаться в Node для тестов.
import type { SavedSecrets, SftpForm, StorageForm } from './api'
import { t, lang } from './i18n.ts'

/**
 * Варианты срока хранения файлов во внешнем хранилище, часы. 0 — не чистить.
 * Функция, а не константа: подписи зависят от языка, а он выбирается после
 * загрузки модуля.
 */
export function ttlBase(): { hours: number; title: string }[] {
  return [
    { hours: 6, title: t('6 часов') },
    { hours: 12, title: t('12 часов') },
    { hours: 24, title: t('1 день') },
    { hours: 72, title: t('3 дня') },
    { hours: 168, title: t('Неделя') },
    { hours: 720, title: t('30 дней') },
    { hours: 0, title: t('Не удалять') },
  ]
}

/** Варианты списка; сохранённое нестандартное значение тоже в нём. */
export function ttlOptions(current: number): { hours: number; title: string }[] {
  const base = ttlBase()
  if (base.some((o) => o.hours === current)) return base
  const title = current % 24 === 0 ? t('{n} дн.', { n: current / 24 }) : t('{n} ч', { n: current })
  return [...base.slice(0, -1), { hours: current, title }, base.at(-1)!].sort(
    (a, b) => (a.hours || Infinity) - (b.hours || Infinity),
  )
}

/** Хост и порт, к которым относится отпечаток ключа хоста. */
export function hostId(f: Pick<SftpForm, 'host' | 'port'>): string {
  return `${f.host.trim().toLowerCase()}:${f.port}`
}

/** Сменился хост или порт: подтверждение отпечатка больше не годится. */
export function hostChanged(f: Pick<SftpForm, 'host' | 'port'>, confirmedFor: string): boolean {
  return hostId(f) !== confirmedFor
}

/** Адрес S3 по HTTP к localhost или адресу локальной сети (так же решает движок). */
export function isLocalHost(host: string): boolean {
  const h = host.toLowerCase().replace(/^\[|\]$/g, '')
  if (h === 'localhost' || h.endsWith('.localhost') || h === '::1') return true
  const m = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/)
  if (m) {
    const [a, b] = [Number(m[1]), Number(m[2])]
    return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254)
  }
  return /^f[cd][0-9a-f]{2}:/.test(h) || h.startsWith('fe80:')
}

/** Адрес S3 по HTTP (без шифрования канала). */
export function isHttp(endpoint: string): boolean {
  return endpoint.trim().toLowerCase().startsWith('http://')
}

/** Отпечаток ключа хоста к виду `SHA256:<base64 без =>` (так же, как движок). */
export function normFingerprint(s: string): string {
  const v = s.trim()
  const body = v.startsWith('SHA256:') || v.startsWith('sha256:') ? v.slice(7) : v
  return `SHA256:${body.replace(/=+$/, '')}`
}

/**
 * Тот же сервер SFTP, но подтверждён другой ключ сервера. Сохранённый пароль
 * или ключ ему не отправляются: смена ключа сервера может значить перехват.
 * Пустой отпечаток в форме — только запрос отпечатка, без входа.
 */
export function hostKeyChanged(form: StorageForm, saved: StorageForm): boolean {
  if (form.kind !== 'sftp' || saved.kind !== 'sftp') return false
  const a = form.sftp.host_key_sha256
  return !!a.trim() && normFingerprint(a) !== normFingerprint(saved.sftp.host_key_sha256)
}

/**
 * Сохранённый секрет остаётся, если поле пустое и хранилище то же: адрес и ключ
 * доступа (S3) или хост, порт, пользователь и подтверждённый ключ сервера
 * (SFTP) не менялись. Так же решает приложение (`storage.rs`), здесь — чтобы
 * сразу подсказать.
 */
export function keepsSecret(form: StorageForm, saved: StorageForm, has: SavedSecrets): boolean {
  if (form.kind !== saved.kind) return false
  if (form.kind === 's3') {
    const norm = (s: string) => s.trim().replace(/\/+$/, '')
    return has.s3_secret && norm(form.s3.endpoint) === norm(saved.s3.endpoint) && form.s3.access_key.trim() === saved.s3.access_key.trim()
  }
  if (form.kind === 'sftp') {
    const a = form.sftp
    const b = saved.sftp
    if (a.host.trim() !== b.host.trim() || a.port !== b.port || a.user.trim() !== b.user.trim() || a.auth !== b.auth) return false
    if (hostKeyChanged(form, saved)) return false
    return a.auth === 'password' ? has.sftp_password : has.sftp_key
  }
  return false
}

/** Ошибки полей: имя поля → текст. Пусто — можно проверять подключение. */
export function validate(form: StorageForm, saved: StorageForm, has: SavedSecrets): Record<string, string> {
  const e: Record<string, string> = {}
  const keep = keepsSecret(form, saved, has)
  if (form.kind === 's3') {
    const f = form.s3
    const url = f.endpoint.trim()
    if (!url) e.endpoint = t('Укажите адрес API')
    else {
      let parsed: URL | null = null
      try {
        parsed = new URL(url)
      } catch {
        e.endpoint = t('Адрес должен начинаться с https://')
      }
      if (parsed) {
        if (parsed.protocol === 'http:' && !isLocalHost(parsed.hostname)) {
          e.endpoint = t('По HTTP можно только к своей локальной сети, нужен https://')
        } else if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
          e.endpoint = t('Адрес должен начинаться с https://')
        }
      }
    }
    if (!f.bucket.trim()) e.bucket = t('Укажите бакет')
    else if (!f.path_style && f.bucket.includes('.') && !isHttp(url)) {
      e.bucket = t('Бакет с точкой в имени работает только с адресацией через путь')
    }
    if (!f.access_key.trim()) e.access_key = t('Укажите ключ доступа')
    if (!f.secret_key && !keep) e.secret_key = t('Укажите секретный ключ')
  } else if (form.kind === 'sftp') {
    const f = form.sftp
    if (!f.host.trim()) e.host = t('Укажите сервер')
    else if (/[\s/@]/.test(f.host.trim())) e.host = t('Только имя или адрес сервера, без пользователя и пути')
    if (!Number.isInteger(f.port) || f.port < 1 || f.port > 65535) e.port = t('Порт — число от 1 до 65535')
    if (!f.user.trim()) e.user = t('Укажите пользователя')
    const rekeyed = hostKeyChanged(form, saved)
    if (f.auth === 'password') {
      if (!f.password && !keep) e.password = rekeyed ? t('Ключ сервера изменился: введите пароль заново') : t('Введите пароль')
    } else {
      const missing = rekeyed ? t('Ключ сервера изменился: вставьте закрытый ключ заново') : t('Вставьте закрытый ключ или выберите файл')
      const problem = f.private_key.trim() ? keyProblem(f.private_key) : keep ? '' : missing
      if (problem) e.private_key = problem
    }
  }
  return e
}

/** Что не так с текстом закрытого ключа; пусто — похоже на ключ. */
export function keyProblem(text: string): string {
  const v = text.trim()
  if (/^(ssh-(rsa|ed25519|dss)|ecdsa-sha2-)\s/.test(v)) return t('Это открытый ключ (.pub), нужен закрытый')
  if (v.startsWith('PuTTY-User-Key-File')) return t('Ключ PuTTY (.ppk) не подходит, сохраните его в формате OpenSSH')
  if (!/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(v)) return t('Не похоже на закрытый ключ: нужен текст с -----BEGIN … PRIVATE KEY-----')
  return ''
}

/** Предел файла ключа: настройки группы целиком не больше 16 КБ. */
export const MAX_KEY_BYTES = 12 * 1024

/**
 * Отпечаток текущих настроек: проверка подключения относится к ним, после
 * любой правки её надо повторить. Срок хранения на подключение не влияет.
 */
export function probeKey(form: StorageForm): string {
  const { kind, s3, sftp } = form
  if (kind === 'server') return 'server'
  if (kind === 's3') return JSON.stringify(['s3', s3])
  // Отпечаток ключа хоста в счёт не идёт: его подтверждают по итогам проверки.
  return JSON.stringify(['sftp', { ...sftp, host_key_sha256: '' }])
}

/** Есть ли несохранённые изменения. */
export function isDirty(form: StorageForm, saved: StorageForm): boolean {
  if (form.kind !== saved.kind || form.file_ttl_hours !== saved.file_ttl_hours) return true
  if (form.kind === 's3') return JSON.stringify(form.s3) !== JSON.stringify(saved.s3)
  if (form.kind === 'sftp') return JSON.stringify(form.sftp) !== JSON.stringify(saved.sftp)
  return false
}

/** Глубокая копия формы (вложенные объекты правятся через bind). */
export function copyForm(f: StorageForm): StorageForm {
  return { ...f, s3: { ...f.s3 }, sftp: { ...f.sftp } }
}

/** «12:05» или «13 нояб., 12:05» для строки предупреждения. */
export function warningTime(ms: number, now = Date.now()): string {
  const d = new Date(ms)
  const loc = lang() === 'en' ? 'en-GB' : 'ru-RU'
  const time = d.toLocaleTimeString(loc, { hour: '2-digit', minute: '2-digit' })
  return new Date(now).toDateString() === d.toDateString()
    ? time
    : `${d.toLocaleDateString(loc, { day: 'numeric', month: 'short' })}, ${time}`
}
