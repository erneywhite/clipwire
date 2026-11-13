// Проверка логики карточки «Хранилище файлов». Запуск: node src/lib/storage.test.ts
// (Node 23.6+ сам убирает типы), как devices.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { SavedSecrets, StorageForm } from './api'

const { copyForm, hostChanged, hostId, isDirty, isLocalHost, keepsSecret, keyProblem, probeKey, ttlOptions, validate } =
  (await import('./storage' + '.ts')) as typeof import('./storage')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const none: SavedSecrets = { s3_secret: false, sftp_password: false, sftp_key: false, sftp_passphrase: false }

function form(over: Partial<StorageForm> = {}): StorageForm {
  return {
    kind: 'server',
    file_ttl_hours: 24,
    s3: { endpoint: '', region: 'us-east-1', bucket: '', prefix: '', access_key: '', secret_key: '', path_style: true },
    sftp: { host: '', port: 22, user: '', auth: 'password', password: '', private_key: '', passphrase: '', path: '', host_key_sha256: '' },
    ...over,
  }
}

const KEY = '-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----'

// Сервер Clipwire: проверять нечего.
check(Object.keys(validate(form(), form(), none)).length === 0, 'сервер без ошибок')

// S3: пустые поля и адреса.
const s3 = form({ kind: 's3' })
const e1 = validate(s3, form(), none)
check(!!e1.endpoint && !!e1.bucket && !!e1.access_key && !!e1.secret_key, 'пустой S3 — все обязательные поля')
const good = copyForm(s3)
Object.assign(good.s3, { endpoint: 'https://s3.example.com', bucket: 'clip', access_key: 'AK', secret_key: 'SK' })
check(Object.keys(validate(good, form(), none)).length === 0, 'заполненный S3 годится')
good.s3.endpoint = 'http://s3.example.com'
check(validate(good, form(), none).endpoint.includes('локальной сети'), 'HTTP в интернет нельзя')
good.s3.endpoint = 'http://192.168.1.10:9000'
check(!validate(good, form(), none).endpoint, 'HTTP к домашнему MinIO можно')
good.s3.endpoint = 's3.example.com'
check(!!validate(good, form(), none).endpoint, 'адрес без схемы')
good.s3.endpoint = 'https://s3.example.com'
good.s3.path_style = false
good.s3.bucket = 'my.bucket'
check(!!validate(good, form(), none).bucket, 'бакет с точкой без адресации через путь')
check(isLocalHost('10.0.0.5') && isLocalHost('localhost') && isLocalHost('[::1]') && !isLocalHost('8.8.8.8') && !isLocalHost('172.32.0.1'), 'локальные адреса')

// Сохранённый секрет остаётся, пока хранилище то же.
const savedS3 = copyForm(s3)
Object.assign(savedS3.s3, { endpoint: 'https://s3.example.com', bucket: 'clip', access_key: 'AK' })
const has = { ...none, s3_secret: true }
const edit = copyForm(savedS3)
check(keepsSecret(edit, savedS3, has) && !validate(edit, savedS3, has).secret_key, 'секрет можно не вводить заново')
edit.s3.endpoint = 'https://s3.example.com/'
check(keepsSecret(edit, savedS3, has), 'слэш в конце адреса не в счёт')
edit.s3.endpoint = 'https://other.example.com'
check(!keepsSecret(edit, savedS3, has) && !!validate(edit, savedS3, has).secret_key, 'другой адрес — секрет заново')
edit.s3.endpoint = savedS3.s3.endpoint
edit.s3.access_key = 'AK2'
check(!keepsSecret(edit, savedS3, has), 'другой ключ доступа — секрет заново')
check(!keepsSecret(savedS3, savedS3, none), 'секрета не было — нечего оставлять')

// SFTP.
const sftp = form({ kind: 'sftp' })
const e2 = validate(sftp, form(), none)
check(!!e2.host && !!e2.user && !!e2.password && !e2.port, 'пустой SFTP')
Object.assign(sftp.sftp, { host: 'nas.local', user: 'clip', password: 'p' })
check(Object.keys(validate(sftp, form(), none)).length === 0, 'SFTP с паролем годится')
sftp.sftp.port = 70000
check(!!validate(sftp, form(), none).port, 'порт больше 65535')
sftp.sftp.port = 22
sftp.sftp.host = 'clip@nas.local'
check(!!validate(sftp, form(), none).host, 'пользователь в поле хоста')
sftp.sftp.host = 'nas.local'
sftp.sftp.auth = 'key'
check(!!validate(sftp, form(), none).private_key, 'ключ не вставлен')
sftp.sftp.private_key = 'ssh-ed25519 AAAA clip@nas'
check(validate(sftp, form(), none).private_key.includes('.pub'), 'открытый ключ вместо закрытого')
sftp.sftp.private_key = KEY
check(!validate(sftp, form(), none).private_key, 'ключ OpenSSH годится')
check(keyProblem('PuTTY-User-Key-File-3: ssh-ed25519\n').includes('PuTTY'), 'ключ PuTTY')
check(keyProblem('-----BEGIN RSA PRIVATE KEY-----\nxx') === '', 'старый формат RSA тоже')

// Отпечаток ключа хоста: смена хоста или порта сбрасывает подтверждение.
const confirmed = hostId({ host: 'NAS.local ', port: 22 })
check(!hostChanged({ host: 'nas.local', port: 22 }, confirmed), 'регистр и пробелы не в счёт')
check(hostChanged({ host: 'nas2.local', port: 22 }, confirmed), 'другой хост')
check(hostChanged({ host: 'nas.local', port: 2222 }, confirmed), 'другой порт')

// Проверка подключения относится к настройкам, а не к сроку и отпечатку.
const a = copyForm(sftp)
const b = copyForm(sftp)
b.file_ttl_hours = 72
b.sftp.host_key_sha256 = 'SHA256:x'
check(probeKey(a) === probeKey(b), 'срок и отпечаток не требуют новой проверки')
b.sftp.path = '/data'
check(probeKey(a) !== probeKey(b), 'другая папка — проверить снова')
check(isDirty(b, a) && !isDirty(copyForm(a), a), 'несохранённые изменения')
const c = copyForm(a)
c.s3.bucket = 'другое'
check(!isDirty(c, a), 'поля невыбранного вида хранилища не в счёт')

// Срок хранения.
check(ttlOptions(24).length === 7, 'стандартный срок')
const custom = ttlOptions(48)
check(custom.some((o) => o.hours === 48 && o.title === '2 дн.') && custom.at(-1)!.hours === 0, 'нестандартный срок в списке, «Не удалять» в конце')

console.log('storage: всё прошло')
