// Подписи для элементов истории.

/** Русское множественное число: plural(5, ['символ', 'символа', 'символов']). */
export function plural(n: number, forms: [string, string, string]): string {
  const a = Math.abs(n) % 100
  const b = a % 10
  if (a > 10 && a < 20) return forms[2]
  if (b > 1 && b < 5) return forms[1]
  if (b === 1) return forms[0]
  return forms[2]
}

/** «сейчас», «5 мин», «3 ч», «вчера», «12 нояб.». */
export function ago(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000))
  if (s < 60) return 'сейчас'
  const m = Math.round(s / 60)
  if (m < 60) return `${m} мин`
  const h = Math.round(m / 60)
  const d = new Date(ms)
  const today = new Date(now)
  const yesterday = new Date(now - 86_400_000)
  if (d.toDateString() === today.toDateString()) return `${h} ч`
  if (d.toDateString() === yesterday.toDateString()) return 'вчера'
  return d.toLocaleDateString('ru-RU', { day: 'numeric', month: 'short' })
}

export function sizeLabel(chars: number, lines: number): string {
  if (lines > 1) return `${lines} ${plural(lines, ['строка', 'строки', 'строк'])}`
  return `${chars} ${plural(chars, ['символ', 'символа', 'символов'])}`
}

/** «340 КБ», «1,2 МБ». */
export function bytesLabel(n: number): string {
  if (n < 1024) return `${n} Б`
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} КБ`
  return `${(n / 1024 / 1024).toFixed(1).replace('.', ',')} МБ`
}

/** Похоже на код или команду — показывать моноширинным. */
export function looksLikeCode(text: string): boolean {
  if (/^\s*(\$ |docker |git |npm |cargo |sudo |cd |ls |kubectl )/m.test(text)) return true
  const symbols = (text.match(/[{}();=<>[\]|&]/g) ?? []).length
  return symbols / Math.max(1, text.length) > 0.04 || /^( {2,}|\t)/m.test(text)
}

/** Похоже на ссылку целиком. */
export function isUrl(text: string): boolean {
  return /^https?:\/\/\S+$/i.test(text.trim())
}
