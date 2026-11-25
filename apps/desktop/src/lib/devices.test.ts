// Проверка подписей раздела «Устройства группы». Запуск: node src/lib/devices.test.ts
// (Node 23.6+ сам убирает типы), как transfer.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { GroupDevice } from './api'

const { FRESH_MS, adminAction, adminKeyText, waitOnline, adminConfirmText, canClaimAdmin, canManage, lastAdmin, confirmText, deviceTitle, exposedAccessText, firstSeenLabel, freshWarning, isAdmin, isFresh, isNewDeviceError, leftOut, pendingAdmins, offerText, recipients, rekeyChoice, rekeyedBy, rolesText, sameName, seenLabel, staying } = (await import('./devices' + '.ts')) as typeof import('./devices')
const fmt = (await import('./format' + '.ts')) as typeof import('./format')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

function d(id: string, over: Partial<GroupDevice>): GroupDevice {
  return { id, fingerprint: id.slice(0, 4), name: id, os: 'Windows', known: true, me: false, online: false, last_seen_ms: 0, first_seen_ms: 0, ...over }
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
check(remove.some((s) => s.startsWith('Токен доступа') && s.includes('«Сервер»')), 'предупреждение про токен доступа')
check(!confirmText([me, work, phone], null, fmt).some((s) => s.startsWith('Токен доступа')), 'смена ключа без удаления — без предупреждения про токен')
check(remove.at(-1)!.includes('Код приглашения сменится'), 'про код приглашения')

// Внешнее хранилище: доступы к нему остаются у исключённого.
check(!remove.some((s) => s.includes('хранилищу файлов')), 'хранилище не известно — без предупреждения')
check(!confirmText(list, 'cccc', fmt, 'server').some((s) => s.includes('хранилищу файлов')), 'сервер Clipwire — доступов нет')
const s3 = confirmText(list, 'cccc', fmt, 's3')
check(s3[1].startsWith('Внимание') && s3[1].includes('ключ S3'), 'S3: сменить ключ, сразу после строки про удаляемое')
check(confirmText(list, 'cccc', fmt, 'sftp').some((s) => s.includes('пароль или ключ SSH')), 'SFTP: сменить пароль или ключ')
check(s3[1].includes('у исключённого устройства и у устройства без карточки'), 'S3: доступы и у устройства без карточки')
check(!confirmText([me, work, phone], null, fmt, 's3').some((s) => s.includes('хранилищу файлов')), 'смена ключа без удаления и без неизвестных — без предупреждения')
const keepUnknown = confirmText(list, null, fmt, 'sftp')
check(keepUnknown.some((s) => s.startsWith('Внимание: у устройства без карточки остались доступы') && s.includes('ключ SSH')), 'смена ключа без удаления: доступы у устройства без карточки')
check(exposedAccessText('s3', false, 2).startsWith('Внимание: у устройств без карточки'), 'несколько без карточки')
check(exposedAccessText('s3', true, 0).startsWith('Внимание: у исключённого устройства остались'), 'только исключаемое')
check(exposedAccessText('server', true, 3) === '' && exposedAccessText(null, true, 3) === '', 'без внешнего хранилища — пусто')

const rekey = confirmText([me, work, phone], null, fmt)
check(!rekey.some((s) => s.includes('потеряет доступ')), 'без удаления никто не теряет доступ')
check(rekey.some((s) => s.startsWith('2 устройства получат')), 'двое получат ключ')
check(!rekey.some((s) => s.startsWith('Внимание')), 'без неизвестных нет предупреждения')

// Получатели нового ключа: с карточкой, без этого устройства и без удаляемого.
const ids = (l: GroupDevice[]) => l.map((x) => x.id).join()
check(ids(recipients(list, null)) === 'bbbb,cccc', 'получатели: все с карточкой, кроме своего')
check(ids(recipients(list, 'cccc')) === 'bbbb', 'удаляемое ключ не получает')
check(ids(recipients(list, 'dddd')) === 'bbbb,cccc', 'удаляют устройство без карточки — получатели те же')

// В команду уходит то, что человек видел: отмеченные — получатели, без галочки — исключаются.
const all = rekeyChoice(list, 'cccc', [])
check(all.confirmed.join() === 'bbbb' && all.remove.length === 0, 'подтверждены все показанные получатели')
check(!all.confirmed.includes('aaaa') && !all.confirmed.includes('cccc') && !all.confirmed.includes('dddd'), 'ни своё, ни удаляемое, ни без карточки')
const twin = d('eeee', { name: 'Рабочий', first_seen_ms: now - 60 * 60_000 })
const withTwin = [...list, twin]
const picked = rekeyChoice(withTwin, 'cccc', ['eeee'])
check(picked.confirmed.join() === 'bbbb' && picked.remove.join() === 'eeee', 'без галочки — в исключаемые')
check(rekeyChoice(withTwin, 'cccc', ['cccc', 'aaaa', 'zzzz']).remove.length === 0, 'лишние id в исключаемые не попадают')
// Диалог показывал список без двойника: его id в подтверждённых нет, движок смену отменит.
check(!rekeyChoice(list, 'cccc', []).confirmed.includes('eeee'), 'устройство, которого не было в диалоге, не подтверждено')

// Недавно появившиеся устройства выделяются.
check(isFresh(twin, now), 'час назад — недавнее')
check(!isFresh(d('f', { first_seen_ms: now - FRESH_MS - 1 }), now), 'больше суток — нет')
check(!isFresh(work, now), 'время 0 — устройство в группе давно')
check(!isFresh(d('g', { me: true, first_seen_ms: now }), now), 'своё не выделяется')
check(!isFresh(d('h', { known: false, first_seen_ms: now }), now), 'без карточки не получатель')
check(firstSeenLabel(work, fmt, now) === 'в группе давно', 'подпись: давно')
check(firstSeenLabel(twin, fmt, now) === 'появилось: 1 ч', 'подпись: час назад')
check(firstSeenLabel(d('i', { first_seen_ms: now - 5_000 }), fmt, now) === 'появилось только что', 'подпись: только что')
check(freshWarning(withTwin, 'cccc', [], now).startsWith('Внимание'), 'предупреждение про недавние')
check(freshWarning(withTwin, 'cccc', ['eeee'], now) === '', 'галочку сняли — предупреждать не о чем')
check(freshWarning(list, 'cccc', [], now) === '', 'недавних нет')
check(freshWarning(withTwin, 'eeee', [], now) === '', 'недавнее и есть удаляемое')

// Двойник по имени виден и без «недавно»: он мог войти в группу заранее.
const oldTwin = d('ffff', { name: 'рабочий ' })
check(sameName([...list, oldTwin], work) && sameName([...list, oldTwin], oldTwin), 'одинаковое имя без учёта регистра и пробелов')
check(!sameName(list, work) && !sameName(list, unknown), 'имя одно; без карточки не сравнивается')
check(freshWarning([...list, oldTwin], 'cccc', [], now).includes('одинаковым именем'), 'предупреждение про одинаковые имена')
check(freshWarning([...list, oldTwin], 'cccc', ['ffff'], now) === '', 'с двойника галочку сняли — имя снова одно')
check(ids(staying([...list, oldTwin], 'cccc', ['ffff'])) === 'aaaa,bbbb', 'остаются своё и получатели с галочкой')
check(freshWarning([me, d('gggg', { name: 'Ноутбук' })], null, [], now).includes('одинаковым именем'), 'двойник этого устройства')

// Текст подтверждения учитывает снятые галочки.
const dropTwin = confirmText(withTwin, 'cccc', fmt, 's3', ['eeee'])
check(dropTwin.some((s) => s.startsWith('Без галочки: «Рабочий» (eeee)')), 'кто без галочки')
check(dropTwin.some((s) => s.startsWith('1 устройство получит')), 'получателей на одного меньше')
const dropOnly = confirmText([me, work, phone], null, fmt, 's3', ['cccc'])
check(dropOnly.some((s) => s.startsWith('Внимание: у исключённого устройства остались')), 'смена ключа со снятой галочкой: доступы к хранилищу')
check(dropOnly.some((s) => s.startsWith('Токен доступа')), 'и токен сервера')
check(dropOnly.some((s) => s.startsWith('1 устройство получит')), 'один получатель')

check(isNewDeviceError('В группе появилось новое устройство. Проверьте список получателей и подтвердите смену ключа заново'), 'отмена смены: новое устройство')
check(isNewDeviceError('A new device has joined the group. Check the list'), 'то же по-английски')
check(!isNewDeviceError('Нет связи с сервером'), 'другая ошибка')

// Вопрос на устройстве, которому новый ключ прислали.
const offer = offerText({ name: 'MacBook', os: 'macOS', fingerprint: 'ab12cd34ef', received_ms: now }, 'в 12:00')
check(offer[0].includes('«MacBook»') && offer[0].includes('macOS') && offer[0].includes('ab12cd34ef') && offer[0].includes('в 12:00'), 'кто сменил ключ: имя, ОС, отпечаток, время')
check(offer[0].endsWith('Перейти на него?'), 'вопрос')
check(offer.some((s) => s.includes('синхронизация стоит')) && offer.some((s) => s.includes('не переходите')), 'что происходит и когда отказаться')
check(offerText({ name: ' ', os: '', fingerprint: 'ff', received_ms: 0 }, '')[0].includes('«Неизвестное устройство» (?'), 'без имени и ОС')

// Админ группы. `one` — сервер с одним админом, `many` — с несколькими.
const one = (...admins: string[]) => ({ supported: true, multi: false, admins })
const many = (...admins: string[]) => ({ supported: true, multi: true, admins })
const old = { supported: false, multi: false, admins: [] }
check(canManage(null, list) && canManage(old, list), 'не знаем или старый сервер — можно')
check(canManage(one(), list) && canManage(many(), list), 'группа без админа — можно')
check(canManage(one('aaaa'), list), 'это устройство — админ')
check(!canManage(one('bbbb'), list), 'админ — другое устройство')
check(canManage(many('bbbb', 'aaaa'), list) && !canManage(many('bbbb', 'cccc'), list), 'одно из админов — это устройство')
check(isAdmin(one('bbbb'), work) && !isAdmin(one('bbbb'), me), 'пометка админа')
check(isAdmin(many('aaaa', 'bbbb'), work) && isAdmin(many('aaaa', 'bbbb'), me) && !isAdmin(many('aaaa', 'bbbb'), phone), 'пометка у каждого админа')
check(rolesText(one('bbbb'), list).includes('«Рабочий»'), 'кто админ — по имени')
check(rolesText(one(), list).includes('кодом сервера администратора'), 'группа без админа')
check(rolesText(old, list) === '', 'старый сервер — без пояснения')
check(rolesText(many('bbbb', 'cccc'), list).includes('«Рабочий», «Старый»'), 'админы — по именам')
check(rolesText(many('aaaa'), list).includes('единственный'), 'единственный админ — это устройство')
check(rolesText(many('aaaa', 'bbbb'), list).includes('назначают или снимают'), 'одно из админов')

// Действия с ролью — только у админа и только на сервере с несколькими админами.
check(adminAction(many('aaaa'), list, work) === 'grant', 'назначить другое устройство')
check(adminAction(many('aaaa', 'bbbb'), list, work) === 'revoke', 'снять другого админа')
check(adminAction(many('aaaa', 'bbbb'), list, me) === 'revoke', 'снять себя')
check(adminAction(many('aaaa'), list, unknown) === null, 'без карточки не назначить')
check(adminAction(many('bbbb'), list, phone) === null, 'не админ — ничего')
check(adminAction(one('aaaa'), list, work) === null, 'старый сервер — только передача')
check(lastAdmin(many('aaaa'), list, me) && !lastAdmin(many('aaaa', 'bbbb'), list, me), 'последний админ')
check(lastAdmin(many('aaaa', 'ffff'), list, me), 'админ не из списка (не вошёл после смены ключа) не считается')
check(adminConfirmText(work, false).title.includes('«Рабочий»'), 'назначить — по имени')

// Админ, оставленный при смене ключа, но ещё не вошедший с новым: в списке его нет.
const ghost = 'ffff'.padEnd(64, '0')
check(pendingAdmins(many('aaaa', ghost), list).map((d) => d.fingerprint).join() === ghost.slice(0, 10), 'призрак — строкой с отпечатком')
check(pendingAdmins(many('aaaa', 'bbbb'), list).length === 0, 'все админы в списке — призраков нет')
check(pendingAdmins(one(ghost), list).length === 0 && pendingAdmins(many(ghost), []).length === 0, 'старый сервер или пустой список — не показываем')
check(rolesText(many('bbbb', ghost), list).includes(`устройство ${ghost.slice(0, 10)}, которое ещё не вошло`), 'призрак в пояснении — не обрывком id')
check(!rolesText(many('bbbb', ghost), list).includes(`«${ghost.slice(0, 10)}»`), 'без кавычек вокруг id')
check(adminConfirmText(pendingAdmins(many(ghost), list)[0], true, true).title.includes(ghost.slice(0, 10)), 'снять призрака — по отпечатку')
check(adminConfirmText(me, true).title === 'Перестать быть админом?', 'снять себя')
check(adminConfirmText(work, true).title.includes('Снять админа'), 'снять другого')

// Администратор сервера забирает роль, только если админ — другое устройство.
check(canClaimAdmin(one('bbbb'), list, true), 'админ другой — можно забрать')
check(canClaimAdmin(many('bbbb', 'cccc'), list, true), 'админы другие — можно стать ещё одним')
check(!canClaimAdmin(one('bbbb'), list, false), 'без ключа администратора — нет')
check(!canClaimAdmin(one('aaaa'), list, true) && !canClaimAdmin(many('bbbb', 'aaaa'), list, true), 'уже админ — нет')
check(!canClaimAdmin(one(), list, true), 'без админа роль приходит сама')
check(!canClaimAdmin(old, list, true) && !canClaimAdmin(null, list, true), 'старый сервер или нет связи — нет')

// Ключ администратора сервера при удалении: галочка у администратора,
// предупреждение у остальных. Оба текста говорят «если был».
check(adminKeyText(true).startsWith('Если у удалённого устройства был код'), 'у администратора — пояснение к галочке')
check(adminKeyText(false).startsWith('Внимание: если у удалённого устройства был ключ'), 'без ключа — предупреждение')
check(adminKeyText(false).includes('токен доступа'), 'и про токен доступа')

// Ожидание связи после перезапуска синхронизации.
{
  const noSleep = async () => {}
  const seq = (...kinds: string[]) => {
    let i = 0
    return async () => {
      const k = kinds[Math.min(i++, kinds.length - 1)]
      if (k === 'throw') throw new Error('нет ответа')
      return { kind: k }
    }
  }
  check(await waitOnline(seq('connecting', 'throw', 'online'), 30_000, 500, noSleep), 'дождались связи')
  check(!(await waitOnline(seq('offline'), 1_000, 500, noSleep)), 'связи нет — сдаёмся')
  let calls = 0
  await waitOnline(async () => (calls++, { kind: 'connecting' }), 1_000, 500, noSleep)
  check(calls === 3, 'опрос в пределах срока: 0, 500, 1000 мс')
}

console.log('devices: всё прошло')