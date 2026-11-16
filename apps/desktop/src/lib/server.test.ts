// Проверка логики формы «Сервер». Запуск: node src/lib/server.test.ts
// (Node 23.6+ сам убирает типы), как storage.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
import type { GroupInvite, ServerGroup, ServerSetting } from './api'

const { bytesText, collectChanges, tokenRefused, draftsFor, groupSettings, hoursText, parseInput, pickUnit, rangeError, resetDraft, settingText, switchUnit, toInput } =
  (await import('./server' + '.ts')) as typeof import('./server')
const { deleteGroupText, groupActivity, groupDevicesText, groupMuted, groupSizeText, groupStateText, sortGroups } = (await import('./server' + '.ts')) as typeof import('./server')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const MIB = 1024 * 1024
const GIB = 1024 * MIB

function setting(over: Partial<ServerSetting>): ServerSetting {
  return { key: 'max_devices', unit: 'count', min: 1, max: 1000, value: 64, default: 64, stored: false, env: 'CLIPWIRE_MAX_DEVICES', ...over }
}

const devices = setting({})
const file = setting({ key: 'max_file_bytes', unit: 'bytes', min: 1024, max: 1024 * GIB, value: 100 * MIB, default: 100 * MIB, env: 'CLIPWIRE_MAX_FILE_BYTES' })
const ttl = setting({ key: 'ttl_hours', unit: 'hours', min: 1, max: 24 * 366 * 10, value: 48, default: 24, stored: true, env: 'CLIPWIRE_TTL_HOURS' })
const item = setting({ key: 'max_item_bytes', unit: 'bytes', min: 1024, max: 15 * MIB, value: 1024, default: 8 * MIB, env: 'CLIPWIRE_MAX_ITEM_BYTES' })

// Единицы и подписи.
check(pickUnit(100 * MIB) === 'MB' && pickUnit(2 * GIB) === 'GB', 'единица по размеру')
check(bytesText(100 * MIB) === '100 МБ', '100 МБ без дробной части')
check(bytesText(1.5 * GIB) === '1,5 ГБ', '1,5 ГБ с запятой')
check(bytesText(64 * 1024) === '64 КБ', 'килобайты')
check(hoursText(24) === '24 часа (1 день)', 'сутки')
check(hoursText(5) === '5 часов', 'часы без дней')
check(toInput(file, 100 * MIB) === '100', 'поле в МБ')
check(toInput(file, 2 * GIB, 'GB') === '2', 'поле в ГБ')
check(toInput(file, 2 * GIB, 'MB') === '2048', 'тот же размер в МБ')
check(settingText('max_devices').title === 'Устройств в группе', 'подпись настройки')
check(settingText('future_x').title === 'future_x', 'незнакомая настройка — по имени')

// Разбор ввода.
const p = parseInput(file, '1,5', 'GB')
check('value' in p && p.value === 1.5 * GIB, 'дробные гигабайты с запятой')
check('error' in parseInput(devices, '2.5', 'MB'), 'штуки только целые')
check('error' in parseInput(devices, '', 'MB'), 'пусто — ошибка')
check('error' in parseInput(devices, '-3', 'MB'), 'минус — ошибка')
check('error' in parseInput(ttl, '1e3', 'MB'), 'экспонента не принимается')

// Пределы.
check(rangeError(devices, 1000) === '' && rangeError(devices, 1) === '', 'границы включительно')
check(rangeError(devices, 0) === 'Допустимо от 1 до 1000', 'ниже предела')
check(rangeError(item, 16 * MIB) === 'Допустимо от 1 КБ до 15 МБ', 'предел в размерах')

// Черновик и изменения.
const list = [devices, file, ttl, item]
const drafts = draftsFor(list)
check(collectChanges(list, drafts).changes.length === 0, 'ничего не меняли — нечего отправлять')
// Поле 1 КБ в МБ округлено, но нетронутое не уходит.
check(drafts.max_item_bytes.text === '0,001', 'маленький размер с тремя знаками')
drafts.max_devices = { ...drafts.max_devices, text: '10' }
drafts.max_file_bytes = { ...drafts.max_file_bytes, text: '1' , unit: 'GB' }
let r = collectChanges(list, drafts)
check(JSON.stringify(r.changes) === JSON.stringify([{ key: 'max_devices', value: 10 }, { key: 'max_file_bytes', value: GIB }]), 'изменённые значения в байтах')
drafts.max_devices = { ...drafts.max_devices, text: '5000' }
r = collectChanges(list, drafts)
check(r.errors.max_devices === 'Допустимо от 1 до 1000', 'ошибка по полю')
check(!r.changes.some((c) => c.key === 'max_devices'), 'ошибочное не отправляется')

// Сброс: только для заданного из клиента, в поле — значение по умолчанию.
const reset = resetDraft(ttl)
check(reset.reset && reset.text === '24', 'сброс показывает умолчание')
r = collectChanges([ttl], { ttl_hours: reset })
check(JSON.stringify(r.changes) === JSON.stringify([{ key: 'ttl_hours', value: null }]), 'сброс уходит как null')
check(collectChanges([devices], { max_devices: resetDraft(devices) }).changes.length === 0, 'сброс незаданного — нечего отправлять')

// Смена единицы не меняет значение.
const gb = switchUnit(file, drafts.max_file_bytes, 'MB')
check(gb.text === '1024' && gb.unit === 'MB', 'ГБ → МБ')
const exact = switchUnit(item, draftsFor([item]).max_item_bytes, 'GB')
check(collectChanges([item], { max_item_bytes: switchUnit(item, exact, 'MB') }).changes.length === 0, 'нетронутое поле после смены единиц не уходит')

// Группы формы.
const groups = groupSettings([...list, setting({ key: 'future_x' })])
check(groups[0].items[0].key === 'ttl_hours', 'история первой')
check(groups.at(-1)!.items[0].key === 'future_x', 'незнакомое — в конце')

// Отказ по токену доступа узнаётся и по тексту сервера, и по тексту агента.
check(tokenRefused('сервер отказал: токен сервера сменён: нужен новый код приглашения'), 'смена токена')
check(tokenRefused('сервер отказал: неверный код сервера'), 'неверный токен')
check(tokenRefused('The relay denied access: bad token'), 'relay по-английски')
check(!tokenRefused('таймаут подключения'), 'таймаут — не отказ по токену')

// Карточка «Группы».
function group(over: Partial<ServerGroup>): ServerGroup {
  return {
    id: 'ab'.repeat(32), fingerprint: 'ababababab', name: '', mine: false, created_ms: 1, last_seen_ms: 0, devices: 0, online: 0,
    items: 0, item_bytes: 0, files: 0, file_bytes: 0, pins: 0, pin_bytes: 0, has_admin: true, state: 'live', days_left: null, ...over,
  }
}
const noon = new Date(2026, 10, 16, 12, 0).getTime()
check(groupActivity(noon - 3600_000, noon) === 'сегодня', 'час назад — сегодня')
check(groupActivity(new Date(2026, 10, 15, 23, 50).getTime(), noon) === 'вчера', 'вчера поздно вечером')
check(groupActivity(new Date(2026, 10, 13, 9, 0).getTime(), noon) === '3 дня назад', 'три дня назад')
check(groupActivity(new Date(2026, 10, 11, 9, 0).getTime(), noon) === '5 дней назад', 'пять дней назад')
check(groupActivity(0, noon) === 'устройства не входили', 'нет активности')
check(groupStateText(group({})) === '', 'у живой пояснения нет')
check(groupStateText(group({ state: 'rotated', days_left: 2 })) === 'ключ сменён, новый ключ ждёт устройства ещё 2 дня', 'дни до надгробия')
check(groupStateText(group({ state: 'rotated', days_left: 0 })) === 'ключ сменён, осталось надгробие', 'надгробие')
check(groupStateText(group({ state: 'idle' })) === 'заброшена, данные удалены', 'заброшенная')
check(groupStateText(group({ state: 'pending' })) === 'смена ключа не завершена', 'незавершённая')
check(groupMuted(group({})) && groupMuted(group({ state: 'idle', devices: 2 })), 'пустая и заброшенная приглушены')
check(!groupMuted(group({ devices: 1 })) && !groupMuted(group({ state: 'pending', devices: 1 })), 'живая с устройством — нет')
check(groupDevicesText(group({ devices: 3, online: 1 })) === '3 устройства, в сети 1', 'устройства и в сети')
check(groupDevicesText(group({})) === 'без устройств', 'без устройств')
check(groupSizeText(group({ item_bytes: 2 * MIB, pin_bytes: 12 * 1024 })) === 'история 2 МБ · закрепы 12 КБ', 'только непустое')
check(groupSizeText(group({})) === 'на сервере пусто', 'пусто')
const sorted = sortGroups([group({ id: 'a', last_seen_ms: 5 }), group({ id: 'b', last_seen_ms: 9 }), group({ id: 'c', last_seen_ms: 5, created_ms: 7 })])
check(sorted.map((g) => g.id).join('') === 'bca', 'по активности, затем по созданию')
const confirm = deleteGroupText(group({ items: 5, files: 1, pins: 2, devices: 3 }))
check(confirm.length === 3, 'что удалится, кто отключится, необратимо')
check(confirm[0].includes('5 элементов') && confirm[0].includes('1 файл') && confirm[0].includes('2 закрепа'), confirm[0])
check(confirm[1].startsWith('3 устройства группы отключатся'), confirm[1])
check(confirm[2].includes('необратимо'), 'необратимо')
check(deleteGroupText(group({})).length === 2, 'без устройств — без абзаца об отключении')

// Имена групп.
const { GROUP_NAME_MAX, groupLabel, groupNameError } = (await import('./server' + '.ts')) as typeof import('./server')
check(groupLabel(group({})) === 'ababababab', 'без имени — отпечаток')
check(groupLabel(group({ name: 'Семья' })) === '«Семья» (ababababab)', 'с именем — имя и отпечаток')
check(deleteGroupText(group({ name: 'Семья' }))[0].includes('«Семья» (ababababab)'), 'имя в подтверждении удаления')
check(groupNameError('  Семья ') === '', 'пробелы по краям можно')
check(groupNameError('') === '' && groupNameError('   ') === '', 'пустое — снять имя')
check(groupNameError('я'.repeat(GROUP_NAME_MAX)) === '', 'ровно предел')
check([0x200b, 0x200e, 0x2028, 0xfeff, 0xad].every((c) => groupNameError('а' + String.fromCharCode(c) + 'б') !== ''), 'невидимые символы — ошибка')
check(groupNameError('я'.repeat(GROUP_NAME_MAX + 1)) !== '', 'длиннее предела')
check(groupNameError('😀'.repeat(GROUP_NAME_MAX)) === '', 'эмодзи считаются символами, а не половинками')
check(groupNameError('а\nб') !== '' && groupNameError('а\u0007б') !== '', 'управляющие символы')
check(groupNameError('‮анам') !== '', 'символы направления текста')

// Коды создания группы.
const { creationKind, inviteIssuedText, inviteLabel, inviteState, inviteStateText, inviteTtlText, sortInvites, timeLeftText, DEFAULT_INVITE_TTL, INVITE_TTLS } =
  (await import('./server' + '.ts')) as typeof import('./server')
const HOUR = 3_600_000
function invite(over: Partial<GroupInvite>): GroupInvite {
  return { id: 1, label: 'Маше', created_ms: noon - HOUR, expires_ms: noon + 7 * 24 * HOUR, used_ms: null, used_group: null, revoked_ms: null, ...over }
}
check(inviteState(invite({}), noon) === 'active', 'действует')
check(inviteState(invite({ expires_ms: noon }), noon) === 'expired', 'срок вышел ровно сейчас')
check(inviteState(invite({ revoked_ms: noon - 1 }), noon) === 'revoked', 'отозван')
// Использован важнее истёкшего: использованный код потом и истекает.
check(inviteState(invite({ used_ms: noon - 1, expires_ms: noon - 1 }), noon) === 'used', 'использован')
check(timeLeftText(noon + 7 * 24 * HOUR, noon) === '7 дней', 'дни')
check(timeLeftText(noon + 5 * HOUR + 1, noon) === '5 часов', 'часы')
check(timeLeftText(noon + HOUR - 1, noon) === 'меньше часа', 'меньше часа')
check(inviteStateText(invite({}), noon) === 'действует ещё 7 дней', inviteStateText(invite({}), noon))
check(inviteStateText(invite({ used_ms: noon - 1, used_group: 'abcdef0123' }), noon) === 'использован сегодня, группа abcdef0123', 'использован с группой')
check(inviteStateText(invite({ used_ms: noon - 1 }), noon) === 'использован сегодня', 'использован без группы')
check(inviteStateText(invite({ expires_ms: noon - 24 * HOUR }), noon) === 'истёк вчера', 'истёк')
check(inviteStateText(invite({ revoked_ms: noon - 3 * 24 * HOUR }), noon) === 'отозван 3 дня назад', 'отозван')
check(inviteIssuedText(invite({}), noon) === 'выпущен сегодня', 'выпущен')
check(inviteLabel(invite({ label: '  ' })) === 'без подписи' && inviteLabel(invite({})) === 'Маше', 'подпись')
const inv = sortInvites([invite({ id: 1, created_ms: 1, used_ms: 5 }), invite({ id: 2, created_ms: 2 }), invite({ id: 3, created_ms: 3, revoked_ms: 4 }), invite({ id: 4, created_ms: 1 })], noon)
check(inv.map((i) => i.id).join('') === '2431', 'действующие первыми, внутри новые первыми')
check(INVITE_TTLS.includes(DEFAULT_INVITE_TTL) && DEFAULT_INVITE_TTL === 168, 'по умолчанию 7 дней')
check(inviteTtlText(168) === '7 дней' && inviteTtlText(24) === '1 день' && inviteTtlText(5) === '5 часов', 'срок кнопки')
check(creationKind(' CWSERVER2-abc') === 'admin', 'код администратора')
check(creationKind('cwgroup1-abc\n') === 'invite', 'код создания группы')
check(creationKind('clipwire1-abc') === 'unknown' && creationKind('') === 'unknown', 'остальное')

console.log('server: всё прошло')
