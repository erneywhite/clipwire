// Фигуры редактора и геометрия без canvas: размеры, рамки, попадание мышью.
// Координаты везде в пикселях картинки, не экрана.

export type Pt = { x: number; y: number }
export type Tool = 'select' | 'arrow' | 'pen' | 'marker' | 'rect' | 'ellipse' | 'text' | 'blur' | 'crop'
export type Kind = Exclude<Tool, 'select' | 'crop'>

export interface Shape {
  id: number
  kind: Kind
  color: string
  /** 0, 1, 2 — S, M, L. */
  size: number
  a: Pt
  b: Pt
  /** Перо и маркер. */
  pts?: Pt[]
  /** Текст в плашке. */
  text?: string
}

export interface Rect {
  x: number
  y: number
  w: number
  h: number
}

/** Цвета рисования одни для всех тем: заметка должна выглядеть одинаково у всех. */
export const COLORS = ['#e5484d', '#f0b030', '#3fbf7a', '#3b82f6', '#1d1f23', '#ffffff']

/** Базовая единица толщины: на 4K линии толще, чем на маленьком скрине. */
export function unit(w: number, h: number): number {
  return Math.max(2, Math.round(Math.max(w, h) / 500))
}

export function strokeWidth(u: number, size: number): number {
  return u * [1, 1.6, 2.6][size]
}

export function fontSize(u: number, size: number): number {
  return u * [5, 7, 10][size]
}

export function blurBlock(u: number, size: number): number {
  return Math.max(4, u * [3, 5, 8][size])
}

/** Цвет текста на плашке: тёмный на светлых цветах, белый на остальных. */
export function textOn(color: string): string {
  const n = parseInt(color.slice(1), 16)
  const [r, g, b] = [(n >> 16) & 255, (n >> 8) & 255, n & 255]
  return 0.299 * r + 0.587 * g + 0.114 * b > 160 ? '#1d1f23' : '#ffffff'
}

/** Прямоугольник по двум углам в любом порядке. */
export function norm(a: Pt, b: Pt): Rect {
  return { x: Math.min(a.x, b.x), y: Math.min(a.y, b.y), w: Math.abs(b.x - a.x), h: Math.abs(b.y - a.y) }
}

/** Shift: квадрат или круг для рамки, шаг 45° для стрелки. */
export function constrain(kind: Kind | 'crop', a: Pt, b: Pt): Pt {
  const dx = b.x - a.x
  const dy = b.y - a.y
  if (kind === 'arrow') {
    const len = Math.hypot(dx, dy)
    const ang = Math.round(Math.atan2(dy, dx) / (Math.PI / 4)) * (Math.PI / 4)
    return { x: a.x + Math.cos(ang) * len, y: a.y + Math.sin(ang) * len }
  }
  const s = Math.max(Math.abs(dx), Math.abs(dy))
  return { x: a.x + Math.sign(dx || 1) * s, y: a.y + Math.sign(dy || 1) * s }
}

/** Наконечник стрелки: вершина в b, два угла основания. */
export function arrowHead(a: Pt, b: Pt, width: number): [Pt, Pt, Pt] {
  const ang = Math.atan2(b.y - a.y, b.x - a.x)
  const len = Math.min(Math.max(width * 4, 10), Math.hypot(b.x - a.x, b.y - a.y))
  const spread = Math.PI / 7
  return [
    b,
    { x: b.x - len * Math.cos(ang - spread), y: b.y - len * Math.sin(ang - spread) },
    { x: b.x - len * Math.cos(ang + spread), y: b.y - len * Math.sin(ang + spread) },
  ]
}

/** Рамка фигуры для выделения. Текст передаёт свою измеренную ширину и высоту. */
export function bounds(s: Shape, textBox?: { w: number; h: number }): Rect {
  if (s.kind === 'text' && textBox) return { x: s.a.x, y: s.a.y, ...textBox }
  if (s.pts?.length) {
    const xs = s.pts.map((p) => p.x)
    const ys = s.pts.map((p) => p.y)
    const x = Math.min(...xs)
    const y = Math.min(...ys)
    return { x, y, w: Math.max(...xs) - x, h: Math.max(...ys) - y }
  }
  return norm(s.a, s.b)
}

/** Попадание точки в рамку с запасом `pad`. */
export function inside(p: Pt, r: Rect, pad: number): boolean {
  return p.x >= r.x - pad && p.x <= r.x + r.w + pad && p.y >= r.y - pad && p.y <= r.y + r.h + pad
}

/** Расстояние от точки до отрезка — чтобы стрелку можно было схватить за линию. */
export function distToSegment(p: Pt, a: Pt, b: Pt): number {
  const dx = b.x - a.x
  const dy = b.y - a.y
  const l2 = dx * dx + dy * dy
  const t = l2 === 0 ? 0 : Math.max(0, Math.min(1, ((p.x - a.x) * dx + (p.y - a.y) * dy) / l2))
  return Math.hypot(p.x - (a.x + t * dx), p.y - (a.y + t * dy))
}

/** Сдвинуть фигуру целиком. */
export function moved(s: Shape, dx: number, dy: number): Shape {
  const m = (p: Pt) => ({ x: p.x + dx, y: p.y + dy })
  return { ...s, a: m(s.a), b: m(s.b), pts: s.pts?.map(m) }
}

/** Обрезка в пределах картинки и не меньше пары пикселей, иначе её нет. */
export function clampCrop(r: Rect, w: number, h: number): Rect | null {
  const x = Math.max(0, Math.round(r.x))
  const y = Math.max(0, Math.round(r.y))
  const x2 = Math.min(w, Math.round(r.x + r.w))
  const y2 = Math.min(h, Math.round(r.y + r.h))
  if (x2 - x < 4 || y2 - y < 4) return null
  return { x, y, w: x2 - x, h: y2 - y }
}
