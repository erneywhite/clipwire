// Проверка подписей раздела «Устройства группы». Запуск: node src/lib/devices.test.ts
// (Node 23.6+ сам убирает типы), как transfer.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { GroupDevice } from './api'

const { confirmText, deviceTitle, leftOut, rekeyedBy, seenLabel } = (await import('./devices' + '.ts')) as typeof import('./devices')
const fmt = (await import('./format' + '.ts')) as typeof import('./format')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

function d(id: string, over: Partial<GroupDevice>): GroupDevice {
  return { id, fingerprint: id.slice(0, 4), name: id, os: 'Windows', known: true, me: false, online: false, last_seen_ms: 0, ...over }
}

const now = Date.parse('2026-11-13T12:00:00Z')
const me = d('aaaa', { me: true, online: true, name: 'Ноутбук' })
const work = d('bbbb', { name: 'Рабочий' })
const phone = d('cccc', { name: 'Старый' })
const unknown = d('dddd', { name: null, os: null, known: false })
const list = [me, work, phone, unknown]

check(deviceTitle(unknown) === 'Неизвестное устройство', 'без карточки — неизвестное устройство')
check(rekeyedBy('Рабочий') === 'Ключ группы сменило устройство «Рабочий»', 'кто сменил ключ')
check(rekeyedBy(' ') === 'Ключ группы сменён на другом устройстве', 'без имени')
check(deviceTitle(d('x', { name: '  ' })) === 'Неизвестное устройство', 'пустое имя тоже')
check(seenLabel(me, fmt, now) === 'в сети', 'своё в сети')
check(seenLabel(work, fmt, now) === 'не в сети', 'без времени — не в сети')
check(seenLabel(d('e', { last_seen_ms: now - 5 * 60_000 }), fmt, now) === 'был в сети: 5 мин', 'пять минут назад')

check(leftOut(list, null).map((x) => x.id).join() === 'dddd', 'неизвестное не получит ключ')
check(leftOut(list, 'dddd').length === 0, 'удаляемое неизвестное — без предупреждения')

const remove = confirmText(list, 'cccc', fmt)
check(remove[0].includes('«Старый»'), 'первым — что будет с удаляемым')
check(remove.some((s) => s.startsWith('1 устройство получит')), 'остальные с карточкой')
check(remove.some((s) => s.includes('dddd')), 'предупреждение про устройство без карточки')
check(remove.at(-1)!.includes('Код приглашения сменится'), 'про код приглашения')

const rekey = confirmText([me, work, phone], null, fmt)
check(!rekey.some((s) => s.includes('потеряет доступ')), 'без удаления никто не теряет доступ')
check(rekey.some((s) => s.startsWith('2 устройства получат')), 'двое получат ключ')
check(!rekey.some((s) => s.startsWith('Внимание')), 'без неизвестных нет предупреждения')

console.log('devices: всё прошло')
