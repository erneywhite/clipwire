// Сочетание клавиш: из события клавиатуры в формат плагина global-shortcut
// ("CommandOrControl+Shift+V") и обратно в подпись для людей ("Ctrl + Shift + V").

import { t } from './i18n.ts'

import { isMac } from './platform.ts'

/** Основная клавиша из KeyboardEvent.code; null — клавиша не годится для сочетания. */
function mainKey(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3)
  if (/^Digit[0-9]$/.test(code)) return code.slice(5)
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code
  const named: Record<string, string> = {
    Space: 'Space',
    Backquote: '`',
    Insert: 'Insert',
    Home: 'Home',
    End: 'End',
    PageUp: 'PageUp',
    PageDown: 'PageDown',
  }
  return named[code] ?? null
}

/** Возвращает сочетание или null, если нажатое сочетание не подходит. */
export function fromEvent(e: Pick<KeyboardEvent, 'code' | 'ctrlKey' | 'metaKey' | 'altKey' | 'shiftKey'>): string | null {
  const key = mainKey(e.code)
  if (!key) return null
  const parts: string[] = []
  if (isMac ? e.metaKey : e.ctrlKey) parts.push('CommandOrControl')
  if (isMac && e.ctrlKey) parts.push('Control')
  if (!isMac && e.metaKey) parts.push('Super')
  if (e.altKey) parts.push('Alt')
  if (e.shiftKey) parts.push('Shift')
  // Без модификаторов разрешаем только F-клавиши: иначе сочетание съест обычную печать.
  if (parts.length === 0 && !/^F\d+$/.test(key)) return null
  return [...parts, key].join('+')
}

/** Подпись для интерфейса. */
export function label(accel: string): string {
  return accel
    .split('+')
    .map((p) => {
      switch (p) {
        case 'CommandOrControl':
          return isMac ? '⌘' : 'Ctrl'
        case 'Control':
          return isMac ? '⌃' : 'Ctrl'
        case 'Super':
          return 'Win'
        case 'Alt':
          return isMac ? '⌥' : 'Alt'
        case 'Shift':
          return isMac ? '⇧' : 'Shift'
        case 'Space':
          return t('Пробел')
        default:
          return p
      }
    })
    .join(isMac ? '' : ' + ')
}
