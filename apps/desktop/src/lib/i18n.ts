// Перевод интерфейса. Ключ — сам русский текст: `t('Проверить обновления')`.
// Английский — в `i18n-en.ts`, сопоставлением «русский → английский».
// Почему так: строки переводятся по файлам независимо, без общего списка
// ключей, а проверка полноты (`i18n.test.ts`) находит каждую строку без
// перевода. Язык выбирается при запуске окна (`main.ts`); при смене языка
// окна перезагружаются, поэтому реактивность здесь не нужна.
import en from './i18n-en.ts'

export type Lang = 'ru' | 'en'
/** Выбор в настройках: «как в системе» или конкретный язык. */
export type LangPref = 'system' | Lang

let current: Lang = 'ru'

export function setLang(l: Lang) {
  current = l
  if (typeof document !== 'undefined') document.documentElement.lang = l
}

export function lang(): Lang {
  return current
}

/** Язык по настройке: «как в системе» — русский для ru-*, иначе английский. */
export function resolveLang(pref: LangPref, system: string): Lang {
  if (pref === 'ru' || pref === 'en') return pref
  return system.toLowerCase().startsWith('ru') ? 'ru' : 'en'
}

type Vars = Record<string, string | number>

function fill(s: string, vars?: Vars): string {
  if (!vars) return s
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m))
}

/** Перевести строку; `{имя}` в тексте подставляется из `vars`. */
export function t(ru: string, vars?: Vars): string {
  return fill(current === 'en' ? (en[ru] ?? ru) : ru, vars)
}

/**
 * Число с формой слова: `tn(5, ['файл', 'файла', 'файлов'], ['file', 'files'])`
 * → «5 файлов» / «5 files». `{n}` в форме — место числа (по умолчанию в начале).
 */
export function tn(n: number, ru: [string, string, string], enForms: [string, string]): string {
  const form = current === 'en' ? enForms[n === 1 ? 0 : 1] : ru[ruIndex(n)]
  return form.includes('{n}') ? form.replace('{n}', String(n)) : `${n} ${form}`
}

/** Только форма слова, без числа. */
export function form(n: number, ru: [string, string, string], enForms: [string, string]): string {
  return current === 'en' ? enForms[n === 1 ? 0 : 1] : ru[ruIndex(n)]
}

function ruIndex(n: number): 0 | 1 | 2 {
  const a = Math.abs(n) % 100
  const b = a % 10
  if (a > 10 && a < 20) return 2
  if (b > 1 && b < 5) return 1
  if (b === 1) return 0
  return 2
}
