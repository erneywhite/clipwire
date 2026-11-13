// Проверка сводки передач. Запуск: node src/lib/transfer.test.ts
// (Node 23.6+ сам убирает типы). Тестового фреймворка у фронтенда нет,
// поэтому простые проверки с исключением.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { FileInfo, Transfer } from './api'

// Node требует расширение в пути, а TypeScript без allowImportingTsExtensions его
// не пропускает: путь собирается строкой, тип берётся отдельно.
const { allUploaded, canDownload, entryProgress, fetchLabel, isDirect, isGone, onlyThere, percentOf, progressLabel } = (await import('./transfer' + '.ts')) as typeof import('./transfer')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

function t(blob: string, over: Partial<Transfer>): Transfer {
  return { blob, direction: 'up', done: 0, total: 100, state: 'active', error: null, ...over }
}

check(percentOf(0, 0) === 0, 'пустой файл — 0%')
check(percentOf(999, 1000) === 99, '99,9% показывается как 99%')
check(percentOf(1000, 1000) === 100, 'всё передано — 100%')
check(percentOf(2000, 1000) === 100, 'больше 100% не бывает')

check(entryProgress([{ blob: null }], {}) === null, 'папка без передач — ничего')
check(entryProgress([{ blob: 'a' }], {}) === null, 'передачи ещё нет — ничего')
check(entryProgress([{ blob: 'a' }], { a: t('a', { state: 'done', done: 100 }) }) === null, 'всё готово — полосы нет')

{
  const p = entryProgress([{ blob: 'a' }, { blob: 'b' }, { blob: null }], {
    a: t('a', { done: 50, total: 100 }),
    b: t('b', { done: 0, total: 300 }),
  })
  check(p !== null && p.done === 50 && p.total === 400 && p.percent === 12, 'прогресс суммарный по файлам')
  check(p !== null && progressLabel(p) === '↑ 12%', 'подпись загрузки')
}

{
  // Первый файл уже готов, последнее событие о нём пришло чуть раньше конца.
  const p = entryProgress([{ blob: 'a' }, { blob: 'b' }], {
    a: t('a', { direction: 'down', state: 'done', done: 90, total: 100 }),
    b: t('b', { direction: 'down', done: 50, total: 100 }),
  })
  check(p !== null && p.direction === 'down' && p.done === 150 && p.active, 'готовый файл засчитан целиком')
  check(p !== null && progressLabel(p) === '↓ 75%', 'подпись скачивания')
}

{
  const p = entryProgress([{ blob: 'a' }, { blob: 'b' }], {
    a: t('a', { done: 10 }),
    b: t('b', { direction: 'down', done: 20 }),
  })
  check(p !== null && p.direction === 'down', 'скачивание важнее загрузки')
}

{
  const p = entryProgress([{ blob: 'a' }, { blob: 'b' }], {
    a: t('a', { state: 'failed', error: 'файл изменился после копирования' }),
    b: t('b', { state: 'done', done: 100 }),
  })
  check(p !== null && !p.active, 'после ошибки ничего не идёт')
  check(p !== null && progressLabel(p) === 'не отправлено: файл изменился после копирования', 'подпись ошибки')
}

{
  const p = entryProgress([{ blob: 'a' }], { a: t('a', { direction: 'down', state: 'failed', error: null }) })
  check(p !== null && progressLabel(p) === 'не скачано: неизвестная ошибка', 'ошибка без текста')
}

{
  // Какие файлы можно скачать, какие пропали насовсем.
  const f = (over: Partial<FileInfo>): FileInfo => ({
    name: 'a.txt',
    path: 'C:\\a.txt',
    size: 3,
    is_dir: false,
    exists: false,
    blob: 'b'.repeat(32),
    p2p: false,
    blob_size: 3,
    local_path: null,
    remote: false,
    ...over,
  })
  check(canDownload(f({ remote: true })), 'чужой файл с сервера можно скачать')
  check(canDownload(f({})), 'свой пропавший файл с копией на сервере можно скачать')
  check(!isGone(f({})), 'свой пропавший с копией на сервере — не пропал')
  check(!canDownload(f({ exists: true, local_path: 'C:\\a.txt' })), 'файл на месте качать не надо')
  check(canDownload(f({ is_dir: true, remote: true })), 'чужую папку можно скачать архивом')
  check(canDownload(f({ is_dir: true })), 'свою пропавшую папку до порога можно скачать с сервера')
  check(!canDownload(f({ is_dir: true, remote: true, exists: true, local_path: 'D:\\files\\b\\Папка' })), 'распакованную папку качать не надо')
  check(canDownload(f({ remote: true, p2p: true })), 'чужой файл больше порога качается напрямую')
  check(isDirect(f({ remote: true, p2p: true })), 'чужой p2p-файл — напрямую с устройства')
  check(!isDirect(f({ remote: true })), 'чужой файл до порога — через сервер')
  check(!canDownload(f({ p2p: true })), 'свой p2p-файл пропал — брать неоткуда')
  check(isGone(f({ p2p: true })), 'свой p2p-файл пропал — пропал насовсем')
  check(isGone(f({ blob: null })), 'свой пропавший без копии — пропал')
  check(!isGone(f({ blob: null, remote: true })), 'чужой без копии не «пропал», он только там')
  check(onlyThere(f({ blob: null, remote: true })), 'чужой без копии — есть только на источнике')
  check(onlyThere(f({ blob: null, remote: true, is_dir: true })), 'чужая папка без blob — только на источнике')
  check(!onlyThere(f({ remote: true, p2p: true })), 'p2p-файл не «только там»: его можно скачать')

  check(fetchLabel(f({ remote: true, p2p: true }), 'Ноутбук') === 'напрямую с устройства «Ноутбук»', 'подпись прямой передачи')
  check(fetchLabel(f({ remote: true }), 'Ноутбук') === 'не скачан', 'подпись чужого с сервера')
  check(fetchLabel(f({}), 'Ноутбук') === 'нет на месте, копия на сервере', 'подпись своего пропавшего')
  check(fetchLabel(f({ remote: true, exists: true, local_path: 'D:\\a.txt' }), 'Ноутбук') === null, 'скачанному подпись не нужна')
}

{
  const up = (state: 'active' | 'done' | 'failed', direction: 'up' | 'down' = 'up') =>
    ({ blob: 'b', direction, done: 1, total: 1, state, error: null })
  const one = [{ blob: 'b' }]
  check(allUploaded(one, { b: up('done') }), 'отправленный файл — на сервере')
  check(!allUploaded(one, { b: up('active') }), 'идущая отправка — ещё не на сервере')
  check(!allUploaded(one, {}), 'без сведений об отправке — не утверждаем')
  check(!allUploaded(one, { b: up('done', 'down') }), 'скачанный файл — не «на сервере»')
  check(!allUploaded([{ blob: 'b' }, { blob: 'c' }], { b: up('done'), c: { ...up('failed'), blob: 'c' } }), 'один не ушёл — не все на сервере')
}

console.log('transfer: все проверки прошли')
