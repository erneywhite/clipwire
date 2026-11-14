// Английский перевод: русский текст → английский. Ключи — ровно те строки,
// что стоят в `t('…')`, включая `{подстановки}`. Разбит на части по окнам,
// чтобы их можно было переводить независимо. Полноту проверяет
// `node src/lib/i18n.test.ts`.
import settings from './i18n-en/settings.ts'
import popup from './i18n-en/popup.ts'
import panels from './i18n-en/panels.ts'
import lib from './i18n-en/lib.ts'

const en: Record<string, string> = { ...settings, ...popup, ...panels, ...lib }

export default en
