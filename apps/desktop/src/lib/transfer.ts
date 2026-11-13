// Передачи файлов через сервер: что показывать в строке и в предпросмотре.
// Rust шлёт событие clipwire://transfer на каждый файл (blob), здесь из них
// собирается одна сводка на запись.
import type { FileInfo, Transfer } from './api'

/** Передачи по blob: последнее известное состояние каждого файла. */
export type TransferMap = Record<string, Transfer>

export interface Progress {
  direction: 'up' | 'down'
  done: number
  total: number
  /** 0…100, целое. */
  percent: number
  /** Есть файлы, которые ещё передаются. */
  active: boolean
  /** Текст первой ошибки, если что-то не передалось. */
  error: string | null
}

/** Целый процент с округлением вниз: «100%» только когда передано всё. */
export function percentOf(done: number, total: number): number {
  if (total <= 0) return 0
  return Math.max(0, Math.min(100, Math.floor((done * 100) / total)))
}

/**
 * Суммарный прогресс по файлам записи. Возвращает null, если показывать
 * нечего: передач нет или все закончились успешно. Если идут и загрузка, и
 * скачивание (бывает редко), показывается скачивание: его ждёт пользователь.
 * Ошибка важнее прогресса, но не прячет его: полоса остаётся на месте сбоя.
 */
export function entryProgress(files: Pick<FileInfo, 'blob'>[], transfers: TransferMap): Progress | null {
  const list = files.flatMap((f) => (f.blob && transfers[f.blob] ? [transfers[f.blob]] : []))
  if (list.length === 0) return null
  const failed = list.find((t) => t.state === 'failed')
  const active = list.filter((t) => t.state === 'active')
  if (!failed && active.length === 0) return null
  const direction = (failed ?? active.find((t) => t.direction === 'down') ?? active[0]).direction
  const same = list.filter((t) => t.direction === direction)
  const total = same.reduce((n, t) => n + t.total, 0)
  // Готовый файл засчитывается целиком, даже если последнее событие о нём
  // пришло с done чуть меньше total.
  const done = same.reduce((n, t) => n + (t.state === 'done' ? t.total : Math.min(t.done, t.total)), 0)
  return {
    direction,
    done,
    total,
    percent: percentOf(done, total),
    active: same.some((t) => t.state === 'active'),
    error: failed?.error || (failed ? 'неизвестная ошибка' : null),
  }
}

/** Подпись для меты строки: «↑ 45%», «↓ 12%», «не отправлено: причина». */
export function progressLabel(p: Progress): string {
  if (p.error) return `${p.direction === 'up' ? 'не отправлено' : 'не скачано'}: ${p.error}`
  return `${p.direction === 'up' ? '↑' : '↓'} ${p.percent}%`
}

/** Файл можно показать и вставить с этого компьютера. */
export function isHere(f: FileInfo): boolean {
  return f.local_path !== null && f.exists
}

/** Файл с другого устройства, который можно скачать с сервера. */
export function canDownload(f: FileInfo): boolean {
  return f.remote && !f.is_dir && f.blob !== null && !isHere(f)
}

/** Файл с другого устройства, которого нет на сервере: папка или файл крупнее порога. */
export function onlyThere(f: FileInfo): boolean {
  return f.remote && !isHere(f) && f.blob === null
}
