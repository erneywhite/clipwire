// Логика формы раздела «Сервер»: подписи настроек, единицы, разбор ввода,
// проверка пределов до отправки и сброс к значению по умолчанию. Без Svelte,
// чтобы проверялось в Node (`server.test.ts`).
import type { ServerSetting, ServerSummary, SettingChange } from './api'
import { t, tn, lang } from './i18n.ts'

const KIB = 1024
const MIB = 1024 * KIB
const GIB = 1024 * MIB

/** В чём вводится размер. */
export type ByteUnit = 'MB' | 'GB'

export const BYTE_UNITS: ByteUnit[] = ['MB', 'GB']

export function unitSize(u: ByteUnit): number {
  return u === 'GB' ? GIB : MIB
}

/** Подпись единицы: «МБ», «ГБ». */
export function unitLabel(u: ByteUnit): string {
  return u === 'GB' ? t('ГБ') : t('МБ')
}

/** Единица для показа размера: от гигабайта — ГБ, меньше — МБ. */
export function pickUnit(bytes: number): ByteUnit {
  return bytes >= GIB ? 'GB' : 'MB'
}

/** Название и пояснение настройки для формы. */
export function settingText(key: string): { title: string; hint: string } {
  switch (key) {
    case 'history_len':
      return {
        title: t('История группы: сколько элементов'),
        hint: t('Сколько последних элементов сервер хранит для каждой группы. Их получают устройства, которые были не в сети.'),
      }
    case 'ttl_hours':
      return {
        title: t('История группы: сколько часов'),
        hint: t('Через сколько часов элемент истории удаляется с сервера. Локальная история на устройствах не трогается.'),
      }
    case 'max_item_bytes':
      return {
        title: t('Предел одного элемента'),
        hint: t('Текст, картинка или карточка файлов одним элементом, в зашифрованном виде. Больше 15 МБ нельзя: элемент идёт одним сообщением.'),
      }
    case 'max_file_bytes':
      return { title: t('Предел одного файла'), hint: t('Файлы крупнее не загружаются на сервер, их берут напрямую с устройства-источника.') }
    case 'blob_quota':
      return { title: t('Место под файлы одной группы'), hint: t('Сколько места на диске сервера может занять каждая группа. Файлы закреплённых записей тоже считаются здесь.') }
    case 'blob_total_quota':
      return { title: t('Место под файлы всего сервера'), hint: t('Предел для всех групп вместе.') }
    case 'history_total_bytes':
      return { title: t('Общий объём истории'), hint: t('Предел истории элементов всех групп вместе. Лишнее вытесняется у самой большой группы.') }
    case 'max_groups':
      return { title: t('Число групп'), hint: t('Сколько групп можно создать на сервере. Группы без устройств удаляются сами через 90 дней.') }
    case 'max_devices':
      return { title: t('Устройств в группе'), hint: t('Новое устройство сверх предела не войдёт, уже известные входят как раньше.') }
    case 'max_device_conns':
      return { title: t('Соединений с одного устройства'), hint: t('Сколько одновременных соединений сервер принимает от одного устройства.') }
    case 'max_reserves':
      return { title: t('Резервы под файлы'), hint: t('Сколько файлов одно устройство может держать зарезервированными, пока не загрузило ни одной части.') }
    case 'pin_max':
      return { title: t('Закрепы: сколько записей'), hint: t('Сколько закреплённых записей группа может держать на сервере.') }
    case 'pin_bytes':
      return { title: t('Закрепы: сколько места'), hint: t('Место под закреплённые записи группы. Файлы закреплённых записей считаются в месте под файлы группы.') }
    default:
      // Настройка новее приложения: показываем имя как есть.
      return { title: key, hint: '' }
  }
}

/** Группы полей формы, в порядке показа. */
export const SETTING_GROUPS: { title: () => string; keys: string[] }[] = [
  { title: () => t('История группы'), keys: ['history_len', 'ttl_hours', 'history_total_bytes'] },
  { title: () => t('Размеры'), keys: ['max_item_bytes', 'max_file_bytes'] },
  { title: () => t('Место под файлы'), keys: ['blob_quota', 'blob_total_quota', 'max_reserves'] },
  { title: () => t('Закрепы'), keys: ['pin_max', 'pin_bytes'] },
  { title: () => t('Группы и устройства'), keys: ['max_groups', 'max_devices', 'max_device_conns'] },
]

/** Настройки по группам формы; незнакомые серверные — в конце отдельной группой. */
export function groupSettings(list: ServerSetting[]): { title: string; items: ServerSetting[] }[] {
  const known = new Set(SETTING_GROUPS.flatMap((g) => g.keys))
  const out = SETTING_GROUPS.map((g) => ({
    title: g.title(),
    items: g.keys.map((k) => list.find((s) => s.key === k)).filter((s): s is ServerSetting => !!s),
  })).filter((g) => g.items.length > 0)
  const other = list.filter((s) => !known.has(s.key))
  if (other.length) out.push({ title: t('Другое'), items: other })
  return out
}

/** Число для поля ввода: до трёх знаков после запятой, без лишних нулей. */
function num(x: number): string {
  const s = String(Math.round(x * 1000) / 1000)
  return lang() === 'en' ? s : s.replace('.', ',')
}

/** Размер точно: «100 МБ», «1,5 ГБ», «64 КБ». */
export function bytesText(n: number): string {
  if (n >= GIB) return t('{n} ГБ', { n: num(n / GIB) })
  if (n >= MIB) return t('{n} МБ', { n: num(n / MIB) })
  if (n >= KIB) return t('{n} КБ', { n: num(n / KIB) })
  return t('{n} Б', { n })
}

/** Часы с днями: «24 часа (1 день)». */
export function hoursText(h: number): string {
  const hours = tn(h, ['час', 'часа', 'часов'], ['hour', 'hours'])
  if (h >= 24 && h % 24 === 0) return `${hours} (${tn(h / 24, ['день', 'дня', 'дней'], ['day', 'days'])})`
  return hours
}

/** Значение настройки по-человечески. */
export function valueText(s: Pick<ServerSetting, 'unit'>, v: number): string {
  if (s.unit === 'bytes') return bytesText(v)
  if (s.unit === 'hours') return hoursText(v)
  return String(v)
}

/** Текст поля ввода для значения `v` в единице `unit` (у размеров). */
export function toInput(s: Pick<ServerSetting, 'unit'>, v: number, unit: ByteUnit = pickUnit(v)): string {
  return s.unit === 'bytes' ? num(v / unitSize(unit)) : String(v)
}

/**
 * Разобрать ввод: число в байтах (часах, штуках) или текст ошибки. Размер
 * можно с дробной частью (запятая или точка), остальное — целое.
 */
export function parseInput(s: Pick<ServerSetting, 'unit'>, text: string, unit: ByteUnit): { value: number } | { error: string } {
  const raw = text.trim().replace(',', '.')
  if (!raw) return { error: t('Введите число') }
  if (!/^\d+(\.\d+)?$/.test(raw)) return { error: t('Нужно число не меньше нуля') }
  const x = Number(raw)
  if (s.unit === 'bytes') return { value: Math.round(x * unitSize(unit)) }
  if (!Number.isInteger(x)) return { error: t('Нужно целое число') }
  return { value: x }
}

/** Проверка пределов сервера (как `check_setting`): пусто — всё в порядке. */
export function rangeError(s: ServerSetting, v: number): string {
  if (v >= s.min && v <= s.max) return ''
  return t('Допустимо от {min} до {max}', { min: valueText(s, s.min), max: valueText(s, s.max) })
}

/** Правка одного поля формы. */
export interface Draft {
  text: string
  unit: ByteUnit
  /** Сбросить к значению по умолчанию (настройка лежит в базе сервера). */
  reset: boolean
}

/** Черновик формы по ответу сервера. */
export function draftsFor(list: ServerSetting[]): Record<string, Draft> {
  return Object.fromEntries(
    list.map((s) => {
      const unit = pickUnit(s.value)
      return [s.key, { text: toInput(s, s.value, unit), unit, reset: false }]
    }),
  )
}

/** «Сбросить»: в поле — значение по умолчанию, на сервер уйдёт сброс. */
export function resetDraft(s: ServerSetting): Draft {
  const unit = pickUnit(s.default)
  return { text: toInput(s, s.default, unit), unit, reset: true }
}

/** Другая единица у поля размера: число пересчитывается, значение то же. */
export function switchUnit(s: ServerSetting, d: Draft, unit: ByteUnit): Draft {
  // Нетронутое поле и сброс пересчитываются из точного значения, а не из
  // округлённого текста.
  const exact = d.reset ? s.default : d.text === toInput(s, s.value, d.unit) ? s.value : null
  const p = exact !== null ? { value: exact } : parseInput(s, d.text, d.unit)
  return { ...d, unit, text: 'value' in p ? toInput(s, p.value, unit) : d.text }
}

/**
 * Что отправить серверу: изменённые значения и сбросы. Ошибки — по полям;
 * пока они есть, отправлять нельзя.
 */
export function collectChanges(list: ServerSetting[], drafts: Record<string, Draft>): { changes: SettingChange[]; errors: Record<string, string> } {
  const changes: SettingChange[] = []
  const errors: Record<string, string> = {}
  for (const s of list) {
    const d = drafts[s.key]
    if (!d) continue
    if (d.reset) {
      if (s.stored) changes.push({ key: s.key, value: null })
      continue
    }
    // Поле не трогали: показанное число могло быть округлено, отправлять его незачем.
    if (d.text === toInput(s, s.value, d.unit)) continue
    const p = parseInput(s, d.text, d.unit)
    if ('error' in p) {
      errors[s.key] = p.error
      continue
    }
    const bad = rangeError(s, p.value)
    if (bad) {
      errors[s.key] = bad
      continue
    }
    if (p.value !== s.value) changes.push({ key: s.key, value: p.value })
  }
  return { changes, errors }
}

/** Сводка сервера строками «название — значение». */
export function summaryRows(s: ServerSummary): [string, string][] {
  return [
    [t('Групп'), String(s.groups)],
    [t('Устройств'), String(s.devices)],
    [t('История элементов'), bytesText(s.bytes_items)],
    [t('Файлы'), bytesText(s.bytes_files)],
    [t('Закрепы'), bytesText(s.bytes_pins)],
    [t('Версия сервера'), s.version],
  ]
}
