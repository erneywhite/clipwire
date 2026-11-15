// Отложенный вызов: несколько вызовов подряд дают один, через `ms` после
// последнего. Нужен строке поиска: редкое слово ищется полным проходом по
// истории, и запрос на каждую нажатую клавишу только нагружает базу.

export interface Debounced {
  /** Запланировать вызов заново: отсчёт начинается сначала. */
  (): void
  /** Забыть ждущий вызов. */
  cancel(): void
  readonly pending: boolean
}

export function debounce(fn: () => void, ms: number): Debounced {
  let timer: ReturnType<typeof setTimeout> | null = null
  const run = () => {
    timer = null
    fn()
  }
  const d = (() => {
    if (timer !== null) clearTimeout(timer)
    timer = setTimeout(run, ms)
  }) as Debounced
  d.cancel = () => {
    if (timer !== null) clearTimeout(timer)
    timer = null
  }
  Object.defineProperty(d, 'pending', { get: () => timer !== null })
  return d
}
