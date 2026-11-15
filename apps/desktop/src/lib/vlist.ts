// Виртуальная прокрутка списка: в DOM только видимые строки и запас вокруг.
// Строки разной высоты (текст, миниатюры до 132 px, файлы с полосой
// прогресса), поэтому высоты меряются у отрисованных строк и запоминаются,
// а у ещё не виденных оцениваются по среднему для такого же вида строк.
// Здесь только расчёт, без DOM: его проверяет vlist.test.ts.

/** Полуоткрытый отрезок индексов [start, end). */
export interface Range {
  start: number
  end: number
}

/** Вид строки для оценки высоты: у строк с миниатюрой она заметно больше. */
export type RowShape = 'row' | 'thumb'

/** Оценки до первого замера: две строки текста с метой и строка с миниатюрой. */
export const DEFAULT_HEIGHT: Record<RowShape, number> = { row: 56, thumb: 160 }

/**
 * Запомненные высоты строк по id записи. Пока строку не мерили, её высота —
 * средняя по замеренным строкам того же вида, а до первого замера — DEFAULT_HEIGHT.
 */
export class HeightCache {
  private known = new Map<string, { h: number; shape: RowShape }>()
  private sum: Record<RowShape, number> = { row: 0, thumb: 0 }
  private count: Record<RowShape, number> = { row: 0, thumb: 0 }
  // Ширина строк при замерах; 0 — ещё не мерили.
  private width = 0

  /** Высота строки: замер или оценка. */
  get(id: string, shape: RowShape): number {
    const k = this.known.get(id)
    if (k) return k.h
    return this.count[shape] ? this.sum[shape] / this.count[shape] : DEFAULT_HEIGHT[shape]
  }

  /** Есть ли замер у строки. */
  has(id: string): boolean {
    return this.known.has(id)
  }

  /** Записать замер. Возвращает, на сколько изменилась высота, которую отдавал get(). */
  set(id: string, shape: RowShape, h: number): number {
    const before = this.get(id, shape)
    const old = this.known.get(id)
    if (old) {
      this.sum[old.shape] -= old.h
      this.count[old.shape]--
    }
    this.known.set(id, { h, shape })
    this.sum[shape] += h
    this.count[shape]++
    return h - before
  }

  /** Забыть строки, которых больше нет в списке: кэш не растёт вместе с историей. */
  keepOnly(ids: Set<string>) {
    for (const [id, k] of this.known) {
      if (ids.has(id)) continue
      this.known.delete(id)
      this.sum[k.shape] -= k.h
      this.count[k.shape]--
    }
  }

  /**
   * Ширина строк, под которую сделаны замеры. Строки стали другой ширины —
   * текст переносится иначе, старые замеры не годятся: кэш очищается, и
   * возвращается true. Нулевая ширина (окно спрятано, строки не разложены)
   * за замер не считается. Сравнивается настоящая ширина строк, а не
   * настройка: ширина окна зажата в пределы, и разные настройки дают одну ширину.
   */
  setWidth(w: number): boolean {
    if (!(w > 0) || Math.abs(w - this.width) < 0.5) return false
    const had = this.width > 0
    this.width = w
    if (!had) return false
    this.clear()
    return true
  }

  clear() {
    this.known.clear()
    this.sum = { row: 0, thumb: 0 }
    this.count = { row: 0, thumb: 0 }
  }

  get size(): number {
    return this.known.size
  }
}

/** Верх каждой строки: offs[i] — начало строки i, offs[n] — высота всего списка. */
export function offsets(n: number, heightAt: (i: number) => number): Float64Array {
  const offs = new Float64Array(n + 1)
  for (let i = 0; i < n; i++) offs[i + 1] = offs[i] + heightAt(i)
  return offs
}

/** Строка, в которую попадает координата y. Для пустого списка — 0. */
export function rowAt(offs: Float64Array, y: number): number {
  const n = offs.length - 1
  if (n <= 0) return 0
  // Последняя строка, у которой верх не ниже y.
  let lo = 0
  let hi = n - 1
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1
    if (offs[mid] <= y) lo = mid
    else hi = mid - 1
  }
  return lo
}

/** Какие строки держать в DOM: видимые и по `overscan` строк сверху и снизу. */
export function visibleRange(offs: Float64Array, scrollTop: number, viewport: number, overscan: number): Range {
  const n = offs.length - 1
  if (n <= 0) return { start: 0, end: 0 }
  const first = rowAt(offs, Math.max(0, scrollTop))
  // Нижний край окна сам по себе не входит: строка, которая начинается ровно
  // на нём, ещё не видна.
  const last = rowAt(offs, Math.max(0, scrollTop + Math.max(viewport, 0) - 0.5))
  return { start: Math.max(0, first - overscan), end: Math.min(n, last + 1 + overscan) }
}

/**
 * Прокрутка, при которой строка i видна целиком, как scrollIntoView({block:'nearest'}):
 * если она уже видна, прокрутка не меняется; выше окна — строка встаёт к верху,
 * ниже — к низу. Строка выше окна прижимается к верху.
 */
export function scrollFor(offs: Float64Array, i: number, scrollTop: number, viewport: number): number {
  const n = offs.length - 1
  if (n <= 0 || i < 0 || i >= n) return scrollTop
  const top = offs[i]
  const bottom = offs[i + 1]
  if (top < scrollTop) return top
  if (bottom > scrollTop + viewport) return Math.min(top, bottom - viewport)
  return scrollTop
}

/**
 * Прокрутка после смены высот: строка, которая была вверху окна, остаётся на
 * том же месте, сколько бы ни выросли или сжались строки над ней.
 */
export function anchored(before: Float64Array, after: Float64Array, scrollTop: number): number {
  if (before.length !== after.length || before.length < 2) return scrollTop
  const i = rowAt(before, scrollTop)
  return Math.max(0, after[i] + (scrollTop - before[i]))
}

/**
 * Прокрутка после смены высот, когда положение уже выставлено по настоящим
 * строкам в DOM: прыжок к выбранной строке (scrollIntoView) или к далёкому
 * месту списка. Отрисованные строки уже стоят на своих настоящих высотах,
 * сдвинется только поле сверху (offs[start]: оценки строк выше первой
 * отрисованной). На него и поправляется прокрутка, чтобы видимое не уехало.
 * `start` — первая строка в DOM.
 */
export function keptInDom(before: Float64Array, after: Float64Array, start: number, scrollTop: number): number {
  if (before.length !== after.length || start < 0 || start >= before.length) return scrollTop
  return Math.max(0, scrollTop + after[start] - before[start])
}

/**
 * Прокрутка после замены списка (сверху пришли новые записи, какие-то
 * удалились): верхняя видимая запись остаётся на месте, если она есть и в
 * новом списке. В самом верху прокрутка не трогается: новая запись должна
 * появиться на виду.
 */
export function anchoredById(
  before: Float64Array,
  idsBefore: readonly string[],
  after: Float64Array,
  idsAfter: readonly string[],
  scrollTop: number,
): number {
  if (scrollTop <= 0 || idsBefore.length === 0) return scrollTop
  const i = rowAt(before, scrollTop)
  const j = idsAfter.indexOf(idsBefore[i])
  if (j < 0) return scrollTop
  return Math.max(0, after[j] + (scrollTop - before[i]))
}

/**
 * Сколько записей перечитывать при изменении истории. Не всё подгруженное, а
 * до видимого конца (или выбранной строки) с запасом, округлённым до целой
 * страницы: после перечитывания до конца остаётся не меньше `ahead` записей,
 * и следующая страница сразу не запрашивается. Не меньше `page` и не больше,
 * чем было подгружено.
 */
export function reloadCount(loaded: number, needed: number, page: number, ahead: number): number {
  const want = Math.ceil((needed + ahead) / page) * page
  return Math.max(page, Math.min(loaded, want))
}

/**
 * Два отрезка строк не пересекаются: после перехода в DOM не осталось ни
 * одной прежней строки (прыжок ползунком, переход по кругу с первой записи на
 * последнюю). Пустой отрезок ни с чем не пересекается.
 */
export function disjoint(a: Range, b: Range): boolean {
  return b.start >= a.end || b.end <= a.start
}

/** Сколько записей осталось до конца подгруженного: от выбранной и от нижней видимой. */
export function rowsLeft(n: number, sel: number, lastVisible: number): number {
  return Math.min(n - 1 - sel, n - 1 - lastVisible)
}
