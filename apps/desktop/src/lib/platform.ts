// Платформа окна и подписи клавиш под неё.
//
// Подписи в интерфейсе пишутся «как на Windows» («Ctrl+P», «Alt + ← →»): на маке
// `keys()` переводит их в принятые там значки (⌘P, ⌥← →). Обработчики клавиш
// и так понимают и Ctrl, и Cmd (`ctrlKey || metaKey`).

/** Окно открыто на macOS. В Node (тесты) — нет: там нет `document`. */
export const isMac =
  typeof document !== 'undefined' && typeof navigator !== 'undefined' && /Mac/i.test(navigator.platform)

/** «Ctrl + P» → «⌘P», «Alt+←» → «⌥←», «Shift + Delete» → «⇧Delete» (на маке). */
export function keys(s: string, mac = isMac): string {
  if (!mac) return s
  return s
    .replace(/\bCtrl ?\+ ?/g, '⌘')
    .replace(/\bCtrl (?=\S)/g, '⌘')
    .replace(/\bAlt ?\+ ?/g, '⌥')
    .replace(/\bShift ?\+ ?/g, '⇧')
}

/**
 * Имя устройства для показа. Вне мака убирает символы из области частного
 * использования Unicode: значок Apple (U+F8FF) в имени «MacBook Pro» есть
 * только в шрифтах macOS, на Windows и Linux он рисуется пустым квадратиком.
 */
export function deviceName(name: string, mac = isMac): string {
  return mac ? name.trim() : name.replace(/[-]/g, '').replace(/\s+/g, ' ').trim()
}
