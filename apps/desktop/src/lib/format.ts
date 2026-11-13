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
  if (n < 1024 ** 3) return `${(n / 1024 / 1024).toFixed(1).replace('.', ',')} МБ`
  return `${(n / 1024 ** 3).toFixed(1).replace('.', ',')} ГБ`
}

/** Подпись для набора файлов: «Отчёт.pdf», «3 файла: a.pdf, b.png, c.txt». */
export function filesLabel(files: { name: string }[]): string {
  if (files.length === 1) return files[0].name
  const n = files.length
  return `${n} ${plural(n, ['файл', 'файла', 'файлов'])}: ${files.map((f) => f.name).join(', ')}`
}

/**
 * Мета для файлов: размер, у папки — «папка» и размер содержимого, у картинки —
 * пиксели. У папок из старых записей размер не считался (0): тогда просто «папка».
 */
export function filesMeta(files: { size: number; is_dir: boolean }[], bytes: number, w: number, h: number): string {
  if (files.length === 1 && files[0].is_dir) return bytes > 0 ? `папка · ${bytesLabel(bytes)}` : 'папка'
  const parts = []
  if (w && h) parts.push(`${w}×${h}`)
  parts.push(bytesLabel(bytes))
  return parts.join(' · ')
}

/** Похоже на код или команду — показывать моноширинным. */
export function looksLikeCode(text: string): boolean {
  if (/^\s*(\$ |docker |git |npm |cargo |sudo |cd |ls |kubectl )/m.test(text)) return true
  const symbols = (text.match(/[{}();=<>[\]|&]/g) ?? []).length
  return symbols / Math.max(1, text.length) > 0.04 || /^( {2,}|\t)/m.test(text)
}

/**
 * Документ для предпросмотра HTML из буфера. Показывается во фрейме с пустым
 * sandbox (скрипты, формы и переходы запрещены), а CSP внутри не даёт грузить
 * ничего извне: чужое письмо или страница не «позвонит домой» при просмотре.
 * Картинки, вложенные прямо в HTML (data:), показываются.
 */
export function richDoc(html: string): string {
  const csp = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:"
  const base =
    'html,body{margin:0;background:#fff;color:#1d1f23;font:14px/1.45 system-ui,sans-serif;overflow-wrap:anywhere}' +
    'body{padding:10px 12px}img{max-width:100%;height:auto}table{border-collapse:collapse;max-width:100%}pre{white-space:pre-wrap}' +
    'button{all:unset}.cw-alt{color:#6b675f}'
  return `<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${csp}"><style>${base}</style></head><body>${tameImages(html)}</body></html>`
}

/**
 * Внешние картинки в предпросмотре всё равно не загрузятся, а значок битой
 * картинки выглядит как поломка. Меняем их на подпись `alt` (у смайликов это
 * их название) или убираем. DOMParser строит инертный документ: ничего не
 * загружает и не исполняет. Во вставку уходит исходный HTML.
 */
export function tameImages(html: string): string {
  const doc = new DOMParser().parseFromString(html, 'text/html')
  for (const img of Array.from(doc.querySelectorAll('img'))) {
    const src = img.getAttribute('src') ?? ''
    if (src.startsWith('data:')) continue
    const alt = (img.getAttribute('alt') ?? '').trim()
    if (alt) {
      const span = doc.createElement('span')
      span.className = 'cw-alt'
      span.textContent = alt
      img.replaceWith(span)
    } else {
      img.remove()
    }
  }
  return doc.body.innerHTML
}

/** Похоже на ссылку целиком. */
export function isUrl(text: string): boolean {
  return /^https?:\/\/\S+$/i.test(text.trim())
}
